"""Loopback HTTP and SELECT fixtures; never contact Discord or an operational DB."""
import ast
from contextlib import contextmanager
import copy
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import unittest
from unittest.mock import patch

from tests import test_n1_canary_oracle as fixture_helpers
from tests.test_n1_canary_oracle import CONTRACT, ENTRY, PACKAGE, ROOT

sys.path.insert(0, str(ROOT / "scripts"))
from n1_canary import Unknown, loads
from n1_canary.discord import collect, getter, history_valid
from n1_canary.evidence import PG_SELECT, pg_select

SCOPE = json.loads(CONTRACT.read_text())["discord"]


def message(identity, channel="123", author="456"):
    return {"id": str(identity), "channel_id": channel, "author": {"id": author}, "content": "fixture"}


@contextmanager
def server(replies):
    requests = []

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            requests.append((self.command, self.path))
            status, payload = replies.pop(0) if replies else (500, {})
            self.send_response(status)
            if status == 302:
                self.send_header("Location", "/must-not-follow")
            self.end_headers()
            self.wfile.write(payload if isinstance(payload, bytes) else json.dumps(payload).encode())

        def log_message(self, *args):
            pass

    httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=httpd.serve_forever)
    thread.start()
    try:
        yield f"http://127.0.0.1:{httpd.server_port}/api/v10", requests
    finally:
        httpd.shutdown()
        httpd.server_close()
        thread.join()


class DiscordContracts(unittest.TestCase):
    def collect_replies(self, replies, scope=SCOPE):
        with server(replies) as (base, requests):
            result = collect(scope, getter(base, None))
        self.assertTrue(all(method == "GET" for method, _ in requests))
        return result, requests

    def test_page_cursor_boundary_and_individual_target(self):
        result, requests = self.collect_replies([(200, [message(400), message(200)]),
                                                 (200, [message(150), message(90)]),
                                                 (200, message(200))])
        self.assertTrue(history_valid(result, SCOPE))
        self.assertEqual([m["id"] for m in result["messages"]], ["400", "200", "150"])
        self.assertEqual([p for _, p in requests], [
            "/api/v10/channels/123/messages?limit=100&before=500",
            "/api/v10/channels/123/messages?limit=100&before=200",
            "/api/v10/channels/123/messages/200"])

    def test_short_page_requires_terminal_boundary_or_empty_page(self):
        result, _ = self.collect_replies([(200, [message(200)]), (429, {})])
        self.assertFalse(result["complete"])
        self.assertIn("discord_rate_limited", result["reasons"])
        self.assertEqual(result["pages"], 1)
        self.assertEqual(len(result["messages"]), 1)

    def test_page_boundaries_limits_and_http_errors_are_unknown(self):
        cases = [([(200, [message(500)])], "discord_page_boundary"),
                 ([(200, [message(200), message(200)])], "discord_page_boundary"),
                 ([(200, [message(150), message(200)])], "discord_page_boundary"),
                 ([(200, [message(200)]), (200, [message(200)])], "discord_page_boundary"),
                 ([(403, {})], "discord_http_error"), ([(500, {})], "discord_http_error"),
                 ([(429, {})], "discord_rate_limited"), ([(302, {})], "discord_http_error"),
                 ([(200, b"bad json")], "discord_response_unavailable"),
                 ([(200, [message(200, author="")])], "discord_page_invalid")]
        for replies, reason in cases:
            with self.subTest(reason=reason, replies=replies):
                result, _ = self.collect_replies(copy.deepcopy(replies))
                self.assertFalse(result["complete"])
                self.assertIn(reason, result["reasons"])
                self.assertFalse(history_valid(result, SCOPE))

    def test_page_budget_partial_and_individual_404(self):
        result, _ = self.collect_replies([(200, [message(200)])], {**SCOPE, "max_pages": 1})
        self.assertFalse(result["complete"])
        self.assertIn("discord_partial_collection", result["reasons"])
        result, _ = self.collect_replies([(200, []), (404, {})])
        self.assertFalse(result["complete"])
        self.assertIn("discord_http_error", result["reasons"])
        result, _ = self.collect_replies([(200, []), (200, message(201))])
        self.assertFalse(result["complete"])
        self.assertIn("discord_individual_invalid", result["reasons"])

    def test_endpoint_guard_rejects_non_loopback_standins(self):
        for endpoint in ("http://example.com/api/v10", "https://discord.com.evil/api/v10",
                         "https://discord.com/api/v10?secret=x", "http://user@127.0.0.1/api/v10"):
            with self.subTest(endpoint=endpoint), self.assertRaises(Unknown):
                getter(endpoint, None)

    def test_json_duplicates_and_nonfinite_numbers_are_unknown(self):
        for raw in (b'{"a":1,"a":2}', b'{"a":NaN}'):
            with self.assertRaises(Unknown):
                loads(raw)


