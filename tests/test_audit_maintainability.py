"""Unit tests for scripts/audit_maintainability.py and its check modules.

Each audit check gets a focused fixture: a temporary ``src/`` tree is
created with files designed to trigger (or specifically not trigger) the
rule, and we assert the harness emits the expected findings.
"""

from __future__ import annotations

import importlib
import importlib.util
import io
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
    direct_discord_reactions,
    footer_view_writes,
    git_subprocess,
    legacy_sqlite,
    limit_clamp_duplication,
    manual_json_mapping,
    namespace_size_caps,
    route_srp,
    service_server_backflow,
    source_of_truth_alias,
)
from audit_maintainability.checks import giant_files  # noqa: E402
from audit_maintainability.checks import giant_file_ratchet  # noqa: E402
from audit_maintainability.checks import parent_test_residue  # noqa: E402


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


class GiantFileRatchetCheck(unittest.TestCase):
    _BASELINE = "scripts/audit_maintainability_giant_baseline.toml"
    # The admission audit reads this; without it every run adds a metadata error.
    _HISTORY = {"scripts/ratchet_admission_history.toml": "schema_version = 1\nadmission = []\n"}

    def test_real_pin_overrun_and_threshold_controls_reach_check_exit_code(self) -> None:
        path = "src/pinned.rs"
        cases = ((1000, 1000, False), (1001, 1000, True), (999, 500, False))
        for loc, cap, warns in cases:
            with self.subTest(loc=loc, cap=cap), _FakeSrcTree({
                path: "fn production() {}\n" * loc,
                self._BASELINE: f'[giant_file_ratchet]\n"{path}" = {cap}\n',
                **self._HISTORY,
                "empty-allowlist.toml": "",
            }) as root:
                allowlist = root / "empty-allowlist.toml"
                self.assertFalse((root / ".git").exists())
                self.assertEqual(HARNESS.load_allowlist(allowlist), {})
                self.assertEqual(giant_file_ratchet.load_giant_baseline(root / self._BASELINE), {path: cap})
                # A pin below 1000 does not expand the audit's measured population.
                self.assertEqual(giant_files.giant_production_loc(), {path: loc} if loc >= 1000 else {})
                hits = list(giant_file_ratchet.CHECK.runner(set()))
                if warns:
                    self.assertEqual(len(hits), 1)
                    self.assertEqual((hits[0].rule, hits[0].file, hits[0].severity),
                                     ("giant_file_ratchet", path, "warn"))
                    self.assertEqual((hits[0].extra["loc"], hits[0].extra["baseline"]),
                                     ("1001", "1000"))
                else:
                    self.assertEqual(hits, [])
                self.assertFalse(any("admission metadata invalid" in hit.message for hit in hits))
                with mock.patch.object(HARNESS, "load_check_specs", return_value=[giant_file_ratchet.CHECK]), \
                        mock.patch.object(sys, "stdout", new=mock.MagicMock()), \
                        mock.patch.object(sys, "stderr", new=mock.MagicMock()):
                    rc = HARNESS.main(["--check", "--format", "json", "--allowlist", str(allowlist)])
                self.assertEqual(rc, 0)

    def test_warns_for_growth_and_new_giant(self) -> None:
        over = "fn x() {}\n" * (giant_files.THRESHOLD + 50)  # 1050 prod LoC
        with _FakeSrcTree(
            {
                "src/services/discord/tmux_watcher.rs": over,
                "src/other_giant.rs": over,  # a new giant warns without registration
                **self._HISTORY,
            }
        ) as root:
            _write(
                root,
                self._BASELINE,
                """
                [giant_file_ratchet]
                "src/services/discord/tmux_watcher.rs" = 1000
                """,
            )
            hits = list(giant_file_ratchet.CHECK.runner(set()))
        self.assertEqual(_files(hits), {"src/services/discord/tmux_watcher.rs", "src/other_giant.rs"})

    def test_at_or_under_baseline_passes(self) -> None:
        content = "fn x() {}\n" * (giant_files.THRESHOLD + 5)  # 1005 prod LoC
        with _FakeSrcTree({"src/services/discord/tmux_watcher.rs": content, **self._HISTORY}) as root:
            _write(
                root,
                self._BASELINE,
                """
                [giant_file_ratchet]
                "src/services/discord/tmux_watcher.rs" = 2000
                """,
            )
            hits = list(giant_file_ratchet.CHECK.runner(set()))
        self.assertEqual(hits, [])

    def _growth_hits(self, loc: int) -> list:
        path = "src/services/discord/tmux_watcher.rs"
        with _FakeSrcTree({
            path: "fn x() {}\n" * loc,
            self._BASELINE: f'[giant_file_ratchet]\n"{path}" = 1200\n',
            **self._HISTORY,
        }):
            return list(giant_file_ratchet.CHECK.runner(set()))

    def test_growth_warns_with_delta_without_a_cap(self) -> None:
        for loc in (1201, 1230, 1500):
            with self.subTest(loc=loc):
                hits = self._growth_hits(loc)
                self.assertEqual(len(hits), 1)
                self.assertEqual(hits[0].severity, "warn")
                self.assertIn(f"+{loc - 1200} production lines", hits[0].message)
                self.assertEqual(hits[0].extra["delta"], str(loc - 1200))

    def test_file_dropped_below_threshold_is_not_a_regression(self) -> None:
        small = "fn y() {}\n" * 10  # below giant threshold -> not in giant map
        with _FakeSrcTree({"src/services/discord/tmux_watcher.rs": small, **self._HISTORY}) as root:
            _write(
                root,
                self._BASELINE,
                """
                [giant_file_ratchet]
                "src/services/discord/tmux_watcher.rs" = 1000
                """,
            )
            hits = list(giant_file_ratchet.CHECK.runner(set()))
        self.assertEqual(hits, [])

    def test_inherited_overage_growth_warns_and_check_passes(self) -> None:
        import giant_file_progress as progress
        from ratchet_admission import AdmissionAudit

        path = "src/giant.rs"
        for prior, current in ((1250, 1250), (1250, 1249), (1250, 1251), (999, 1000)):
            with self.subTest(prior=prior, current=current), _FakeSrcTree({
                    path: "fn x() {}\n" * current,
                    self._BASELINE: f'[giant_file_ratchet]\n"{path}" = 1200\n',
                    **self._HISTORY,
            }), mock.patch.object(giant_file_ratchet, "audit_repository_admissions",
                                  return_value=AdmissionAudit((), (), "a" * 40)), \
                    mock.patch.object(progress, "archive"), \
                    mock.patch.object(giant_files._INVENTORY, "giant_file_snapshot",
                                      return_value={"modules": {path: prior}, "raw_modules": {path: prior}}), \
                    mock.patch.object(HARNESS, "load_check_specs", return_value=[giant_file_ratchet.CHECK]), \
                    mock.patch.object(HARNESS, "load_allowlist", return_value={}), \
                    mock.patch.object(sys, "stdout"), mock.patch.object(sys, "stderr"):
                self.assertEqual(HARNESS.main(["--check"]), 0)

    def test_allowlist_suppresses(self) -> None:
        over = "fn x() {}\n" * (giant_files.THRESHOLD + 50)
        with _FakeSrcTree({"src/services/discord/tmux_watcher.rs": over, **self._HISTORY}) as root:
            _write(
                root,
                self._BASELINE,
                """
                [giant_file_ratchet]
                "src/services/discord/tmux_watcher.rs" = 1000
                """,
            )
            hits = list(
                giant_file_ratchet.CHECK.runner(
                    {"src/services/discord/tmux_watcher.rs"}
                )
            )
        self.assertEqual(hits, [])


