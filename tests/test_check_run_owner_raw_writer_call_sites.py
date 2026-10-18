"""Tests for the run-owner raw writer scanner and the create path's lock-order call order."""

from __future__ import annotations

import importlib.util
import re
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/check_run_owner_raw_writer_call_sites.py"
SPEC = importlib.util.spec_from_file_location("run_owner_raw_writer_guard", SCRIPT)
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)

PROD = "src/db/fixture_writer.rs"


def write(root: Path, rel: str, body: str) -> None:
    path = root / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")


def rust_sql(sql: str) -> str:
    return f'pub async fn write() {{\n    sqlx::query(\n        r#"{sql}"#,\n    );\n}}\n'


class FixtureTree(unittest.TestCase):
    def tree(self, files: dict[str, str]) -> Path:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        write(root, "src/lib.rs", "pub mod db;\n")
        for rel, body in files.items():
            write(root, rel, body)
        return root

    def hits(self, files: dict[str, str], pinned: frozenset[str] = frozenset()) -> dict:
        return guard.scan(self.tree(files), pinned_test_only_files=pinned)


class RealTreeTests(unittest.TestCase):
    def test_real_tree_matches_the_pinned_exceptions_exactly(self):
        """T9a: every raw owner write outside the guard writes is a pinned site."""
        hits = guard.scan(ROOT)
        self.assertEqual(guard.violations(hits), [])
        self.assertEqual(
            {key: len(lines) for key, lines in hits.items()},
            {key: count for key, (count, _) in guard.EXPECTED_EXCEPTIONS.items()},
        )

    def test_create_takes_the_context_run_token_before_stale_review_rows(self):
        """T9c: the context guard precedes the stale review-decision cancel in the create tx.

        A PG test cannot cheaply provoke the row-then-token inversion this order prevents.
        """
        text = (ROOT / "src/dispatch/dispatch_create.rs").read_text(encoding="utf-8")
        mask, _ = guard.lex_rust(text)
        start = mask.index("fn apply_dispatch_attached_intents_pg(")
        brace = mask.index("{", mask.index(") -> Result<()>", start))
        depth, end = 0, brace
        for end in range(brace, len(mask)):
            depth += {"{": 1, "}": -1}.get(mask[end], 0)
            if depth == 0:
                break
        body = mask[brace:end]
        guard_call = re.search(r"\bguard_context_owner_publication_on_pg_tx\s*\(", body)
        stale_cancel = re.search(r"\bcancel_stale_review_decisions_pg_tx\s*\(", body)
        self.assertIsNotNone(guard_call, "create tx no longer takes the context run token")
        self.assertIsNotNone(stale_cancel, "stale review-decision cancel moved out of the create tx")
        self.assertLess(guard_call.start(), stale_cancel.start())


class DetectionTests(FixtureTree):
    def test_new_production_dispatch_insert_is_red(self):
        """T9b: a raw dispatch INSERT outside the guard writes fails the gate."""
        hits = self.hits({PROD: rust_sql("INSERT INTO task_dispatches (id) VALUES ($1)")})
        self.assertEqual(hits, {(PROD, guard.INSERT_DISPATCH): [3]})
        self.assertTrue(guard.violations(hits, {}))

    def test_guard_writes_module_is_unrestricted(self):
        hits = self.hits({guard.GUARD_WRITES: rust_sql("INSERT INTO task_dispatches (id) VALUES ($1)")})
        self.assertEqual(hits, {})

    def test_external_cfg_test_module_is_skipped_without_changing_exceptions(self):
        """T9d: a writer in a file mounted only by `#[cfg(test)] mod fixture;` is not production."""
        files = {
            "src/db.rs": "pub mod fixture_writer;\n#[cfg(test)]\nmod fixture;\n",
            PROD: "pub fn noop() {}\n",
            "src/db/fixture.rs": rust_sql("INSERT INTO task_dispatches (id) VALUES ($1)"),
        }
        hits = self.hits(files, pinned=frozenset({"src/db/fixture.rs"}))
        self.assertEqual(guard.violations(hits, {}), [])

    def test_exception_count_mismatch_is_red(self):
        """T9e: one more entry link than pinned fails even inside a pinned file."""
        sql = "UPDATE auto_queue_entries SET dispatch_id = $1 WHERE id = $2"
        hits = self.hits({PROD: rust_sql(sql) + rust_sql(sql).replace("write", "write_again")})
        expected = {(PROD, guard.UPDATE_ENTRY_DISPATCH): (1, "fixture")}
        self.assertEqual(len(hits[(PROD, guard.UPDATE_ENTRY_DISPATCH)]), 2)
        self.assertTrue(guard.violations(hits, expected))
        self.assertEqual(guard.violations(hits, {(PROD, guard.UPDATE_ENTRY_DISPATCH): (2, "f")}), [])

    def test_policy_phase_gate_owner_write_is_red(self):
        """T9f: a policy script setting a phase-gate dispatch owner fails the gate."""
        hits = self.hits(
            {
                PROD: "pub fn noop() {}\n",
                "policies/x.js": 'agentdesk.db.execute("UPDATE auto_queue_phase_gates SET dispatch_id = ?");\n',
                "policies/__tests__/x.test.js": '"INSERT INTO task_dispatches (id) VALUES (?)"\n',
            }
        )
        self.assertEqual(hits, {("policies/x.js", guard.UPDATE_GATE_OWNER): [0]})


