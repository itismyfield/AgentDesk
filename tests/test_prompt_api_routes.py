import contextlib
import io
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from scripts.check_prompt_api_routes import check, inventory

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
        for expression in (
            '"http://localhost:1/api/"discord/send',
            "'http://localhost:1/api/'discord/send",
            '"http://localhost:1/api/"' + "'discord/send'",
            '"http://localhost:1/api/"$ROUTE',
            '"http://localhost:1/api/""$ROUTE"',
            '"$API"/api/discord/send',
            '$PREFIX"http://localhost:1/api/send"',
        ):
            with self.subTest(expression=expression):
                rc, output = self.run_check('curl ' + expression)
                self.assertEqual(rc, 0)
                self.assertIn('no_applicable_urls', output)
        self.assertEqual(self.run_check('curl "http://localhost:1/api/send"')[0], 1)

    def test_literal_prose_and_leading_quote_joins(self):
        prose = ['엔드포인트="URL"', 'POST 대상="URL"', '기본값:"URL"', '**"URL"** 로 호출',
                 '["URL"](x)', '| endpoint |"URL"|', '"URL"이 응답한다', 'URL은 "URL"다.',
                 "설정값='URL'", '~~~bash\ncurl "URL"이후\n~~~']
        joins = ["curl 'https://example.com/?next='\"URL\"", 'curl "https://example.com/?next="\'URL\'',
                 'curl "$API"\'URL\'', "curl ''\"URL\"", 'curl ""\'URL\'']
        for template, expected in [(x, 1) for x in prose] + [(x, 0) for x in joins]:
            with self.subTest(template=template):
                self.assertEqual(self.run_check(template.replace('URL', 'http://localhost:1/api/send'))[0], expected)

    def test_shell_context_and_join_boundaries(self):
        prose = ['curl 사용 시 엔드포인트="URL"', '`POST` 대상="URL"', '토큰은 $TOKEN 이고 기본값:"URL"',
                 'curl 설명: "URL"deprecated', "curl 문서에서 설정값='URL' 로 안내", '**"URL"** curl 로 호출',
                 '~~~sh\ncurl "URL"; echo ok\n~~~', '엔드포인트="URL"', "curl 'URL'.",
                 '~~~bash\n# 스테이징 대상은 "URL".\n~~~', '`ops` 라우트는 "URL".']
        joins = ['curl "prefix""URL"', 'curl "URL""suffix"', 'curl base:"URL"', 'curl $(base)"URL"']
        for template, expected in [(x, 1) for x in prose] + [(x, 0) for x in joins]:
            with self.subTest(template=template):
                self.assertEqual(self.run_check(template.replace('URL', 'http://localhost:1/api/send'))[0], expected)

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

    def test_staging_seam(self):
        deploy = (REPO / 'scripts/deploy-release.sh').read_text()
        seam = deploy.split('# Stage agent prompt files atomically', 1)[1].split('# Stage managed skills', 1)[0]
        seam = seam[seam.index('OBSIDIAN_DEFAULT_VAULT_ROOT='):]
        self.prompt.write_text('http://localhost:1/api/send')
        script = 'set -eu\nrsync() { cp "$2"/*.prompt.md "$3"; }\n' + seam + '\necho SENTINEL\n'
        env = dict(os.environ, HOME=str(self.root), ADK_REL=str(self.root / 'release'), REPO=str(REPO), AGENTDESK_OBSIDIAN_AGENTS_SRC=str(self.prompts))
        result = subprocess.run(['bash', '-c', script], env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('unknown_path /api/send', result.stdout)
        self.assertIn('Prompt API route inspection warning', result.stdout)
        self.assertIn('SENTINEL', result.stdout)
        env['AGENTDESK_OBSIDIAN_AGENTS_SRC'] = str(self.root / 'absent')
        result = subprocess.run(['bash', '-c', script], env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0)
        self.assertNotIn('inspected_files=', result.stdout)
        self.assertIn('SENTINEL', result.stdout)

    def test_ci_discovery(self):
        self.assertIn('tests/*.sh', (REPO / 'scripts/ci-script-checks.sh').read_text())
        self.assertIn('python3 -m unittest tests.test_prompt_api_routes', (REPO / 'tests/test_prompt_api_routes.sh').read_text())
        workflow = (REPO / '.github/workflows/ci-pr.yml').read_text()
        self.assertIn('scripts/**', workflow)
        self.assertIn('tests/**', workflow)
