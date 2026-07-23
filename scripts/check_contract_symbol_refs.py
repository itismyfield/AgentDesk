#!/usr/bin/env python3

"""Doc<->code sync gate for relay-state contract symbol anchors (#4268).

Background: ``docs/relay-state-contract.md`` used ``file:line`` hard references
that module decomposition silently broke. Symbol-path references fixed the line
drift, but a *text* gate that judged whether a Rust symbol still exists kept
losing to raw strings, macros, cfg-gated items, and trait/impl matching — every
round found a new regex bypass. Judging Rust definitions with regex is the wrong
tool.

Round-3 review found the deeper hole: the previous version derived the Rust
anchor SET from ``// sym:`` *comments*. A comment is not compiled, so you could
comment out (or ``use super::*;``-replace) the real reference and keep the
``// sym:`` label — the set comparison still passed and ``cargo check`` no longer
proved anything. The label and the compiled reference were unbound.

This gate closes that: it parses the anchor set **from the compiler-checked code
itself**, never from comments. There are no ``// sym:`` labels anymore.

* **Existence is proven by the compiler.** Each anchor is a real Rust reference
  inside a ``#[cfg(test)] mod relay_state_contract_refs`` block:

  - ``use <path> as _;`` for functions/items,
  - ``let _ = <Type>::<assoc_fn>;`` for associated functions,
  - ``let _ = |x: &<Type>| { let _ = &x.<field>; };`` for fields (``use`` cannot
    name a field).

  Each fails to COMPILE if its symbol is renamed/moved/removed.
  ``cargo check --workspace --all-targets`` — an already-required CI gate —
  compiles those blocks, so the compiler is the source of truth for existence.
  Raw strings, macros, and cfg items cannot fool a real compile.

* **The anchor NAME is parsed from that same code**, not a comment. The parser
  reads the ``use`` path / field expression, resolves ``super::`` /
  ``crate::services::`` to the canonical doc path (with the historical
  ``discord::`` segment omitted), and that resolved path is the anchor. Comment
  out the reference and the anchor vanishes with it;
  the set comparison then fails. There is no label left to lie.

* **Block cfg and item attributes are byte-exact whitelists, not parsed.** The
  block's gate must be one of ``_ALLOWED_ANCHOR_CFGS`` (``#[cfg(test)]`` or
  ``#[cfg(all(test, unix))]``) and every attribute INSIDE the block must be
  ``#[test]`` (``_ALLOWED_ITEM_ATTRS``). ``unix`` is allowed because the only
  required PR Rust compile is ``check_fast`` (ubuntu-latest), where ``cfg(unix)``
  is true, so that required job compiles the block. Anything else fails loudly —
  a windows/non-ubuntu gate (compiled by no required job), a malformed cfg, or an
  item-level ``#[cfg(feature = "never")]`` that would drop one reference while
  the block survives. There is no cfg grammar/evaluator: this PR's history is
  that every added cfg parser sprouted a new bypass within a round, so we removed
  interpretation and enumerate the exact allowed spellings instead.

* **All Rust-source matching runs on comment/string-stripped text** (r6).
  ``_strip_comments_and_strings`` blanks ``//`` comments, nested ``/* */``
  comments, and (raw) string literals with same-length whitespace before block
  discovery, the attribute walk, and reference matching. So a comment cannot
  break the attribute walk (rustc attaches attributes through comments/blank
  lines — an illegal cfg hidden above a comment is still collected and fails), a
  fake block inside a raw string/block comment cannot shadow the real one, and a
  block-commented reference stops counting as an anchor (set mismatch, loud
  FAIL).

* **Doc<->code agreement is proven here** by a cheap, exact set comparison: the
  distinct ``sym:`` anchors in the doc must equal the distinct anchors parsed
  from the reference blocks. This script never parses Rust *definitions*, only
  the reference expressions, so there is nothing for a raw string / macro / cfg
  to bypass.

The round-2 "mislabeled comment" limitation is GONE: the anchor is now the
symbol the code actually references, so a wrong label is impossible — there is
no label.

Threat model (r7 — read before "hardening" this further): this gate defends
against DRIFT and honest mistakes — decomposition moves, renames, accidental
comment-outs, refactors that nest the anchor module under a cfg'd parent. It
does NOT defend against a deliberate in-repo saboteur: anyone who can commit
adversarial Rust (macro decoys, lexer traps) can just as easily edit this
checker, the whitelists, or the doc itself, so an in-repo gate cannot beat an
in-repo attacker even in principle. Do not grow this checker to chase such
constructs; see PR #4388's seven review rounds for why every added parser layer
became new attack surface.
"""

