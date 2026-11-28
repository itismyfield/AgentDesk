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

``feature = "test-support"`` is off in production and on only where an upper
crate's tests link this one, so a gate is also test-only when every such
feature-linked build that includes it is matched by the plain test build:
``any(test, feature = "test-support")`` qualifies, the feature alone does not.

Callers must blank comments and strings before using the attribute finder.  It
preserves the conservative convention established by
``check_durable_frontier_writer_call_sites.py``.  Passing the unblanked
``source`` lets it read cfg string values; without it the alias is not seen.
A gate that is test-only only through that feature excludes its item only when
it stays test-only joined with every cfg around and inside the item, and a
mount only when joined with the cfgs up its mount chain and in its files.
"""

from __future__ import annotations

import dataclasses
import re
from collections.abc import Callable, Iterable, Iterator
from itertools import product
from pathlib import Path

try:
    from rust_lex import COMMENT, StripState, lex_segments, strip_line
except ModuleNotFoundError:
    from scripts.rust_lex import COMMENT, StripState, lex_segments, strip_line


TEST_SUPPORT_FEATURE = "test-support"
_STRING_RE = re.compile(r'"(?:[^"\\]|\\.)*"')
CFG_ATTR_START_RE = re.compile(r"#\s*!?\s*\[\s*(cfg|cfg_attr)\s*\(")
ATTR_OPEN_RE = re.compile(r"#\s*!?\s*\[")


class _CfgNode:
    __slots__ = ("kind", "name", "children", "value")

    def __init__(
        self,
        kind: str,
        name: str | None = None,
        children: tuple["_CfgNode", ...] = (),
        value: str | None = None,
    ) -> None:
        self.kind = kind
        self.name = name
        self.children = children
        self.value = value


class CfgAttributeMatch:
    """Small match-like span object used by line-oriented scanners.

    ``end()`` intentionally returns the closing parenthesis position, matching
    the historical durable-writer scanner contract.  ``attribute_end()`` is
    the exclusive end of the complete ``#[...]`` attribute.
    """

    __slots__ = ("_start", "_close_paren", "_attribute_end", "is_inner")

    def __init__(
        self,
        start: int,
        close_paren: int,
        attribute_end: int,
        *,
        is_inner: bool = False,
    ) -> None:
        self._start = start
        self._close_paren = close_paren
        self._attribute_end = attribute_end
        self.is_inner = is_inner

    def start(self) -> int:
        return self._start

    def end(self) -> int:
        return self._close_paren

    def attribute_end(self) -> int:
        return self._attribute_end


def _cfg_tokens(expression: str) -> list[str]:
    """Tokenise cfg meta syntax after strings/comments were blanked."""

    return re.findall(r'"(?:[^"\\]|\\.)*"|[A-Za-z_]\w*|[(),=]', expression)


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

        # Keep a key/value predicate as one unknown atom; its string survives
        # only when the caller passed unblanked text.  Only the plain
        # ``key = "value"`` spelling is read: a raw string stays an unknown atom.
        start = index
        while index < len(tokens) and tokens[index] not in {",", ")"}:
            index += 1
        rest = tokens[start:index]
        value = rest[1][1:-1] if len(rest) == 2 and rest[0] == "=" and rest[1].startswith('"') else None
        return _CfgNode("atom", name=name, value=value)

    node = parse_expr()
    if node is None or index != len(tokens):
        return None
    return node


def _is_test_support_feature(node: _CfgNode) -> bool:
    return node.kind == "atom" and node.name == "feature" and node.value == TEST_SUPPORT_FEATURE


def _cfg_atoms(node: _CfgNode) -> Iterator[_CfgNode]:
    if node.kind == "atom":
        yield node
    for child in node.children:
        yield from _cfg_atoms(child)


def _cfg_eval(
    node: _CfgNode, atom_value: Callable[[_CfgNode], bool],
    test_enabled: bool, feature_enabled: bool | None = False,
) -> bool:
    if node.kind == "atom":
        if node.name == "test":
            return test_enabled
        if _is_test_support_feature(node) and feature_enabled is not None:
            return feature_enabled
        return atom_value(node)
    children = (_cfg_eval(child, atom_value, test_enabled, feature_enabled) for child in node.children)
    if node.kind == "all":
        return all(children)
    if node.kind == "any":
        return any(children)
    if node.kind == "not" and len(node.children) == 1:
        return not next(children)
    return True


def _can_be_true_without_test(node: _CfgNode, feature_aware: bool = True) -> bool:
    """Reachable in a production build, or in a feature-linked build but not the plain test build.

    With ``feature_aware`` off the feature is an ordinary atom: the old rule, kept under any join.
    """
    unknowns = [atom for atom in _cfg_atoms(node)
                if atom.name != "test" and not (feature_aware and _is_test_support_feature(atom))]
    if len(unknowns) > 12:
        return True
    for mask in range(1 << len(unknowns)):
        values = {
            id(atom): bool(mask & (1 << bit)) for bit, atom in enumerate(unknowns)
        }

        def atom_value(atom: _CfgNode) -> bool:
            return values[id(atom)]

        feature = False if feature_aware else None
        if _cfg_eval(node, atom_value, False, feature):
            return True
        if feature_aware and _cfg_eval(node, atom_value, False, True) and not _cfg_eval(node, atom_value, True):
            return True
    return False


def _missing_from_dependency_test(node: _CfgNode) -> bool:
    unknowns = tuple(dict.fromkeys(
        (atom.name, atom.value) for atom in _cfg_atoms(node)
        if atom.name != "test" and not _is_test_support_feature(atom)
    ))
    if len(unknowns) > 12:
        return False
    unit_test_reachable = False
    for bits in product((False, True), repeat=len(unknowns)):
        values = dict(zip(unknowns, bits))

        def atom_value(atom: _CfgNode) -> bool:
            return values[atom.name, atom.value]

        if _cfg_eval(node, atom_value, test_enabled=False, feature_enabled=True):
            return False
        unit_test_reachable = (
            unit_test_reachable
            or _cfg_eval(node, atom_value, test_enabled=True, feature_enabled=False)
            or _cfg_eval(node, atom_value, test_enabled=True, feature_enabled=True)
        )
    return unit_test_reachable


def _feature_dependent(node: _CfgNode) -> bool:
    """Test-only only because of the feature, so a join with another cfg can undo it."""
    return not _can_be_true_without_test(node) and _can_be_true_without_test(node, feature_aware=False)


def _joins_stay_test_only(gate: _CfgNode, others: Iterable[_CfgNode | None]) -> bool:
    """Whether ``gate`` stays test-only joined with each cfg; pairwise joins cover the whole join."""
    return all(
        other is not None and not _can_be_true_without_test(_CfgNode("all", children=(gate, other)))
        for other in others
    )


def _strip_comments(expression: str) -> str:
    """Blank real comments in raw cfg text, keeping string literals; Rust reads comments as spaces."""
    state = StripState()
    return "\n".join(
        "".join(" " * len(text) if kind == COMMENT else text for kind, text in lex_segments(line, state))
        for line in expression.split("\n")
    )


def parse_cfg_condition(expression: str) -> _CfgNode | None:
    """Parse one raw ``cfg(...)`` body; comments are blanked first."""
    return _parse_cfg_expression(_strip_comments(expression))


def cfg_requires_test(expression: str, *, strict: bool = False, dependency_test: bool = False) -> bool:
    """Return whether a raw cfg predicate cannot hold with ``test`` disabled.

    ``strict=True`` asks the narrower question a crate cut needs, whether the item
    is missing from a dependency build, where the test-support feature can be on.
    ``dependency_test=True`` finds predicates reachable in a unit test but absent
    with test-support on and ``test`` off, sharing values for repeated predicates.
    """

    parsed = parse_cfg_condition(expression)
    if dependency_test:
        return parsed is not None and _missing_from_dependency_test(parsed)
    return parsed is not None and not _can_be_true_without_test(parsed, feature_aware=not strict)


def cfg_conditions_gate(conditions: Iterable[str]) -> str | None:
    """``"strict"`` or ``"feature"`` when the joined raw cfg bodies are test-only, else ``None``."""
    nodes = [parse_cfg_condition(condition) for condition in conditions]
    known = tuple(node for node in nodes if node is not None)
    if not known:
        return None
    gate = _CfgNode("all", children=known)
    if not _can_be_true_without_test(gate, feature_aware=False):
        return "strict"
    if len(known) == len(nodes) and not _can_be_true_without_test(gate):
        return "feature"
    return None


def attribute_conditions(attributes: str) -> tuple[str, ...]:
    """Raw cfg bodies an attribute run gates on, with ``cfg_attr(C, cfg(G))`` as ``any(not(C), all(G))``."""
    text = _strip_comments(attributes)
    blanked = _STRING_RE.sub(lambda m: " " * len(m.group()), text)
    conditions = []
    for start in CFG_ATTR_START_RE.finditer(blanked):
        open_paren = blanked.find("(", start.start(), start.end())
        close = _item_end(blanked, open_paren + 1)
        body = text[open_paren + 1 : close]
        condition = body if start.group(1) == "cfg" else _cfg_attr_condition(body)
        if condition is not None:
            conditions.append(condition)
    return tuple(conditions)


def _cfg_attr_condition(body: str) -> str | None:
    """The cfg a ``cfg_attr`` body adds, ``None`` when it gates nothing; unreadable bodies stay unparsable."""
    args = _split_top_level_args(body)
    if args is None or len(args) < 2:
        return f"cfg_attr({body})"
    gates = []
    for attribute in args[1:]:
        for name in ("cfg", "cfg_attr"):
            if attribute.startswith(name + "(") and attribute.endswith(")"):
                inner = attribute[len(name) + 1 : -1]
                gate = inner if name == "cfg" else _cfg_attr_condition(inner)
                if gate is not None:
                    gates.append(f"all({gate})")
                break
    return f"any(not({args[0]}), all({', '.join(gates)}))" if gates else None


def _mentions_test(node: _CfgNode | None) -> bool:
    if node is None:
        return True
    if node.kind == "atom":
        return node.name == "test" or _is_test_support_feature(node)
    return any(_mentions_test(child) for child in node.children)


def _mount_chain_conditions(declarations_by_parent: dict) -> dict[Path, set[str]]:
    """Per file, every test-naming cfg on any mount chain that reaches it."""
    chains: dict[Path, set[str]] = {}
    changed = True
    while changed:
        changed = False
        for parent, declarations in declarations_by_parent.items():
            inherited = chains.get(parent, set())
            for declaration in declarations:
                own = {c for c in declaration.conditions if _mentions_test(parse_cfg_condition(c))}
                for child in declaration.child_paths:
                    known = chains.setdefault(child, set())
                    if not (inherited | own) <= known:
                        known |= inherited | own
                        changed = True
    return chains


def _module_subtree(roots: Iterable[Path], declarations_by_parent: dict) -> set[Path]:
    """Source files a mount reaches: declared children and everything under their module dirs."""
    files = set(declarations_by_parent)
    seen: set[Path] = set()
    pending = [root for root in roots if root in files]
    while pending:
        path = pending.pop()
        if path in seen:
            continue
        seen.add(path)
        module_dir = path.parent if path.name == "mod.rs" else path.parent / path.stem
        pending.extend(f for f in files if f not in seen and module_dir in f.parents)
        for declaration in declarations_by_parent[path]:
            pending.extend(child for child in declaration.child_paths if child in files)
    return seen


def demote_uncomposed_feature_mounts(
    declarations_by_parent: dict, read_text: Callable[[Path], str]
) -> dict:
    """Keep a feature-dependent mount test-only only when it stays so joined with its chain and files."""
    if not any(d.feature_dependent for ds in declarations_by_parent.values() for d in ds):
        return declarations_by_parent
    chains = _mount_chain_conditions(declarations_by_parent)
    file_gates: dict[Path, list[_CfgNode | None]] = {}

    def gates_in(path: Path) -> list[_CfgNode | None]:
        if path not in file_gates:
            try:
                text = read_text(path)
            except (OSError, UnicodeDecodeError):
                text = ""
            state = StripState()
            code = "\n".join(strip_line(line, state).ljust(len(line)) for line in text.split("\n"))
            file_gates[path] = [gate for _, gate in _active_cfg_attributes(code, text)]
        return file_gates[path]

    result = {}
    for parent, declarations in declarations_by_parent.items():
        kept = []
        for declaration in declarations:
            if declaration.feature_dependent:
                gate = _CfgNode("all", children=tuple(parse_cfg_condition(c) for c in declaration.conditions))
                around = [parse_cfg_condition(c) for c in chains.get(parent, ())]
                around += [g for f in _module_subtree(declaration.child_paths, declarations_by_parent) for g in gates_in(f)]
                if not _joins_stay_test_only(gate, around):
                    declaration = dataclasses.replace(declaration, requires_test=False, feature_dependent=False)
            kept.append(declaration)
        result[parent] = kept
    return result


def feature_gates_under_negated_mounts(declarations_by_parent: dict, read_text: Callable[[Path], str]) -> list[str]:
    """Files whose mount chain can undo a feature-dependent gate inside them, as ``file (cfg)``."""
    alias = parse_cfg_condition(f'any(test, feature = "{TEST_SUPPORT_FEATURE}")')
    found = []
    for path, conditions in sorted(_mount_chain_conditions(declarations_by_parent).items()):
        undoing = sorted(c for c in conditions if not _joins_stay_test_only(alias, [parse_cfg_condition(c)]))
        if not undoing or path not in declarations_by_parent:
            continue
        try:
            text = read_text(path)
        except (OSError, UnicodeDecodeError):
            continue
        state = StripState()
        code = "\n".join(strip_line(line, state).ljust(len(line)) for line in text.split("\n"))
        if feature_dependent_gate_in(code, text):
            found.append(f"{path.as_posix()} ({', '.join(undoing)})")
    return found


def _split_top_level_args(expression: str) -> list[str] | None:
    args: list[str] = []
    depth = 0
    start = 0
    for index, char in enumerate(_STRING_RE.sub(lambda m: " " * len(m.group()), expression)):
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


def _with_source_strings(blanked: str, raw: str | None) -> str:
    """Use the raw span only when it differs from the blanked span by string literals."""
    if raw is not None and _STRING_RE.sub(lambda m: " " * len(m.group()), raw) == blanked:
        return raw
    return blanked


def _attribute_gate(
    code: str, opening: re.Match[str], source: str | None = None
) -> tuple[CfgAttributeMatch, _CfgNode | None] | None:
    """Parse one cfg/cfg_attr attribute at its opening token into its span and gate."""
    cfg = CFG_ATTR_START_RE.match(code, opening.start())
    if cfg is None:
        return None
    open_paren = code.find("(", cfg.start(), cfg.end())
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
        return None
    attribute_end = close_paren + 1
    while attribute_end < len(code) and code[attribute_end].isspace():
        attribute_end += 1
    if attribute_end >= len(code) or code[attribute_end] != "]":
        return None
    attribute_end += 1

    expression = _with_source_strings(
        code[open_paren + 1 : close_paren],
        None if source is None else source[open_paren + 1 : close_paren],
    )
    kind = cfg.group(1)
    parsed = (
        _parse_cfg_expression(expression)
        if kind == "cfg"
        else _cfg_attr_gate(expression)
    )
    is_inner = re.match(r"#\s*!", code[cfg.start() :]) is not None
    return CfgAttributeMatch(cfg.start(), close_paren, attribute_end, is_inner=is_inner), parsed


def _gated_span(code: str, match: CfgAttributeMatch) -> tuple[int, int]:
    """Over-approximate the code one attribute gates: its item, or the block an inner one opens."""
    if match.is_inner:
        depth = 0
        for index in range(match.start() - 1, -1, -1):
            if code[index] in ")]}":
                depth += 1
            elif code[index] in "([{":
                if depth == 0:
                    return index, _item_end(code, index)
                depth -= 1
        return 0, len(code)
    return match.start(), _item_end(code, match.attribute_end())


def _item_end(code: str, index: int) -> int:
    """End of the first `;` or braced block (with `else` arms) at depth 0, or of the enclosing one."""
    depth = 0
    while index < len(code):
        char = code[index]
        if char in "([{":
            depth += 1
        elif char in ")]}":
            if depth == 0:
                return index
            depth -= 1
            if depth == 0 and char == "}" and not re.match(r"\s*else\b", code[index + 1 :]):
                return index + 1
        elif char == ";" and depth == 0:
            return index + 1
        index += 1
    return len(code)


def find_test_only_cfg_attributes(
    code: str, source: str | None = None
) -> Iterator[CfgAttributeMatch]:
    """Yield test-only cfg attributes outside strings, attributes, and macro token trees.

    `code` must already have comments and strings blanked. Attribute-like tokens in
    a doc RHS or macro input are tokens, not active build gates. Delimiter state is
    kept across lines so multi-line invocations are treated the same way.
    ``source`` is the same text unblanked, offset for offset.
    """
    attributes = list(_active_cfg_attributes(code, source))
    spans: list[tuple[int, int]] | None = None
    for match, gate in attributes:
        if gate is None or _can_be_true_without_test(gate):
            continue
        if _feature_dependent(gate):
            spans = spans or [_gated_span(code, other) for other, _ in attributes]
            start, end = _gated_span(code, match)
            around = [
                other_gate
                for (other, other_gate), (low, high) in zip(attributes, spans)
                if other.start() != match.start()
                and (low <= match.start() < high or start <= other.start() < end)
            ]
            if not _joins_stay_test_only(gate, around):
                continue
        yield match


def feature_dependent_gate_in(code: str, source: str | None = None) -> bool:
    """Whether some active cfg here is test-only only through the test-support feature."""
    return any(gate is not None and _feature_dependent(gate) for _, gate in _active_cfg_attributes(code, source))


def _active_cfg_attributes(
    code: str, source: str | None = None
) -> Iterator[tuple[CfgAttributeMatch, _CfgNode | None]]:
    """Every cfg/cfg_attr attribute outside strings, attributes, and macro token trees."""
    stack: list[tuple[str, bool, bool]] = []  # opener, macro-token-tree, attribute
    index = 0
    while index < len(code):
        char = code[index]
        if char == "#":
            opening = ATTR_OPEN_RE.match(code, index)
            if opening:
                nested = any(is_macro or is_attribute for _, is_macro, is_attribute in stack)
                if not nested:
                    found = _attribute_gate(code, opening, source)
                    if found is not None:
                        yield found
                bracket = code.find("[", index, opening.end())
                stack.append(("[", False, True))
                index = bracket + 1
                continue
        if char in "([{":
            macro = _macro_delimiter_precedes(code, index)
            stack.append((char, macro, False))
        elif char in ")]}" and stack:
            expected = {")": "(", "]": "[", "}": "{"}[char]
            if stack[-1][0] == expected:
                stack.pop()
        index += 1


def _macro_delimiter_precedes(code: str, index: int) -> bool:
    """Recognize macro calls and named definitions with any token-tree delimiter."""
    def word_before(position: int) -> tuple[str, int]:
        while position >= 0 and code[position].isspace():
            position -= 1
        end = position + 1
        while position >= 0 and (code[position].isalnum() or code[position] == "_"):
            position -= 1
        start = position + 1
        if start < end and position >= 1 and code[position - 1 : position + 1] == "r#":
            position -= 2
        return code[start:end], position

    cursor = index - 1
    while cursor >= 0 and code[cursor].isspace():
        cursor -= 1
    if cursor >= 0 and code[cursor] == "!":
        name, _ = word_before(cursor - 1)
        return bool(name)
    if index >= 0 and code[index] in "([{":
        name, cursor = word_before(cursor)
        while cursor >= 0 and code[cursor].isspace():
            cursor -= 1
        if name and cursor >= 0 and code[cursor] == "!":
            macro_rules, _ = word_before(cursor - 1)
            return macro_rules == "macro_rules"
    return False


def find_test_only_cfg_attribute(
    code: str, start: int = 0, source: str | None = None
) -> CfgAttributeMatch | None:
    """Find the next active cfg/cfg_attr whose effective gate requires ``test``."""
    return next(
        (
            match
            for match in find_test_only_cfg_attributes(code, source)
            if match.start() >= start
        ),
        None,
    )
