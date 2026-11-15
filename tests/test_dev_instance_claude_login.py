"""Explicit default Claude login binding, without reading login material."""
import copy
import json
from pathlib import Path
from unittest import TestCase
from unittest.mock import patch

import test_dev_instance as declarations
from scripts.dev_instance.files import InstanceError
from scripts.dev_instance.manifest import child_environment, validate


class ClaudeLoginTests(TestCase):
    def setUp(self):
        self.fixture = declarations.InstanceDeclarationTests()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        home = self.fixture.home
        self.login = {'mode': 'default_active', 'account_slot': 3, 'login_home': str(home),
                      'config_dir': str(home / '.claude'), 'cli': str(home / '.local/share/claude/versions/fixture')}

    def test_opt_in_binds_manifest_receipt_and_only_claude_catalog(self):
        m = self.fixture.prepare(claude_login=self.login)
        root = Path(m['root'])
        self.assertEqual(m['claude_login'], self.login)
        self.assertEqual(json.loads((root/'receipt.json').read_text())['claude_login'], self.login)
        org = json.loads((root/'config/org.yaml').read_text())
        self.assertEqual(org['provider_auth_profiles']['test-claude']['home'], self.login['config_dir'])
        self.assertEqual(org['provider_auth_profiles']['test-codex']['home'], str(root/'home/.adk/profiles/codex/test-codex'))
        env = child_environment(m, {})
        self.assertEqual(env['HOME'], str(root/'home'))
        self.assertEqual(env['CLAUDE_CONFIG_DIR'], self.login['config_dir'])
        self.assertEqual(env['CODEX_HOME'], str(root/'home/.adk/profiles/codex/test-codex'))
        self.assertIn(str(self.fixture.home/'.local/bin'), env['PATH'].split(':'))
        self.assertFalse(any((root/'home').rglob('auth.json')))
        self.assertEqual(list((root/'credential').iterdir()), [])
        self.assertEqual(validate(self.fixture.base, m['instance_id'], {})['claude_login'], self.login)

    def test_no_opt_in_keeps_original_declaration_and_runtime_path(self):
        m = self.fixture.prepare()
        self.assertNotIn('claude_login', m)
        self.assertNotIn('claude_login', json.loads((Path(m['root'])/'receipt.json').read_text()))
        self.assertEqual(child_environment(m, {})['PATH'], str(self.fixture.worktree/'target/debug')+':/usr/bin:/bin')

    def test_slot_one_and_noncanonical_binding_are_rejected_before_create(self):
        for key, value in [('account_slot', 1), ('account_slot', True), ('mode', 'implicit'),
                           ('config_dir', str(self.fixture.home/'other')), ('extra', 'forbidden')]:
            with self.subTest(key=key, value=value):
                changed = {**self.login, key: value}
                with self.assertRaises(InstanceError):
                    self.fixture.prepare(claude_login=changed)
                self.assertFalse((self.fixture.base/self.fixture.instance_id).exists())

    def test_changed_binding_is_rejected_even_if_receipt_is_changed(self):
        m = self.fixture.prepare(claude_login=self.login)
        root = Path(m['root'])
        for name in ['manifest.json', 'receipt.json']:
            d = json.loads((root/name).read_text());d['claude_login']['config_dir'] = str(self.fixture.home/'other')
            (root/name).write_text(json.dumps(d))
        with self.assertRaises(InstanceError):
            validate(self.fixture.base, m['instance_id'], {})

    def test_discovery_uses_only_active_slot_and_discards_private_fields(self):
        from scripts.dev_instance.config import active_claude_login
        from types import SimpleNamespace
        output = json.dumps({'activeAccountNumber': 3, 'accounts': [{'number': 1, 'email': 'private@example.invalid'}]})
        executable = self.fixture.home/'.local/bin/claude'
        executable.parent.mkdir(parents=True)
        executable.write_text('fixture executable')
        with patch('scripts.dev_instance.config.subprocess.run', return_value=SimpleNamespace(returncode=0, stdout=output)) as run:
            value = active_claude_login(self.fixture.home)
        self.assertEqual(value['account_slot'], 3)
        self.assertNotIn('private@example.invalid', json.dumps(value))
        self.assertEqual(run.call_args.args[0][-2:], ['list', '--json'])
        with patch('scripts.dev_instance.config.subprocess.run', return_value=SimpleNamespace(returncode=0, stdout='{"activeAccountNumber":1}')):
            with self.assertRaises(InstanceError):active_claude_login(self.fixture.home)

    def test_discovery_timeout_rejects_without_exposing_output(self):
        from scripts.dev_instance.config import active_claude_login
        import subprocess
        with patch('scripts.dev_instance.config.subprocess.run', side_effect=subprocess.TimeoutExpired(['cswap', 'list'], 30, output='private@example.invalid')):
            with self.assertRaises(InstanceError) as raised:
                active_claude_login(self.fixture.home)
        self.assertNotIn('private@example.invalid', str(raised.exception))
