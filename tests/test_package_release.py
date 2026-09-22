"""Exercise real zip/tar artifacts, not copies of packaging implementation."""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
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
            "scripts/queue-stability-batch.sh": "#!/bin/sh\nexit 0\n",
            "scripts/_defaults.sh": "# defaults\n",
            "migrations/postgres/0001_initial.sql": "SELECT 1;\n",
        }.items():
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
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


if __name__ == "__main__":
    unittest.main()
