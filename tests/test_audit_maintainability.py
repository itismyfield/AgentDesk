"""Unit tests for scripts/audit_maintainability.py and its check modules.

Each of the 9 checks gets a focused fixture: a temporary ``src/`` tree is
created with files designed to trigger (or specifically not trigger) the
rule, and we assert the harness emits the expected findings.
"""

from __future__ import annotations

import importlib
import importlib.util
import json
import sys
import textwrap
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Iterable
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / "scripts" / "audit_maintainability.py"

# Make scripts/ importable so ``audit_maintainability`` (the package) and
# ``audit_maintainability_harness`` (the script) both load cleanly.
SCRIPTS_DIR = REPO_ROOT / "scripts"
if str(SCRIPTS_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPTS_DIR))

# Load the harness as a module under a non-conflicting name.
_SPEC = importlib.util.spec_from_file_location("audit_maintainability_harness", SCRIPT_PATH)
HARNESS = importlib.util.module_from_spec(_SPEC)
assert _SPEC.loader is not None
sys.modules[_SPEC.name] = HARNESS
_SPEC.loader.exec_module(HARNESS)

import audit_maintainability.common as common  # noqa: E402
from audit_maintainability.checks import (  # noqa: E402
    direct_discord_sends,
    git_subprocess,
    legacy_sqlite,
    limit_clamp_duplication,
    manual_json_mapping,
    namespace_size_caps,
    route_srp,
    source_of_truth_alias,
)
from audit_maintainability.checks import giant_files  # noqa: E402


def _write(root: Path, rel: str, body: str) -> None:
    target = root / rel
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(textwrap.dedent(body).lstrip("\n"), encoding="utf-8")


class _FakeSrcTree:
    """Context manager that creates a tmp src/ tree and patches
    ``audit_maintainability.common`` to point at it."""

    def __init__(self, files: dict[str, str]) -> None:
        self.files = files
        self._tmp: TemporaryDirectory | None = None
        self._patches: list = []

    def __enter__(self) -> Path:
        self._tmp = TemporaryDirectory()
        root = Path(self._tmp.name)
        for rel, body in self.files.items():
            _write(root, rel, body)
        src_root = root / "src"
        scripts_root = root / "scripts"
        # Patch the module-level constants used by the checks.
        p_repo = mock.patch.object(common, "REPO_ROOT", root)
        p_src = mock.patch.object(common, "SRC_ROOT", src_root)
        p_scripts = mock.patch.object(common, "SCRIPTS_ROOT", scripts_root)
        p_repo.start()
        p_src.start()
        p_scripts.start()
        self._patches.extend([p_repo, p_src, p_scripts])
        return root

    def __exit__(self, exc_type, exc, tb) -> None:
        for p in self._patches:
            p.stop()
        if self._tmp is not None:
            self._tmp.cleanup()


def _files(findings: Iterable) -> set[str]:
    return {f.file for f in findings}


class GiantFilesCheck(unittest.TestCase):
    def test_flags_files_over_threshold(self) -> None:
        big = "fn x() {}\n" * (giant_files.THRESHOLD + 5)
        small = "fn y() {}\n"
        with _FakeSrcTree(
            {
                "src/big_module.rs": big,
                "src/small_module.rs": small,
                "src/other_tests.rs": big,  # tests must be excluded
            }
        ):
            hits = list(giant_files.CHECK.runner(set()))
        self.assertEqual(_files(hits), {"src/big_module.rs"})

    def test_allowlist_suppresses(self) -> None:
        big = "fn x() {}\n" * (giant_files.THRESHOLD + 5)
        with _FakeSrcTree({"src/legacy.rs": big}):
            hits = list(giant_files.CHECK.runner({"src/legacy.rs"}))
        self.assertEqual(hits, [])

    def test_change_surfaces_doc_suppresses_existing_giant_file(self) -> None:
        big = "fn x() {}\n" * (giant_files.THRESHOLD + 5)
        with _FakeSrcTree({"src/legacy.rs": big}) as root:
            _write(
                root,
                "docs/agent-maintenance/change-surfaces.md",
                "- `src/legacy.rs` (1005 lines).\n",
            )
            hits = list(giant_files.CHECK.runner(set()))
        self.assertEqual(hits, [])

    def test_change_surfaces_doc_expands_brace_groups(self) -> None:
        big = "fn x() {}\n" * (giant_files.THRESHOLD + 5)
        with _FakeSrcTree(
            {
                "src/dispatch/mod.rs": big,
                "src/dispatch/dispatch_status.rs": big,
            }
        ) as root:
            _write(
                root,
                "docs/agent-maintenance/change-surfaces.md",
                "- `src/dispatch/{mod,dispatch_status}.rs` (giant files).\n",
            )
            hits = list(giant_files.CHECK.runner(set()))
        self.assertEqual(hits, [])

    def test_namespace_caps_own_matching_giant_files(self) -> None:
        big = "fn x() {}\n" * (giant_files.THRESHOLD + 5)
        with _FakeSrcTree(
            {
                "src/services/discord/commands/inspect/render_last.rs": big,
                "src/other_giant.rs": big,
            }
        ) as root:
            _write(
                root,
                "scripts/audit_maintainability_config.toml",
                """
                [namespace_size_caps]
                "src/services/discord/commands/inspect/**" = 700
                """,
            )
            hits = list(giant_files.CHECK.runner(set()))
        self.assertEqual(_files(hits), {"src/other_giant.rs"})


