"""Exercise real zip/tar artifacts, not copies of packaging implementation."""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import package_release as packaging
import verify_release_artifacts as verification
import install_runtime_assets as runtime_assets


class ReleasePackagingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "repo"
        self.root.mkdir()
        self.output = Path(self.temp.name) / "dist"
        for name, text in {
            "Cargo.toml": '[package]\nname="agentdesk"\nversion="1.2.3"\n',
            "policies/default-pipeline.yaml": "stages: []\n",
            "policies/lib/shared.js": "module.exports = { enabled: true };\n",
            "policies/timeouts/default.js": "module.exports = 30;\n",
            "policies/auto-queue.capabilities.yaml": "allow: []\n",
            "routines/queue.js": "module.exports = {};\n",
            "scripts/queue-stability-batch.sh": "#!/bin/sh\nexit 0\n",
            "scripts/_defaults.sh": "# defaults\n",
            "migrations/postgres/0001_initial.sql": "SELECT 1;\n",
        }.items():
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
        for relative in runtime_assets.runtime_assets(ROOT):
            path = self.root / relative
            if relative in {"policies", "routines", "skills", "scripts/launchd-migrated"} or (ROOT / relative).is_dir():
                path = path / "contract-fixture.dat"
            path.parent.mkdir(parents=True, exist_ok=True)
            if not path.exists():
                path.write_text("runtime fixture\n", encoding="utf-8")
        for relative in ("scripts/runtime-assets.txt", "scripts/install_runtime_assets.py"):
            shutil.copyfile(ROOT / relative, self.root / relative)
        for args in [["init", "-q"], ["add", "."],
                     ["-c", "user.name=Release Test", "-c", "user.email=release@example.invalid", "commit", "-qm", "fixture"]]:
            subprocess.run(["git", "-C", str(self.root), *args], check=True, capture_output=True)
        # These local files must never leak into packages, even under assets.
        (self.root / "agentdesk.yaml").write_text("private configuration")
        (self.root / "policies/operator-local.yaml").write_text("private policy")
        dashboard = self.root / "dashboard/dist"
        dashboard.mkdir(parents=True)
        (dashboard / "index.html").write_text("<html>test</html>")

    def binary(self, system):
        path = Path(self.temp.name) / f"binary-{system}"
        if system == "windows":
            header = bytearray(64)
            header[:2] = b"MZ"
            struct.pack_into("<I", header, 60, 64)
            header += b"PE\0\0" + struct.pack("<H", 0x8664)
        elif system == "darwin":
            header = b"\xcf\xfa\xed\xfe" + struct.pack("<I", 0x0100000C) + bytes(56)
        else:
            header = bytearray(64)
            header[:6] = b"\x7fELF\x02\x01"
            struct.pack_into("<H", header, 18, 62)
        path.write_bytes(header)
        return path

    def read_archive(self, archive):
        if archive.suffix == ".zip":
            with zipfile.ZipFile(archive) as bundle:
                return {n: bundle.read(n) for n in bundle.namelist()}
        with tarfile.open(archive) as bundle:
            return {m.name: bundle.extractfile(m).read() for m in bundle if m.isfile()}

    def test_all_three_platform_layouts_and_manifest_hashes(self):
        for target, system in [("x86_64-pc-windows-msvc", "windows"),
                               ("aarch64-apple-darwin", "darwin"),
                               ("x86_64-unknown-linux-gnu", "linux")]:
            with self.subTest(target=target):
                archive = packaging.package(self.root, self.binary(system), target, self.output)
                files = self.read_archive(archive)
                prefix = archive.name.removesuffix(".tar.gz").removesuffix(".zip") + "/"
                manifest = json.loads(files[prefix + "release-manifest.json"])
                self.assertEqual(manifest["target"], target)
                self.assertEqual(manifest["version"], "1.2.3")
                self.assertTrue(manifest["dashboard_included"])
                self.assertIn("dashboard/dist/index.html", manifest["files"])
                self.assertIn("scripts/queue-stability-batch.sh", manifest["files"])
                self.assertNotIn("agentdesk.yaml", manifest["files"])
                self.assertNotIn("policies/operator-local.yaml", manifest["files"])
                for relative, digest in manifest["files"].items():
                    self.assertEqual(hashlib.sha256(files[prefix + relative]).hexdigest(), digest)
                runtime = json.loads(files[prefix + "runtime/release-source.json"])
                self.assertEqual(runtime["repo_head"], packaging.git(self.root, "rev-parse", "HEAD").strip())
                self.assertIsInstance(runtime["repo_dirty"], str)
                checksum = (self.output / (archive.name + ".sha256")).read_text()
                self.assertEqual(checksum, f"{packaging.sha256(archive)}  {archive.name}\n")
                self.assertEqual(verification.verify_archive(
                    archive, manifest["repo_head"], "1.2.3", allow_dirty=True), target)
        self.assertEqual(len((self.output / "checksums.txt").read_text().splitlines()), 3)

    def test_publishing_rejects_a_different_build_profile(self):
        target = "x86_64-pc-windows-msvc"
        archive = packaging.package(self.root, self.binary("windows"), target, self.output, profile="release-fast")
        head = packaging.git(self.root, "rev-parse", "HEAD").strip()
        self.assertEqual(verification.verify_archive(archive, head, "1.2.3", allow_dirty=True, profile="release-fast"), target)
        with self.assertRaisesRegex(ValueError, "build profile"):
            verification.verify_archive(archive, head, "1.2.3", allow_dirty=True, profile="release")

    def test_dashboard_exclusion_is_explicit_and_missing_dashboard_fails(self):
        binary = self.binary("windows")
        archive = packaging.package(self.root, binary, "x86_64-pc-windows-msvc", self.output, False)
        self.assertFalse(any("/dashboard/" in n for n in self.read_archive(archive)))
        (self.root / "dashboard/dist/index.html").unlink()
        with self.assertRaisesRegex(ValueError, "dashboard/dist/index.html"):
            packaging.package(self.root, binary, "x86_64-pc-windows-msvc", self.output)

    def test_wrong_binary_target_is_rejected_before_archive_is_written(self):
        with self.assertRaisesRegex(ValueError, "format/architecture"):
            packaging.package(self.root, self.binary("linux"), "x86_64-pc-windows-msvc", self.output)
        self.assertFalse(self.output.exists())

    def test_publish_rejects_dirty_checkout_wrong_commit_and_corrupt_archive(self):
        archive = packaging.package(self.root, self.binary("windows"), "x86_64-pc-windows-msvc", self.output)
        head = packaging.git(self.root, "rev-parse", "HEAD").strip()
        with self.assertRaisesRegex(ValueError, "dirty checkout"):
            verification.verify_archive(archive, head, "1.2.3")
        with self.assertRaisesRegex(ValueError, "source/version"):
            verification.verify_archive(archive, "0" * 40, "1.2.3", allow_dirty=True)
        with archive.open("ab") as stream:
            stream.write(b"corrupt")
        with self.assertRaisesRegex(ValueError, "archive checksum"):
            verification.verify_archive(archive, head, "1.2.3", allow_dirty=True)

    def test_publish_rejects_missing_and_modified_runtime_files(self):
        for corrupt in ["missing", "modified"]:
            with self.subTest(corrupt=corrupt):
                archive = packaging.package(self.root, self.binary("windows"), "x86_64-pc-windows-msvc", self.output)
                files = self.read_archive(archive)
                entry = "agentdesk-windows-x86_64/dashboard/dist/index.html"
                if corrupt == "missing":
                    del files[entry]
                else:
                    files[entry] = b"unexpected replacement"
                with zipfile.ZipFile(archive, "w") as bundle:
                    for name, content in files.items():
                        bundle.writestr(name, content)
                (self.output / (archive.name + ".sha256")).write_text(
                    f"{packaging.sha256(archive)}  {archive.name}\n")
                with self.assertRaisesRegex(ValueError, "contents differ|runtime file checksum"):
                    verification.verify_archive(archive, packaging.git(self.root, "rev-parse", "HEAD").strip(),
                                                "1.2.3", allow_dirty=True)

    def test_asset_link_cannot_escape_package_source_boundary(self):
        outside = Path(self.temp.name) / "operator-secret"
        outside.write_text("do not package")
        with self.assertRaisesRegex(ValueError, "outside"):
            packaging.copy_file(outside, self.output / "leak", self.root)
        self.assertFalse(self.output.exists())

    def install_assets(self, source, destination, helper=None, contract=None):
        command = [sys.executable, str(helper or source / "scripts/install_runtime_assets.py"),
                   str(source), str(destination)]
        if contract is not None:
            command.extend(["--contract", str(contract)])
        result = subprocess.run(
            command,
            capture_output=True, text=True, check=False,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def test_source_and_release_install_every_packaged_contract_asset(self):
        archive = packaging.package(self.root, self.binary("darwin"), "aarch64-apple-darwin", self.output)
        release = Path(self.temp.name) / "unpacked"
        prefix = "agentdesk-darwin-aarch64/"
        for name, content in self.read_archive(archive).items():
            path = release / name.removeprefix(prefix)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
        expected = set(packaging.git(self.root, "ls-files", "-z", "--", *runtime_assets.runtime_assets(self.root)).split("\0")) - {""}
        for source in (self.root, release):
            with self.subTest(source=source):
                destination = Path(self.temp.name) / (source.name + "-install")
                self.install_assets(source, destination)
                ledger = json.loads((destination / "runtime/install-assets.json").read_text())
                self.assertEqual(set(ledger), expected)
                for relative in expected:
                    self.assertEqual((destination / relative).read_bytes(), (source / relative).read_bytes())
                self.assertFalse((destination / "policies/operator-local.yaml").exists())

    def test_old_release_uses_external_helper_contract_and_preserves_operator_edits(self):
        archive = packaging.package(self.root, self.binary("darwin"), "aarch64-apple-darwin", self.output)
        release = Path(self.temp.name) / "old-release"
        prefix = "agentdesk-darwin-aarch64/"
        bootstrap = {"scripts/runtime-assets.txt", "scripts/install_runtime_assets.py"}
        for name, content in self.read_archive(archive).items():
            relative = name.removeprefix(prefix)
            if relative in bootstrap:
                continue
            path = release / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
        destination = Path(self.temp.name) / "old-release-install"
        operator = destination / "routines/operator.js"
        operator.parent.mkdir(parents=True)
        operator.write_text("operator routine")
        helper = ROOT / "scripts/install_runtime_assets.py"
        contract = ROOT / "scripts/runtime-assets.txt"
        self.install_assets(release, destination, helper=helper, contract=contract)
        expected = set(packaging.git(self.root, "ls-files", "-z", "--",
                                    *runtime_assets.runtime_assets(self.root)).split("\0")) - {""} - bootstrap
        ledger = destination / "runtime/install-assets.json"
        self.assertEqual(set(json.loads(ledger.read_text())), expected)
        owned = "policies/lib/shared.js"
        edited = destination / "routines/queue.js"
        edited.write_text("operator edit")
        (release / owned).write_text("module.exports = { updated: true };\n")
        (release / "routines/queue.js").write_text("upstream edit")
        self.install_assets(release, destination, helper=helper, contract=contract)
        self.assertEqual((destination / owned).read_bytes(), (release / owned).read_bytes())
        self.assertEqual(edited.read_text(), "operator edit")
        self.assertEqual(operator.read_text(), "operator routine")
        for relative in bootstrap:
            self.assertFalse((release / relative).exists())
            self.assertFalse((destination / relative).exists())

    def test_reinstall_updates_owned_assets_and_preserves_operator_files_and_links(self):
        destination = Path(self.temp.name) / "install"
        private = {
            "config/agentdesk.yaml": "operator config",
            "credentials/token": "operator credential",
            "workspaces/example/file": "operator workspace",
            "policies/default-pipeline.yaml": "operator override",
            "policies/operator.js": "operator policy",
            "routines/operator.js": "operator routine",
        }
        for relative, content in private.items():
            path = destination / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)
        self.install_assets(self.root, destination)
        edited = destination / "routines/queue.js"
        edited.write_text("operator edit")
        private["routines/queue.js"] = "operator edit"
        owned = "policies/lib/shared.js"
        (self.root / owned).write_text("module.exports = { updated: true };\n")
        (self.root / "routines/queue.js").write_text("upstream change")
        outside = Path(self.temp.name) / "outside"
        outside.mkdir()
        (outside / "default.js").write_text("operator symlink target")
        timeout = destination / "policies/timeouts"
        shutil.rmtree(timeout)
        timeout.symlink_to(outside, target_is_directory=True)
        result = self.install_assets(self.root, destination)
        self.assertIn("Preserved operator asset", result.stdout)
        self.assertEqual((destination / owned).read_bytes(), (self.root / owned).read_bytes())
        for relative, content in private.items():
            self.assertEqual((destination / relative).read_text(), content)
        self.assertTrue(timeout.is_symlink())
        self.assertEqual((outside / "default.js").read_text(), "operator symlink target")
        ledger = destination / "runtime/install-assets.json"
        saved = ledger.read_bytes()
        ledger.unlink()
        ledger.symlink_to(outside / "ledger")
        failure = subprocess.run([sys.executable, str(self.root / "scripts/install_runtime_assets.py"),
                                  str(self.root), str(destination)], capture_output=True, text=True)
        self.assertNotEqual(failure.returncode, 0)
        self.assertIn("ledger is linked", failure.stderr)
        self.assertFalse((outside / "ledger").exists())
        self.assertTrue(saved)


if __name__ == "__main__":
    unittest.main()
