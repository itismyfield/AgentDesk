import tempfile
import unittest
from pathlib import Path
from scripts import check_legacy_supervision_census as guard


class DormantCensusTests(unittest.TestCase):
    def inspect(self, source, name="src/consumer.rs"):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "fixture.rs"
            path.write_text(source)
            code = guard.classifier._production_text(path)
            return guard.audit({name: code}, entries={}, protected={})

    def test_external_install_and_function_value_fail_with_owner(self):
        errors = self.inspect("fn start() { let f = BootCohort::install_process; }")
        self.assertEqual(len(errors), 2)
        self.assertTrue(all("src/consumer.rs:1:start:" in error for error in errors))

    def test_comment_strings_and_raw_strings_are_not_callers(self):
        source = '\n'.join(['// BootCohort::install_process', '/* nested /* BootPublication */ */',
            'fn prose() { let text = "BootCohort::install_process";',
            'let raw = r###"{ #[cfg(test)] BootPublication"###; }'])
        self.assertEqual(self.inspect(source), [])

    def test_prose_cannot_hide_real_caller_afterward(self):
        source = 'fn start() { let raw = r###"{ #[cfg(test)]"###; BootCohort::install_process(); }'
        self.assertEqual(len(self.inspect(source)), 2)

    def test_test_name_and_test_attribute_do_not_hide_production(self):
        for source in ('fn tests() { BootCohort::install_process(); }',
                       '#[test] fn tests() { BootCohort::install_process(); }'):
            with self.subTest(source=source):
                self.assertEqual(len(self.inspect(source)), 2)

    def test_test_basename_cannot_hide_an_unregistered_production_caller(self):
        self.assertEqual(len(self.inspect('fn start() { BootCohort::install_process(); }', 'src/fake_tests.rs')), 2)

    def test_cfg_test_and_all_test_are_excluded(self):
        for attr in ('#[cfg(test)]', '#[cfg(all(test, unix))]'):
            with self.subTest(attr=attr):
                self.assertEqual(self.inspect(attr + ' fn fixture() { BootCohort::install_process(); }'), [])

    def test_cfg_any_and_not_test_are_production(self):
        for attr in ('#[cfg(any(test, feature = "live"))]', '#[cfg(not(test))]'):
            with self.subTest(attr=attr):
                self.assertEqual(len(self.inspect(attr + ' fn start() { BootCohort::install_process(); }')), 2)

    def test_alias_reexport_and_macro_fail_closed(self):
        for source in ('use crate::services::discord::boot_retirement as hidden;',
                       'pub use hidden::BootCohort as Other;',
                       'macro_rules! hidden { () => { BootCohort::install_process() } }'):
            with self.subTest(source=source):
                self.assertTrue(self.inspect(source))

    def test_registered_test_module_is_excluded(self):
        self.assertEqual(guard.audit({'src/registered.rs': 'BootCohort'}, entries={}, protected={}, skips={'src/registered.rs'}), [])

    def test_canonical_and_other_retired_symbols_are_distinct(self):
        self.assertEqual(self.inspect('fn unrelated() { RETIRED.insert(key); }'), [])
        errors = self.inspect('fn writer() { legacy_supervision::RETIRED.get_or_init(make); }')
        self.assertEqual(len(errors), 1)
        self.assertIn(':writer: external RETIRED', errors[0])

    def test_protected_body_and_helper_manifest_drift_fail(self):
        errors = guard.audit({'primitive.rs': 'fn extra() {}', 'canonical.rs': 'changed'},
            entries={'primitive.rs': ''}, protected={'canonical.rs': guard.digest('original')})
        self.assertEqual(len(errors), 2)
        self.assertIn('manifest drift', errors[0])
        self.assertIn('protected boot/RETIRED body', errors[1])

    def test_new_primitive_macro_or_hidden_file_fails(self):
        entries = {guard.R + '.rs': ''}
        errors = guard.audit({guard.R + '.rs': 'macro_rules! spawn {}'}, entries=entries, protected={})
        self.assertTrue(any('activation syntax' in error for error in errors))
        errors = guard.audit({guard.R + '/hidden.rs': 'fn new() {}'}, entries={}, protected={})
        self.assertTrue(any('unclassified primitive file' in error for error in errors))

    def test_ci_wires_both_gate_and_self_tests(self):
        code = Path('scripts/ci-script-checks.sh').read_text()
        self.assertIn('scripts/check_legacy_supervision_census.py', code)
        self.assertIn('tests.test_check_legacy_supervision_census', code)
