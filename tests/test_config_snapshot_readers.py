"""Disk-reader guard fixtures exercise references, narrow exceptions and fatal CI wiring."""
import copy
import tempfile
import unittest
import subprocess
from unittest.mock import patch
from pathlib import Path

from scripts import check_config_snapshot_readers as guard
from scripts import check_writer_gate_ci_wiring as wiring


class ConfigSnapshotReadersTests(unittest.TestCase):
    def entry(self, path, code, name='allowed'):
        refs, errors, items = guard.scan_text(path, code)
        self.assertEqual(errors, [])
        item, = [item for item in (items or guard.functions(code)) if item[0] == name]
        counts = guard.Counter(api for at, api in refs if item[1] <= at < item[2])
        return {'file': path, 'function': name, 'signature': item[3],
                'kind': 'shared-fallback', 'reason': 'A/B fixture',
                'tests': ['fixture_snapshot_A_disk_B'], 'apis': dict(counts),
                'sha256': guard.digest(code[item[1]:item[2]])}

    def fixture(self, extra=''):
        path = 'src/reader.rs'
        code = 'fn allowed() { crate::config::load(); }\n' + extra
        return path, code, self.entry(path, code)

    def errors(self, code, entries=()):
        return guard.audit({'src/reader.rs': code}, entries, required=())[0]

    def test_current_function_exception_is_narrow_and_nonempty(self):
        path, code, entry = self.fixture()
        errors, counts = guard.audit({path: code}, [entry], required=())
        self.assertEqual(errors, [])
        self.assertEqual(counts['production_files'], 1)
        self.assertEqual(counts['disk_references'], 1)

    def test_consumer_loader_reintroduction_is_red(self):
        path, code, entry = self.fixture('fn consumer() { crate::config::load_graceful(); }')
        errors, _ = guard.audit({path: code}, [entry], required=())
        self.assertTrue(any('consumer: unapproved' in error for error in errors), errors)

    def test_consumer_function_value_is_red_without_call_parenthesis(self):
        code = 'fn consumer() { spawn_blocking(crate::config::load_graceful); }'
        self.assertTrue(any('unapproved' in error for error in self.errors(code)))

    def test_raw_yaml_config_read_is_red(self):
        code = ('fn consumer() { let path = crate::config::config_path::config_path_in(root); '
                'let bytes = read_optional(path); serde_yaml::from_slice::<Config>(&bytes); }')
        errors = self.errors(code)
        self.assertTrue(any('raw-config-yaml' in error for error in errors), errors)
        self.assertTrue(any('config_path_in' in error for error in errors), errors)

    def test_absolute_path_raw_yaml_and_config_type_alias_are_red(self):
        for declaration, typename in (('', 'crate::config::Config'),
                                      ('use crate::config::Config as Candidate;', 'Candidate'),
                                      ('type Candidate = crate::config::Config;', 'Candidate')):
            code = (declaration + 'fn consumer() { let bytes = std::fs::read(path); '
                    'let value: ' + typename + ' = serde_yaml::from_str(&bytes); }')
            with self.subTest(typename=typename):
                self.assertTrue(any('raw-config-yaml' in error for error in self.errors(code)))

    def test_proof_mapping_is_bound_to_an_existing_source_function(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'src').mkdir()
            (root / 'src/proof.rs').write_text('#[test] fn check_contract() {}')
            rows = [{'tests': ['src/proof.rs::check_contract']}]
            self.assertEqual(guard.proof_errors(root, rows), [])
            for proof in ('src/proof.rs::missing', 'src/missing.rs::check_contract',
                          'src/../escape.rs::check_contract', 'TODO'):
                with self.subTest(proof=proof):
                    self.assertTrue(guard.proof_errors(root, [{'tests': [proof]}]))

    def test_proof_selector_requires_a_test_attribute_and_ignores_prose(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'src').mkdir()
            path = root / 'src/proof.rs'
            rows = [{'tests': ['src/proof.rs::check_contract']}]
            rejected = (
                '// #[test] fn check_contract() {}',
                'const TEXT: &str = "#[test] fn check_contract() {}";',
                'fn check_contract() {}',
                '#[test] fn another_contract() {} fn check_contract() {}',
            )
            for source in rejected:
                with self.subTest(source=source):
                    path.write_text(source)
                    self.assertTrue(guard.proof_errors(root, rows))
            for attribute in ('#[test]', '#[tokio::test]', '#[async_std::test]'):
                with self.subTest(attribute=attribute):
                    path.write_text(attribute + ' async fn check_contract() {}')
                    self.assertEqual(guard.proof_errors(root, rows), [])

    def test_aliased_yaml_parser_and_deserializer_constructor_are_red(self):
        for parser in ('use serde_yaml::from_str as decode; fn consumer() { let c: Config = decode(text); }',
                       'fn consumer() -> Config { serde_yaml::Deserializer::from_str(text); Config::deserialize(value) }'):
            with self.subTest(parser=parser):
                self.assertTrue(any('raw-config-yaml' in error for error in self.errors(parser)))

    def test_tracked_file_missing_and_empty_tracked_census_fail_closed(self):
        root = Path('/checkout')
        with patch.object(guard.subprocess, 'run') as run:
            run.return_value = subprocess.CompletedProcess([], 0, 'src/lib.rs\0src/missing.rs\0', '')
            self.assertEqual(guard.tracked_missing(root, [root / 'src/lib.rs']),
                             ['src/missing.rs: tracked source missing from scan'])
            run.return_value = subprocess.CompletedProcess([], 0, '', '')
            with self.assertRaisesRegex(RuntimeError, 'census is empty'):
                guard.tracked_missing(root, [])

    def test_grouped_alias_module_alias_and_reexport_are_red(self):
        cases = [
            'use crate::config::load as disk; fn consumer() { disk(); }',
            'use crate::{config::{load_graceful as disk}}; fn consumer() { disk(); }',
            'use crate::config as cfg; fn consumer() { cfg::load(); }',
            'pub use crate::config::load as disk;',
            'pub use crate::config as cfg;',
            'use crate::config::{self as cfg}; fn consumer() { cfg::load(); }',
            'fn consumer() { use crate::config::load; let function = load; }',
        ]
        for code in cases:
            with self.subTest(code=code):
                self.assertTrue(self.errors(code))

    def test_config_glob_is_fail_closed(self):
        for code in ('use crate::config::*; fn consumer() { load(); }',
                     'use crate::config as c; use c::*; fn consumer() { load(); }'):
            with self.subTest(code=code):
                self.assertTrue(any('glob' in error for error in self.errors(code)))

    def test_non_ascii_config_alias_and_raw_parser_outside_function_are_red(self):
        code = 'use crate::config as 설정; fn consumer() { 설정::load(); }'
        self.assertTrue(any('unsupported config import alias' in error for error in self.errors(code)))
        code = 'const PARSER: fn() -> Config = serde_yaml::from_str::<Config>;'
        self.assertTrue(any('unowned raw-config-yaml' in error for error in self.errors(code)))

    def test_conflicting_alias_is_fail_closed(self):
        code = 'use crate::config::load as x; mod inner { use other::thing as x; fn f() { x(); } }'
        self.assertTrue(any('ambiguous' in error for error in self.errors(code)))

    def test_parent_namespace_alias_cannot_hide_a_child_reader(self):
        path, code, entry = self.fixture()
        cases = (
            ('extern crate self as root;', 'root::config::load_graceful();'),
            ('use serde_yaml as yaml;',
             'let value: Config = yaml::from_str(text);'),
            ('use serde_yaml::from_str as decode;',
             'let value: Config = decode(text);'),
            ('use crate::config as cfg;', 'cfg::load_graceful();'),
            ('use crate as root;', 'root::config::load_graceful();'),
            ('use crate::config::Config as Candidate;',
             'let value: Candidate = serde_yaml::from_str(text);'),
            ('type Candidate = crate::config::Config;',
             'let value: Candidate = serde_yaml::from_str(text);'),
        )
        for declaration, reader in cases:
            with self.subTest(declaration=declaration):
                sources = {path: code + declaration + 'mod child;',
                           'src/reader/child.rs': 'use super::*; fn consumer() { ' + reader + ' }'}
                self.assertTrue(guard.audit(sources, [entry], required=())[0])

    def test_relative_imports_cannot_erase_the_parent_config_namespace(self):
        path, code, entry = self.fixture()
        cases = (
            ('use crate::config::config_path;',
             'use super::config_path; config_path::load_config_file(path);'),
            ('use crate::config;', 'use super::config as cfg; cfg::load_graceful();'),
            ('use crate::config;', 'use super::config; config::load_graceful();'),
            ('use crate::config;', 'use super as parent; parent::config::load_graceful();'),
            ('use crate::config;', 'use super::config::{load as disk}; disk();'),
            ('use crate::config::Config;',
             'use super::Config as Candidate; let c: Candidate = serde_yaml::from_str(text);'),
        )
        for parent, child in cases:
            with self.subTest(child=child):
                sources = {path: code + parent + 'mod child;',
                           'src/reader/child.rs': 'fn consumer() { ' + child + ' }'}
                self.assertTrue(guard.audit(sources, [entry], required=())[0])

    def test_dynamic_config_module_macro_is_unclassified(self):
        path, code, entry = self.fixture()
        source = code + ('macro_rules! read { ($module:path) => { $module::load(); }; } '
                         'fn consumer() { read!(crate::config); }')
        self.assertTrue(guard.audit({path: source}, [entry], required=())[0])

    def test_raw_identifiers_and_whitespace_paths_are_red(self):
        for code in ('fn f() { crate :: config :: load_graceful(); }',
                     'fn f() { crate::r#config::r#load(); }'):
            with self.subTest(code=code):
                self.assertTrue(any('unapproved' in error for error in self.errors(code)))

    def test_editor_exception_does_not_cover_runtime_neighbor(self):
        path, code, entry = self.fixture('fn runtime_voice() { crate::config::load(); }')
        errors, _ = guard.audit({path: code}, [entry], required=())
        self.assertTrue(any('runtime_voice: unapproved' in error for error in errors), errors)

    def test_indirect_editor_consumer_rollback_is_pinned_without_exception(self):
        code = 'fn allowed() { if let Some(live) = snapshot() { return live; } load_editable_config() }'
        entry = self.entry('src/reader.rs', code)
        entry['kind'] = 'snapshot-consumer'
        self.assertEqual(entry['apis'], {})
        errors, _ = guard.audit({'src/reader.rs': code}, [entry], required=())
        self.assertEqual(errors, ['disk API census is empty; producer/exception coverage disappeared'])
        altered = 'fn allowed() { load_editable_config() }'
        self.assertTrue(any('signature/body changed' in error
                            for error in guard.audit({'src/reader.rs': altered}, [entry], required=())[0]))
        altered = code.replace('load_editable_config()', 'crate::config::load()')
        self.assertTrue(any('API count mismatch' in error
                            for error in guard.audit({'src/reader.rs': altered}, [entry], required=())[0]))

    def test_added_reference_and_changed_body_are_red(self):
        path, code, entry = self.fixture()
        changed = code.replace('crate::config::load();',
                               'crate::config::load(); crate::config::load();')
        errors, _ = guard.audit({path: changed}, [entry], required=())
        self.assertTrue(any('API count mismatch' in error for error in errors), errors)
        self.assertTrue(any('signature/body changed' in error for error in errors), errors)

    def test_exception_expansion_without_repin_is_red(self):
        path, code, entry = self.fixture()
        changed = code.replace('load();', 'load_graceful();')
        errors, _ = guard.audit({path: changed}, [entry], required=())
        self.assertTrue(any('signature/body changed' in error for error in errors), errors)
        self.assertTrue(any('API count mismatch' in error for error in errors), errors)

    def test_empty_stale_duplicate_and_ambiguous_registry_are_red(self):
        path, code, entry = self.fixture()
        for field, value in [('function', ''), ('signature', ''), ('file', '*'),
                             ('apis', {}), ('apis', {'load': 0})]:
            altered = copy.deepcopy(entry)
            altered[field] = value
            with self.subTest(field=field, value=value):
                self.assertTrue(guard.audit({path: code}, [altered], required=())[0])
        self.assertTrue(guard.audit({path: code}, [entry, entry], required=())[0])
        self.assertTrue(guard.audit({}, [entry], required=())[0])
        duplicate = code + 'mod inner { fn allowed() { crate::config::load(); } }'
        self.assertTrue(any('expected one function' in error
                            for error in guard.audit({path: duplicate}, [entry], required=())[0]))

    def test_missing_anchor_and_empty_scan_never_pass_as_zero(self):
        self.assertTrue(any('required source missing' in error for error in guard.audit({}, [])[0]))
        self.assertTrue(any('target count is zero' in error for error in guard.audit({}, [])[0]))
        self.assertTrue(any('census is empty' in error
                            for error in guard.audit({'src/reader.rs': 'fn f() {}'}, [], required=())[0]))

    def production(self, source):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'case.rs'
            path.write_text(source)
            return guard._production_text(path)

    def test_nested_cfg_test_does_not_exempt_production_line_four(self):
        code = self.production('mod nested {\n#![cfg(test)]\n}\n'
                               'fn bypass() { crate::config::load(); }\n')
        refs, errors, _ = guard.scan_text('src/reader.rs', code)
        self.assertEqual(errors, [])
        self.assertEqual([code[:at].count('\n') + 1 for at, _ in refs], [4])

    def test_file_leading_cfg_test_stays_exempt(self):
        code = self.production('#![cfg(test)]\nfn helper() { crate::config::load(); }\n')
        self.assertEqual(guard.scan_text('src/reader.rs', code)[0], [])

    def test_mixed_cfg_and_macro_tokens_stay_production(self):
        for attribute in ('#[cfg(any(test, unix))]', '#[cfg(not(test))]',
                          '#[cfg(all(not(test), unix))]'):
            with self.subTest(attribute=attribute):
                code = self.production(attribute + '\nfn f() { crate::config::load(); }')
                self.assertTrue(guard.scan_text('src/reader.rs', code)[0])
        code = self.production('macro_rules! f { () => { crate::config::load(); }; }')
        self.assertTrue(guard.scan_text('src/reader.rs', code)[0])

    def test_test_module_prose_and_string_braces_are_masked(self):
        code = self.production('// crate::config::load();\n'
                               'fn f() { let text = r#"} crate::config::load();"#; }\n'
                               '#[cfg(test)]\nmod tests { fn t() { crate::config::load(); } }')
        self.assertEqual(guard.scan_text('src/reader.rs', code)[0], [])

    def test_unterminated_literals_and_comments_fail_closed(self):
        for source in ('fn f() { "unterminated', '/* comment', 'r###" raw'):
            with self.subTest(source=source):
                self.assertTrue(guard.lexical_errors('src/case.rs', source))

    def test_basename_production_mount_and_double_mount_are_rejected(self):
        for declarations in ('mod helper_tests;',
                             '#[cfg(test)] mod helper_tests; mod prod { #[path="../helper_tests.rs"] mod helper; }'):
            with self.subTest(declarations=declarations), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / 'src').mkdir()
                lib = root / 'src/lib.rs'
                child = root / 'src/helper_tests.rs'
                lib.write_text(declarations)
                child.write_text('fn f() { crate::config::load(); }')
                with self.assertRaises(RuntimeError):
                    guard.skip_pin.validate_basename_mounts([lib, child], {child}, set())

    def test_guard_and_unit_invocations_are_protected_outside_aggregate(self):
        root = Path(__file__).resolve().parents[1]
        aggregate = (root / 'scripts/ci-script-checks.sh').read_text()
        self.assertEqual(wiring.check_text(aggregate), [])
        for command in ('"$PYTHON" scripts/check_config_snapshot_readers.py',
                        '"$PYTHON" -m unittest tests.test_config_snapshot_readers'):
            self.assertEqual(aggregate.count(command + '\n'), 1)
            altered = aggregate.replace(command, '# removed', 1)
            self.assertTrue(wiring.check_text(altered))


if __name__ == '__main__':
    unittest.main()
