from __future__ import annotations

import fcntl
import importlib.util
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest


REPO_ROOT = Path(__file__).resolve().parents[1]
HELPER_PATH = REPO_ROOT / "scripts" / "build_token.py"
DEPLOY_PATH = REPO_ROOT / "scripts" / "deploy-release.sh"
BUILD_RELEASE_PATH = REPO_ROOT / "scripts" / "build-release.sh"
DEFAULTS_PATH = REPO_ROOT / "scripts" / "_defaults.sh"
SOURCE_OF_TRUTH_PATH = REPO_ROOT / "docs" / "source-of-truth.md"

DRIVER = r"""
import importlib.util
from pathlib import Path
import sys

spec = importlib.util.spec_from_file_location("adk_build_token_driver", sys.argv[1])
module = importlib.util.module_from_spec(spec)
assert spec.loader is not None
spec.loader.exec_module(module)
module.run_command(sys.argv[3:], token_path=Path(sys.argv[2]))
"""


def load_helper():
    spec = importlib.util.spec_from_file_location("adk_build_token_test", HELPER_PATH)
    if spec is None or spec.loader is None:
        raise AssertionError(f"could not load {HELPER_PATH}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def driver_command(token_path: Path, command: list[str]) -> list[str]:
    return [
        sys.executable,
        "-c",
        DRIVER,
        str(HELPER_PATH),
        str(token_path),
        *command,
    ]


def wait_for_path(path: Path, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists():
            return
        time.sleep(0.02)
    raise AssertionError(f"timed out waiting for {path}")


class BuildTokenBehaviorTests(unittest.TestCase):
    def test_two_commands_serialize_on_one_persistent_inode(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            token = root / "token.lock"
            first_started = root / "first.started"
            second_started = root / "second.started"
            first_code = (
                "from pathlib import Path; import sys,time; "
                "Path(sys.argv[1]).write_text('started'); time.sleep(0.8)"
            )
            second_code = "from pathlib import Path; import sys; Path(sys.argv[1]).write_text('started')"

            first = subprocess.Popen(
                driver_command(token, [sys.executable, "-c", first_code, str(first_started)])
            )
            self.addCleanup(lambda: first.poll() is None and first.kill())
            wait_for_path(first_started)
            inode = token.stat().st_ino

            second = subprocess.Popen(
                driver_command(token, [sys.executable, "-c", second_code, str(second_started)])
            )
            self.addCleanup(lambda: second.poll() is None and second.kill())
            time.sleep(0.2)
            self.assertFalse(second_started.exists(), "second command spawned before token release")

            self.assertEqual(first.wait(timeout=5), 0)
            self.assertEqual(second.wait(timeout=5), 0)
            self.assertTrue(second_started.exists())
            self.assertEqual(token.stat().st_ino, inode)

    def test_jobs_are_forced_to_two_and_exit_code_is_preserved(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            token = root / "token.lock"
            observed = root / "jobs"
            code = (
                "from pathlib import Path; import os,sys; "
                "Path(sys.argv[1]).write_text(os.environ.get('CARGO_BUILD_JOBS','')); "
                "raise SystemExit(37)"
            )
            env = os.environ.copy()
            env["CARGO_BUILD_JOBS"] = "99"
            result = subprocess.run(
                driver_command(token, [sys.executable, "-c", code, str(observed)]),
                env=env,
                check=False,
            )
            self.assertEqual(result.returncode, 37)
            self.assertEqual(observed.read_text(), "2")

    def test_nested_helper_reuses_valid_inherited_fd_without_deadlock(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            token = root / "token.lock"
            observed = root / "nested"
            inner = driver_command(
                token,
                [
                    sys.executable,
                    "-c",
                    "from pathlib import Path; import sys; Path(sys.argv[1]).write_text('ok')",
                    str(observed),
                ],
            )
            shell = root / "nested.sh"
            shell.write_text("#!/usr/bin/env bash\nexec \"$@\"\n")
            shell.chmod(0o700)
            result = subprocess.run(
                driver_command(token, [str(shell), *inner]),
                check=False,
                timeout=5,
            )
            self.assertEqual(result.returncode, 0)
            self.assertEqual(observed.read_text(), "ok")

    def test_plain_or_wrong_inherited_fd_cannot_spawn_command(self):
        helper = load_helper()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            token = root / "token.lock"
            token.touch()
            wrong = root / "wrong.lock"
            wrong.touch()
            sentinel = root / "spawned"
            wrong_fd = os.open(wrong, os.O_RDWR)
            self.addCleanup(os.close, wrong_fd)
            env = os.environ.copy()
            env[helper.TOKEN_FD_ENV] = str(wrong_fd)
            result = subprocess.run(
                driver_command(
                    token,
                    [sys.executable, "-c", "from pathlib import Path; import sys; Path(sys.argv[1]).touch()", str(sentinel)],
                ),
                env=env,
                pass_fds=(wrong_fd,),
                check=False,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(sentinel.exists())

            env[helper.TOKEN_FD_ENV] = "999999"
            result = subprocess.run(
                driver_command(token, [sys.executable, "-c", "raise SystemExit(0)"]),
                env=env,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0)

    def test_cancelled_waiter_never_spawns_and_never_removes_token(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            token = root / "token.lock"
            holder_started = root / "holder.started"
            waiter_spawned = root / "waiter.spawned"
            holder_code = (
                "from pathlib import Path; import sys,time; "
                "Path(sys.argv[1]).touch(); time.sleep(30)"
            )
            holder = subprocess.Popen(
                driver_command(token, [sys.executable, "-c", holder_code, str(holder_started)])
            )
            self.addCleanup(lambda: holder.poll() is None and holder.kill())
            wait_for_path(holder_started)
            inode = token.stat().st_ino
            waiter = subprocess.Popen(
                driver_command(
                    token,
                    [sys.executable, "-c", "from pathlib import Path; import sys; Path(sys.argv[1]).touch()", str(waiter_spawned)],
                )
            )
            time.sleep(0.2)
            waiter.terminate()
            waiter.wait(timeout=5)
            self.assertFalse(waiter_spawned.exists())
            self.assertTrue(token.exists())
            self.assertEqual(token.stat().st_ino, inode)

    def test_unlink_recreate_while_waiting_fails_closed(self):
        helper = load_helper()
        with tempfile.TemporaryDirectory() as tmp:
            token = Path(tmp) / "token.lock"
            token.touch(mode=0o600)
            holder_fd = os.open(token, os.O_RDWR)
            self.addCleanup(os.close, holder_fd)
            fcntl.flock(holder_fd, fcntl.LOCK_EX)
            opened = threading.Event()
            outcome: list[BaseException | int] = []

            def wait_for_token() -> None:
                try:
                    fd, _inherited = helper._acquire_token(token, on_open=opened.set)
                except BaseException as error:  # assertion records the precise fail-closed path
                    outcome.append(error)
                else:
                    outcome.append(fd)

            waiter = threading.Thread(target=wait_for_token, daemon=True)
            waiter.start()
            self.assertTrue(opened.wait(timeout=5))
            old_inode = os.fstat(holder_fd).st_ino
            token.unlink()
            token.touch(mode=0o600)
            self.assertNotEqual(token.stat().st_ino, old_inode)
            fcntl.flock(holder_fd, fcntl.LOCK_UN)
            waiter.join(timeout=5)
            self.assertFalse(waiter.is_alive())
            self.assertEqual(len(outcome), 1)
            self.assertIsInstance(outcome[0], helper.BuildTokenError)

    def test_non_regular_and_stale_inherited_authorities_are_rejected(self):
        helper = load_helper()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            token = root / "token.lock"
            token.touch(mode=0o600)
            stale_fd = os.open(token, os.O_RDWR)
            self.addCleanup(os.close, stale_fd)
            os.set_inheritable(stale_fd, True)
            token.unlink()
            token.touch(mode=0o600)
            env = os.environ.copy()
            env[helper.TOKEN_FD_ENV] = str(stale_fd)
            with self.assertRaises(helper.BuildTokenError):
                helper._acquire_token(token, environ=env)

            token.unlink()
            token.mkdir()
            with self.assertRaises(helper.BuildTokenError):
                helper._acquire_token(token, environ={})

    def test_helper_never_unlinks_or_recreates_the_token(self):
        source = HELPER_PATH.read_text()
        self.assertNotIn("unlink(", source)
        self.assertNotIn("remove(", source)
        self.assertNotIn("replace(", source)
        self.assertNotIn('"w"', source)
        self.assertIn("O_APPEND", source)
        self.assertIn("fcntl.flock", source)


class BuildTokenWiringTests(unittest.TestCase):
    def test_release_entrypoints_route_executable_cargo_through_shared_helper(self):
        defaults = DEFAULTS_PATH.read_text()
        deploy = DEPLOY_PATH.read_text()
        build_release = BUILD_RELEASE_PATH.read_text()
        self.assertIn("_with_build_token()", defaults)
        self.assertIn('python3 "$defaults_dir/build_token.py" -- "$@"', defaults)
        self.assertIn('_with_build_token cargo metadata --format-version 1 --no-deps', deploy)
        self.assertIn('_with_build_token "${clean_cmd[@]}"', deploy)
        self.assertIn('_with_build_token cargo build --release --bin agentdesk', deploy)
        self.assertIn('_with_build_token cargo build --profile "$DEPLOY_BUILD_PROFILE" --bin agentdesk', deploy)
        self.assertIn("_with_build_token cargo build --release", build_release)

        self.assertNotIn('(cd "$REPO" && cargo ', deploy)
        self.assertNotIn("\ncargo build --release", build_release)

    def test_deploy_rejects_outer_token_and_acquires_deploy_lock_first(self):
        deploy = DEPLOY_PATH.read_text()
        reject_call = deploy.index('_reject_inherited_build_token_for_deploy')
        deploy_lock_call = deploy.rindex('_acquire_release_deploy_lock "$@"')
        build_call = deploy.index('_with_build_token cargo build', deploy_lock_call)
        clean_call = deploy.rindex('_clean_release_build_cache_after_staging')
        self.assertLess(reject_call, deploy_lock_call)
        self.assertLess(deploy_lock_call, build_call)
        self.assertLess(build_call, clean_call)

    def test_source_of_truth_names_one_build_token_authority(self):
        source = SOURCE_OF_TRUTH_PATH.read_text()
        self.assertIn("Host-wide Cargo build token", source)
        self.assertIn("scripts/build_token.py", source)
        self.assertIn("/tmp/adk-build-token.lock", source)


if __name__ == "__main__":
    unittest.main()
