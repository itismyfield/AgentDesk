"""Offline contract checks for the unexecuted continuous E2E plan."""
import copy
import hashlib
import json
from pathlib import Path
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / 'docs/e2e-continuous/bundles.schema1.json'


def validate(plan):
    def require(condition, reason):
        if not condition:
            raise ValueError(reason)

    require(plan['schema_version'] == 1, 'unknown manifest schema')
    require(plan['kind'] == 'continuous_e2e_planning_manifest', 'not a planning manifest')
    require(plan['evidence_status'] == 'NOT_RUN', 'plan is not execution evidence')
    require(plan['activation']['enabled'] is False, 'measurement required before activation')
    require(plan['activation']['deployment_hold'] is False, 'deployment policy is alerts only')
    for key, value in plan['activation'].items():
        if key not in ('enabled', 'deployment_hold'):
            require(value is None, 'unmeasured budget or period must remain unknown')
    for bundle in plan['bundles'].values():
        require(bundle['enabled'] is False, 'unmeasured bundle enabled')
        require(bundle['measured'] == dict(runs=0, elapsed_s=None, native_turns=None, tokens=None),
                'zero samples cannot imply zero cost or a measured run')
    catalog = plan['scenario_catalog']
    sources = {}
    for path in (ROOT / 'tests/e2e/tui_relay/scenarios').glob('*.yaml'):
        scenario = yaml.safe_load(path.read_text())
        sources[scenario['id']] = (path, scenario)
    require(set(catalog) == set(sources), 'catalog differs from active YAML inventory')
    for key, (path, scenario) in sources.items():
        entry = catalog[key]
        require(entry['path'] == str(path.relative_to(ROOT)), 'wrong scenario path')
        require(entry['sha256'] == hashlib.sha256(path.read_bytes()).hexdigest(), 'YAML drift')
        require(entry['cells'] == scenario['cells'], 'unsupported cell declared')
        require(entry['coverage_class'] == scenario['coverage_class'], 'coverage reclassified')
    full = plan['bundles']['F']
    ids = list(full['additional_scenarios'])
    for name in full['includes']:
        ids.extend(plan['bundles'][name]['scenarios'])
    live = {key for key, value in catalog.items() if value['coverage_class'] == 'live'}
    require(len(ids) == len(set(ids)) and set(ids) == live, 'full bundle loses or repeats live scenarios')
    require(full['expected_existing_live_scenarios'] == len(live), 'stale full count')
    require(set(full['new_scenarios']) == set(plan['new_scenarios']) ==
            {'R-marker', 'R-inject', 'R-fresh', 'R-host'}, 'control merged into native regressions')
    core = plan['bundles']['P']['occurrences']
    require(len(core) == 6 and len({entry['scenario'] for entry in core}) == 6, 'core is not six distinct occurrences')
    expected_smoke = {
        'E-50': ('claude', 'herdr', 'claude-herdr'),
        'E-51': ('codex', 'tmux', 'codex-tui'),
    }
    smoke = {entry['scenario']: (entry['provider'], entry['host'], entry['cell'])
             for entry in core if not entry['scenario'].startswith('R-')}
    require(smoke == expected_smoke, 'wrong core smoke scenario or host')
    for entry in core:
        if entry['scenario'].startswith('R-'):
            require({key: value for key, value in entry.items() if key != 'scenario'} ==
                    plan['new_scenarios'][entry['scenario']], 'core qualifier drift')
    require(plan['bundles']['P']['planned_work_prompts'] == 13 and
            plan['bundles']['P']['planned_native_turns_if_busy_joins'] == 10, 'core cost plan drift')
    controls = plan.get('control_occurrences', {})
    require(isinstance(controls, dict) and set(controls) == {'R-inject-managed'}, 'missing managed control')
    control = controls['R-inject-managed']
    for key, expected in dict(provider='claude', host='herdr', surface='channel', state='busy',
                             busy_basis='managed', origin='controlled_ingress',
                             coverage_scenario='R-inject-managed').items():
        require(control.get(key) == expected, 'managed control merged with native result')
    require({'inject_before_turn_end', 'injection_latency', 'exactly_once'} <= set(control['operations']),
            'managed control lacks assertions')
    require({'managed_active_claim_or_inflight', 'human_branch_entered'} <= set(control['preconditions']),
            'managed control lacks preconditions')
    for name, bundle in plan['bundles'].items():
        expected = ['R-inject-managed'] if name in ('R2', 'F') else []
        require(bundle.get('control_occurrences', []) == expected, 'managed control assignment drift')
    require(full.get('control_selection') == 'union_included_controls_once', 'full repeats rotation control')
    contract = plan['execution_contract']
    require(contract.get('missing_precondition_witness') == 'UNEVALUABLE', 'missing witness must stay unevaluable')
    require({'scenario', 'busy_basis', 'origin', 'operation', 'occurrence'} <=
            set(contract['occurrence_key_fields']), 'result key merges distinct cohorts')
    require(contract['controlled_ingress_counts_as_human_gateway'] is False, 'ingress is not human Gateway evidence')
    require(contract['human_acceptance']['periodic_coverage'] == 0, 'one-time acceptance is not periodic coverage')
    require(contract['missing_usage'] is None and contract['zero_execution'] == 'NO_DATA', 'missing evidence became success')
    inject = plan['new_scenarios']['R-inject']
    require(inject['busy_basis'] == 'native' and inject['origin'] == 'controlled_ingress', 'wrong original-cause cohort')
    require({'native_busy_source', 'no_managed_claim_or_inflight_before_B',
             'no_durable_external_holder_before_B', 'pre_submit_busy_branch_witness'} <= set(inject['preconditions']),
            'original-cause branch lacks witnesses')