def _residue_body(prod_lines: int, test_lines: int) -> str:
    """A Rust source body with ``prod_lines`` production fns plus a whole
    ``#[cfg(test)] mod tests`` block of ``test_lines`` inner fns.

    ``split_prod_test_lines`` (shared with ``giant_files``) counts the entire
    ``#[cfg(test)] mod { ... }`` span — attribute line through closing brace —
    as test LoC, and everything else as production LoC.
    """

    prod = "".join(f"fn p{i}() {{}}\n" for i in range(prod_lines))
    inner = "".join(f"    fn t{i}() {{}}\n" for i in range(test_lines))
    return f"{prod}#[cfg(test)]\nmod tests {{\n{inner}}}\n"


class ParentTestResidueCheck(unittest.TestCase):
    def test_flags_graveyard_but_not_balanced_small_or_pure_test(self) -> None:
        files = {
            # ratio ~4.3x, raw ~625 -> flagged
            "src/services/discord/graveyard.rs": _residue_body(120, 500),
            # ratio < 1 (test < prod) -> not flagged even though raw is large
            "src/services/discord/balanced.rs": _residue_body(400, 100),
            # ratio high but raw < MIN_RAW_LOC (500) -> not flagged (small file)
            "src/services/discord/tiny.rs": _residue_body(20, 100),
            # zero production LoC -> pure test module, ratio undefined, skipped
            "src/services/discord/pure_tests.rs": _residue_body(0, 300),
        }
        with _FakeSrcTree(files):
            hits = list(parent_test_residue.CHECK.runner(set()))
        self.assertEqual(_files(hits), {"src/services/discord/graveyard.rs"})

    def test_reuses_giant_files_split_accounting(self) -> None:
        body = _residue_body(120, 500)
        with _FakeSrcTree({"src/services/discord/graveyard.rs": body}):
            hits = list(parent_test_residue.CHECK.runner(set()))
            # The finding's prod/test numbers must equal the exact values the
            # shared giant_files accounting reports for the same text — i.e. we
            # reuse split_prod_test_lines, not a second divergent parser.
            prod, test = giant_files._INVENTORY.split_prod_test_lines(body)
        self.assertEqual(len(hits), 1)
        self.assertEqual(hits[0].extra["prod"], str(prod))
        self.assertEqual(hits[0].extra["test"], str(test))
        self.assertGreater(test / prod, parent_test_residue.RATIO_THRESHOLD)

    def test_allowlist_suppresses(self) -> None:
        body = _residue_body(120, 500)
        with _FakeSrcTree({"src/services/discord/graveyard.rs": body}):
            hits = list(
                parent_test_residue.CHECK.runner({"src/services/discord/graveyard.rs"})
            )
        self.assertEqual(hits, [])

    def test_baseline_gate_passes_seeded_offender(self) -> None:
        body = _residue_body(120, 500)
        rel = "src/services/discord/graveyard.rs"
        with _FakeSrcTree({rel: body}) as root:
            prod, test = giant_files._INVENTORY.split_prod_test_lines(body)
            _write(
                root,
                parent_test_residue.CONFIG_REL_PATH,
                f"""
                [parent_test_residue]
                "{rel}" = {test}
                """,
            )
            hits = list(parent_test_residue.CHECK.runner(set()))
            gate = list(parent_test_residue.CHECK.baseline_gate(hits))
        self.assertEqual(_files(hits), {rel})  # still reported
        self.assertEqual(gate, [])  # but not a regression

    def test_baseline_gate_flags_new_graveyard(self) -> None:
        body = _residue_body(120, 500)
        rel = "src/services/discord/graveyard.rs"
        with _FakeSrcTree({rel: body}):
            # No [parent_test_residue] config at all -> unseeded -> regression.
            hits = list(parent_test_residue.CHECK.runner(set()))
            gate = list(parent_test_residue.CHECK.baseline_gate(hits))
        self.assertEqual(_files(gate), {rel})
        self.assertTrue(all(f.severity == "warn" for f in gate))

    def test_baseline_gate_flags_grown_residue(self) -> None:
        body = _residue_body(120, 500)
        rel = "src/services/discord/graveyard.rs"
        with _FakeSrcTree({rel: body}) as root:
            prod, test = giant_files._INVENTORY.split_prod_test_lines(body)
            # Freeze the ceiling one line BELOW current -> current > ceiling.
            _write(
                root,
                parent_test_residue.CONFIG_REL_PATH,
                f"""
                [parent_test_residue]
                "{rel}" = {test - 1}
                """,
            )
            hits = list(parent_test_residue.CHECK.runner(set()))
            gate = list(parent_test_residue.CHECK.baseline_gate(hits))
        self.assertEqual(_files(gate), {rel})
        self.assertTrue(all(f.severity == "warn" for f in gate))


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