class NamespaceSizeCapsCheck(unittest.TestCase):
    def test_flags_files_over_configured_namespace_cap(self) -> None:
        over = "fn x() {}\n" * 4
        at_cap = "fn y() {}\n" * 3
        with _FakeSrcTree(
            {
                "src/services/discord/commands/inspect/mod.rs": over,
                "src/services/discord/commands/inspect/query.rs": at_cap,
                "src/services/discord/commands/inspect/tests.rs": over,
                "src/other.rs": over,
            }
        ) as root:
            _write(
                root,
                "scripts/audit_maintainability_config.toml",
                """
                [namespace_size_caps]
                "src/services/discord/commands/inspect/**" = 3
                """,
            )
            hits = list(namespace_size_caps.CHECK.runner(set()))
        self.assertEqual(_files(hits), {"src/services/discord/commands/inspect/mod.rs"})
        self.assertEqual(hits[0].extra["max_lines"], "3")

    def test_allowlist_suppresses_namespace_cap(self) -> None:
        over = "fn x() {}\n" * 4
        rel = "src/services/discord/prompt_builder/mod.rs"
        with _FakeSrcTree({rel: over}) as root:
            _write(
                root,
                "scripts/audit_maintainability_config.toml",
                """
                [namespace_size_caps]
                "src/services/discord/prompt_builder/**" = 3
                """,
            )
            hits = list(namespace_size_caps.CHECK.runner({rel}))
        self.assertEqual(hits, [])

    def test_loads_all_configured_caps(self) -> None:
        with _FakeSrcTree({"src/main.rs": "fn main() {}\n"}) as root:
            config = root / "scripts" / "audit_maintainability_config.toml"
            _write(
                root,
                "scripts/audit_maintainability_config.toml",
                """
                [namespace_size_caps]
                "src/a/**" = 10
                "src/b/**" = 20
                """,
            )
            caps = namespace_size_caps.load_namespace_size_caps(config)
        self.assertEqual(
            caps,
            (
                namespace_size_caps.NamespaceSizeCap("src/a/**", 10),
                namespace_size_caps.NamespaceSizeCap("src/b/**", 20),
            ),
        )


