"""Shared lexical helpers for classifying Rust ``cfg`` attributes.

The CI ratchets that import this module do not run rustc and therefore cannot
resolve target or feature predicates.  They classify an item as test-only only
when its Boolean cfg gates cannot be true with ``test`` disabled, under any
assignment of the other predicates.  Malformed, unsupported, or deliberately
large expressions stay production-visible.

``cfg_attr`` is handled by its actual gating semantics rather than by spotting
the word ``test``.  Only nested ``cfg(...)``/``cfg_attr(...)`` payloads can
remove an item from a build; lint, derive, path, and other conditional
attributes do not.  In particular, ``cfg_attr(test, allow(...))`` and even
``cfg_attr(test, cfg(...))`` remain production-visible because the conditional
attribute is absent when ``test`` is disabled.  A form such as
``cfg_attr(not(test), cfg(any()))`` is test-only because it applies an
always-false cfg gate to non-test builds.

Callers must blank comments and strings before using the attribute finder.  It
preserves the conservative convention established by
``check_durable_frontier_writer_call_sites.py``.
"""

from __future__ import annotations

import re


CFG_ATTR_START_RE = re.compile(r"#\s*\[\s*(cfg|cfg_attr)\s*\(")


class _CfgNode:
    __slots__ = ("kind", "name", "children")

    def __init__(
        self,
        kind: str,
        name: str | None = None,
        children: tuple["_CfgNode", ...] = (),
    ) -> None:
        self.kind = kind
        self.name = name
        self.children = children


class CfgAttributeMatch:
    """Small match-like span object used by line-oriented scanners.

    ``end()`` intentionally returns the closing parenthesis position, matching
    the historical durable-writer scanner contract.  ``attribute_end()`` is
    the exclusive end of the complete ``#[...]`` attribute.
    """

    __slots__ = ("_start", "_close_paren", "_attribute_end")

    def __init__(self, start: int, close_paren: int, attribute_end: int) -> None:
        self._start = start
        self._close_paren = close_paren
        self._attribute_end = attribute_end

    def start(self) -> int:
        return self._start

    def end(self) -> int:
        return self._close_paren

    def attribute_end(self) -> int:
        return self._attribute_end


def _cfg_tokens(expression: str) -> list[str]:
    """Tokenise cfg meta syntax after strings/comments were blanked."""

    return re.findall(r"[A-Za-z_]\w*|[(),=]", expression)


def _parse_cfg_expression(expression: str) -> _CfgNode | None:
    """Parse enough cfg meta grammar to evaluate test-only reachability."""

    tokens = _cfg_tokens(expression)
    if not tokens:
        return None
    index = 0

    def parse_expr() -> _CfgNode | None:
        nonlocal index
        if index >= len(tokens):
            return None
        name = tokens[index]
        index += 1
        if name in {"all", "any", "not"} and index < len(tokens) and tokens[index] == "(":
            index += 1
            children: list[_CfgNode] = []
            while index < len(tokens) and tokens[index] != ")":
                child = parse_expr()
                if child is None:
                    return None
                children.append(child)
                if index < len(tokens) and tokens[index] == ",":
                    index += 1
                    continue
                if index < len(tokens) and tokens[index] == ")":
                    break
                return None
            if index >= len(tokens) or tokens[index] != ")":
                return None
            index += 1
            return _CfgNode(name, children=tuple(children))

        # A key/value predicate's string was blanked by the caller.  Consume
        # its punctuation and retain the key as an unknown Boolean atom.
        while index < len(tokens) and tokens[index] not in {",", ")"}:
            index += 1
        return _CfgNode("atom", name=name)

    node = parse_expr()
    if node is None or index != len(tokens):
        return None
    return node