class ServiceServerBackflowCheck(unittest.TestCase):
    def test_flags_server_paths_outside_assembly_modules(self) -> None:
        body = """
        use crate::server::routes::AppState;
        async fn handler() {
            super::server::routes::dispatches::resolve_channel_alias("ops");
        }
        """
        comments_only = """
        // use crate::server::routes::AppState;
        /* super::server::routes::dispatches */
        pub fn clean() {}
        """
        inline_test_module = """
        pub fn clean() {}

        #[cfg(test)]
        mod tests {
            use crate::server::outbox_worker::drain_message_outbox_once;
        }
        """
        with _FakeSrcTree(
            {
                "src/services/dirty.rs": body,
                "src/engine/dirty.rs": body,
                "src/lib.rs": "pub use server::routes::AppState;\n",
                "src/services/comments.rs": comments_only,
                "src/services/inline_test_module.rs": inline_test_module,
                "src/server/routes/dirty.rs": body,
                "src/cli/dirty.rs": body,
                "src/launch.rs": body,
                "src/services/dirty_tests.rs": body,
            }
        ):
            hits = list(service_server_backflow.CHECK.runner(set()))
        self.assertEqual(
            _files(hits), {"src/services/dirty.rs", "src/engine/dirty.rs", "src/lib.rs"}
        )
        self.assertEqual(len(hits), 5)

    def test_flags_import_forms_without_a_crate_server_literal(self) -> None:
        dirty = {
            "src/kanban/group.rs": "use crate::{config, server::routes::AppState};\n",
            "src/kanban/alias.rs": "use crate::{server as srv};\n",
            "src/kanban/glob.rs": "use crate::*;\nfn f() { server::routes::run(); }\n",
            "src/kanban/nested.rs": "use super::super::server::cluster;\n",
            "src/kanban/macro_body.rs": (
                "macro_rules! m { () => { $crate::server::run() }; }\n"
            ),
        }
        clean = (
            "use crate::config::server_surface;\n"
            "use crate::services::{opencode::{server}};\n"
            "fn f(server: u8) -> u8 { let s = \"crate::server::x\"; server }\n"
        )
        with _FakeSrcTree({**dirty, "src/kanban/clean.rs": clean}):
            hits = list(service_server_backflow.CHECK.runner(set()))
        self.assertEqual(sorted(f.file for f in hits), sorted(dirty))

    def test_removed_runtime_root_alias_fails_check_in_any_rust_file(self) -> None:
        dirty = {
            "src/cli/dcserver.rs": (
                "pub fn agentdesk_runtime_root() -> Option<PathBuf> {\n"
                "    crate::config::runtime_root()\n}\n"
            ),
            "src/lib.rs": "pub(crate) use cli::agentdesk_runtime_root;\n",
            "src/services/fence_tests.rs": "let r = crate::cli::agentdesk_runtime_root();\n",
        }
        clean = (
            "// agentdesk_runtime_root() was removed\n"
            "fn isolate_agentdesk_runtime_root() { let _ = \"agentdesk_runtime_root\"; }\n"
        )
        baseline = json.dumps({"schema_version": 1, "total_count": 0, "files": {}})
        with _FakeSrcTree({**dirty, "src/services/clean_tests.rs": clean,
                           service_server_backflow.BASELINE_REL_PATH: baseline}), \
                mock.patch.object(HARNESS, "load_check_specs",
                                  return_value=[service_server_backflow.CHECK]), \
                mock.patch.object(HARNESS, "load_allowlist", return_value={}), \
                mock.patch.object(sys, "stdout"), mock.patch.object(sys, "stderr"):
            hits = list(service_server_backflow.CHECK.runner(set()))
            rc = HARNESS.main(["--check"])
        self.assertEqual(sorted(f.file for f in hits), sorted(dirty))
        self.assertEqual(rc, 1)

    def test_baseline_gate_allows_committed_file_counts(self) -> None:
        body = """
        use crate::server::routes::AppState;
        """
        with _FakeSrcTree({"src/services/legacy.rs": body}) as root:
            _write(
                root,
                service_server_backflow.BASELINE_REL_PATH,
                json.dumps(
                    {
                        "schema_version": 1,
                        "rule": "service_server_backflow",
                        "total_count": 1,
                        "files": {"src/services/legacy.rs": {"count": 1}},
                    }
                ),
            )
            hits = list(service_server_backflow.CHECK.runner(set()))
            baseline_gate = service_server_backflow.CHECK.baseline_gate
            self.assertIsNotNone(baseline_gate)
            failures = list(baseline_gate(hits)) if baseline_gate else []
        self.assertEqual(failures, [])

    def test_baseline_gate_flags_new_service_backflow_file(self) -> None:
        body = """
        use crate::server::routes::AppState;
        """
        with _FakeSrcTree({"src/services/new.rs": body}) as root:
            _write(
                root,
                service_server_backflow.BASELINE_REL_PATH,
                json.dumps(
                    {
                        "schema_version": 1,
                        "rule": "service_server_backflow",
                        "total_count": 1,
                        "files": {"src/services/old.rs": {"count": 1}},
                    }
                ),
            )
            hits = list(service_server_backflow.CHECK.runner(set()))
            baseline_gate = service_server_backflow.CHECK.baseline_gate
            self.assertIsNotNone(baseline_gate)
            failures = list(baseline_gate(hits)) if baseline_gate else []
        self.assertEqual(_files(failures), {"src/services/new.rs"})