class RouteSrpCheck(unittest.TestCase):
    def test_flags_route_with_sql_json_and_domain(self) -> None:
        body = """
        use crate::services::auto_queue;
        async fn handler() {
            let _ = sqlx::query("SELECT * FROM agents").fetch_all(&db).await;
            return json!({"ok": true});
        }
        """
        clean_body = """
        async fn handler() {
            return json!({"ok": true});
        }
        """
        with _FakeSrcTree(
            {
                "src/server/routes/dirty.rs": body,
                "src/server/routes/clean.rs": clean_body,
                "src/server/mod.rs": body,  # not in routes/ -> ignored
            }
        ):
            hits = list(route_srp.CHECK.runner(set()))
        self.assertEqual(_files(hits), {"src/server/routes/dirty.rs"})

    def test_baseline_gate_allows_committed_file_counts(self) -> None:
        body = """
        use crate::services::auto_queue;
        async fn handler() {
            let _ = sqlx::query("SELECT * FROM agents").fetch_all(&db).await;
            return json!({"ok": true});
        }
        """
        with _FakeSrcTree({"src/server/routes/legacy.rs": body}) as root:
            _write(
                root,
                route_srp.BASELINE_REL_PATH,
                json.dumps(
                    {
                        "schema_version": 1,
                        "rule": "route_srp_violations",
                        "total_count": 1,
                        "files": {"src/server/routes/legacy.rs": {"count": 1}},
                    }
                ),
            )
            hits = list(route_srp.CHECK.runner(set()))
            baseline_gate = route_srp.CHECK.baseline_gate
            self.assertIsNotNone(baseline_gate)
            failures = list(baseline_gate(hits)) if baseline_gate else []
        self.assertEqual(failures, [])

    def test_baseline_gate_flags_cross_file_regression_when_total_matches(self) -> None:
        body = """
        use crate::services::auto_queue;
        async fn handler() {
            let _ = sqlx::query("SELECT * FROM agents").fetch_all(&db).await;
            return json!({"ok": true});
        }
        """
        with _FakeSrcTree({"src/server/routes/new.rs": body}) as root:
            _write(
                root,
                route_srp.BASELINE_REL_PATH,
                json.dumps(
                    {
                        "schema_version": 1,
                        "rule": "route_srp_violations",
                        "total_count": 1,
                        "files": {"src/server/routes/old.rs": {"count": 1}},
                    }
                ),
            )
            hits = list(route_srp.CHECK.runner(set()))
            baseline_gate = route_srp.CHECK.baseline_gate
            self.assertIsNotNone(baseline_gate)
            failures = list(baseline_gate(hits)) if baseline_gate else []
        self.assertEqual(_files(failures), {"src/server/routes/new.rs"})


class DirectDiscordSendsCheck(unittest.TestCase):
    def test_flags_direct_send_outside_outbound(self) -> None:
        outside = """
        async fn notify(http: Arc<Http>, ch: ChannelId) {
            ch.send_message(&http, |m| m.content("x")).await.ok();
        }
        """
        outbound = """
        async fn dispatcher(http: Arc<Http>, ch: ChannelId) {
            ch.send_message(&http, |m| m.content("ok")).await.ok();
        }
        """
        with _FakeSrcTree(
            {
                "src/services/agents.rs": outside,
                "src/services/discord/outbound/mod.rs": outbound,
            }
        ):
            hits = list(direct_discord_sends.CHECK.runner(set()))
        self.assertEqual(_files(hits), {"src/services/agents.rs"})

    def test_stable_allowlist_key_survives_line_drift(self) -> None:
        body = """
        async fn notify(http: Arc<Http>, ch: ChannelId) {
            ch.send_message(&http, |m| m.content("x")).await.ok();
        }
        """
        shifted = "\n\n" + body
        with _FakeSrcTree({"src/services/agents.rs": body}):
            hits = list(direct_discord_sends.CHECK.runner(set()))
        self.assertEqual(len(hits), 1)
        key = hits[0].extra["allowlist_key"]

        with _FakeSrcTree({"src/services/agents.rs": shifted}):
            shifted_hits = list(direct_discord_sends.CHECK.runner({key}))
        self.assertEqual(shifted_hits, [])


class ManualJsonMappingCheck(unittest.TestCase):
    def test_flags_serde_json_value_try_get(self) -> None:
        body = """
        let raw = row.try_get::<serde_json::Value, _>("data")?;
        let typed = row.try_get::<sqlx::types::Json<MyType>, _>("payload")?;
        let ok = row.try_get::<i64, _>("id")?;
        """
        with _FakeSrcTree({"src/db/queries.rs": body}):
            hits = list(manual_json_mapping.CHECK.runner(set()))
        self.assertEqual(_files(hits), {"src/db/queries.rs"})
        self.assertEqual(len(hits), 2)


