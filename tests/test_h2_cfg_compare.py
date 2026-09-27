"""Exercise compiler cfg snapshots through the public comparison CLI."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "scripts/ci/h2_cfg_compare.py"


class CfgCompareTest(unittest.TestCase):
    def run_cli(self, linux, macos):
        with tempfile.TemporaryDirectory() as tmp:
            paths = [Path(tmp, "linux.cfg"), Path(tmp, "macos.cfg")]
            for path, data in zip(paths, (linux, macos)):
                if data is not None:
                    path.write_bytes(data.encode("utf-8") if isinstance(data, str) else data)
            proc = subprocess.run(
                [sys.executable, str(SCRIPT), "--linux", str(paths[0]), "--macos", str(paths[1])],
                capture_output=True, text=True, env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1"),
            )
            for path, data in zip(paths, (linux, macos)):
                if data is None:
                    self.assertFalse(path.exists())
                else:
                    self.assertEqual(path.read_bytes(), data.encode("utf-8") if isinstance(data, str) else data)
            return proc

    def assert_report(self, linux, macos, code, report):
        proc = self.run_cli(linux, macos)
        self.assertEqual(proc.returncode, code, proc.stderr)
        self.assertEqual(proc.stderr, "")
        self.assertEqual(json.loads(proc.stdout), report)

    def test_equal_lists_ignore_order_duplicates_and_blank_lines(self):
        self.assert_report(
            'unix\n\ntarget_os="linux"\nunix\n', '  target_os="linux"\r\n unix \r\n', 0,
            {"common": ['target_os="linux"', "unix"], "linux_only": [], "macos_only": []},
        )

    def test_target_and_custom_cfg_differences_are_reported_in_both_directions(self):
        self.assert_report(
            'unix\ntarget_os="linux"\ntarget_arch="x86_64"\nfeature="tls"\n',
            'unix\ntarget_os="macos"\ntarget_arch="aarch64"\ncustom_build\n', 1,
            {"common": ["unix"],
             "linux_only": ['feature="tls"', 'target_arch="x86_64"', 'target_os="linux"'],
             "macos_only": ["custom_build", 'target_arch="aarch64"', 'target_os="macos"']},
        )

    def test_one_sided_difference_is_not_lost(self):
        for linux, macos, expected in (
            ("unix\ndebug_assertions\n", "unix\n",
             {"common": ["unix"], "linux_only": ["debug_assertions"], "macos_only": []}),
            ("unix\n", "unix\ndebug_assertions\n",
             {"common": ["unix"], "linux_only": [], "macos_only": ["debug_assertions"]}),
        ):
            with self.subTest(linux=linux, macos=macos):
                self.assert_report(linux, macos, 1, expected)

    def test_multivalued_keys_and_quoted_values_remain_separate_atoms(self):
        self.assert_report(
            'target_has_atomic="8"\ntarget_has_atomic="64"\nfeature=""\n사용자="any(unix)"\n'
            'custom="a\\\"b"\n',
            'target_has_atomic="64"\ntarget_has_atomic="ptr"\n사용자="any(unix)"\n'
            'custom="a\\\"b"\n', 1,
            {"common": ['custom="a\\\"b"', 'target_has_atomic="64"', '사용자="any(unix)"'],
             "linux_only": ['feature=""', 'target_has_atomic="8"'],
             "macos_only": ['target_has_atomic="ptr"']},
        )

    def test_non_output_syntax_is_rejected_with_lane_and_line(self):
        for bad in ('#[cfg(unix)]', 'any(unix, windows)', '--cfg unix', 'target_os=linux',
                    'warning: ignored flag', 'target_os="linux', 'x="a" trailing', 'x="a\x00b"'):
            for lane in ("linux", "macos"):
                with self.subTest(bad=bad, lane=lane):
                    snapshots = {"linux": "unix\n", "macos": "unix\n"}
                    snapshots[lane] += bad + "\n"
                    proc = self.run_cli(**snapshots)
                    self.assertEqual(proc.returncode, 2)
                    self.assertEqual(proc.stdout, "")
                    self.assertIn(f"h2-cfg-compare: {lane}:", proc.stderr)
                    self.assertIn(f"{lane}.cfg:2: expected a compiler cfg atom", proc.stderr)
                    self.assertNotIn("Traceback", proc.stderr)

    def test_empty_missing_and_non_utf8_inputs_are_errors_not_equal_lists(self):
        for data, diagnostic in ((" \n\n", "empty compiler cfg list"),
                                 (None, "No such file"), (b"\xff\n", "decode")):
            for lane in ("linux", "macos"):
                with self.subTest(data=data, lane=lane):
                    snapshots = {"linux": "unix\n", "macos": "unix\n"}
                    snapshots[lane] = data
                    proc = self.run_cli(**snapshots)
                    self.assertEqual(proc.returncode, 2)
                    self.assertEqual(proc.stdout, "")
                    self.assertIn(f"h2-cfg-compare: {lane}:", proc.stderr)
                    self.assertIn(diagnostic, proc.stderr)
                    self.assertNotIn("Traceback", proc.stderr)

    def test_both_lane_paths_are_required(self):
        proc = subprocess.run([sys.executable, str(SCRIPT), "--linux", "unused.cfg"],
                              capture_output=True, text=True)
        self.assertEqual(proc.returncode, 2)
        self.assertEqual(proc.stdout, "")
        self.assertIn("required: --macos", proc.stderr)


if __name__ == "__main__":
    unittest.main()
