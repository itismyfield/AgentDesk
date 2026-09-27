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
    def run_cli(self, linux, macos, **env):
        with tempfile.TemporaryDirectory() as tmp:
            paths = [Path(tmp, "linux.json"), Path(tmp, "macos.json")]
            inputs = [json.dumps(data, ensure_ascii=False) if isinstance(data, list) else data
                      for data in (linux, macos)]
            for path, data in zip(paths, inputs):
                if data is not None:
                    path.write_bytes(data.encode("utf-8") if isinstance(data, str) else data)
            proc = subprocess.run(
                [sys.executable, str(SCRIPT), "--linux", str(paths[0]), "--macos", str(paths[1])],
                capture_output=True, text=True, env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1", **env),
            )
            for path, data in zip(paths, inputs):
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

    def test_equal_lists_ignore_order_duplicates_and_json_whitespace(self):
        self.assert_report(
            [["unix"], ["target_os", "linux"], ["unix"]], '\n[ ["target_os", "linux"],\r\n ["unix"] ]\n', 0,
            {"common": [["target_os", "linux"], ["unix"]], "linux_only": [], "macos_only": []},
        )

    def test_target_and_custom_cfg_differences_are_reported_in_both_directions(self):
        self.assert_report(
            [["unix"], ["target_os", "linux"], ["target_arch", "x86_64"], ["feature", "tls"]],
            [["unix"], ["target_os", "macos"], ["target_arch", "aarch64"], ["custom_build"]], 1,
            {"common": [["unix"]],
             "linux_only": [["feature", "tls"], ["target_arch", "x86_64"], ["target_os", "linux"]],
             "macos_only": [["custom_build"], ["target_arch", "aarch64"], ["target_os", "macos"]]},
        )

    def test_one_sided_difference_is_not_lost(self):
        for linux, macos, expected in (
            ([["unix"], ["debug_assertions"]], [["unix"]],
             {"common": [["unix"]], "linux_only": [["debug_assertions"]], "macos_only": []}),
            ([["unix"]], [["unix"], ["debug_assertions"]],
             {"common": [["unix"]], "linux_only": [], "macos_only": [["debug_assertions"]]}),
        ):
            with self.subTest(linux=linux, macos=macos):
                self.assert_report(linux, macos, 1, expected)

    def test_multivalued_keys_and_quoted_values_remain_separate_atoms(self):
        self.assert_report(
            [["target_has_atomic", "8"], ["target_has_atomic", "64"], ["feature", ""],
             ["사용자", "any(unix)"], ["custom", 'a"b']],
            [["target_has_atomic", "64"], ["target_has_atomic", "ptr"],
             ["사용자", "any(unix)"], ["custom", 'a"b']], 1,
            {"common": [["custom", 'a"b'], ["target_has_atomic", "64"], ["사용자", "any(unix)"]],
             "linux_only": [["feature", ""], ["target_has_atomic", "8"]],
             "macos_only": [["target_has_atomic", "ptr"]]},
        )

    def test_rustc_print_collision_is_rejected_and_structured_atoms_differ(self):
        for separator in ("\n", "\r\n", "\u2028", "\u0085", "\x1c"):
            with self.subTest(separator=repr(separator)):
                raw = 'foo="a"' + separator + 'b="c"\n'
                proc = self.run_cli(raw, 'foo="a"\nb="c"\n')
                self.assertEqual(proc.returncode, 2, proc.stdout)
                self.assertEqual(proc.stdout, "")
                self.assertIn("raw --print cfg is ambiguous", proc.stderr)
                self.assert_report(
                    [["foo", 'a"' + separator + 'b="c']], [["foo", "a"], ["b", "c"]], 1,
                    {"common": [], "linux_only": [["foo", 'a"' + separator + 'b="c']],
                     "macos_only": [["b", "c"], ["foo", "a"]]},
                )

    def test_flag_empty_value_backslash_and_control_values_stay_distinct(self):
        for left, right in ((["key"], ["key", ""]), (["key", "\\n"], ["key", "\n"]),
                            (["key", "\\u2028"], ["key", "\u2028"]),
                            (["key", 'a\\"b'], ["key", 'a"b']), (["key", " "], ["key", ""]),
                            (["key", "\x00"], ["key", "\\0"])):
            with self.subTest(left=left, right=right):
                self.assert_report([left], [right], 1,
                                   {"common": [], "linux_only": [left], "macos_only": [right]})

    def test_non_json_input_is_rejected_with_lane_and_line(self):
        for bad in ('#[cfg(unix)]', 'any(unix, windows)', '--cfg unix', 'target_os=linux',
                    'warning: ignored flag', 'foo="a"b"', '[["x", "a\x00b"]]'):
            for lane in ("linux", "macos"):
                with self.subTest(bad=bad, lane=lane):
                    snapshots = {"linux": [["unix"]], "macos": [["unix"]]}
                    snapshots[lane] = "\n" + bad
                    proc = self.run_cli(**snapshots)
                    self.assertEqual(proc.returncode, 2)
                    self.assertEqual(proc.stdout, "")
                    self.assertIn(f"h2-cfg-compare: {lane}:", proc.stderr)
                    self.assertIn(f"{lane}.json:2: expected structured cfg JSON", proc.stderr)
                    self.assertNotIn("Traceback", proc.stderr)

    def test_wrong_atom_shapes_and_object_key_overwrites_are_rejected(self):
        for bad in ('{}', '{"foo":"a", "foo":"b"}', '[["foo", {"x":1, "x":2}]]',
                    '[["foo", null]]', '[[]]', '[["x", "a", "b"]]', '[["1name"]]',
                    '[["x", 1]]', '[true]', '[["x", "\\ud800"]]'):
            with self.subTest(bad=bad):
                proc = self.run_cli(bad, [["unix"]])
                self.assertEqual(proc.returncode, 2)
                self.assertEqual(proc.stdout, "")
                self.assertIn("h2-cfg-compare: linux:", proc.stderr)
                self.assertNotIn("Traceback", proc.stderr)

    def test_empty_missing_and_non_utf8_inputs_are_errors_not_equal_lists(self):
        for data, diagnostic in (("[]", "expected a nonempty cfg array"), (" \n", "expected structured cfg JSON"),
                                 (None, "No such file"), (b"\xff\n", "decode")):
            for lane in ("linux", "macos"):
                with self.subTest(data=data, lane=lane):
                    snapshots = {"linux": [["unix"]], "macos": [["unix"]]}
                    snapshots[lane] = data
                    proc = self.run_cli(**snapshots)
                    self.assertEqual(proc.returncode, 2)
                    self.assertEqual(proc.stdout, "")
                    self.assertIn(f"h2-cfg-compare: {lane}:", proc.stderr)
                    self.assertIn(diagnostic, proc.stderr)
                    self.assertNotIn("Traceback", proc.stderr)

    def test_both_lane_paths_are_required(self):
        for present, absent in (("--linux", "--macos"), ("--macos", "--linux")):
            with self.subTest(absent=absent):
                proc = subprocess.run([sys.executable, str(SCRIPT), present, "unused.json"],
                                      capture_output=True, text=True)
                self.assertEqual(proc.returncode, 2)
                self.assertEqual(proc.stdout, "")
                self.assertIn(f"required: {absent}", proc.stderr)

    def test_unicode_identifiers_and_values_work_with_ascii_stdout(self):
        proc = self.run_cli('[["किताब"], ["사용자", "가"]]', '[["किताब"], ["사용자", "\\uac00"]]',
                            PYTHONIOENCODING="ascii")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(proc.stderr, "")
        self.assertEqual(json.loads(proc.stdout),
                         {"common": [["किताब"], ["사용자", "가"]], "linux_only": [], "macos_only": []})


if __name__ == "__main__":
    unittest.main()
