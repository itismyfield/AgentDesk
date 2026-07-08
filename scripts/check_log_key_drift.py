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
(`"… channel={} …"`), `let` bindings, or plain reassignments. Detection matches
the two shapes a tracing field key actually takes:

  * canonical rustfmt multi-line form — ``<key> = <value>,`` on its own line, and
  * the sigil form — ``<key> = %expr`` / ``<key> = ?expr`` (Display / Debug),

both of which are unambiguous tracing syntax. Statements (`;`-terminated),
`let`/`x.field` bindings, and string-literal interpolations do not match either.

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

# Multi-line tracing field form: `<key> = <value>,` on its own line.
#   - `^\s*<key>` anchors the key at line start → excludes `let <key> =`,
#     `x.<key> =`, and `"… <key>=…"` string interpolations.
#   - `[^=]` after `=` excludes the `==` comparison operator.
#   - trailing `,` is the tracing field separator → excludes `;`-terminated
#     statements (e.g. `session_id = None;`) and struct-init `:` fields.
_KEY_ALT = "|".join(re.escape(k) for k in (*CHANNEL_KEYS, SESSION_KEY))
MULTILINE_FIELD = re.compile(rf"^\s*(?P<key>{_KEY_ALT})\s*=\s*(?P<val>[^=].*),\s*$")

# Sigil form: `<key> = %expr` / `<key> = ?expr` — Display/Debug capture. `\b`
# keeps `channel` from matching inside `channel_id`. `= %` / `= ?` is tracing-only
# syntax (a bare `%`/`?` cannot open a normal Rust r-value expression).
SIGIL_FIELD = re.compile(rf"\b(?P<key>{_KEY_ALT})\s*=\s*(?P<val>[%?]\S.*?)\s*,?\s*$")

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
        for lineno, raw in enumerate(
            path.read_text(encoding="utf-8").splitlines(), start=1
        ):
            if raw.lstrip().startswith("//"):
                continue
            # Drop trailing line comments so prose can mention old keys freely.
            code = raw.split("//", 1)[0]

            match = MULTILINE_FIELD.match(code) or SIGIL_FIELD.search(code)
            if not match:
                continue
            key = match.group("key")
            value = match.group("val").strip().rstrip(",").strip()

            if key == SESSION_KEY:
                if any(
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
