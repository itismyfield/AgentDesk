"""Contracts for the hosted-runner swap safety net and the memory probe."""

from __future__ import annotations

import os
import re
import shlex
import signal
import stat
import subprocess
import tempfile
import textwrap
import time
import unittest
from pathlib import Path

import yaml


REPO_ROOT = Path(__file__).resolve().parents[1]
ENSURE_SWAP = REPO_ROOT / ".github/actions/ensure-swap/ensure-swap.sh"
MEM_MEASURE = REPO_ROOT / "scripts/ci/mem-measure.sh"
SWAP_ACTION = "./.github/actions/ensure-swap"

# Ubuntu jobs that link the lib test binary or run workspace clippy, per workflow.
EXPECTED_SWAP_JOBS = {
    ".github/workflows/ci-pr.yml": {
        "test_fast",
        "high-risk-recovery",
        "library_sweep",
        "lint",
        "scripts",
        "relay_authority_targets",
        "relay_authority_mutations",
    },
    ".github/workflows/ci-main.yml": {
        "full_non_pg",
        "lint",
        "postgres",
        "high-risk-recovery",
        "scripts",
    },
    ".github/workflows/ci-nightly.yml": {
        "postgres_full",
        "multinode_regression",
        "high_risk_recovery_full",
        "relay_authority_mutations_full",
    },
}
LIB_BUILD_MARKERS = (
    re.compile(r"cargo test\b[^\n]*--lib\b"),
    re.compile(r"\bjust test-postgres"),
    re.compile(r"--observe-selection"),
    re.compile(r"run_relay_authority_mutations\.sh"),
    re.compile(r"check_relay_authority_contract\.py"),
)
CLIPPY_MARKER = re.compile(r"\bjust lint\b|\bcargo clippy\b")


def needs_swap(step: dict, job: dict) -> bool:
    run = str(step.get("run", ""))
    if CLIPPY_MARKER.search(run) or any(marker.search(run) for marker in LIB_BUILD_MARKERS):
        return True
    # The cargo Script checks shard runs --verify-lib-inventory.
    shard = (step.get("env") or {}).get("SCRIPT_CHECK_SHARD")
    return "ci-script-checks.sh" in run and shard == "cargo"


def expected_df(target: Path) -> str:
    """The df -h line a fake df prints: /, /mnt (or its parent), nearest existing target."""
    mnt = "/mnt" if os.path.exists("/mnt") else "/"
    while not target.exists():
        target = target.parent
    return f"DF-H / {mnt} {target}"


# Prints the paths df -h was asked about; the -Pm free-space probe gets a table.
FAKE_DF_H = '[ "$1" = "-h" ] && { shift; echo "DF-H $*"; exit 0; }\n'


