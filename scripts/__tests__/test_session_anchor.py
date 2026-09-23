"""Offline CLI/MCP regression checks; never invokes a real model."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / "session-anchor.py"
ZSHRC = Path(os.environ.get("SESSION_ANCHOR_TEST_ZSHRC", str(SCRIPT.with_suffix(".zsh"))))
spec = importlib.util.spec_from_file_location("session_anchor", SCRIPT)
anchor = importlib.util.module_from_spec(spec)
spec.loader.exec_module(anchor)
# Launch-window inputs the wrappers read; a Claude Code parent shell exports the first.
COMPACT_ENV = ("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "CC_COMPACT_WINDOW")
ANCHOR = "[ANCHOR MEMORY]\n- Global 'quoted' $(touch BAD) `false` $HOME\n- 작업 기준"


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.server.calls.append(request)
        if self.server.mode == "http_error":
            self.send_error(503, "DO_NOT_LEAK_SECRET")
            return
        if self.server.mode == "timeout":
            time.sleep(4.2)
        if request["method"] == "initialize":
            result = {"protocolVersion": "2025-11-25", "capabilities": {}}
        else:
            if request["params"]["name"] == "recall":
                payload = {"success": True, "fragments": [
                    {"id": "global", "workspace": None, "content": "global exact content"},
                    {"id": "private", "workspace": "unrelated", "content": "NEVER_INJECT"},
                ]}
            else:
                payload = {"success": True, "anchorCount": 2,
                           "anchors": {"permanent": [{"content": "Global 'quoted' $(touch BAD) `false` $HOME"},
                                                       {"content": "작업 기준"}]},
                           "injectionText": ANCHOR + "\n\n[CORE MEMORY]\nNEVER_INJECT"}
                if self.server.mode == "empty":
                    payload = {"success": True, "anchorCount": 0, "injectionText": "[CORE MEMORY]\nCORE"}
                elif self.server.mode == "malformed":
                    payload = {"success": True, "anchorCount": 2, "injectionText": "not anchors"}
                elif self.server.mode == "partial":
                    payload["_meta"] = {"anchorSelection": {"partial": True}}
            result = {"content": [{"type": "text", "text": json.dumps(payload)}]}
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("MCP-Session-Id", "fake-session")
        self.end_headers()
        try:
            self.wfile.write(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}).encode())
        except BrokenPipeError:
            pass


class AnchorTests(unittest.TestCase):
    compact_default = os.environ.get("SESSION_ANCHOR_TEST_COMPACT_DEFAULT", "500000")

    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()
        text = ZSHRC.read_text()
        cls.wrappers = text[text.index("# Claude Code shortcut"):].split("alias gem=", 1)[0]

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="anchor-cli-test-")
        self.root = Path(self.temp.name).resolve()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.codex_home = self.root / "codex"
        self.codex_home.mkdir()
        self.server.calls = []
        self.server.mode = "ok"
        inherited = {k: v for k, v in os.environ.items() if k not in COMPACT_ENV}
        self.env = dict(inherited, HOME=str(self.root), CODEX_HOME=str(self.codex_home),
                        PATH=str(self.bin) + ":" + os.environ["PATH"],
                        MEMENTO_ACCESS_KEY="DO_NOT_LEAK_SECRET", MEMENTO_WORKSPACE="test-project",
                        MEMENTO_MCP_URL="http://127.0.0.1:" + str(self.server.server_port) + "/mcp",
                        ADK_SESSION_ANCHOR_HELPER=str(SCRIPT), ADK_SESSION_ANCHOR_PYTHON=sys.executable,
                        FAKE_CLI_LOG=str(self.root / "args.json"),
                        FAKE_TMUX_LOG=str(self.root / "tmux.json"), TMUX="fake-existing-client")
        for cli in ("claude", "codex"):
            file = self.bin / cli
            file.write_text("#!" + sys.executable + "\nimport json,os,sys\n"
                            "from pathlib import Path\n"
                            "Path(os.environ['FAKE_CLI_LOG']).write_text(json.dumps({'args':sys.argv[1:],"
                            "'compact':os.environ.get('CLAUDE_CODE_AUTO_COMPACT_WINDOW')}))\n")
            file.chmod(0o755)
        tmux = self.bin / "tmux"
        tmux.write_text("#!" + sys.executable + "\nimport json,os,subprocess,sys\nfrom pathlib import Path\n"
                        "if sys.argv[1]=='has-session': sys.exit(0 if os.getenv('FAKE_TMUX_EXISTS') else 1)\n"
                        "if sys.argv[1]=='new-session':\n"
                        " Path(os.environ['FAKE_TMUX_LOG']).write_text(json.dumps(sys.argv[2:]))\n"
                        " sys.exit(subprocess.call(['/bin/zsh','-f','-c',sys.argv[-1]]))\n")
        tmux.chmod(0o755)

    def tearDown(self):
        self.temp.cleanup()

    def run_wrapper(self, name, args=()):
        result = subprocess.run(["/bin/zsh", "-f", "-c", self.wrappers + '\n' + name + ' "$@"', "test", *args],
                                cwd=self.root, env=self.env, text=True, capture_output=True, timeout=8)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("DO_NOT_LEAK_SECRET", result.stdout + result.stderr)
        self.assertNotIn("Global 'quoted'", result.stdout + result.stderr)
        log = self.root / "args.json"
        return (json.loads(log.read_text()) if log.exists() else None), result

    def test_four_launch_paths_preserve_arguments_and_inject_once(self):
        prompt = "single ' double \" dollar $(touch BAD) backtick `false`\n두 번째 줄"
        for name in ("cc", "cct", "cdx", "cdxt"):
            with self.subTest(wrapper=name):
                self.server.calls = []
                data, _ = self.run_wrapper(name, ["--model", "selected-model", prompt])
                args = data["args"]
                self.assertEqual(args[-3:], ["--model", "selected-model", prompt])
                self.assertEqual(len(self.server.calls), 2)
                self.assertEqual(self.server.calls[1]["params"]["arguments"]["workspace"], "test-project")
                self.assertEqual(self.server.calls[1]["params"]["arguments"]["agentId"], "default")
                if name.startswith("cc"):
                    self.assertIn("--dangerously-skip-permissions", args)
                    text = args[args.index("--append-system-prompt") + 1]
                    self.assertEqual(data["compact"], self.compact_default if name == "cct" else None)
                else:
                    self.assertIn("--dangerously-bypass-approvals-and-sandbox", args)
                    self.assertIn("--no-alt-screen", args)
                    text = anchor.tomllib.loads(args[args.index("-c") + 1])["developer_instructions"]
                self.assertEqual(text.count("[ANCHOR MEMORY]"), 1)
                self.assertTrue(text.endswith(ANCHOR))
                self.assertNotIn("NEVER_INJECT", text)
                self.assertFalse((self.root / "BAD").exists())

    def test_claude_preserves_custom_system_and_append(self):
        data, _ = self.run_wrapper("cc", ["--system-prompt", "custom system", "--append-system-prompt", "existing append"])
        args = data["args"]
        self.assertEqual(args[args.index("--system-prompt") + 1], "custom system")
        self.assertTrue(args[args.index("--append-system-prompt") + 1].startswith("existing append\n\n"))

    def test_codex_preserves_cli_developer_instructions_and_model(self):
        data, _ = self.run_wrapper("cdx", ["--config=developer_instructions=\"existing developer\"", "-m", "mine"])
        overrides = anchor.option_values(data["args"], ("-c", "--config"))
        for item in overrides:
            self.assertTrue(anchor.tomllib.loads(item)["developer_instructions"].startswith("existing developer\n\n"))
        self.assertEqual(data["args"][-2:], ["-m", "mine"])

    def test_codex_user_profile_and_trusted_project(self):
        (self.codex_home / "config.toml").write_text('developer_instructions="user"\n[projects.' + json.dumps(str(self.root)) + ']\ntrust_level="trusted"\n')
        (self.codex_home / "work.config.toml").write_text('developer_instructions="profile"\n')
        (self.root / ".codex").mkdir()
        (self.root / ".codex" / "config.toml").write_text('developer_instructions="project"\n')
        data, _ = self.run_wrapper("cdx", ["--profile", "work"])
        value = anchor.tomllib.loads(data["args"][1])["developer_instructions"]
        self.assertTrue(value.startswith("project\n\n"))

    def test_home_filters_default_workspace_anchors(self):
        self.env.pop("MEMENTO_WORKSPACE")
        data, _ = self.run_wrapper("cc")
        prompt = data["args"][1]
        self.assertIn("global exact content", prompt)
        self.assertNotIn("NEVER_INJECT", prompt)
        self.assertEqual(len(self.server.calls), 3)

    def test_effective_cd_and_git_worktree_use_canonical_repo(self):
        self.env.pop("MEMENTO_WORKSPACE")
        repo = self.root / "CanonicalProject"
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        subprocess.run(["git", "-C", str(repo), "-c", "user.name=Test", "-c", "user.email=test@example.invalid",
                        "commit", "--allow-empty", "-qm", "test"], check=True)
        worktree = self.root / "temporary-worktree"
        subprocess.run(["git", "-C", str(repo), "worktree", "add", "--detach", "-q", str(worktree)], check=True)
        self.run_wrapper("cdx", ["-C", str(worktree)])
        self.assertEqual(self.server.calls[1]["params"]["arguments"]["workspace"], "canonicalproject")

    def test_failures_and_empty_preserve_original_launch(self):
        for mode in ("http_error", "malformed", "empty", "timeout"):
            with self.subTest(mode=mode):
                self.server.mode = mode
                data, result = self.run_wrapper("cc", ["hello"])
                self.assertEqual(data["args"], ["--dangerously-skip-permissions", "hello"])
                if mode != "empty":
                    self.assertIn("continuing with original instructions", result.stderr)

    def test_resume_help_management_do_not_fetch(self):
        for name, args in (("cc", ["--resume", "id"]), ("cc", ["--continue"]),
                           ("cc", ["--help"]), ("cc", ["--version"]), ("cc", ["-v"]), ("cc", ["auth", "status"]),
                           ("cdx", ["resume", "--last"]), ("cdx", ["mcp", "list"]),
                           ("cdx", ["login", "status"]), ("cdx", ["--help"])):
            with self.subTest(name=name, args=args):
                self.run_wrapper(name, args)
                self.assertEqual(self.server.calls, [])

    def test_partial_metadata_rejects_project_and_global_before_recall(self):
        for global_only in (False, True):
            with self.subTest(global_only=global_only):
                if global_only:
                    self.env.pop("MEMENTO_WORKSPACE")
                self.server.calls = []
                self.server.mode = "partial"
                data, result = self.run_wrapper("cc", ["hello"])
                self.assertEqual(data["args"], ["--dangerously-skip-permissions", "hello"])
                self.assertIn("continuing with original instructions", result.stderr)
                self.assertEqual(len(self.server.calls), 2)

    def test_extractor_rejects_partial_metadata_even_with_valid_records(self):
        with self.assertRaises(ValueError):
            anchor.extract_anchors({"anchorCount": 1, "anchors": {"permanent": [{"content": "complete-looking"}]},
                                    "injectionText": "[ANCHOR MEMORY]\n- complete-looking",
                                    "_meta": {"anchorSelection": {"partial": True}}})

    def test_existing_tmux_session_does_not_refetch(self):
        self.env["FAKE_TMUX_EXISTS"] = "1"
        for name in ("cct", "cdxt"):
            data, _ = self.run_wrapper(name)
            self.assertIsNone(data)
            self.assertEqual(self.server.calls, [])

    def test_explicit_compact_window_preserved(self):
        data, _ = self.run_wrapper("cct", ["350k", "hello"])
        self.assertEqual(data["compact"], "350000")
        self.assertNotIn("350k", data["args"])

    def test_compact_window_sources_per_launch_path(self):
        self.env["CLAUDE_CODE_AUTO_COMPACT_WINDOW"] = "123"
        self.assertEqual(self.run_wrapper("cc")[0]["compact"], "123")
        self.assertEqual(self.run_wrapper("cct")[0]["compact"], self.compact_default)
        self.env["CC_COMPACT_WINDOW"] = "456"
        self.assertEqual(self.run_wrapper("cct")[0]["compact"], "456")

    def test_malformed_and_oversize_anchor_rejected(self):
        for payload in ({}, {"anchorCount": 1, "injectionText": "[ANCHOR MEMORY]\n"},
                        {"anchorCount": 1, "injectionText": "[ANCHOR MEMORY]\n" + "x" * 70000}):
            with self.assertRaises(ValueError):
                anchor.extract_anchors(payload)

    def test_multiline_anchor_section_names_are_preserved(self):
        content = "literal\n[CORE MEMORY]\ninside an anchor"
        expected = "[ANCHOR MEMORY]\n- " + content
        self.assertEqual(anchor.extract_anchors({"anchorCount": 1,
                         "anchors": {"permanent": [{"content": content}]},
                         "injectionText": expected + "\n\n[CORE MEMORY]\nnot an anchor"}), expected)


class MacMiniAnchorTests(AnchorTests):
    compact_default = "1000000"

    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        cls.wrappers = SCRIPT.with_name("session-anchor-mac-mini.zsh").read_text()


if __name__ == "__main__":
    unittest.main()
