from pathlib import Path
import tempfile
import unittest

import scripts.audit_legacy_sqlite_sunset as audit


class LegacySqliteSunsetAuditTest(unittest.TestCase):
    def test_collect_metrics_classifies_prod_stub_dependency(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / "src" / "engine" / "ops"
            src.mkdir(parents=True)
            target = src / "queue_ops.rs"
            target.write_text(
                'pub fn queue(db: &Db) { let conn = db.read_conn()?; }\n',
                encoding="utf-8",
            )

            report = audit.collect_metrics(root)

        self.assertEqual(len(report.files), 1)
        self.assertEqual(report.files[0].path, "src/engine/ops/queue_ops.rs")
        self.assertEqual(report.files[0].category, "prod_stub_dependency")
        self.assertEqual(report.files[0].db_conn_calls, 1)
        self.assertEqual(report.files[0].prod_db_conn_calls, 1)

    def test_inline_unit_tests_do_not_hide_prod_stub_dependency(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / "src" / "dispatch"
            src.mkdir(parents=True)
            target = src / "dispatch_status.rs"
            target.write_text(
                "pub fn status(db: &Db) { let conn = db.separate_conn()?; }\n"
                "#[test]\n"
                "fn test_status(db: &Db) { let conn = db.read_conn()?; }\n",
                encoding="utf-8",
            )

            report = audit.collect_metrics(root)

        self.assertEqual(len(report.files), 1)
        self.assertEqual(report.files[0].path, "src/dispatch/dispatch_status.rs")
        self.assertEqual(report.files[0].category, "prod_stub_dependency")
        self.assertEqual(report.files[0].db_conn_calls, 2)
        self.assertEqual(report.files[0].prod_db_conn_calls, 1)

    def test_test_only_inline_db_calls_are_not_prod_blockers(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / "src" / "engine"
            src.mkdir(parents=True)
            target = src / "mod.rs"
            target.write_text(
                "#[cfg(test)]\n"
                "mod tests {\n"
                "    #[test]\n"
                "    fn test_config(db: &Db) { let conn = db.separate_conn()?; }\n"
                "}\n",
                encoding="utf-8",
            )

            report = audit.collect_metrics(root)

        self.assertEqual(len(report.files), 1)
        self.assertEqual(report.files[0].path, "src/engine/mod.rs")
        self.assertEqual(report.files[0].category, "test_surface")
        self.assertEqual(report.files[0].db_conn_calls, 1)
        self.assertEqual(report.files[0].prod_db_conn_calls, 0)

    def test_cfg_test_blocks_do_not_create_prod_blockers(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / "src" / "engine"
            src.mkdir(parents=True)
            target = src / "mod.rs"
            target.write_text(
                "fn drain(db: &Db) {\n"
                '    #[cfg(all(test, feature = "legacy-sqlite-tests"))]\n'
                "    {\n"
                "        let conn = db.separate_conn()?;\n"
                "    }\n"
                "}\n",
                encoding="utf-8",
            )

            report = audit.collect_metrics(root)

        self.assertEqual(len(report.files), 1)
        self.assertEqual(report.files[0].path, "src/engine/mod.rs")
        self.assertEqual(report.files[0].category, "test_surface")
        self.assertEqual(report.files[0].db_conn_calls, 1)
        self.assertEqual(report.files[0].prod_db_conn_calls, 0)

    def test_cfg_not_test_blocks_count_as_prod_blockers(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / "src" / "engine"
            src.mkdir(parents=True)
            target = src / "mod.rs"
            target.write_text(
                "fn drain(db: &Db) {\n"
                "    #[cfg(not(test))]\n"
                "    {\n"
                "        let conn = db.separate_conn()?;\n"
                "    }\n"
                "}\n",
                encoding="utf-8",
            )

            report = audit.collect_metrics(root)

        self.assertEqual(len(report.files), 1)
        self.assertEqual(report.files[0].path, "src/engine/mod.rs")
        self.assertEqual(report.files[0].category, "prod_stub_dependency")
        self.assertEqual(report.files[0].db_conn_calls, 1)
        self.assertEqual(report.files[0].prod_db_conn_calls, 1)

    def test_cfg_all_not_test_blocks_count_as_prod_blockers(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / "src" / "engine"
            src.mkdir(parents=True)
            target = src / "mod.rs"
            target.write_text(
                "fn drain(db: &Db) {\n"
                '    #[cfg(all(not(test), feature = "prod-db"))]\n'
                "    {\n"
                "        let conn = db.read_conn()?;\n"
                "    }\n"
                "}\n",
                encoding="utf-8",
            )

            report = audit.collect_metrics(root)

        self.assertEqual(len(report.files), 1)
        self.assertEqual(report.files[0].path, "src/engine/mod.rs")
        self.assertEqual(report.files[0].category, "prod_stub_dependency")
        self.assertEqual(report.files[0].db_conn_calls, 1)
        self.assertEqual(report.files[0].prod_db_conn_calls, 1)

    def test_cfg_any_test_feature_blocks_count_as_prod_blockers(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / "src" / "engine"
            src.mkdir(parents=True)
            target = src / "mod.rs"
            target.write_text(
                "fn drain(db: &Db) {\n"
                '    #[cfg(any(test, feature = "prod-db"))]\n'
                "    {\n"
                "        let conn = db.read_conn()?;\n"
                "    }\n"
                "}\n",
                encoding="utf-8",
            )

            report = audit.collect_metrics(root)

        self.assertEqual(len(report.files), 1)
        self.assertEqual(report.files[0].path, "src/engine/mod.rs")
        self.assertEqual(report.files[0].category, "prod_stub_dependency")
        self.assertEqual(report.files[0].db_conn_calls, 1)
        self.assertEqual(report.files[0].prod_db_conn_calls, 1)

    def test_tokio_test_attr_db_calls_are_test_surface(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / "src" / "engine"
            src.mkdir(parents=True)
            target = src / "mod.rs"
            target.write_text(
                "#[tokio::test]\n"
                "async fn test_bridge(db: &Db) { let conn = db.read_conn()?; }\n",
                encoding="utf-8",
            )

            report = audit.collect_metrics(root)

        self.assertEqual(len(report.files), 1)
        self.assertEqual(report.files[0].path, "src/engine/mod.rs")
        self.assertEqual(report.files[0].category, "test_surface")
        self.assertEqual(report.files[0].db_conn_calls, 1)
        self.assertEqual(report.files[0].prod_db_conn_calls, 0)

    def test_async_std_test_attr_db_calls_are_test_surface(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / "src" / "engine"
            src.mkdir(parents=True)
            target = src / "mod.rs"
            target.write_text(
                "#[async_std::test]\n"
                "async fn test_bridge(db: &Db) { let conn = db.separate_conn()?; }\n",
                encoding="utf-8",
            )

            report = audit.collect_metrics(root)

        self.assertEqual(len(report.files), 1)
        self.assertEqual(report.files[0].path, "src/engine/mod.rs")
        self.assertEqual(report.files[0].category, "test_surface")
        self.assertEqual(report.files[0].db_conn_calls, 1)
        self.assertEqual(report.files[0].prod_db_conn_calls, 0)

    def test_render_markdown_includes_phase_zero_blockers(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / "src" / "services"
            src.mkdir(parents=True)
            target = src / "tmux_lifecycle.rs"
            target.write_text(
                'fn lookup(db: &Db) { let conn = db.separate_conn()?; }\n',
                encoding="utf-8",
            )

            markdown = audit.render_markdown(audit.collect_metrics(root), top_limit=10)

        self.assertIn("## Phase 0 Blockers", markdown)
        self.assertIn("`src/services/tmux_lifecycle.rs`", markdown)

    def test_render_markdown_includes_obsolete_sqlite_remove_candidates(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / "src" / "server" / "routes" / "routes_tests"
            src.mkdir(parents=True)
            target = src / "auto_queue_tests.rs"
            target.write_text(
                '#[test]\n'
                '#[ignore = "obsolete SQLite auto-queue fixture; runtime path is PostgreSQL-only"]\n'
                "fn old_sqlite_fixture() {}\n",
                encoding="utf-8",
            )

            markdown = audit.render_markdown(audit.collect_metrics(root), top_limit=10)

        self.assertIn("## Remove/Migrate Decision Inventory", markdown)
        self.assertIn("`src/server/routes/routes_tests/auto_queue_tests.rs`", markdown)
        self.assertIn("| obsolete_sqlite_ignored_tests | 1 |", markdown)


if __name__ == "__main__":
    unittest.main()
