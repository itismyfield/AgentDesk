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


if __name__ == "__main__":
    unittest.main()
