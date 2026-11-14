"""Resource-only sampler behavior with Linux counter fixtures on any POSIX host."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/ci/runner-memory.sh"
MIB = 1048576


class RunnerMemoryTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.proc = self.root / "proc"
        self.cgroup = self.root / "cgroup"
        (self.proc / "self").mkdir(parents=True)
        (self.cgroup / "build").mkdir(parents=True)
        self.write(self.proc / "self/cgroup", "0::/build\n")
        self.memory()
        self.process("100", "rustc", 4, 6)
        self.process("200", "python3", 2, 3)
        for path, value in {
            "memory.max": 64 * MIB, "build/memory.max": 256 * MIB,
            "build/memory.current": 12 * MIB, "build/memory.peak": 20 * MIB,
        }.items():
            self.write(self.cgroup / path, str(value))
        self.env = {
            **os.environ, "RUNNER_TEMP": str(self.root),
            "RUNNER_MEMORY_PROC_ROOT": str(self.proc),
            "RUNNER_MEMORY_CGROUP_ROOT": str(self.cgroup),
            "GITHUB_STEP_SUMMARY": str(self.root / "job-summary.md"),
            "MEMORY_TEST_SECRET": "must-never-appear-in-memory-artifacts",
        }
        self.env.pop("RUNNER_MEMORY_INTERVAL_SECONDS", None)
        self.addCleanup(self.stop)

    def write(self, path, value):
        path.parent.mkdir(parents=True, exist_ok=True)
        temporary = path.with_suffix(".new")
        temporary.write_text(value)
        temporary.replace(path)

    def memory(self, available=48, swap_free=12):
        self.write(self.proc / "meminfo", f"MemTotal: 65536 kB\nMemAvailable: {available * 1024} kB\nSwapTotal: 16384 kB\nSwapFree: {swap_free * 1024} kB\n")

    def process(self, pid, name, rss, hwm):
        self.write(self.proc / pid / "status", f"Name:\t{name}\nVmRSS:\t{rss * 1024} kB\nVmHWM:\t{hwm * 1024} kB\n")

    def run_probe(self, *arguments, check=True):
        result = subprocess.run(["bash", str(SCRIPT), *arguments], env=self.env,
                                text=True, capture_output=True, timeout=5)
        if check:
            self.assertEqual(result.returncode, 0, result.stderr)
        return result

    def stop(self):
        if (self.root / "runner-memory/metadata.json").exists():
            self.run_probe("finish")

    def summary(self):
        return json.loads((self.root / "runner-memory/summary.json").read_text())

    def test_step_boundaries_capture_short_steps_and_keep_lifetime_peaks_distinct(self):
        self.run_probe("start")
        self.run_probe("begin", "Archive, list, compare and plan")
        self.memory(available=24, swap_free=4)
        self.process("300", "rustc", 8, 10)
        self.run_probe("end")
        self.memory(available=60, swap_free=16)
        self.run_probe("begin", "just lint")
        self.run_probe("end")
        self.run_probe("finish")
        data = self.summary()
        self.assertEqual(data["status"], "complete")
        build = data["phases"]["Archive, list, compare and plan"]
        lint = data["phases"]["just lint"]
        self.assertGreaterEqual(build["samples"], 2)
        self.assertEqual(build["max"]["host_used_bytes"], 40 * MIB)
        self.assertEqual(build["max"]["swap_used_bytes"], 12 * MIB)
        self.assertEqual(build["max"]["rss_sum_bytes"], 14 * MIB)
        self.assertEqual(build["max"]["largest_rss_bytes"], 8 * MIB)
        self.assertEqual(build["max"]["largest_hwm_bytes"], 10 * MIB)
        self.assertEqual(build["max"]["rustc_processes"], 2)
        self.assertEqual(lint["max"]["host_used_bytes"], 4 * MIB)
        self.assertEqual(lint["max"]["swap_used_bytes"], 0)
        self.assertEqual(data["cgroup_limit_bytes"], 64 * MIB)
        self.assertEqual(data["lifetime_cgroup_peak_bytes"], 20 * MIB)
        self.assertIn("not a per-step peak", (self.root / "job-summary.md").read_text())

    def test_detached_sampler_does_not_hold_capture_pipes_and_stops_before_interval(self):
        before = time.monotonic()
        self.run_probe("start")
        self.assertLess(time.monotonic() - before, 2)
        before = time.monotonic()
        self.run_probe("finish")
        self.assertLess(time.monotonic() - before, 2)
        self.assertEqual(self.summary()["sampler_status"], "stopped")
        done = json.loads((self.root / "runner-memory/done.json").read_text())
        self.assertEqual(done["status"], "stopped")

    def test_absent_counters_are_partial_and_null_not_zero(self):
        self.env["RUNNER_MEMORY_PROC_ROOT"] = str(self.root / "missing-proc")
        self.env["RUNNER_MEMORY_CGROUP_ROOT"] = str(self.root / "missing-cgroup")
        self.run_probe("start")
        self.run_probe("begin", "build")
        self.run_probe("end")
        self.run_probe("finish")
        data = self.summary()
        self.assertEqual(data["status"], "partial")
        self.assertIsNone(data["host_total_bytes"])
        self.assertIsNone(data["phases"]["build"]["max"]["rustc_processes"])
        self.assertIn("meminfo_unavailable", data["errors"])
        self.assertIn("unavailable", (self.root / "job-summary.md").read_text())

    def test_periodic_sampler_sees_peak_between_boundaries(self):
        self.env["RUNNER_MEMORY_INTERVAL_SECONDS"] = "0.05"
        self.run_probe("start")
        self.run_probe("begin", "build")
        self.memory(available=8)
        time.sleep(0.2)
        self.memory(available=60)
        self.run_probe("end")
        self.run_probe("finish")
        phase = self.summary()["phases"]["build"]
        self.assertGreaterEqual(phase["samples"], 4)
        self.assertEqual(phase["max"]["host_used_bytes"], 56 * MIB)

    def test_finish_without_start_still_reports_unavailable(self):
        self.run_probe("finish")
        data = self.summary()
        self.assertEqual(data["status"], "unavailable")
        self.assertEqual(data["sampler_status"], "not_started")
        self.assertEqual(data["phases"], {})
        self.assertIsNone(data["host_total_bytes"])

    def test_only_resource_fields_are_written_without_environment_or_process_names(self):
        secret = self.env["MEMORY_TEST_SECRET"]
        self.process("999", secret, 1, 1)
        self.run_probe("start")
        self.run_probe("finish")
        for path in (self.root / "runner-memory").iterdir():
            if path.is_file():
                self.assertNotIn(secret, path.read_text(), path.name)

    def test_invalid_interval_fails_without_logging_environment(self):
        self.env["RUNNER_MEMORY_INTERVAL_SECONDS"] = "0"
        result = self.run_probe("start", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn(self.env["MEMORY_TEST_SECRET"], result.stderr)
        self.run_probe("finish")
        self.assertEqual(self.summary()["status"], "unavailable")


if __name__ == "__main__":
    unittest.main()
