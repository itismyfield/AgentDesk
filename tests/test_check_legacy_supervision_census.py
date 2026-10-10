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
            entries={'primitive.rs': ''}, protected={'canonical.rs': {'*': guard.digest('original')}})
        self.assertEqual(len(errors), 2)
        self.assertIn('manifest drift', errors[0])
        self.assertIn('protected boot/RETIRED body', errors[1])

    # Narrowed pins run on the real main sources: what each function pin catches and what it lets pass.
    TURN = 'src/services/discord/tui_direct_pending_start/turn_retirement.rs'
    LEGACY = 'src/services/discord/health/legacy_supervision.rs'

    def protected_audit(self, path, edit=lambda code: code):
        code = edit(guard.classifier._production_text(Path(path)))
        return guard.audit({path: code}, entries={}, protected={path: guard.PROTECTED[path]})

    def test_main_sources_match_every_pin(self):
        for path in guard.PROTECTED:
            with self.subTest(path=path):
                self.assertEqual(self.protected_audit(path), [])

    def test_call_added_to_protected_function_fails_with_repin_digest(self):
        errors = self.protected_audit(self.TURN, lambda code: code.replace(
            'confirm_turn_channels(provider, config.map', 'drop(0);\n    confirm_turn_channels(provider, config.map', 1))
        self.assertEqual(len(errors), 1)
        self.assertIn(self.TURN + '::confirm_at_boot: protected boot/RETIRED body changed', errors[0])
        self.assertRegex(errors[0], r"re-pin 'confirm_at_boot': '[0-9a-f]{64}'")

    def test_edit_outside_protected_functions_passes(self):
        edited = lambda code: code.replace('let mut boundaries = Vec::new();',
                                           'let mut boundaries = Vec::with_capacity(4);', 1)
        self.assertNotEqual(edited(guard.classifier._production_text(Path(self.TURN))),
                            guard.classifier._production_text(Path(self.TURN)))
        self.assertEqual(self.protected_audit(self.TURN, edited), [])

    def test_protected_function_renamed_or_deleted_fails(self):
        for edit in (lambda code: code.replace('fn confirm_at_boot(', 'fn confirm_on_boot(', 1),
                     lambda code: code[:code.index('pub(in crate::services::discord) fn confirm_at_boot(')]):
            with self.subTest(edit=edit):
                errors = self.protected_audit(self.TURN, edit)
                self.assertEqual(len(errors), 1)
                self.assertIn('::confirm_at_boot: protected function extraction failed (0 definitions)', errors[0])

    def test_extraction_failure_fails_closed(self):
        pins = {'f.rs': {'start': guard.digest('fn start() {}')}}
        for source, reason in (('fn start() {} fn start() {}', '2 definitions'),
                               ('fn start() { if x {', 'unbalanced body'),
                               ('fn start();', 'no body')):
            with self.subTest(source=source):
                errors = guard.audit({'f.rs': source}, entries={}, protected=pins)
                self.assertEqual(errors, [f'f.rs::start: protected function extraction failed ({reason})'])
        errors = guard.audit({}, entries={}, protected=pins)
        self.assertIn('f.rs: protected file missing or unpinned', errors)
        self.assertEqual(guard.audit({'f.rs': ''}, entries={}, protected={'f.rs': {}}),
                         ['f.rs: protected file missing or unpinned'])

    def test_braces_in_strings_and_comments_do_not_end_a_body(self):
        source = 'fn start(a: [u8; 2]) { let s = "}"; let c = \'}\'; // }\n /* } */ run(); }\nfn later() {}'
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / 'fixture.rs'
            path.write_text(source)
            code = guard.classifier._production_text(path)
        self.assertTrue(guard.function_text(code, 'start').endswith('run(); }'))

    def test_new_pub_fn_in_legacy_supervision_fails(self):
        errors = self.protected_audit(self.LEGACY, lambda code: code + (
            '\npub(in crate::services::discord) fn mark(p: &str, c: u64) '
            '{ RETIRED.get_or_init(Default::default); }\n'))
        self.assertEqual(len(errors), 1)
        self.assertIn(self.LEGACY + '::*: protected boot/RETIRED body changed', errors[0])

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
