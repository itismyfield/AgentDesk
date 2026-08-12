"""Tests for scripts/check_discord_message_unit_usage.py."""

from __future__ import annotations

import importlib.util
import sys
import textwrap
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts/check_discord_message_unit_usage.py"
SPEC = importlib.util.spec_from_file_location("check_discord_message_unit_usage", SCRIPT)
CHECKER = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules[SPEC.name] = CHECKER
SPEC.loader.exec_module(CHECKER)


def scan_fixture(source: str):
    with TemporaryDirectory() as tmp:
        root = Path(tmp)
        path = root / "src/services/discord/probe.rs"
        path.parent.mkdir(parents=True)
        path.write_text(textwrap.dedent(source), encoding="utf-8")
        return CHECKER.scan(root)


class DiscordMessageUnitUsageTest(unittest.TestCase):
    def test_direct_byte_and_scalar_comparisons_die(self) -> None:
        violations = scan_fixture(
            """
            fn byte(text: &str) -> bool { text.len() > DISCORD_MSG_LIMIT }
            fn scalar(text: &str) -> bool { text.chars().count() > DISCORD_MSG_LIMIT }
            """
        )
        self.assertEqual([line for (_path, line, _source) in violations], [2, 3])

    def test_limit_alias_comparison_dies(self) -> None:
        violations = scan_fixture(
            """
            fn probe(text: &str) -> bool {
                let limit = DISCORD_MSG_LIMIT.saturating_sub(6);
                text.len() > limit
            }
            """
        )
        self.assertEqual(len(violations), 1)
        self.assertIn("text.len() > limit", violations[0][2])

    def test_utf16_helpers_and_independent_byte_budget_pass(self) -> None:
        self.assertEqual(
            scan_fixture(
                """
                fn probe(text: &str) -> bool {
                    discord_message_units(text) > DISCORD_MSG_LIMIT
                }
                fn budget(text: &str) -> bool { text.len() > PANEL_BUDGET_BYTES }
                #[cfg(test)]
                mod tests { fn fixture(text: &str) -> bool { text.len() > DISCORD_MSG_LIMIT } }
                """
            ),
            [],
        )
