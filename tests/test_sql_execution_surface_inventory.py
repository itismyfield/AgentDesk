import contextlib
import dataclasses
import io
import json
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
        result = subprocess.CompletedProcess(["git", "ls-files", "-z", "--"], 0, payload, b"")
        return mock.patch.object(scanner.subprocess, "run", return_value=result)

    def run_main(self, root: Path, *args: str) -> tuple[int, str, str]:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            rc = scanner.main(list(args), root)
        return rc, stdout.getvalue(), stderr.getvalue()

    def test_enumerates_only_tracked_three_roots(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            names = ["src/lib.rs", "policies/rule.js", "policies/default.yaml",
                "migrations/postgres/0001.sql",
                "migrations/postgres/immutable-checksums.json",
            ]
            for name in names:
                self.write(root, name, "// fixture\n")
            self.write(root, "policies/ignored.js", "agentdesk.db.query('ignored')")
            self.write(root, "README.md", "not an input")
            with self.tracked(names) as git:
                inputs = scanner.enumerate_tracked_inputs(root)
            self.assertEqual([item.rel_path for item in inputs], sorted(names))
            self.assertEqual({item.root for item in inputs}, {"src", "policies", "migrations/postgres"})
            self.assertEqual(git.call_args.args[0][-3:], ["src", "policies", "migrations/postgres"])
            self.assertEqual(next(i.kind for i in inputs if i.rel_path.endswith(".json")), "MIGRATION_METADATA")
            with self.tracked(["migrations/postgres/immutable-checksums.json"]):
                rc, out, err = self.run_main(root, "--verbose")
            self.assertEqual((rc, err), (0, ""))
            self.assertIn("NON_SQL_TRACKED", out)
            self.assertIn("immutable-checksums.json", out)
    def test_tracked_symlink_and_unexpected_extension_fail_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.write(root, "src/real.rs", "fn main() {}\n")
            os.symlink(root / "src/real.rs", root / "src/link.rs")
            with self.tracked(["src/link.rs"]):
                with self.assertRaises(scanner.InventoryError):
                    scanner.enumerate_tracked_inputs(root)
            self.write(root, "src/bad.txt", "fixture")
            with self.tracked(["src/bad.txt"]):
                with self.assertRaises(scanner.InventoryError):
                    scanner.enumerate_tracked_inputs(root)
            nested = self.write(root, "migrations/postgres/archive/0001.sql", "SELECT 1")
            with self.tracked([nested.relative_to(root).as_posix()]):
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
agentdesk.db.query(`SELECT id FROM cards`);
agentdesk.db.query(sql);
agentdesk.db.query("SELECT id FROM " + table);
agentdesk.db.query(`SELECT id FROM ${table}`);
agentdesk.db.query(makeSql());
""",
            )
            records = scanner.scan_js_calls(path, root)
            self.assertEqual([record.classification for record in records], [
                "STATIC", "STATIC", "STATIC", "UNRESOLVED", "UNRESOLVED",
                "UNRESOLVED", "UNRESOLVED",
            ])
            self.assertEqual(records[1].table_tokens, ("cards",))
            self.assertEqual(records[4].table_tokens, ())
            boundaries = [('"a" + "b"', "STATIC"), ('"a" + suffix', "UNRESOLVED"),
                ('`a ${suffix}`', "UNRESOLVED"),
                ('makeSql()', "UNRESOLVED"),
            ]
            self.assertEqual(
                [scanner.classify_sql_argument(value, "javascript") for value, _ in boundaries],
                [expected for _, expected in boundaries],
            )

    def test_rust_literal_raw_and_dynamic_boundaries(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = self.write(
                root,
                "src/sql.rs",
                r'''
// sqlx::query("FROM decoy")
let _decoy = r#"sqlx::query("DELETE FROM hidden")"#;
let _a = sqlx::query(r#"SELECT id FROM cards"#);
let _b = sqlx::query("UPDATE cards SET seen = 1");
let _c = sqlx::query::<Row>(r##"SELECT id FROM cards"##);
let _d = sqlx::query(format!(r#"SELECT * FROM {}"#, table));
let _e = sqlx::query(sql);
let _f = QueryBuilder::new("SELECT id FROM cards");
let _g = QueryBuilder::<Postgres>::new("SELECT id FROM cards");
let _decoy = NotQueryBuilder::<Postgres>::new("SELECT 1");
let _h = db_execute_raw_pg(&pool, "DELETE FROM cards", &[], started);
let _i = execute_policy_sql(Some(&pool), "UPDATE cards SET seen = 1", &[]);
let _j = db_query_raw(pool.clone(), "SELECT id FROM cards", "[]");
let _k = sqlx::raw_sql("CREATE TABLE things(id int)");
let _l = sqlx::raw_sql(&format!("DROP TABLE {}", table));
let _m = rewrite_insert_conflict("INSERT OR REPLACE INTO cards (id) VALUES (?)", ConflictMode::Replace);
''',
            )
            records = scanner.scan_rust_calls(path, root)
            query_classes = [record.classification for record in records if record.api == "sqlx::query"]
            self.assertEqual(query_classes, ["STATIC", "STATIC", "STATIC", "UNRESOLVED", "UNRESOLVED"])
            builder_classes = [r.classification for r in records if r.api == "QueryBuilder::new"]
            self.assertEqual(builder_classes, ["UNRESOLVED", "UNRESOLVED"])
            by_api = {r.api: r for r in records if r.api not in {"sqlx::query", "QueryBuilder::new", "sqlx::raw_sql"}}
            for api in ("db_execute_raw_pg", "execute_policy_sql", "db_query_raw", "rewrite_insert_conflict"):
                self.assertEqual(by_api[api].classification, "STATIC")
                self.assertIn("cards", by_api[api].table_tokens)
            raw_sql_classes = [r.classification for r in records if r.api == "sqlx::raw_sql"]
            self.assertEqual(raw_sql_classes, ["STATIC", "UNRESOLVED"])

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
            with self.tracked(["policies/a.js", "policies/b.js"]):
                shuffled = scanner.scan_inventory(root)
            self.assertEqual([record.path for record in result], ["policies/a.js", "policies/b.js"])
            keys = lambda rows: [(r.stable_key(), r.fingerprint) for r in rows]
            self.assertEqual(keys(result), keys(shuffled))
            before = scanner.scan_js_calls(root / "policies/a.js", root)[0]
            self.write(root, "policies/a.js", "\n\nagentdesk.db.query(sql);\n")
            after = scanner.scan_js_calls(root / "policies/a.js", root)[0]
            self.assertNotEqual(before.line, after.line)
            self.assertEqual(before.fingerprint, after.fingerprint)
            with self.assertRaises(scanner.InventoryError):
                scanner.validate_records([result[0], result[0]])

            with self.tracked(["policies/a.js"]):
                rc, out, err = self.run_main(root)
            self.assertEqual((rc, bool(out), err), (0, True, ""))

            self.write(root, "policies/bad.txt", "bad")
            with self.tracked(["policies/bad.txt"]):
                rc, _out, err = self.run_main(root)
            self.assertEqual(rc, 1)
            self.assertIn("UNRESOLVED:", err)
            self.assertIn("LIMITS:", err)

    def test_unresolved_and_limits_remain_in_success_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.write(root, "policies/dynamic.js", "const sql = makeSql();\nagentdesk.db.execute(sql);\n")
            self.write(root, "migrations/postgres/0001.sql", "DROP TABLE cards;\n")
            names = ["policies/dynamic.js", "migrations/postgres/0001.sql"]
            with self.tracked(names):
                rc, out, err = self.run_main(root)
            self.assertEqual((rc, err), (0, ""))
            self.assertIn("UNRESOLVED:", out)
            self.assertIn("dynamic.js agentdesk.db.execute", out)
            self.assertIn("ROOT COUNTS:", out)
            self.assertNotIn("STATIC_FILE migrations/postgres/MIGRATION", out)
            for limit in scanner.LIMITS:
                self.assertIn(limit, out)

            with self.tracked(names):
                rc, verbose_out, err = self.run_main(root, "--verbose")
            self.assertEqual((rc, err), (0, ""))
            self.assertIn("STATIC_FILE migrations/postgres/MIGRATION", verbose_out)

            help_out = io.StringIO()
            with contextlib.redirect_stdout(help_out), self.assertRaises(SystemExit) as exit_status:
                scanner.main(["--help"])
            self.assertEqual(exit_status.exception.code, 0)
            for definition in scanner.CLASSIFICATION_DEFINITIONS.splitlines():
                self.assertIn(definition, help_out.getvalue())

            bad_err = io.StringIO()
            with contextlib.redirect_stderr(bad_err), self.assertRaises(SystemExit) as bad_status:
                scanner.main(["--unknown"])
            self.assertEqual(bad_status.exception.code, 2)
            self.assertIn("UNRESOLVED:", bad_err.getvalue())
            self.assertIn("LIMITS:", bad_err.getvalue())

            static = self.write(root, "policies/static.js", 'agentdesk.db.query("SELECT 1");\n')
            with self.tracked([static.relative_to(root).as_posix()]):
                rc, static_out, err = self.run_main(root)
            self.assertEqual((rc, err), (0, ""))
            self.assertIn("UNRESOLVED:\n  - (none observed; absence is not completeness evidence)", static_out)

    def test_baseline_round_trip_and_bidirectional_drift(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = self.write(root, "policies/outside-manifest.js", 'agentdesk.db.query("SELECT 1");\n')
            record = scanner.scan_js_calls(path, root)[0]
            baseline_path = root / "baseline.json"
            scanner.write_baseline(baseline_path, [record], "8" * 40)
            baseline = scanner.load_baseline(baseline_path)
            self.assertNotIn("line", baseline["records"][0])
            self.assertEqual(scanner.baseline_drift([record], baseline), [])

            changed = (
                [],
                [record, dataclasses.replace(record, path="policies/new-static.js")],
                [dataclasses.replace(record, path="policies/moved.js")],
                [dataclasses.replace(record, fingerprint="sha256:" + "1" * 64)],
                [dataclasses.replace(record, classification="UNRESOLVED")],
            )
            for rows in changed:
                with self.subTest(rows=rows):
                    self.assertTrue(scanner.baseline_drift(rows, baseline))
            drift_output = scanner._render([record], scanner.baseline_drift([], baseline), root)
            self.assertTrue(drift_output.rstrip().endswith(scanner.REPIN_GUIDANCE))
            unresolved = dataclasses.replace(record, classification="UNRESOLVED")
            reverse = scanner.baseline_snapshot([unresolved], "8" * 40)
            self.assertTrue(scanner.baseline_drift([record], reverse))

            migration = self.write(root, "migrations/postgres/0001.sql", "CREATE TABLE cards(id int);\n")
            tracked = scanner.TrackedInput("migrations/postgres", "MIGRATION", migration,
                                           "migrations/postgres/0001.sql")
            migration_baseline = scanner.baseline_snapshot(scanner.scan_migrations(tracked), "8" * 40)
            migration.write_text("CREATE TABLE other(id int);\n", encoding="utf-8")
            self.assertTrue(scanner.baseline_drift(scanner.scan_migrations(tracked), migration_baseline))
            renamed = dataclasses.replace(tracked, rel_path="migrations/postgres/0002.sql")
            self.assertTrue(scanner.baseline_drift(scanner.scan_migrations(renamed), migration_baseline))

            empty_path = root / "empty.json"
            empty_path.write_text(json.dumps({
                "schema_version": 1,
                "measured_at_sha": "8" * 40,
                "records": [],
            }), encoding="utf-8")
            with self.assertRaisesRegex(scanner.InventoryError, "records"):
                scanner.load_baseline(empty_path)
            with self.assertRaisesRegex(scanner.InventoryError, "must not be empty"):
                scanner.baseline_snapshot([], "8" * 40)

    def test_gated_modes_fail_closed_on_empty_scan_and_missing_roots(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with mock.patch.object(scanner, "scan_inventory", return_value=[]):
                rc, out, err = self.run_main(root, "--write-baseline")
            self.assertEqual((rc, out), (1, ""))
            self.assertIn("inventory record set is empty", err)
            for required_root in scanner.REQUIRED_ROOTS:
                self.assertIn(f"inventory root {required_root} has 0 records", err)
            self.assertFalse((root / scanner.BASELINE_PATH).exists())

        record = scanner.SurfaceRecord(
            "src", "RUST", "src/lib.rs", "sqlx::query", "query", "STATIC",
            "sha256:" + "1" * 64,
        )
        errors = scanner.cardinality_errors([record])
        self.assertFalse(any("root src" in error for error in errors))
        self.assertTrue(any("root policies" in error for error in errors))
        self.assertTrue(any("root migrations/postgres" in error for error in errors))

    def test_rewrite_table_name_decoy_rebinding_is_red(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = self.write(root, scanner.REWRITE_PATH, """
fn rewrite_insert_conflict(sql: &str, mode: ConflictMode) {
    let rest = sql;
    let table_end = rest.len();
    let table_name = rest[..table_end].trim();
}
""")
            self.assertIsNone(scanner._last_rewrite_table_binding_error(root))
            source.write_text("""
fn rewrite_insert_conflict(sql: &str, mode: ConflictMode) {
    let rest = sql;
    let table_end = rest.len();
    let table_name = rest[..table_end].trim();
    let table_name = "auto_queue_runs";
}
""", encoding="utf-8")
            error = scanner._last_rewrite_table_binding_error(root)
            self.assertIsNotNone(error)
            self.assertIn("last direct table_name binding", error)

        records = [scanner.SurfaceRecord(
            "src", "RUST", scanner.REWRITE_PATH, "rewrite_insert_conflict",
            "rewrite_insert_conflict", "STATIC", "sha256:" + "2" * 64,
        )]
        self.assertEqual(scanner._rewrite_dynamic_records(records), [])
        contract_errors = scanner.live_contract_errors(records, root)
        self.assertTrue(any("lost rewrite_insert_conflict UNRESOLVED" in e for e in contract_errors))

    def test_live_known_blind_spots_and_auto_queue_runs_report(self):
        records = scanner.scan_inventory(scanner.REPO_ROOT)
        def matching(path, api, classification):
            return [r for r in records if (r.path, r.api, r.classification) ==
                    (path, api, classification)]

        db_ops = "src/engine/ops/db_ops.rs"
        self.assertEqual(len(matching(db_ops, "db_execute_raw_pg", "UNRESOLVED")), 1)
        self.assertEqual(len(matching("src/engine/intent.rs", "execute_policy_sql", "UNRESOLVED")), 1)
        self.assertIn("Intent::ExecuteSQL { sql, params } =>", (scanner.REPO_ROOT / "src/engine/intent.rs").read_text(encoding="utf-8"))
        self.assertTrue(matching(db_ops, "db_query_raw_with_json_mode", "UNRESOLVED"))
        source = (scanner.REPO_ROOT / db_ops).read_text(encoding="utf-8")
        self.assertRegex(source, r"let\s+table_name\s*=\s*rest\s*\[\.\.table_end\]\s*\.trim\(\);")
        self.assertEqual(len(matching(db_ops, "rewrite_insert_conflict", "UNRESOLVED")), 2)

        policy_path = "policies/lib/auto-queue-phase-gate.js"
        policy_lines = (scanner.REPO_ROOT / policy_path).read_text(encoding="utf-8").splitlines()
        for symbol in ("beginPhaseGateGraceWindow", "clearPhaseGateGraceWindow"):
            start = next(
                (i for i, line in enumerate(policy_lines) if line.startswith(f"function {symbol}(") ),
                None,
            )
            self.assertIsNotNone(start, f"missing grace writer function {symbol}")
            end = next(
                (i for i, line in enumerate(policy_lines[start + 1:], start + 1)
                 if line.startswith("function ")),
                None,
            )
            self.assertIsNotNone(end, f"missing function boundary after {symbol}")
            calls = [r for r in matching(policy_path, "agentdesk.db.execute", "STATIC")
                     if start < r.line <= end and "auto_queue_runs" in r.table_tokens]
            self.assertEqual(len(calls), 1, symbol)

        migrations = [r for r in records if r.kind == "MIGRATION"]
        self.assertTrue(any(r.path.endswith("0025_auto_queue_phase_gate_grace.sql") for r in migrations))
        report = "\n".join(scanner._auto_queue_runs_report(records, scanner.REPO_ROOT))
        for root in ("src", "policies", "migrations/postgres"):
            self.assertIn(f"root={root}", report)
        self.assertIn("UNRESOLVED dynamic_boundary=src/engine/ops/db_ops.rs rewrite_insert_conflict.table_name", report)
        self.assertIn("records=2", report)

        self.assertEqual(scanner.live_contract_errors(records, scanner.REPO_ROOT), [])
        rendered = scanner._render(records, repo_root=scanner.REPO_ROOT)
        for symbol, path, table in scanner.GUARD_EXPECTED_CONTRACTS:
            self.assertIn(f"{symbol} {path} table={table} records=1", rendered)


if __name__ == "__main__":
    unittest.main()
