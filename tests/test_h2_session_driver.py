"""Exercise a coordinator-built driver with compiler-child spies: H2_SESSION_DRIVER=/path/to/modmap-driver."""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts/ci"))
import h2_env


@unittest.skipUnless(os.environ.get("H2_SESSION_DRIVER"), "set H2_SESSION_DRIVER to a rebuilt driver")
class DriverControls(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()
        self.run = self.root / "run"
        self.run.mkdir()
        self.lib = self.root / "lib.rs"
        self.lib.write_text("pub fn fixture() {}\n")
        self.manifest = self.root / "Cargo.toml"
        self.manifest.touch()
        self.driver = Path(os.environ["H2_SESSION_DRIVER"]).resolve(strict=True)
        self.approved, self.foreign = self.root / "clippy", self.root / "foreign"
        for executable in (self.approved, self.foreign):
            self.spy(executable)
        request = dict(toolchain=dict(clippy_driver=str(self.approved), clippy="approved-version",
                                      clippy_rustc="commit-hash: approved"))
        (self.run / "request.json").write_text(json.dumps(request))
        self.env = dict(h2_env.environment("measure"), CARGO_MANIFEST_DIR=str(self.root), CARGO_PKG_NAME="fixture",
                        MODMAP_EXPECT_MANIFEST=str(self.manifest), MODMAP_EXPECT_PACKAGE="fixture",
                        MODMAP_EXPECT_LIB=str(self.lib), MODMAP_SESSION_OUT=str(self.run / "session.json"),
                        MODMAP_CLIPPY_DRIVER=str(self.approved), MODMAP_CFG_NONCE="n" * 32, MODMAP_RUN_ID="r" * 32)
        self.args = ["/rustc", str(self.lib), "--crate-name", "fixture", "--crate-type", "lib"]

    def spy(self, path, version="approved-version"):
        path.write_text(f'''#!{sys.executable}
import json, pathlib, sys
with pathlib.Path({str(path) + ".calls"!r}).open("a") as f:
    f.write(json.dumps(sys.argv[1:]) + "\\n")
if sys.argv[1:] == ["--version"]: print({version!r})
elif sys.argv[1:] == ["--rustc", "-vV"]: print("commit-hash: approved")
else: print("delegated")
''')
        path.chmod(0o755)

    def invoke(self, args=None, **overlay):
        return subprocess.run([str(self.driver), *(self.args if args is None else args)], cwd=self.root,
                              env={**self.env, **overlay}, capture_output=True, text=True, timeout=20)

    def snapshot(self):
        return {p.name: p.read_bytes() for p in self.run.iterdir()}

    def assert_no_child(self):
        self.assertFalse(self.approved.with_suffix(".calls").exists())
        self.assertFalse(self.foreign.with_suffix(".calls").exists())
        self.assertFalse((self.run / "manifest.json").exists())

    def test_post_preflight_clippy_overlay_fails_before_claim_or_child(self):
        before = self.snapshot()
        # Cargo's force=true overlay replaces the child environment after the request was written.
        result = self.invoke(MODMAP_CLIPPY_DRIVER=str(self.foreign))
        self.assertEqual(result.returncode, 101, result.stderr)
        self.assertIn("clippy path", result.stderr)
        self.assertEqual(self.snapshot(), before)
        self.assert_no_child()

    def test_response_files_hiding_target_or_test_fail_before_outputs(self):
        for hidden in ("--target=aarch64-unknown-linux-gnu", "--test"):
            with self.subTest(hidden=hidden):
                response = self.root / "args.rsp"
                response.write_text(hidden + "\n")
                before = self.snapshot()
                result = self.invoke([*self.args, "@" + str(response)])
                self.assertEqual(result.returncode, 101, result.stderr)
                self.assertIn("response", result.stderr)
                self.assertEqual(self.snapshot(), before)
                self.assert_no_child()

    def test_recognized_nonproducers_with_response_args_still_delegate(self):
        cases = [([*self.args, "--test"], {}), ([*self.args, "--print=cfg"], {}),
                 (["/rustc", "-vV"], {}), (self.args, {"CARGO_PKG_NAME": "helper"}),
                 (self.args, {"CARGO_MANIFEST_DIR": str(self.run)}),
                 ([*self.args[:-1], "bin"], {}), ([*self.args[:-1], "proc-macro"], {})]
        for args, overlay in cases:
            with self.subTest(args=args, overlay=overlay):
                before = self.snapshot()
                result = self.invoke([*args, "@missing.rsp"], **overlay)
                self.assertEqual((result.returncode, result.stdout.strip()), (0, "delegated"), result.stderr)
                self.assertEqual(self.snapshot(), before)
                calls = self.approved.with_suffix(".calls")
                self.assertEqual(json.loads(calls.read_text()), [*args, "@missing.rsp"])
                calls.unlink()

    def test_preclaimed_or_published_producer_does_not_start_children(self):
        for name, content in (("session.json.claim", ""), ("session.json.claim", '{"pid":'),
                              ("session.json", "{}")):
            with self.subTest(name=name, content=content):
                path = self.run / name
                path.write_text(content)
                before = self.snapshot()
                result = self.invoke()
                self.assertEqual(result.returncode, 101, result.stderr)
                self.assertEqual(self.snapshot(), before)
                self.assert_no_child()
                path.unlink()

    def test_approved_path_with_changed_identity_stops_before_items_child(self):
        self.spy(self.approved, version="changed-version")
        result = self.invoke()
        self.assertEqual(result.returncode, 101, result.stderr)
        self.assertIn("clippy identity", result.stderr)
        self.assertEqual(set(self.snapshot()), {"request.json", "session.json.claim"})
        self.assertFalse(self.foreign.with_suffix(".calls").exists())


if __name__ == "__main__":
    unittest.main()