class PGContracts(unittest.TestCase):
    def test_select_only_query_and_readonly_connection_are_executed(self):
        scope = json.loads(CONTRACT.read_text())["scope"]
        expected = {"intake_outbox": [], "turn_lifecycle_events": []}
        with patch.dict(os.environ, {"N1_FIXTURE_DSN": "fixture-secret"}), patch(
                "n1_canary.evidence.subprocess.run",
                return_value=subprocess.CompletedProcess([], 0, json.dumps(expected), "")) as run:
            self.assertEqual(pg_select("N1_FIXTURE_DSN", scope), expected)
        args, kwargs = run.call_args
        self.assertEqual(args[0][0], "psql")
        self.assertIn("-X", args[0])
        self.assertNotIn("fixture-secret", args[0])
        self.assertEqual(kwargs["input"], PG_SELECT)
        self.assertTrue(PG_SELECT.startswith("SELECT "))
        self.assertNotRegex(PG_SELECT.upper(), r"\b(INSERT|UPDATE|DELETE|CREATE|DROP|ALTER|COPY|CALL)\b")
        self.assertEqual(kwargs["env"]["PGOPTIONS"], "-c default_transaction_read_only=on -c statement_timeout=10000")
        self.assertEqual(kwargs["env"]["PGDATABASE"], "fixture-secret")

    def test_failure_parse_error_missing_dsn_are_unknown(self):
        scope = json.loads(CONTRACT.read_text())["scope"]
        for outcome in (OSError(), subprocess.TimeoutExpired("psql", 20),
                        subprocess.CalledProcessError(1, "psql"),
                        subprocess.CompletedProcess([], 0, "bad json", ""),
                        subprocess.CompletedProcess([], 0, "null", "")):
            with self.subTest(outcome=outcome), patch.dict(os.environ, {"N1_FIXTURE_DSN": "fixture"}):
                with patch("n1_canary.evidence.subprocess.run", **(
                        {"side_effect": outcome} if isinstance(outcome, Exception) else {"return_value": outcome})):
                    with self.assertRaises(Unknown):
                        pg_select("N1_FIXTURE_DSN", scope)
        with patch.dict(os.environ, {}, clear=True), self.assertRaises(Unknown):
            pg_select("N1_FIXTURE_DSN", scope)


class CLIFixture(unittest.TestCase):
    setUp = fixture_helpers.Fixture.setUp
    save = fixture_helpers.Fixture.save
    cli = fixture_helpers.Fixture.cli

    def test_discord_loopback_failure_reaches_cli_unknown(self):
        with server([(429, {})]) as (base, requests):
            output = self.cli("--discord-base-url", base)
        self.assertIn("discord_rate_limited", output["reasons"])
        self.assertIn("discord_incomplete", output["reasons"])
        self.assertEqual(len(requests), 1)

    def test_partial_history_fixture_never_establishes_absence(self):
        self.values["discord"]["complete"] = False
        self.save()
        self.assertIn("discord_incomplete", self.cli()["reasons"])

    def test_complete_marker_cannot_replace_page_closure_proof(self):
        self.values["discord"]["page_boundaries"] = [{"channel_id": 123, "before": "500", "ids": ["200"]}]
        self.values["discord"]["messages"] = [message(200)]
        self.save()
        self.assertIn("discord_incomplete", self.cli()["reasons"])


class EffectBoundary(unittest.TestCase):
    def test_production_has_only_audited_io(self):
        self.assertTrue(ENTRY.is_file(), "required CLI entry is missing")
        for path in [ENTRY, *PACKAGE.glob("*.py")]:
            tree = ast.parse(path.read_text())
            for node in ast.walk(tree):
                if not isinstance(node, ast.Call):
                    continue
                name = ast.unparse(node.func)
                self.assertNotIn(name.split(".")[-1], {"unlink", "remove", "rmdir", "rmtree", "write_bytes", "write_text",
                                 "system", "popen", "connect", "execute", "executemany", "commit", "send",
                                 "post", "patch", "delete", "rename", "mkdir", "makedirs", "truncate", "touch"})
                if name.endswith("Request"):
                    self.assertEqual({k.arg: ast.literal_eval(k.value) for k in node.keywords if k.arg == "method"}, {"method": "GET"})
                if name.endswith(".open") and name != "opener.open":
                    self.assertEqual(ast.literal_eval(node.args[0]), "x")
                if name == "subprocess.run":
                    self.assertEqual(path.name, "evidence.py")
                    self.assertNotIn("shell", {k.arg for k in node.keywords})

    def test_ci_registration_is_in_a_required_shard(self):
        script = (ROOT / "scripts/ci-script-checks.sh").read_text()
        expected = ('if run_check guards "N1 canary oracle input contracts"; then\n'
                    '"$PYTHON" -m unittest tests.test_n1_canary_oracle tests.test_n1_canary_collection\n'
                    '"$PYTHON" tests/run_n1_canary_mutations.py\nfi')
        self.assertTrue(expected in script, "oracle tests must execute in the guards shard")


if __name__ == "__main__":
    unittest.main()