from __future__ import annotations

import argparse
import re
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DISCORD_ROOT = REPO_ROOT / "src" / "services" / "discord"

DEFAULT_DOC = REPO_ROOT / "docs" / "relay-state-contract.md"

# Rust files hosting a `#[cfg(test)] mod relay_state_contract_refs` block, mapped
# to the module path (from `src/services/discord/`) of the FILE that hosts the
# block — i.e. the parent module of `relay_state_contract_refs`. `super::` inside
# the block resolves to this path; each extra `super::` drops one trailing
# component. Blocks are split by visibility island: many contract symbols are
# `pub(super)`/private and are only nameable from within their own module
# subtree, so the references are co-located there rather than in one central
# module.
REFERENCE_SOURCE_MODULES: dict[str, tuple[str, ...]] = {
    "inflight/store.rs": ("inflight", "store"),
    "turn_bridge/terminal_delivery.rs": ("turn_bridge", "terminal_delivery"),
    "tmux_watcher/liveness.rs": ("tmux_watcher", "liveness"),
    "router/message_handler/watchdog.rs": ("router", "message_handler", "watchdog"),
    "mailbox_finish.rs": ("mailbox_finish",),
    "session_relay_sink.rs": ("session_relay_sink",),
}

DEFAULT_REFERENCE_SOURCES: tuple[Path, ...] = tuple(
    DISCORD_ROOT / rel for rel in REFERENCE_SOURCE_MODULES
)

# Absolute-path prefix that names items from the services root. Stripped so
# `crate::services::discord::tmux::advance_watcher_confirmed_end` yields the doc
# anchor `tmux::advance_watcher_confirmed_end`, while service siblings such as
# `crate::services::provider::CancelToken::turn_nonce` retain `provider::...`.
SERVICES_PREFIX: tuple[str, ...] = ("crate", "services")

# Distinct-anchor floor (defense in depth). Set comparison already fails if the
# doc and code diverge, but a synchronized gutting of BOTH to a couple of anchors
# would compare "equal" while covering nothing — the floor rejects that. Never
# lower this (#4269).
MIN_CONTRACT_ANCHORS = 20

ANCHOR_MOD_NAME = "relay_state_contract_refs"

# Doc anchors: inline `sym:PATH` code spans. The `<`-free character class means a
# placeholder like `sym:<module>::<Symbol>` in prose is never captured.
DOC_ANCHOR_RE = re.compile(r"`sym:([A-Za-z0-9_:]+)`")

# --- Rust reference-expression forms (parsed from CODE, never from comments) ---
# A `//`-prefixed line can never match any of these (they all require `use`/`let`
# as the first token), so comments are structurally incapable of being anchors.
#
# `use <path> as _;` — functions/items.
_USE_RE = re.compile(r"^\s*use\s+([A-Za-z0-9_:]+)\s+as\s+_\s*;", re.MULTILINE)
# `let _ = |x: &<Type>| { let _ = &x.<field>; };` — fields (rustfmt may wrap it
# across lines, hence DOTALL). Compiler-enforces field existence.
_FIELD_RE = re.compile(
    r"let\s+_\s*=\s*\|\s*\w+\s*:\s*&\s*([A-Za-z0-9_:]+)\s*\|\s*\{"
    r"\s*let\s+_\s*=\s*&\s*\w+\.([A-Za-z0-9_]+)\s*;\s*\}",
    re.DOTALL,
)
# `let _ = <Type>::<assoc_fn>;` — associated functions (`use` cannot name them).
# Must run AFTER the field regex has been stripped so it never swallows a closure.
_ASSOC_RE = re.compile(r"^\s*let\s+_\s*=\s*([A-Za-z0-9_:]+)\s*;", re.MULTILINE)

