"""Unit tests for scripts/check_agent_maintenance_docs.py."""

from __future__ import annotations

import importlib.util
import sys
import textwrap
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / "scripts" / "check_agent_maintenance_docs.py"

_SPEC = importlib.util.spec_from_file_location("check_agent_maintenance_docs", SCRIPT_PATH)
CHECKER = importlib.util.module_from_spec(_SPEC)
assert _SPEC.loader is not None
sys.modules[_SPEC.name] = CHECKER
_SPEC.loader.exec_module(CHECKER)


def _write(root: Path, rel: str, body: str) -> None:
    target = root / rel
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(textwrap.dedent(body).lstrip("\n"), encoding="utf-8")


class LastRefreshedHeaderTest(unittest.TestCase):
    def test_parses_required_header_shape(self) -> None:
        parsed = CHECKER.parse_last_refreshed(
            """
            # Doc

            > Last refreshed: 2026-04-29 (against `main` @ `1d165cd3844e94015ab30cda8e4b1bba717f934d`).
            """
        )
        self.assertIsNotNone(parsed)
        assert parsed is not None
        refreshed_on, commit, line_no = parsed
        self.assertEqual(refreshed_on.isoformat(), "2026-04-29")
        self.assertEqual(commit, "1d165cd3844e94015ab30cda8e4b1bba717f934d")
        self.assertEqual(line_no, 4)

    def test_rejects_last_reviewed_alias(self) -> None:
        parsed = CHECKER.parse_last_refreshed(
            "> Last reviewed: 2026-04-29 against `origin/main` @ `abc1234`\n"
        )
        self.assertIsNone(parsed)


class DocTouchRulesTest(unittest.TestCase):
    def test_outbound_source_change_requires_migration_doc_touch(self) -> None:
        findings = CHECKER.check_doc_touch_rules(
            {"src/services/discord/outbound/message.rs"}
        )
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].severity, "error")
        self.assertEqual(
            findings[0].path,
            "docs/agent-maintenance/discord-outbound-migration.md",
        )

    def test_outbound_doc_touch_satisfies_rule(self) -> None:
        findings = CHECKER.check_doc_touch_rules(
            {
                "src/services/discord/outbound/message.rs",
                "docs/agent-maintenance/discord-outbound-migration.md",
            }
        )
        self.assertEqual(findings, [])

    def test_tmux_source_change_requires_change_surfaces_touch(self) -> None:
        findings = CHECKER.check_doc_touch_rules({"src/services/discord/tmux.rs"})
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].path, "docs/agent-maintenance/change-surfaces.md")


class ChangeSurfaceLineCountTest(unittest.TestCase):
    def test_warns_when_copied_line_count_drifts_from_inventory(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            _write(
                root,
                "docs/generated/module-inventory.md",
                """
                | Module | Path | Lines | Flags |
                | --- | --- | ---: | --- |
                | `services::foo` | `src/services/foo.rs` | 42 |  |
                """,
            )
            _write(
                root,
                "docs/agent-maintenance/change-surfaces.md",
                "- `src/services/foo.rs` (41 lines, giant-file).\n",
            )

            findings = CHECKER.check_change_surface_line_counts(root)

        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].severity, "warning")
        self.assertIn("but 42 in module-inventory.md", findings[0].message)


if __name__ == "__main__":
    unittest.main()
