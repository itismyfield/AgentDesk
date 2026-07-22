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

    def test_cluster_intake_change_requires_multinode_doc_touch(self) -> None:
        findings = CHECKER.check_doc_touch_rules(
            {"src/services/cluster/intake_worker_capabilities.rs"}
        )
        self.assertEqual(len(findings), 1)
        self.assertEqual(
            findings[0].path,
            "docs/agent-maintenance/multinode-transition.md",
        )

    def test_migration_0093_change_requires_multinode_doc_touch(self) -> None:
        findings = CHECKER.check_doc_touch_rules({CHECKER.MIGRATION_0093_PATH})
        self.assertEqual(len(findings), 1)
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


class Migration0093RolloutContractTest(unittest.TestCase):
    _DOC_PATH = "docs/agent-maintenance/multinode-transition.md"

    @staticmethod
    def _contract_text() -> str:
        return "\n".join(CHECKER.MIGRATION_0093_ROLLOUT_MARKERS)

    def test_ignores_missing_contract_when_migration_is_unchanged(self) -> None:
        with TemporaryDirectory() as tmp:
            findings = CHECKER.check_migration_0093_rollout_contract(Path(tmp), set())

        self.assertEqual(findings, [])

    def test_accepts_complete_contract_when_migration_changes(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            _write(root, self._DOC_PATH, self._contract_text())
            findings = CHECKER.check_migration_0093_rollout_contract(
                root, {CHECKER.MIGRATION_0093_PATH}
            )

        self.assertEqual(findings, [])

    def test_rejects_incomplete_contract_when_migration_changes(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            _write(root, self._DOC_PATH, CHECKER.MIGRATION_0093_ROLLOUT_MARKERS[0])
            findings = CHECKER.check_migration_0093_rollout_contract(
                root, {CHECKER.MIGRATION_0093_PATH}
            )

        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].severity, "error")
        self.assertIn("missing required marker", findings[0].message)

    def test_warning_only_hard_fails_targeted_migration_gate(self) -> None:
        rollout_error = CHECKER.Finding(
            "error", self._DOC_PATH, "migration 0093 rollout contract is missing"
        )
        with TemporaryDirectory() as tmp, patch.object(
            CHECKER, "check_doc_headers", return_value=[]
        ), patch.object(
            CHECKER, "check_change_surface_line_counts", return_value=[]
        ), patch.object(
            CHECKER, "check_migration_0093_rollout_contract", return_value=[rollout_error]
        ), patch.object(
            CHECKER, "check_doc_touch_rules", return_value=[]
        ):
            result = CHECKER.main(
                [
                    "--repo-root",
                    tmp,
                    "--changed-file",
                    CHECKER.MIGRATION_0093_PATH,
                    "--warning-only",
                    "--migration-0093-rollout-gate",
                ]
            )

        self.assertEqual(result, 1)

    def test_warning_only_does_not_activate_gate_for_unrelated_change(self) -> None:
        with TemporaryDirectory() as tmp, patch.object(
            CHECKER, "check_doc_headers", return_value=[]
        ), patch.object(
            CHECKER, "check_change_surface_line_counts", return_value=[]
        ), patch.object(
            CHECKER, "check_doc_touch_rules", return_value=[]
        ):
            result = CHECKER.main(
                [
                    "--repo-root",
                    tmp,
                    "--changed-file",
                    "src/lib.rs",
                    "--warning-only",
                    "--migration-0093-rollout-gate",
                ]
            )

        self.assertEqual(result, 0)