# Up to 3 leading spaces still opens a fenced block (CommonMark); 4 would be an
# indented code block instead, handled separately below.
_FENCE_RE = re.compile(r"^\s{0,3}(?:```|~~~)")

_MOD_RE = re.compile(rf"^(\s*)mod\s+{ANCHOR_MOD_NAME}\b")

# Char literal for the stripper (r7): quote + (escape sequence incl. \u{…}/\xNN
# or one non-quote/non-backslash char) + quote. Anything else after `'` is a
# lifetime and is NOT consumed.
_CHAR_LIT_RE = re.compile(r"'(?:\\(?:u\{[0-9a-fA-F_]{1,6}\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])'")
_ATTR_RE = re.compile(r"^\s*#\!?\[(.*)\]\s*$")
# Detects an attribute line by its START (`#[` / `#![`), so it catches an item
# attribute whether on its own line or inline before an item.
_ATTR_START_RE = re.compile(r"^\s*#!?\[")

# The ONLY cfg forms an anchor block's gate may take, matched BYTE-EXACT after
# strip() — no grammar, no parser. A converged lesson of this PR (#4268) is that
# every added cfg parser sprouts a new bypass within a round (regex, then
# recursive-descent, then an evaluator), while removing interpretation holds.
# `unix` is allowed because the only REQUIRED PR Rust compile is `check_fast`
# (ci-pr.yml matrix `os: [ubuntu-latest]`), where `cfg(unix)` is true, so that
# required job compiles the block and a vanished symbol fails it. A windows-only
# or non-ubuntu gate would compile in NO required job (the windows lane is
# advisory and skipped for relay-only changes), silently disabling the proof. If
# a required windows test lane is ever added, extend this set in the same commit
# that wires it. `all(test, target_os = "linux")` and friends are deliberately
# NOT here: one canonical spelling per gate keeps the policy readable.
_ALLOWED_ANCHOR_CFGS = frozenset(
    {
        "#[cfg(test)]",
        "#[cfg(all(test, unix))]",
    }
)

# Inside a block, the ONLY attribute an item may carry is `#[test]` (the
# `contract_symbols_exist` wrapper). An item-level cfg — e.g.
# `#[cfg(feature = "never")]` on a `use super::X as _;` — keeps the block alive
# (so a block-cfg check passes) while dropping that ONE reference from the
# required compile, so a vanished symbol raises no E0432. Whitelisting item
# attributes to `#[test]` seals that hole (#4268 r5).
_ALLOWED_ITEM_ATTRS = frozenset({"#[test]"})


@dataclass(frozen=True)
class ContractRefReport:
    doc_anchors: frozenset[str]
    rust_anchors: frozenset[str]
    errors: tuple[str, ...]
    min_anchors: int

    @property
    def missing_in_code(self) -> frozenset[str]:
        """Doc anchors with no compiler-checked reference."""
        return frozenset(self.doc_anchors - self.rust_anchors)

    @property
    def missing_in_doc(self) -> frozenset[str]:
        """Compiler-checked references not documented as `sym:` anchors."""
        return frozenset(self.rust_anchors - self.doc_anchors)

    @property
    def distinct_count(self) -> int:
        return min(len(self.doc_anchors), len(self.rust_anchors))

    @property
    def below_floor(self) -> bool:
        return self.distinct_count < self.min_anchors

    def is_clean(self) -> bool:
        return not (
            self.errors
            or self.missing_in_code
            or self.missing_in_doc
            or self.below_floor
        )


