import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest

from scripts.ci import aggregate_pr_scope as scope


FOCUS = ["agentdesk", "m::selected"]
OTHER_BINARY = ["agentdesk-core", "m::selected"]


def observation(**fields):
    return {
        "version": 1,
        "run_id": "12",
        "run_attempt": "1",
        "recorded_at": "2026-11-22T03:30:00Z",
        "mode": "scoped",
        "reason": "matched",
        "scope_ids": [FOCUS],
        "scope_seconds": 12.5,
        "scope_state": "passed",
        "full_state": "complete",
        "scope_failure_ids": [],
        "full_failure_ids": [],
        "scope_miss_ids": [],
        **fields,
    }


class ScopeAggregateTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)

    def write(self, directory, **fields):
        path = self.root / directory / scope.OBSERVATION_NAME
        path.parent.mkdir()
        path.write_text(json.dumps(observation(**fields)))
        return path

    def cli(self, *args):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = scope.main([str(self.root), *args])
        return code, out.getvalue(), err.getvalue()

    def test_week_uses_exact_ids_and_accounts_for_censoring(self):
        failures = dict(full_failure_ids=[OTHER_BINARY], scope_miss_ids=[OTHER_BINARY])
        self.write("miss", **failures)
        self.write("duplicate", **failures)
        self.write("early", run_attempt=2, scope_state="failed", full_state="not_run",
                   scope_failure_ids=[FOCUS])
        self.write("partial", run_id=13, full_state="incomplete", **failures)
        self.write("fallback", run_id=14, mode="full", reason="shared-input", scope_ids=[],
                   scope_seconds=None, scope_state="not_run", full_failure_ids=[FOCUS])
        self.write("skipped", run_id=15, mode="skipped", reason="change-gate", scope_ids=[],
                   scope_seconds=None, scope_state="not_run", full_state="not_run")
        self.write("week-end", run_id=16, recorded_at="2026-11-29T03:30:00Z")

        code, stdout, stderr = self.cli("--since", "2026-11-22T12:30:00+09:00",
                                       "--until", "2026-11-29T12:30:00+09:00")
        self.assertEqual(code, 0, stderr)
        result = json.loads(stdout)
        self.assertEqual(result["attempts"], 5)
        self.assertEqual(result["modes"], {"full": 1, "scoped": 3, "skipped": 1})
        self.assertEqual(result["misses_by_id_in_complete_runs"], [[*OTHER_BINARY, 1]])
        self.assertEqual(result["scoped_full_censored"], 2)
        self.assertEqual(result["scoped_indeterminate"], 1)
        self.assertEqual(result["censored_miss_observations"], 1)
        self.assertEqual(result["scope_only_false_green_rate_in_decidable_scoped_runs"]["ratio"], 0.5)
        self.assertEqual(result["miss_id_rate_among_observed_scoped_failures"]["denominator"], 3)

    def test_no_coverage_is_distinct_from_a_zero_miss_rate(self):
        code, out, err = self.cli()
        self.assertEqual((code, out), (2, ""))
        self.assertIn("no scope observations", err)
        self.write("fallback", mode="full", reason="shared-input", scope_ids=[],
                   scope_seconds=None, scope_state="not_run")
        code, out, err = self.cli()
        self.assertEqual(code, 0, err)
        self.assertIsNone(json.loads(out)["miss_run_rate_after_scope_pass"]["ratio"])

    def test_rejects_contradictory_miss_and_duplicate_attempt(self):
        path = self.write("bad-miss", full_failure_ids=[OTHER_BINARY])
        code, out, err = self.cli()
        self.assertEqual((code, out), (2, ""))
        self.assertIn("failure IDs do not match", err)
        path.unlink()
        self.write("copy1")
        self.write("copy2", scope_seconds=9)
        code, out, err = self.cli()
        self.assertEqual((code, out), (2, ""))
        self.assertIn("conflicting observations", err)
