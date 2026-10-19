"""Exercise shared libtest and fallback without compiling or running Rust."""

import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location(
    "lib_test_artifact", Path(__file__).resolve().parents[1]
    / "scripts/ci/lib-test-artifact.py")
artifact = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(artifact)
PLAIN = "alpha::test: test\nbeta::ignored: test\n\n2 tests, 0 benchmarks\n"
IGNORED = "beta::ignored: test\n\n1 test, 0 benchmarks\n"


class LibTestArtifactTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.directory = self.root / "artifact"
        self.executable = self.root / "target/lib-tests"
        self.executable.parent.mkdir()
        self.executable.write_text("fake compiled executable\n")
        self.executable.chmod(0o755)
        self.builds = 0
        self.executed = []
        self.live_plain = PLAIN
        self.live_ignored = IGNORED
        self.exit_code = 0
        self.profile = {"test": True, "debuginfo": 0}
        for mocked in (
            patch.dict(os.environ, {"GITHUB_RUN_ID": "1234", "GITHUB_RUN_ATTEMPT": "2",
                                    "CARGO_PROFILE_DEV_DEBUG": "0",
                                    "CARGO_PROFILE_TEST_DEBUG": "0"}, clear=True),
            patch.object(artifact.sys, "platform", "linux"),
            patch.object(artifact, "_capture", side_effect=self.capture),
            patch.object(artifact.subprocess, "call", side_effect=self.execute),
        ):
            mocked.start()
            self.addCleanup(mocked.stop)

    def capture(self, argv, root, env=None):
        if argv == ["rustc", "-vV"]:
            return "rustc 1.94.1\nhost: x86_64-unknown-linux-gnu\nrelease: 1.94.1\n"
        if argv == ["git", "rev-parse", "HEAD"]:
            return "a" * 40 + "\n"
        if argv == artifact.BUILD:
            self.builds += 1
            self.assertEqual(env["CARGO_PROFILE_DEV_DEBUG"], "0")
            self.assertEqual(env["CARGO_PROFILE_TEST_DEBUG"], "0")
            return json.dumps({"reason": "compiler-artifact", "profile": self.profile,
                               "target": {"kind": ["lib"]},
                               "executable": str(self.executable)}) + "\n"
        if argv[-2:] == ["--list", "--ignored"]:
            return self.live_ignored
        if argv[-1:] == ["--list"]:
            return self.live_plain
        self.fail(f"unexpected captured command: {argv}")

    def execute(self, argv, cwd):
        self.executed.append(argv)
        return self.exit_code

    def prepare(self):
        artifact.prepare(self.directory, self.root)
        os.environ["ADK_LIB_TEST_ARTIFACT"] = str(self.directory)

    def mutate_metadata(self, key, value):
        path = self.directory / "metadata.json"
        metadata = json.loads(path.read_text())
        metadata[key] = value
        path.write_text(json.dumps(metadata))

    def test_one_build_serves_all_consumers_and_inventory(self):
        self.prepare()
        commands = [
            ["cargo", "test", "--lib", "--", "--exact", "alpha::test"],
            ["cargo", "test", "--lib", "--", "--skip", "alpha::test"],
            ["cargo", "test", "--lib", "recovery", "--", "--test-threads=1"],
            ["cargo", "test", "--lib", "relay", "--", "--exact"],
            ["cargo", "test", "--lib", "--", "--skip", "pg", "--nocapture"],
        ]
        for command in commands:
            self.assertEqual(artifact.run(command, self.root), 0)
        self.assertEqual(artifact.validated_inventory(self.root), PLAIN)
        self.assertEqual(artifact.validated_inventory(self.root, ignored=True), IGNORED)
        self.assertEqual(self.builds, 1)
        self.assertEqual(len(self.executed), 5)
        self.assertTrue(all(argv[0] == str(self.directory / "lib-tests")
                            for argv in self.executed))
        self.assertEqual(self.executed[2][1:], ["recovery", "--test-threads=1"])

    def test_previous_attempt_serves_binary_and_inventory_without_fallback(self):
        self.prepare()
        os.environ["GITHUB_RUN_ATTEMPT"] = "3"
        self.assertEqual(artifact.run(["cargo", "test", "--lib", "alpha"], self.root), 0)
        self.assertEqual(artifact.validated_inventory(self.root), PLAIN)
        self.assertEqual(artifact.validated_inventory(self.root, ignored=True), IGNORED)
        self.assertEqual(self.executed, [[str(self.directory / "lib-tests"), "alpha"]])
        self.assertEqual(self.builds, 1)
        metadata = json.loads((self.directory / "metadata.json").read_text())
        self.assertEqual(metadata["run_attempt"], "2")

    def test_missing_producer_or_download_falls_back(self):
        command = ["cargo", "test", "--lib", "--", "--list"]
        for directory in (None, str(self.directory)):
            with self.subTest(directory=directory):
                if directory:
                    os.environ["ADK_LIB_TEST_ARTIFACT"] = directory
                else:
                    os.environ.pop("ADK_LIB_TEST_ARTIFACT", None)
                self.assertEqual(artifact.run(command, self.root), 0)
                self.assertEqual(self.executed[-1], command)
                self.assertIsNone(artifact.validated_inventory(self.root))

    def test_contract_mismatches_fall_back(self):
        self.prepare()
        os.environ["GITHUB_RUN_ATTEMPT"] = "3"
        original = (self.directory / "metadata.json").read_text()
        mutants = {"sha": "b" * 40, "root": "/other/checkout", "host": "other-host",
                   "rust": "1.94.0", "profile": "release", "features": "all",
                   "run_id": "other-run", "flags": {},
                   "build": ["cargo", "test", "--all-features"]}
        command = ["cargo", "test", "--lib"]
        for key, value in mutants.items():
            with self.subTest(key=key):
                (self.directory / "metadata.json").write_text(original)
                self.mutate_metadata(key, value)
                self.assertEqual(artifact.run(command, self.root), 0)
                self.assertEqual(self.executed[-1], command)
                self.assertIsNone(artifact.validated_inventory(self.root))

    def test_each_corrupt_file_falls_back(self):
        self.prepare()
        command = ["cargo", "test", "--lib"]
        for name in artifact.FILES:
            with self.subTest(name=name):
                path = self.directory / name
                saved = path.read_bytes()
                path.write_bytes(saved + b"damage")
                self.assertEqual(artifact.run(command, self.root), 0)
                self.assertEqual(self.executed[-1], command)
                path.write_bytes(saved)

    def test_compiler_and_target_flag_drift_falls_back(self):
        self.prepare()
        command = ["cargo", "test", "--lib"]
        for key in ("CARGO_BUILD_RUSTFLAGS", "CARGO_BUILD_TARGET",
                    "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
                    "RUSTC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"):
            with self.subTest(key=key), patch.dict(os.environ, {key: "changed"}):
                self.assertEqual(artifact.run(command, self.root), 0)
                self.assertEqual(self.executed[-1], command)
                self.assertIsNone(artifact.validated_inventory(self.root))

    def test_live_inventory_mismatch_and_nonexecutable_fall_back(self):
        self.prepare()
        command = ["cargo", "test", "--lib"]
        self.live_plain = "different: test\n\n1 test, 0 benchmarks\n"
        self.assertEqual(artifact.run(command, self.root), 0)
        self.assertEqual(self.executed[-1], command)
        self.live_plain = PLAIN
        (self.directory / "lib-tests").chmod(0o644)
        self.assertEqual(artifact.run(command, self.root), 0)
        self.assertEqual(self.executed[-1], command)

    def test_failed_shared_test_never_retries_cargo(self):
        self.prepare()
        self.exit_code = 101
        self.assertEqual(artifact.run(["cargo", "test", "--lib", "alpha"], self.root), 101)
        self.assertEqual(self.executed, [[str(self.directory / "lib-tests"), "alpha"]])
        self.assertEqual(self.builds, 1)

    def test_failed_cargo_fallback_stays_red(self):
        self.exit_code = 101
        self.assertEqual(artifact.run(["cargo", "test", "--lib"], self.root), 101)
        self.assertEqual(self.executed, [["cargo", "test", "--lib"]])

    def test_unsupported_build_option_uses_original_cargo(self):
        self.prepare()
        for option in ("--release", "--all-features", "--workspace"):
            command = ["cargo", "test", "--lib", option]
            self.assertEqual(artifact.run(command, self.root), 0)
            self.assertEqual(self.executed[-1], command)

    def test_manifest_locked_and_default_libtest_translation(self):
        self.prepare()
        command = ["cargo", "test", "--locked", "--manifest-path",
                   str(self.root / "Cargo.toml"), "--lib"]
        self.assertEqual(artifact.run(command, self.root), 0)
        self.assertEqual(self.executed[-1], [str(self.directory / "lib-tests")])

    def test_malformed_inventory_is_rejected(self):
        for value in ("", "a: test\n", "a: test\n1 test, 1 benchmark\n",
                      "a: test\na: test\n2 tests, 0 benchmarks\n",
                      "a: invalid\n0 tests, 0 benchmarks\n",
                      "0 tests, 0 benchmarks\na: test\n"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                artifact._inventory(value)

    def test_rehashed_malformed_inventory_cannot_be_used(self):
        self.prepare()
        path = self.directory / "plain.list"
        path.write_text("malformed but rehashed\n")
        metadata = json.loads((self.directory / "metadata.json").read_text())
        metadata["checksums"]["plain.list"] = artifact._checksum(path)
        self.mutate_metadata("checksums", metadata["checksums"])
        self.assertIsNone(artifact.validated_inventory(self.root))

    def test_producer_rejects_profile_and_empty_inventory(self):
        self.profile["debuginfo"] = 2
        with self.assertRaisesRegex(ValueError, "debuginfo"):
            artifact.prepare(self.directory, self.root)
        self.profile["debuginfo"] = 0
        self.live_plain = "0 tests, 0 benchmarks\n"
        with self.assertRaisesRegex(ValueError, "empty"):
            artifact.prepare(self.directory, self.root)

    def test_execution_start_failure_stays_red(self):
        self.prepare()
        with patch.object(artifact.subprocess, "call", side_effect=OSError("gone")) as call:
            self.assertEqual(artifact.run(["cargo", "test", "--lib"], self.root), 1)
        self.assertEqual(call.call_count, 1)
        self.assertEqual(call.call_args.args[0], [str(self.directory / "lib-tests")])


if __name__ == "__main__":
    unittest.main()