class SetClauseTests(FixtureTree):
    def owner_writes(self, sql: str) -> list[str]:
        return guard.owner_writes(sql)

    def test_terminal_status_writers_with_status_cas_pass(self):
        for sql in (
            "UPDATE task_dispatches SET status = 'completed', result = $1 WHERE id = $2 AND status = $3",
            "UPDATE task_dispatches SET status = 'failed', updated_at = NOW() WHERE id = $1",
            "UPDATE task_dispatches td SET status = 'cancelled' WHERE td.id = $2 AND td.status = $3"
            " AND EXISTS (SELECT 1 FROM auto_queue_entries e WHERE e.dispatch_id = td.id)",
            "UPDATE auto_queue_phase_gates SET status = 'failed' WHERE run_id = $1 AND dispatch_id = $2",
            "UPDATE auto_queue_entries SET status = 'done' WHERE dispatch_id = $1",
        ):
            with self.subTest(sql=sql):
                self.assertEqual(self.owner_writes(sql), [])

    def test_live_or_parameter_status_writes_are_red(self):
        for sql in (
            "UPDATE task_dispatches SET status = $1, updated_at = NOW() WHERE id = $2",
            "UPDATE task_dispatches SET status = 'pending' WHERE id = $1 AND status = 'failed'",
            "UPDATE task_dispatches SET context = $1, status='dispatched' WHERE id = $2",
            "UPDATE task_dispatches SET result = (SELECT x FROM y WHERE z = 1), status = $2 WHERE id = $3",
        ):
            with self.subTest(sql=sql):
                self.assertEqual(self.owner_writes(sql), [guard.UPDATE_DISPATCH_LIVE])
                hits = self.hits({PROD: rust_sql(sql)})
                self.assertTrue(guard.violations(hits, {}))

    def test_owner_column_set_on_gate_and_entry_is_red(self):
        self.assertEqual(
            self.owner_writes("UPDATE auto_queue_phase_gates SET run_id = $1 WHERE id = $2"),
            [guard.UPDATE_GATE_OWNER],
        )
        self.assertEqual(
            self.owner_writes("UPDATE auto_queue_entries SET status = 'dispatched', dispatch_id = $1"),
            [guard.UPDATE_ENTRY_DISPATCH],
        )


class OffsetTests(FixtureTree):
    def test_insert_after_long_comment_and_inline_test_module_is_still_seen(self):
        """Literal offsets stay aligned with the test-block mask past comments and test code."""
        body = (
            "// " + "x" * 400 + " \"quoted\" // nested\n"
            "/* block { comment */\n"
            "#[cfg(test)]\nmod tests {\n"
            "    fn t() { let _ = \"INSERT INTO task_dispatches\"; let _ = '{'; }\n"
            "}\n"
            "// trailing comment with a brace {\n"
            + rust_sql("INSERT INTO task_dispatches (id) VALUES ($1)")
        )
        hits = self.hits({PROD: body})
        self.assertEqual(hits, {(PROD, guard.INSERT_DISPATCH): [10]})


if __name__ == "__main__":
    unittest.main()
