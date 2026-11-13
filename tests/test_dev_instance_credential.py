"""Exercise Python credential commands through the actual lane Rust binary."""

import io
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import unittest

from tests import test_dev_instance as declaration_fixture
from tests import test_dev_instance_files as files_fixture
from scripts.dev_instance.files import InstanceError, validate_tree, write_credential


def require_lane_binary(test):
    directory = Path(os.environ.get("PATH", "").split(os.pathsep)[0])
    binary = directory / "agentdesk"
    test.assertTrue(directory.is_absolute() and binary.is_file() and os.access(binary, os.X_OK),
                    "BLOCKED_BINARY: place the built lane agentdesk directory first on PATH")
    return directory


class InstanceCredentialCliTests(unittest.TestCase):
    def setUp(self):
        self.binary_directory = require_lane_binary(self)
        self.binary = self.binary_directory / "agentdesk"
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


class InstanceCredentialFilesTests(unittest.TestCase):
    def setUp(self):
        require_lane_binary(self)
        self.fixture = files_fixture.InstanceFilesTests()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.root = self.fixture.root

    def test_credentials_require_three_distinct_private_files(self):
        validate_tree(self.root)
        with self.assertRaisesRegex(InstanceError, "three"):
            validate_tree(self.root, require_credentials=True)
        for index, name in enumerate(("claude_bot_token", "codex_bot_token", "driver_bot_token")):
            sentinel = f"sentinel-fake-token-{index}"
            write_credential(self.root, name, io.StringIO(sentinel + "\n"))
            path = self.root / "credential" / name
            self.assertEqual(path.read_text(), sentinel)
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        validate_tree(self.root, require_credentials=True)
        (self.root / "credential/driver_bot_token").write_bytes(
            (self.root / "credential/claude_bot_token").read_bytes())
        with self.assertRaisesRegex(InstanceError, "distinct"):
            validate_tree(self.root, require_credentials=True)

    def test_invalid_token_input_is_rejected_without_echoing_it(self):
        cases = (
            ("unexpected_bot_token", b"sentinel-token"),
            ("claude_bot_token", b""),
            ("claude_bot_token", b"sentinel-token\nother"),
            ("claude_bot_token", b"sentinel-token\r"),
            ("claude_bot_token", b" sentinel-token"),
            ("claude_bot_token", b"x" * 4097),
            ("claude_bot_token", b"x" * 4096 + b"\ntrailing"),
        )
        for name, value in cases:
            with self.subTest(name=name, size=len(value)):
                with self.assertRaises(InstanceError) as error:
                    write_credential(self.root, name, io.BytesIO(value))
                self.assertNotIn("sentinel-token", str(error.exception))
                self.assertEqual(list((self.root / "credential").iterdir()), [])
        write_credential(self.root, "claude_bot_token", io.BytesIO(b"sentinel-distinct"))
        with self.assertRaisesRegex(InstanceError, "distinct"):
            write_credential(self.root, "driver_bot_token", io.BytesIO(b"sentinel-distinct"))
        self.assertFalse((self.root / "credential/driver_bot_token").exists())

    def test_unknown_or_unsafe_existing_credentials_are_not_migrated(self):
        path = self.root / "credential/claude_bot_token"
        write_credential(self.root, path.name, io.BytesIO(b"sentinel-fake-token"))
        for mode in (0o644, 0o400):
            path.chmod(mode)
            with self.subTest(mode=mode), self.assertRaisesRegex(InstanceError, "private"):
                validate_tree(self.root)
        path.chmod(0o600)
        credentials = self.root / "credential"
        credentials.chmod(0o755)
        with self.assertRaisesRegex(InstanceError, "0700"):
            validate_tree(self.root)
        credentials.chmod(0o700)
        path.write_bytes(b"")
        with self.assertRaisesRegex(InstanceError, "nonempty"):
            validate_tree(self.root)
        path.unlink()
        for name, directory in (("unknown_token", False), ("codex_bot_token", True)):
            entry = credentials / name
            entry.mkdir() if directory else entry.write_bytes(b"sentinel-legacy")
            with self.subTest(name=name), self.assertRaisesRegex(InstanceError, "unknown"):
                validate_tree(self.root)
            entry.rmdir() if directory else entry.unlink()


if __name__ == "__main__":
    unittest.main()