class ServiceServerBackflowRegressionCheck(unittest.TestCase):
    def _fixture_check(self, files, allowlist=None):
        baseline = json.dumps({"schema_version": 1, "total_count": 0, "files": {}})
        allowlist = {} if allowlist is None else allowlist
        source_files = {
            **files,
            service_server_backflow.BASELINE_REL_PATH: baseline,
            "fixture-allowlist.toml": "\n".join(
                f"{key} = {json.dumps(entries)}" for key, entries in allowlist.items()
            ),
        }
        with _FakeSrcTree(source_files) as root:
            raw_hits = list(service_server_backflow.CHECK.runner(set()))
            with mock.patch.object(HARNESS, "load_check_specs",
                                   return_value=[service_server_backflow.CHECK]), \
                    mock.patch.object(sys, "stdout", new=io.StringIO()) as stdout, \
                    mock.patch.object(sys, "stderr", new=io.StringIO()) as stderr:
                rc = HARNESS.main([
                    "--check", "--format", "json", "--allowlist",
                    str(root / "fixture-allowlist.toml"),
                ])
            hits = json.loads(stdout.getvalue())["checks"]["service_server_backflow"]["findings"]
        return raw_hits, hits, rc, stderr.getvalue()

    def test_root_module_reexport_cannot_hide_server_dependency(self) -> None:
        for original in ("server", "crate::server"):
            with self.subTest(original=original):
                raw, _hits, rc, _stderr = self._fixture_check({
                    "src/lib.rs": f"mod server;\nmod engine;\npub(crate) use {original} as srv;\n",
                    "src/server.rs": "pub(crate) fn run() {}\n",
                    "src/engine.rs": "pub(crate) fn f() { crate::srv::run(); }\n",
                })
                self.assertEqual([(f.file, f.line) for f in raw], [("src/lib.rs", 3)])
                self.assertEqual(rc, 1)

    def test_bare_local_server_imports_are_not_root_dependencies(self) -> None:
        local = "mod server {}\nuse server as srv;\n"
        raw, _hits, rc, _stderr = self._fixture_check({
            "src/engine.rs": local,
            "src/lib.rs": "mod local {\n" + local + "}\n",
        })
        self.assertEqual(raw, [])
        self.assertEqual(rc, 0)

    def test_use_group_alias_destination_is_not_a_server_path(self) -> None:
        for visibility in ("", "pub(crate) "):
            for imports in (
                "use crate::{config::runtime_root as server, error::AppResult};\n",
                "use crate::config::runtime_root as server;\nuse crate::error::AppResult;\n",
                "use crate::services::{opencode::{server}};\n",
            ):
                with self.subTest(visibility=visibility, imports=imports):
                    raw, _hits, rc, _stderr = self._fixture_check({
                        "src/engine.rs": visibility + imports + "fn f() { let _ = server(); }\n",
                    })
                    self.assertEqual(raw, [])
                    self.assertEqual(rc, 0)

    def test_use_groups_keep_original_server_paths_and_nested_prefixes(self) -> None:
        imports = (
            "use crate::{server as srv, error::AppResult};\n",
            "pub(crate) use crate::{config, {server as srv}};\n",
            "use super::super::{config, {server as srv}};\n",
            "macro_rules! m { () => { use $crate::{server as srv}; }; }\n",
        )
        for source in imports:
            with self.subTest(source=source):
                raw, _hits, rc, _stderr = self._fixture_check({"src/engine/probe.rs": source})
                self.assertEqual(len(raw), 1)
                self.assertEqual(raw[0].rule, service_server_backflow.BACKFLOW_RULE)
                self.assertEqual(rc, 1)

    def test_backflow_allowlist_is_rejected_and_findings_are_preserved(self) -> None:
        path = "src/engine/probe.rs"
        for entry in (path, f"{path}:1"):
            with self.subTest(entry=entry):
                raw, hits, rc, stderr = self._fixture_check(
                    {path: "use crate::server as srv;\n"},
                    {"service_server_backflow": [entry]},
                )
                self.assertEqual(len(raw), 1)
                self.assertTrue(any(f["file"] == path and f["line"] == 1 for f in hits))
                self.assertEqual(rc, 1)
                self.assertIn("does not support allowlist", stderr)

    def test_removed_alias_allowlist_is_rejected_and_error_is_preserved(self) -> None:
        path = "src/lib.rs"
        for entry in (path, f"{path}:1"):
            with self.subTest(entry=entry):
                raw, hits, rc, stderr = self._fixture_check(
                    {path: "pub use crate::config::runtime_root as agentdesk_runtime_root;\n"},
                    {"service_server_backflow": [entry]},
                )
                self.assertEqual(len(raw), 1)
                self.assertTrue(any(f["rule"] == service_server_backflow.ALIAS_RULE
                                    and f["severity"] == "error" for f in hits))
                self.assertEqual(rc, 1)
                self.assertIn("does not support allowlist", stderr)

    def test_invalid_backflow_allowlist_fails_without_source_findings(self) -> None:
        raw, hits, rc, stderr = self._fixture_check(
            {"src/lib.rs": "pub fn clean() {}\n"},
            {"service_server_backflow": ["src/absent.rs"]},
        )
        self.assertEqual(raw, [])
        self.assertTrue(any(f["severity"] == "error" for f in hits))
        self.assertEqual(rc, 1)
        self.assertIn("does not support allowlist", stderr)
        for allowlist in ({}, {"service_server_backflow": []}):
            with self.subTest(allowlist=allowlist):
                raw, hits, rc, _stderr = self._fixture_check(
                    {"src/lib.rs": "pub fn clean() {}\n"}, allowlist,
                )
                self.assertEqual((raw, hits, rc), ([], [], 0))

    def test_backflow_allowlist_rejection_applies_to_report_mode(self) -> None:
        path = "src/engine/probe.rs"
        with _FakeSrcTree({path: "use crate::server as srv;\n"}):
            for check_stale in (False, True):
                with self.subTest(check_stale=check_stale):
                    findings, _stale = HARNESS.run_all(
                        [service_server_backflow.CHECK],
                        {"service_server_backflow": {path}}, check_stale=check_stale,
                    )
                    hits = findings[service_server_backflow.CHECK.key]
                    self.assertTrue(any(f.file == path and f.line == 1 for f in hits))
                    self.assertTrue(any(f.severity == "error" and
                                        "does not support allowlist" in f.message for f in hits))

    def test_other_checks_keep_file_and_line_allowlists(self) -> None:
        path = "src/services/probe.rs"
        body = "async fn f() { ch.send_message(&http, |m| m.content(\"x\")).await; }\n"
        with _FakeSrcTree({path: body}) as root:
            raw = list(direct_discord_sends.CHECK.runner(set()))
            self.assertEqual(len(raw), 1)
            for entry in (path, f"{path}:{raw[0].line}"):
                with self.subTest(entry=entry):
                    _write(root, "fixture-allowlist.toml",
                           "direct_discord_sends = " + json.dumps([entry]))
                    allowlist = HARNESS.load_allowlist(root / "fixture-allowlist.toml")
                    for check_stale in (False, True):
                        findings, stale = HARNESS.run_all(
                            [direct_discord_sends.CHECK], allowlist, check_stale=check_stale,
                        )
                        self.assertEqual(findings[direct_discord_sends.CHECK.key], [])
                        self.assertEqual(stale, {})

    def test_renamed_public_runtime_root_forwarder_and_glob_fail_check(self) -> None:
        for location in ("src/cli/mod.rs", "src/cli/dcserver.rs", "src/lib.rs"):
            for body in ("crate::config::runtime_root()", "return crate::config::runtime_root();"):
                with self.subTest(location=location, body=body):
                    files = {
                        "src/lib.rs": "mod cli;\npub(crate) use cli::*;\n",
                        "src/engine.rs": "fn consume() { let _ = crate::different_root(); }\n",
                        location: "pub(crate) fn different_root() -> Option<PathBuf> { " + body + " }\n",
                    }
                    raw, _hits, rc, _stderr = self._fixture_check(files)
                    self.assertEqual([(f.file, f.rule, f.severity) for f in raw],
                                     [(location, service_server_backflow.ALIAS_RULE, "error")])
                    self.assertEqual(rc, 1)

    def test_renamed_public_runtime_root_reexports_fail_check(self) -> None:
        sources = (
            ("src/lib.rs", "pub(crate) use config::runtime_root as different_root;\n"),
            ("src/lib.rs", "pub use crate::config::{runtime_root as different_root, something};\n"),
            ("src/cli/mod.rs", "pub use crate::config::runtime_root as different_root;\n"),
        )
        for path, source in sources:
            with self.subTest(path=path, source=source):
                raw, _hits, rc, _stderr = self._fixture_check({path: source})
                self.assertEqual(len(raw), 1)
                self.assertEqual((raw[0].file, raw[0].rule, raw[0].severity),
                                 (path, service_server_backflow.ALIAS_RULE, "error"))
                self.assertEqual(rc, 1)

    def test_runtime_root_forwarders_under_cfg_and_macro_are_detected(self) -> None:
        sources = (
            "#[cfg(windows)]\npub fn different_root() -> Option<PathBuf> { crate::config::runtime_root() }\n",
            "#[cfg(feature = \"probe\")]\npub fn different_root() -> Option<PathBuf> { crate::config::runtime_root() }\n",
            "macro_rules! root { () => { pub fn different_root() -> Option<PathBuf> { $crate::config::runtime_root() } }; }\n",
        )
        for source in sources:
            with self.subTest(source=source):
                raw, _hits, rc, _stderr = self._fixture_check({"src/cli/nested.rs": source})
                self.assertEqual(len(raw), 1)
                self.assertEqual(raw[0].rule, service_server_backflow.ALIAS_RULE)
                self.assertEqual(rc, 1)

    def test_canonical_calls_helpers_and_prose_are_not_runtime_root_aliases(self) -> None:
        clean = {
            "src/config.rs": "pub fn runtime_root() -> Option<PathBuf> { resolve() }\n",
            "src/engine.rs": "pub fn read() -> Option<PathBuf> { crate::config::runtime_root() }\n",
            "src/cli/mod.rs": (
                "fn private_helper() -> Option<PathBuf> { crate::config::runtime_root() }\n"
                "pub fn with_behavior() -> Option<PathBuf> { log(); crate::config::runtime_root() }\n"
                "pub fn initialize() { crate::config::runtime_root(); }\n"
                "#[cfg(test)]\npub fn fixture_root() -> Option<PathBuf> { crate::config::runtime_root() }\n"
                "#[cfg(test)]\nfn isolate_agentdesk_runtime_root() { let _ = crate::config::runtime_root(); }\n"
                "// pub fn prose() { crate::config::runtime_root() }\n"
                "const EXAMPLE: &str = r#\"pub fn example() { crate::config::runtime_root() }\"#;\n"
                "/* agentdesk_runtime_root() */\n"
            ),
            "src/cli/nested.rs": "pub fn local() -> Option<PathBuf> { super::config::runtime_root() }\n",
            "src/services/clean_tests.rs": "fn isolate_agentdesk_runtime_root() {}\n",
        }
        raw, hits, rc, _stderr = self._fixture_check(clean)
        self.assertEqual((raw, hits, rc), ([], [], 0))


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


