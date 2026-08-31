from __future__ import annotations

import importlib.util
from hashlib import sha256
import inspect
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest

if sys.platform != "win32":
    import fcntl


REPO_ROOT = Path(__file__).resolve().parents[1]
HELPER_PATH = REPO_ROOT / "scripts" / "build_token.py"
FACADE_PATH = REPO_ROOT / "scripts" / "_build_token.sh"
DEFAULTS_PATH = REPO_ROOT / "scripts" / "_defaults.sh"
DEPLOY_PATH = REPO_ROOT / "scripts" / "deploy-release.sh"
BUILD_RELEASE_PATH = REPO_ROOT / "scripts" / "build-release.sh"
CI_SCRIPT_PATH = REPO_ROOT / "scripts" / "ci-script-checks.sh"
SOURCE_OF_TRUTH_PATH = REPO_ROOT / "docs" / "source-of-truth.md"
WORKFLOW_PATH = REPO_ROOT / ".github" / "workflows" / "ci-pr.yml"
WIN_HELPER_PATH = REPO_ROOT / "scripts" / "build_token_win32.py"
WIN_TEST_PATH = REPO_ROOT / "tests" / "test_build_token_win32_5663.py"

DRIVER = r"""
import importlib.util
from pathlib import Path
import sys

spec = importlib.util.spec_from_file_location("adk_build_token_driver", sys.argv[1])
module = importlib.util.module_from_spec(spec)
assert spec.loader is not None
spec.loader.exec_module(module)
try:
    result = module._supervise_for_test(sys.argv[3:], Path(sys.argv[2]))
except module.BuildTokenError:
    result = module.TOKEN_ERROR_EXIT
raise SystemExit(result)
"""


