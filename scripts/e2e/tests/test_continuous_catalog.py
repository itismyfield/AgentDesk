"""Exercise catalog generation through its CLI without changing the repo manifest."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

GENERATOR = Path(__file__).resolve().parents[1] / 'generate_continuous_catalog.py'


class ContinuousCatalogTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='continuous-catalog-')
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.scenarios = self.root / 'yaml'
        self.scenarios.mkdir()
        self.manifest = self.root / 'plan.json'
        self.original = dict(scenario_catalog={}, witness={'missing': 'UNEVALUABLE'},
                             bundles={'P': {'enabled': False, 'planned_work_prompts': 13}},
                             control_occurrences={'R-inject-managed': {'busy_basis': 'managed'}})
        self.manifest.write_text(json.dumps(self.original))

    def cli(self, *args):
        return subprocess.run([sys.executable, str(GENERATOR), '--root', str(self.root),
                               '--scenario-dir', 'yaml', '--manifest', 'plan.json', *args],
                              capture_output=True, text=True, timeout=10)

    def scenario(self, filename='one.yaml', identifier='E-1'):
        path = self.scenarios / filename
        path.write_text(f'id: {identifier}\ncoverage_class: live\ncells: [claude-tui]\n')
        return path

    def test_check_detects_drift_without_writing_then_regenerates_only_catalog(self):
        source = self.scenario()
        before = self.manifest.read_bytes()
        checked = self.cli('--check')
        self.assertEqual(checked.returncode, 1, checked.stdout + checked.stderr)
        self.assertIn('Regenerate: python3 scripts/e2e/generate_continuous_catalog.py --write', checked.stderr)
        self.assertEqual(self.manifest.read_bytes(), before)
        generated = self.cli('--write')
        self.assertEqual(generated.returncode, 0, generated.stderr)
        plan = json.loads(self.manifest.read_text())
        self.assertEqual(plan.pop('scenario_catalog'), {'E-1': dict(
            path='yaml/one.yaml', sha256=hashlib.sha256(source.read_bytes()).hexdigest(),
            coverage_class='live', cells=['claude-tui'])})
        self.assertEqual(plan, {k: v for k, v in self.original.items() if k != 'scenario_catalog'})
        stable = self.manifest.read_bytes()
        self.assertEqual(self.cli('--check').returncode, 0)
        self.assertEqual(self.cli('--write').returncode, 0)
        self.assertEqual(self.manifest.read_bytes(), stable)

    def test_yaml_content_drift_updates_sha_without_changing_contracts(self):
        source = self.scenario()
        self.assertEqual(self.cli('--write').returncode, 0)
        plan = json.loads(self.manifest.read_text())
        source.write_text(source.read_text() + '# new metadata\n')
        self.assertEqual(self.cli('--check').returncode, 1)
        self.assertEqual(self.cli('--write').returncode, 0)
        changed = json.loads(self.manifest.read_text())
        self.assertNotEqual(changed['scenario_catalog']['E-1']['sha256'], plan['scenario_catalog']['E-1']['sha256'])
        changed['scenario_catalog'] = plan['scenario_catalog']
        self.assertEqual(changed, plan)

    def test_empty_inventory_and_duplicate_ids_fail_without_writing(self):
        before = self.manifest.read_bytes()
        empty = self.cli('--write')
        self.assertEqual(empty.returncode, 2)
        self.assertIn('empty scenario inventory', empty.stderr)
        self.scenario('one.yaml')
        self.scenario('two.yaml')
        duplicate = self.cli('--write')
        self.assertEqual(duplicate.returncode, 2)
        self.assertIn('duplicate scenario id E-1', duplicate.stderr)
        self.assertEqual(self.manifest.read_bytes(), before)

    def test_catalog_order_is_sorted_and_retired_subdirectories_are_excluded(self):
        self.scenario('z.yaml', 'E-2')
        self.scenario('a.yaml', 'E-1')
        (self.scenarios / 'retired').mkdir()
        (self.scenarios / 'retired' / 'old.yaml').write_text('id: E-retired\n')
        result = self.cli('--write')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(list(json.loads(self.manifest.read_text())['scenario_catalog']), ['E-1', 'E-2'])


if __name__ == '__main__':
    unittest.main()
