"""Exercise instance worker re-entry through the built lane executable."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import queue
import subprocess
import threading
import time
import unittest

from tests import test_dev_instance as declaration_fixture
from tests.test_dev_instance_credential import require_lane_binary


class InstanceWorkerCliTests(unittest.TestCase):
    def test_hook_workers_deliver_drain_and_publish_failure_under_the_instance_root(self):
        binary = require_lane_binary(self) / "agentdesk"
        fixture = declaration_fixture.InstanceDeclarationTests()
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        fixture.prepare()
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
        environment = {
            "HOME": str(fixture.home), "PATH": "/usr/bin:/bin", "LC_ALL": "C",
            "AGENTDESK_TEST_MANIFEST": str(root / "manifest.json"),
            "AGENTDESK_ROOT_DIR": str(root),
            "AGENTDESK_CONFIG": str(root / "config/agentdesk.yaml"),
        }

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


if __name__ == "__main__":
    unittest.main()