def extract_doc_anchors(text: str) -> set[str]:
    """Distinct `sym:` anchors from doc prose, excluding ALL code blocks.

    Both fenced (``` / ~~~) and indented (4-space / tab, CommonMark-opened after
    a blank line) code blocks are skipped, so an example anchor placed in either
    kind of block cannot inflate the anchor set.
    """

    anchors: set[str] = set()
    in_fence = False
    in_indent_code = False
    prev_blank = True  # start of document behaves like "after a blank line"
    for line in text.splitlines():
        is_blank = line.strip() == ""

        if _FENCE_RE.match(line):
            in_fence = not in_fence
            prev_blank = False
            continue
        if in_fence:
            prev_blank = is_blank
            continue

        indented = line.startswith("    ") or line.startswith("\t")
        if in_indent_code:
            if is_blank or indented:
                prev_blank = is_blank
                continue
            in_indent_code = False
        if not is_blank and indented and prev_blank:
            in_indent_code = True
            prev_blank = False
            continue

        prev_blank = is_blank
        if is_blank:
            continue
        for match in DOC_ANCHOR_RE.finditer(line):
            anchors.add(match.group(1))
    return anchors


def _resolve_symbol(path: str, module_base: tuple[str, ...]) -> str:
    """Resolve a Rust reference path to its canonical doc-anchor path.

    `super::X`  (from a `relay_state_contract_refs` block whose parent module is
    `module_base`) -> `<module_base>::X`; every extra leading `super::` drops one
    trailing component of `module_base`. `crate::services::discord::X` -> `X`,
    and `crate::services::provider::X` -> `provider::X`.
    """

    segs = path.split("::")
    if segs[0] == "crate":
        if tuple(segs[: len(SERVICES_PREFIX)]) != SERVICES_PREFIX:
            raise ValueError(
                f"anchor path {path!r} is crate-absolute but not under "
                f"{'::'.join(SERVICES_PREFIX)}"
            )
        resolved = segs[len(SERVICES_PREFIX) :]
        if resolved and resolved[0] == "discord":
            resolved = resolved[1:]
        return "::".join(resolved)
    if segs[0] == "super":
        supers = 0
        while supers < len(segs) and segs[supers] == "super":
            supers += 1
        keep = len(module_base) - (supers - 1)
        if keep < 0:
            raise ValueError(
                f"anchor path {path!r} has more super:: hops than the host "
                f"module {'::'.join(module_base)} has components"
            )
        return "::".join(list(module_base[:keep]) + segs[supers:])
    # A bare path (no super/crate prefix) is assumed already canonical.
    return path