def write_tool(directory: Path, name: str, body: str) -> None:
    path = directory / name
    path.write_text("#!/usr/bin/env bash\n" + textwrap.dedent(body), "utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)


class SwapStepWiring(unittest.TestCase):
    def test_every_ubuntu_lib_build_job_adds_swap_before_its_first_cargo_step(self) -> None:
        for relative, expected in EXPECTED_SWAP_JOBS.items():
            jobs = yaml.safe_load((REPO_ROOT / relative).read_text("utf-8"))["jobs"]
            discovered = {
                job_id for job_id, job in jobs.items()
                if "ubuntu" in str(job.get("runs-on", ""))
                and any(needs_swap(step, job) for step in job.get("steps", []))
            }
            self.assertEqual(discovered, expected, relative)
            for job_id in sorted(expected):
                with self.subTest(workflow=relative, job=job_id):
                    steps = jobs[job_id]["steps"]
                    swaps = [i for i, s in enumerate(steps) if s.get("uses") == SWAP_ACTION]
                    self.assertEqual(len(swaps), 1, "exactly one ensure-swap step")
                    swap = swaps[0]
                    checkout = next(
                        i for i, s in enumerate(steps)
                        if str(s.get("uses", "")).startswith("actions/checkout@")
                    )
                    first_cargo = min(
                        i for i, s in enumerate(steps)
                        if "cargo" in str(s.get("run", "")) or needs_swap(s, jobs[job_id])
                    )
                    self.assertLess(checkout, swap, "local action needs the checkout")
                    self.assertLess(swap, first_cargo, "swap must exist before the build")
                    first_build = next(s for s in steps if needs_swap(s, jobs[job_id]))
                    self.assertIn(steps[swap].get("if"), (None, first_build.get("if")))

    def test_observe_steps_report_memory_without_changing_their_pipeline(self) -> None:
        jobs = yaml.safe_load(
            (REPO_ROOT / ".github/workflows/ci-pr.yml").read_text("utf-8")
        )["jobs"]
        for job_id in ("test_fast", "high-risk-recovery"):
            with self.subTest(job=job_id):
                observe = next(
                    s for s in jobs[job_id]["steps"]
                    if s.get("name") == "Observe curated lane selections"
                )
                lines = [line.strip() for line in observe["run"].splitlines() if line.strip()]
                self.assertEqual(lines[0], "set -o pipefail")
                self.assertRegex(
                    lines[1],
                    r"^bash scripts/ci/mem-measure\.sh \S+ -- python3 "
                    r"scripts/check_test_target_integrity\.py --observe-selection .* \| tee ",
                )


class EnsureSwapBehavior(unittest.TestCase):
    def run_swap(self, *, os_name="Linux", swap_mib=4096, avail_mib=70000,
                 fail_swapon=False, size_gb="16"):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        tmp = Path(scratch.name)
        bin_dir = tmp / "bin"
        bin_dir.mkdir()
        log = tmp / "calls.log"
        write_tool(bin_dir, "uname", f'echo "{os_name}"\n')
        write_tool(bin_dir, "free", f"""\
            echo "              total        used        free"
            echo "Mem:          15990        2000       13990"
            echo "Swap:         {swap_mib}           0        {swap_mib}"
        """)
        write_tool(bin_dir, "df", FAKE_DF_H + f"""\
            echo "Filesystem 1048576-blocks Used Available Capacity Mounted on"
            echo "/dev/sdb1 75000 5000 {avail_mib} 7% /mnt"
        """)
        write_tool(bin_dir, "sudo", f"""\
            [ "$1" = "-n" ] && shift
            echo "$*" >> "{log}"
            exec "$@"
        """)
        write_tool(bin_dir, "fallocate", ': > "$3"\n')
        write_tool(bin_dir, "mkswap", "exit 0\n")
        write_tool(bin_dir, "swapon", f"""\
            [ "$#" -eq 0 ] || [ "$1" = "--show" ] && {{ echo "NAME TYPE SIZE"; exit 0; }}
            {"exit 1" if fail_swapon else "exit 0"}
        """)
        swap_path = tmp / "mnt" / "agentdesk-swapfile"
        swap_path.parent.mkdir()
        (tmp / "ws").mkdir()
        self.target = tmp / "ws" / "target"
        env = {
            **os.environ,
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "ENSURE_SWAP_SIZE_GB": size_gb,
            "ENSURE_SWAP_PATH": str(swap_path),
            "CARGO_TARGET_DIR": str(self.target),
        }
        proc = subprocess.run(["bash", str(ENSURE_SWAP)], env=env, text=True,
                              capture_output=True, check=False)
        calls = log.read_text("utf-8").splitlines() if log.exists() else []
        return proc, calls, swap_path

    def test_short_swap_gets_a_swapfile_of_the_requested_size(self) -> None:
        proc, calls, path = self.run_swap(swap_mib=4096)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(calls, [
            f"fallocate -l 16G {path}",
            f"chmod 600 {path}",
            f"mkswap {path}",
            f"swapon {path}",
        ])
        self.assertIn("Swap:", proc.stdout)
        self.assertNotIn("::warning", proc.stdout)

    def test_enough_swap_is_left_alone(self) -> None:
        proc, calls, path = self.run_swap(swap_mib=16384)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(calls, [])
        self.assertFalse(path.exists())
        self.assertIn("Swap:", proc.stdout)

    def test_memory_and_disk_are_recorded_before_the_build(self) -> None:
        for swap_mib in (4096, 16384):
            with self.subTest(swap_mib=swap_mib):
                proc, _, _ = self.run_swap(swap_mib=swap_mib)
                self.assertEqual(proc.returncode, 0, proc.stderr)
                lines = proc.stdout.splitlines()
                self.assertIn(expected_df(self.target), lines)
                self.assertTrue(any(line.startswith("Swap:") for line in lines))

    def test_failed_swapon_warns_removes_its_file_and_keeps_the_job(self) -> None:
        proc, calls, path = self.run_swap(fail_swapon=True)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("::warning title=ensure-swap::", proc.stdout)
        self.assertEqual(calls[-1], f"rm -f {path}")
        self.assertFalse(path.exists())

    def test_short_disk_warns_without_allocating(self) -> None:
        proc, calls, path = self.run_swap(avail_mib=10000)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("::warning title=ensure-swap::", proc.stdout)
        self.assertEqual(calls, [])

    def test_non_linux_runner_is_skipped(self) -> None:
        proc, calls, _ = self.run_swap(os_name="Darwin", swap_mib=0)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(calls, [])
        self.assertNotIn("::warning", proc.stdout)


class MemMeasureBehavior(unittest.TestCase):
    def measure_env(self, *, gnu_time: bool, proc_root: Path | None = None,
                    tools: dict[str, str] | None = None) -> tuple[Path, dict[str, str]]:
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        tmp = Path(scratch.name)
        bin_dir = tmp / "bin"
        bin_dir.mkdir()
        write_tool(bin_dir, "free", 'echo "Swap:  16384  512  15872"\n')
        write_tool(bin_dir, "df", FAKE_DF_H)
        for name, body in (tools or {}).items():
            write_tool(bin_dir, name, body)
        self.target = tmp / "target"
        runner_temp = tmp / "runner-temp"
        runner_temp.mkdir()
        fake_time = bin_dir / "gnu-time"
        # Mirrors GNU time: -v -o FILE [--] CMD, exit status of CMD.
        write_tool(bin_dir, "gnu-time", """\
            [ "$1" = "-v" ] && [ "$2" = "-o" ] || exit 64
            out="$3"; shift 3; [ "$1" = "--" ] && shift
            "$@"; rc=$?
            echo "	Maximum resident set size (kbytes): 4242" > "$out"
            exit "$rc"
        """)
        env = {
            **os.environ,
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "MEM_MEASURE_TIME_BIN": str(fake_time) if gnu_time else str(tmp / "missing"),
            "RUNNER_TEMP": str(runner_temp),
            "CARGO_TARGET_DIR": str(self.target),
        }
        if proc_root is not None:
            env["MEM_MEASURE_PROC_ROOT"] = str(proc_root)
        return runner_temp, env

    def run_measure(self, command: str, *, gnu_time: bool, proc_root: Path | None = None,
                    tools: dict[str, str] | None = None):
        self.runner_temp, env = self.measure_env(gnu_time=gnu_time, proc_root=proc_root, tools=tools)
        # A wrapper that never returns fails here instead of hanging the suite.
        return subprocess.run(
            ["bash", str(MEM_MEASURE), "probe", "--", "bash", "-c", command],
            env=env, capture_output=True, check=False, timeout=30,
        )

    def fake_proc(self, rows: tuple[tuple[int, str, int], ...]) -> Path:
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        proc = Path(scratch.name) / "proc"
        for pid, name, hwm in rows:
            (proc / str(pid)).mkdir(parents=True)
            (proc / str(pid) / "status").write_text(
                f"Name:\t{name}\nPid:\t{pid}\nVmHWM:\t{hwm} kB\n", "utf-8")
        return proc

    def test_stdout_and_status_are_the_commands_own(self) -> None:
        for gnu_time in (True, False):
            for status in (0, 3):
                with self.subTest(gnu_time=gnu_time, status=status):
                    proc = self.run_measure(
                        f"printf 'evidence line\\n'; exit {status}", gnu_time=gnu_time
                    )
                    self.assertEqual(proc.stdout, b"evidence line\n")
                    self.assertEqual(proc.returncode, status)
                    self.assertIn(f"mem-measure probe: rc={status}".encode(), proc.stderr)
                    self.assertIn(b"Swap:", proc.stderr)

    def test_memory_and_disk_are_recorded_after_the_build_even_when_it_fails(self) -> None:
        for status in (0, 3):
            with self.subTest(status=status):
                proc = self.run_measure(f"exit {status}", gnu_time=True)
                self.assertEqual(proc.returncode, status)
                lines = proc.stderr.decode().splitlines()
                summary = lines.index(next(l for l in lines if l.startswith("mem-measure probe:")))
                self.assertIn(expected_df(self.target), lines[summary + 1:])

    def test_peak_rss_is_reported_from_gnu_time(self) -> None:
        proc = self.run_measure("exit 0", gnu_time=True)
        self.assertIn(b"max_rss_kib=4242", proc.stderr)
        fallback = self.run_measure("exit 0", gnu_time=False)
        self.assertIn(b"max_rss_kib=unavailable", fallback.stderr)

    def test_peak_processes_are_named_largest_first_with_the_rustc_crate(self) -> None:
        proc = self.fake_proc(((101, "rustc", 2097152), (102, "rust-lld", 3000),
                               (103, "cargo", 500), (104, "rustc", 40)))
        (proc / "101" / "cmdline").write_bytes(b"rustc\0--crate-name\0agentdesk\0--test\0")
        # A dangling status link reads like a process that exits between the glob and the read.
        (proc / "105").mkdir()
        (proc / "105" / "status").symlink_to(proc / "105" / "exited")
        globbed = subprocess.run(
            ["bash", "-c", 'cat "$1"/[0-9]*/status >/dev/null 2>&1', "_", str(proc)], check=False)
        self.assertNotEqual(globbed.returncode, 0, "the fixture must make one status read fail")
        sampled = self.run_measure("exit 0", gnu_time=True, proc_root=proc)
        self.assertIn(b"peak_sampler=ok peak_procs=rustc/agentdesk+test=2097152,"
                      b"rust-lld=3000,cargo=500\n", sampled.stderr)
        missing = self.run_measure("exit 0", gnu_time=True, proc_root=proc / "missing")
        self.assertIn(b"peak_sampler=off peak_procs=unavailable\n", missing.stderr)

    def test_names_keep_spaces_and_a_later_crate_read_replaces_an_unknown_rustc(self) -> None:
        proc = self.fake_proc(((101, "rustc", 2097152), (102, "worker with spc", 3000)))
        cmdline = shlex.quote(str(proc / "101" / "cmdline"))
        # Supplies the cmdline only after a sample has recorded the rustc as unresolved.
        command = (
            "for _ in $(seq 100); do "
            "grep -qF 'rustc/?' \"$RUNNER_TEMP\"/mem-measure-peaks.?????? 2>/dev/null && "
            "{ echo seen; break; }; sleep 0.1; done; "
            f"printf 'rustc\\0--crate-name\\0agentdesk\\0--test\\0' > {cmdline}"
        )
        result = self.run_measure(command, gnu_time=True, proc_root=proc)
        self.assertEqual(result.stdout, b"seen\n")
        self.assertIn(b"peak_procs=rustc/agentdesk+test=2097152,worker with spc=3000\n",
                      result.stderr)

    def test_a_stop_file_that_cannot_be_created_does_not_hold_the_command_status(self) -> None:
        proc = self.fake_proc(((101, "cargo", 500),))
        # touch failing as on ENOSPC must not keep the wrapper from returning.
        result = self.run_measure("printf 'command-done\\n'; exit 3", gnu_time=True,
                                  proc_root=proc, tools={"touch": "exit 1\n"})
        self.assertEqual(result.stdout, b"command-done\n")
        self.assertEqual(result.returncode, 3)
        self.assertIn(b"mem-measure probe: rc=3 ", result.stderr)

    def test_losing_the_temp_dir_mid_run_still_returns_the_command_status(self) -> None:
        proc = self.fake_proc(((101, "cargo", 500),))
        # Renaming works as root too, unlike dropping write permission.
        result = self.run_measure('mv "$RUNNER_TEMP" "$RUNNER_TEMP.gone"; exit 3',
                                  gnu_time=True, proc_root=proc)
        self.assertEqual(result.returncode, 3)
        self.assertIn(b"mem-measure probe: rc=3 ", result.stderr)
        self.assertIn(b" peak_sampler=incomplete ", result.stderr)

    def test_a_sampler_that_will_not_stop_is_killed_with_its_children_in_bounded_time(self) -> None:
        proc = self.fake_proc(((101, "cargo", 500),))
        pids = proc.parent / "stuck.pids"
        stuck_cat = (
            f'case "$*" in */status*) echo $$ >> {shlex.quote(str(pids))}; exec sleep 60 ;; esac\n'
            'exec /bin/cat "$@"\n'
        )
        started = time.monotonic()
        result = self.run_measure("exit 0", gnu_time=True, proc_root=proc, tools={"cat": stuck_cat})
        self.assertLess(time.monotonic() - started, 20)
        self.assertEqual(result.returncode, 0)
        self.assertIn(b" peak_sampler=killed ", result.stderr)
        stuck = [int(line) for line in pids.read_text("utf-8").split()]
        self.assertTrue(stuck)
        deadline = time.monotonic() + 5
        for pid in stuck:
            while time.monotonic() < deadline:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.05)
            else:
                self.fail(f"sampler child {pid} outlived the wrapper")

    def test_a_signal_to_the_wrapper_is_passed_on_after_the_command_and_cleanup(self) -> None:
        proc = self.fake_proc(((101, "cargo", 500),))
        runner_temp, env = self.measure_env(gnu_time=True, proc_root=proc)
        ready = runner_temp.parent / "ready"
        command = f": > {shlex.quote(str(ready))}; sleep 1; printf 'done\\n'"
        wrapper = subprocess.Popen(
            ["bash", str(MEM_MEASURE), "probe", "--", "bash", "-c", command],
            env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        deadline = time.monotonic() + 10
        while not ready.exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        wrapper.send_signal(signal.SIGTERM)
        stdout, stderr = wrapper.communicate(timeout=30)
        self.assertEqual(wrapper.returncode, -signal.SIGTERM)
        self.assertEqual(stdout, b"done\n")
        self.assertIn(b"mem-measure probe: rc=0 ", stderr)
        self.assertEqual(sorted(os.listdir(runner_temp)), [])

if __name__ == "__main__":
    unittest.main()