class ChangeSurfaceLineCountTest(unittest.TestCase):
    _INVENTORY_HEADER = (
        "| Module | Path | Lines | Prod | Test | Flags |\n"
        "| --- | --- | ---: | ---: | ---: | --- |\n"
    )

    def _setup(self, root: Path, inventory_row: str, surface_line: str) -> None:
        _write(
            root,
            "docs/generated/module-inventory.md",
            self._INVENTORY_HEADER + inventory_row + "\n",
        )
        _write(root, "docs/agent-maintenance/change-surfaces.md", surface_line)

    def test_allows_inventory_count_churn_without_doc_change(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            surface_line = "- `src/services/foo.rs` (frozen giant surface).\n"
            self._setup(
                root,
                "| `services::foo` | `src/services/foo.rs` | 2100 | 1500 | 600 |  |",
                surface_line,
            )
            findings_before = CHECKER.check_change_surface_line_counts(root)
            _write(
                root,
                "docs/generated/module-inventory.md",
                self._INVENTORY_HEADER
                + "| `services::foo` | `src/services/foo.rs` | 2150 | 1550 | 600 |  |\n",
            )
            findings_after = CHECKER.check_change_surface_line_counts(root)

        self.assertEqual(findings_before, [])
        self.assertEqual(findings_after, [])

    def test_ignores_unmarked_path_references(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._setup(
                root,
                "| `services::foo` | `src/services/foo.rs` | 99 | 1500 | 57 |  |",
                "- `src/services/foo.rs` is documented elsewhere.\n",
            )
            findings = CHECKER.check_change_surface_line_counts(root)

        self.assertEqual(findings, [])

    def test_errors_on_ghost_freeze_entry_below_threshold(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._setup(
                root,
                "| `services::foo` | `src/services/foo.rs` | 4000 | 64 | 3936 |  |",
                "- `src/services/foo.rs` (frozen giant surface).\n",
            )
            findings = CHECKER.check_change_surface_line_counts(root)

        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].severity, "error")
        self.assertIn("no longer a giant file", findings[0].message)


    def test_warning_only_line_count_gate_hard_fails_ghost_entry(self) -> None:
        with TemporaryDirectory() as tmp, patch.object(
            CHECKER, "check_doc_headers", return_value=[]
        ), patch.object(
            CHECKER, "check_migration_0093_rollout_contract", return_value=[]
        ), patch.object(
            CHECKER, "check_doc_touch_rules", return_value=[]
        ):
            root = Path(tmp)
            self._setup(
                root,
                "| `services::foo` | `src/services/foo.rs` | 4000 | 64 | 3936 |  |",
                "- `src/services/foo.rs` (frozen giant surface).\n",
            )
            result = CHECKER.main(
                ["--repo-root", tmp, "--warning-only", "--line-count-gate"]
            )

        self.assertEqual(result, 1)

    def test_warning_only_line_count_gate_allows_count_free_giant(self) -> None:
        with TemporaryDirectory() as tmp, patch.object(
            CHECKER, "check_doc_headers", return_value=[]
        ), patch.object(
            CHECKER, "check_migration_0093_rollout_contract", return_value=[]
        ), patch.object(
            CHECKER, "check_doc_touch_rules", return_value=[]
        ):
            root = Path(tmp)
            self._setup(
                root,
                "| `services::foo` | `src/services/foo.rs` | 2200 | 2100 | 100 |  |",
                "- `src/services/foo.rs` (frozen giant surface).\n",
            )
            result = CHECKER.main(
                ["--repo-root", tmp, "--warning-only", "--line-count-gate"]
            )

        self.assertEqual(result, 0)

    def test_gates_count_free_frozen_entry(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._setup(
                root,
                "| `services::foo` | `src/services/foo.rs` | 4000 | 64 | 3936 |  |",
                "- `src/services/foo.rs` (frozen giant surface) — provider adapter.\n",
            )
            findings = CHECKER.check_change_surface_line_counts(root)

        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].severity, "error")
        self.assertIn("no longer a giant file", findings[0].message)

    def test_count_free_entry_is_not_double_counted(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._setup(
                root,
                "| `services::foo` | `src/services/foo.rs` | 2000 | 64 | 1936 |  |",
                "- `src/services/foo.rs` (frozen giant surface, owner: services).\n",
            )
            findings = CHECKER.check_change_surface_line_counts(root)

        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].severity, "error")

    def test_errors_when_frozen_path_missing_from_disk(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            # Inventory has an unrelated module; the frozen path does not exist
            # on disk (deleted/renamed) -> hard error, not a warning.
            self._setup(
                root,
                "| `services::bar` | `src/services/bar.rs` | 1500 | 1500 | 0 |  |",
                "- `src/services/gone.rs` (frozen giant surface).\n",
            )
            findings = CHECKER.check_change_surface_line_counts(root)

        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].severity, "error")
        self.assertIn("missing from disk", findings[0].message)

    def test_warns_when_frozen_path_is_test_file(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "src/db").mkdir(parents=True)
            (root / "src/db/tests.rs").write_text("// test harness\n", encoding="utf-8")
            self._setup(
                root,
                "| `services::bar` | `src/services/bar.rs` | 1500 | 1500 | 0 |  |",
                "- `src/db/tests.rs` (frozen giant surface).\n",
            )
            findings = CHECKER.check_change_surface_line_counts(root)

        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].severity, "warning")
        self.assertIn("test file", findings[0].message)


if __name__ == "__main__":
    unittest.main()
