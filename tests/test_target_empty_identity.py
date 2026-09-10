"""Static non-lib identity regression controls for #5081 (no cargo)."""
import shlex
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from tests.test_check_test_target_integrity import integrity


class TargetEmptyIdentity(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / "src").mkdir()
        (self.root / "tests").mkdir()
        (self.root / "Cargo.toml").write_text(
            '[package]\nname="fixture"\n[[bin]]\nname="alias"\n'
            'path="src/custom.rs"\n', encoding="utf-8")
        self.source = self.root / "src/custom.rs"
        self.source.write_text('fn main() {}\n', encoding="utf-8")
        (self.root / "tests/smoke.rs").write_text(
            '#[test]\nfn my_real_case() {}\n', encoding="utf-8")

    def validate(self, command):
        inventories = {key: integrity.collect_modules(path, self.root)
                       for key, path in integrity.discover_targets(self.root).items()}
        return integrity.validate_command(
            integrity.parse_command(shlex.split(command)), inventories, self.root)

    def test_top_level_integration_identity(self):
        self.assertEqual(self.validate('cargo test --test smoke my_real_case'), [])
        self.assertEqual([k for k, _ in self.validate(
            'cargo test --test smoke nonexistent')], ['zero-match'])

    def test_bin_alias_and_unrelated_module_do_not_hide_typo(self):
        for extra in ('', 'mod unrelated {}\n'):
            with self.subTest(extra=extra):
                self.source.write_text(
                    '#[test]\nfn my_real_case() {}\n' + extra, encoding='utf-8')
                self.assertEqual(self.validate('cargo test --bin alias my_real_case'), [])
                self.assertEqual([k for k, _ in self.validate(
                    'cargo test --bin alias nonexistent')], ['zero-match'])

    def test_empty_bin_uses_test_identity_not_module_count(self):
        for extra in ('', 'mod unrelated {}\n'):
            self.source.write_text('fn main() {}\n' + extra, encoding='utf-8')
            self.assertEqual([k for k, _ in self.validate(
                'cargo test --bin alias typo')], ['empty-target'])
        self.assertEqual(self.validate('cargo test --bin alias'), [])

    def test_redirected_test_identity_and_libtest_selection(self):
        self.source.write_text('#[path="child.rs"]\nmod renamed;\n', encoding='utf-8')
        (self.root / 'src/child.rs').write_text(
            '#[test]\nfn my_real_case() {}\n', encoding='utf-8')
        self.assertEqual(self.validate(
            'cargo test --bin alias renamed::my_real_case -- --exact'), [])
        for args in ('renamed::typo', 'my_real_case -- --exact',
                     'renamed::my_real_case -- --skip my_real_case'):
            self.assertEqual([k for k, _ in self.validate(
                'cargo test --bin alias ' + args)], ['zero-match'])

    def test_missing_redirect_fails_closed(self):
        self.source.write_text('#[path="missing.rs"]\nmod missing;\n', encoding='utf-8')
        self.assertEqual([k for k, _ in self.validate(
            'cargo test --bin alias missing')], ['inventory-error'])

    def test_read_failure_fails_closed(self):
        with mock.patch.object(integrity, 'collect_static_tests',
                               side_effect=OSError('unreadable source')):
            self.assertEqual([k for k, _ in self.validate(
                'cargo test --test smoke my_real_case')], ['inventory-error'])

    def test_multiple_targets_use_identity_union(self):
        self.assertEqual(self.validate(
            'cargo test --bin alias --test smoke my_real_case'), [])


if __name__ == '__main__':
    unittest.main()