class FooterViewWritesCheck(unittest.TestCase):
    def test_live_footer_write_matcher_covers_fq_alias_and_turngateway_forms(self) -> None:
        watcher_fq = """
        async fn update_status(http: Http, channel_id: ChannelId, status_msg_id: MessageId) {
            let panel_text = shared.ui.placeholder_live_events.render_status_panel(
                channel_id,
                &watcher_provider,
                started_at_unix,
            );
            crate::services::discord::http::edit_channel_message(
                &http,
                channel_id,
                status_msg_id,
                &panel_text,
            )
            .await
            .ok();
        }
        """
        watcher_alias = """
        async fn stream_footer(http: Http, channel_id: ChannelId, msg_id: MessageId) {
            let display_text = build_watcher_streaming_edit_text(
                shared.ui.status_panel_v2_enabled,
                current_portion,
                &status_block,
                &watcher_provider,
            );
            edit_channel_message(&http, channel_id, msg_id, &display_text)
                .await
                .ok();
        }
        """
        bridge_turngateway = """
        async fn stream_bridge(gateway: Arc<dyn TurnGateway>, channel_id: ChannelId) {
            let stable_display_text = build_turn_bridge_streaming_edit_text(
                shared_owned.ui.status_panel_v2_enabled,
                current_portion,
                &status_block,
                &provider,
            );
            TurnGateway::edit_message(
                gateway.as_ref(),
                channel_id,
                current_msg_id,
                &stable_display_text,
            )
            .await
            .ok();
        }
        """
        with _FakeSrcTree(
            {
                "src/services/discord/tmux_watcher.rs": watcher_fq + watcher_alias,
                "src/services/discord/turn_bridge/mod.rs": bridge_turngateway,
            }
        ):
            hits = list(footer_view_writes.CHECK.runner(set()))

        self.assertEqual(
            [hit.message for hit in hits],
            [
                "live footer/status-panel Discord write pending S4-b2: "
                "`crate::services::discord::http::edit_channel_message(`",
                "live footer/status-panel Discord write pending S4-b2: "
                "`edit_channel_message(`",
                "live footer/status-panel Discord write pending S4-b2: "
                "`TurnGateway::edit_message(`",
            ],
        )

    def test_live_footer_write_matcher_ignores_non_footer_write(self) -> None:
        body = """
        async fn plain_notice(gateway: Arc<dyn TurnGateway>, channel_id: ChannelId) {
            gateway
                .send_message(channel_id, "plain operator notice")
                .await
                .ok();
        }
        """
        with _FakeSrcTree({"src/services/discord/turn_bridge/mod.rs": body}):
            hits = list(footer_view_writes.CHECK.runner(set()))
        self.assertEqual(hits, [])


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
        same = "let limit = limit.clamp(1, 999);\n"
        unique = "let limit = limit.clamp(1, 50);\n"
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
        body = "let limit = limit.clamp(1, 2_000);\n"
        with _FakeSrcTree({"src/server/routes/only.rs": body}):
            hits = list(limit_clamp_duplication.CHECK.runner(set()))
        self.assertEqual(_files(hits), {"src/server/routes/only.rs"})

    def test_helper_definition_marker_opts_out(self) -> None:
        helper = (
            "// audit-allow: limit_clamp_helper_definition\n"
            "pub fn clamp_api_limit(limit: Option<usize>) -> usize {\n"
            "    limit.unwrap_or(50).clamp(1, 2_000)\n"
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
        outbox_route = (
            REPO_ROOT / "src" / "server" / "routes" / "message_outbox.rs"
        ).read_text(encoding="utf-8")
        source_registry = (
            REPO_ROOT
            / "src"
            / "services"
            / "discord"
            / "outbound"
            / "source_registry.rs"
        ).read_text(encoding="utf-8")

        self.assertIn("/api/message-outbox/monitor-alerts", monitor)
        self.assertIn('source: "auto-queue-monitor"', outbox_route)
        self.assertIn('"auto-queue-monitor"', source_registry)


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
    def test_runs_all_fourteen_checks_and_emits_yaml_keys(self) -> None:
        with _FakeSrcTree({"src/main.rs": "fn main() {}\n"}):
            specs = HARNESS.load_check_specs()
            findings, _ = HARNESS.run_all(specs, {})
            yaml_text = HARNESS.render_yaml(specs, findings)
            json_text = HARNESS.render_json(specs, findings)
            md_text = HARNESS.render_markdown(specs, findings)
        for key in (
            "giant_files",
            "giant_file_ratchet",
            "parent_test_residue",
            "namespace_size_caps",
            "route_srp_violations",
            "service_server_backflow",
            "direct_discord_sends",
            "direct_discord_reactions",
            "footer_view_writes",
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
            findings, _ = HARNESS.run_all(specs, {})
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
                "direct_discord_sends",
                "direct_discord_reactions",
                "footer_view_writes",
                "manual_json_row_mapping",
                "legacy_sqlite_refs",
                "source_of_truth_alias_writes",
                "git_subprocess_callsites",
            },
        )
        self.assertEqual(
            baseline_gated,
            {"route_srp_violations", "service_server_backflow", "parent_test_residue"},
        )
        warning_only = {
            "giant_file_ratchet",
            "route_srp_violations",
            "limit_clamp_duplication",
            "parent_test_residue",
        }
        self.assertTrue(all(not spec.hard_gate for spec in specs if spec.key in warning_only))

    def test_renderers_report_hard_gate_enabled(self) -> None:
        with _FakeSrcTree({"src/main.rs": "fn main() {}\n"}):
            specs = HARNESS.load_check_specs()
            findings, _ = HARNESS.run_all(specs, {})
            yaml_text = HARNESS.render_yaml(specs, findings)
            json_payload = json.loads(HARNESS.render_json(specs, findings))
        self.assertIn("hard_gate_enabled: true", yaml_text)
        self.assertIn("hard_gate_count: 7", yaml_text)
        self.assertIn("baseline_gate_count: 3", yaml_text)
        self.assertIs(json_payload["hard_gate_enabled"], True)
        self.assertEqual(json_payload["hard_gate_count"], 7)
        self.assertEqual(json_payload["baseline_gate_count"], 3)

    def test_check_mode_returns_zero_with_no_findings(self) -> None:
        with _FakeSrcTree({"src/main.rs": "fn main() {}\n"}) as root:
            allowlist = root / "empty.toml"
            allowlist.write_text("", encoding="utf-8")
            with mock.patch.object(sys, "stdout", new=mock.MagicMock()):
                rc = HARNESS.main(
                    ["--check", "--format", "json", "--allowlist", str(allowlist)]
                )
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

    def test_check_mode_warns_on_route_srp_baseline_regression(self) -> None:
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
        self.assertEqual(rc, 0)

    def test_check_mode_fails_on_server_backflow_regression(self) -> None:
        body = "use crate::server::routes::AppState;\n"
        with _FakeSrcTree({"src/engine/new.rs": body}) as root:
            _write(
                root,
                service_server_backflow.BASELINE_REL_PATH,
                json.dumps(
                    {
                        "schema_version": 1,
                        "rule": "service_server_backflow",
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

    def test_check_mode_warns_on_undocumented_new_giant(self) -> None:
        big = "fn x() {}\n" * (giant_files.THRESHOLD + 5)
        with _FakeSrcTree({"src/new_giant.rs": big,
                           GiantFileRatchetCheck._BASELINE: "[giant_file_ratchet]\n",
                           **GiantFileRatchetCheck._HISTORY}) as root:
            allowlist = root / "empty.toml"
            allowlist.write_text("", encoding="utf-8")
            with mock.patch.object(sys, "stdout", new=mock.MagicMock()), mock.patch.object(
                sys, "stderr", new=io.StringIO()
            ):
                rc = HARNESS.main(["--check", "--format", "json", "--allowlist", str(allowlist)])
                self.assertIn("warning `giant_file_ratchet` src/new_giant.rs", sys.stderr.getvalue())
                self.assertIn("+1005 production lines", sys.stderr.getvalue())
        self.assertEqual(rc, 0)

    def test_operational_guards_still_block_real_fixtures(self) -> None:
        fixtures = ((direct_discord_sends.CHECK, 'ch.send_message(&http, payload);'),
                    (direct_discord_reactions.CHECK, 'ch.create_reaction(&http, msg, reaction);'),
                    (footer_view_writes.CHECK, 'register_completion_footer_target(target);'),
                    (manual_json_mapping.CHECK, 'row.try_get::<serde_json::Value, _>("data")?;'),
                    (legacy_sqlite.CHECK, 'use rusqlite::Connection;'),
                    (git_subprocess.CHECK, 'std::process::Command::new("git");'),
                    (source_of_truth_alias.CHECK, 'let path = repo_root.join("ARCHITECTURE.md"); std::fs::write(&path, contents)?;'))
        for spec, source in fixtures:
            with self.subTest(rule=spec.key), _FakeSrcTree({"src/services/fixture.rs": source}), \
                    mock.patch.object(HARNESS, "load_check_specs", return_value=[spec]), \
                    mock.patch.object(HARNESS, "load_allowlist", return_value={}), \
                    mock.patch.object(sys, "stdout"), mock.patch.object(sys, "stderr"):
                self.assertEqual(HARNESS.main(["--check"]), 1)

    def test_warning_gate_metadata_cannot_promote_warning_to_error(self) -> None:
        finding = common.Finding("style", "warn", "src/style.rs", None, "style drift")
        spec = HARNESS.CheckSpec("style", "Style", "Style", True, lambda _: [finding],
                                 lambda _: [finding])
        with mock.patch.object(HARNESS, "load_check_specs", return_value=[spec]), \
                mock.patch.object(HARNESS, "load_allowlist", return_value={}), \
                mock.patch.object(sys, "stdout", new=mock.MagicMock()), \
                mock.patch.object(sys, "stderr", new=mock.MagicMock()) as stderr:
            self.assertEqual(HARNESS.main(["--check"]), 0)
        self.assertIn("warning", " ".join(str(c) for c in stderr.write.call_args_list))

    def test_error_blocks_even_when_check_is_report_only(self) -> None:
        finding = common.Finding("scanner", "error", "src/error.rs", None, "scan failed")
        spec = HARNESS.CheckSpec("scanner", "Scanner", "Scanner", False, lambda _: [finding])
        with mock.patch.object(HARNESS, "load_check_specs", return_value=[spec]), \
                mock.patch.object(HARNESS, "load_allowlist", return_value={}), \
                mock.patch.object(sys, "stdout", new=mock.MagicMock()), \
                mock.patch.object(sys, "stderr", new=mock.MagicMock()):
            self.assertEqual(HARNESS.main(["--check"]), 1)

    def test_malformed_warning_baseline_still_blocks(self) -> None:
        with _FakeSrcTree({"src/services/fixture.rs": "use crate::server::routes::AppState;",
                           service_server_backflow.BASELINE_REL_PATH: "invalid json"}), \
                mock.patch.object(HARNESS, "load_check_specs", return_value=[service_server_backflow.CHECK]), \
                mock.patch.object(HARNESS, "load_allowlist", return_value={}), \
                mock.patch.object(sys, "stdout"), mock.patch.object(sys, "stderr"):
            self.assertEqual(HARNESS.main(["--check"]), 1)

    def test_scanner_exception_is_not_a_warning_pass(self) -> None:
        spec = HARNESS.CheckSpec("scanner", "Scanner", "Scanner", False,
                                 mock.Mock(side_effect=RuntimeError("scan failed")))
        with mock.patch.object(HARNESS, "load_check_specs", return_value=[spec]):
            with self.assertRaisesRegex(RuntimeError, "scan failed"):
                HARNESS.main(["--check"])

    def test_size_style_fixtures_warn_without_blocking(self) -> None:
        fixtures = ((giant_files.CHECK, {"src/giant.rs": "fn x() {}\n" * 1000}),
                    (namespace_size_caps.CHECK, {"src/size.rs": "fn x() {}\n" * 3,
                     "scripts/audit_maintainability_config.toml":
                     '[namespace_size_caps]\n"src/size.rs" = 2\n'}),
                    (parent_test_residue.CHECK, {"src/residue.rs": _residue_body(100, 450)}))
        for spec, files in fixtures:
            with self.subTest(rule=spec.key), _FakeSrcTree(files), \
                    mock.patch.object(HARNESS, "load_check_specs", return_value=[spec]), \
                    mock.patch.object(HARNESS, "load_allowlist", return_value={}), \
                    mock.patch.object(sys, "stdout", new=mock.MagicMock()), \
                    mock.patch.object(sys, "stderr", new=mock.MagicMock()) as stderr:
                self.assertEqual(HARNESS.main(["--check"]), 0)
                self.assertIn("warning", " ".join(str(c) for c in stderr.write.call_args_list))

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


class ModuleLayersCheck(unittest.TestCase):
    def setUp(self) -> None:
        self.layers = importlib.import_module("audit_maintainability.checks.module_layers")

    @staticmethod
    def _node(name: str = "", crate: str = "src/lib.rs") -> str:
        return f"{crate}::crate" + (f"::{name}" if name else "")

    def test_nested_import_aliases_and_reexports_keep_canonical_owners(self) -> None:
        with _FakeSrcTree({"src/lib.rs": """
            mod origin {
                pub struct Item;
                pub mod nested { pub struct Other; }
            }
            mod facade {
                pub use crate::origin::Item as Export;
                pub use crate::origin::nested;
            }
            mod consumer {
                use crate::{facade::{Export as Alias, nested::{self as deep, Other}}};
                struct Holder { value: Alias, other: Other }
                fn call() { deep::Other; crate::facade::Export; }
            }
        """}) as root:
            result = self.layers.analyze(root)
        consumer, origin = self._node("consumer"), self._node("origin")
        edges = [e for e in result["edges"] if e["source"] == consumer]
        targets = {e["target"] for e in edges}
        self.assertIn(origin, targets)
        self.assertIn(self._node("origin::nested"), targets)
        self.assertNotIn(self._node("facade"), targets)
        self.assertTrue(all(e["sites"] for e in edges))
        self.assertTrue(all(site.startswith("src/lib.rs:") for e in edges for site in e["sites"]))

    def test_relative_paths_follow_logical_modules_and_declarations_are_not_edges(self) -> None:
        with _FakeSrcTree({"src/lib.rs": """
            mod target { pub fn hit() {} }
            mod consumer {
                mod local { pub fn hit() {} }
                fn call() {
                    crate::target::hit();
                    super::target::hit();
                    self::local::hit();
                }
            }
        """}) as root:
            result = self.layers.analyze(root)
        pairs = {(e["source"], e["target"]) for e in result["edges"]}
        self.assertIn((self._node("consumer"), self._node("target")), pairs)
        self.assertIn((self._node("consumer"), self._node("consumer::local")), pairs)
        self.assertFalse(any(source == self._node() for source, _target in pairs))
        self.assertEqual(result["coverage"]["logical_modules"], 4)

    def test_test_only_functions_fields_and_modules_do_not_add_production_edges(self) -> None:
        with _FakeSrcTree({"src/lib.rs": """
            mod production { pub struct Kept; }
            mod testing { pub struct Hidden; }
            #[cfg(test)]
            mod tests;
            mod consumer {
                struct Record {
                    kept: crate::production::Kept,
                    #[cfg(test)]
                    hidden: crate::testing::Hidden,
                }
                #[cfg(test)]
                fn only_test() { crate::testing::Hidden; }
                fn regular() { crate::production::Kept; }
            }
        """, "src/tests.rs": "use crate::testing::Hidden;"}) as root:
            result = self.layers.analyze(root)
        targets = {e["target"] for e in result["edges"] if e["source"] == self._node("consumer")}
        self.assertEqual(targets, {self._node("production")})
        self.assertNotIn("src/tests.rs", result["coverage"]["reached_files"])
        self.assertNotIn(self._node("tests"), result["nodes"])

    def test_non_test_cfgs_and_multiple_crate_roots_remain_visible(self) -> None:
        with _FakeSrcTree({"src/lib.rs": """
            #[cfg(unix)] mod unix_side;
            #[cfg(windows)] mod windows_side;
            #[cfg(feature = "optional")] mod feature_side;
        """, "src/unix_side.rs": "pub fn run() {}",
            "src/windows_side.rs": "pub fn run() {}",
            "src/feature_side.rs": "pub fn run() {}",
            "src/main.rs": "mod main_only { pub fn run() {} }"}) as root:
            result = self.layers.analyze(root)
        coverage = result["coverage"]
        self.assertEqual(coverage["cfg_scope"], "all-non-test-cfg-union")
        self.assertTrue({"src/unix_side.rs", "src/windows_side.rs", "src/feature_side.rs"}
                        <= set(coverage["reached_files"]))
        self.assertIn(self._node(), result["nodes"])
        self.assertIn(self._node(crate="src/main.rs"), result["nodes"])
        self.assertEqual(set(coverage["unclassified_modules"]), set(result["nodes"]))
        self.assertEqual(result["schema_version"], 1)
        self.assertIn("input_sha", result)

    def test_read_failures_and_missing_modules_are_coverage_failures(self) -> None:
        real_read = Path.read_text
        with _FakeSrcTree({"src/lib.rs": "mod unreadable; mod missing;",
                           "src/unreadable.rs": "pub fn invisible() {}"}) as root:
            def read(path, *args, **kwargs):
                if path == root / "src/unreadable.rs":
                    raise OSError("synthetic permission denied")
                return real_read(path, *args, **kwargs)
            with mock.patch.object(Path, "read_text", read):
                result = self.layers.analyze(root)
        coverage = result["coverage"]
        self.assertIn("unreadable.rs", str(coverage["read_failures"]))
        self.assertIn("synthetic permission denied", str(coverage["read_failures"]))
        self.assertIn("missing", str(coverage["module_errors"]))

    def test_unresolved_globs_macros_and_dynamic_includes_stay_visible(self) -> None:
        with _FakeSrcTree({"src/lib.rs": """
            mod target { pub struct Item; }
            mod consumer {
                use crate::target::*;
                include!(concat!(env!("OUT_DIR"), "/generated.rs"));
                unknown_macro!();
                fn call() { crate::absent::run(); }
            }
        """}) as root:
            result = self.layers.analyze(root)
        unresolved = str(result["coverage"]["unresolved_references"])
        self.assertIn("glob", unresolved)
        self.assertIn("macro", unresolved)
        self.assertIn("absent", unresolved)
        self.assertIn("include", str(result["coverage"]["cfg_uncertainties"]))

    def test_exported_macro_dependencies_are_attributed_to_callers(self) -> None:
        with _FakeSrcTree({"src/lib.rs": """
            mod upper { pub fn trace() {} }
            mod macros {
                #[macro_export]
                macro_rules! trace_call { () => { $crate::upper::trace(); } }
            }
            mod lower { fn call() { crate::trace_call!(); } }
        """}) as root:
            result = self.layers.analyze(root)
        edges = [e for e in result["edges"] if e["source"] == self._node("lower")]
        self.assertIn(self._node("upper"), {e["target"] for e in edges})
        self.assertTrue(any("src/lib.rs:6" in e["sites"] for e in edges))

    def test_census_distinguishes_path_import_and_reexport_without_blocking(self) -> None:
        with _FakeSrcTree({"src/lib.rs": """
            mod services { pub mod discord { pub struct Handle; } }
            mod adapter {
                use crate::services::discord::Handle;
                pub use crate::services::discord::Handle as Export;
                fn run() { crate::services::discord::Handle; }
            }
        """}) as root:
            result = self.layers.analyze(root)
            findings = list(self.layers.CHECK.runner(set()))
        census = [r for r in result["census"] if r["source"] == self._node("adapter")]
        self.assertEqual({r["kind"] for r in census}, {"import", "reexport", "path"})
        self.assertTrue(all(r["file"] == "src/lib.rs" and r["line"] > 0 for r in census))
        self.assertTrue(findings)
        self.assertTrue(all(f.severity == "info" for f in findings))
        self.assertFalse(self.layers.CHECK.hard_gate)
        self.assertIn(self.layers.CHECK.key, {spec.key for spec in HARNESS.load_check_specs()})

    def test_alias_paths_preserve_associated_items_reexport_chains_and_block_scope(self) -> None:
        cases = (
            ("associated item", """
                mod origin { pub struct T; impl T { pub fn run() {} } }
                mod consumer {
                    use crate::origin::T as Handle;
                    fn call() { Handle::run(); }
                }
            """, {"origin"}),
            ("reexport chain", """
                mod origin { pub struct T; }
                mod first { pub use crate::origin::T as Mid; }
                mod second { pub use crate::first::Mid as Final; }
                mod consumer {
                    use crate::second::Final as Handle;
                    fn call() { Handle::run(); }
                }
            """, {"origin"}),
            ("block scope", """
                mod origin { pub struct T; }
                mod other { pub struct T; }
                mod consumer {
                    use crate::origin::T as Handle;
                    fn call() {
                        { use crate::other::T as Handle; Handle::run(); }
                        Handle::run();
                    }
                }
            """, {"origin", "other"}),
            ("raw identifiers", """
                mod r#type { pub struct r#struct; }
                mod consumer {
                    use crate::r#type::r#struct as Handle;
                    fn call() { Handle::run(); }
                }
            """, {"type"}),
        )
        for label, code, expected in cases:
            with self.subTest(case=label), _FakeSrcTree({"src/lib.rs": code}) as root:
                result = self.layers.analyze(root)
            edges = [e for e in result["edges"]
                     if e["source"] == self._node("consumer") and e["kind"] == "path"]
            self.assertEqual({e["target"] for e in edges}, {self._node(n) for n in expected})
            if label == "block scope":
                outer = next(e for e in edges if e["target"] == self._node("origin"))
                inner = next(e for e in edges if e["target"] == self._node("other"))
                self.assertIn("src/lib.rs:7", outer["sites"])
                self.assertNotIn("src/lib.rs:7", inner["sites"])

    def test_cfg_disjoint_aliases_keep_both_imports_and_expose_ambiguity(self) -> None:
        with _FakeSrcTree({"src/lib.rs": """
            mod unix_side { pub struct T; }
            mod windows_side { pub struct T; }
            mod consumer {
                #[cfg(unix)] use crate::unix_side::T as Handle;
                #[cfg(windows)] use crate::windows_side::T as Handle;
                fn call() { Handle::run(); }
            }
        """}) as root:
            result = self.layers.analyze(root)
        edges = [e for e in result["edges"]
                 if e["source"] == self._node("consumer") and e["kind"] == "import"]
        self.assertEqual({e["target"] for e in edges},
                         {self._node("unix_side"), self._node("windows_side")})
        self.assertTrue(any("ambiguous" in ref["reason"] and ref["path"] == "Handle::run"
                            for ref in result["coverage"]["unresolved_references"]))

    def test_path_redirects_and_literal_includes_keep_logical_owners(self) -> None:
        cases = (
            ({"src/lib.rs": '#[path="relocated.rs"] mod target; mod consumer;',
              "src/relocated.rs": "pub fn hit() {}",
              "src/consumer.rs": 'include!("bits.rs");',
              "src/bits.rs": "fn call() { crate::target::hit(); }"},
             "consumer", "src/bits.rs:1"),
            ({"src/lib.rs": 'mod target { pub fn hit() {} } mod outer { #[path="moved.rs"] mod inner; }',
              "src/outer/moved.rs": "fn call() { crate::target::hit(); }"},
             "outer::inner", "src/outer/moved.rs:1"),
        )
        for files, owner, site in cases:
            with self.subTest(owner=owner), _FakeSrcTree(files) as root:
                result = self.layers.analyze(root)
            edges = [e for e in result["edges"] if e["source"] == self._node(owner)]
            self.assertTrue(any(e["target"] == self._node("target") and site in e["sites"]
                                for e in edges))
            self.assertEqual(result["coverage"]["module_errors"], [])

    def test_mixed_line_test_items_and_test_attribute_are_excluded(self) -> None:
        bodies = (
            "fn regular() { crate::kept::K; } #[cfg(test)] fn hidden() { crate::testing::T; }",
            "struct Record { kept: crate::kept::K, #[cfg(test)] hidden: crate::testing::T }",
            "struct Record { #[cfg(test)] hidden: crate::testing::T, kept: crate::kept::K }",
            "#[test] fn only_test() { crate::testing::T; } fn regular() { crate::kept::K; }",
        )
        for body in bodies:
            with self.subTest(body=body), _FakeSrcTree({"src/lib.rs":
                    "mod kept { pub struct K; }\nmod testing { pub struct T; }\n"
                    + "mod consumer { " + body + " }\n"}) as root:
                result = self.layers.analyze(root)
            targets = {e["target"] for e in result["edges"] if e["source"] == self._node("consumer")}
            self.assertEqual(targets, {self._node("kept")})

    def test_sccs_measure_dependency_cycles_without_module_declaration_edges(self) -> None:
        with _FakeSrcTree({"src/lib.rs": """
            mod a { use crate::b::B; pub struct A; }
            mod b { use crate::c::C; pub struct B; }
            mod c { use crate::a::A; pub struct C; }
            mod isolated { pub struct Alone; }
        """}) as root:
            result = self.layers.analyze(root)
        self.assertEqual(set(result["sccs"][0]), {self._node(n) for n in ("a", "b", "c")})
        self.assertIn([self._node()], result["sccs"])
        self.assertIn([self._node("isolated")], result["sccs"])
        self.assertFalse(any(e["source"] == self._node() for e in result["edges"]))

    def test_macro_namespaces_visibility_and_body_spans_keep_edges_precise(self) -> None:
        cases = (
            ("separate namespaces", """
                fn same() {}
                mod macros { #[macro_export] macro_rules! same { () => {} } }
                mod caller { fn call() { crate::same(); crate::same!(); } }
            """, {("caller", "", "path"), ("caller", "macros", "macro")},
             {("caller", "macros", "path")}),
            ("visibility restriction", """
                mod a {
                    pub fn call() { self::b::run(); }
                    pub mod b { pub(in crate::a) fn run() {} }
                }
            """, {("a", "a::b", "path")}, {("a::b", "a", "path")}),
            ("same line macro boundary", """
                mod high { pub fn hit() {} }
                mod macros { #[macro_export] macro_rules! noop { () => {} } fn ordinary() { crate::high::hit(); } }
                mod caller { fn call() { crate::noop!(); } }
            """, {("macros", "high", "path"), ("caller", "macros", "macro")},
             {("caller", "high", "macro")}),
        )
        for label, code, expected, absent in cases:
            with self.subTest(case=label), _FakeSrcTree({"src/lib.rs": code}) as root:
                result = self.layers.analyze(root)
            edges = {(e["source"], e["target"], e["kind"]) for e in result["edges"]}
            self.assertTrue({(self._node(s), self._node(t), k) for s, t, k in expected} <= edges)
            self.assertFalse({(self._node(s), self._node(t), k) for s, t, k in absent} & edges)
            if label == "visibility restriction":
                self.assertTrue(all(len(group) == 1 for group in result["sccs"]))

    def test_module_paths_outside_repository_are_reported_without_crashing(self) -> None:
        with TemporaryDirectory() as outside:
            external = Path(outside) / "external.rs"
            external.write_text("pub fn external() {}", encoding="utf-8")
            for declaration in (f'#[path="{external.as_posix()}"] mod external;',
                                f'include!("{external.as_posix()}");'):
                with self.subTest(declaration=declaration), _FakeSrcTree({"src/lib.rs": declaration}) as root:
                    result = self.layers.analyze(root)
                coverage = result["coverage"]
                self.assertTrue(coverage["module_errors"])
                self.assertIn("external.rs", str(coverage["module_errors"]))


if __name__ == "__main__":
    unittest.main()
