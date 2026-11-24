"""Resource-only sampler behavior with Linux counter fixtures on any POSIX host."""
import base64
import hashlib
import http.server
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest import mock
import zipfile

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

    def process(self, pid, name, rss, hwm, args=()):
        self.write(self.proc / pid / "status", f"Name:\t{name}\nVmRSS:\t{rss * 1024} kB\nVmHWM:\t{hwm * 1024} kB\n")
        if args:
            self.write(self.proc / pid / "cmdline", "\0".join((*args, "")))

    def run_probe(self, *arguments, check=True):
        result = subprocess.run(["bash", str(SCRIPT), *arguments], env=self.env,
                                text=True, capture_output=True, timeout=30)
        if check:
            self.assertEqual(result.returncode, 0, result.stderr)
        return result

    def stop(self):
        if (self.root / "runner-memory/metadata.json").exists():
            self.run_probe("finish")

    def summary(self):
        return json.loads((self.root / "runner-memory/summary.json").read_text())

    def samples(self):
        return [json.loads(line) for line in (self.root / "runner-memory/samples.jsonl").read_text().splitlines()]

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

    def test_checkpoint_keeps_an_independent_copy_before_a_lost_build(self):
        self.env["CARGO_BUILD_JOBS"] = "2"
        self.run_probe("start")
        self.run_probe("checkpoint")
        checkpoint = self.root / "runner-memory-checkpoint"
        before = (checkpoint / "samples.jsonl").read_bytes()
        metadata = json.loads((checkpoint / "metadata.json").read_text())
        self.assertEqual(metadata["cargo_build_jobs"], "2")
        self.assertTrue(before)
        self.memory(available=8)
        self.run_probe("begin", "just lint")
        self.assertEqual((checkpoint / "samples.jsonl").read_bytes(), before)
        self.assertNotEqual((self.root / "runner-memory/samples.jsonl").read_bytes(), before)
        self.assertFalse((self.root / "runner-memory/stop").exists())
        self.assertNotIn(self.env["MEMORY_TEST_SECRET"], "".join(p.read_text() for p in checkpoint.iterdir()))

    def test_begin_uploads_a_copy_while_the_build_step_still_runs(self):
        requests, refusals, finalized = [], [500], threading.Event()

        class Artifacts(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                status = refusals.pop() if refusals else 200
                self.send_response(status)
                self.end_headers()
                if status != 200:
                    return
                requests.append((self.command + " " + self.path, self.headers.get("Authorization"), body))
                self.wfile.write(json.dumps({"ok": True, "signed_upload_url": origin + "/blob?sig=signed"}).encode())
                if self.path.endswith("/FinalizeArtifact"):
                    finalized.set()

            do_PUT = do_POST
            log_message = lambda *_: None

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Artifacts)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        origin = f"http://127.0.0.1:{server.server_port}"
        state = self.root / "runner-memory"
        self.run_probe("start")
        self.run_probe("begin", "setup")
        self.run_probe("end")
        # Jobs that do not opt in start no exporter.
        self.assertFalse((state / "export.log").exists())
        claims = base64.urlsafe_b64encode(b'{"scp": "Actions.Other Actions.Results:run-1:job-2"}').decode().rstrip("=")
        token = f"header.{claims}.signature"
        self.env.update(RUNNER_MEMORY_EXPORT_MINUTES="0 0.02", RUNNER_MEMORY_EXPORT_NAME="runner-memory-job-3-attempt-1",
                        ACTIONS_RESULTS_URL=origin + "/results/", ACTIONS_RUNTIME_TOKEN=token, no_proxy="*")
        self.run_probe("begin", "build")
        self.assertTrue(finalized.wait(20))
        # A refused upload is logged by type and status only, and the next copy still leaves.
        self.assertIn("-at-0m failed (HTTPError 500)", (state / "export.log").read_text())
        (create_call, create_auth, create), (blob_call, blob_auth, blob), (final_call, final_auth, final) = requests
        prefix = "POST /twirp/github.actions.results.api.v1.ArtifactService/"
        self.assertEqual((create_call, blob_call, final_call), (prefix + "CreateArtifact", "PUT /blob?sig=signed", prefix + "FinalizeArtifact"))
        self.assertEqual((create_auth, blob_auth, final_auth), ("Bearer " + token, None, "Bearer " + token))
        ids = {"workflow_run_backend_id": "run-1", "workflow_job_run_backend_id": "job-2", "name": "runner-memory-job-3-attempt-1-at-0.02m"}
        created = json.loads(create)
        self.assertRegex(created.pop("expires_at"), r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$")
        self.assertEqual(created, {**ids, "version": 4})
        self.assertEqual(json.loads(final), {**ids, "size": str(len(blob)), "hash": "sha256:" + hashlib.sha256(blob).hexdigest()})
        with zipfile.ZipFile(io.BytesIO(blob)) as archive:
            self.assertEqual(sorted(archive.namelist()), ["metadata.json", "phase.json", "samples.jsonl"])
            self.assertEqual(json.loads(archive.read("samples.jsonl").splitlines()[-1])["phase"], "build")
        self.assertFalse((state / "stop").exists())
        self.assertNotIn(token, "".join(path.read_text() for path in state.iterdir() if path.is_file()))

    def test_detached_sampler_does_not_hold_capture_pipes_and_stops_before_interval(self):
        self.env["RUNNER_MEMORY_INTERVAL_SECONDS"] = "60"
        # Captured pipes must close under the subprocess watchdog, before a full sample interval.
        self.run_probe("start")
        self.run_probe("finish")
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
        for row in self.samples():
            self.assertNotIn("largest_rss_process", row)
            self.assertNotIn("largest_hwm_process", row)

    def test_sample_attributes_rss_to_test_compiler_and_hwm_to_linker(self):
        self.process("100", "rustc", 10, 11, ("/toolchain/bin/rustc", "--crate-name", "agentdesk",
                                              "--crate-type=lib", "--test", "--out-dir", "/private/build path"))
        self.process("300", "ld.lld", 8, 14, ("/toolchain/bin/ld.lld", "/private/link input.o"))
        self.run_probe("start")
        row = self.samples()[0]
        self.assertEqual(row["largest_rss_bytes"], 10 * MIB)
        self.assertEqual(row["largest_hwm_bytes"], 14 * MIB)
        self.assertEqual(row["largest_rss_process"], {
            "comm": "rustc", "crate_name": "agentdesk", "crate_type": "lib", "test": True,
        })
        self.assertEqual(row["largest_hwm_process"], {"comm": "ld.lld"})

    def test_sample_attributes_hwm_to_non_test_compiler_and_rss_to_general_process(self):
        self.process("100", "rustc", 4, 12, ("rustc", "--crate-name=normal_build", "--crate-type", "rlib,cdylib",
                                             "--crate-type=staticlib", "--", "--test"))
        self.process("200", "python3", 6, 7, ("python3", "--test", "--crate-name", "ignored"))
        self.run_probe("start")
        row = self.samples()[0]
        self.assertEqual(row["largest_rss_process"], {"comm": "python3"})
        self.assertEqual(row["largest_hwm_process"], {
            "comm": "rustc", "crate_name": "normal_build", "crate_type": "rlib,cdylib,staticlib", "test": False,
        })

    def test_missing_rustc_cmdline_keeps_both_counters_and_comm(self):
        self.run_probe("start")
        self.run_probe("finish")
        row = self.samples()[0]
        self.assertEqual(row["largest_rss_bytes"], 4 * MIB)
        self.assertEqual(row["largest_hwm_bytes"], 6 * MIB)
        self.assertEqual(row["largest_rss_process"], {"comm": "rustc"})
        self.assertEqual(row["largest_hwm_process"], {"comm": "rustc"})
        self.assertEqual(self.summary()["status"], "complete")

    def test_finish_accepts_old_sample_rows_without_process_details(self):
        self.run_probe("start")
        self.run_probe("finish")
        expected = self.summary()
        rows = self.samples()
        for row in rows:
            row.pop("largest_rss_process", None)
            row.pop("largest_hwm_process", None)
        self.write(self.root / "runner-memory/samples.jsonl", "".join(json.dumps(row) + "\n" for row in rows))
        self.run_probe("finish")
        self.assertEqual(self.summary(), expected)

    def test_periodic_sampler_sees_peak_between_boundaries(self):
        state = self.root / "runner-memory"
        self.write(state / "metadata.json", json.dumps({"interval_seconds": 1}))
        self.run_probe("begin", "build")

        elapsed = 0.0

        def advance(seconds):
            nonlocal elapsed
            elapsed += seconds
            if elapsed == seconds:
                self.memory(available=8)
            elif elapsed >= 1.5:
                self.memory(available=60)
                (state / "stop").touch()

        clock = SimpleNamespace(monotonic=lambda: elapsed, sleep=advance)
        body = SCRIPT.read_text().split("<<'PY'\n", 1)[1].rsplit("\nPY\n", 1)[0]
        with mock.patch.dict(os.environ, self.env, clear=True), \
             mock.patch.object(sys, "argv", ["-", str(SCRIPT), "_sample"]), \
             mock.patch.dict(sys.modules, {"time": clock}):
            exec(compile(body, str(SCRIPT), "exec"), {})

        self.run_probe("end")
        self.run_probe("finish")
        phase = self.summary()["phases"]["build"]
        self.assertEqual(phase["samples"], 4)
        self.assertEqual(phase["max"]["host_used_bytes"], 56 * MIB)

    def test_finish_without_start_still_reports_unavailable(self):
        self.run_probe("finish")
        data = self.summary()
        self.assertEqual(data["status"], "unavailable")
        self.assertEqual(data["sampler_status"], "not_started")
        self.assertEqual(data["phases"], {})
        self.assertIsNone(data["host_total_bytes"])

    def test_self_hosted_scope_is_in_metadata_and_summary(self):
        self.env["RUNNER_ENVIRONMENT"] = "self-hosted"
        self.run_probe("start")
        self.run_probe("checkpoint")
        self.run_probe("finish")
        for folder, name in (
            ("runner-memory", "metadata.json"),
            ("runner-memory-checkpoint", "metadata.json"),
            ("runner-memory", "summary.json"),
        ):
            data = json.loads((self.root / folder / name).read_text())
            self.assertEqual(data["runner_environment"], "self-hosted")
            self.assertIs(data["shared_host"], True)
        self.assertIn("shared host", (self.root / "runner-memory/summary.md").read_text())

    def test_runner_scope_is_available_even_without_samples(self):
        for environment, expected in (("github-hosted", False), ("unrecognized", None)):
            with self.subTest(environment=environment):
                self.env["RUNNER_ENVIRONMENT"] = environment
                self.run_probe("finish")
                data = self.summary()
                self.assertEqual(data["status"], "unavailable")
                self.assertIs(data["shared_host"], expected)
                self.assertEqual(data["runner_environment"], environment if expected is not None else None)

    def test_artifacts_exclude_environment_unselected_names_and_cmdline_paths(self):
        secret = self.env["MEMORY_TEST_SECRET"]
        self.process("999", secret, 1, 1)
        private_path = "/private/build path"
        self.process("100", "rustc", 4, 6, (f"{private_path}/rustc", "--crate-name", private_path,
                                            "--crate-type=" + private_path, "--out-dir", f"/tmp/{secret}"))
        self.run_probe("start")
        self.run_probe("finish")
        for path in (self.root / "runner-memory").iterdir():
            if path.is_file():
                content = path.read_text()
                self.assertNotIn(secret, content, path.name)
                self.assertNotIn(private_path, content, path.name)

    def test_invalid_interval_fails_without_logging_environment(self):
        self.env["RUNNER_MEMORY_INTERVAL_SECONDS"] = "0"
        result = self.run_probe("start", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn(self.env["MEMORY_TEST_SECRET"], result.stderr)
        self.run_probe("finish")
        self.assertEqual(self.summary()["status"], "unavailable")


if __name__ == "__main__":
    unittest.main()
