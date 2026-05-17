"""Unit tests for scripts/check_agent_maintenance_docs.py."""

from __future__ import annotations

import importlib.util
import subprocess
import sys
import textwrap
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

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
        self.assertEqual(parsed.refreshed_on.isoformat(), "2026-04-29")
        self.assertEqual(parsed.commit, "1d165cd3844e94015ab30cda8e4b1bba717f934d")
        self.assertEqual(parsed.line, 4)

    def test_parses_issue_anchored_header_shape(self) -> None:
        parsed = CHECKER.parse_last_refreshed(
            "> Last refreshed: 2026-04-30 (against #1438 PG-only cleanup; default tree).\n"
        )
        self.assertIsNotNone(parsed)
        assert parsed is not None
        self.assertEqual(parsed.refreshed_on.isoformat(), "2026-04-30")
        self.assertIsNone(parsed.commit)
        self.assertEqual(parsed.anchor, "#1438 PG-only cleanup; default tree")

    def test_parses_pr_prefixed_issue_anchor(self) -> None:
        parsed = CHECKER.parse_last_refreshed(
            "> Last refreshed: 2026-05-17 (against PR #2064 freshness regex relax).\n"
        )
        self.assertIsNotNone(parsed)
        assert parsed is not None
        self.assertEqual(parsed.refreshed_on.isoformat(), "2026-05-17")
        self.assertIsNone(parsed.commit)
        self.assertEqual(parsed.anchor, "#2064 freshness regex relax")

    def test_rejects_pr_anchor_without_hash(self) -> None:
        parsed = CHECKER.parse_last_refreshed(
            "> Last refreshed: 2026-05-17 (against PR 2064 missing hash).\n"
        )
        self.assertIsNone(parsed)

    def test_parses_manual_header_shape(self) -> None:
        parsed = CHECKER.parse_last_refreshed(
            "> Last refreshed: 2026-04-30 (manual: verified generated inventory only).\n"
        )
        self.assertIsNotNone(parsed)
        assert parsed is not None
        self.assertIsNone(parsed.commit)
        self.assertEqual(parsed.anchor, "verified generated inventory only")

    def test_scans_documented_top_matter_window(self) -> None:
        text = "\n".join(
            ["# Doc", *[f"> note {idx}" for idx in range(1, 30)]]
            + [
                "> Last refreshed: 2026-04-30 "
                "(against `main` @ `1d165cd3844e94015ab30cda8e4b1bba717f934d`)."
            ]
        )
        parsed = CHECKER.parse_last_refreshed(text)
        self.assertIsNotNone(parsed)
        assert parsed is not None
        self.assertEqual(parsed.line, 31)

    def test_rejects_last_reviewed_alias(self) -> None:
        parsed = CHECKER.parse_last_refreshed(
            "> Last reviewed: 2026-04-29 against `origin/main` @ `abc1234`\n"
        )
        self.assertIsNone(parsed)

    def test_rejects_unmarked_freeform_against_header(self) -> None:
        parsed = CHECKER.parse_last_refreshed(
            "> Last refreshed: 2026-04-29 (against main @ abc1234)\n"
        )
        self.assertIsNone(parsed)


class HeaderValidationTest(unittest.TestCase):
    def test_unresolvable_commit_is_warning_in_shallow_checkout(self) -> None:
        commit = "1d165cd3844e94015ab30cda8e4b1bba717f934d"

        def fake_run_git(_repo_root: Path, args: list[str]) -> subprocess.CompletedProcess[str]:
            if args == ["rev-parse", "--is-shallow-repository"]:
                return subprocess.CompletedProcess(args, 0, "true\n", "")
            if args[:2] == ["rev-parse", "--verify"]:
                return subprocess.CompletedProcess(args, 1, "", "missing object")
            raise AssertionError(f"unexpected git args: {args}")

        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            for rel_path in CHECKER.MIGRATION_SENSITIVE_DOCS:
                _write(
                    root,
                    rel_path,
                    f"""
                    # Doc

                    > Last refreshed: 2026-04-30 (against `main` @ `{commit}`).
                    """,
                )

            with patch.object(CHECKER, "run_git", side_effect=fake_run_git):
                findings = CHECKER.check_doc_headers(
                    root, CHECKER.dt.date(2026, 4, 30), CHECKER.DEFAULT_FRESHNESS_DAYS
                )

        self.assertEqual(len(findings), len(CHECKER.MIGRATION_SENSITIVE_DOCS))
        self.assertTrue(all(finding.severity == "warning" for finding in findings))
        self.assertTrue(
            all("shallow checkout" in finding.message for finding in findings)
        )


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

    def test_multinode_source_change_requires_multinode_doc_touch(self) -> None:
        findings = CHECKER.check_doc_touch_rules({"src/server/worker_registry.rs"})
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].severity, "error")
        self.assertEqual(
            findings[0].path,
            "docs/agent-maintenance/multinode-transition.md",
        )

    def test_multinode_doc_touch_satisfies_rule(self) -> None:
        findings = CHECKER.check_doc_touch_rules(
            {
                "src/server/routes/dispatches/outbox.rs",
                "docs/agent-maintenance/multinode-transition.md",
            }
        )
        self.assertEqual(findings, [])


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
