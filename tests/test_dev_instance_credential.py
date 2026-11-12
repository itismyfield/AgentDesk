"""Exercise Python credential commands through the actual lane Rust binary."""

import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import unittest

from tests import test_dev_instance as declaration_fixture


class InstanceCredentialCliTests(unittest.TestCase):
    def setUp(self):
        first_path = os.environ.get("PATH", "").split(os.pathsep)[0]
        self.binary_directory = Path(first_path)
        self.binary = self.binary_directory / "agentdesk"
        self.assertTrue(self.binary_directory.is_absolute(), "lane binary PATH entry must be absolute")
        self.assertTrue(self.binary.is_file() and os.access(self.binary, os.X_OK),
                        "build the lane agentdesk binary and place its directory first on PATH")
        self.fixture = declaration_fixture.InstanceDeclarationTests()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.fixture.prepare()
        self.root = self.fixture.base / self.fixture.instance_id

    def credential(self, name, token, first_path=None):
        selected = first_path if first_path is not None else self.binary_directory
        return subprocess.run(
            [sys.executable, "-E", "-s", "-m", "scripts.dev_instance", "credential",
             "--instance", self.fixture.instance_id, "--name", name],
            cwd=declaration_fixture.REPOSITORY,
            env={"HOME": str(self.fixture.home), "LC_ALL": "C",
                 "PATH": os.pathsep.join((str(selected), str(self.binary_directory), "/usr/bin", "/bin"))},
            input=token, capture_output=True, check=False)

    def test_stdin_credential_is_written_by_the_lane_binary_without_secret_output(self):
        token = b"FAKE_TEST_CREDENTIAL_FROM_STDIN_ONLY"
        result = self.credential("claude_bot_token", token + b"\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["status"], "CREDENTIAL_WRITTEN")
        path = self.root / "credential/claude_bot_token"
        self.assertEqual(path.read_bytes(), token)
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(path.parent.stat().st_mode), 0o700)
        for output in (result.stdout, result.stderr,
                       (self.root / "manifest.json").read_bytes(),
                       (self.root / "receipt.json").read_bytes()):
            self.assertNotIn(token, output)

    def test_shared_writer_failure_preserves_existing_file_and_fails_the_python_cli(self):
        original = b"FAKE_EXISTING_TEST_CREDENTIAL"
        first = self.credential("claude_bot_token", original)
        self.assertEqual(first.returncode, 0, first.stderr)
        replacement = b"FAKE_DIFFERENT_REPLACEMENT_CREDENTIAL"
        rejected = self.credential("claude_bot_token", replacement)
        self.assertEqual(rejected.returncode, 2, rejected.stderr)
        self.assertEqual(json.loads(rejected.stdout)["status"], "REJECTED")
        self.assertEqual((self.root / "credential/claude_bot_token").read_bytes(), original)
        for token in (original, replacement):
            self.assertNotIn(token, rejected.stdout + rejected.stderr)

    def test_missing_first_path_binary_is_blocked_without_falling_back_or_writing(self):
        empty_bin = self.fixture.home / "empty-bin"
        empty_bin.mkdir()
        token = b"FAKE_BLOCKED_CREDENTIAL"
        result = self.credential("driver_bot_token", token, first_path=empty_bin)
        self.assertEqual(result.returncode, 4, result.stderr)
        self.assertEqual(json.loads(result.stdout)["status"], "BLOCKED_BINARY")
        self.assertEqual(list((self.root / "credential").iterdir()), [])
        self.assertNotIn(token, result.stdout + result.stderr)

    def test_rust_entrypoint_rejects_inline_arguments_without_echoing_secrets(self):
        token = b"FAKE_INLINE_ARGUMENT_NEVER_ECHO"
        for prefix in ([], ["--json"]):
            with self.subTest(prefix=prefix):
                result = subprocess.run(
                    [str(self.binary), *prefix, "dev-instance", "credential-write", "--root", str(self.root),
                     "--name", "claude_bot_token", token.decode()],
                    env={"PATH": "/usr/bin:/bin", "LC_ALL": "C"},
                    input=b"", capture_output=True, check=False)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn(token, result.stdout + result.stderr)
                self.assertFalse((self.root / "credential/claude_bot_token").exists())


if __name__ == "__main__":
    unittest.main()