def load_helper():
    spec = importlib.util.spec_from_file_location("adk_build_token_test", HELPER_PATH)
    if spec is None or spec.loader is None:
        raise AssertionError(f"could not load {HELPER_PATH}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def driver_command(token_path: Path, command: list[str]) -> list[str]:
    return [sys.executable, "-c", DRIVER, str(HELPER_PATH), str(token_path), *command]


def write_executable(path: Path, body: str, *, interpreter: str = "python3") -> Path:
    path.write_text(f"#!/usr/bin/env {interpreter}\n{body}\n")
    path.chmod(0o700)
    return path


def wait_for_path(path: Path, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists():
            return
        time.sleep(0.02)
    raise AssertionError(f"timed out waiting for {path}")


def stop_process(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is None:
        process.terminate()
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=2)


def unquoted_shell_code(line: str) -> str:
    out: list[str] = []
    quote: str | None = None
    escaped = False
    for char in line:
        if escaped:
            out.append(" " if quote else char)
            escaped = False
            continue
        if char == "\\":
            escaped = True
            out.append(" " if quote else char)
            continue
        if quote:
            if char == quote:
                quote = None
            out.append(" ")
            continue
        if char in {"'", '"'}:
            quote = char
            out.append(" ")
            continue
        if char == "#":
            break
        out.append(char)
    return "".join(out)


def raw_cargo_sites(source: str) -> list[str]:
    violations: list[str] = []
    if "clean_cmd=(cargo clean" in source and '_with_build_token "${clean_cmd[@]}"' not in source:
        violations.append("cargo clean array executes without _with_build_token")
    for raw_line in source.splitlines():
        fragments = [raw_line]
        fragments.extend(match.group(1) for match in re.finditer(r"\$\((.*)\)", raw_line))
        for fragment in fragments:
            code = unquoted_shell_code(fragment).strip()
            if not re.search(r"(^|[^A-Za-z0-9_])cargo(?:\.exe)?(?:\s|$)", code):
                continue
            if re.search(r"\b_with_build_token\s+cargo(?:\.exe)?\b", code):
                continue
            if re.fullmatch(r"clean_cmd=\(cargo clean .+\)", code):
                continue
            if re.search(r"\bcommand\s+-v\s+cargo\b", code):
                continue
            violations.append(code)
    return violations


def workflow_block(text: str, exact_header: str) -> str:
    lines = text.splitlines()
    matches = [index for index, line in enumerate(lines) if line == exact_header]
    assert len(matches) == 1, f"expected one workflow header {exact_header!r}, found {len(matches)}"
    start, indent = matches[0], len(exact_header) - len(exact_header.lstrip())
    end = next((index for index in range(start + 1, len(lines)) if lines[index].strip() and len(lines[index]) - len(lines[index].lstrip()) <= indent), len(lines))
    while end > start + 1 and not lines[end - 1].strip():
        end -= 1
    return "\n".join(lines[start:end])


@unittest.skipUnless(os.name == "posix", "POSIX build-token supervisor")
class BuildTokenPosixBehaviorTests(unittest.TestCase):
    def test_two_commands_serialize_on_one_persistent_inode(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            token = root / "token.lock"
            first_started = root / "first.started"
            second_started = root / "second.started"
            first = write_executable(root / "cargo", "from pathlib import Path\nimport sys,time\nPath(sys.argv[1]).touch()\ntime.sleep(0.8)")
            second = write_executable(root / "cargo.exe", "from pathlib import Path\nimport sys\nPath(sys.argv[1]).touch()")
            first_process = subprocess.Popen(driver_command(token, [str(first), str(first_started)]))
            self.addCleanup(stop_process, first_process)
            wait_for_path(first_started)
            inode = token.stat().st_ino
            second_process = subprocess.Popen(driver_command(token, [str(second), str(second_started)]))
            self.addCleanup(stop_process, second_process)
            time.sleep(0.2)
            self.assertFalse(second_started.exists())
            self.assertEqual(first_process.wait(timeout=5), 0)
            self.assertEqual(second_process.wait(timeout=5), 0)
            self.assertEqual(token.stat().st_ino, inode)

    def test_jobs_forced_to_two_and_exact_exit_preserved(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            token = root / "token.lock"
            observed = root / "jobs"
            cargo = write_executable(root / "cargo", "from pathlib import Path\nimport os,sys\nPath(sys.argv[1]).write_text(os.environ.get('CARGO_BUILD_JOBS',''))\nraise SystemExit(37)")
            env = os.environ.copy()
            env["CARGO_BUILD_JOBS"] = "99"
            result = subprocess.run(driver_command(token, [str(cargo), str(observed)]), env=env, check=False)
            self.assertEqual(result.returncode, 37)
            self.assertEqual(observed.read_text(), "2")

    def test_all_explicit_cargo_jobs_forms_reject_before_open(self):
        helper = load_helper()
        forms = [["cargo", "build", "-j", "4"], ["cargo", "build", "-j4"], ["cargo", "build", "--jobs", "4"], ["cargo", "build", "--jobs=4"]]
        with tempfile.TemporaryDirectory() as tmp:
            token = Path(tmp) / "never-created.lock"
            for command in forms:
                with self.subTest(command=command):
                    with self.assertRaises(helper.BuildTokenError):
                        helper._supervise_for_test(command, token)
                    self.assertFalse(token.exists())

    def test_production_command_surface_is_direct_cargo_only(self):
        helper = load_helper()
        accepted = [["cargo", "build"], ["/toolchain/bin/cargo", "check"], ["cargo.exe", "test"]]
        rejected = [["bash", "-c", "cargo build"], ["env", "cargo", "build"], [str(HELPER_PATH), "--", "cargo", "build"], [str(DEPLOY_PATH)]]
        for command in accepted:
            helper._validate_command(command)
        for command in rejected:
            with self.subTest(command=command), self.assertRaises(helper.BuildTokenError):
                helper._validate_command(command)

    def test_nested_poison_is_rejection_only_and_spawns_nothing(self):
        helper = load_helper()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            token = root / "never-created.lock"
            sentinel = root / "spawned"
            cargo = write_executable(root / "cargo", "from pathlib import Path\nimport sys\nPath(sys.argv[1]).touch()")
            env = os.environ.copy()
            env[helper.NESTING_ENV] = "1"
            result = subprocess.run(driver_command(token, [str(cargo), str(sentinel)]), env=env, check=False)
            self.assertEqual(result.returncode, helper.TOKEN_ERROR_EXIT)
            self.assertFalse(token.exists())
            self.assertFalse(sentinel.exists())

    def test_foreground_cargo_cannot_reenter_production_helper(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            token = root / "token.lock"
            observed = root / "nested.rc"
            cargo = write_executable(
                root / "cargo",
                "from pathlib import Path\nimport subprocess,sys\n"
                "result=subprocess.run([sys.executable,sys.argv[1],'--','cargo','check'],check=False)\n"
                "Path(sys.argv[2]).write_text(str(result.returncode))",
            )
            result = subprocess.run(
                driver_command(token, [str(cargo), str(HELPER_PATH), str(observed)]),
                check=False,
            )
            self.assertEqual(result.returncode, 0)
            self.assertEqual(observed.read_text(), "73")

    def test_actual_lock_fd_never_reaches_child(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            token = root / "token.lock"
            observed = root / "fds"
            cargo = write_executable(
                root / "cargo",
                "from pathlib import Path\nimport os,sys\ntoken=os.stat(sys.argv[1])\nleaked=[]\nfor fd in range(3,128):\n  try: opened=os.fstat(fd)\n  except OSError: continue\n  if (opened.st_dev,opened.st_ino)==(token.st_dev,token.st_ino): leaked.append(fd)\nPath(sys.argv[2]).write_text(','.join(map(str,leaked)))",
            )
            result = subprocess.run(driver_command(token, [str(cargo), str(token), str(observed)]), check=False)
            self.assertEqual(result.returncode, 0)
            self.assertEqual(observed.read_text(), "")

    def test_background_descendant_cannot_extend_foreground_lease(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            token = root / "token.lock"
            first_started = root / "first.started"
            second_started = root / "second.started"
            first = write_executable(root / "cargo", '(sleep 3) &\nprintf started > "$1"', interpreter="bash")
            second = write_executable(root / "cargo.exe", "from pathlib import Path\nimport sys\nPath(sys.argv[1]).touch()")
            self.assertEqual(subprocess.run(driver_command(token, [str(first), str(first_started)]), check=False).returncode, 0)
            wait_for_path(first_started)
            second_process = subprocess.Popen(driver_command(token, [str(second), str(second_started)]))
            self.addCleanup(stop_process, second_process)
            wait_for_path(second_started, timeout=0.5)
            self.assertEqual(second_process.wait(timeout=2), 0)

    def test_waiting_cancellation_spawns_zero_commands(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            token = root / "token.lock"
            holder_started = root / "holder.started"
            waiter_spawned = root / "waiter.spawned"
            holder_cargo = write_executable(root / "cargo", "from pathlib import Path\nimport sys,time\nPath(sys.argv[1]).touch()\ntime.sleep(30)")
            waiter_cargo = write_executable(root / "cargo.exe", "from pathlib import Path\nimport sys\nPath(sys.argv[1]).touch()")
            holder = subprocess.Popen(driver_command(token, [str(holder_cargo), str(holder_started)]))
            self.addCleanup(stop_process, holder)
            wait_for_path(holder_started)
            waiter = subprocess.Popen(driver_command(token, [str(waiter_cargo), str(waiter_spawned)]))
            time.sleep(0.2)
            waiter.send_signal(signal.SIGTERM)
            self.assertEqual(waiter.wait(timeout=5), 128 + signal.SIGTERM)
            self.assertFalse(waiter_spawned.exists())

    def test_running_signals_forward_to_group_and_return_128_plus_signal(self):
        signals = [signal.SIGINT, signal.SIGTERM]
        if hasattr(signal, "SIGHUP"):
            signals.append(signal.SIGHUP)
        for signum in signals:
            with self.subTest(signum=signum), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                token = root / "token.lock"
                started = root / "started"
                cargo = write_executable(root / "cargo", "from pathlib import Path\nimport sys,time\nPath(sys.argv[1]).touch()\ntime.sleep(30)")
                process = subprocess.Popen(driver_command(token, [str(cargo), str(started)]))
                self.addCleanup(stop_process, process)
                wait_for_path(started)
                process.send_signal(signum)
                self.assertEqual(process.wait(timeout=5), 128 + signum)

    def test_ignored_term_is_killed_and_reaped_after_bounded_grace(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            token = root / "token.lock"
            started = root / "started"
            cargo = write_executable(
                root / "cargo",
                "from pathlib import Path\nimport signal,sys,time\n"
                "signal.signal(signal.SIGTERM,signal.SIG_IGN)\n"
                "Path(sys.argv[1]).touch()\ntime.sleep(30)",
            )
            process = subprocess.Popen(driver_command(token, [str(cargo), str(started)]))
            self.addCleanup(stop_process, process)
            wait_for_path(started)
            began = time.monotonic()
            process.send_signal(signal.SIGTERM)
            self.assertEqual(process.wait(timeout=5), 128 + signal.SIGTERM)
            elapsed = time.monotonic() - began
            self.assertGreaterEqual(elapsed, 1.5)
            self.assertLess(elapsed, 4.0)

    def test_unlink_recreate_makes_observed_stale_waiter_fail_closed(self):
        helper = load_helper()
        with tempfile.TemporaryDirectory() as tmp:
            token = Path(tmp) / "token.lock"
            token.touch(mode=0o600)
            holder_fd = os.open(token, os.O_RDWR | os.O_APPEND)
            self.addCleanup(os.close, holder_fd)
            fcntl.flock(holder_fd, fcntl.LOCK_EX)
            opened = threading.Event()
            outcome: list[BaseException | int] = []
            def wait_for_token() -> None:
                try:
                    outcome.append(helper._acquire_posix_token(token, on_open=opened.set))
                except BaseException as error:
                    outcome.append(error)
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
            self.assertIsInstance(outcome[0], helper.BuildTokenError)

    def test_first_open_rejects_symlink_without_touching_target(self):
        helper = load_helper()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            target = root / "target"
            target.write_text("unchanged")
            token = root / "token.lock"
            token.symlink_to(target)
            with self.assertRaises(helper.BuildTokenError):
                helper._acquire_posix_token(token)
            self.assertEqual(target.read_text(), "unchanged")


class BuildTokenContractTests(unittest.TestCase):
    def test_public_production_surface_has_no_token_path_override(self):
        helper = load_helper()
        self.assertFalse(hasattr(helper, "run_command"))
        self.assertNotIn("token_path", inspect.signature(helper.main).parameters)
        source = HELPER_PATH.read_text()
        test_seam_lines = [line for line in source.splitlines() if "_supervise_for_test(" in line]
        self.assertEqual(len(test_seam_lines), 1)

    def test_repo_only_facade_is_sourced_once_and_defaults_stages_cleanly(self):
        facade = FACADE_PATH.read_text()
        defaults = DEFAULTS_PATH.read_text()
        deploy = DEPLOY_PATH.read_text()
        build_release = BUILD_RELEASE_PATH.read_text()
        self.assertIn("_with_build_token()", facade)
        self.assertIn("raw Cargo fallback is forbidden", facade)
        self.assertNotRegex(facade, r"\|\|\s*cargo")
        self.assertNotIn("_with_build_token()", defaults)
        self.assertNotIn("build_token.py", defaults)
        self.assertEqual(deploy.count('. "$SCRIPT_DIR/_build_token.sh"'), 1)
        self.assertEqual(build_release.count('. "$SCRIPT_DIR/_build_token.sh"'), 1)
        self.assertIn('cp "scripts/_defaults.sh" "$STAGING/scripts/_defaults.sh"', build_release)
        self.assertIn('cp "$REPO/scripts/_defaults.sh" "$RELEASE_ROOT_SCRIPTS_STAGED/_defaults.sh"', deploy)

    def test_raw_cargo_inventory_is_exact_and_mutation_discriminating(self):
        deploy = DEPLOY_PATH.read_text()
        build_release = BUILD_RELEASE_PATH.read_text()
        self.assertEqual(raw_cargo_sites(deploy), [])
        self.assertEqual(raw_cargo_sites(build_release), [])
        mutations = [
            deploy.replace("_with_build_token cargo metadata", "cargo metadata", 1),
            deploy.replace("_with_build_token cargo build", "command cargo build", 1),
            deploy.replace('_with_build_token "${clean_cmd[@]}"', '"${clean_cmd[@]}"', 1),
            build_release.replace("_with_build_token cargo build", "env cargo build", 1),
            build_release + "\ncargo test --workspace\n",
        ]
        for mutated in mutations:
            with self.subTest(mutant=mutated[-80:]):
                self.assertNotEqual(raw_cargo_sites(mutated), [])

    def test_deploy_rejects_outer_token_before_detach_and_deploy_lock(self):
        deploy = DEPLOY_PATH.read_text()
        reject = deploy.index("\n_reject_inherited_build_token_for_deploy\n")
        detach = deploy.index("# --- macOS: always run detached")
        deploy_lock = deploy.rindex('_acquire_release_deploy_lock "$@"')
        self.assertLess(reject, detach)
        self.assertLess(detach, deploy_lock)

    def test_required_ci_and_source_of_truth_contracts_are_pinned(self):
        ci_script = CI_SCRIPT_PATH.read_text()
        source_of_truth = SOURCE_OF_TRUTH_PATH.read_text()
        workflow = WORKFLOW_PATH.read_text()
        self.assertIn("tests/test_build_token_5663.sh", ci_script)
        self.assertIn("Host-wide Cargo build token", source_of_truth)
        self.assertIn("cooperative", source_of_truth.lower())
        self.assertIn("third entrant", source_of_truth.lower())
        output = "      build_token_integration: ${{ steps.filter.outputs.build_token_integration }}"
        selector = "            build_token_integration: ['scripts/build_token.py', 'scripts/_build_token.sh', 'scripts/build-release.sh', 'scripts/deploy-release.sh', 'tests/test_build_token_5663.py', '.github/workflows/ci-pr.yml']"
        self.assertEqual(workflow_block(workflow, "    outputs:").splitlines().count(output), 1)
        self.assertEqual(workflow_block(workflow, "          filters: |").splitlines().count(selector), 1)
        job = workflow_block(workflow, "  build_token_integration:")
        mirror = workflow_block(workflow, "  build_token_integration_required_context:")
        self.assertEqual(sha256(job.encode()).hexdigest(), "95ad48cf0c569793c38e441c06341cc3083c973d2625513b1f82b0bb6bac9e99")
        self.assertEqual(sha256(mirror.encode()).hexdigest(), "172a3a79fa5dd431465a7265e16c236de1910bd8427b5bfd4a90473255bd3f3c")
        self.assertEqual(sha256((job + "\0" + mirror).encode()).hexdigest(), "2a506ae433086dd8ddd4b0d4155e4702379061cedd74f51e0ca7db462f6d7525")
        native = workflow_block(workflow, "  win32_build_token:")
        native_mirror = workflow_block(workflow, "  win32_build_token_required_context:")
        self.assertEqual(sha256(native.encode()).hexdigest(), "9967076fc22441fddebb330d21a4f996047147fa4a854cf7e127ab58d97a9753")
        self.assertEqual(sha256(native_mirror.encode()).hexdigest(), "d12fa02cb7cbd6b3a72ee4f4df2d148abba62abc9a1ee42f7da1e3bb88e296df")
        self.assertEqual(sha256((native + "\0" + native_mirror).encode()).hexdigest(), "c7959d0b8bd23e73c983d3969caf701b3bab283f66b3329f99e29813ddcf80e8")
        self.assertEqual(subprocess.check_output(["git", "hash-object", WIN_HELPER_PATH], text=True).strip(), "bc191deb1c965ed778ec661b50f1f92887b59280")
        self.assertEqual(subprocess.check_output(["git", "hash-object", WIN_TEST_PATH], text=True).strip(), "2c7aaef649c91b1b80608cd743d43a167838809e")
        probe = """
import builtins, importlib.util, sys
real_import = builtins.__import__
def guarded_import(name, *args, **kwargs):
    if name == "fcntl":
        raise AssertionError("fcntl imported on Win32")
    return real_import(name, *args, **kwargs)
builtins.__import__ = guarded_import
sys.platform = "win32"
spec = importlib.util.spec_from_file_location("probe", sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
"""
        result = subprocess.run([sys.executable, "-c", probe, __file__], check=False)
        self.assertEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
