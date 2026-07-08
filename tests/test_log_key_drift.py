"""Unit tests for scripts/check_log_key_drift.py (#4218).

Pins the detection contract of the log field-key drift guard:
  * both violation shapes — rustfmt multi-line field lines AND inline
    single-line macro calls (codex r1: inline forms were previously missed);
  * string-literal / comment stripping (codex r1: prose like
    `"channel = foo,"` inside strings previously false-positived);
  * the statement/binding exclusions and the session_id allowlist.
"""

from __future__ import annotations

import importlib.util
import sys
import textwrap
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / "scripts" / "check_log_key_drift.py"

_SPEC = importlib.util.spec_from_file_location("check_log_key_drift", SCRIPT_PATH)
CHECKER = importlib.util.module_from_spec(_SPEC)
assert _SPEC.loader is not None
sys.modules[_SPEC.name] = CHECKER
_SPEC.loader.exec_module(CHECKER)


def _scan_fixture(body: str, rel: str = "src/services/discord/probe.rs"):
    """Write one fixture file into a temp repo root and scan it."""
    with TemporaryDirectory() as tmp:
        root = Path(tmp)
        target = root / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(textwrap.dedent(body).lstrip("\n"), encoding="utf-8")
        return CHECKER.scan(root)


def _keys(violations) -> list[str]:
    return [key for (_rel, _line, key, _src) in violations]


class MultiLineFieldDetectionTest(unittest.TestCase):
    """The original rustfmt multi-line shapes stay covered."""

    def test_all_forbidden_keys_flagged_in_multiline_form(self) -> None:
        violations = _scan_fixture(
            """
            fn probe() {
                tracing::info!(
                    channel = channel_id.get(),
                    chan = x,
                    discord_channel_id = y,
                    session_id = other.session_id,
                    provider = %p,
                    "probe"
                );
            }
            """
        )
        self.assertEqual(
            _keys(violations),
            ["channel", "chan", "discord_channel_id", "session_id"],
        )

    def test_shorthand_and_canonical_keys_pass(self) -> None:
        violations = _scan_fixture(
            """
            fn probe() {
                tracing::info!(
                    channel_id = channel_id.get(),
                    channel_id,
                    session_key = trace.session_key().unwrap_or(""),
                    "probe"
                );
            }
            """
        )
        self.assertEqual(violations, [])


class InlineMacroDetectionTest(unittest.TestCase):
    """codex r1 gap 1: single-line macro calls must be caught too."""

    def test_inline_single_line_macro_non_sigil_flagged(self) -> None:
        violations = _scan_fixture(
            """
            fn probe() {
                tracing::info!(channel = channel_id, "msg");
            }
            """
        )
        self.assertEqual(_keys(violations), ["channel"])

    def test_inline_sigil_flagged(self) -> None:
        violations = _scan_fixture(
            """
            fn probe() {
                tracing::debug!(channel = %cid, "inline sigil");
            }
            """
        )
        self.assertEqual(_keys(violations), ["channel"])

    def test_mid_line_field_after_comma_flagged(self) -> None:
        violations = _scan_fixture(
            """
            fn probe() {
                tracing::info!(
                    provider = x, channel = y,
                    "msg"
                );
            }
            """
        )
        self.assertEqual(_keys(violations), ["channel"])

    def test_instrument_attribute_field_flagged(self) -> None:
        violations = _scan_fixture(
            """
            #[tracing::instrument(fields(channel = %id), skip_all)]
            fn probe() {}
            """
        )
        self.assertEqual(_keys(violations), ["channel"])

    def test_trailing_comma_less_last_field_flagged(self) -> None:
        """codex r2: depth-0 `)` terminates the LAST field of a call."""
        violations = _scan_fixture(
            """
            fn probe() {
                tracing::info!(channel = channel_id);
            }
            """
        )
        self.assertEqual(_keys(violations), ["channel"])

    def test_instrument_fields_without_trailing_comma_flagged(self) -> None:
        """codex r2: `fields(channel = id)` closes with `)` — still a field."""
        violations = _scan_fixture(
            """
            #[tracing::instrument(fields(channel = id))]
            fn probe() {}
            """
        )
        self.assertEqual(_keys(violations), ["channel"])

    def test_last_field_with_method_call_value_flagged(self) -> None:
        # Matched pairs inside the value must not eat the closing terminator.
        violations = _scan_fixture(
            """
            fn probe() {
                tracing::info!(channel = channel_id.get());
            }
            """
        )
        self.assertEqual(_keys(violations), ["channel"])

    def test_sigil_last_field_without_comma_extracts_clean_value(self) -> None:
        # Allowlist matching depends on the value excluding the `)` closer.
        violations = _scan_fixture(
            """
            fn probe() {
                tracing::info!(session_id = %event.session_id);
            }
            """,
            rel="src/services/discord/tui_prompt_relay.rs",
        )
        self.assertEqual(violations, [])