def _strip_comments_and_strings(text: str) -> str:
    """Lexer preprocessing (#4268 r6): blank out comments and string literals so
    every downstream match runs on code the compiler actually sees.

    Replaces `// …` line comments, `/* … */` block comments (NESTED — Rust block
    comments nest, so a depth counter, not a find), `"…"` strings (with `\\`
    escape handling), and raw strings `r"…"` / `r#"…"#` (any hash count — the
    closer is `"` plus exactly the opening hash run) with same-length whitespace,
    preserving newlines so line structure survives. Char literals (`'a'`, `'"'`,
    `'\\''`, `'\\u{1F600}'`) are blanked too — a quote char left unlexed desyncs
    the string state and can hide a following attribute line (#4268 r7). A lone
    `'` that does not complete a char literal is a lifetime and stays as code.

    Without this pass the checker matched raw text: a comment line broke the
    attribute walk (hiding an illegal cfg above it), a fake block inside a raw
    string/block comment could shadow the real one, and a block-commented `use`
    still counted as an anchor.
    """

    out = list(text)
    n = len(text)

    def blank(a: int, b: int) -> None:
        for k in range(a, min(b, n)):
            if out[k] != "\n":
                out[k] = " "

    i = 0
    while i < n:
        # Literal prefix (r8): a `b`/`c` at token start before a char/string/raw
        # opener is part of the token (`b'x'`, `b"…"`, `br#"…"#`, `c"…"`,
        # `cr"…"`). Fold it into the blanked span — r7 lexed only the bare
        # `'x'`/`"…"`, leaving a stray prefix letter that downstream regexes
        # collected as a phantom anchor symbol.
        pfx = 0
        if (
            text[i] in ("b", "c")
            and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_"))
            and i + 1 < n
            and (
                text[i + 1] in ("'", '"')
                or (text[i + 1] == "r" and i + 2 < n and text[i + 2] in ('"', "#"))
            )
        ):
            pfx = 1
        p = i + pfx
        ch = text[p]
        nxt = text[p + 1] if p + 1 < n else ""
        if ch == "/" and nxt == "/":
            j = text.find("\n", i)
            j = n if j == -1 else j
            blank(i, j)
            i = j
        elif ch == "/" and nxt == "*":
            depth = 1
            j = i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth += 1
                    j += 2
                elif text.startswith("*/", j):
                    depth -= 1
                    j += 2
                else:
                    j += 1
            blank(i, j)  # unterminated comment blanks to EOF (fail-closed)
            i = j
        elif ch == "r" and nxt in ('"', "#"):
            j = p + 1
            hashes = 0
            while j < n and text[j] == "#":
                hashes += 1
                j += 1
            if j < n and text[j] == '"':
                closer = '"' + "#" * hashes
                k = text.find(closer, j + 1)
                k = n if k == -1 else k + len(closer)
                blank(i, k)
                i = k
            else:
                i += 1  # raw identifier like r#fn — not a string, leave it
        elif ch == '"':
            j = p + 1
            while j < n:
                if text[j] == "\\":
                    j += 2
                elif text[j] == '"':
                    j += 1
                    break
                else:
                    j += 1
            blank(i, j)
            i = j
        elif ch == "'":
            # Char literal (standard Rust lexing): `'` + (escape or one non-`'`
            # char) + `'` — blank it so a quote inside (`'"'`) cannot desync the
            # string state (r7). No match => lifetime (`'a`), leave as code.
            m = _CHAR_LIT_RE.match(text, p)
            if m:
                blank(i, m.end())
                i = m.end()
            else:
                i += 1
        else:
            i += 1
    return "".join(out)