def _cfg_eval(node: _CfgNode, values: dict[int, bool], test_enabled: bool) -> bool:
    if node.kind == "atom":
        if node.name == "test":
            return test_enabled
        return values[id(node)]
    if node.kind == "all":
        return all(_cfg_eval(child, values, test_enabled) for child in node.children)
    if node.kind == "any":
        return any(_cfg_eval(child, values, test_enabled) for child in node.children)
    if node.kind == "not" and len(node.children) == 1:
        return not _cfg_eval(node.children[0], values, test_enabled)
    return True


def _can_be_true_without_test(node: _CfgNode) -> bool:
    unknowns: list[_CfgNode] = []

    def collect(current: _CfgNode) -> None:
        if current.kind == "atom":
            if current.name != "test":
                unknowns.append(current)
            return
        for child in current.children:
            collect(child)

    collect(node)
    if len(unknowns) > 12:
        return True
    for mask in range(1 << len(unknowns)):
        values = {
            id(atom): bool(mask & (1 << bit)) for bit, atom in enumerate(unknowns)
        }
        if _cfg_eval(node, values, test_enabled=False):
            return True
    return False


def cfg_requires_test(expression: str) -> bool:
    """Return whether a cfg predicate cannot hold with ``test`` disabled."""

    parsed = _parse_cfg_expression(expression)
    return parsed is not None and not _can_be_true_without_test(parsed)


def _split_top_level_args(expression: str) -> list[str] | None:
    args: list[str] = []
    depth = 0
    start = 0
    for index, char in enumerate(expression):
        if char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
            if depth < 0:
                return None
        elif char == "," and depth == 0:
            args.append(expression[start:index].strip())
            start = index + 1
    if depth != 0:
        return None
    args.append(expression[start:].strip())
    return args


def _cfg_attr_gate(expression: str) -> _CfgNode | None:
    """Return the effective cfg gate contributed by one cfg_attr body."""

    args = _split_top_level_args(expression)
    if args is None or len(args) < 2:
        return None
    condition = _parse_cfg_expression(args[0])
    if condition is None:
        return None

    gates: list[_CfgNode] = []
    for attribute in args[1:]:
        attribute = attribute.strip()
        for name in ("cfg", "cfg_attr"):
            prefix = name + "("
            if not attribute.startswith(prefix) or not attribute.endswith(")"):
                continue
            body = attribute[len(prefix) : -1]
            gate = (
                _parse_cfg_expression(body)
                if name == "cfg"
                else _cfg_attr_gate(body)
            )
            if gate is None:
                return None
            gates.append(gate)
            break

    if not gates:
        return _CfgNode("all")
    # cfg_attr(C, cfg(G), ...) contributes C => G, or !C || G.
    return _CfgNode(
        "any",
        children=(
            _CfgNode("not", children=(condition,)),
            _CfgNode("all", children=tuple(gates)),
        ),
    )


def find_test_only_cfg_attribute(
    code: str, start: int = 0
) -> CfgAttributeMatch | None:
    """Find the next cfg/cfg_attr whose effective gate requires ``test``."""

    for opening in CFG_ATTR_START_RE.finditer(code, start):
        open_paren = code.find("(", opening.start(), opening.end())
        depth = 0
        close_paren = None
        for index in range(open_paren, len(code)):
            char = code[index]
            if char == "(":
                depth += 1
            elif char == ")":
                depth -= 1
                if depth == 0:
                    close_paren = index
                    break
        if close_paren is None:
            continue
        attribute_end = close_paren + 1
        while attribute_end < len(code) and code[attribute_end].isspace():
            attribute_end += 1
        if attribute_end >= len(code) or code[attribute_end] != "]":
            continue
        attribute_end += 1

        expression = code[open_paren + 1 : close_paren]
        kind = opening.group(1)
        parsed = (
            _parse_cfg_expression(expression)
            if kind == "cfg"
            else _cfg_attr_gate(expression)
        )
        if parsed is not None and not _can_be_true_without_test(parsed):
            return CfgAttributeMatch(opening.start(), close_paren, attribute_end)
    return None