class LimitClampDuplicationCheck(unittest.TestCase):
    def test_flags_when_3_or_more_files_share_signature(self) -> None:
        same = "let limit = limit.clamp(1, 100);\n"
        unique = "let limit = limit.clamp(1, 999);\n"
        with _FakeSrcTree(
            {
                "src/server/routes/a.rs": same,
                "src/server/routes/b.rs": same,
                "src/server/routes/c.rs": same,
                "src/server/routes/d.rs": unique,
            }
        ):
            hits = list(limit_clamp_duplication.CHECK.runner(set()))
        self.assertEqual(
            _files(hits),
            {
                "src/server/routes/a.rs",
                "src/server/routes/b.rs",
                "src/server/routes/c.rs",
            },
        )

    def test_does_not_flag_when_only_two_files(self) -> None:
        same = "let limit = limit.clamp(1, 50);\n"
        with _FakeSrcTree(
            {
                "src/server/routes/a.rs": same,
                "src/server/routes/b.rs": same,
            }
        ):
            hits = list(limit_clamp_duplication.CHECK.runner(set()))
        self.assertEqual(hits, [])

    def test_flags_inline_api_limit_shape_even_in_a_single_file(self) -> None:
        body = "let limit = limit.clamp(1, 100);\n"
        with _FakeSrcTree({"src/server/routes/only.rs": body}):
            hits = list(limit_clamp_duplication.CHECK.runner(set()))
        self.assertEqual(_files(hits), {"src/server/routes/only.rs"})

    def test_helper_definition_marker_opts_out(self) -> None:
        helper = (
            "// audit-allow: limit_clamp_helper_definition\n"
            "pub fn clamp_api_limit(limit: Option<usize>) -> usize {\n"
            "    limit.unwrap_or(50).clamp(1, 100)\n"
            "}\n"
        )
        with _FakeSrcTree({"src/utils/api.rs": helper}):
            hits = list(limit_clamp_duplication.CHECK.runner(set()))
        self.assertEqual(hits, [])


class GitSubprocessCheck(unittest.TestCase):
    def test_flags_git_command_outside_services_git(self) -> None:
        body = """
        let out = Command::new("git").args(["status"]).output()?;
        """
        clean = """
        let _ = Command::new("ls").output();
        """
        with _FakeSrcTree(
            {
                "src/services/maintenance/foo.rs": body,
                "src/services/git/runner.rs": body,  # exempt by path
                "src/services/other.rs": clean,
            }
        ):
            hits = list(git_subprocess.CHECK.runner(set()))
        self.assertEqual(_files(hits), {"src/services/maintenance/foo.rs"})


class LegacySqliteCheck(unittest.TestCase):
    def test_flags_rusqlite_outside_migration_paths(self) -> None:
        body = """
        use rusqlite::Connection;
        let pool: SqlitePool = ...;
        let path = "agentdesk.sqlite";
        """
        with _FakeSrcTree(
            {
                "src/services/foo.rs": body,
                "src/cli/migrate/sqlite_import.rs": body,  # exempt
                "src/compat/legacy.rs": body,  # exempt
            }
        ):
            hits = list(legacy_sqlite.CHECK.runner(set()))
        self.assertEqual(_files(hits), {"src/services/foo.rs"})

    def test_flags_runtime_sqlite_in_operational_shell_scripts(self) -> None:
        stale = """
        DB="$HOME/.adk/release/data/agentdesk.sqlite"
        sqlite3 "$DB" "SELECT id FROM auto_queue_runs"
        """
        clean = """
        curl -sf "$API/api/queue/status" | jq .
        """
        with _FakeSrcTree(
            {
                "src/main.rs": "fn main() {}\n",
                "scripts/auto-queue-monitor.sh": stale,
                "scripts/queue-stability-batch.sh": clean,
            }
        ):
            hits = list(legacy_sqlite.CHECK.runner(set()))
        self.assertEqual(_files(hits), {"scripts/auto-queue-monitor.sh"})

    def test_auto_queue_monitor_notification_source_is_allowed(self) -> None:
        monitor = (REPO_ROOT / "scripts" / "auto-queue-monitor.sh").read_text(
            encoding="utf-8"
        )
        discord_health = (
            REPO_ROOT / "src" / "services" / "discord" / "health.rs"
        ).read_text(encoding="utf-8")

        self.assertIn('source:"auto-queue"', monitor)
        self.assertNotIn('source:"auto-queue-monitor"', monitor)
        self.assertIn('"auto-queue"', discord_health)


class SourceOfTruthAliasCheck(unittest.TestCase):
    def test_flags_writes_to_alias_paths(self) -> None:
        body = """
        let path = repo_root.join("ARCHITECTURE.md");
        std::fs::write(&path, contents)?;
        """
        clean = """
        let path = repo_root.join("ARCHITECTURE.md");
        let _ = std::fs::read_to_string(&path);
        """
        with _FakeSrcTree(
            {
                "src/services/agents.rs": body,
                "src/services/agents_clean.rs": clean,
                "src/runtime_layout/sync.rs": body,  # exempt
            }
        ):
            hits = list(source_of_truth_alias.CHECK.runner(set()))
        self.assertEqual(_files(hits), {"src/services/agents.rs"})