def _find_anchor_block(text: str, rel: str) -> tuple[str | None, list[str]]:
    """Return (block_body, errors) for the single `relay_state_contract_refs`
    module in `text`, verifying its cfg gate is one of ``_ALLOWED_ANCHOR_CFGS``
    (byte-exact) and that no item inside carries a disallowed attribute.
    `text` must already be comment/string-stripped (see
    ``_strip_comments_and_strings``); ``extract_rust_anchors`` does this."""

    lines = text.splitlines()
    errors: list[str] = []
    # Uniqueness (r7): exactly ONE declaration per host file. Two or more means a
    # decoy exists (e.g. an unexpanded macro_rules! transcriber) and first-match
    # adoption could validate the wrong one; zero means the host lost its block.
    mod_matches = [
        (idx, m) for idx, line in enumerate(lines) if (m := _MOD_RE.match(line))
    ]
    if not mod_matches:
        errors.append(f"{rel}: no `mod {ANCHOR_MOD_NAME}` block found")
        return None, errors
    if len(mod_matches) > 1:
        errors.append(
            f"{rel}: `mod {ANCHOR_MOD_NAME}` appears {len(mod_matches)} times "
            f"(lines {[idx + 1 for idx, _ in mod_matches]}); it must appear "
            f"exactly once per host file — a duplicate is a decoy the checker "
            f"could validate instead of the real block (#4268 r7)"
        )
        return None, errors
    mod_line_idx, mod_match = mod_matches[0]
    # Top-level (r7): an indented declaration is nested inside another item and
    # can inherit an ancestor cfg (e.g. `#[cfg(feature = "never")] mod parent`)
    # that silently disables the compiler proof.
    if mod_match.group(1) != "":
        errors.append(
            f"{rel}: `mod {ANCHOR_MOD_NAME}` is indented (nested) — the anchor "
            f"module must be declared at file top level; nesting can inherit an "
            f"ancestor cfg that silently disables the compiler proof (#4268 r7)"
        )
        return None, errors

    # The contiguous attribute line(s) directly above `mod` must be exactly one
    # whitelisted cfg gate — byte-exact, no parsing. Rust attaches attributes
    # through blank lines and comments, and comments are already blanked by the
    # stripper, so skipping whitespace-only lines here walks exactly the lines
    # rustc walks: a `#[cfg(feature = "never")]` hidden above a comment (or a
    # `#[cfg_attr(…)]` above a blank line) IS collected and fails the whitelist.
    attrs_above: list[str] = []
    j = mod_line_idx - 1
    while j >= 0:
        stripped = lines[j].strip()
        if stripped == "":
            j -= 1
            continue
        if _ATTR_RE.match(lines[j]):
            attrs_above.append(stripped)
            j -= 1
            continue
        break
    if len(attrs_above) != 1 or attrs_above[0] not in _ALLOWED_ANCHOR_CFGS:
        errors.append(
            f"{rel}: `mod {ANCHOR_MOD_NAME}` must be gated by exactly one of "
            f"{sorted(_ALLOWED_ANCHOR_CFGS)} (byte-exact after strip), found "
            f"{attrs_above} — any other cfg (feature/non-test, malformed, or a "
            f"platform gate false on the required ubuntu compile) can drop the "
            f"block from every required CI job and silently disable the proof"
        )

    # Brace-match the module body. Comments and strings are already blanked by
    # the stripper, so every brace counted here is a real code brace.
    depth = 0
    started = False
    body: list[str] = []
    for line in lines[mod_line_idx:]:
        opens = line.count("{")
        closes = line.count("}")
        if started:
            body.append(line)
        depth += opens - closes
        if opens and not started:
            started = True
        if started and depth <= 0:
            break
    if not started or depth > 0:
        errors.append(f"{rel}: could not brace-match `mod {ANCHOR_MOD_NAME}` body")
        return None, errors
    # Drop the trailing closing-brace line captured by the loop.
    content = body[:-1] if body else []

    # Item-level attribute whitelist. Only `#[test]` may appear inside the block;
    # an item-level cfg (e.g. `#[cfg(feature = "never")]` on a `use`) would keep
    # the block alive but silently drop that reference from the required compile,
    # so a vanished symbol raises no E0432. Match by attribute START so an inline
    # `#[cfg(...)] use ...;` is caught too.
    for content_line in content:
        if _ATTR_START_RE.match(content_line) and content_line.strip() not in _ALLOWED_ITEM_ATTRS:
            errors.append(
                f"{rel}: disallowed attribute {content_line.strip()!r} inside "
                f"`mod {ANCHOR_MOD_NAME}` — only {sorted(_ALLOWED_ITEM_ATTRS)} is "
                f"permitted. An item-level cfg can silently drop a reference from "
                f"the required compile while the block still exists (#4268 r5)."
            )

    return "\n".join(content), errors


def extract_rust_anchors(text: str, module_base: tuple[str, ...], rel: str = "") -> tuple[set[str], list[str]]:
    """Parse the anchor set from the compiler-checked reference block in `text`.

    The text is first comment/string-stripped (r6), so block discovery, the
    attribute walk, and reference matching all see only the code the compiler
    sees: a fake block in a raw string vanishes, a comment cannot hide an
    attribute, and a commented-out reference stops counting. Anchors come from
    `use`/field/assoc-fn expressions only. Returns (anchors, errors).
    """

    anchors: set[str] = set()
    errors: list[str] = []
    text = _strip_comments_and_strings(text)
    body, block_errors = _find_anchor_block(text, rel)
    errors.extend(block_errors)
    if body is None:
        return anchors, errors

    def resolve(path: str, kind: str) -> None:
        try:
            anchors.add(_resolve_symbol(path, module_base))
        except ValueError as exc:
            errors.append(f"{rel}: {kind} {exc}")

    # Fields first, then strip them so the assoc-fn regex cannot swallow a
    # closure that happens to start `let _ =`.
    residual = body
    for match in _FIELD_RE.finditer(body):
        type_path, field = match.group(1), match.group(2)
        resolve(f"{type_path}::{field}", "field")
    residual = _FIELD_RE.sub("", body)
    for match in _USE_RE.finditer(residual):
        resolve(match.group(1), "use")
    for match in _ASSOC_RE.finditer(residual):
        resolve(match.group(1), "assoc-fn")
    return anchors, errors


