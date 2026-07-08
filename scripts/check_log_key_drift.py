#!/usr/bin/env python3
"""Log field-key drift guard for `src/services/discord/` (#4218).

Tracing log FIELD keys had drifted across the Discord relay: channel identifiers
were logged under `channel = …`, `chan = …`, and `discord_channel_id = …`, and a
handful of session identifiers under `session_id = …`, instead of the canonical
`channel_id = …` / `session_key = …` used by the majority of the relay path.
#4218 unified them. This guard blocks the drift from creeping back: it scans the
Discord service tree for the forbidden log-field keys and fails (non-zero exit,
`file:line` output) if any reappear.

The scope is deliberately narrow — ONLY tracing-macro FIELD keys are policed,
never struct fields, DB columns, SQL text, format-string interpolations
(`"… channel={} …"`), `let` bindings, or plain reassignments.

Detection (on string/comment-stripped code):
  A forbidden key counts as a tracing field when `<key> = <value>` appears with
  the key preceded (ignoring whitespace) by start-of-line, `(`, or `,` — the
  positions a tracing-macro field key can occupy, which covers both the rustfmt
  multi-line form and single-line calls like
  `tracing::info!(channel = id, "msg");` — AND the value is either a sigil
  capture (`%expr` / `?expr`, tracing-only syntax) or terminated at
  bracket-depth 0, before any depth-0 `;`, by a `,` (field separator) or an
  unmatched `)` / `]` / `}` (enclosing call closing after a trailing-comma-less
  LAST field, e.g. `tracing::info!(channel = id);`). `let` bindings
  (`let channel = …`) fail the preceder rule; reassignment statements
  (`session_id = None;`) fail the terminator rule.

String/comment handling: a lightweight cross-line scanner blanks out string
literals (normal strings with escapes, `b"…"`, and `r"…"` / `r#"…"#` raw
strings — all of which may span lines), char literals (so `'"'` cannot desync
quote tracking), `//` line comments, and nested `/* … */` block comments
before matching. This kills false positives from prose such as
`"… channel = foo, …"` inside log messages, SQL text, or doc examples.

Known limits (accepted; this is a grep-grade guard, not a Rust parser): a
field whose VALUE expression spans lines with the terminating `,` on a later
line is not detected, and macro-generated code is out of scope. rustfmt
(enforced by fmt-check in CI) keeps real field lines in the shapes covered
above. Covered and excluded shapes are pinned by `tests/test_log_key_drift.py`
(run in CI next to this guard).

`session_id` carries a small, documented allowlist: a few log sites record a
genuinely different identifier than the relay's `adk_session_key` — the Discord
voice-gateway session (songbird `DriverConnect`/`DriverReconnect`) and the raw
provider-CLI hook session (`HookEvent.session_id`). Renaming those to
`session_key` would mislabel the value, so they are exempt by
`(path-suffix, value-expression)` signature rather than by line number (which
drifts). New `session_id` log fields anywhere else in the Discord tree fail.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

SCAN_ROOT = Path("src/services/discord")

# Canonical replacements, surfaced in the failure message.
CHANNEL_KEYS = ("channel", "chan", "discord_channel_id")
SESSION_KEY = "session_id"

_KEY_ALT = "|".join(re.escape(k) for k in (*CHANNEL_KEYS, SESSION_KEY))
# `<key> = <not-=>` anywhere in stripped code (`[^=]` rejects `==`). The
# position/termination rules live in `_field_violation` — this regex alone is
# deliberately loose.
KEY_ASSIGN = re.compile(rf"\b(?P<key>{_KEY_ALT})\s*=\s*(?P<rest>[^=].*)$")

# Char literal (so `'"'` / `'\"'` cannot desync the quote scanner). Lifetimes
# (`'a`) do not match and fall through harmlessly.
_CHAR_LITERAL = re.compile(r"'(\\.|[^'\\])'")

# Raw / byte string openers: r"…", r#"…"#, br"…", b"…" is handled separately.
_RAW_STRING_OPEN = re.compile(r'(?:r|br)(#*)"')

# `session_id` sites that log a genuinely different identifier than the relay
# `adk_session_key`. Exempt by (path suffix, value expression after `=`).
SESSION_ID_ALLOWLIST: set[tuple[str, str]] = {
    # Discord voice-gateway session id (songbird DriverConnect / DriverReconnect
    # event payload) — the voice WebSocket session, not an agent session.
    ("services/discord/voice_lifecycle.rs", "data.session_id"),
    # Raw provider-CLI hook session id (HookEvent.session_id) observed off the
    # UserPromptSubmit hook — the provider's own session, not adk_session_key.
    ("services/discord/tui_prompt_relay.rs", "%event.session_id"),
}


class StripState:
    """Cross-line lexer state: strings and block comments span lines."""

    __slots__ = ("in_string", "raw_hashes", "block_depth")

    def __init__(self) -> None:
        self.in_string = False  # inside a normal "…" / b"…" string
        self.raw_hashes: int | None = None  # inside r"…" / r#"…"# (hash count)
        self.block_depth = 0  # nested /* … */ depth


def strip_line(line: str, state: StripState) -> str:
    """Blank out string-literal/comment content, preserving column positions.

    Quote and comment delimiters themselves are blanked too — only real code
    survives. `state` carries over between lines so multi-line strings
    (including `\\`-newline continuations) and block comments stay stripped.
    """
    out: list[str] = []
    i = 0
    n = len(line)
    while i < n:
        if state.block_depth > 0:
            if line.startswith("/*", i):
                state.block_depth += 1
                out.append("  ")
                i += 2
            elif line.startswith("*/", i):
                state.block_depth -= 1
                out.append("  ")
                i += 2
            else:
                out.append(" ")
                i += 1
            continue
        if state.raw_hashes is not None:
            closer = '"' + "#" * state.raw_hashes
            if line.startswith(closer, i):
                state.raw_hashes = None
                out.append(" " * len(closer))
                i += len(closer)
            else:
                out.append(" ")
                i += 1
            continue
        if state.in_string:
            if line[i] == "\\" and i + 1 < n:
                out.append("  ")
                i += 2
            else:
                if line[i] == '"':
                    state.in_string = False
                out.append(" ")
                i += 1
            continue
        # --- normal code ---
        if line.startswith("//", i):
            break  # line comment: drop the rest of the line
        if line.startswith("/*", i):
            state.block_depth = 1
            out.append("  ")
            i += 2
            continue
        raw = _RAW_STRING_OPEN.match(line, i)
        if raw:
            state.raw_hashes = len(raw.group(1))
            out.append(" " * (raw.end() - i))
            i = raw.end()
            continue
        if line[i] == '"' or line.startswith('b"', i):
            skip = 2 if line[i] == "b" else 1
            state.in_string = True
            out.append(" " * skip)
            i += skip
            continue
        if line[i] == "'":
            m = _CHAR_LITERAL.match(line, i)
            if m:
                out.append(" " * (m.end() - i))
                i = m.end()
                continue
        out.append(line[i])
        i += 1
    return "".join(out)


def _field_violation(code: str, match: re.Match[str]) -> str | None:
    """Return the field's value expression if this match is a tracing field.

    Preceder rule: the key must sit at line start or right after `(` / `,`
    (ignoring whitespace) — the only places a tracing field key can appear.
    This excludes `let <key> = …`, `x.<key> = …`, `=> <key> = …`, etc.

    Terminator rule: sigil values (`%`/`?`) are tracing-only syntax and count
    immediately; otherwise the value must be closed — at bracket-depth 0,
    before any depth-0 `;` — by either a `,` (field separator) or an unmatched
    closer `)` / `]` / `}` (the enclosing macro/`fields(…)` list ending right
    after the LAST field, which single-line calls like
    `tracing::info!(channel = id);` write without a trailing comma; codex r2).
    Treating the closer as a terminator is safe because the preceder rule has
    already pinned the key to a call-argument position, and plain Rust has no
    named arguments — `(key = value)` outside a macro would be an
    assignment-expression-in-parens, which real code does not write.
    Statements end with `;` and stay excluded.
    """
    before = code[: match.start()].rstrip()
    if before and before[-1] not in "(,":
        return None

    rest = match.group("rest")
    stripped = rest.lstrip()
    if stripped.startswith("%") or stripped.startswith("?"):
        value = _terminated_value(rest)
        # Sigil syntax is tracing-only: still a violation even if no same-line
        # terminator is found (e.g. a multi-line sigil value).
        return value if value is not None else stripped
    return _terminated_value(rest)


def _terminated_value(rest: str) -> str | None:
    """Return the value slice if `rest` closes like a tracing field on this line.

    Scans with bracket-depth tracking: a depth-0 `,` (field separator) or a
    depth-0 unmatched closer (`)`, `]`, `}` — the enclosing call ending after
    the last field) terminates the value; a depth-0 `;` means a statement.
    Matched pairs inside the value (`Some(x)`, `pair[1]`, `id.get()`) pass
    through: their closers drop the depth back without hitting -1.
    """
    depth = 0
    for pos, ch in enumerate(rest):
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            if depth == 0:
                return rest[:pos].strip()  # enclosing call closed: last field
            depth -= 1
        elif depth == 0 and ch == ";":
            return None  # plain (re)assignment statement
        elif depth == 0 and ch == ",":
            return rest[:pos].strip()
    return None  # no same-line terminator — statement or multi-line value


def _canonical_hint(key: str) -> str:
    if key in CHANNEL_KEYS:
        return "channel_id"
    return "session_key"


def scan(repo_root: Path) -> list[tuple[str, int, str, str]]:
    """Return (relpath, lineno, key, source-line) for every forbidden log field."""
    violations: list[tuple[str, int, str, str]] = []
    root = repo_root / SCAN_ROOT
    for path in sorted(root.rglob("*.rs")):
        rel = path.relative_to(repo_root)
        if "target" in rel.parts:
            continue
        rel_str = rel.as_posix()
        state = StripState()
        for lineno, raw in enumerate(
            path.read_text(encoding="utf-8").splitlines(), start=1
        ):
            code = strip_line(raw, state)
            search_from = 0
            while True:
                match = KEY_ASSIGN.search(code, search_from)
                if not match:
                    break
                search_from = match.start() + 1
                value = _field_violation(code, match)
                if value is None:
                    continue
                key = match.group("key")
                if key == SESSION_KEY and any(
                    rel_str.endswith(suffix) and value == allowed_val
                    for suffix, allowed_val in SESSION_ID_ALLOWLIST
                ):
                    continue
                violations.append((rel_str, lineno, key, raw.strip()))
    return violations


def main() -> int:
    repo_root = Path(__file__).resolve().parent.parent
    violations = scan(repo_root)

    if violations:
        print(
            f"FAIL: {len(violations)} forbidden log field key(s) in "
            f"{SCAN_ROOT}/ (#4218 drift).",
            file=sys.stderr,
        )
        print(
            "      Tracing log field keys must be canonical: "
            "channel/chan/discord_channel_id -> channel_id, session_id -> "
            "session_key (relay session). Struct fields, DB columns, SQL text, "
            "format strings, and `let` bindings are NOT policed — only tracing "
            "field keys are.",
            file=sys.stderr,
        )
        for rel, lineno, key, src in violations:
            print(
                f"        {rel}:{lineno}: `{key} =` -> `{_canonical_hint(key)} =`"
                f"    {src}",
                file=sys.stderr,
            )
        return 1

    print(f"OK: no forbidden log field keys in {SCAN_ROOT}/ (#4218).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