class HarnessCli(unittest.TestCase):
    def test_runs_all_nine_checks_and_emits_yaml_keys(self) -> None:
        with _FakeSrcTree({"src/main.rs": "fn main() {}\n"}):
            specs = HARNESS.load_check_specs()
            findings = HARNESS.run_all(specs, {})
            yaml_text = HARNESS.render_yaml(specs, findings)
            json_text = HARNESS.render_json(specs, findings)
            md_text = HARNESS.render_markdown(specs, findings)
        for key in (
            "giant_files",
            "namespace_size_caps",
            "route_srp_violations",
            "direct_discord_sends",
            "manual_json_row_mapping",
            "limit_clamp_duplication",
            "git_subprocess_callsites",
            "legacy_sqlite_refs",
            "source_of_truth_alias_writes",
        ):
            self.assertIn(f"  {key}:", yaml_text, f"missing yaml section for {key}")
            self.assertIn(f'"{key}"', json_text, f"missing json section for {key}")
            self.assertIn(f"`{key}`", md_text, f"missing markdown section for {key}")

    def test_markdown_summary_includes_every_registered_check(self) -> None:
        with _FakeSrcTree({"src/main.rs": "fn main() {}\n"}):
            specs = HARNESS.load_check_specs()
            findings = HARNESS.run_all(specs, {})
            md_text = HARNESS.render_markdown(specs, findings)

        summary_keys: list[str] = []
        in_summary = False
        for line in md_text.splitlines():
            if line == "## Summary":
                in_summary = True
                continue
            if in_summary and line.startswith("## "):
                break
            if in_summary and line.startswith("| `"):
                summary_keys.append(line.split("`", 2)[1])

        self.assertEqual(summary_keys, [spec.key for spec in specs])

    def test_only_selected_checks_are_hard_gated(self) -> None:
        specs = HARNESS.load_check_specs()
        hard_gated = {spec.key for spec in specs if spec.hard_gate}
        baseline_gated = {spec.key for spec in specs if spec.baseline_gate}
        self.assertEqual(
            hard_gated,
            {
                "giant_files",
                "namespace_size_caps",
                "direct_discord_sends",
                "legacy_sqlite_refs",
                "source_of_truth_alias_writes",
                "git_subprocess_callsites",
            },
        )
        self.assertEqual(baseline_gated, {"route_srp_violations"})
        warning_only = {
            "route_srp_violations",
            "manual_json_row_mapping",
            "limit_clamp_duplication",
        }
        self.assertTrue(all(not spec.hard_gate for spec in specs if spec.key in warning_only))

    def test_renderers_report_hard_gate_enabled(self) -> None:
        with _FakeSrcTree({"src/main.rs": "fn main() {}\n"}):
            specs = HARNESS.load_check_specs()
            findings = HARNESS.run_all(specs, {})
            yaml_text = HARNESS.render_yaml(specs, findings)
            json_payload = json.loads(HARNESS.render_json(specs, findings))
        self.assertIn("hard_gate_enabled: true", yaml_text)
        self.assertIn("hard_gate_count: 6", yaml_text)
        self.assertIn("baseline_gate_count: 1", yaml_text)
        self.assertIs(json_payload["hard_gate_enabled"], True)
        self.assertEqual(json_payload["hard_gate_count"], 6)
        self.assertEqual(json_payload["baseline_gate_count"], 1)

    def test_check_mode_returns_zero_with_no_findings(self) -> None:
        with _FakeSrcTree({"src/main.rs": "fn main() {}\n"}):
            with mock.patch.object(sys, "stdout", new=mock.MagicMock()):
                rc = HARNESS.main(["--check", "--format", "json"])
        self.assertEqual(rc, 0)

    def test_check_mode_fails_on_direct_discord_send_regression(self) -> None:
        body = """
        async fn notify(http: Arc<Http>, ch: ChannelId) {
            ch.send_message(&http, |m| m.content("x")).await.ok();
        }
        """
        with _FakeSrcTree({"src/services/agents.rs": body}) as root:
            allowlist = root / "empty.toml"
            allowlist.write_text("", encoding="utf-8")
            with mock.patch.object(sys, "stdout", new=mock.MagicMock()), mock.patch.object(
                sys, "stderr", new=mock.MagicMock()
            ):
                rc = HARNESS.main(["--check", "--format", "json", "--allowlist", str(allowlist)])
        self.assertEqual(rc, 1)

    def test_check_mode_fails_on_route_srp_baseline_regression(self) -> None:
        body = """
        use crate::services::auto_queue;
        async fn handler() {
            let _ = sqlx::query("SELECT * FROM agents").fetch_all(&db).await;
            return json!({"ok": true});
        }
        """
        with _FakeSrcTree({"src/server/routes/new.rs": body}) as root:
            _write(
                root,
                route_srp.BASELINE_REL_PATH,
                json.dumps(
                    {
                        "schema_version": 1,
                        "rule": "route_srp_violations",
                        "total_count": 0,
                        "files": {},
                    }
                ),
            )
            allowlist = root / "empty.toml"
            allowlist.write_text("", encoding="utf-8")
            with mock.patch.object(sys, "stdout", new=mock.MagicMock()), mock.patch.object(
                sys, "stderr", new=mock.MagicMock()
            ):
                rc = HARNESS.main(["--check", "--format", "json", "--allowlist", str(allowlist)])
        self.assertEqual(rc, 1)

    def test_check_mode_fails_on_legacy_sqlite_regression(self) -> None:
        body = "use rusqlite::Connection;\n"
        with _FakeSrcTree({"src/services/foo.rs": body}) as root:
            allowlist = root / "empty.toml"
            allowlist.write_text("", encoding="utf-8")
            with mock.patch.object(sys, "stdout", new=mock.MagicMock()), mock.patch.object(
                sys, "stderr", new=mock.MagicMock()
            ):
                rc = HARNESS.main(["--check", "--format", "json", "--allowlist", str(allowlist)])
        self.assertEqual(rc, 1)

    def test_check_mode_fails_on_source_of_truth_alias_write_regression(self) -> None:
        body = """
        let path = repo_root.join("ARCHITECTURE.md");
        std::fs::write(&path, contents)?;
        """
        with _FakeSrcTree({"src/services/agents.rs": body}) as root:
            allowlist = root / "empty.toml"
            allowlist.write_text("", encoding="utf-8")
            with mock.patch.object(sys, "stdout", new=mock.MagicMock()), mock.patch.object(
                sys, "stderr", new=mock.MagicMock()
            ):
                rc = HARNESS.main(["--check", "--format", "json", "--allowlist", str(allowlist)])
        self.assertEqual(rc, 1)

    def test_check_mode_fails_on_undocumented_giant_file_regression(self) -> None:
        big = "fn x() {}\n" * (giant_files.THRESHOLD + 5)
        with _FakeSrcTree({"src/new_giant.rs": big}) as root:
            allowlist = root / "empty.toml"
            allowlist.write_text("", encoding="utf-8")
            with mock.patch.object(sys, "stdout", new=mock.MagicMock()), mock.patch.object(
                sys, "stderr", new=mock.MagicMock()
            ):
                rc = HARNESS.main(["--check", "--format", "json", "--allowlist", str(allowlist)])
        self.assertEqual(rc, 1)

    def test_allowlist_loader_scopes_per_rule(self) -> None:
        with TemporaryDirectory() as tmp:
            p = Path(tmp) / "audit_allowlist.toml"
            p.write_text(
                textwrap.dedent(
                    """
                    giant_files = [
                      "src/big.rs",
                    ]
                    direct_discord_sends = [
                      "src/services/notify.rs",
                      "src/services/notify.rs:42",
                      "src/services/notify.rs#direct_discord_sends:abc123",
                    ]
                    """
                ),
                encoding="utf-8",
            )
            data = HARNESS.load_allowlist(p)
        self.assertEqual(data.get("giant_files"), {"src/big.rs"})
        self.assertEqual(
            data.get("direct_discord_sends"),
            {
                "src/services/notify.rs",
                "src/services/notify.rs:42",
                "src/services/notify.rs#direct_discord_sends:abc123",
            },
        )
        self.assertNotIn("src/services/notify.rs", data.get("giant_files", set()))


if __name__ == "__main__":
    unittest.main()
