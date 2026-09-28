"""Sessions use fake Cargo events and never execute a compiler."""
from __future__ import annotations

import copy
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts/ci"))
import h2_session as s

LIST_PIDS = s.list_pids


class Harness(unittest.TestCase):
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
        self.md = {"workspace_root": str(self.root), "packages": [{"manifest_path": str(self.crate / "Cargo.toml"), "name": "fixture",
            "id": "path+file:///fixture#0.0.0", "targets": [{"name": "fixture", "kind": ["cdylib", "rlib"],
            "crate_types": ["rlib", "cdylib"], "src_path": str(self.lib)}]}]}
        self.version = f"release: 1.94.1\ncommit-hash: {s.ALLOWED['commit']}\nhost: aarch64-apple-darwin\n"
        self.answers = {("rustc", "-vV"): self.version, ("cargo", "-V"): "cargo 1.94.1 (abc)",
            ("rustc", "--print", "sysroot"): str(self.sysroot),
            (str(self.clippy), "--version"): "clippy 0.1.94 (e408947bfd 2026-03-25)",
            (str(self.clippy), "--rustc", "-vV"): self.version,
            (str(self.driver), "__modmap_version"): "1.94.1 (e408947bf 2026-03-25)"}
        self.cfg = ["clippy", "debug_assertions", 'target_arch="aarch64"', 'target_os="macos"',
                    'target_env=""', 'target_vendor="apple"', 'target_pointer_width="64"']
        self.calls, self.checks, self.metadata = [], [], []
        self.mutate = lambda run, proof, claim, events: None
        self.rc = 0
        self.after = lambda run: None
        # Tests that clear CARGO_HOME must not fall back to the host ~/.cargo.
        for p in (patch.object(s.subprocess, "run", side_effect=self.command),
                  patch.object(s.modmap, "source_capture", return_value=({"sha": "unchanged"}, {}, {})),
                  patch.object(s.Path, "home", return_value=self.root / "home"),
                  patch.object(s, "list_pids", return_value=[os.getpid()]),
                  patch.dict(os.environ, RUSTUP_TOOLCHAIN="fixture-toolchain", CARGO_HOME=str(self.root / "cargo-home"),
                             HOME=str(self.root / "home"))):
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
        unit = {k: v for k, v in req["unit"].items() if k not in ("package_id", "workspace_root")}
        unit.update(root=str(self.crate), metadata="abcd", test=False)
        proof = dict(schema="h2-session/2", unit=unit, pid=42, nonce=req["nonce"], run_id=req["run_id"],
            argv=["/rustc", str(self.lib), "--crate-name", "fixture", "--crate-type", "cdylib,rlib"],
            env_sha256="a" * 64, cfg=list(self.cfg), driver_rustc=s.ALLOWED["driver_rustc"])
        proof.update({key: req["toolchain"][key] for key in ("clippy_driver", "clippy", "clippy_rustc")})
        proof["protected_env"] = {key: env[key] for key in s.PROTECTED_ENV}
        claim = dict(pid=42, unit=copy.deepcopy(unit))
        events = [dict(reason="compiler-artifact", package_id=req["unit"]["package_id"],
                       target=copy.deepcopy(self.md["packages"][0]["targets"][0]), profile={"test": False}, fresh=False)]
        for suffix in ("items-cfg.txt", "clippy-cfg.txt"):
            (run / f"session.json.{suffix}").write_text("\n".join(self.cfg) + "\n")
        for suffix in ("items.stdout", "items.stderr", "probe.stdout", "probe.stderr"):
            (run / f"session.json.{suffix}").write_text("")
        header = dict(schema=1, run_id=req["run_id"], nonce=req["nonce"], kind="canary-items",
                      root=str(self.crate), crate="fixture", cfg_clippy=True)
        row = ["rust/library.rs", 0, 18, "fn", "fixture::caller", None, "caller", 1, 1, 0, "Fn", False]
        body = (json.dumps(header) + "\n" + json.dumps(row) + "\n").encode()
        (run / "items.jsonl").write_bytes(body)
        proof.update(items_sha256=s.collect.digest(body), items_records=1)
        (run / "items.jsonl.sha256").write_text(json.dumps(dict(sha256=proof["items_sha256"], records=1)))
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


