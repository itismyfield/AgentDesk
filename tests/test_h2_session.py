"""Cfg-only sessions use fake Cargo events and never execute a compiler."""
from __future__ import annotations

import copy
import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts/ci"))
import h2_session as s


class Session(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()
        self.crate = self.root / "crate"
        self.crate.mkdir()
        (self.crate / "Cargo.toml").write_text('[package]\nname="fixture"\nversion="0.0.0"\n')
        self.lib = self.crate / "rust/library.rs"
        self.lib.parent.mkdir()
        self.lib.write_text("pub fn caller() {}\n")
        os.utime(self.lib, ns=(1, 1))
        self.conf = self.root / "conf"
        self.conf.mkdir()
        (self.conf / "clippy.toml").write_text("disallowed-methods=[]\n")
        self.driver = self.root / "modmap-driver"
        self.driver.touch()
        self.sysroot = self.root / "toolchain"
        self.clippy = self.sysroot / "bin/clippy-driver"
        self.clippy.parent.mkdir(parents=True)
        self.clippy.touch()
        self.md = {"packages": [{"manifest_path": str(self.crate / "Cargo.toml"), "name": "fixture",
            "id": "path+file:///fixture#0.0.0", "targets": [{"name": "fixture", "kind": ["cdylib", "rlib"],
            "crate_types": ["rlib", "cdylib"], "src_path": str(self.lib)}]}]}
        self.version = f"release: 1.94.1\ncommit-hash: {s.ALLOWED['commit']}\nhost: aarch64-apple-darwin\n"
        self.answers = {("rustc", "-vV"): self.version, ("cargo", "-V"): "cargo 1.94.1 (abc)",
            ("rustc", "--print", "sysroot"): str(self.sysroot),
            (str(self.clippy), "--version"): "clippy 0.1.94 (e408947bfd 2026-03-25)",
            (str(self.clippy), "--rustc", "-vV"): self.version,
            (str(self.driver), "__modmap_version"): "1.94.1 (e408947bf 2026-03-25)"}
        self.calls, self.checks, self.metadata = [], [], []
        self.mutate = lambda run, proof, claim, events: None
        self.rc = 0
        self.after = lambda run: None
        for p in (patch.object(s.subprocess, "run", side_effect=self.command),
                  patch.object(s.modmap, "source_state", return_value={"sha": "unchanged"}),
                  patch.dict(os.environ, RUSTUP_TOOLCHAIN="fixture-toolchain")):
            p.start()
            self.addCleanup(p.stop)

    def command(self, argv, *, cwd, env, **kwargs):
        self.calls.append((argv, cwd, env.copy()))
        if tuple(argv) in self.answers:
            return subprocess.CompletedProcess(argv, 0, self.answers[tuple(argv)], "")
        if argv[:2] == ["cargo", "metadata"]:
            self.metadata.append(argv)
            return subprocess.CompletedProcess(argv, 0, json.dumps(self.md), "")
        self.assertEqual(argv[:3], ["cargo", "check", "--lib"])
        self.checks.append(argv)
        run = Path(env["MODMAP_SESSION_OUT"]).parent
        req = json.loads((run / "request.json").read_text())
        unit = {k: v for k, v in req["unit"].items() if k != "package_id"}
        unit.update(root=str(self.crate), metadata="abcd", test=False)
        proof = dict(schema="h2-session/1-cfg", unit=unit, pid=42, nonce=req["nonce"], run_id=req["run_id"],
            argv=["/rustc", str(self.lib), "--crate-name", "fixture", "--crate-type", "cdylib,rlib"],
            env_sha256="a" * 64, cfg=["clippy", "debug_assertions"], driver_rustc=s.ALLOWED["driver_rustc"])
        claim = dict(pid=42, unit=copy.deepcopy(unit))
        events = [dict(reason="compiler-artifact", package_id=req["unit"]["package_id"],
                       target={"src_path": str(self.lib)}, profile={"test": False}, fresh=False)]
        for suffix in ("items-cfg.txt", "clippy-cfg.txt"):
            (run / f"session.json.{suffix}").write_text("clippy\ndebug_assertions\n")
        for suffix in ("items.stdout", "items.stderr", "probe.stdout", "probe.stderr"):
            (run / f"session.json.{suffix}").write_text("")
        self.mutate(run, proof, claim, events)
        for name, value in (("session.json", proof), ("session.json.claim", claim)):
            if value is not None and not value.get("omit"):
                (run / name).write_text(json.dumps(value))
        self.after(run)
        return subprocess.CompletedProcess(argv, self.rc, "".join(json.dumps(e) + "\n" for e in events), "")

    def run_session(self, name="run", **kwargs):
        return s.session(self.root, self.crate, self.root / name, self.conf, "macos", driver=self.driver, **kwargs)

    def reject(self, mutate, pattern, name="bad"):
        self.mutate = mutate
        with self.assertRaisesRegex(s.MeasureError, pattern):
            self.run_session(name)
        self.assertFalse((self.root / name / "manifest.json").exists())

    def test_contract_env_touch_and_seal(self):
        helper = copy.deepcopy(self.md["packages"][0])
        helper.update(name="helper", manifest_path=str(self.crate / "helper/Cargo.toml"))
        self.md["packages"].insert(0, helper)
        with patch.dict(os.environ, RUSTFLAGS="--cfg poison", RUSTC_WRAPPER="cache", CLIPPY_ARGS="poison"):
            result = self.run_session(extra=("--features", "live"))
        argv, cwd, env = self.calls[-1]
        self.assertEqual(cwd, self.crate)
        self.assertEqual(argv[-2:], ["--features", "live"])
        self.assertNotIn("RUSTFLAGS", env)
        self.assertEqual(env["RUSTC_WRAPPER"], "")
        self.assertEqual(env["CARGO_INCREMENTAL"], "0")
        self.assertEqual(env["CLIPPY_TERMINAL_WIDTH"], "0")
        expected = ["--cap-lints", "warn", "--force-warn", "clippy::disallowed_methods", "--force-warn",
                    "clippy::disallowed_types", "--force-warn", "clippy::duplicate_mod", ""]
        self.assertEqual(env["CLIPPY_ARGS"].split("__CLIPPY_HACKERY__"), expected)
        self.assertEqual(env["RUSTC_WORKSPACE_WRAPPER"], str(self.driver))
        self.assertEqual(env["MODMAP_CLIPPY_DRIVER"], str(self.clippy))
        self.assertEqual(env["MODMAP_EXPECT_LIB"], str(self.lib))
        self.assertGreater(self.lib.stat().st_mtime_ns, 1)
        self.assertFalse((self.crate / "src/lib.rs").exists())
        self.assertEqual((result["schema"], result["kind"]), (s.collect.SCHEMA, "canary-cfg"))
        self.assertEqual(result, json.loads((self.root / "run/manifest.json").read_text()))
        for name, digest in result["digests"].items():
            self.assertEqual(digest, s.collect.digest((self.root / "run" / name).read_bytes()))
        self.assertNotIn("items", result["proof"])
        with self.assertRaisesRegex(ValueError, "kind mismatch"):
            s.collect.read_manifest(self.root / "run/manifest.json", lane="macos", root=self.crate)
        self.assertTrue(all(cwd == self.crate and e["RUSTUP_TOOLCHAIN"] == "fixture-toolchain"
                            for _, cwd, e in self.calls))

    def test_printed_cfg_preserves_embedded_newlines(self):
        def escaped(run, proof, claim, events):
            text = 'clippy\nh2_probe_escape="quote=" slash=\\ newline=\n한글"\nunix\n'
            proof["cfg"] = text.splitlines()
            for suffix in ("items-cfg.txt", "clippy-cfg.txt"):
                (run / f"session.json.{suffix}").write_text(text)
        self.mutate = escaped
        manifest = self.run_session()
        self.assertIn('한글"', manifest["proof"]["cfg"])

    def test_unit_identity_accepts_workspace_relative_compiler_input(self):
        def relative(run, proof, claim, events):
            proof["argv"][1] = "crate/rust/library.rs"
        self.mutate = relative
        self.assertEqual(self.run_session()["proof"]["unit"]["lib"], str(self.lib))

    def test_allowed_matches_repo_and_ci(self):
        import tomllib
        channel = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
        self.assertEqual(s.ALLOWED["release"], channel)
        versions = re.findall(r'toolchain: ["\']([^"\']+)["\']', (ROOT / ".github/workflows/ci-pr.yml").read_text())
        self.assertTrue(versions)
        self.assertEqual(set(versions), {channel})

    def test_guard_mismatches_never_reach_metadata_or_check(self):
        cases = [(key, "unsupported") for key in self.answers if key != ("rustc", "--print", "sysroot")]
        cases += [(("rustc", "-vV"), self.version.replace("1.94.1", "1.78.0")),
                  (("rustc", "-vV"), self.version.replace(s.ALLOWED["commit"], "0" * 40)),
                  ((str(self.clippy), "--rustc", "-vV"), self.version.replace(s.ALLOWED["commit"], "0" * 40))]
        for i, (key, value) in enumerate(cases):
            with self.subTest(key=key, value=value), patch.dict(self.answers, {key: value}):
                with self.assertRaisesRegex(s.MeasureError, "toolchain"):
                    self.run_session(f"guard{i}")
                self.assertFalse((self.root / f"guard{i}/request.json").exists())
        with self.assertRaisesRegex(s.MeasureError, "clippy-driver"):
            self.run_session("wrong-path", clippy=self.driver)
        self.assertEqual((self.metadata, self.checks), ([], []))

    def test_guard_uses_target_cwd_toolchain(self):
        (self.root / "rust-toolchain.toml").write_text('[toolchain]\nchannel="1.94.1"\n')
        (self.crate / "rust-toolchain.toml").write_text('[toolchain]\nchannel="1.78.0"\n')
        original = self.command
        def by_cwd(argv, **kw):
            if argv == ["rustc", "-vV"] and kw["cwd"] == self.crate:
                return subprocess.CompletedProcess(argv, 0, self.version.replace("1.94.1", "1.78.0"), "")
            return original(argv, **kw)
        with patch.dict(os.environ, {}, clear=True), patch.object(s.subprocess, "run", side_effect=by_cwd):
            with self.assertRaisesRegex(s.MeasureError, "toolchain"):
                self.run_session()
        self.assertEqual((self.metadata, self.checks), ([], []))
        self.assertFalse((self.root / "run/request.json").exists())

    def test_request_rejects_ambiguous_or_unsupported_targets(self):
        original = copy.deepcopy(self.md)
        cases = [[], original["packages"] * 2]
        for targets in ([], [{"kind": ["proc-macro"]}], original["packages"][0]["targets"] * 2):
            pkg = copy.deepcopy(original["packages"][0])
            pkg["targets"] = targets
            cases.append([pkg])
        for i, packages in enumerate(cases):
            with self.subTest(i=i):
                self.md = {"packages": packages}
                with self.assertRaisesRegex(s.MeasureError, "request"):
                    self.run_session(f"request{i}")
        self.assertEqual(self.checks, [])

    def test_unit_identity_must_match_even_when_claim_agrees(self):
        changes = {"manifest": "/other/Cargo.toml", "package": "other", "lib": "/other/lib.rs",
                   "crate_name": "other", "crate_types": ["lib"], "test": True, "root": "/other"}
        for key, value in changes.items():
            def mutate(run, proof, claim, events):
                proof["unit"][key] = value
                claim["unit"][key] = value
            with self.subTest(key=key):
                self.reject(mutate, "unit", key)

    def test_claim_and_artifact_identity(self):
        cases = {
            "claim-missing": lambda r, p, c, e: c.update(omit=True),
            "claim-pid": lambda r, p, c, e: c.update(pid=43),
            "claim-unit": lambda r, p, c, e: c["unit"].update(metadata="other"),
            "artifact-zero": lambda r, p, c, e: e.clear(),
            "artifact-two": lambda r, p, c, e: e.append(copy.deepcopy(e[0])),
            "artifact-fresh": lambda r, p, c, e: e[0].update(fresh=True),
            "artifact-package": lambda r, p, c, e: e[0].update(package_id="other"),
            "artifact-test": lambda r, p, c, e: e[0]["profile"].update(test=True),
        }
        def torn(run, proof, claim, events):
            claim.update(omit=True)
            (run / "session.json.claim").write_text('{"pid":')
        cases["claim-torn"] = torn
        for name, mutate in cases.items():
            with self.subTest(name=name):
                self.reject(mutate, "claim|artifact", name)

    def test_proof_and_cfg_binding(self):
        cases = {
            "proof-missing": (lambda r, p, c, e: p.update(omit=True), "no root compile"),
            "nonce": (lambda r, p, c, e: p.update(nonce="old"), "nonce"),
            "run_id": (lambda r, p, c, e: p.update(run_id="old"), "run_id"),
            "schema": (lambda r, p, c, e: p.update(schema="h2-session/2"), "schema"),
            "cfg-empty": (lambda r, p, c, e: p.update(cfg=[]), "cfg"),
            "cfg-mismatch": (lambda r, p, c, e: (r / "session.json.items-cfg.txt").write_text("unix\n"), "cfg"),
            "cfg-no-clippy": (lambda r, p, c, e: p.update(cfg=["debug_assertions"]), "cfg"),
            "driver": (lambda r, p, c, e: p.update(driver_rustc="other"), "driver"),
            "env": (lambda r, p, c, e: p.update(env_sha256="bad"), "env"),
            "argv": (lambda r, p, c, e: p.update(argv=[]), "argv"),
            "partial": (lambda r, p, c, e: (r / "session.json.partial").touch(), "partial"),
            "mtime": (lambda r, p, c, e: os.utime(r / "session.json.items-cfg.txt", ns=(0, 0)), "predates"),
            "source": (lambda r, p, c, e: self.lib.write_text("changed"), "source"),
            "config": (lambda r, p, c, e: (self.conf / "clippy.toml").write_text("changed"), "source"),
            "request": (lambda r, p, c, e: (r / "request.json").write_text("{}"), "request"),
        }
        for name, (mutate, pattern) in cases.items():
            with self.subTest(name=name):
                self.reject(mutate, pattern, name)

    def test_all_version_commands_must_succeed_and_host_must_match(self):
        original = self.command
        for i, command in enumerate(self.answers):
            def failed(argv, **kw):
                if tuple(argv) == command:
                    return subprocess.CompletedProcess(argv, 1, self.answers[command], "failed")
                return original(argv, **kw)
            with self.subTest(command=command), patch.object(s.subprocess, "run", side_effect=failed):
                with self.assertRaisesRegex(s.MeasureError, "command failed"):
                    self.run_session(f"command{i}")
        with patch.dict(self.answers, {("rustc", "-vV"): self.version.replace("aarch64-apple-darwin", "other-host")}):
            with self.assertRaisesRegex(s.MeasureError, "host"):
                self.run_session("host")
        self.assertEqual((self.metadata, self.checks), ([], []))

    def test_stale_proof_claim_symlink_and_cfg_digest_are_rejected(self):
        for name in ("session.json", "session.json.claim"):
            self.after = lambda run: os.utime(run / name, ns=(0, 0))
            with self.subTest(name=name):
                self.reject(lambda *args: None, "predates", name)
        def symlink(run):
            path = run / "session.json.claim"
            path.rename(run / "moved-claim")
            path.symlink_to(run / "moved-claim")
        self.after = symlink
        self.reject(lambda *args: None, "canonical", "symlink")
        self.after = lambda run: None
        manifest = self.run_session("sealed")
        cfg = self.root / "sealed/session.json.clippy-cfg.txt"
        cfg.write_text("clippy\nunix\n")
        self.assertNotEqual(s.collect.digest(cfg.read_bytes()), manifest["digests"][cfg.name])
        with self.assertRaisesRegex(s.MeasureError, "cfg"):
            s.validate(cfg.parent, manifest["request"])

    def test_cargo_failure_and_used_run_are_never_sealed(self):
        self.rc = 101
        self.reject(lambda *args: None, "Cargo", "failed")
        self.rc = 0
        run = self.root / "used"
        run.mkdir()
        claim = run / "session.json.claim"
        claim.write_text('{"pid":42}')
        with self.assertRaisesRegex(s.MeasureError, "new|empty"):
            self.run_session("used")
        self.assertEqual(claim.read_text(), '{"pid":42}')


if __name__ == "__main__":
    unittest.main()
