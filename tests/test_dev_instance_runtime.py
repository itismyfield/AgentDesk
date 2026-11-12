"""Exercise instance worker re-entry through the built lane executable."""

import copy
from pathlib import Path
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import queue
import subprocess
import threading
import time
import unittest

from tests import test_dev_instance as declaration_fixture
from tests.test_dev_instance_credential import require_lane_binary
from scripts.dev_instance.manifest import child_environment


class InstanceWorkerCliTests(unittest.TestCase):
    def test_hook_workers_deliver_drain_and_publish_failure_under_the_instance_root(self):
        binary = require_lane_binary(self) / "agentdesk"
        fixture = declaration_fixture.InstanceDeclarationTests()
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        manifest = fixture.prepare()
        root = fixture.base / fixture.instance_id
        received = queue.Queue()

        class Receiver(BaseHTTPRequestHandler):
            def do_POST(self):
                payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                received.put((self.path, payload))
                status = 500 if "/UserPromptSubmit?" in self.path else 200
                self.send_response(status)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"{}")

            def log_message(self, *_args):
                pass

        server = HTTPServer(("127.0.0.1", 0), Receiver)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()

        def close_receiver():
            server.shutdown()
            server.server_close()
            worker.join(timeout=5)
            self.assertFalse(worker.is_alive(), "private receiver did not stop")

        self.addCleanup(close_receiver)
        endpoint = f"http://127.0.0.1:{server.server_port}"
        environment = child_environment(manifest, {})

        def relay(event):
            result = subprocess.run(
                [str(binary), "claude-hook-relay", "--endpoint", endpoint,
                 "--provider", "claude", "--event", event,
                 "--session-id", "private-worker-session"],
                env=environment, input='{"prompt":"synthetic hook input"}',
                text=True, capture_output=True, timeout=15, check=False)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIsInstance(json.loads(result.stdout), dict)
            try:
                path, payload = received.get(timeout=5)
            except queue.Empty:
                self.fail("validated instance worker did not reach the private receiver")
            self.assertEqual(path, f"/hooks/claude/{event}?session_id=private-worker-session")
            self.assertEqual(payload["prompt"], "synthetic hook input")

        def wait_for(predicate, message):
            deadline = time.monotonic() + 5
            while not predicate():
                remaining = deadline - time.monotonic()
                self.assertGreater(remaining, 0, message)
                threading.Event().wait(min(0.01, remaining))

        relay("PostToolUse")
        relay("UserPromptSubmit")
        markers = root / "runtime/claude_tui_hook_relay_failures"
        wait_for(lambda: markers.exists() and bool(list(markers.glob("*.json"))),
                 "self-exec failure worker did not publish its marker")
        published = [json.loads(path.read_text()) for path in markers.glob("*.json")]
        self.assertEqual(len(published), 1)
        marker = published[0]
        self.assertEqual((marker["provider"], marker["event"], marker["session_id"], marker["endpoint"]),
                         ("claude", "UserPromptSubmit", "private-worker-session", endpoint))
        self.assertIn("HTTP 500", marker["error"])
        queues = root / "runtime/claude_tui_hook_relay_queue"
        wait_for(lambda: not list(queues.rglob("*.ingress.json"))
                 and not list(queues.rglob("*.request.json"))
                 and not list(queues.rglob("*.response.json")),
                 "instance hook queue did not drain")


class InstanceEnvironmentCliTests(unittest.TestCase):
    def setUp(self):
        self.binary = require_lane_binary(self) / "agentdesk"
        self.fixture = declaration_fixture.InstanceDeclarationTests()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.manifest = self.fixture.prepare()
        self.root = Path(self.manifest["root"])
        self.environment = child_environment(self.manifest, {})

    def run_cli(self, *arguments, environment=None):
        return subprocess.run(
            [str(self.binary), *arguments],
            env=self.environment if environment is None else environment,
            text=True, capture_output=True, check=False, timeout=15)

    def test_startup_rejects_home_api_label_session_and_memento_drift(self):
        positive = self.run_cli("--version")
        self.assertEqual(positive.returncode, 0, positive.stderr)
        for key, bad in (
            ("HOME", str(self.fixture.home)), ("TMPDIR", str(self.fixture.home)),
            ("XDG_CONFIG_HOME", str(self.fixture.home)),
            ("CLAUDE_CONFIG_DIR", str(self.fixture.home)),
            ("CODEX_HOME", str(self.fixture.home)),
            ("AGENTDESK_DCSERVER_LABEL", "com.agentdesk.release"),
            ("AGENTDESK_RELEASE_TMUX_SESSION", "AgentDesk-dcserver"),
            ("AGENTDESK_API_URL", "http://127.0.0.1:8791"),
            ("ADK_API_URL", "http://127.0.0.1:8791"),
            ("MEMENTO_ACCESS_KEY", declaration_fixture.SENTINEL),
        ):
            for remove in ((False,) if key == "MEMENTO_ACCESS_KEY" else (False, True)):
                with self.subTest(key=key, missing=remove):
                    environment = dict(self.environment)
                    if remove:
                        del environment[key]
                    else:
                        environment[key] = bad
                    result = self.run_cli("--version", environment=environment)
                    self.assertNotEqual(result.returncode, 0, key)
                    self.assertIn("test instance", result.stderr)
                    self.assertNotIn(declaration_fixture.SENTINEL, result.stdout + result.stderr)
        config_path = self.root / "config/agentdesk.yaml"
        original = config_path.read_text()
        config = json.loads(original)
        for mutate in (
            lambda value: value["server"].update(port=8791),
            lambda value: value["mcp_servers"].update(memento={"url": "http://127.0.0.1:57332/mcp"}),
            lambda value: value["memory"]["mcp"].update(endpoint="http://127.0.0.1:57332/mcp"),
        ):
            changed = copy.deepcopy(config)
            mutate(changed)
            config_path.write_text(json.dumps(changed))
            try:
                result = self.run_cli("--version")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("test instance", result.stderr)
            finally:
                config_path.write_text(original)

    def test_monitoring_posts_and_deletes_only_at_the_declared_api(self):
        received = []

        class Receiver(BaseHTTPRequestHandler):
            def respond(self):
                body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                received.append((self.command, self.path, body))
                self.send_response(200)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"{}")

            do_POST = respond
            do_DELETE = respond

            def log_message(self, *_arguments):
                pass

        server = HTTPServer(("127.0.0.1", self.manifest["api"]["port"]), Receiver)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()

        def close_receiver():
            server.shutdown()
            server.server_close()
            worker.join(timeout=5)
            self.assertFalse(worker.is_alive())

        self.addCleanup(close_receiver)
        for arguments in (
            ("monitoring", "start", "--channel", "900004", "--key", "private-test", "--description", "private monitor"),
            ("monitoring", "stop", "--channel", "900004", "--key", "private-test"),
        ):
            result = self.run_cli(*arguments)
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([(method, path) for method, path, _ in received], [
            ("POST", "/api/channels/900004/monitoring"),
            ("DELETE", "/api/channels/900004/monitoring/private-test"),
        ])
        self.assertEqual(json.loads(received[0][2]), {"key": "private-test", "description": "private monitor"})


if __name__ == "__main__":
    unittest.main()
