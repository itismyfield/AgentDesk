import contextlib
import io
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import check_env_flag as guard


class EnvFlagGuardTests(unittest.TestCase):
    def test_inline_multiline_and_local_comparisons(self):
        variants = [
            'std::env::var("FLAG").as_deref() == Ok("1")',
            'std::env::var("FLAG").is_ok_and(|v| v == "true")',
            'std::env::var_os("FLAG").is_some_and(|v| v == "0")',
            'std::env::var("FLAG")\n.ok().map(|v| matches!(v.trim(), "1" | "true"))',
            'std::env::var("FLAG").unwrap_or_default().parse::<bool>()',
        ]
        for expression in variants:
            with self.subTest(expression=expression):
                self.assertEqual(len(guard.sites(f'fn f() {{ let enabled = {expression}; }}')), 1)
        self.assertEqual(len(guard.sites('fn f() { let raw = std::env::var("FLAG").ok(); let normalized = raw.as_deref(); matches!(normalized, Some("yes")) }')), 1)
        self.assertEqual(len(guard.sites('fn f() { std::env::var("F") == Ok("1") || std::env::var("F") == Ok("1") }')), 2)

    def test_import_aliases_cannot_bypass_the_reader_guard(self):
        for imports, call in [
            ('use std::env::var;', 'var'),
            ('use std::env::var as flag;', 'flag'),
            ('use std::env::{var as flag, var_os};', 'flag'),
            ('use std::{env::{var_os as flag}};', 'flag'),
            ('use std::env as process_env;', 'process_env::var'),
            ('use std::{env as process_env};', 'process_env::var_os'),
            ('use std::env::{self as process_env};', 'process_env::var'),
        ]:
            with self.subTest(imports=imports):
                self.assertEqual(len(guard.sites(imports + ' fn f() { ' + call + '("FLAG").as_deref() == Ok("1"); }')), 1)

    def test_cfg_and_prose_and_data_presence_are_not_bool_flags(self):
        text = '''
        // std::env::var("FLAG").as_deref() == Ok("1")
        const EXAMPLE: &str = r#"std::env::var("FLAG") == Ok("1")"#;
        #[cfg(all(unix, test))]
        fn test_helper() { let _ = std::env::var("FLAG").as_deref() == Ok("1"); }
        #[cfg(test)] mod tests { fn test() { let _ = std::env::var("FLAG") == Ok("1"); } }
        fn credential() { std::env::var("TOKEN").is_ok_and(|v| !v.is_empty()); }
        fn url() { std::env::var("URL").unwrap_or_default() == "https://example.com"; }
        #[test] fn standalone() { std::env::var("FLAG") == Ok("1"); }
        #[tokio::test(flavor = "current_thread")]
        async fn asynchronous() { std::env::var("FLAG") == Ok("1"); }
        fn wrapper() { env_flag("FLAG", false); }
        fn branch() { #[cfg(test)] env_flag("TEST", false) }
        struct Fields { #[cfg(test)] value: bool, live: bool }
        fn namespace() { #[cfg(test)] helper::call(); }
        '''
        self.assertEqual(guard.sites(text), [])
        self.assertEqual(guard.collect([('src/a_tests.rs', 'fn f() { std::env::var("F") == Ok("1"); }')])[0], {})

    def test_delta_rejects_new_read_without_a_baseline_file_and_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            def git(*args):
                subprocess.run(['git', '-C', tmp, *args], check=True, capture_output=True)
            git('init', '-q')
            git('config', 'user.name', 'Fixture')
            git('config', 'user.email', 'fixture@example.com')
            (root / 'src').mkdir()
            source = root / 'src/a.rs'
            source.write_text('fn old() { std::env::var("OLD").as_deref() == Ok("1"); }')
            git('add', '.')
            git('commit', '-qm', 'base')
            ci = (guard.ROOT / 'scripts/ci-script-checks.sh').read_text()
            registration = re.search(r'if run_check guards "Environment bool single parser"; then\n(.*?)\nfi', ci, re.S)
            self.assertIsNotNone(registration, 'required guards registration missing')
            command = registration[1].splitlines()[0]
            def registered_command():
                return subprocess.run(
                    ['bash', '-c', command + ' --root "$1" --base-ref HEAD', '_', tmp],
                    cwd=guard.ROOT, env={**os.environ, 'PYTHON': sys.executable},
                    text=True, capture_output=True,
                )
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(guard.main(['--root', tmp, '--base-ref', 'HEAD']), 0)
                source.write_text(source.read_text() + '\nfn new() { std::env::var("NEW").as_deref() != Ok("0"); }')
                self.assertEqual(guard.main(['--root', tmp, '--base-ref', 'HEAD']), 1)
                red = registered_command()
                self.assertEqual(red.returncode, 1, red.stdout + red.stderr)
                self.assertIn('ENV_FLAG FAIL', red.stdout)
                for expression in [
                    'let Ok(value) = std::env::var("FLAG") else { return true; }; !matches!(value.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no")',
                    'let Some(value) = std::env::var("FLAG").ok() else { return false; }; value.eq("1")',
                    'let Ok(mut value) = std::env::var("FLAG") else { return false; }; value.make_ascii_lowercase(); value.eq("true")',
                    'std::env::var("FLAG").is_ok_and(|v| v.eq_ignore_ascii_case("true"))',
                    'match 0 { #[cfg(test)] 1 => false, _ => std::env::var("FLAG").as_deref() == Ok("1") }',
                    'std::env::var("FLAG").is_ok_and(|v| ["1", "true"].contains(&v.as_str()))',
                ]:
                    with self.subTest(expression=expression):
                        source.write_text('fn enabled() -> bool { ' + expression + ' }')
                        rejected = registered_command()
                        self.assertEqual(rejected.returncode, 1, rejected.stdout + rejected.stderr)
                        self.assertIn('added=1', rejected.stdout)
                source.write_text('fn new() { env_flag("NEW", true); } #[cfg(test)] fn fixture() { std::env::var("ADK_TEST_FIXTURE").is_ok_and(|v| v.eq("1")); }')
                self.assertEqual(guard.main(['--root', tmp, '--base-ref', 'HEAD']), 0)
                green = registered_command()
                self.assertEqual(green.returncode, 0, green.stdout + green.stderr)
                self.assertIn('ENV_FLAG PASS', green.stdout)
                self.assertEqual(guard.main(['--root', tmp, '--base-ref', 'missing']), 1)