def build_report(
    *,
    doc_anchors: set[str] | None = None,
    rust_anchors: set[str] | None = None,
    doc: Path = DEFAULT_DOC,
    rust_sources: dict[Path, tuple[str, ...]] | None = None,
    min_anchors: int = MIN_CONTRACT_ANCHORS,
) -> ContractRefReport:
    errors: list[str] = []

    if doc_anchors is None:
        if doc.is_file():
            doc_anchors = extract_doc_anchors(doc.read_text(encoding="utf-8"))
        else:
            doc_anchors = set()
            errors.append(f"contract doc is missing: {_rel(doc)}")

    if rust_anchors is None:
        if rust_sources is None:
            rust_sources = {
                DISCORD_ROOT / rel: base
                for rel, base in REFERENCE_SOURCE_MODULES.items()
            }
        rust_anchors = set()
        for source, module_base in rust_sources.items():
            if source.is_file():
                found, src_errors = extract_rust_anchors(
                    source.read_text(encoding="utf-8"), module_base, _rel(source)
                )
                rust_anchors |= found
                errors.extend(src_errors)
            else:
                errors.append(f"reference source is missing: {_rel(source)}")

    return ContractRefReport(
        doc_anchors=frozenset(doc_anchors),
        rust_anchors=frozenset(rust_anchors),
        errors=tuple(errors),
        min_anchors=min_anchors,
    )


def _rel(path: Path) -> str:
    try:
        return path.resolve().relative_to(REPO_ROOT).as_posix()
    except ValueError:
        return path.as_posix()


def format_report(report: ContractRefReport) -> str:
    if report.is_clean():
        return (
            f"contract symbol-ref check passed "
            f"({len(report.doc_anchors)} anchors, doc<->code in sync)"
        )

    lines = ["Contract symbol-ref drift detected:"]
    for error in report.errors:
        lines.append(f"  - {error}")
    for anchor in sorted(report.missing_in_code):
        lines.append(
            f"  - `sym:{anchor}` is documented but has no compiler-checked "
            f"reference in a `{ANCHOR_MOD_NAME}` block"
        )
    for anchor in sorted(report.missing_in_doc):
        lines.append(
            f"  - `{anchor}` is referenced in code but not documented as a "
            f"`sym:` anchor in the contract"
        )
    if report.below_floor:
        lines.append(
            f"  - only {report.distinct_count} distinct anchors "
            f"(floor is {report.min_anchors}); contract anchors look gutted"
        )
    lines.append("")
    lines.append(
        "Fix: keep the `sym:` anchors in docs/relay-state-contract.md and the "
        f"compiler-checked references in the `{ANCHOR_MOD_NAME}` blocks in exact "
        "1:1 correspondence. The reference blocks are what `cargo check "
        "--all-targets` compiles to prove each symbol still exists; the anchor "
        "name is parsed from the reference expression, not from any comment."
    )
    return "\n".join(lines)


def _parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Contract symbol-ref sync gate (#4268).")
    parser.add_argument("--doc", type=Path, default=DEFAULT_DOC, help="Contract doc path.")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = _parse_args(argv)
    report = build_report(doc=args.doc)
    print(format_report(report))
    return 0 if report.is_clean() else 1


if __name__ == "__main__":
    raise SystemExit(main())