class Session(Harness):
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
        self.assertEqual((result["schema"], result["kind"]), (s.collect.SCHEMA, "canary-items"))
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
            text += "\n".join(self.cfg[2:]) + "\n"
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
        result = self.run_session()
        self.assertEqual(result["proof"]["unit"]["lib"], str(self.lib))
        self.assertEqual(result["request"]["unit"]["workspace_root"], str(self.root))

    def test_request_records_exact_protected_environment(self):
        result = self.run_session()
        env = self.calls[-1][2]
        keys = ("CLIPPY_ARGS", "CLIPPY_CONF_DIR", "CLIPPY_TERMINAL_WIDTH", "MODMAP_SESSION_OUT",
                "MODMAP_CFG_NONCE", "MODMAP_RUN_ID", "MODMAP_EXPECT_MANIFEST", "MODMAP_EXPECT_PACKAGE", "MODMAP_EXPECT_LIB")
        expected = {key: env[key] for key in keys}
        self.assertEqual(result["request"].get("protected_env"), expected)
        self.assertEqual(result["proof"].get("protected_env"), expected)

    def test_protected_environment_mismatch_or_omission_prevents_seal(self):
        for i, (key, value) in enumerate((("CLIPPY_ARGS", "@hidden.rsp__CLIPPY_HACKERY__"),
                                         ("CLIPPY_ARGS", "--test__CLIPPY_HACKERY__"), ("CLIPPY_ARGS", ""),
                                         ("CLIPPY_CONF_DIR", "/other"), ("CLIPPY_TERMINAL_WIDTH", "80"),
                                         ("MODMAP_CFG_NONCE", "other"))):
            with self.subTest(key=key, value=value):
                self.reject(lambda r, p, c, e: p.setdefault("protected_env", {}).update({key: value}),
                            "protected env", f"protected{i}")
        self.reject(lambda r, p, c, e: p.pop("protected_env", None), "protected env", "missing-protected")

    def test_links_rustc_env_cannot_override_protected_keys(self):
        config = self.crate / ".cargo/config.toml"
        config.parent.mkdir()
        for i, key in enumerate(("CLIPPY_ARGS", "CLIPPY_CONF_DIR", "MODMAP_RUN_ID")):
            config.write_text('[target.aarch64-apple-darwin.fixture]\nrustc-cfg=["test"]\n'
                              f'rustc-env={{ {key}="@hidden.rsp__CLIPPY_HACKERY__" }}\n')
            with self.subTest(key=key), self.assertRaisesRegex(s.MeasureError, "reserved"):
                self.run_session(f"links{i}")
        self.assertEqual(len(self.calls), 0)
        self.assertFalse(any(self.root.glob("links*/manifest.json")))
        config.write_text('[target.aarch64-apple-darwin.fixture]\nrustc-env={REVIEW_BINDING="live"}\n')
        self.assertEqual(self.run_session("plain-links")["kind"], "canary-items")

    def test_cargo_env_overrides_are_rejected_before_any_cargo(self):
        roots = (self.crate / ".cargo", self.root / ".cargo", self.root / "cargo-home")
        keys = ("MODMAP_CLIPPY_DRIVER", "MODMAP_SESSION_OUT", "CLIPPY_ARGS", "RUSTC_WORKSPACE_WRAPPER",
                "RUSTUP_TOOLCHAIN", "CARGO_BUILD_RUSTC", "CARGO_BUILD_TARGET", "CARGO_HOME", "PATH",
                "CARGO_INCREMENTAL", "CARGO_PKG_NAME", "CARGO_MANIFEST_DIR", "CARGO_ENCODED_RUSTFLAGS",
                "CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS", "RUSTC_BOOTSTRAP")
        for root in roots:
            root.mkdir(exist_ok=True)
            for filename in ("config", "config.toml"):
                for key in keys:
                    for forced in (False, True):
                        path = root / filename
                        value = '{ value = "/foreign/driver", force = true }' if forced else '"/foreign/driver"'
                        path.write_text(f"[env]\n{key} = {value}\n")
                        try:
                            with self.subTest(root=root, filename=filename, key=key, forced=forced):
                                with self.assertRaisesRegex(s.MeasureError, "reserved"):
                                    self.run_session(f"config-{len(self.calls)}")
                                self.assertEqual(len(self.calls), 0)
                        finally:
                            path.unlink()
        self.assertFalse(any(self.root.glob("config-*/manifest.json")))

    def test_config_discovery_keeps_plain_build_env_and_rejects_compiler_controls(self):
        config = self.crate / ".cargo/config.toml"
        config.parent.mkdir()
        config.write_text('[env]\nREVIEW_BINDING={value="live", force=true}\n[build]\nrustflags=["--cfg", "live"]\n')
        self.assertIn(str(config), self.run_session()["request"]["cargo_config"])
        for i, text in enumerate(('include=["other.toml"]', '[build]\nrustc="other"',
                                  '[build]\nrustc-wrapper="cache"', '[build]\nrustc-workspace-wrapper="other"')):
            with self.subTest(text=text):
                config.write_text(text)
                before = len(self.calls)
                with self.assertRaisesRegex(s.MeasureError, "config includes|compiler setting"):
                    self.run_session(f"compiler{i}")
                self.assertEqual(len(self.calls), before)
        config.unlink()
        relative = self.crate / "cargo-home/config"
        relative.parent.mkdir()
        relative.write_text('[env]\nMODMAP_CLIPPY_DRIVER="foreign"\n')
        with patch.dict(os.environ, CARGO_HOME="cargo-home"):
            with self.assertRaisesRegex(s.MeasureError, "reserved"):
                self.run_session("relative-home")

    def test_config_change_between_metadata_and_compile_is_rejected(self):
        original = s.requested_unit
        def changed(*args):
            unit = original(*args)
            config = self.root / "cargo-home/config.toml"
            config.parent.mkdir()
            config.write_text('[env]\nREVIEW_BINDING="changed"\n')
            return unit
        with patch.object(s, "requested_unit", side_effect=changed):
            with self.assertRaisesRegex(s.MeasureError, "config changed"):
                self.run_session()
        self.assertEqual(len(self.metadata), 1)
        self.assertEqual(self.checks, [])
        self.assertFalse((self.root / "run/manifest.json").exists())

    def test_config_and_package_changing_extra_options_are_rejected(self):
        cases = (("--config", 'env.MODMAP_CLIPPY_DRIVER="foreign"'), ("--config=other.toml",),
                 ("--conf=other.toml",), ("--features", "--config=other.toml"), ("-Zunstable-options",), ("-C", "other"),
                 ("--package", "helper"), ("--package=helper",), ("-phelper",), ("-p", "helper"),
                 ("--workspace",), ("--all",), ("--exclude=fixture",), ("--manifest-path=other.toml",),
                 ("--", "--config", "other"), ("+nightly",))
        for i, extra in enumerate(cases):
            with self.subTest(extra=extra):
                with self.assertRaisesRegex(s.MeasureError, "extra"):
                    self.run_session(f"extra{i}", extra=extra)
                self.assertFalse((self.root / f"extra{i}/manifest.json").exists())
        self.assertEqual(len(self.calls), 0)

    def test_proof_binds_effective_clippy_after_cargo_overlay(self):
        for key, wrong in (("clippy_driver", "/foreign/clippy-driver"), ("clippy", "other-version"),
                           ("clippy_rustc", "commit-hash: other")):
            with self.subTest(key=key):
                self.reject(lambda r, p, c, e: p.update({key: wrong}), "clippy", key)

    def test_response_file_and_foreign_cfg_cannot_be_sealed(self):
        for i, hidden in enumerate(("--target=aarch64-unknown-linux-gnu", "--test")):
            response = self.crate / f"args{i}"
            response.write_text(hidden + "\n")
            with self.subTest(hidden=hidden):
                self.reject(lambda r, p, c, e: p["argv"].append("@" + str(response)), "response", f"response{i}")
        def foreign(run, proof, claim, events):
            proof["cfg"] = [c for c in proof["cfg"] if not c.startswith("target_arch=")]
            proof["cfg"].append('target_arch="x86_64"')
            for suffix in ("items-cfg.txt", "clippy-cfg.txt"):
                (run / f"session.json.{suffix}").write_text("\n".join(proof["cfg"]) + "\n")
        self.reject(foreign, "cfg target", "foreign-cfg")

    def test_linux_cfg_target_is_bound_as_well(self):
        version = self.version.replace("aarch64-apple-darwin", "x86_64-unknown-linux-gnu")
        self.answers[("rustc", "-vV")] = version
        self.answers[(str(self.clippy), "--rustc", "-vV")] = version
        self.cfg = ["clippy", 'target_arch="x86_64"', 'target_os="linux"', 'target_env="gnu"',
                    'target_vendor="unknown"', 'target_pointer_width="64"']
        result = s.session(self.root, self.crate, self.root / "linux", self.conf, "linux", driver=self.driver)
        self.assertEqual(result["request"]["target"], "x86_64-unknown-linux-gnu")
        for atom in self.cfg[1:]:
            proof = result["proof"]
            original = list(proof["cfg"])
            proof["cfg"].remove(atom)
            run = self.root / "linux"
            (run / "session.json").write_text(json.dumps(proof))
            for suffix in ("items-cfg.txt", "clippy-cfg.txt"):
                (run / f"session.json.{suffix}").write_text("\n".join(proof["cfg"]) + "\n")
            with self.subTest(atom=atom), self.assertRaisesRegex(s.MeasureError, "cfg target"):
                s.validate(run, result["request"])
            proof["cfg"] = original

    def test_workspace_default_members_do_not_select_the_requested_package(self):
        with (self.crate / "Cargo.toml").open("a") as manifest:
            manifest.write('[workspace]\nmembers=["helper"]\ndefault-members=["helper"]\n')
        helper = copy.deepcopy(self.md["packages"][0])
        helper.update(name="helper", id="helper-id", manifest_path=str(self.crate / "helper/Cargo.toml"))
        self.md["packages"].insert(0, helper)
        result = self.run_session()
        self.assertIn("--package", self.checks[-1])
        selected = self.checks[-1][self.checks[-1].index("--package") + 1]
        self.assertEqual(selected, result["request"]["unit"]["package_id"])
        self.assertNotEqual(selected, helper["id"])

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
            "schema": (lambda r, p, c, e: p.update(schema="h2-session/1-cfg"), "schema"),
            "cfg-empty": (lambda r, p, c, e: p.update(cfg=[]), "cfg"),
            "cfg-mismatch": (lambda r, p, c, e: (r / "session.json.items-cfg.txt").write_text("unix\n"), "cfg"),
            "cfg-no-clippy": (lambda r, p, c, e: p.update(cfg=["debug_assertions"]), "cfg"),
            "driver": (lambda r, p, c, e: p.update(driver_rustc="other"), "driver"),
            "env": (lambda r, p, c, e: p.update(env_sha256="bad"), "env"),
            "argv": (lambda r, p, c, e: p.update(argv=[]), "argv"),
            "partial": (lambda r, p, c, e: (r / "session.json.partial").touch(), "partial"),
            "mtime": (lambda r, p, c, e: os.utime(r / "session.json.items-cfg.txt", ns=(0, 0)), "predates"),
            "source": (lambda r, p, c, e: self.lib.write_text("changed"), "written during Cargo"),
            "config": (lambda r, p, c, e: (self.conf / "clippy.toml").write_text("changed"), "written during Cargo"),
            "request": (lambda r, p, c, e: (r / "request.json").write_text("{}"), "request"),
        }
        for name, (mutate, pattern) in cases.items():
            with self.subTest(name=name):
                self.reject(mutate, pattern, name)

    def test_non_lib_artifacts_sharing_the_lib_file_do_not_count(self):
        def share(kind, crate_types):
            def mutate(run, proof, claim, events):
                other = copy.deepcopy(events[0])
                other["target"].update(kind=kind, crate_types=crate_types, name="build-script-build")
                events.append(other)
            return mutate
        self.mutate = share(["custom-build"], ["bin"])
        self.assertEqual(self.run_session("build")["kind"], "canary-items")
        self.mutate = share(["bin"], ["bin"])
        self.assertEqual(self.run_session("bin")["kind"], "canary-items")
        self.reject(share(["rlib"], ["rlib"]), "artifact", "two-libs")

    def test_items_are_bound_at_callback_proof_and_seal(self):
        result = self.run_session()
        run = self.root / "run"
        digest = s.collect.digest((run / "items.jsonl").read_bytes())
        self.assertEqual(result["proof"]["schema"], "h2-session/2")
        self.assertEqual(result["proof"]["items_records"], 1)
        self.assertEqual(result["proof"]["items_sha256"], digest)
        self.assertEqual(json.loads((run / "items.jsonl.sha256").read_bytes()), dict(sha256=digest, records=1))
        self.assertEqual(result["digests"]["items.jsonl"], digest)
        self.assertIn("items.jsonl.sha256", result["digests"])

    def test_items_tampering_missing_fields_and_old_formats_never_seal(self):
        def alter(run, proof, claim, events, change, refresh=False):
            path = run / "items.jsonl"
            lines = [json.loads(line) for line in path.read_bytes().splitlines()]
            change(lines)
            body = ("\n".join(json.dumps(line) for line in lines) + "\n").encode()
            path.write_bytes(body)
            if refresh:
                digest = s.collect.digest(body)
                proof.update(items_sha256=digest, items_records=len(lines) - 1)
                (run / "items.jsonl.sha256").write_text(json.dumps(dict(sha256=digest, records=len(lines) - 1)))
        cases = {
            "file": lambda r, p, c, e: (r / "items.jsonl").unlink(),
            "sidecar": lambda r, p, c, e: (r / "items.jsonl.sha256").unlink(),
            "digest-absent": lambda r, p, c, e: p.pop("items_sha256"),
            "digest-empty": lambda r, p, c, e: p.update(items_sha256=""),
            "count-absent": lambda r, p, c, e: p.pop("items_records"),
            "count-wrong": lambda r, p, c, e: p.update(items_records=2),
            "count-bool": lambda r, p, c, e: p.update(items_records=True),
            "proof-digest": lambda r, p, c, e: p.update(items_sha256="0" * 64),
            "callback-digest": lambda r, p, c, e: (r / "items.jsonl.sha256").write_text('{"sha256":"","records":1}'),
            "callback-count": lambda r, p, c, e: (r / "items.jsonl.sha256").write_text(
                json.dumps(dict(sha256=p["items_sha256"], records=2))),
            "body-splice": lambda *a: alter(*a, lambda rows: rows[1].__setitem__(4, "fixture::other")),
            "truncated": lambda r, p, c, e: (r / "items.jsonl").write_bytes(b'{"schema":'),
            "partial-items": lambda r, p, c, e: (r / "items.jsonl.partial").touch(),
            "zero": lambda *a: alter(*a, lambda rows: rows.pop(), True),
            "old-jsonl": lambda *a: alter(*a, lambda rows: rows.pop(0), True),
            "old-array": lambda *a: alter(*a, lambda rows: rows.__setitem__(1, ["rust/library.rs", 0, 18, "fn"]), True),
            "old-object": lambda *a: alter(*a, lambda rows: rows.__setitem__(1, {"file": "rust/library.rs"}), True),
        }
        for key, value in (("nonce", "old"), ("run_id", "old"), ("root", "/other"), ("crate", "other"),
                           ("kind", "canary-cfg"), ("cfg_clippy", False), ("schema", 0)):
            cases[key] = lambda *a, k=key, v=value: alter(*a, lambda rows: rows[0].update({k: v}), True)
        invalid = [(i, v) for i in (1, 2, 7, 8, 9) for v in (None, True, -1)]
        invalid += [(i, v) for i in (0, 3, 6, 10) for v in (None, "")]
        invalid += [(3, "unknown"), (4, ""), (4, None), (5, "both"), (9, 1), (11, 1)]
        for i, (field, value) in enumerate(invalid):
            cases[f"field{i}"] = lambda *a, k=field, v=value: alter(*a, lambda rows: rows[1].__setitem__(k, v), True)
        cases["all-null"] = lambda *a: alter(*a, lambda rows: rows.__setitem__(1, [None] * 12), True)
        cases["duplicate-id"] = lambda *a: alter(*a, lambda rows: rows.append(rows[1].copy()), True)
        for fold in (-1, True, 2, 1):
            nested = ["rust/library.rs", 1, 17, "nested_fn", "fixture::caller", None, "nested", 1, 2, 1, "Fn", False, fold, False]
            cases[f"fold-{fold}"] = lambda *a, row=nested: alter(*a, lambda rows: rows.append(row), True)
        cases["missing-container"] = lambda *a: alter(*a, lambda rows: (rows[1].__setitem__(3, "trait_method"), rows[1].__setitem__(10, "AssocFn")), True)
        cycle = [["rust/library.rs", 1, 17, "nested_fn", "fixture::caller", None, "nested", 1, d, 99, "Fn", False, f, False]
                 for d, f in ((2, 2), (3, 1))]
        cases["fold-cycle"] = lambda *a: alter(*a, lambda rows: rows.extend(cycle), True)
        for name, mutate in cases.items():
            with self.subTest(name=name):
                self.reject(mutate, "items|partial", "items-" + name)
        for parent in (1, 99, 3):
            nested = ["rust/library.rs", 1, 17, "nested_fn", "fixture::caller", None, "nested", 1, 2, parent, "Fn", False, 0, False]
            middle = ["rust/library.rs", 0, 18, "const", None, "const-item", "constant", 1, 3, 1, "Const", False]
            self.mutate = lambda *a: alter(*a, lambda rows: rows.extend([nested, middle]), True)
            self.assertEqual(self.run_session(f"partial-graph-{parent}")["proof"]["items_records"], 3)
        for name in ("items.jsonl", "items.jsonl.sha256"):
            with self.subTest(stale=name):
                self.reject(lambda r, p, c, e: os.utime(r / name, ns=(0, 0)), "predates", "stale-" + name)
            def alias(run, proof, claim, events):
                path = run / name
                path.rename(run / "alias")
                path.symlink_to(run / "alias")
            with self.subTest(alias=name):
                self.reject(alias, "canonical", "alias-" + name)

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


