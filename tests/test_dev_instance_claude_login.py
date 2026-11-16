"""Explicit default Claude login binding, without reading login material."""
import copy
import hashlib
import json
import stat
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

    def _prepare_golden(self, manifest):
        root = Path(manifest['root'])
        replacements = [(str(root), '<root>'), (str(self.fixture.worktree), '<worktree>'),
                        (str(self.fixture.home), '<home>'), (manifest['instance_id'], '<uuid>'),
                        (manifest['instance_id'].replace('-', ''), '<uuid-hex>'),
                        (manifest['head'], '<head>'), (str(manifest['api']['port']), '<api>'),
                        (str(manifest['postgres']['port']), '<pg>')]
        def normalize(value):
            for original, replacement in replacements:
                value = value.replace(original, replacement)
            return value
        entries = []
        for path in sorted(root.rglob('*')):
            metadata = path.lstat()
            self.assertFalse(path.is_symlink())
            entries.append([str(path.relative_to(root)), stat.S_IMODE(metadata.st_mode),
                            'directory' if path.is_dir() else normalize(path.read_text())])
        snapshot = {'files': entries, 'env': {key: normalize(value) for key, value in child_environment(manifest, {}).items()}}
        return hashlib.sha256(json.dumps(snapshot, sort_keys=True).encode()).hexdigest()

    def test_explicit_prepare_seeds_only_private_nonsecret_claude_state(self):
        manifest = self.fixture.prepare(claude_login=self.login)
        root = Path(manifest['root'])
        path = Path(manifest['claude_login']['config_dir'])/'.claude.json'
        self.assertTrue(path.is_relative_to(root))
        self.assertEqual(json.loads(path.read_text()), {'hasCompletedOnboarding': True, 'theme': 'dark',
            'projects': {str(root/'workspaces/claude'): {'hasTrustDialogAccepted': True}}})
        settings = path.with_name('settings.json')
        self.assertEqual(json.loads(settings.read_text()), {'skipDangerousModePermissionPrompt': True})
        self.assertEqual(stat.S_IMODE(settings.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        self.assertEqual(list((root/'credential').iterdir()), [])
        self.assertEqual(list((root/'home/.adk/profiles/codex/test-codex').iterdir()), [])
        self.assertFalse((self.fixture.home/'.claude').exists())
        self.assertFalse(list((root/'home').rglob('.credentials.json')))
        self.assertEqual(validate(self.fixture.base, manifest['instance_id'], {})['claude_login'], manifest['claude_login'])

    def test_no_opt_in_preserves_original_prepare_files_and_environment_golden(self):
        manifest = self.fixture.prepare()
        self.assertFalse(list(Path(manifest['root']).rglob('.claude.json')))
        self.assertEqual(self._prepare_golden(manifest), 'c297bab0ed6fd05a1bac81549ec373a92a8be433652a6b02ede6d658ad13a782')

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
        from scripts.dev_instance.config import claude_runtime_path
        self.assertEqual(env['PATH'], claude_runtime_path(self.fixture.worktree, True))
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
        import sys
        expected_path = previous['PATH']
        if sys.platform == 'darwin':
            formula = next(p for p in (Path('/opt/homebrew/opt/tmux/bin'), Path('/usr/local/opt/tmux/bin')) if p.exists())
            expected_path += ':'+str(formula.resolve(strict=True))
        self.assertEqual(env['PATH'], expected_path)
        self.assertNotIn('/opt/homebrew/bin', env['PATH'].split(':'))
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

    def test_opt_in_exposes_only_the_dedicated_tmux_directory(self):
        import shutil
        import subprocess
        directory = self.fixture.home/'formula/tmux/bin'
        directory.mkdir(parents=True)
        binary = directory/'tmux'
        binary.write_text('#!/bin/sh\nprintf "fixture-tmux\\n"\n')
        binary.chmod(0o700)
        outside = self.fixture.home/'.local/bin/codex'
        outside.parent.mkdir(parents=True)
        outside.write_text('#!/bin/sh\nexit 99\n')
        outside.chmod(0o700)
        m = self.fixture.prepare(claude_login=self.login)
        with patch('scripts.dev_instance.config.sys.platform', 'darwin'), patch('scripts.dev_instance.config.TMUX_FORMULA_BINS', (directory,), create=True):
            env = child_environment(m, {})
            self.assertEqual(env['PATH'], str(self.fixture.worktree/'target/debug')+':/usr/bin:/bin:'+str(directory))
            self.assertEqual(shutil.which('tmux', path=env['PATH']), str(binary))
            self.assertIsNone(shutil.which('codex', path=env['PATH']))
            result = subprocess.run(['tmux', '--version'], env=env, capture_output=True, text=True, check=True)
            self.assertEqual(result.stdout, 'fixture-tmux\n')
            (directory/'codex').write_text('unexpected executable')
            with self.assertRaises(InstanceError):child_environment(m, {})
