#!/usr/bin/env python3
"""Ratchet destructive Rust call sites by category and exact per-file count.

This is a bounded lexical inventory, not a Rust/type/data-flow analysis and not
proof that any listed destruction is safe.  It scans stripped source text for:
tmux kill wrappers; watcher AtomicBool ``store(true, ...)`` calls; process kill
calls; watcher-registry removal calls; and (#5071 relay-tail S4)
``under_identity_fence`` and ``with_terminal_delivery_fence`` bindings.  The
first two categories include test call sites, matching the #5071 map.  Process,
registry and fence categories reuse the repository's existing lexical
``cfg(test)`` classifier and exclude whole test modules.  Aliases, re-exports,
macros that construct names, indirection, and semantically equivalent spellings
can remain unseen.

The ``identity_fence_bind`` and ``delivery_fence_bind`` categories are the
inverse of the others: they count the FENCED entry points, not unfenced
destruction.

The ``structural_candidate_apply`` and ``destructive_warrant_bind`` warrant
categories require equal per-file pair counts and fail closed on either
one-sided mismatch. This is a two-sided check, not a no-growth check: one-sided
deletion is rejected by pairing, while a paired addition in an existing file or
a move to a new destination requires a reviewed baseline diff. The bounded
lexical inventory cannot detect paired deletion, a count-preserving relocation,
or replacement of one pair by another pair in the same file.

Its remaining limits are explicit:

* **Return-value discard:** the inventory counts calls, not whether the warrant
  result is consumed. Spelling both symbols while discarding the warrant bool is
  invisible. The stale-sweep behavioral witness, not the return type, enforces
  consumption by proving that a production veto stops destructive apply.
* **Argument identity:** equal counts do not prove both calls describe the same
  candidate.
* **Control-flow dominance:** equal counts do not prove the warrant dominates the
  destructive operation or that no branch bypasses it.
* **Unused diagnostics are not enforcement:** some named discarded results may
  receive an ordinary unused-binding warning, for either struct or scalar
  returns, but ``_``/``_name`` can suppress it and ``DestructiveWarrant`` has no
  ``#[must_use]`` contract. It is neither type enforcement nor required-CI
  protection.

The behavioral witness covers return-value discard and veto bypass. It does not
prove argument identity or control-flow dominance at other sites.

Pinning fence binders does NOT make an unfenced destructive removal
impossible — those spell the unfenced helper names and land in
``registry_remove`` instead.  What it does is force any change to the set of
fence-bearing call sites (adding one, moving one, deleting one) to appear as a
reviewed baseline diff in the same commit, so an S4 fence cannot be silently
detached from a call site that keeps its ``registry_remove`` count.

The two fence categories are additionally checked AGAINST EACH OTHER, per file,
by :func:`pairing_errors` — a fenced site must carry both binders, so the counts
must be equal in every file that has either.  This is deliberately two-sided and
NOT a no-growth check: dropping ``.with_terminal_delivery_fence(..)`` from a
site that keeps ``under_identity_fence(..)`` is a DECREASE, which the growth
ratchet permits by design, and it is exactly the silent unfencing this guards.
The compiler is the other half — ``TmuxWatcherRegistry::under_identity_fence``
returns a view with no destructive method, so at this SHA that same deletion
also fails to typecheck.  This check exists because that is a property of one
type's shape, which a refactor can relax without anyone noticing, while a
pairing diff is visible in review.

``inflight_row_clear_call`` (#5462 S5) pins, per file, the calls to an inflight
ROW-DESTRUCTION helper made from OUTSIDE the owner module
``src/services/discord/inflight/clear_store/``.  The owner exclusion is the same
idiom ``registry_remove`` uses for ``REGISTRY_OWNER``: definitions and
helper-to-helper composition live there, and pinning them would count the
implementation instead of its consumers.  The callee's NAME carries the
destructive meaning here, which is why it sees helper-mediated destruction that a
counter keyed on path-resolution symbols cannot: in this repository the callee,
not the caller, resolves the inflight path.  Its limits:

* A SPELLING count, not a semantic one.  Definitions and re-export wrappers are
  counted too.  Harmless for a no-growth ratchet, wrong if read as "number of
  destruction sites".
* Direct ``fs::remove_file`` unlinks are INVISIBLE.  ``inflight/removal.rs``,
  ``inflight/rebind_reap.rs`` and ``inflight.rs`` unlink directly and count 0 for
  those unlinks.  Helper-mediated class only; the direct-unlink surface is a
  separate issue.
* The generation-fenced ``*_for_reconcile`` wrappers count AS destruction, not as
  a separate fenced-entry-point category.  They delegate to the same unlink, and
  excluding them would blind the category on the very reconcile sites it exists
  to pin: those sites spelled the bare helper when the baseline was designed, so
  converting one to its fenced form would otherwise read as a DECREASE the
  no-growth ratchet waves through.  Whether the fence actually refuses is a
  runtime property this file cannot see — ``reconcile_gate.rs`` counts that.
* Aliases, re-export call names, macro-assembled names, general indirection and
  semantically equivalent spellings stay unseen, same as every other category.
  A named row-destruction entry point that this repository already exposes is
  NOT in that residual — it belongs in the pattern.
  ``clear_inflight_state_for_channel``,
  ``archive_inflight_state_if_matches_identity_generation*`` (renames the row
  into the archive path, so the inflight row is gone) and
  ``clear_lifecycle_inflight_state_if_matches*`` were left out by the #5462 §4.5
  enumeration and are in the pattern as of S5 r2; each has production consumers
  that the earlier pattern let grow without limit.

``host_terminate`` counts spellings of the Herdr close RPCs (``pane.close``,
``server.stop``) across all of ``src/**`` including tests: each string or char
literal after escape decoding, and ``stringify!`` tokens; pieces joined only by a
real concatenation (``concat!`` arguments, ``+`` chains, ``[..].concat()`` or
``.join(sep)``, positional ``format!``-family arguments), counted when a match
spans a join so no piece counts twice; and the method names as identifier word
parts in any case style (``PaneClose``, ``RPCPaneClose``, ``herdr_server_stop``,
``CLOSE_PANE``), which also covers ``use ... as`` aliases and wrapper
definitions. Array elements, tuple items and call arguments are never joined, and
completed-state names (``PANE_CLOSED``) do not count. Spellings are limited to
``HOST_TERMINATE_OWNER`` and fixed declaration sites with per-file maxima.
The baseline pins owner growth only and cannot relax declaration maxima. Pieces
passed through a variable, built at runtime (``push_str``, ``+=``) or read from a
non-Rust file stay unseen, and the count proves nothing about the warrant a call receives.

``--check`` rejects growth in an existing file, every UNLISTED file, any
host_terminate spelling outside its declared sites, and any identity/delivery pairing
mismatch.  A decrease is allowed for growth: this is a
no-growth ratchet.  For an intentional change, run ``--write-baseline`` and
review the JSON diff in the same commit.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Mapping, Sequence


REPO_ROOT = Path(__file__).resolve().parents[1]
BASELINE_PATH = Path("scripts/destructive_call_site_baseline.json")
CATEGORIES = (
    "tmux_kill",
    "watcher_cancel",
    "process_kill",
    "registry_remove",
    "identity_fence_bind",
    "delivery_fence_bind",
    "inflight_row_clear_call",
    "structural_candidate_apply",
    "destructive_warrant_bind",
    "host_terminate",
)
WARNING = "These counts are a growth-blocking baseline, not proof of safety."
HOST_TERMINATE_COMMENT = (
    "Herdr close RPC spellings (pane.close, server.stop, their method names and "
    "aliases). Only src/services/termination_audit/host_terminate.rs uses this "
    "baseline map; its growth needs a reviewed diff here. Fixed declaration "
    "sites use checker constants, and every other site fails regardless of this map."
)
REPIN = (
    "Intentional change: run scripts/check_destructive_call_site_ratchet.py "
    "--write-baseline and commit the reviewed JSON diff."
)

ALL_SOURCE_PATTERNS = {
    "tmux_kill": re.compile(
        r"\b(?:crate\s*::\s*services\s*::\s*)?platform\s*::\s*tmux\s*::\s*"
        r"kill_session(?:_output_timeout|_output|_checked)?\s*\("
    ),
    "watcher_cancel": re.compile(
        r"\b(?:cancel|cancel_for_commit|expected_cancel|watcher_cancel)\s*\.\s*"
        r"store\s*\(\s*true\b"
    ),
}
PROCESS_PATTERN = re.compile(
    r"(?:\b(?:kill_pid_tree|terminate_process_handle)\s*\(|\.\s*kill\s*\(\s*\))"
)
REGISTRY_PATTERNS = {
    "direct_channel_remove": re.compile(
        r"\btmux_watchers\s*\.\s*(?:remove|remove_locked)\s*\("
    ),
    "remove_if_current": re.compile(r"\bremove_tmux_session_if_current\s*\("),
    "cancel_and_remove_if_current": re.compile(
        r"\bcancel_and_remove_channel_if_current\s*\("
    ),
    "remove_locked_helper": re.compile(r"\bremove_tmux_session_locked\s*\("),
}
REGISTRY_OWNER = "src/services/discord/tmux_watcher_registry.rs"
# #5071 relay-tail S4: the binder for `WatcherIdentityFence`, and (r2) the
# chained binder for `TerminalDeliveryFence`.  Counted with the registry
# categories, so the owner file's own definitions are excluded the same way.
IDENTITY_FENCE_PATTERN = re.compile(r"\bunder_identity_fence\s*\(")
DELIVERY_FENCE_PATTERN = re.compile(r"\bwith_terminal_delivery_fence\s*\(")
FENCE_PAIR = ("identity_fence_bind", "delivery_fence_bind")
STRUCTURAL_CANDIDATE_PATTERN = re.compile(r"(?<!fn )\bstructural_candidate_apply\s*\(")
DESTRUCTIVE_WARRANT_PATTERN = re.compile(r"(?<!fn )\bdestructive_warrant_bind\s*\(")
WARRANT_PAIR = ("structural_candidate_apply", "destructive_warrant_bind")
# #5462 S5: inflight row-destruction helpers.  Scanned over all of `src/**`, not
# just `src/services/discord/`, because `services/turn_lifecycle.rs` calls them
# from outside the Discord tree.
INFLIGHT_ROW_CLEAR_PATTERN = re.compile(
    r"\b(?:clear_inflight_state(?:_if_matches\w*|_for_reconcile\w*|_for_channel)?"
    r"|clear_rebind_origin_inflight_state_if_matches_identity\w*"
    r"|clear_rebind_origin_for_reconcile\w*"
    r"|archive_inflight_state_if_matches_identity_generation\w*"
    r"|clear_lifecycle_inflight_state_if_matches\w*"
    r"|delete_inflight_state_file"
    r"|clear_inflight_by_tmux_name"
    r"|request_inflight_abandon_if_matches\w*)\s*\("
)
INFLIGHT_ROW_CLEAR_OWNER_PREFIX = "src/services/discord/inflight/clear_store/"
HOST_TERMINATE_OWNER = "src/services/termination_audit/host_terminate.rs"
# Fixed model, judged leaf and test spellings need no baseline growth;
# any additional spelling still exceeds a measured per-file ceiling.
HOST_TERMINATE_DECLARED_SITES = {
    "src/services/session_host/herdr/model.rs": 2,
    "src/services/session_host/herdr_gate.rs": 3,
    "src/services/session_host/herdr_gate_tests.rs": 5,
    "src/services/session_host/herdr_host_tests.rs": 5,
}
HOST_TERMINATE_LITERAL_PATTERN = re.compile(
    r"(?i)\bpane\s*\.\s*close\b|\bserver\s*\.\s*stop\b"
)
# Word parts in any case style, so prefixes and suffixes (`herdr_pane_close`,
# `RPCPaneClose`) still count while `pane_closed` and `PANE_CLOSED` do not.
HOST_TERMINATE_IDENT_PATTERN = re.compile(
    r"(?:(?<![A-Za-z0-9])|(?<=[a-z0-9])(?=[A-Z])|(?<=[A-Z])(?=[A-Z][a-z]))"
    r"(?i:pane_?close|server_?stop|close_?pane|stop_?server)"
    r"(?:(?<=[a-z])(?![a-z])|(?<=[A-Z])(?![A-Za-z]))"
)
_ESCAPE = re.compile(
    r"\\(?:x([0-9A-Fa-f]{2})|u\{([0-9A-Fa-f_]+)\}|\n\s*|(.))", re.S
)
_CODE_TOKEN = re.compile(r"[A-Za-z_][A-Za-z0-9_]*|\S")
_CONCAT_MACROS = frozenset({"concat", "concat_bytes"})
# Macros whose first argument (after the writer for `write*!`) is a format string.
_FORMAT_MACROS = frozenset(
    {"format", "format_args", "print", "println", "eprint", "eprintln", "panic"}
)
_WRITE_MACROS = frozenset({"write", "writeln"})
# `lit.to_owned()` and friends keep a literal's value inside a `+` chain.
_STRING_CONVERSIONS = frozenset({"to_owned", "to_string", "into", "as_str", "clone"})
_SIMPLE_ESCAPES = {"n": "\n", "r": "\r", "t": "\t", "0": "\0"}


class RatchetError(RuntimeError):
    pass


def _load_rust_lexer():
    """Reuse the landed call-site gate's comment/string/cfg(test) machinery."""
    path = REPO_ROOT / "scripts/check_durable_frontier_writer_call_sites.py"
    spec = importlib.util.spec_from_file_location("destructive_ratchet_rust_lexer", path)
    if spec is None or spec.loader is None:
        raise RatchetError(f"cannot load Rust lexer: {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


RUST_LEXER = _load_rust_lexer()


def _load_rust_lex():
    path = REPO_ROOT / "scripts/rust_lex.py"
    spec = importlib.util.spec_from_file_location("destructive_ratchet_rust_lex", path)
    if spec is None or spec.loader is None:
        raise RatchetError(f"cannot load Rust segment lexer: {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


RUST_LEX = _load_rust_lex()


def _stripped_text(path: Path) -> str:
    state = RUST_LEXER.StripState()
    return "\n".join(
        RUST_LEXER.strip_line(line, state)
        for line in path.read_text(encoding="utf-8").splitlines()
    )


def _literal_content(token: str) -> str:
    """Decoded text of one complete string or char literal token."""
    raw = re.fullmatch(r'b?r(#*)"(.*)"\1', token, flags=re.S)
    if raw:
        return raw.group(2)
    quoted = re.fullmatch(r"b?([\"'])(.*)\1", token, flags=re.S)
    body = quoted.group(2) if quoted else token

    def decode(match: re.Match[str]) -> str:
        hex_byte, unicode, simple = match.groups()
        if hex_byte:
            return chr(int(hex_byte, 16))
        if unicode is not None:
            digits = unicode.replace("_", "")
            if not digits or int(digits, 16) > 0x10FFFF:
                return "\0"
            return chr(int(digits, 16))
        if simple is None:
            return ""
        return _SIMPLE_ESCAPES.get(simple, simple)

    return _ESCAPE.sub(decode, body)


def _host_terminate_tokens(text: str) -> tuple[str, list[tuple[str, str]]]:
    """Comment-free code text plus a token stream of decoded literals and code."""
    state = RUST_LEX.StripState()
    code: list[str] = []
    tokens: list[tuple[str, str]] = []
    open_literal: list[str] | None = None

    def close_literal() -> None:
        nonlocal open_literal
        tokens.append(("lit", _literal_content("\n".join(open_literal))))
        open_literal = None

    for line in text.splitlines():
        for kind, chunk in RUST_LEX.lex_segments(line, state):
            if kind == RUST_LEX.LITERAL:
                if open_literal is not None:
                    open_literal.append(chunk)
                    continue
                open_literal = [chunk]
                continue
            if open_literal is not None:
                close_literal()
            if kind == RUST_LEX.CODE:
                code.append(chunk)
                tokens.extend(("code", tok) for tok in _CODE_TOKEN.findall(chunk))
        # A literal still open at end of line continues on the next one.
        if open_literal is not None and not (state.in_string or state.raw_hashes is not None):
            close_literal()
        code.append("\n")
    if open_literal is not None:
        close_literal()
    return " ".join(code), tokens


def _host_terminate_tree(tokens: list[tuple[str, str]]) -> list[list[tuple]]:
    """Nest tokens into delimiter groups ``("group", macro, opener, args)``.

    Commas split a group's arguments; a group opened right after ``name!``
    records that macro name.  The root is one argument list.
    """
    stack: list[tuple[str | None, str | None, list[list[tuple]]]] = [(None, None, [[]])]
    for kind, value in tokens:
        current = stack[-1][2][-1]
        if kind == "code" and value in "([{":
            macro = None
            if (
                len(current) >= 2
                and current[-1] == ("code", "!")
                and current[-2][0] == "code"
                and current[-2][1].isidentifier()
            ):
                macro = current[-2][1]
                del current[-2:]
            stack.append((macro, value, [[]]))
        elif kind == "code" and value in ")]}" and len(stack) > 1:
            macro, opener, args = stack.pop()
            stack[-1][2][-1].append(("group", macro, opener, args))
        elif kind == "code" and value == "," and len(stack) > 1:
            stack[-1][2].append([])
        else:
            current.append((kind, value))
    while len(stack) > 1:
        macro, opener, args = stack.pop()
        stack[-1][2][-1].append(("group", macro, opener, args))
    return stack[0][2]


def _units(arg: list[tuple]) -> list[tuple]:
    """Fold ``[..].concat()``/``.join(sep)`` and conversion suffixes into one unit."""
    units: list[tuple] = []
    i = 0
    while i < len(arg):
        node = arg[i]
        tail = arg[i + 1 : i + 4]
        if (
            node[0] == "group"
            and node[2] == "["
            and node[1] in (None, "vec")
            and len(tail) == 3
            and tail[0] == ("code", ".")
            and tail[1] in (("code", "concat"), ("code", "join"))
            and tail[2][0] == "group"
            and tail[2][2] == "("
        ):
            units.append(("arraycat", node, tail[2]))
            i += 4
        else:
            units.append(node)
            i += 1
        while (
            i + 2 < len(arg)
            and arg[i] == ("code", ".")
            and arg[i + 1][0] == "code"
            and arg[i + 1][1] in _STRING_CONVERSIONS
            and arg[i + 2][0] == "group"
            and arg[i + 2][2] == "("
            and _unit_value(units[-1]) is not None
        ):
            i += 3
    return units


def _arg_value(arg: list[tuple]) -> str | None:
    """Value of an argument that is one string unit or a ``+`` chain of them."""
    units = [unit for unit in _units(arg) if unit != ("code", "&")]
    values: list[str] = []
    for index, unit in enumerate(units):
        if index % 2:
            if unit != ("code", "+"):
                return None
            continue
        value = _unit_value(unit)
        if value is None:
            return None
        values.append(value)
    return "".join(values) if values and len(units) % 2 else None


def _format_parts(format_string: str, args: list[list[tuple]]) -> list[str | None]:
    """Pieces of a format string interleaved with the positional arguments it names."""
    named: dict[str, str | None] = {}
    positional: list[str | None] = []
    for arg in args:
        is_named = len(arg) > 2 and arg[1] == ("code", "=") and arg[2] != ("code", "=")
        value = _arg_value(arg[2:] if is_named else arg)
        if is_named:
            named[arg[0][1]] = value
        positional.append(value)
    parts: list[str | None] = []
    piece, index, cursor = "", 0, 0
    while index < len(format_string):
        pair = format_string[index : index + 2]
        if pair in ("{{", "}}"):
            piece += pair[0]
            index += 2
            continue
        close = format_string.find("}", index) if format_string[index] == "{" else -1
        if close < 0:
            piece += format_string[index]
            index += 1
            continue
        name, _colon, spec = format_string[index + 1 : close].partition(":")
        name = name.strip()
        if not name:
            value = positional[cursor] if cursor < len(positional) else None
            cursor += 1
        elif name.isdigit():
            slot = int(name)
            value = positional[slot] if slot < len(positional) else None
        else:
            value = named.get(name)
        parts.extend([piece, None if "?" in spec else value])
        piece, index = "", close + 1
    parts.append(piece)
    return parts


def _composite_parts(unit: tuple) -> list[str | None] | None:
    """The joined pieces a concatenating unit evaluates to, or None."""
    if unit[0] == "arraycat":
        _tag, array, call = unit
        sep_args = [arg for arg in call[3] if arg]
        if len(sep_args) > 1:
            return None
        sep = _arg_value(sep_args[0]) if sep_args else ""
        parts: list[str | None] = []
        for index, arg in enumerate(arg for arg in array[3] if arg):
            if index:
                parts.append(sep)
            parts.append(_arg_value(arg))
        return parts
    if unit[0] != "group":
        return None
    _tag, macro, _opener, args = unit
    args = [arg for arg in args if arg]
    if macro in _CONCAT_MACROS:
        return [_arg_value(arg) for arg in args]
    if macro in _WRITE_MACROS:
        args = args[1:]
    elif macro not in _FORMAT_MACROS:
        return None
    format_string = _unit_value(args[0][0]) if args and len(args[0]) == 1 else None
    if format_string is None:
        return None
    return _format_parts(format_string, args[1:])


def _unit_value(unit: tuple) -> str | None:
    """String value of one unit; unknown pieces inside it become NUL."""
    if unit[0] == "lit":
        return unit[1]
    if unit[0] == "group" and unit[1] == "stringify":
        return "".join(value for arg in unit[3] for value in _token_texts(arg))
    if unit[0] == "group" and unit[1] is None and unit[2] == "(":
        args = [arg for arg in unit[3] if arg]
        return _arg_value(args[0]) if len(args) == 1 else None
    parts = _composite_parts(unit)
    if parts is None:
        return None
    return "".join("\0" if part is None else part for part in parts)


def _token_texts(arg: list[tuple]) -> list[str]:
    """Token spelling of one argument, as ``stringify!`` renders it minus spaces."""
    texts: list[str] = []
    for node in arg:
        if node[0] == "group":
            texts.append(node[2])
            for index, inner in enumerate(node[3]):
                texts.extend([","] * bool(index) + _token_texts(inner))
            texts.append({"(": ")", "[": "]", "{": "}"}[node[2]])
        else:
            texts.append(node[1])
    return texts


def _joined_count(parts: list[str | None]) -> int:
    """Matches that span a boundary between parts, so no part is counted twice."""
    if len(parts) < 2:
        return 0
    text, bounds = "", []
    for index, part in enumerate(parts):
        if index:
            bounds.append(len(text))
        text += "\0" if part is None else part
    return sum(
        any(match.start() < bound < match.end() for bound in bounds)
        for match in HOST_TERMINATE_LITERAL_PATTERN.finditer(text)
    )


def _literal_count(args: list[list[tuple]]) -> int:
    found = 0
    for arg in args:
        units = _units(arg)
        for unit in units:
            parts = _composite_parts(unit)
            if parts is not None:
                found += _joined_count(parts)
        # `a + "pane" + ".close"`: each maximal run of string units joined by `+`.
        run: list[str | None] = []
        joined = False
        for unit in units + [("code", ";")]:
            if unit == ("code", "&"):
                continue
            if unit == ("code", "+"):
                joined = bool(run)
                continue
            value = _unit_value(unit)
            if value is None or not joined:
                found += _joined_count(run)
                run = []
            if value is not None:
                run.append(value)
            joined = False
        for node in arg:
            if node[0] == "lit":
                found += len(HOST_TERMINATE_LITERAL_PATTERN.findall(node[1]))
            elif node[0] == "group":
                if node[1] == "stringify":
                    found += len(HOST_TERMINATE_LITERAL_PATTERN.findall(_unit_value(node)))
                found += _literal_count(node[3])
    return found


def host_terminate_count(text: str) -> int:
    """Close-RPC spellings in comment-free source; see the module docstring."""
    code, tokens = _host_terminate_tokens(text)
    found = len(HOST_TERMINATE_IDENT_PATTERN.findall(code))
    return found + _literal_count(_host_terminate_tree(tokens))


def _is_whole_test_file(path: Path, rel: str) -> bool:
    return (
        RUST_LEXER.is_test_file(path.name)
        or rel in RUST_LEXER.PINNED_TEST_ONLY_MODULE_FILES
    )


def scan(repo_root: Path) -> tuple[dict[str, dict[str, int]], dict[str, int]]:
    root = Path(repo_root).resolve()
    source_root = root / "src"
    if not source_root.is_dir():
        raise RatchetError("scan root src/ is missing")
    counts: dict[str, dict[str, int]] = {category: {} for category in CATEGORIES}
    registry_subcounts = {name: 0 for name in REGISTRY_PATTERNS}
    paths = sorted(source_root.rglob("*.rs"))
    if not paths:
        raise RatchetError("scan root src/ contains no Rust files")
    for path in paths:
        if path.is_symlink():
            raise RatchetError(f"source symlink is outside the lexical model: {path}")
        rel = path.relative_to(root).as_posix()
        stripped = _stripped_text(path)
        close_found = host_terminate_count(path.read_text(encoding="utf-8"))
        if close_found:
            counts["host_terminate"][rel] = close_found
        for category, pattern in ALL_SOURCE_PATTERNS.items():
            found = len(pattern.findall(stripped))
            if found:
                counts[category][rel] = found

        if _is_whole_test_file(path, rel):
            continue
        production = RUST_LEXER._production_text(path)
        structural_found = len(STRUCTURAL_CANDIDATE_PATTERN.findall(production))
        if structural_found:
            counts["structural_candidate_apply"][rel] = structural_found
        warrant_found = len(DESTRUCTIVE_WARRANT_PATTERN.findall(production))
        if warrant_found:
            counts["destructive_warrant_bind"][rel] = warrant_found
        # Widest surface first: this category is the only one that looks outside
        # `src/services/discord/`.
        if not rel.startswith(INFLIGHT_ROW_CLEAR_OWNER_PREFIX):
            clear_found = len(INFLIGHT_ROW_CLEAR_PATTERN.findall(production))
            if clear_found:
                counts["inflight_row_clear_call"][rel] = clear_found

        if not rel.startswith("src/services/discord/"):
            continue
        process_found = len(PROCESS_PATTERN.findall(production))
        if process_found:
            counts["process_kill"][rel] = process_found
        # Definitions and helper-to-helper composition live in this owner file;
        # the #5071 map deliberately pins external removal consumers only.
        if rel == REGISTRY_OWNER:
            continue
        registry_found = 0
        for name, pattern in REGISTRY_PATTERNS.items():
            found = len(pattern.findall(production))
            registry_subcounts[name] += found
            registry_found += found
        if registry_found:
            counts["registry_remove"][rel] = registry_found
        fence_found = len(IDENTITY_FENCE_PATTERN.findall(production))
        if fence_found:
            counts["identity_fence_bind"][rel] = fence_found
        delivery_found = len(DELIVERY_FENCE_PATTERN.findall(production))
        if delivery_found:
            counts["delivery_fence_bind"][rel] = delivery_found
    return counts, registry_subcounts


def _category_files(payload: Mapping[str, object], category: str) -> dict[str, int]:
    categories = payload.get("categories")
    if not isinstance(categories, dict) or category not in categories:
        raise RatchetError(f"baseline missing category: {category}")
    entry = categories[category]
    if not isinstance(entry, dict) or not isinstance(entry.get("files"), dict):
        raise RatchetError(f"baseline category {category} has no files map")
    files = entry["files"]
    if any(not isinstance(path, str) or not isinstance(count, int) or count <= 0
           for path, count in files.items()):
        raise RatchetError(f"baseline category {category} has invalid per-file count")
    return dict(files)


def load_baseline(path: Path) -> tuple[dict[str, dict[str, int]], dict[str, object]]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise RatchetError(f"cannot load baseline {path}: {exc}") from exc
    if payload.get("schema_version") != 1:
        raise RatchetError("baseline schema_version must be 1")
    counts = {category: _category_files(payload, category) for category in CATEGORIES}
    return counts, payload


def growth_errors(
    actual: Mapping[str, Mapping[str, int]],
    baseline: Mapping[str, Mapping[str, int]],
) -> list[str]:
    errors: list[str] = []
    for category in CATEGORIES:
        expected = baseline.get(category, {})
        for path, found in sorted(actual.get(category, {}).items()):
            pinned = (
                HOST_TERMINATE_DECLARED_SITES.get(path, expected.get(path, 0))
                if category == "host_terminate"
                else expected.get(path, 0)
            )
            if found <= pinned:
                continue
            if pinned == 0:
                errors.append(f"{category}: UNLISTED call site in {path} ({found}x)")
            else:
                errors.append(
                    f"{category}: GROWTH in {path}: found {found}x, baseline {pinned}x"
                )
    return errors


def pairing_errors(actual: Mapping[str, Mapping[str, int]]) -> list[str]:
    """Require both lexical members of each pair; equality is two-sided."""
    errors: list[str] = []
    identity, delivery = (dict(actual.get(category, {})) for category in FENCE_PAIR)
    for path in sorted(set(identity) | set(delivery)):
        identity_found = identity.get(path, 0)
        delivery_found = delivery.get(path, 0)
        if identity_found == delivery_found:
            continue
        errors.append(
            f"fence_pairing: {path} binds under_identity_fence {identity_found}x but "
            f"with_terminal_delivery_fence {delivery_found}x; every fenced destructive "
            "removal must carry both S4 conjuncts"
        )

    structural, warrant = (dict(actual.get(category, {})) for category in WARRANT_PAIR)
    for path in sorted(set(structural) | set(warrant)):
        structural_found = structural.get(path, 0)
        warrant_found = warrant.get(path, 0)
        if structural_found == warrant_found:
            continue
        errors.append(
            f"warrant_pairing: {path} binds structural_candidate_apply {structural_found}x "
            f"but destructive_warrant_bind {warrant_found}x; every automatic destructive "
            "candidate binding must carry its T5 S6a warrant binding"
        )
    return errors


def owner_only_errors(
    actual: Mapping[str, Mapping[str, int]],
    baseline: Mapping[str, Mapping[str, int]],
) -> list[str]:
    """Only the owner and fixed declaration sites may spell close RPCs."""
    errors: list[str] = []
    for path, found in sorted(actual.get("host_terminate", {}).items()):
        if path != HOST_TERMINATE_OWNER and path not in HOST_TERMINATE_DECLARED_SITES:
            errors.append(
                f"host_terminate: close RPC spelled outside {HOST_TERMINATE_OWNER} "
                f"in {path} ({found}x)"
            )
    for path in sorted(baseline.get("host_terminate", {})):
        if path != HOST_TERMINATE_OWNER:
            errors.append(f"host_terminate: baseline lists non-owner file {path}")
    return errors


def _snapshot(
    counts: Mapping[str, Mapping[str, int]],
    registry_subcounts: Mapping[str, int],
    measured_sha: str,
) -> dict[str, object]:
    direct = registry_subcounts["direct_channel_remove"]
    if_current = registry_subcounts["remove_if_current"]
    cancel_current = registry_subcounts["cancel_and_remove_if_current"]
    locked = registry_subcounts["remove_locked_helper"]
    return {
        "schema_version": 1,
        "measured_at_sha": measured_sha,
        "comment": WARNING,
        "categories": {
            "tmux_kill": {"files": dict(sorted(counts["tmux_kill"].items()))},
            "watcher_cancel": {"files": dict(sorted(counts["watcher_cancel"].items()))},
            "process_kill": {"files": dict(sorted(counts["process_kill"].items()))},
            "registry_remove": {
                "comment": (
                    f"Measured production external removals are {direct}/{if_current}/"
                    f"{cancel_current}/{locked} (total {direct + if_current + cancel_current + locked}). "
                    "The design's 10/2/3/2=17 counted turn_finalizer/watcher_backstop.rs:252, "
                    "which is inside #[cfg(test)] at this SHA. health/recovery.rs remove_locked "
                    "is classified as direct channel remove, not remove_tmux_session_locked."
                ),
                "files": dict(sorted(counts["registry_remove"].items())),
            },
            "identity_fence_bind": {
                "comment": (
                    "#5071 relay-tail S4: production `under_identity_fence` binders, "
                    "excluding the owner file that defines it. These are the ONLY "
                    "destructive removals that carry the WatcherIdentityFence and "
                    "TerminalDeliveryFence conjuncts; every other entry in "
                    "registry_remove reaches an unfenced helper. Pinning this set does "
                    "not fence those — it makes adding, moving or dropping a fenced "
                    "site a reviewed baseline diff."
                ),
                "files": dict(sorted(counts["identity_fence_bind"].items())),
            },
            "delivery_fence_bind": {
                "comment": (
                    "#5071 relay-tail S4 r2: production "
                    "`with_terminal_delivery_fence` binders, same exclusions as "
                    "identity_fence_bind. This must be the SAME per-file set: the "
                    "checker's pairing pass rejects any file where the two counts "
                    "differ, which is what catches a delivery fence being dropped "
                    "from a site that keeps its identity fence — a DECREASE the "
                    "no-growth ratchet would otherwise wave through."
                ),
                "files": dict(sorted(counts["delivery_fence_bind"].items())),
            },
            "inflight_row_clear_call": {
                "comment": (
                    "#5462 S5: calls to an inflight ROW-DESTRUCTION helper from "
                    "outside the owner module src/services/discord/inflight/"
                    "clear_store/. A spelling count: definitions and re-export "
                    "wrappers are counted, direct fs::remove_file unlinks are "
                    "invisible, and the generation-fenced *_for_reconcile "
                    "wrappers count AS destruction so converting a bare call to "
                    "its fenced form is not scored as a decrease. S5 r2 added "
                    "the three named entry points the design's enumeration "
                    "missed: clear_inflight_state_for_channel, "
                    "archive_inflight_state_if_matches_identity_generation* and "
                    "clear_lifecycle_inflight_state_if_matches*. See the module "
                    "docstring for the full limits."
                ),
                "files": dict(sorted(counts["inflight_row_clear_call"].items())),
            },
            "structural_candidate_apply": {
                "comment": '#5464 T5 S6b-B: per-file structural/warrant counts are a two-sided pairing check, not no-growth; one-sided mismatch and per-file growth fail closed, while paired deletion or count-preserving relocation/replacement remain invisible. See the module docstring.',
                "files": dict(sorted(counts["structural_candidate_apply"].items())),
            },
            "destructive_warrant_bind": {
                "comment": '#5464 T5 S6b-B: per-file warrant/structural counts are a two-sided pairing check, not no-growth; calls do not prove result consumption, argument identity, or control-flow dominance. The stale-sweep behavioral witness enforces veto consumption. See the module docstring.',
                "files": dict(sorted(counts["destructive_warrant_bind"].items())),
            },
            "host_terminate": {
                "comment": HOST_TERMINATE_COMMENT,
                "files": {
                    path: count
                    for path, count in sorted(counts["host_terminate"].items())
                    if path == HOST_TERMINATE_OWNER
                },
            },
        },
    }


def write_baseline(
    path: Path,
    counts: Mapping[str, Mapping[str, int]],
    registry_subcounts: Mapping[str, int],
    measured_sha: str,
) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(_snapshot(counts, registry_subcounts, measured_sha), indent=2) + "\n",
        encoding="utf-8",
    )


def _totals(counts: Mapping[str, Mapping[str, int]]) -> str:
    return ", ".join(
        f"{category}={sum(counts[category].values())}/{len(counts[category])}files"
        for category in CATEGORIES
    )


def main(argv: Sequence[str] | None = None, repo_root: Path = REPO_ROOT) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group(required=True)
    modes.add_argument("--check", action="store_true")
    modes.add_argument("--write-baseline", action="store_true")
    args = parser.parse_args(argv)
    root = Path(repo_root).resolve()
    try:
        counts, registry_subcounts = scan(root)
        if args.write_baseline:
            sha = subprocess.run(
                ["git", "rev-parse", "HEAD"], cwd=root, text=True,
                capture_output=True, check=True,
            ).stdout.strip()
            write_baseline(root / BASELINE_PATH, counts, registry_subcounts, sha)
            print(f"WROTE destructive call-site baseline at {sha}: {_totals(counts)}")
            return 0
        baseline, _payload = load_baseline(root / BASELINE_PATH)
        errors = (
            growth_errors(counts, baseline)
            + owner_only_errors(counts, baseline)
            + pairing_errors(counts)
        )
    except Exception as exc:
        print(f"FAIL: destructive call-site ratchet: {type(exc).__name__}: {exc}", file=sys.stderr)
        return 1
    if errors:
        print("FAIL: destructive call-site growth or pairing violation", file=sys.stderr)
        print("\n".join(f"  - {error}" for error in errors), file=sys.stderr)
        print(REPIN, file=sys.stderr)
        return 1
    print(f"OK: destructive call-site ratchet: {_totals(counts)}. {WARNING}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
