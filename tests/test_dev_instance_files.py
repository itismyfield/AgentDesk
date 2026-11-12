"""Behavioral tests for private, fresh instance files; no live credentials."""

import json
import stat
import tempfile
import unittest
import uuid
from pathlib import Path

from scripts.dev_instance.files import (
    InstanceError, create_layout, instance_root, safe_absolute,
    validate_tree, write_private_json,
)


class InstanceFilesTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name).resolve()
        self.root = instance_root(self.base, str(uuid.uuid4()))
        create_layout(self.root)

    def test_fresh_layout_and_json_are_private(self):
        validate_tree(self.root)
        for relative in ("", "config", "credential", "runtime", "data", "postgres/data", "postgres/socket"):
            self.assertEqual(stat.S_IMODE((self.root / relative).stat().st_mode), 0o700)
        write_private_json(self.root, "config/startup.json", {"enabled": True})
        path = self.root / "config/startup.json"
        self.assertEqual(json.loads(path.read_text()), {"enabled": True})
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)

    def test_existing_root_and_json_are_preserved(self):
        write_private_json(self.root, "receipt.json", {"owned": True})
        before = (self.root / "receipt.json").read_bytes()
        with self.assertRaisesRegex(InstanceError, "fresh"):
            create_layout(self.root)
        with self.assertRaisesRegex(InstanceError, "new"):
            write_private_json(self.root, "receipt.json", {"owned": False})
        self.assertEqual((self.root / "receipt.json").read_bytes(), before)

    def test_root_declarations_reject_external_aliases_and_non_uuid_ids(self):
        for path in (Path("relative"), self.base / ".." / "escape", self.base / ".adk/release/data"):
            with self.subTest(path=path), self.assertRaises(InstanceError):
                safe_absolute(path)
        for instance_id in ("../escape", "lane-name", str(uuid.uuid4()).upper(), ""):
            with self.subTest(instance_id=instance_id), self.assertRaisesRegex(InstanceError, "UUID"):
                instance_root(self.base, instance_id)

    def test_root_ancestor_and_leaf_symlinks_preserve_external_files(self):
        external = self.base / "external"
        external.mkdir()
        marker = external / "marker"
        marker.write_bytes(b"unchanged")
        alias = self.base / "alias"
        alias.symlink_to(external, target_is_directory=True)
        for path in (alias, alias / "future-root", alias / "marker"):
            with self.subTest(path=path), self.assertRaisesRegex(InstanceError, "symlink"):
                safe_absolute(path)
        leaf = self.root / "config/link.json"
        leaf.symlink_to(marker)
        with self.assertRaisesRegex(InstanceError, "symlink"):
            write_private_json(self.root, "config/link.json", {"replacement": True})
        self.assertEqual(marker.read_bytes(), b"unchanged")

    def test_tree_rejects_legacy_credentials_and_unrelated_links(self):
        for relative in ("config/credential", "runtime/credentials"):
            path = self.root / relative
            path.mkdir()
            with self.subTest(relative=relative), self.assertRaisesRegex(InstanceError, "legacy"):
                validate_tree(self.root)
            path.rmdir()
        link = self.root / "outputs/alias"
        link.symlink_to(self.base)
        with self.assertRaisesRegex(InstanceError, "symlink"):
            validate_tree(self.root)

    def test_case_aliases_of_release_and_legacy_credentials_are_rejected(self):
        marker = self.base / "external-marker"
        marker.write_bytes(b"outside unchanged")
        cases = (
            (self.base / ".ADK/ReLeAsE/config/profile.json", False),
            (self.root / "config/Credential/unknown_token", True),
            (self.root / "runtime/CREDENTIALS/unknown_token", True),
        )
        for path, inside_root in cases:
            path.parent.mkdir(parents=True)
            path.write_bytes(b"forbidden unchanged")
            with self.subTest(path=path.relative_to(self.base)):
                try:
                    with self.assertRaises(InstanceError):
                        validate_tree(self.root) if inside_root else safe_absolute(path)
                finally:
                    self.assertEqual(path.read_bytes(), b"forbidden unchanged")
                    self.assertEqual(marker.read_bytes(), b"outside unchanged")
                    path.unlink()
                    path.parent.rmdir()


    def test_json_cannot_escape_instance_root(self):
        external = self.base / "external.json"
        for relative in (external, "../external.json", "config/../../external.json"):
            with self.subTest(relative=relative), self.assertRaises(InstanceError):
                write_private_json(self.root, relative, {"escape": True})
        self.assertFalse(external.exists())


if __name__ == "__main__":
    unittest.main()
