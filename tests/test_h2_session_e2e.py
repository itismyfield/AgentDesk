"""Real Cargo sessions; build the pinned release modmap-driver before invoking this suite."""
from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts/ci"))
import h2_env
import h2_session as session


class WorkspaceSession(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.driver = Path(os.environ.get("H2_SESSION_DRIVER", ROOT / "target/modmap-driver/release/modmap-driver")).resolve(strict=True)
        cls.lane = "macos" if sys.platform == "darwin" else "linux"
        base = ROOT / "target/h2/session-e2e"
        base.mkdir(parents=True, exist_ok=True)
        cls.evidence = Path(tempfile.mkdtemp(dir=base)).resolve()
        print(f"H2 session e2e evidence: {cls.evidence}", flush=True)

    def setUp(self):
        self.case = self.evidence / self._testMethodName
        self.crate = self.case / "workspace"
        shutil.copytree(ROOT / "tests/fixtures/h2_session", self.crate)
        self.conf = self.case / "config"
        self.conf.mkdir()
        (self.conf / "clippy.toml").write_text('disallowed-methods = ["requested_root::sink"]\n')
        self.env = {k: v for k, v in h2_env.environment().items() if not k.startswith("MODMAP_")}
        self.env.update(CLIPPY_CONF_DIR=str(self.conf), CLIPPY_TERMINAL_WIDTH="0")

    def collect(self, tag, *extra, crate=None, target=None):
        run = self.case / tag
        manifest = session.session(ROOT, crate or self.crate, run, self.conf, self.lane, driver=self.driver,
                                   extra=("--locked", "--offline", "--target-dir", str(target or self.case / (tag + "-target")), *extra))
        self.assertEqual(manifest["kind"], "canary-items")
        self.assertEqual(manifest["proof"]["schema"], "h2-session/2")
        self.assertEqual(manifest["proof"]["unit"]["lib"], str((crate or self.crate) / "src/lib.rs"))
        self.assertEqual(list(run.glob("*.claim")), [run / "session.json.claim"])
        return manifest, run

    def clippy(self, tag, *extra, target):
        os.utime(self.crate / "src/lib.rs", None)
        flags = ["--cap-lints", "warn"] + [v for lint in (*session.LINTS, *session.RO_LINTS) for v in ("--force-warn", lint)]
        proc = subprocess.run(["cargo", "clippy", "--lib", "--locked", "--offline", "--message-format=json",
                               "--target-dir", str(target), *extra, "--", *flags], cwd=self.crate, env=self.env,
                              capture_output=True, timeout=120)
        (self.case / (tag + ".jsonl")).write_bytes(proc.stdout)
        (self.case / (tag + ".stderr")).write_bytes(proc.stderr)
        self.assertEqual(proc.returncode, 0, proc.stderr.decode())
        return proc.stdout

    @staticmethod
    def messages(body):
        return [line for line in body.splitlines(keepends=True) if json.loads(line).get("reason") == "compiler-message"]

    def test_workspace_dependencies_match_clippy_cold_and_warm(self):
        for feature, dependency in (("workspace_lib", "helper"), ("workspace_proc", "mac")):
            args = ("--no-default-features", "--features", feature)
            for temperature in ("cold", "warm"):
                tag = feature + "-" + temperature
                target = self.case / (feature + "-target")
                self.assertEqual(target.exists(), temperature == "warm")
                manifest, run = self.collect(tag, *args, target=target)
                actual = self.messages((run / "clippy.jsonl").read_bytes())
                expected = self.messages(self.clippy(tag + "-control", *args, target=self.case / (feature + "-control-target")))
                self.assertEqual(actual, expected)
                codes = [json.loads(line)["message"]["code"]["code"] for line in actual]
                self.assertIn("unused_imports", codes)
                self.assertIn("clippy::disallowed_methods", codes)
                # HIR queries emit early lints; child diagnostics must stay out of Cargo's JSONL.
                child = [(json.loads(line).get("code") or {}).get("code")
                         for line in (run / "session.json.items.stderr").read_bytes().splitlines()]
                marker = ["h2_expansion_marker"] if dependency == "mac" else []
                self.assertIn("unused_imports", child)
                self.assertEqual([code for code in child if code == "h2_expansion_marker"], marker)
                self.assertEqual([code for code in codes if code == "h2_expansion_marker"], marker)
                self.assertEqual(manifest["proof"]["unit"]["package"], "requested_root")
                events = [json.loads(line) for line in (run / "clippy.jsonl").read_text().splitlines()]
                artifacts = [e for e in events if e.get("reason") == "compiler-artifact" and e["target"]["name"] == dependency]
                self.assertEqual(len(artifacts), 1)
                self.assertIs(artifacts[0]["fresh"], temperature == "warm")

    def test_parallel_members_delegate_and_workspace_is_rejected(self):
        for n in range(3):
            manifest, run = self.collect(str(n), "-j", "2")
            self.assertEqual(manifest["proof"]["unit"]["package"], "requested_root")
            events = [json.loads(line) for line in (run / "clippy.jsonl").read_text().splitlines()]
            compiled = {e["target"]["name"] for e in events if e.get("reason") == "compiler-artifact" and e["fresh"] is False}
            self.assertTrue({"requested_root", "helper", "mac"} <= compiled, compiled)
        run = self.case / "workspace-flag"
        with self.assertRaisesRegex(session.MeasureError, "unsupported: --workspace"):
            session.session(ROOT, self.crate, run, self.conf, self.lane, driver=self.driver,
                            extra=("--locked", "--offline", "--workspace", "-j", "2"))
        self.assertFalse(run.exists())

    def test_other_member_entry_is_delegated(self):
        run, argv, env = self.direct("other")
        other = self.crate / "other/src/lib.rs"
        out = self.case / "other-out"
        out.mkdir()
        argv = [a.replace("requested_root", "other_member") for a in argv]
        argv[argv.index("--out-dir") + 1], argv[-1] = str(out), str(other)
        env = dict(env, CARGO_MANIFEST_DIR=str(other.parent.parent), CARGO_PKG_NAME="other_member")
        before = sorted(p.name for p in run.iterdir())
        proc = self.enter(argv, env)
        self.assertEqual(proc.returncode, 0, proc.stderr.decode())
        self.assertEqual(sorted(p.name for p in run.iterdir()), before)
        self.assertTrue(list(out.glob("libother_member*.rmeta")))

    def test_all_targets_delegates_tests_and_benches(self):
        crate = self.case / "canary"
        shutil.copytree(ROOT / "tools/modmap-driver/canary", crate)
        for directory in ("tests", "benches"):
            (crate / directory).mkdir()
            (crate / directory / "probe.rs").write_text("#[test]\nfn probe() {}\n")
        manifest, run = self.collect("all-targets", "--all-targets", crate=crate)
        unit = manifest["proof"]["unit"]
        self.assertEqual(unit["crate_types"], ["cdylib", "rlib"])
        self.assertIs(unit["test"], False)
        self.assertTrue({"clippy", "h2_items_bs_clippy"} <= set(manifest["proof"]["cfg"]))
        events = [json.loads(line) for line in (run / "clippy.jsonl").read_text().splitlines()]
        kinds = {kind for e in events if e.get("reason") == "compiler-artifact" and e["profile"]["test"] for kind in e["target"]["kind"]}
        self.assertTrue({"test", "bench"} <= kinds, kinds)
        self.assertTrue(any(e.get("reason") == "compiler-artifact" and e["profile"]["test"]
                            and e["target"]["src_path"] == unit["lib"] for e in events))

    def direct(self, tag):
        run = self.case / tag
        run.mkdir()
        toolchain = session.toolchain_guard(self.crate, self.env, self.driver)
        env = dict(self.env, MODMAP_CLIPPY_DRIVER=toolchain["clippy_driver"],
                   MODMAP_EXPECT_MANIFEST=str(self.crate / "Cargo.toml"), MODMAP_EXPECT_PACKAGE="requested_root",
                   MODMAP_EXPECT_LIB=str(self.crate / "src/lib.rs"), CARGO_MANIFEST_DIR=str(self.crate),
                   CARGO_PKG_NAME="requested_root", MODMAP_SESSION_OUT=str(run / "session.json"),
                   MODMAP_CFG_NONCE=tag, MODMAP_RUN_ID=tag, CLIPPY_ARGS="--cap-lints__CLIPPY_HACKERY__warn__CLIPPY_HACKERY__")
        request = dict(toolchain=toolchain, protected_env={key: env[key] for key in session.PROTECTED_ENV})
        (run / "request.json").write_text(json.dumps(request))
        rustc = Path(toolchain["clippy_driver"]).with_name("rustc")
        argv = [str(self.driver), str(rustc), "--sysroot", str(rustc.parent.parent),
                "--crate-name", "requested_root", "--crate-type", "lib", "--edition=2024",
                "--emit=metadata", "--out-dir", str(run), "--error-format=json", str(self.crate / "src/lib.rs")]
        return run, argv, env

    def enter(self, argv, env):
        return subprocess.run(argv, cwd=self.crate, env=env, capture_output=True, timeout=120)

    def test_preclaimed_session_writes_nothing(self):
        for n, contents in enumerate((b"", b'{"pid":')):
            run, argv, env = self.direct(f"preclaimed-{n}")
            claim = run / "session.json.claim"
            claim.write_bytes(contents)
            before = {p.name: (p.read_bytes(), p.stat().st_mtime_ns) for p in run.iterdir()}
            proc = self.enter(argv, env)
            self.assertEqual(proc.returncode, 101, proc.stderr.decode())
            self.assertIn(b"File exists", proc.stderr)
            self.assertEqual(before, {p.name: (p.read_bytes(), p.stat().st_mtime_ns) for p in run.iterdir()})
            (self.case / f"preclaimed-{n}.stderr").write_bytes(proc.stderr)

    def test_two_simultaneous_entries_have_one_producer(self):
        for n in range(5):
            run, argv, env = self.direct(f"race-{n}")
            barrier = threading.Barrier(2)
            def enter():
                barrier.wait(timeout=30)
                return self.enter(argv, env)
            with ThreadPoolExecutor(max_workers=2) as pool:
                futures = [pool.submit(enter) for _ in range(2)]
                results = [future.result() for future in futures]
            for i, proc in enumerate(results):
                (self.case / f"race-{n}-{i}.stderr").write_bytes(proc.stderr)
            self.assertEqual(sorted(p.returncode for p in results), [0, 101], [p.stderr.decode() for p in results])
            loser = next(p for p in results if p.returncode)
            self.assertIn(loser.stderr.strip(), (
                b"modmap-driver (clippy session): File exists (os error 17)",
                b"modmap-driver (clippy session): proof already exists: second producer in this session",
            ))
            proof, claim = (json.loads((run / name).read_text()) for name in ("session.json", "session.json.claim"))
            self.assertEqual(claim, {"pid": proof["pid"], "unit": proof["unit"]})


if __name__ == "__main__":
    unittest.main()