class StringAndCommentExclusionTest(unittest.TestCase):
    """codex r1 gap 2: matches inside string literals/comments must not flag."""

    def test_plain_string_literal_not_flagged(self) -> None:
        violations = _scan_fixture(
            """
            fn probe() {
                let msg = "channel = foo, bar";
                let sql = "WHERE discord_channel_id = $1";
            }
            """
        )
        self.assertEqual(violations, [])

    def test_raw_string_literal_not_flagged(self) -> None:
        violations = _scan_fixture(
            """
            fn probe() {
                let q = r#"channel = x, session_id = y,"#;
                let r = r"chan = z,";
            }
            """
        )
        self.assertEqual(violations, [])

    def test_multiline_string_continuation_not_flagged(self) -> None:
        violations = _scan_fixture(
            """
            fn probe() {
                tracing::warn!(
                    "prefix channel = old, \\
                     session_id = stale, tail",
                    count = 1,
                );
            }
            """
        )
        self.assertEqual(violations, [])

    def test_comments_not_flagged(self) -> None:
        violations = _scan_fixture(
            """
            fn probe() {
                // channel = x,
                /* chan = y,
                   session_id = z, */
                let a = 1; // trailing: discord_channel_id = w,
            }
            """
        )
        self.assertEqual(violations, [])

    def test_quote_char_literal_does_not_desync_stripping(self) -> None:
        # If '"' desynced quote tracking, the string on the next line would
        # be treated as code and false-positive.
        violations = _scan_fixture(
            """
            fn probe() {
                let c = '"';
                let s = "channel = nope,";
            }
            """
        )
        self.assertEqual(violations, [])

    def test_code_after_string_on_same_line_still_scanned(self) -> None:
        violations = _scan_fixture(
            """
            fn probe() {
                tracing::info!("channel = prose,", channel = real_id, "m");
            }
            """
        )
        self.assertEqual(_keys(violations), ["channel"])


class StatementAndBindingExclusionTest(unittest.TestCase):
    def test_let_bindings_not_flagged(self) -> None:
        violations = _scan_fixture(
            """
            fn probe() {
                let channel = ChannelId::new(1);
                let chan = serenity::ChannelId::new(channel_id);
                let discord_channel_id = ChannelId::new(channel_id);
            }
            """
        )
        self.assertEqual(violations, [])

    def test_reassignment_statements_not_flagged(self) -> None:
        violations = _scan_fixture(
            """
            fn probe() {
                session_id = None;
                session_id = Some(pair[1].clone());
                session_id = Some(restored_session_id);
            }
            """
        )
        self.assertEqual(violations, [])

    def test_field_access_assignment_not_flagged(self) -> None:
        violations = _scan_fixture(
            """
            fn probe() {
                state.channel = 5;
                self.session_id = value;
            }
            """
        )
        self.assertEqual(violations, [])

    def test_tail_expression_with_matched_parens_not_flagged(self) -> None:
        # codex r2 guard: a matched `(…)` pair inside the value drops depth
        # back to 0 without terminating — `)` only terminates when UNmatched.
        violations = _scan_fixture(
            """
            fn probe() {
                session_id = compute(x)
            }
            """
        )
        self.assertEqual(violations, [])

    def test_closure_body_assignment_not_flagged(self) -> None:
        # codex r2 guard: `|| key = value)` — the preceder is `|`, not `(`/`,`.
        violations = _scan_fixture(
            """
            fn probe() {
                spawn(move || session_id = None);
                run(|| channel = next());
            }
            """
        )
        self.assertEqual(violations, [])


class SessionIdAllowlistTest(unittest.TestCase):
    def test_allowlisted_voice_gateway_site_passes(self) -> None:
        violations = _scan_fixture(
            """
            fn probe() {
                tracing::info!(
                    session_id = data.session_id,
                    "voice driver connected"
                );
            }
            """,
            rel="src/services/discord/voice_lifecycle.rs",
        )
        self.assertEqual(violations, [])

    def test_same_value_in_other_file_still_fails(self) -> None:
        violations = _scan_fixture(
            """
            fn probe() {
                tracing::info!(
                    session_id = data.session_id,
                    "not the voice file"
                );
            }
            """,
            rel="src/services/discord/other.rs",
        )
        self.assertEqual(_keys(violations), ["session_id"])

    def test_allowlisted_file_with_other_value_still_fails(self) -> None:
        violations = _scan_fixture(
            """
            fn probe() {
                tracing::info!(
                    session_id = something_else,
                    "voice file but different value"
                );
            }
            """,
            rel="src/services/discord/voice_lifecycle.rs",
        )
        self.assertEqual(_keys(violations), ["session_id"])


class CleanTreeTest(unittest.TestCase):
    def test_clean_fixture_returns_no_violations(self) -> None:
        violations = _scan_fixture(
            """
            fn probe() {
                tracing::info!(channel_id = 1, session_key = %key, "ok");
            }
            """
        )
        self.assertEqual(violations, [])

    def test_live_tree_has_no_violations(self) -> None:
        """The real Discord tree must stay clean under the current rules."""
        self.assertEqual(CHECKER.scan(REPO_ROOT), [])


if __name__ == "__main__":
    unittest.main()
