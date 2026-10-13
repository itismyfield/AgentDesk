import tempfile
import unittest
import re
import subprocess
import sys
from unittest.mock import patch
from pathlib import Path
from scripts import check_legacy_supervision_census as guard


class SkipPromotionTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        (self.root / 'src').mkdir()
        (self.root / 'scripts').mkdir()
        (self.root / 'src/lib.rs').write_text(
            'mod promoted;\n#[cfg(test)] mod still_test;\n')
        for name in ('promoted.rs', 'still_test.rs', 'fixture_tests.rs'):
            (self.root / 'src' / name).write_text('fn fixture() {}\n')
        self.path = self.root / 'scripts/test_only_module_skip_pin.py'
        self.original = (
            'PINNED_BASENAME_TEST_FILES = frozenset(\n    {\n'
            '        "src/fixture_tests.rs",\n    }\n)\n'
            'PINNED_RESOLVER_TEST_ONLY_FILES = frozenset(\n    {\n'
            '        "src/promoted.rs",\n        "src/still_test.rs",\n    }\n)\n')
        self.path.write_text(self.original)
        pinned = frozenset(('src/fixture_tests.rs', 'src/promoted.rs', 'src/still_test.rs'))
        patched = patch.object(guard.classifier._SKIP_PIN, 'PINNED_TEST_ONLY_MODULE_FILES', pinned)
        patched.start()
        self.addCleanup(patched.stop)

    def outcome(self, promoted=()):
        try:
            guard.sync_skips(self.root, promoted)
        except RuntimeError as error:
            return str(error)
        return None

    def test_explicit_promotion_removes_only_its_pin(self):
        self.assertIsNone(self.outcome(('src/promoted.rs',)))
        self.assertNotIn('"src/promoted.rs"', self.path.read_text())
        self.assertIn('"src/still_test.rs"', self.path.read_text())
        self.assertIn('"src/fixture_tests.rs"', self.path.read_text())

    def test_omitted_promotion_refuses_removal_without_writing(self):
        self.assertEqual(self.outcome(), 'refusing pin removal')
        self.assertEqual(self.path.read_text(), self.original)

    def test_missing_promotion_path_refuses_without_writing(self):
        original = self.original.replace('"src/promoted.rs",',
                                         '"src/promoted.rs",\n        "src/missing.rs",')
        self.path.write_text(original)
        pin = guard.classifier._SKIP_PIN
        with patch.object(pin, 'PINNED_TEST_ONLY_MODULE_FILES',
                          pin.PINNED_TEST_ONLY_MODULE_FILES | {'src/missing.rs'}):
            self.assertEqual(self.outcome(('src/promoted.rs', 'src/missing.rs')),
                             'promotion path missing: src/missing.rs')
        self.assertEqual(self.path.read_text(), original)

    def test_still_test_only_promotion_refuses_without_writing(self):
        self.assertEqual(self.outcome(('src/still_test.rs',)),
                         'promotion remains test-only: src/still_test.rs')
        self.assertEqual(self.path.read_text(), self.original)

    def test_addition_stays_next_to_its_directory_without_removing_existing_pins(self):
        (self.root / 'src/new_tests.rs').write_text('fn fixture() {}\n')
        self.assertIsNone(self.outcome(('src/promoted.rs',)))
        source = self.path.read_text()
        self.assertLess(source.index('"src/new_tests.rs"'), source.index('"src/fixture_tests.rs"'))
        self.assertIn('"src/still_test.rs"', source)


class N4DInstallerDormancyTests(unittest.TestCase):
    INSTALLER = 'src/services/discord/watchers/lifecycle/o_only_install.rs'
    SYMBOL = re.compile(r'\b(?:install_retired_o_watcher|claim_retired_o_watcher_for_host|o_only_install|OOnlyInstall(?:Request|Outcome|Reason))\b')

    def errors(self, raw, code, name):
        errors = [f'{name}: production installer symbol {m.group()}' for m in self.SYMBOL.finditer(code)]
        lines = code.splitlines()
        for match in re.finditer(r'\binclude\s*!\s*\([^;]*?\)|#\[[^\]]*\]', raw, re.S):
            if 'o_only_install' not in match.group():
                continue
            line = raw[:match.start()].count('\n')
            column = match.start() - raw.rfind('\n', 0, match.start()) - 1
            active = lines[line][column:] if line < len(lines) else ''
            if active.startswith('#[') or re.match(r'include\s*!', active):
                errors.append(f'{name}: production installer reinclude')
        return errors

    def inspect(self, source):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / 'source.rs'
            path.write_text(source)
            return self.errors(source, guard.classifier._production_text(path), 'source.rs')

    def test_actual_installer_has_its_own_dormant_production_census(self):
        root = Path.cwd()
        files, skips = guard.classifier._scan_inputs(root, guard.classifier.PINNED_TEST_ONLY_MODULE_FILES)
        self.assertIn(root / self.INSTALLER, files)
        self.assertNotIn(root / self.INSTALLER, skips, 'installer must be scanned as production')
        source = (root / 'src/services/discord/watchers/lifecycle/n4d2_boundary_tests.rs').read_text()
        census = re.findall(
            r'fn n4d2_dormant_census_counts_actual_production_callers\(\)\s*\{\s*let script = r#"(.*?)"#;',
            source, re.S)
        self.assertEqual(len(census), 1, 'shared dormant census must have exactly one definition')
        result = subprocess.run([sys.executable, '-c', census[0], str(root)],
                                capture_output=True, text=True, check=False, timeout=180)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stdout.splitlines(), [
            'canonical_retired_production_writers=0',
            'restore_retired_o_watchers_owner_runtime_boot_callers=0',
            'canonical_retired_condition_unchanged=true',
            'activation_ready=false',
        ])

    def test_installer_direct_call_function_value_alias_and_reexport_fail(self):
        for source in (
            'fn startup() { install_retired_o_watcher(request); }',
            'fn startup() { let f = install_retired_o_watcher; }',
            'use crate::services::discord::install_retired_o_watcher as hidden;',
            'pub use hidden::OOnlyInstallRequest as Request;',
            'fn startup() { claim_retired_o_watcher_for_host(args); }',
        ):
            with self.subTest(source=source):
                self.assertTrue(self.inspect(source))

    def test_installer_cfg_removal_and_reinclude_fail_without_hiding_linux(self):
        for item in (
            '#[path = "watchers/lifecycle/o_only_install.rs"] mod hidden;',
            'include!("watchers/lifecycle/o_only_install.rs");',
            'pub use hidden::install_retired_o_watcher;',
        ):
            with self.subTest(item=item):
                self.assertEqual(self.inspect('#[cfg(test)]\n' + item), [])
                self.assertTrue(self.inspect(item), 'removing the test-only gate must fail')
                self.assertTrue(self.inspect('#[cfg(any(test, target_os = "linux"))]\n' + item))

    def test_installer_prose_and_test_only_items_are_not_production_callers(self):
        source = ('// install_retired_o_watcher\n'
                  'fn prose() { let a = r#"include!(\"o_only_install.rs\");"#; }\n'
                  '#[cfg(test)] fn fixture() { install_retired_o_watcher(request); }')
        self.assertEqual(self.inspect(source), [])


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
