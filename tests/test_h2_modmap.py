"""Tests for the H2 module map wrapper against stub cargo/rustup: only a complete map this run wrote is accepted."""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT / "scripts/ci"))
import h2_depinfo  # noqa: E402
import h2_measure as h2  # noqa: E402

# The canary rows the real driver writes: `shared`, `spliced` and `wrapped::passed` are clean, the rest is not.
CANARY_ROWS = ["src/shared.rs\tcrate::shared\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:13:1: 13:12 (#0)\t-\tfile",
               "src/lib.rs\tcrate::wrapped\t#4\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:9:49: 9:68 (#4)\t-\tinline",
               "src/lib.rs\tcrate::wrapped\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:15:17: 15:32 (#0)\t-\twrapped",
               "src/wrapped/passed.rs\tcrate::wrapped::passed\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:15:17: 15:32 (#0)\t-\tfile",
               "src/lib.rs\tcrate::named\t#0\t#5\tmodule\tsrc/lib.rs\tsrc/lib.rs:16:7: 16:31 (#0)\t-\tinline",
               "src/lib.rs\tcrate::named\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:16:13: 16:29 (#0)\t-\twrapped",
               "src/lib.rs\tcrate::keyword\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:17:5: 17:37 (#0)\t-\tinline",
               "src/lib.rs\tcrate::keyword\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:17:19: 17:35 (#0)\t-\twrapped",
               "src/lib.rs\tcrate::spliced\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:19:1: 21:2 (#0)\t-\tinline",
               "src/shared.rs\tcrate::spliced\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/shared.rs:1:1: 1:19 (#0)\t-\tinclude",
               'src/shared.rs\tcrate::{fn probe}::injected\t#8\t#8\tfn:probe\tsrc/lib.rs\tsrc/lib.rs:6:9: 6:22 (#8)'
               '\tpath#8["shared.rs"]\tfile']
# The canary as a driver that lost every expansion context would write it.
DRIFTED_ROWS = [re.sub(r"#[1-9]\d*", "#0", row) for row in CANARY_ROWS]
# Stub cargo: `build` leaves a driver binary; `check` does what STUB_CANARY / STUB_REPO says with MODMAP_OUT.
STUB_CARGO = """\
#!/usr/bin/env python3
import os, pathlib, shutil, sys
args = sys.argv[1:]
if args[0] == "build":
    driver = pathlib.Path(args[args.index("--target-dir") + 1], "release/modmap-driver")
    driver.parent.mkdir(parents=True, exist_ok=True)
    driver.touch()
    sys.exit(0)
manifest = args[args.index("--manifest-path") + 1]
action, _, source = os.environ["STUB_CANARY" if "/canary/" in manifest else "STUB_REPO"].partition(":")
out = pathlib.Path(os.environ["MODMAP_OUT"])
if action == "fail":
    sys.exit(101)
if action in ("copy", "old"):
    shutil.copy(source, out)
if action == "old":
    os.utime(out, ns=(0, 0))
"""

def write_modmap(path: Path, rows) -> Path:
    path.write_text("".join(f"{line}\n" for line in (h2_depinfo.MODMAP_HEADER, h2_depinfo.MODMAP_ROOT, *rows)), encoding="utf-8")
    return path

class Wrapper(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.maps, self.root, stubs = Path(tmp.name), Path(tmp.name) / "repo", Path(tmp.name) / "bin"
        (self.root / "scripts/ci").mkdir(parents=True)
        (self.root / h2.BASELINE_FILES[0]).write_text("", encoding="utf-8")
        stubs.mkdir()
        for name, text in (("cargo", STUB_CARGO), ("rustup", "#!/bin/sh\necho rustc-dev-x86_64-unknown-linux-gnu\n")):
            (stubs / name).write_text(text, encoding="utf-8")
            (stubs / name).chmod(0o755)
        clean = [f"src/m{n}.rs\tcrate::m{n}\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:{n}:1: {n}:9 (#0)\t-\tfile" for n in range(1000)]
        # only file rows count: the short map pads 3 file modules with 997 inline ones
        padded = clean[:3] + [row.replace("\tfile", "\tinline") for row in clean[3:]]
        self.full, self.short = write_modmap(self.maps / "full.tsv", clean), write_modmap(self.maps / "short.tsv", padded)
        self.env = dict(os.environ, PATH=f"{stubs}{os.pathsep}{os.environ['PATH']}", STUB_REPO=f"copy:{self.full}",
                        STUB_CANARY=f"copy:{write_modmap(self.maps / 'canary.tsv', CANARY_ROWS)}")

    def run_wrapper(self, *args: str, **env: str) -> tuple[int, str]:
        proc = subprocess.run([sys.executable, str(REPO_ROOT / "scripts/ci/h2_modmap.py"), "--repo", str(self.root), *args],
                              env={**self.env, **env}, capture_output=True, text=True)
        return proc.returncode, proc.stdout + proc.stderr

    def test_only_a_complete_map_written_by_this_run_passes(self) -> None:
        code, output = self.run_wrapper()
        self.assertEqual((code, "h2-modmap: 1000 file modules -> " in output), (0, True), output)
        # no write at rc 0 (a pass-through compile), a stale mtime, a short map, a failed check; a map left by an
        # earlier run sits at the output path each time
        for action, needle in (("none", "was not written"), (f"old:{self.full}", "predates this run"),
                               (f"copy:{self.short}", "lists 3 file modules (< 1000)"), ("fail", "failed (101)")):
            with self.subTest(action=action.split(":")[0]):
                shutil.copy(self.full, self.root / "target/h2/modmap.tsv")
                code, output = self.run_wrapper(STUB_REPO=action)
                self.assertEqual((code, needle in output), (1, True), output)

    def test_inert_maps_the_repo_once_a_baseline_exists(self) -> None:
        self.assertEqual(self.run_wrapper("--inert")[0], 0)
        code, output = self.run_wrapper("--inert", STUB_REPO="fail")
        self.assertEqual((code, "failed (101)" in output), (1, True), output)

    def test_the_canary_must_keep_its_verdict_even_while_inert(self) -> None:
        drifted = f"copy:{write_modmap(self.maps / 'drifted.tsv', DRIFTED_ROWS)}"
        self.assertEqual(self.run_wrapper(STUB_CANARY=drifted)[0], 1)
        (self.root / h2.BASELINE_FILES[0]).unlink()
        # inert without a baseline: nothing runs, or only the canary, which still fails hard
        self.assertEqual(self.run_wrapper("--inert", STUB_CANARY="fail", STUB_REPO="fail"),
                         (0, "h2-modmap: no baseline committed; inert no-op\n"))
        code, output = self.run_wrapper("--inert", "--canary", STUB_REPO="fail")
        self.assertEqual((code, "repo map skipped" in output), (0, True), output)
        code, output = self.run_wrapper("--inert", "--canary", STUB_CANARY=drifted)
        self.assertEqual((code, "driver canary drifted" in output), (1, True), output)

if __name__ == "__main__":
    unittest.main()
