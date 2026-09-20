"""Exercise the finite file-reference policy and its deliberate ownership limit."""

from pathlib import Path
import tempfile
import unittest

from scripts.check_test_env_lock_references import audit, canonical_owners, rust_text, use_paths


CONFIG = """
fn shared_test_env_lock() { LOCK.get_or_init(|| std::sync::Mutex::new(())) }
mod test_env_lock {
    fn acquire_shared_test_env_lock() {
        let mutex = super::shared_test_env_lock(); mutex.lock();
    }
}
use test_env::{TestEnvVarGuard, TestRuntimeRootGuard, set_agentdesk_root_for_test};
"""
ENV = """
impl TestEnvVarGuard {
    fn set_path(key: &str, value: &Path) {
        let lock = super::test_env_lock::acquire_shared_test_env_lock();
        std::env::set_var(key, value); Self { _lock: Some(lock) }
    }
}
fn set_agentdesk_root_for_test(path: &Path) {
    TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", path)
}
impl TestRuntimeRootGuard {
    fn new() { let env = set_agentdesk_root_for_test(root.path()); Self { _env: env } }
}
"""
SUPPORT = """
mod test_support {
    impl SharedEnvLock {
        fn lock(&self) { crate::config::shared_test_env_lock().lock() }
    }
    static TEST_ENV_LOCK: SharedEnvLock = SharedEnvLock;
    fn lock_test_env() {
        TEST_ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
"""
WRITE = 'std::env::set_var("AGENTDESK_ROOT_DIR", root);'
RAW = "crate::config::shared_test_env_lock().lock();"
SETTER = 'crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root);'
BORROWED = 'crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root);'
WRAPPER = "crate::services::turn_orchestrator::test_support::lock_test_env();"


