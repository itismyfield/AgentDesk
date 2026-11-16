"""Explicit default Claude login binding, without reading login material."""
import copy
import json
from pathlib import Path
from unittest import TestCase
from unittest.mock import patch

from tests import test_dev_instance as declarations
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
        expected = {**self.login, 'config_dir': str(root/'home/.adk/profiles/claude/test-claude')}
        self.assertEqual(m['claude_login'], expected)
        self.assertEqual(json.loads((root/'receipt.json').read_text())['claude_login'], expected)
        org = json.loads((root/'config/org.yaml').read_text())
        self.assertEqual(org['provider_auth_profiles']['test-claude']['home'], expected['config_dir'])
        self.assertEqual(org['provider_auth_profiles']['test-codex']['home'], str(root/'home/.adk/profiles/codex/test-codex'))
        env = child_environment(m, {})
        self.assertEqual(env['HOME'], str(root/'home'))
        self.assertEqual(env['CLAUDE_CONFIG_DIR'], expected['config_dir'])
        self.assertEqual(env['CODEX_HOME'], str(root/'home/.adk/profiles/codex/test-codex'))
        self.assertEqual(env['PATH'], str(self.fixture.worktree/'target/debug')+':/usr/bin:/bin')
        self.assertFalse(any((root/'home').rglob('auth.json')))
        self.assertEqual(list((root/'credential').iterdir()), [])
        self.assertEqual(validate(self.fixture.base, m['instance_id'], {})['claude_login'], expected)

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

    def test_opt_in_does_not_expose_codex_and_uses_claude_override(self):
        import shutil
        import subprocess
        outside = self.fixture.home / '.local/bin/codex'
        outside.parent.mkdir(parents=True)
        outside.write_text('#!/bin/sh\nexit 99\n')
        outside.chmod(0o700)
        cli = Path(self.login['cli'])
        cli.parent.mkdir(parents=True)
        cli.write_text('#!/bin/sh\nprintf "fixture-claude-version\\n"\n')
        cli.chmod(0o700)
        manifest = self.fixture.prepare(claude_login=self.login)
        env = child_environment(manifest, {})
        legacy = copy.deepcopy(manifest)
        legacy.pop('claude_login')
        previous = child_environment(legacy, {})
        self.assertNotEqual(shutil.which('codex', path=env['PATH']), str(outside))
        self.assertEqual(env['PATH'], previous['PATH'])
        self.assertEqual(shutil.which('codex', path=env['PATH']),
                         shutil.which('codex', path=previous['PATH']))
        self.assertEqual(env['CODEX_HOME'], previous['CODEX_HOME'])
        self.assertNotIn('AGENTDESK_CLAUDE_PATH', previous)
        self.assertEqual(env['AGENTDESK_CLAUDE_PATH'], str(cli))
        probe = subprocess.run([env['AGENTDESK_CLAUDE_PATH'], '--version'], env=env,
                               capture_output=True, text=True, check=True)
        self.assertEqual(probe.stdout, 'fixture-claude-version\n')
        self.assertFalse((Path(manifest['root'])/'home/.adk/profiles/codex/test-codex/auth.json').exists())


    def test_opt_in_data_home_is_inside_instance(self):
        m = self.fixture.prepare(claude_login=self.login)
        data = Path(m['root'])/'home/.adk/profiles/claude/test-claude'
        self.assertEqual(m['claude_login']['config_dir'], str(data))
        self.assertEqual(child_environment(m, {})['CLAUDE_CONFIG_DIR'], str(data))
        self.assertEqual(self.login['config_dir'], str(self.fixture.home/'.claude'))

    def test_login_home_comes_from_the_binding_and_must_own_the_pool(self):
        with patch.object(Path, 'home', return_value=self.fixture.base/'runtime-home'):
            m = self.fixture.prepare(claude_login=self.login)
            self.assertEqual(m['claude_login']['login_home'], self.login['login_home'])
            self.assertEqual(child_environment(m, {})['CLAUDE_CONFIG_DIR'], m['claude_login']['config_dir'])
        foreign = self.fixture.home/'foreign'
        changed = {**self.login, 'login_home': str(foreign),
                   'config_dir': str(foreign/'.claude'), 'cli': str(foreign/'.local/bin/claude')}
        with self.assertRaises(InstanceError):
            from scripts.dev_instance.manifest import declared_manifest
            declared_manifest(self.fixture.base, m['instance_id'], self.fixture.worktree,
                              m['head'], m['profile'], m['api']['port'], m['postgres']['port'], changed)

    def test_legacy_global_binding_can_still_be_removed_without_launch(self):
        from scripts.dev_instance.down import remove
        m = self.fixture.prepare(claude_login=self.login)
        root = Path(m['root'])
        for name in ['manifest.json', 'receipt.json']:
            path = root/name
            value = json.loads(path.read_text())
            value['claude_login']['config_dir'] = self.login['config_dir']
            path.write_text(json.dumps(value))
        path = root/'config/org.yaml'
        org = json.loads(path.read_text())
        org['provider_auth_profiles']['test-claude']['home'] = self.login['config_dir']
        path.write_text(json.dumps(org))
        with patch.object(Path, 'home', return_value=self.fixture.home):
            self.assertEqual(remove(self.fixture.base, m['instance_id'], {})['status'], 'REMOVED')
        self.assertFalse(root.exists())
