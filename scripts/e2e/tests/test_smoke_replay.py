"""Evidence-boundary tests for the historical deploy-log replay."""
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location('smoke_replay', Path(__file__).parents[1]/'smoke_replay.py')
replay = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(replay)


class SmokeReplayTests(unittest.TestCase):
    def parse(self, lines, stamp='20261105T150000Z-10'):
        return replay.parse(f'evidence: post-deploy-smoke-{stamp}.log\n'+lines, 'mac-book', Path('/snapshot.log'), 'snapshot-sha')

    def test_kst_midnight_boundary_is_inclusive(self):
        summary = 'turn scenarios: E-50=skipped E-51=skipped'
        self.assertEqual(self.parse(summary, '20261105T145959Z-10'), [])
        rows = self.parse(summary)
        self.assertEqual(rows[0]['smoke_started_kst'], '2026-11-06T00:00:00+09:00')

    def test_summary_does_not_duplicate_result(self):
        rows = self.parse('relay E-51 cell=codex-tui result=pass\nturn scenarios: E-50=skipped E-51=pass')
        self.assertEqual(len(rows), 2)
        self.assertEqual(sum(r['result'] == 'PASS' for r in rows), 1)

    def test_duplicate_results_fail_closed(self):
        with self.assertRaisesRegex(ValueError, 'duplicate result'):
            self.parse('relay E-51 cell=codex-tui result=pass\nrelay E-51 cell=codex-tui result=pass')

    def test_ambiguous_and_missing_run_stamps_do_not_count(self):
        self.assertEqual(replay.parse('turn scenarios: E-50=pass E-51=pass', 'mac-book', Path('/a'), 'sha'), [])
        with self.assertRaisesRegex(ValueError, 'ambiguous smoke run'):
            self.parse('evidence: post-deploy-smoke-20261106T010000Z-11.log')

    def test_skip_and_reset_are_zero_scenario_execution(self):
        rows = self.parse('FAIL: relay E-51 cell=codex-tui result=FAIL cell-reset unavailable HTTP 409\nturn scenarios: E-50=skipped E-51=FAIL')
        self.assertFalse(rows[0]['executed'])
        self.assertFalse(rows[1]['executed'])
        self.assertTrue(rows[1]['attempted'])
        self.assertEqual(rows[1]['classification'], '불명')

    def test_missing_cost_and_actual_host_stay_unknown(self):
        row = self.parse('relay E-51 cell=codex-tui result=pass path=herdr')[1]
        for key in ('native_turns', 'usage', 'elapsed_s', 'actual_runtime_host', 'executed'):
            self.assertIsNone(row[key])
        self.assertEqual(row['reported_path'], 'herdr')

    def test_only_direct_http503_is_external(self):
        rows = self.parse('relay E-50 cell=claude-tui result=FAIL reason=timeout\nrelay E-51 cell=codex-tui result=FAIL fetch_messages HTTP 503')
        self.assertEqual([r['classification'] for r in rows], ['불명', '외부 503'])

    def test_report_identity_and_negative_elapsed_are_rejected(self):
        row = self.parse('relay E-51 cell=codex-tui result=pass')[1]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            folder = root/'reports/mac-book'
            folder.mkdir(parents=True)
            report = folder/f"post-deploy-smoke-turns-{row['run_id']}--report.codex-tui.json"
            data = dict(schema_version=2, run_id=f"{row['run_id']}-E-51-codex-tui-extra", cell='codex-tui', scenarios=[dict(id='E-51')])
            report.write_text(json.dumps(data))
            with self.assertRaisesRegex(ValueError, 'run/cell identity'):
                replay.augment(row, root)
            data['schema_version'] = 1
            report.write_text(json.dumps(data))
            with self.assertRaisesRegex(ValueError, 'unsupported report schema'):
                replay.augment(row, root)
            data['schema_version'] = 2
            data['run_id'] = f"{row['run_id']}-E-51-codex-tui"
            data['scenarios'][0].update(started_at='2026-11-06T12:01:00', completed_at='2026-11-06T12:00:59')
            report.write_text(json.dumps(data))
            with self.assertRaisesRegex(ValueError, 'negative report elapsed'):
                replay.augment(row, root)


if __name__ == '__main__':
    unittest.main()