class ContinuousManifestTests(unittest.TestCase):
    def setUp(self):
        self.plan = json.loads(MANIFEST.read_text())

    def test_manifest_matches_live_yaml_inventory(self):
        validate(self.plan)

    def test_managed_control_has_separate_full_and_rotation_assignment(self):
        control = self.plan['control_occurrences']['R-inject-managed']
        self.assertEqual(control['busy_basis'], 'managed')
        self.assertEqual(control['coverage_scenario'], 'R-inject-managed')
        self.assertEqual(self.plan['bundles']['R2']['control_occurrences'], ['R-inject-managed'])
        self.assertEqual(self.plan['bundles']['F']['control_occurrences'], ['R-inject-managed'])
        self.assertEqual(self.plan['bundles']['P'].get('control_occurrences', []), [])
        self.assertEqual(self.plan['bundles']['P']['planned_work_prompts'], 13)
        self.assertEqual(self.plan['bundles']['P']['planned_native_turns_if_busy_joins'], 10)

    def test_mutants_reject_missing_managed_control_and_native_accounting(self):
        control = dict(provider='claude', host='herdr', surface='channel', state='busy',
                       busy_basis='managed', origin='controlled_ingress',
                       coverage_scenario='R-inject-managed',
                       operations=['inject_before_turn_end', 'injection_latency', 'exactly_once'],
                       preconditions=['managed_active_claim_or_inflight', 'human_branch_entered'])
        for change in ('missing_control', 'missing_rotation', 'missing_full', 'native_accounting',
                       'native_basis', 'native_regression', 'duplicate_full', 'missing_busy_discriminator'):
            with self.subTest(change=change):
                plan = copy.deepcopy(self.plan)
                plan['control_occurrences'] = {'R-inject-managed': copy.deepcopy(control)}
                for name, bundle in plan['bundles'].items():
                    bundle['control_occurrences'] = ['R-inject-managed'] if name in ('R2', 'F') else []
                plan['bundles']['F']['control_selection'] = 'union_included_controls_once'
                if change == 'missing_control':
                    plan['control_occurrences'].clear()
                elif change == 'missing_rotation':
                    plan['bundles']['R2']['control_occurrences'].clear()
                elif change == 'missing_full':
                    plan['bundles']['F']['control_occurrences'].clear()
                elif change == 'native_accounting':
                    plan['control_occurrences']['R-inject-managed']['coverage_scenario'] = 'R-inject'
                elif change == 'native_basis':
                    plan['control_occurrences']['R-inject-managed']['busy_basis'] = 'native'
                elif change == 'native_regression':
                    plan['new_scenarios']['R-inject-managed'] = control
                    plan['bundles']['F']['new_scenarios'].append('R-inject-managed')
                elif change == 'duplicate_full':
                    plan['bundles']['F']['control_occurrences'].append('R-inject-managed')
                else:
                    plan['execution_contract']['occurrence_key_fields'].remove('busy_basis')
                with self.assertRaises(ValueError):
                    validate(plan)

    def test_mutant_rejects_missing_witness_pass_contract(self):
        plan = copy.deepcopy(self.plan)
        plan['execution_contract']['missing_precondition_witness'] = 'PASS'
        with self.assertRaisesRegex(ValueError, 'missing witness'):
            validate(plan)

    def test_mutants_reject_omission_duplicate_and_fixture_in_live(self):
        for bad_ids in ([], ['E-50', 'E-51', 'E-11', 'E-13', 'E-36', 'E-1'],
                        ['E-50', 'E-51', 'E-11', 'E-13', 'E-24']):
            plan = copy.deepcopy(self.plan)
            plan['bundles']['F']['additional_scenarios'] = bad_ids
            with self.assertRaises(ValueError):
                validate(plan)

    def test_mutants_reject_deploy_hold_and_fixture_core(self):
        plan = copy.deepcopy(self.plan)
        plan['activation']['deployment_hold'] = True
        with self.assertRaises(ValueError):
            validate(plan)
        plan = copy.deepcopy(self.plan)
        plan['bundles']['P']['occurrences'][0].update(
            scenario='E-24', provider='claude', host='pipe', cell='claude-pipe')
        with self.assertRaises(ValueError):
            validate(plan)

    def test_mutants_reject_false_pass_zero_cost_and_human_coverage(self):
        for change in ('pass', 'zero_cost', 'human', 'missing_witness'):
            plan = copy.deepcopy(self.plan)
            if change == 'pass':
                plan['evidence_status'] = 'PASS'
            elif change == 'zero_cost':
                plan['bundles']['P']['measured']['native_turns'] = 0
            elif change == 'human':
                plan['execution_contract']['controlled_ingress_counts_as_human_gateway'] = True
            else:
                plan['new_scenarios']['R-inject']['preconditions'].remove('pre_submit_busy_branch_witness')
                next(entry for entry in plan['bundles']['P']['occurrences'] if entry['scenario'] == 'R-inject')['preconditions'].remove('pre_submit_busy_branch_witness')
            with self.assertRaises(ValueError):
                validate(plan)


if __name__ == '__main__':
    unittest.main()
