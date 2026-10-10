import json
import unittest
import os
from pathlib import Path
import subprocess
import tempfile
from scripts.check_clippy_warning_count import measure

ROOT = Path(__file__).resolve().parents[1]
RUSTC = "rustc 1.94.1 (e408947bf 2026-03-25)\nrelease: 1.94.1\nhost: x86_64-unknown-linux-gnu\ncommit-hash: e408947bfd200af42db322daf0fadfe7e26d3bd1\n"
CLIPPY = "clippy 0.1.94 (e408947bf 2026-03-25)\n"


def stream(*events):
    return "\n".join(json.dumps(event) for event in events)


class WarningCountTest(unittest.TestCase):
    def test_valid_zero_and_warning_count(self):
        finish = {"reason": "build-finished", "success": True}
        warning = {"reason": "compiler-message", "message": {"level": "warning"}}
        self.assertEqual(measure(stream(finish)), 0)
        self.assertEqual(measure(stream(warning, warning, finish)), 2)

    def test_invalid_is_not_zero(self):
        for text in ("", "garbage", "[]", stream({"reason": "build-finished", "success": False}), stream({"reason": "compiler-message"}), stream({"reason": "compiler-message", "message": {"level": "error"}}), stream({"reason": "build-finished", "success": True}, {"reason": "build-finished", "success": True})):
            with self.subTest(text=text), self.assertRaises((ValueError, KeyError, TypeError)):
                measure(text)


class WarningGateTest(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.git("init", "-q")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.com")
        (self.root / "scripts").mkdir()
        self.baseline = self.root / "scripts/clippy_warning_baseline.json"
        self.data = json.loads((ROOT / "scripts/clippy_warning_baseline.json").read_text())
        self.baseline.write_text(json.dumps(self.data))
        self.git("add", ".")
        self.git("commit", "-qm", "baseline")
        self.base = self.git("rev-parse", "HEAD")
        (self.root / "bin").mkdir()
        for name, variable in (("rustc", "RUSTC_VERSION"), ("cargo", "CLIPPY_VERSION")):
            path = self.root / "bin" / name
            path.write_text('#!/bin/bash\nprintf "%s" "$' + variable + '"\n')
            path.chmod(0o755)
        self.env = dict(os.environ, PATH=str(self.root / "bin") + ":" + os.environ["PATH"],
                        RUSTC_VERSION=RUSTC, CLIPPY_VERSION=CLIPPY, RUNNER_OS="Linux")

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.root), *args], text=True).strip()

    def run_gate(self, count=952, payload=None):
        warning = {"reason": "compiler-message", "message": {"level": "warning"}}
        diagnostics = self.root / "diagnostics.jsonl"
        diagnostics.write_text(payload if payload is not None else stream(*([warning] * count), {"reason": "build-finished", "success": True}))
        result = subprocess.run(["python3", str(ROOT / "scripts/check_clippy_warning_count.py"),
                                 "--input", str(diagnostics), "--output", str(self.root / "report.json"),
                                 "--baseline", str(self.baseline), "--base-ref", self.base],
                                cwd=self.root, env=self.env, capture_output=True, text=True)
        report_path = self.root / "report.json"
        return result, json.loads(report_path.read_text()) if report_path.exists() else {}

    def test_gate_counts_actual_diagnostics_and_blocks_growth(self):
        for count, rc in ((952, 0), (951, 0), (953, 1)):
            with self.subTest(count=count):
                result, report = self.run_gate(count)
                self.assertEqual(result.returncode, rc, result.stderr)
                self.assertTrue(report["valid"])
                self.assertEqual(report["warnings"], count)
                self.assertEqual(report["gate"], "PASS" if rc == 0 else "FAIL")

    def test_invalid_stream_and_toolchain_are_not_pass_or_zero(self):
        for payload, rustc, clippy in (("garbage", RUSTC, CLIPPY), (stream({"reason": "build-finished", "success": True}), RUSTC.replace("1.94.1", "1.95.0"), CLIPPY), (stream({"reason": "build-finished", "success": True}), RUSTC, CLIPPY.replace("e408947bf", "000000000"))):
            with self.subTest(payload=payload, rustc=rustc, clippy=clippy):
                self.env.update(RUSTC_VERSION=rustc, CLIPPY_VERSION=clippy)
                result, report = self.run_gate(payload=payload)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertFalse(report["valid"])
                self.assertNotEqual(report.get("gate"), "PASS")

    def test_baseline_increase_is_rejected_even_if_count_fits(self):
        self.data["warnings"] = 953
        self.baseline.write_text(json.dumps(self.data))
        result, report = self.run_gate(953)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("baseline increase", report["error"])

    def test_reviewed_decrease_and_initial_baseline_are_allowed(self):
        self.data["warnings"] = 951
        self.baseline.write_text(json.dumps(self.data))
        self.assertEqual(self.run_gate(951)[0].returncode, 0)
        self.git("rm", "--cached", "scripts/clippy_warning_baseline.json")
        (self.root / "placeholder").write_text("bootstrap")
        self.git("add", "placeholder")
        self.git("commit", "-qm", "without baseline")
        self.base = self.git("rev-parse", "HEAD")
        self.assertEqual(self.run_gate(951)[0].returncode, 0)

    def test_bad_base_and_invalid_baseline_fail_closed(self):
        self.base = "missing"
        self.assertEqual(self.run_gate()[0].returncode, 1)
        self.base = self.git("rev-parse", "HEAD")
        for count in (True, -1, "952"):
            self.data["warnings"] = count
            self.baseline.write_text(json.dumps(self.data))
            self.assertEqual(self.run_gate()[0].returncode, 1)