class FileReferencePolicy(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.write("src/config.rs", CONFIG)
        self.write("src/config/test_env.rs", ENV)
        self.write("src/services/turn_orchestrator.rs", SUPPORT)

    def write(self, path, text):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)

    def check(self, body, valid, path="src/victim.rs"):
        self.write(path, body)
        errors, count, _ = audit(self.root)
        self.assertGreaterEqual(count, 2)
        if valid:
            self.assertEqual(errors, [])
        else:
            self.assertTrue(any(path in error for error in errors), errors)

    def test_new_root_set_file_is_red(self):
        self.check(WRITE, False)

    def test_new_root_remove_file_is_red(self):
        self.check('std::env::remove_var("AGENTDESK_ROOT_DIR");', False)

    def test_borrowed_after_helper_is_not_an_owner(self):
        self.check(BORROWED, False)

    def test_removing_last_owner_turns_file_red(self):
        self.check(RAW + WRITE, True)
        self.check(WRITE, False)

    def test_prose_cannot_supply_owner(self):
        self.check('// ' + RAW + '\nlet example = "' + RAW + '";\n' + WRITE, False)

    def test_local_same_named_mutex_and_wrapper_are_red(self):
        self.check('fn shared_test_env_lock() { OTHER.lock(); } shared_test_env_lock();' + WRITE, False)
        self.check('fn lock_test_env() { OTHER.lock(); } lock_test_env();' + WRITE, False)

    def test_qualified_canonical_raw_lock_is_green(self):
        self.check(RAW + WRITE, True)
        self.check("use crate::config::{self}; config::shared_test_env_lock();" + WRITE, True)

    def test_canonical_owning_setter_is_green(self):
        self.check(SETTER, True)
        self.check('crate::config::set_agentdesk_root_for_test(root);', True)
        self.check('crate::config::TestRuntimeRootGuard::new();', True)

    def test_verified_wrapper_qualified_and_imported_calls(self):
        self.check(WRAPPER + WRITE, True)
        self.check('use crate::services::turn_orchestrator::test_support::{lock_test_env as guard}; guard();' + WRITE, True)

    def test_embedded_multiline_and_constant_discovery(self):
        self.check('mod tests { const ROOT_KEY: &str = "AGENTDESK_ROOT_DIR";\n std::env::set_var(\n ROOT_KEY, root); }', False)
        self.check('mod tests { std::env::remove_var(\n AGENTDESK_ROOT_DIR_ENV\n); }', False)

    def test_unresolved_wrapper_is_unknown_red(self):
        self.check('new_wrapper::lock_test_env();' + WRITE, False)

    def test_forwarding_mutant_is_red(self):
        self.write("src/services/turn_orchestrator.rs", SUPPORT.replace('crate::config::shared_test_env_lock()', 'OTHER'))
        errors, _, _ = audit(self.root)
        self.assertIn("canonical", errors[0])

    def test_decoy_canonical_call_cannot_validate_wrapper(self):
        self.write("src/services/turn_orchestrator.rs", SUPPORT.replace(RAW[:-1], RAW + ' OTHER.lock()'))
        self.assertTrue(audit(self.root)[0])

    def test_wrapper_cannot_discard_canonical_guard_and_return_other(self):
        self.write("src/services/turn_orchestrator.rs", SUPPORT.replace(
            'TEST_ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())',
            'TEST_ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner()); OTHER.lock()'))
        self.assertTrue(audit(self.root)[0])

    def test_same_file_unlocked_function_is_intentionally_green(self):
        self.check('fn guarded() { ' + RAW + WRITE + '} fn unlocked() { ' + WRITE + '}', True)
        self.check(RAW + RAW + WRITE, True)
        self.check(RAW + WRITE, True)

    def test_relative_wrapper_import_and_static_receiver(self):
        path = "src/services/turn_orchestrator/registry_purge.rs"
        self.check('mod tests { use super::super::test_support::lock_test_env; lock_test_env();' + WRITE + '}', True, path)
        self.check('use crate::services::turn_orchestrator::test_support::TEST_ENV_LOCK; TEST_ENV_LOCK.lock();' + WRITE, True, path)

    def test_unrelated_import_scope_cannot_supply_owner(self):
        self.check('mod a { use crate::config::shared_test_env_lock; } mod b { shared_test_env_lock();' + WRITE + '}', False)

    def test_root_table_dynamic_restore_is_checked(self):
        self.check('const KEYS: &[&str] = &["AGENTDESK_ROOT_DIR", "OTHER"];\nstd::env::remove_var(name);', False)
        self.check(RAW + 'const KEYS: &[&str] = &["AGENTDESK_ROOT_DIR"];\nstd::env::set_var(name, value);', True)

    def test_nonroot_dynamic_keys_are_reported_not_proved_safe(self):
        self.write('src/unknown.rs', 'std::env::set_var(name, value);')
        errors, _, outside = audit(self.root)
        self.assertEqual(errors, [])
        self.assertEqual(outside, ['src/unknown.rs'])

    def test_rust_literals_and_comments(self):
        for literal in ('"AGENTDESK_ROOT_DIR"', 'r"AGENTDESK_ROOT_DIR"', 'r##"AGENTDESK_ROOT_DIR"##'):
            with self.subTest(literal=literal):
                self.check('std::env::set_var(' + literal + ', root);', False)
        self.assertNotIn('shared_test_env_lock', rust_text('/* /* nested */ shared_test_env_lock */ r#"shared_test_env_lock {"#'))

    def test_use_tree_aliases(self):
        self.assertEqual(use_paths('crate::config::{self, test_env_lock::{acquire_shared_test_env_lock as acquire}, TestEnvVarGuard}'), [
            ('crate::config', 'config'), ('crate::config::test_env_lock::acquire_shared_test_env_lock', 'acquire'), ('crate::config::TestEnvVarGuard', 'TestEnvVarGuard')])

    def test_integration_test_file_is_scanned(self):
        self.check(WRITE, False, 'tests/ordinary.rs')

    def test_actual_wrapper_forwarding_mutation_is_red(self):
        repo = Path(__file__).resolve().parents[1]
        paths = ('src/config.rs', 'src/config/test_env.rs',
                 'src/services/turn_orchestrator.rs',
                 'src/services/observability/mod.rs',
                 'src/services/observability/test_support.rs')
        sources = {Path(path): rust_text((repo / path).read_text()) for path in paths}
        canonical_owners(sources)
        path = Path('src/services/turn_orchestrator.rs')
        before = sources[path]
        sources[path] = before.replace('crate::config::shared_test_env_lock().lock()', 'OTHER.lock()')
        self.assertNotEqual(before, sources[path])
        with self.assertRaisesRegex(ValueError, 'canonical mutex'):
            canonical_owners(sources)

    def test_ci_wiring_runs_checker_and_tests(self):
        source = (Path(__file__).resolve().parents[1] / 'scripts/ci-script-checks.sh').read_text()
        self.assertIn('"$PYTHON" scripts/check_test_env_lock_references.py\n', source)
        self.assertIn('"$PYTHON" -m unittest tests.test_test_env_lock_references\n', source)


if __name__ == '__main__':
    unittest.main()