THREADED = ("import ctypes, mmap, sys, threading\nlib, main = ctypes.CDLL(None), threading.main_thread().ident\n"
            "f = open(sys.argv[1], 'r+b')\nm = mmap.mmap(f.fileno(), 0)\nm[:1] = m[:1]\n"
            "def worker():\n    lib.pthread_join(ctypes.c_void_p(main), None)\n    print('held', flush=True)\n    sys.stdin.read()\n"
            "threading.Thread(target=worker).start()\nlib.pthread_exit(None)\n")
HOLDER = "import mmap, sys\nf = open(sys.argv[1], 'r+b')\nm = mmap.mmap(f.fileno(), 0)\nm[:1] = m[:1]\nprint('held', flush=True)\nsys.stdin.read()\n"


def restore(path: Path, body: bytes) -> None:
    """Write body over path and put its mtime back, as an A->B->A writer that covers its tracks would."""
    st = path.stat()
    path.write_bytes(body)
    os.utime(path, ns=(st.st_atime_ns, st.st_mtime_ns))


class Fence(Harness):
    def test_restored_bytes_and_mtime_still_break_the_fence(self):
        for name, path in (("lib", lambda: self.lib), ("config", lambda: self.conf / "clippy.toml")):
            with self.subTest(name):
                def aba(run, proof, claim, events, path=path()):
                    body = path.read_bytes()
                    restore(path, body.replace(b"(", b"["))
                    restore(path, body)
                self.reject(aba, r"written during Cargo \(ctime_ns\)", "aba-" + name)

    def test_each_stat_field_is_compared(self):
        stat = s.modmap.stat_of(self.lib)
        fence = dict(files={str(self.lib): stat}, dirs={})
        s.check_fence_end(fence)
        for i, field in enumerate(s.modmap.STAT):
            with self.subTest(field), patch.object(s.modmap, "stat_of", return_value=stat[:i] + [stat[i] + 1] + stat[i + 1:]):
                with self.assertRaisesRegex(s.MeasureError, rf"written during Cargo \({field}\)"):
                    s.check_fence_end(fence)
        self.lib.rename(self.root / "moved.rs")
        with self.assertRaisesRegex(s.MeasureError, "removed"):
            s.check_fence_end(fence)

    def test_replacement_by_rename_or_a_hard_link_write_is_seen(self):
        body = self.lib.read_bytes()
        fence = dict(files={str(self.lib): s.modmap.stat_of(self.lib)}, dirs={})
        twin = self.root / "twin.rs"
        twin.write_bytes(body)
        os.utime(twin, ns=(self.lib.stat().st_atime_ns, self.lib.stat().st_mtime_ns))
        os.replace(twin, self.lib)
        with self.assertRaisesRegex(s.MeasureError, "ino"):
            s.check_fence_end(fence)
        os.link(self.lib, twin)
        probe = s.clock_probe(self.root / "probe")
        fence = dict(files={str(self.lib): s.modmap.stat_of(self.lib)}, dirs={})
        self.assertLess(fence["files"][str(self.lib)][4], probe["ctimes"][-1])
        restore(twin, body)
        with self.assertRaisesRegex(s.MeasureError, "ctime_ns"):
            s.check_fence_end(fence)

    def test_capture_rejects_a_file_written_while_it_is_read(self):
        real_open = open
        def writer(path, mode="r", *args, **kwargs):
            handle = real_open(path, mode, *args, **kwargs)
            if Path(path) == self.lib:
                read = handle.read
                def read_then_restore(*a):
                    body = read(*a)
                    s.clock_probe(self.root / "probe")
                    restore(self.lib, body)
                    return body
                handle.read = read_then_restore
            return handle
        self.assertEqual(s.modmap.read_stable(self.lib)[0], self.lib.read_bytes())
        with patch("builtins.open", writer), self.assertRaisesRegex(s.modmap.ModmapError, "changed while it was captured"):
            s.modmap.read_stable(self.lib)
        with patch("builtins.open", writer), self.assertRaisesRegex(s.MeasureError, "source capture"):
            s.source_capture(self.root, self.lib, self.conf)
        self.assertFalse(list(self.root.glob("*/request.json")))

    def test_real_probe_measures_this_filesystem(self):
        probe = s.clock_probe(self.root / "probe")
        step = s.check_probe(probe)
        print(f"\nh2 fence probe ({sys.platform}): step {step} ns, {probe['writes']} writes, "
              f"{probe['same_tick']} same-tick rewrites", file=sys.stderr)
        self.assertEqual((len(probe["ctimes"]), probe["dev"]), (3, self.root.stat().st_dev))
        self.assertFalse((self.root / "probe").exists())
        manifest = self.run_session("probed")
        self.assertEqual(manifest["fence"], s.fence_marker(manifest["request"]["fence"]))
        self.assertEqual(set(manifest["request"]["fence"]["files"]), {str(self.lib), str(self.conf / "clippy.toml")})

    def test_coarse_or_stalled_ctime_is_refused(self):
        ticks = iter(range(10 ** 6))
        cases = {"whole-seconds": (lambda fd: next(ticks) * 10 ** 9, "whole seconds"),
                 "two-second": (lambda fd: next(ticks) // 7 * 2 * 10 ** 9 + 1, "whole seconds"),
                 "stalled": (lambda fd: 5 * 10 ** 17, "did not advance"),
                 "backwards": (lambda fd: 10 ** 18 - next(ticks), "backwards")}
        for name, (stamp, pattern) in cases.items():
            clock = patch.object(s.time, "monotonic", side_effect=(i / 1000 for i in range(10 ** 7)))
            with self.subTest(name), patch.object(s, "probe_ctime", stamp), clock:
                self.reject(lambda *args: None, pattern, name)
                self.assertFalse((self.root / name / "request.json").exists())
                self.assertFalse((self.root / name / "fence.probe").exists())
        for probe in (None, {}, dict(dev=1, ctimes=[1, 2]), dict(dev=1, ctimes=[1, 2, 2]), dict(dev="1", ctimes=[1, 2, 3]),
                      dict(dev=1, ctimes=[1, 2, 3.0]), dict(dev=1, ctimes=[0, 10 ** 9, 2 * 10 ** 9])):
            with self.subTest(probe=probe), self.assertRaises(s.MeasureError):
                s.check_probe(probe)

    def test_a_file_changed_after_the_probe_is_refused(self):
        probe = s.clock_probe
        def then_write(path):
            result = probe(path)
            restore(self.conf / "clippy.toml", (self.conf / "clippy.toml").read_bytes())
            return result
        with patch.object(s, "clock_probe", then_write):
            self.reject(lambda *args: None, "changed after the clock probe", "late")
        self.assertFalse((self.root / "late/request.json").exists())

    def test_fence_must_cover_exactly_the_captured_bytes(self):
        files, stats = {str(self.lib): self.lib.read_bytes()}, {str(self.lib): s.modmap.stat_of(self.lib)}
        fence = dict(files=stats, dirs=s.seal_dirs(self.root, stats), probe=s.clock_probe(self.root / "probe"),
                     mappings=s.mapping_guard(stats))
        s.check_fence(fence, files, self.root)
        self.assertEqual(list(fence["dirs"]), [str(self.lib.parent), str(self.crate), str(self.root)])
        guard, late = fence["mappings"], fence["dirs"][str(self.crate)][:3] + [fence["probe"]["ctimes"][-1]]
        bad = {"empty": dict(fence, files={}), "missing": dict(fence, probe=None), "none": None,
               "no dirs": {k: v for k, v in fence.items() if k != "dirs"}, "no guard": dict(fence, mappings=None),
               "dir unlisted": dict(fence, dirs={k: v for k, v in fence["dirs"].items() if k != str(self.crate)}),
               "dir late": dict(fence, dirs={**fence["dirs"], str(self.crate): late}),
               "unavailable": dict(fence, mappings=dict(status="unavailable", platform="sunos")),
               "unscanned": dict(fence, mappings=dict(guard, processes=0)),
               "extra": dict(fence, files={**fence["files"], "/other": fence["files"][str(self.lib)]}),
               "short": dict(fence, files={str(self.lib): fence["files"][str(self.lib)][:4]}),
               "bool": dict(fence, files={str(self.lib): [True] + fence["files"][str(self.lib)][1:]}),
               "device": dict(fence, probe=dict(fence["probe"], dev=fence["probe"]["dev"] + 1))}
        for name, forged in bad.items():
            with self.subTest(name), self.assertRaises(s.MeasureError):
                s.check_fence(forged, files, self.root)
        with self.assertRaisesRegex(s.MeasureError, "size"):
            s.check_fence(fence, {str(self.lib): self.lib.read_bytes() + b"x"}, self.root)
        with self.assertRaisesRegex(s.MeasureError, "differs from the captured files"):
            s.check_fence(fence, {**files, "/other": b""}, self.root)
        with patch.object(s.sys, "platform", "sunos"):
            self.reject(lambda *args: None, "shared-mapping guard not checked .*unavailable", "unguarded")

    def test_directory_swap_and_restore_is_refused(self):
        staged = Path(tempfile.mkdtemp(dir=self.root.parent))
        self.addCleanup(lambda: __import__("shutil").rmtree(staged))
        (staged / "B").mkdir()
        (staged / "B" / self.lib.name).write_bytes(self.lib.read_bytes().replace(b"caller", b"callee"))
        seen = []
        def swap(run, proof, claim, events):
            here = self.lib.parent
            seen.append(s.modmap.stat_of(self.lib))
            here.rename(staged / "A")
            (staged / "B").rename(here)
            self.assertIn(b"callee", self.lib.read_bytes())
            here.rename(staged / "B")
            (staged / "A").rename(here)
        self.reject(swap, r"directory \S+/crate changed during Cargo \(mtime_ns,ctime_ns\)", "swapped")
        self.assertEqual(seen, [s.modmap.stat_of(self.lib)])

    def test_cargo_creating_its_target_dir_is_not_a_namespace_change(self):
        for name, extra, target in (("default", (), self.root / "target"), ("flag", ("--target-dir=out",), self.crate / "out")):
            with self.subTest(name):
                self.after = lambda run: (target / "debug").mkdir(parents=True, exist_ok=True)
                self.assertIn(str(self.crate), self.run_session(name, extra=extra)["request"]["fence"]["dirs"])

    def test_symlink_leaf_or_component_is_refused(self):
        (self.crate / "alias.rs").symlink_to(self.lib)
        (self.root / "via").symlink_to(self.lib.parent)
        for path in (self.crate / "alias.rs", self.root / "via" / self.lib.name):
            with self.subTest(path.name), self.assertRaisesRegex(s.MeasureError, "symlink"):
                s.seal_dirs(self.root, {str(path): s.modmap.stat_of(path)})

    def test_shared_writable_mapping_refuses_the_session(self):
        holder = subprocess.Popen([sys.executable, "-c", HOLDER, str(self.lib)], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        self.addCleanup(holder.kill)
        self.assertEqual(holder.stdout.readline(), b"held\n")
        with patch.object(s, "list_pids", LIST_PIDS):
            self.reject(lambda *args: None, rf"process {holder.pid} maps {re.escape(str(self.lib))} shared and writable", "mapped")
            self.assertFalse((self.root / "mapped/request.json").exists())
            holder.communicate(b"")
            guard = self.run_session("unmapped")["request"]["fence"]["mappings"]
        self.assertEqual(guard["status"], "checked")
        self.assertGreater(guard["processes"], 1)

    def test_mapping_left_to_a_worker_after_the_main_thread_exits_is_refused(self):
        holder = subprocess.Popen([sys.executable, "-c", THREADED, str(self.lib)], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        self.addCleanup(holder.communicate)
        self.addCleanup(holder.kill)
        self.assertEqual(holder.stdout.readline(), b"held\n")
        for _ in range(500 if sys.platform == "linux" else 0):
            if not Path(f"/proc/{holder.pid}/maps").read_bytes():
                break
            time.sleep(0.01)
        else:
            self.assertNotEqual(sys.platform, "linux", "the leader kept its address space")
        with patch.object(s, "list_pids", LIST_PIDS):
            self.assertRaisesRegex(s.MeasureError, rf"process {holder.pid} maps \S+ shared and writable", s.mapping_guard,
                                   {str(self.lib): s.modmap.stat_of(self.lib)})

    def test_linux_scan_reads_every_task_and_refuses_an_undecidable_process(self):
        st, stats = self.lib.stat(), {str(self.lib): s.modmap.stat_of(self.lib)}
        line = f"7f00-7f01 rw-s 0 {os.major(st.st_dev):x}:{os.minor(st.st_dev):x} {st.st_ino} {self.lib}\n".encode()
        def task(where, state, maps=b"", flags=0):
            where.mkdir(parents=True)
            (where / "status").write_text(f"State:\t{state} (x)\nUid:\t{os.getuid()}\t{os.getuid()}\n")
            (where / "stat").write_text(f"7 (a) b) {state} 1 1 1 0 -1 {flags} 0")
            (where / "maps").write_bytes(maps)
        for name, leader, flags, tasks, refusal in (
                ("worker", "Z", 0, [("Z", b""), ("S", line)], "shared and writable"), ("unlisted", "S", 0, [], "address space"),
                ("live and empty", "S", 0, [("S", b"")], "address space"), ("unlistable", "S", 0, None, "address space"),
                ("exited", "Z", 0, [("Z", b""), ("Z", b"")], None), ("kernel", "I", s.PF_KTHREAD, [("I", b"")], None)):
            proc = self.root / "proc" / name.replace(" ", "-")
            task(proc / "7", leader, flags=flags)
            (proc / "7/task").mkdir()
            for tid, (state, maps) in enumerate(tasks or [], 7):
                task(proc / f"7/task/{tid}", state, maps)
            os.chmod(proc / "7/task", 0o700 if tasks is not None else 0)
            self.addCleanup(os.chmod, proc / "7/task", 0o700)
            with self.subTest(name), patch.object(s, "PROC_ROOT", proc), patch.object(s.sys, "platform", "linux"), \
                    patch.object(s, "list_pids", LIST_PIDS):
                if refusal:
                    self.assertRaisesRegex(s.MeasureError, refusal, s.mapping_guard, stats)
                else:
                    self.assertEqual(s.mapping_guard(stats)["processes"], 1)

    def test_unreadable_scans_fail_closed_except_the_foreign_residual_boundary(self):
        stats = {str(self.lib): s.modmap.stat_of(self.lib)}
        me, other = os.getuid(), os.getuid() + 1
        # A foreign 0644 pass is the residual boundary: a writable mapping made while the file was looser is not seen.
        for uids, mode, verdict in (({0}, 0o644, "root"), ({other}, 0o644, "foreign"), ({me}, 0o644, None),
                                    ({other}, 0o664, None), ({me, 0}, 0o666, "root")):
            os.chmod(self.lib, mode)
            with self.subTest(uids=uids, mode=mode), patch.object(s, "read_maps", return_value=(uids, None)):
                if verdict is None:
                    self.assertRaisesRegex(s.MeasureError, "cannot read the mappings", s.mapping_guard, stats)
                else:
                    self.assertEqual(s.mapping_guard(stats)[verdict], 1)
        scope, alien = (f"0::/user.slice/user-{u}.slice/user@{u}.service/init.scope\n" for u in (me, other))
        for n, (name, ppid, cgroup, uid) in enumerate((("systemd", 1, scope, me), ("systemd", 2, scope, me),
                                                       ("python3", 1, scope, me), ("systemd", 1, alien, other),
                                                       ("systemd", 1, scope.replace("init", "app"), me))):
            (proc := self.root / f"proc{n}" / "9").mkdir(parents=True)
            (proc / "status").write_text(f"Name:\t{name}\nPPid:\t{ppid}\n"), (proc / "cgroup").write_text(cgroup)
            with self.subTest(name, ppid=ppid, cgroup=cgroup), patch.object(s, "PROC_ROOT", proc.parent), \
                    patch.object(s.sys, "platform", "linux"), patch.object(s, "list_pids", lambda: [9]), \
                    patch.object(s, "read_maps", return_value=({uid}, None)):
                self.assertEqual(s.mapping_guard(stats)["user_managers"], [[9, "systemd"]]) if not n else \
                    self.assertRaisesRegex(s.MeasureError, "cannot read the mappings of process 9", s.mapping_guard, stats)
        pids = iter(range(1, 100))
        with patch.object(s, "list_pids", lambda: [next(pids)]), patch.object(s, "read_maps", return_value=None):
            self.assertRaisesRegex(s.MeasureError, "kept appearing", s.mapping_guard, stats)

    def test_unresolvable_paths_are_measure_errors(self):
        (self.root / "loop").symlink_to(self.root / "loop")
        with self.assertRaisesRegex(s.MeasureError, "cannot resolve"):
            s.session(self.root, self.root / "loop", self.root / "looped", self.conf, "macos", driver=self.driver)
        with patch.object(s.Path, "resolve", side_effect=RuntimeError("Symlink loop")):
            for name in ("crate", "config"):
                with self.subTest(name), self.assertRaisesRegex(s.MeasureError, "cannot resolve .*Symlink loop"):
                    if name == "crate":
                        self.run_session("looped")
                    else:
                        s.source_capture(self.root, self.lib, self.conf)
        self.assertFalse((self.root / "looped").exists())


if __name__ == "__main__":
    unittest.main()
