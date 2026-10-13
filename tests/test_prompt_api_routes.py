import contextlib
import io
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import unittest
from scripts.check_prompt_api_routes import URL, check, inventory

REPO = Path(__file__).resolve().parents[1]


class PromptRoutes(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.parts = self.root / 'src/server/routes/docs/inventory/endpoints'
        self.parts.mkdir(parents=True)
        (self.parts / 'mod.rs').write_text('mod part_01;\n')
        self.part = self.parts / 'part_01.rs'
        self.part.write_text('ep("POST", "/api/discord/send", "ops", "description /api/send")\n.with_curl("/api/send")\n')
        self.prompts = self.root / 'prompts'
        self.prompts.mkdir()
        self.prompt = self.prompts / 'agent.prompt.md'

    def run_check(self, source, repo=None):
        self.prompt.write_text(source)
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            rc = check(repo or self.root, self.prompts)
        return rc, output.getvalue()

    def test_unknown_and_valid(self):
        rc, output = self.run_check('POST http://127.0.0.1:8791/api/send')
        self.assertEqual(rc, 1)
        self.assertIn('agent.prompt.md:1: unknown_path /api/send', output)
        self.assertEqual(self.run_check('http://127.0.0.1:8791/api/discord/send')[0], 0)

    def test_delimiters_privacy_and_dedup(self):
        rc, output = self.run_check('`http://localhost:1/api/send?token=SECRET` [x](https://[::1]:2/api/send#SECRET)')
        self.assertEqual(rc, 1)
        self.assertNotIn('SECRET', output)
        self.assertEqual(output.count('unknown_path /api/send'), 1)
        rc, output = self.run_check('<http://localhost:1/api/send?token=$TOKEN>')
        self.assertEqual(rc, 1)
        self.assertNotIn('TOKEN', output)

    def test_excluded_examples(self):
        source = '/api/send POST /api/send https://example.com:1/api/send http://localhost:$PORT/api/send\n'
        for fence in ('text', 'json', 'plaintext', 'python', ''):
            source += f'```{fence}\nhttp://localhost:1/api/send\n```\n'
        source += 'https://example.com/?next=http://localhost:1/api/send\n'
        source += '"http://localhost:1/api/" + route\nhttp://localhost:1/api/$ROUTE\n'
        source += '> http://localhost:1/api/send\n<!--\nhttp://localhost:1/api/send\n-->\n'
        rc, output = self.run_check(source)
        self.assertEqual(rc, 0)
        self.assertIn('no_applicable_urls', output)

    def test_shell_adjacent_strings(self):
        # r5: a bare word after the closing quote and any prefix no longer prove a join.
        for expression, expected in (
            ('"http://localhost:1/api/"discord/send', 1),
            ("'http://localhost:1/api/'discord/send", 1),
            ('"http://localhost:1/api/"' + "'discord/send'", 0),
            ('"http://localhost:1/api/"$ROUTE', 0),
            ('"http://localhost:1/api/""$ROUTE"', 0),
            ('"$API"/api/discord/send', 0),
            ('$PREFIX"http://localhost:1/api/send"', 1),
        ):
            with self.subTest(expression=expression):
                rc, output = self.run_check('curl ' + expression)
                self.assertEqual(rc, expected)
                self.assertIn('unknown_path' if expected else 'no_applicable_urls', output)
        self.assertEqual(self.run_check('curl "http://localhost:1/api/send"')[0], 1)

    def test_literal_prose_and_leading_quote_joins(self):
        prose = ['엔드포인트="URL"', 'POST 대상="URL"', '기본값:"URL"', '**"URL"** 로 호출',
                 '["URL"](x)', '| endpoint |"URL"|', '"URL"이 응답한다', 'URL은 "URL"다.',
                 "설정값='URL'", '~~~bash\ncurl "URL"이후\n~~~']
        leading = ["curl 'https://example.com/?next='\"URL\"", 'curl "https://example.com/?next="\'URL\'',
                   'curl "$API"\'URL\'', "curl ''\"URL\"", 'curl ""\'URL\'']
        # r5: the token after a leading quote is itself complete, so these count too.
        for template, expected in [(x, 1) for x in prose + leading]:
            with self.subTest(template=template):
                self.assertEqual(self.run_check(template.replace('URL', 'http://localhost:1/api/send'))[0], expected)

    def test_shell_context_and_join_boundaries(self):
        prose = ['curl 사용 시 엔드포인트="URL"', '`POST` 대상="URL"', '토큰은 $TOKEN 이고 기본값:"URL"',
                 'curl 설명: "URL"deprecated', "curl 문서에서 설정값='URL' 로 안내", '**"URL"** curl 로 호출',
                 '~~~sh\ncurl "URL"; echo ok\n~~~', '엔드포인트="URL"', "curl 'URL'.",
                 '~~~bash\n# 스테이징 대상은 "URL".\n~~~', '`ops` 라우트는 "URL".']
        prose += ['curl "prefix""URL"', 'curl base:"URL"', 'curl $(base)"URL"']  # r5: prefixes are literals
        joins = ['curl "URL""suffix"']
        for template, expected in [(x, 1) for x in prose] + [(x, 0) for x in joins]:
            with self.subTest(template=template):
                self.assertEqual(self.run_check(template.replace('URL', 'http://localhost:1/api/send'))[0], expected)

    def test_client_name_prose_and_path_continuation(self):
        prose = ['curl usage: endpoint="URL"', 'wget documentation: default="URL"',
                 'http endpoint="URL" is the recommended route.', 'xh docs: target="URL"',
                 'curl example: "URL"deprecated']
        joins = ['curl --header "X-Name: 한글" "prefix""URL"/tail', 'curl "URL"/suffix # 한글 설명',
                 'curl "http://localhost:1/api"$REST']
        fenced = ['```sh\n' + template + '\n```' for template in prose]
        for template, expected in [(x, 1) for x in prose + fenced] + [(x, 0) for x in joins]:
            with self.subTest(template=template):
                self.assertEqual(self.run_check(template.replace('URL', 'http://localhost:1/api/send'))[0], expected)

    def test_quoted_span_second_token(self):
        # r6: a trailing quote closes the URL's own span only while that quote is still
        # open, so a URL that is not the span's first token stays a candidate.
        for template in ('- ADK API 사용: "POST URL"', '상태 확인은 "curl URL" 로 한다.',
                         'Run "curl URL" first.', "상태 확인은 'curl URL' 로 한다.",
                         '```sh\necho "curl URL"\n```',
                         '"http://localhost:1/api/discord/send 와 URL"'):
            with self.subTest(template=template):
                rc, output = self.run_check(template.replace('URL', 'http://localhost:1/api/send'))
                self.assertEqual(rc, 1)
                self.assertIn('unknown_path /api/send', output)

    def test_apostrophe_parity_traps_and_shell_joins(self):
        # r7: an apostrophe or an escaped quote earlier in the line must not flip the
        # closing-quote verdict and erase a complete literal URL token, while genuine
        # shell assembly past that quote stays excluded.
        literals = ["The agent's endpoint is 'URL'.", "Don't call 'URL' directly.",
                    "Both agents' route is 'URL'.", '```sh\necho "curl \\"URL"\n```',
                    '"90\'s" \'URL\'']
        joins = ['curl "http://localhost:1/api/"$SUFFIX', 'curl \'http://localhost:1/api/\'"$X"',
                 'Don\'t run: curl "http://localhost:1/api/"$SUFFIX']
        for template, expected in [(x, 1) for x in literals] + [(x, 0) for x in joins]:
            source = template.replace('URL', 'http://localhost:1/api/send')
            with self.subTest(template=template):
                self.assertTrue(URL.search(source), 'vacuous case: no URL token matched')
                rc, output = self.run_check(source)
                self.assertEqual(rc, expected)
                self.assertIn('unknown_path /api/send' if expected else 'no_applicable_urls', output)

    def test_fence_closing_contract(self):
        for marker in ('~~~', '```', '````bash'):
            with self.subTest(marker=marker):
                rc, output = self.run_check(
                    '````text\n' + marker + '\nhttp://localhost:1/api/send\n````')
                self.assertEqual(rc, 0)
                self.assertIn('no_applicable_urls', output)

    def test_executable_fences_and_line_numbers(self):
        for fence in ('sh', 'bash', 'shell'):
            rc, output = self.run_check(f'<!-- hidden\ncomment -->\n~~~{fence}\ncurl http://localhost:1/api/send\n~~~\nhttp://localhost:1/api/send')
            self.assertEqual(rc, 1)
            self.assertIn('agent.prompt.md:4:', output)
            self.assertIn('agent.prompt.md:6:', output)

    def test_parameter_fullmatch(self):
        self.part.write_text('ep("GET", "/api/item/{id}", "", "")\nep("GET", "/api/files/{*rest}", "", "")\n')
        for path, expected in [('item/abc', 0), ('item/', 1), ('item/a/b', 1), ('items/a', 1), ('files/a/b', 0), ('files/', 1)]:
            with self.subTest(path=path):
                self.assertEqual(self.run_check('http://localhost:1/api/' + path)[0], expected)

    def test_unavailable(self):
        for source in ('ep("GET", "/api/health", "", "")\nep(METHOD_CONST, "/api/discord/send", "", "")', '', 'ep(dynamic, "/api/send", "", "")', 'ep("GET", r"/api/send", "", "")'):
            self.part.write_text(source)
            rc, output = self.run_check('http://localhost:1/api/discord/send')
            self.assertEqual(rc, 2)
            self.assertIn('inspection_unavailable', output)
        self.part.unlink()
        self.assertEqual(self.run_check('')[0], 2)
        (self.parts / 'mod.rs').write_text('')
        self.assertEqual(self.run_check('')[0], 2)
        self.assertEqual(self.run_check('', self.root / 'missing')[0], 2)

    def test_flat_symlink_and_readonly(self):
        self.prompt.write_text('http://localhost:1/api/discord/send')
        (self.prompts / 'alias.prompt.md').symlink_to(self.root / 'nonexistent')
        (self.prompts / 'subdir').mkdir()
        (self.prompts / 'subdir/bad.prompt.md').write_text('http://localhost:1/api/send')
        (self.prompts / '_shared.md').write_text('http://localhost:1/api/send')
        before = (self.prompt.read_bytes(), self.prompt.stat())
        with contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(check(self.root, self.prompts), 0)
        after = (self.prompt.read_bytes(), self.prompt.stat())
        self.assertEqual(before[0], after[0])
        for field in ('st_mtime_ns', 'st_ctime_ns', 'st_mode', 'st_size', 'st_ino'):
            self.assertEqual(getattr(before[1], field), getattr(after[1], field))
        self.assertIn('skipped_files=1', output.getvalue())
        self.prompt.write_bytes(b'\xff')
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(check(self.root, self.prompts), 2)

    def test_actual_inventory(self):
        routes = inventory(REPO)
        self.assertTrue(any(route.fullmatch('/api/discord/send') for route in routes))
        self.assertFalse(any(route.fullmatch('/api/send') for route in routes))
        self.assertIn('.route("/discord/send",', (REPO / 'src/server/routes/domains/ops.rs').read_text())

    def run_staging(self, *, strict=False, boundary='direct', source='configured', deploy=None):
        deploy = deploy or Path(os.environ.get('AGENTDESK_TEST_DEPLOY_SH', REPO / 'scripts/deploy-release.sh')).read_text()
        parser = deploy.split('# Parse flags non-destructively', 1)[1].split('case "$DEPLOY_FAST"', 1)[0]
        parser = parser[parser.index('\n'):]
        seam = deploy.split('# Stage agent prompt files atomically', 1)[1].split('# Stage managed skills', 1)[0]
        seam = seam[seam.index('OBSIDIAN_DEFAULT_VAULT_ROOT='):]
        release = self.root / 'release'
        staged = release / 'config/agents.new'
        for directory in (staged, release / 'runtime/self_hosted_deploy'):
            if directory.exists():
                shutil.rmtree(directory)
        scripts = self.root / 'scripts'
        scripts.mkdir(exist_ok=True)
        shutil.copyfile(REPO / 'scripts/check_prompt_api_routes.py', scripts / 'check_prompt_api_routes.py')
        events = self.root / 'events'
        events.write_text('')
        variables = dict(ADK_REL=release, REPO=self.root, SCRIPT_DIR=scripts, DEPLOY_FAST='0',
                         DEPLOY_LOCK_FILE=release / 'deploy.lock', DEPLOY_LOCK_TIMEOUT_SECS='1',
                         REPORT_CHANNEL_ID='fixture', REPORT_PROVIDER='fixture', DEPLOY_DELAY_SECS='0',
                         DEPLOY_TEST_MODE='1', POST_DEPLOY_SMOKE_SCOPE='api', BUNDLE_ID='fixture',
                         PLIST_REL='fixture', DEPLOY_PEER_VERDICT_TIMEOUT_SECS='1',
                         DEPLOY_PEER_VERDICT_POLL_INTERVAL_SECS='1')
        script = '#!/bin/bash\nset -euo pipefail\n'
        script += ''.join(f'{key}={shlex.quote(str(value))}\n' for key, value in variables.items())
        script += parser + '\n'
        for name in ('_acquire_release_deploy_lock', '_spawn_detached_helper'):
            script += name + '() {' + deploy.split(name + '() {', 1)[1].split('\n}\n', 1)[0] + '\n}\n'
        script += '''
rm() { [ "$*" = "-rf $ADK_REL/config/agents.new" ]; }
rsync() { printf 'STAGED\n' >> "$REPO/events"; cp "$2"/*.prompt.md "$3"; }
date() { printf 'fixture-time\n'; }
tmux() {
    [ "$#" = 5 ] && [ "$1 $2 $3" = 'new-session -d -s' ]
    printf 'HELPER\n' >> "$REPO/events"
    env -i HOME="$HOME" PATH="$PATH" /bin/bash "$5"
}
'''
        lock = 'flock' if boundary == 'flock' else 'lockf'
        script += f'''command() {{
    if [ "$1" = -v ] && {{ [ "$2" = lockf ] || [ "$2" = flock ]; }}; then
        [ "$2" = {lock} ]; return
    fi
    builtin command "$@"
}}
'''
        for name, count in (('lockf', 4), ('flock', 3)):
            stub = scripts / name
            stub.write_text(f'#!/bin/bash\nprintf "{name}\\n" >> {shlex.quote(str(events))}\nshift {count}\nexec "$@"\n')
            stub.chmod(0o755)
        if boundary == 'helper':
            script += 'if [ "${AGENTDESK_DEPLOY_DETACHED_CHILD:-0}" != 1 ]; then\n_spawn_detached_helper "$@"\nexit 0\nfi\n'
        if boundary != 'direct':
            script += '_acquire_release_deploy_lock "$@"\n'
        script += seam + '\nprintf "ACTIVATE\\nRESTART\\n" >> "$REPO/events"\necho SENTINEL\n'
        fixture = scripts / 'deploy-release.sh'
        fixture.write_text(script)
        fixture.chmod(0o755)
        env = dict(PATH=str(scripts) + os.pathsep + os.environ['PATH'], HOME=str(self.root))
        if source == 'configured':
            env['AGENTDESK_OBSIDIAN_AGENTS_SRC'] = str(self.prompts)
        elif source == 'missing':
            env['AGENTDESK_OBSIDIAN_AGENTS_SRC'] = str(self.root / 'absent')
        elif source == 'empty':
            env['AGENTDESK_OBSIDIAN_AGENTS_SRC'] = ''
        result = subprocess.run(['bash', str(fixture), *(['--strict-prompts'] if strict else [])],
                                env=env, capture_output=True, timeout=10)
        if boundary == 'helper':
            result.stdout += b''.join(p.read_bytes() for p in sorted(release.glob('runtime/self_hosted_deploy/*.log')))
        self.events = events.read_text().splitlines()
        return result

    def test_staging_seam(self):
        for contents, diagnostic in (
            (b'http://localhost:1/api/send', 'agent.prompt.md:1: unknown_path /api/send\n'
             'inspected_files=1 candidate_urls=1 unknown_paths=1 skipped_files=0\n'),
            (b'\xff', 'inspection_unavailable\n'),
        ):
            with self.subTest(contents=contents):
                self.prompt.write_bytes(contents)
                result = self.run_staging()
                expected = ('▸ Staging agent prompts from Obsidian vault...\n' + diagnostic
                            + '⚠ Prompt API route inspection warning; core release deploy will continue.\nSENTINEL\n')
                self.assertEqual((result.returncode, result.stdout, result.stderr), (0, expected.encode(), b''))
                self.assertEqual(self.events, ['STAGED', 'ACTIVATE', 'RESTART'])

    def test_strict_staging_failures(self):
        for contents, rc, diagnostic, reason in (
            (b'http://localhost:1/api/send', 1, b'unknown_path /api/send', b'unknown paths'),
            (b'\xff', 2, b'inspection_unavailable', b'inspection unavailable'),
        ):
            with self.subTest(rc=rc):
                self.prompt.write_bytes(contents)
                result = self.run_staging(strict=True)
                self.assertEqual(result.returncode, rc, result)
                self.assertIn(diagnostic, result.stdout)
                self.assertIn(reason, result.stdout)
                self.assertIn(b'--strict-prompts', result.stdout)
                self.assertNotIn(b'will continue', result.stdout)
                self.assertEqual(self.events, ['STAGED'])
                self.assertEqual((self.root / 'release/config/agents.new/agent.prompt.md').read_bytes(), contents)

    def test_strict_staging_success(self):
        for contents, summary in (('http://localhost:1/api/discord/send', b'candidate_urls=1'),
                                  ('no local URL', b'no_applicable_urls')):
            with self.subTest(contents=contents):
                self.prompt.write_text(contents)
                result = self.run_staging(strict=True)
                self.assertEqual(result.returncode, 0, result)
                self.assertIn(summary, result.stdout)
                self.assertEqual(self.events, ['STAGED', 'ACTIVATE', 'RESTART'])

    def test_strict_flag_survives_reexec_and_helper(self):
        self.prompt.write_text('http://localhost:1/api/send')
        for boundary, events in (('lockf', ['lockf', 'STAGED']), ('flock', ['flock', 'STAGED']),
                                 ('helper', ['HELPER', 'lockf', 'STAGED'])):
            with self.subTest(boundary=boundary):
                result = self.run_staging(strict=True, boundary=boundary)
                self.assertEqual(result.returncode, 1, result)
                self.assertIn(b'unknown_path /api/send', result.stdout)
                self.assertIn(b'--strict-prompts', result.stdout)
                self.assertEqual(self.events, events)

    def test_optional_prompt_source_skip(self):
        mirror = self.root / 'release/config/agents/old.prompt.md'
        mirror.parent.mkdir(parents=True)
        mirror.write_bytes(b'original mirror')
        self.part.unlink()
        for strict in (False, True):
            for source in ('unset', 'empty', 'missing'):
                with self.subTest(strict=strict, source=source):
                    result = self.run_staging(strict=strict, source=source)
                    explicit = source == 'missing'
                    path = self.root / ('absent' if explicit else 'ObsidianVault/RemoteVault/adk-config/agents')
                    state = 'missing_path' if explicit else 'missing_config'
                    prefix = '⚠ Optional connector obsidian_agent_prompts invalid' if explicit else 'ℹ Optional connector obsidian_agent_prompts skipped'
                    expected = (f'{prefix}: {path}\n  state={state} reason={state}; core release deploy will continue.\n'
                                f'  Existing {mirror.parent}/ will be retained.\nSENTINEL\n')
                    self.assertEqual((result.returncode, result.stdout, result.stderr), (0, expected.encode(), b''))
                    self.assertEqual(self.events, ['ACTIVATE', 'RESTART'])
                    self.assertEqual(mirror.read_bytes(), b'original mirror')
                    self.assertFalse((mirror.parent.parent / 'agents.new').exists())

    def test_unset_prompt_override_checks_existing_fallback(self):
        fallback = self.root / 'ObsidianVault/RemoteVault/adk-config/agents'
        fallback.mkdir(parents=True)
        (fallback / 'agent.prompt.md').write_text('http://localhost:1/api/send')
        result = self.run_staging(strict=True, source='unset')
        self.assertEqual(result.returncode, 1, result)
        self.assertIn(b'unknown_path /api/send', result.stdout)
        self.assertEqual(self.events, ['STAGED'])
