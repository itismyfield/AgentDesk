"""Contracts for the hosted-runner swap safety net and the memory probe."""

from __future__ import annotations

import os
import re
import stat
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

import yaml


REPO_ROOT = Path(__file__).resolve().parents[1]
ENSURE_SWAP = REPO_ROOT / ".github/actions/ensure-swap/ensure-swap.sh"
MEM_MEASURE = REPO_ROOT / "scripts/ci/mem-measure.sh"
SWAP_ACTION = "./.github/actions/ensure-swap"

# Ubuntu jobs that link the lib test binary, per workflow.
EXPECTED_SWAP_JOBS = {
    ".github/workflows/ci-pr.yml": {
        "test_fast",
        "high-risk-recovery",
        "library_sweep",
        "scripts",
        "relay_authority_targets",
        "relay_authority_mutations",
    },
    ".github/workflows/ci-main.yml": {
        "full_non_pg",
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


def builds_lib_tests(step: dict, job: dict) -> bool:
    run = str(step.get("run", ""))
    if any(marker.search(run) for marker in LIB_BUILD_MARKERS):
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
                and any(builds_lib_tests(step, job) for step in job.get("steps", []))
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
                        if "cargo" in str(s.get("run", "")) or builds_lib_tests(s, jobs[job_id])
                    )
                    self.assertLess(checkout, swap, "local action needs the checkout")
                    self.assertLess(swap, first_cargo, "swap must exist before the build")
                    first_build = next(s for s in steps if builds_lib_tests(s, jobs[job_id]))
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
    def run_measure(self, command: str, *, gnu_time: bool):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        tmp = Path(scratch.name)
        bin_dir = tmp / "bin"
        bin_dir.mkdir()
        write_tool(bin_dir, "free", 'echo "Swap:  16384  512  15872"\n')
        write_tool(bin_dir, "df", FAKE_DF_H)
        self.target = tmp / "target"
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
            "RUNNER_TEMP": str(tmp),
            "CARGO_TARGET_DIR": str(self.target),
        }
        return subprocess.run(
            ["bash", str(MEM_MEASURE), "probe", "--", "bash", "-c", command],
            env=env, capture_output=True, check=False,
        )

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


if __name__ == "__main__":
    unittest.main()
