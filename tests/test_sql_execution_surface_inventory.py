import contextlib
import io
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import check_sql_execution_surface_inventory as scanner


class SqlExecutionSurfaceInventoryTests(unittest.TestCase):
    def write(self, root: Path, rel: str, text: str = "") -> Path:
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    def tracked(self, names: list[str]) -> mock._patch:
        payload = b"\0".join(name.encode() for name in names) + b"\0"
        result = subprocess.CompletedProcess(
            ["git", "ls-files", "-z", "--"], 0, stdout=payload, stderr=b""
        )
        return mock.patch.object(scanner.subprocess, "run", return_value=result)

    def run_main(self, root: Path, *args: str) -> tuple[int, str, str]:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            rc = scanner.main(["--repo-root", str(root), *args])
        return rc, stdout.getvalue(), stderr.getvalue()

    def test_enumerates_only_tracked_three_roots(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            names = [
                "src/lib.rs",
                "policies/rule.js",
                "policies/default.yaml",
                "migrations/postgres/0001.sql",
            ]
            for name in names:
                self.write(root, name, "// fixture\n")
            self.write(root, "policies/ignored.js", "agentdesk.db.query('ignored')")
            self.write(root, "README.md", "not an input")
            with self.tracked(names):
                inputs = scanner.enumerate_tracked_inputs(root)
            self.assertEqual([item.rel_path for item in inputs], sorted(names))
            self.assertEqual({item.root for item in inputs}, {"src", "policies", "migrations/postgres"})

    def test_tracked_symlink_and_unexpected_extension_fail_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.write(root, "src/real.rs", "fn main() {}\n")
            os.symlink(root / "src/real.rs", root / "src/link.rs")
            with self.tracked(["src/link.rs"]):
                with self.assertRaises(scanner.InventoryError):
                    scanner.enumerate_tracked_inputs(root)

            self.write(root, "policies/bad.txt", "fixture")
            with self.tracked(["policies/bad.txt"]):
                with self.assertRaises(scanner.InventoryError):
                    scanner.enumerate_tracked_inputs(root)

    def test_js_direct_member_bracket_and_supported_aliases(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = self.write(
                root,
                "policies/forms.js",
                """
agentdesk.db.query("SELECT id FROM cards");
agentdesk["db"]["execute"]("DELETE FROM cards");
const db = agentdesk.db;
db.query("SELECT id FROM cards");
const { execute: rawExecute } = agentdesk["db"];
rawExecute("UPDATE cards SET seen = 1");
""",
            )
            records = scanner.scan_js_calls(path, root)
            self.assertEqual(len(records), 4)
            self.assertEqual([record.api for record in records].count("agentdesk.db.query"), 2)
            self.assertEqual([record.api for record in records].count("agentdesk.db.execute"), 2)

    def test_js_comments_strings_and_balanced_multiline_call(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = self.write(
                root,
                "policies/decoys.js",
                r'''
// agentdesk.db.execute("DELETE FROM decoy")
const text = "agentdesk.db.query('SELECT FROM decoy')";
/* agentdesk.db.query("SELECT FROM decoy") */
agentdesk.db.query(
  "SELECT id FROM cards WHERE id IN (SELECT id FROM cards)",
  { ids: ["x", "y"] }
);
''',
            )
            records = scanner.scan_js_calls(path, root)
            self.assertEqual(len(records), 1)
            self.assertEqual(records[0].classification, "STATIC")
            self.assertEqual(records[0].line, 5)
            self.assertIn("cards", records[0].table_tokens)

    def test_js_literal_shapes_static_and_dynamic_shapes_unresolved(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = self.write(
                root,
                "policies/shapes.js",
                """
const sql = "SELECT id FROM cards";
agentdesk.db.query("SELECT id FROM cards");
agentdesk.db.query("SELECT " + "id FROM cards");
agentdesk.db.query(sql);
agentdesk.db.query(`SELECT id FROM ${table}`);
agentdesk.db.query(makeSql());
""",
            )
            records = scanner.scan_js_calls(path, root)
            self.assertEqual([record.classification for record in records], [
                "STATIC", "STATIC", "UNRESOLVED", "UNRESOLVED", "UNRESOLVED"
            ])

    def test_rust_literal_raw_and_dynamic_boundaries(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = self.write(
                root,
                "src/sql.rs",
                r'''
// sqlx::query("FROM decoy")
let _a = sqlx::query(r#"SELECT id FROM cards"#);
let _b = sqlx::query("UPDATE cards SET seen = 1");
let _c = sqlx::query(format!("SELECT * FROM {}", table));
let _d = sqlx::query(sql);
let _e = QueryBuilder::new(runtime_table);
let _f = db_execute_raw_pg(sql);
let _g = execute_policy_sql(format!("DELETE FROM {}", table));
let _h = rewrite_insert_conflict("INSERT OR REPLACE INTO cards (id) VALUES (?)");
''',
            )
            records = scanner.scan_rust_calls(path, root)
            by_api = {record.api: record for record in records if record.api != "sqlx::query"}
            query_classes = [record.classification for record in records if record.api == "sqlx::query"]
            self.assertEqual(query_classes[:2], ["STATIC", "STATIC"])
            self.assertEqual(by_api["QueryBuilder::new"].classification, "UNRESOLVED")
            self.assertEqual(by_api["db_execute_raw_pg"].classification, "UNRESOLVED")
            self.assertEqual(by_api["execute_policy_sql"].classification, "UNRESOLVED")
            self.assertEqual(by_api["rewrite_insert_conflict"].classification, "STATIC")
            self.assertIn("cards", by_api["rewrite_insert_conflict"].table_tokens)

    def test_migration_fingerprint_is_deterministic_and_distinguishes_rename_content(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            first = self.write(root, "migrations/postgres/0001.sql", "CREATE TABLE cards(id int);\n")
            tracked = scanner.TrackedInput("migrations/postgres", "MIGRATION", first, "migrations/postgres/0001.sql")
            one = scanner.scan_migrations(tracked)[0]
            again = scanner.scan_migrations(tracked)[0]
            self.assertEqual(one.fingerprint, again.fingerprint)
            renamed = scanner.TrackedInput("migrations/postgres", "MIGRATION", first, "migrations/postgres/0002.sql")
            self.assertNotEqual(one.fingerprint, scanner.scan_migrations(renamed)[0].fingerprint)
            first.write_text("CREATE TABLE other(id int);\n", encoding="utf-8")
            self.assertNotEqual(one.fingerprint, scanner.scan_migrations(tracked)[0].fingerprint)
            self.assertEqual(one.classification, "STATIC_FILE")
            self.assertEqual(one.table_tokens, ())

    def test_stable_sort_duplicate_rejection_and_exit_code(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.write(root, "policies/a.js", "agentdesk.db.query(sql);\n")
            self.write(root, "policies/b.js", "agentdesk.db.query(sql);\n")
            with self.tracked(["policies/b.js", "policies/a.js"]):
                result = scanner.scan_inventory(root)
            self.assertEqual([record.path for record in result.records], ["policies/a.js", "policies/b.js"])
            with self.assertRaises(scanner.InventoryError):
                scanner.validate_records([result.records[0], result.records[0]])

            self.write(root, "policies/bad.txt", "bad")
            with self.tracked(["policies/bad.txt"]):
                rc, _out, err = self.run_main(root)
            self.assertEqual(rc, 1)
            self.assertIn("LIMITS:", err)

    def test_unresolved_and_limits_remain_in_success_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.write(root, "policies/dynamic.js", "const sql = makeSql();\nagentdesk.db.execute(sql);\n")
            self.write(root, "migrations/postgres/0001.sql", "DROP TABLE cards;\n")
            names = ["policies/dynamic.js", "migrations/postgres/0001.sql"]
            with self.tracked(names):
                rc, out, err = self.run_main(root)
            self.assertEqual(rc, 0)
            self.assertEqual(err, "")
            self.assertIn("UNRESOLVED:", out)
            self.assertIn("dynamic.js agentdesk.db.execute", out)
            for limit in scanner.LIMITS:
                self.assertIn(limit, out)


if __name__ == "__main__":
    unittest.main()
