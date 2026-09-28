"""Exercise the map CLI and metadata consumer with independent Cargo/driver fixtures."""
import json
import os
import shutil
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

from tests import test_h2_modmap as fixture

sys.path.insert(0, str(fixture.REPO_ROOT / "scripts/ci"))
import h2_cfg_collect as collect


class Collection(unittest.TestCase):
    def setUp(self):
        fixture.Wrapper.setUp(self)
        self.maps, self.root = self.maps.resolve(), self.root.resolve()
        self.lane = "macos" if sys.platform == "darwin" else "linux"
        self.host = "aarch64-apple-darwin" if self.lane == "macos" else "x86_64-unknown-linux-gnu"
        for tool in ("rustc", "clippy-driver"):
            path = self.maps / "bin" / tool
            path.write_text(f"#!/bin/sh\necho '{tool} fixture\nhost: {self.host}'\n")
            path.chmod(0o755)
        (self.root / ".gitignore").write_text("target/\n")
        self.env["STUB_CALLS"] = str(self.maps / "calls.jsonl")
        for args in (("init", "-q"), ("add", "."),
                     ("-c", "user.name=fixture", "-c", "user.email=fixture@example.org", "commit", "-qm", "fixture")):
            subprocess.run(["git", *args], cwd=self.root, check=True, capture_output=True)

    def run_wrapper(self, *args, **env):
        return fixture.Wrapper.run_wrapper(self, "--lane", self.lane, *args, **env)

    def bundle(self):
        code, output = self.run_wrapper()
        self.assertEqual(code, 0, output)
        manifest = max((self.root / "target/h2/runs").glob("*/root/*.meta.json"), key=lambda p: p.stat().st_mtime_ns)
        value = collect.read_manifest(manifest, lane=self.lane, root=self.root)
        return manifest, value

    def read(self, manifest):
        return collect.read_manifest(manifest, lane=self.lane, root=self.root)

    def test_single_root_expansion_and_explicit_output(self):
        run = self.root / "target/explicit"
        code, output = self.run_wrapper("--out", str(run / "only.tsv"), "--cfg-out", str(run / "cfg.json"),
                                        "--meta-out", str(run / "manifest.json"))
        self.assertEqual(code, 0, output)
        value = self.read(run / "manifest.json")
        self.assertEqual(value["file_modules"], 1000)
        self.assertEqual(value["paths"]["tsv"], str(run / "only.tsv"))
        calls = [json.loads(line) for line in (self.maps / "calls.jsonl").read_text().splitlines()]
        self.assertEqual([args[0] for args in calls], ["build", "check", "check"])
        checks = [args[args.index("--manifest-path") + 1] for args in calls[1:]]
        self.assertEqual(checks, [str(self.root / "tools/modmap-driver/canary/Cargo.toml"), str(self.root / "Cargo.toml")])
        self.assertEqual(list(run.glob("*.tsv")), [run / "only.tsv"])

    def test_cli_input_host_and_inert_contract(self):
        self.assertEqual(self.run_wrapper("--meta-out", str(self.maps / "wrong/meta.json"))[0], 2)
        self.assertEqual(self.run_wrapper("--repo", str(self.maps / "absent"))[0], 2)
        self.assertEqual(self.run_wrapper("--out", str(self.full / "map.tsv"))[0], 2)
        rustc = self.maps / "bin/rustc"
        rustc.write_text("#!/bin/sh\necho 'host: unknown-host'\n")
        code, output = self.run_wrapper()
        self.assertEqual((code, "requires" in output), (3, True), output)
        (self.root / fixture.h2.BASELINE_FILES[0]).unlink()
        code, output = self.run_wrapper("--inert")
        self.assertEqual((code, "root=skipped" in output), (0, True), output)
        self.assertFalse((self.maps / "calls.jsonl").exists())

    def test_cargo_failure_and_missing_output_never_publish(self):
        self.bundle()
        for mode, needle in (("fail-after", "failed (101)"), ("no-proof", "was not written"),
                             ("repost", "cfg run/nonce mismatch")):
            with self.subTest(mode=mode):
                before = set((self.root / "target/h2/runs").glob("*/*/*.json"))
                code, output = self.run_wrapper(STUB_CFG_ROOT=mode)
                self.assertEqual((code, needle in output), (1, True), output)
                added = set((self.root / "target/h2/runs").glob("*/*/*.json")) - before
                self.assertFalse(any(p.name.endswith("meta.json") for p in added))
                self.assertTrue(any(p.name == "modmap.cfg.json" for p in added))

    def test_bound_canary_semantic_checks_remain_required(self):
        self.bundle()
        for mode, needle in (("feature", "probe atoms differ"), ("target", "session target/codegen atoms differ")):
            with self.subTest(mode=mode):
                code, output = self.run_wrapper("--inert", "--canary", STUB_CFG_CANARY=mode)
                self.assertEqual((code, needle in output), (1, True), output)

    def test_reposted_cfg_with_current_invocation_is_rejected_before_sealing(self):
        old_manifest, old = self.bundle()
        manifest, value = self.bundle()
        proof = json.loads(Path(value["paths"]["invocation"]).read_bytes())
        self.assertEqual(proof["nonce"], value["nonce"])
        self.assertNotEqual(old["nonce"], value["nonce"])
        shutil.copyfile(old["paths"]["cfg"], value["paths"]["cfg"])
        manifest.unlink()
        with self.assertRaisesRegex(ValueError, "cfg run/nonce mismatch"):
            collect.seal(manifest.parent, manifest)
        self.assertFalse(manifest.exists())
        self.read(old_manifest)

    def test_tsv_and_canary_cannot_be_spliced_into_root_bundle(self):
        manifest, value = self.bundle()
        tsv = Path(value["paths"]["tsv"])
        tsv.write_text(tsv.read_text().replace("crate::m0", "crate::other"))
        with self.assertRaisesRegex(ValueError, "driver TSV bytes mismatch"):
            self.read(manifest)
        canary = next(manifest.parent.parent.glob("canary/metadata.json"))
        with self.assertRaisesRegex(ValueError, "root/lane/kind mismatch"):
            self.read(canary)
        manifest.unlink()
        with self.assertRaisesRegex(ValueError, "driver TSV bytes mismatch"):
            collect.seal(manifest.parent, manifest)
        shutil.copyfile(canary.parent / "modmap.tsv", tsv)
        with self.assertRaisesRegex(ValueError, "driver TSV bytes mismatch"):
            collect.seal(manifest.parent, manifest)

    def test_manifest_required_and_last_rename_failure(self):
        manifest, value = self.bundle()
        manifest.unlink()
        with self.assertRaisesRegex(ValueError, "regular canonical run file"):
            self.read(manifest)
        original = Path.replace
        def replace(path, target):
            if Path(target) == manifest:
                raise OSError("manifest rename denied")
            return original(path, target)
        with patch.object(Path, "replace", replace), self.assertRaisesRegex(OSError, "rename denied"):
            collect.seal(manifest.parent, manifest)
        self.assertFalse(manifest.exists())
        partial = manifest.with_name(manifest.name + ".partial")
        self.assertTrue(partial.exists())
        with self.assertRaisesRegex(ValueError, "final manifest path mismatch"):
            self.read(partial)
        with self.assertRaisesRegex(ValueError, "regular canonical run file"):
            self.read(manifest)
        collect.seal(manifest.parent, manifest)
        self.assertEqual(self.read(manifest)["tsv_digest"], value["tsv_digest"])

    def test_invalid_records_and_alternate_file_forms(self):
        manifest, value = self.bundle()
        cfg = Path(value["paths"]["cfg"])
        original, stamp = cfg.read_bytes(), cfg.stat().st_mtime_ns
        for data, needle in ((b"", "Expecting value"), (b'{"schema":', "Expecting value"),
                             (b"[]", "cfg run/nonce mismatch"),
                             (original.replace(b'debug_assertions', b'debug_assumption'), "manifest digest/content mismatch")):
            with self.subTest(data=data[:20]):
                cfg.write_bytes(data)
                os.utime(cfg, ns=(stamp, stamp))
                with self.assertRaisesRegex(ValueError, needle):
                    self.read(manifest)
        cfg.write_bytes(original)
        self.read(manifest)
        saved = self.maps / "saved.cfg"
        cfg.rename(saved)
        cfg.symlink_to(saved)
        with self.assertRaisesRegex(ValueError, "regular canonical run file"):
            self.read(manifest)
        cfg.unlink()
        saved.rename(cfg)
        copied = self.maps / "copied"
        shutil.copytree(manifest.parent, copied)
        with self.assertRaisesRegex(ValueError, "final manifest path mismatch"):
            self.read(copied / manifest.name)
        cargo = Path(value["paths"]["cargo"])
        record = json.loads(cargo.read_bytes())
        record["rc"] = 101
        cargo.write_text(json.dumps(record))
        manifest.unlink()
        with self.assertRaisesRegex(ValueError, "Cargo run failed"):
            collect.seal(manifest.parent, manifest)


if __name__ == "__main__":
    unittest.main()
