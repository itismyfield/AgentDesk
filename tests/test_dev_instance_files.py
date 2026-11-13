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




"""Owned live sockets and pinned runtime files, using only private synthetic fixtures."""

import json
import os
from pathlib import Path
import socket
import stat
import tempfile
import unittest
from unittest.mock import patch
import uuid

from scripts.dev_instance import files, manifest
from tests.test_dev_instance import profile


class InstanceLiveFilesTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="lf-", dir="/tmp")
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        self.worktree = self.base / "worktree"
        self.worktree.mkdir()
        self.instance_id = str(uuid.uuid4())
        with patch.object(manifest, "worktree_identity", return_value="1" * 40), \
                patch.object(manifest, "check_port", side_effect=lambda port, pool: port):
            self.declaration = manifest.prepare(self.base, self.instance_id, self.worktree, profile(), {},
                                                api_port=18791, pg_port=25432)
        self.root = Path(self.declaration["root"])

    def bind(self, relative):
        path = self.root / relative
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.addCleanup(listener.close)
        listener.bind(str(path))
        listener.listen(1)
        os.chmod(path, 0o777)
        meta = path.lstat()
        return path, (meta.st_dev, meta.st_ino)

    def positive(self, function, *args, **kwargs):
        try:
            return function(*args, **kwargs)
        except (TypeError, files.InstanceError) as error:
            self.fail(f"declared owned live effect was refused: {error}")

    def publish(self, data, **kwargs):
        function = getattr(files, "publish_runtime", None)
        self.assertTrue(callable(function), "runtime publication API must exist")
        return function(self.root, "runtime/resources-owner.json", data, **kwargs)

    def test_live_socket_requires_matching_explicit_witness(self):
        path, identity = self.bind("runtime/herdr/socket")
        with self.assertRaises(files.InstanceError):
            files.validate_tree(self.root)
        self.positive(files.validate_tree, self.root, live_sockets={path: identity})
        with self.assertRaises(files.InstanceError):
            files.validate_tree(self.root, live_sockets={path: (identity[0], identity[1] + 1)})
        self.assertTrue(path.is_socket())

    def test_only_declared_pg_herdr_and_keeper_paths_are_allowed(self):
        paths = [self.bind(relative) for relative in
                 ("postgres/socket/.s.PGSQL.25432", ".s.PGSQL.25432", "runtime/herdr/socket",
                  "runtime/herdr/socket-client.sock", "runtime/keeper.sock")]
        witnesses = dict(paths)
        self.positive(files.validate_tree, self.root, live_sockets=witnesses)
        wrong, identity = self.bind("postgres/socket/.s.PGSQL.25433")
        with self.assertRaises(files.InstanceError):
            files.validate_tree(self.root, live_sockets=witnesses | {wrong: identity})
        wrong.unlink()
        arbitrary, identity = self.bind("runtime/arbitrary.sock")
        with self.assertRaises(files.InstanceError):
            files.validate_tree(self.root, live_sockets=witnesses | {arbitrary: identity})
        arbitrary.unlink()
        document = self.root / "manifest.json"
        changed = json.loads(document.read_text())
        changed["hosts"]["herdr"]["socket"] = str(self.root / "runtime/other.sock")
        document.write_text(json.dumps(changed))
        with self.assertRaises(files.InstanceError):
            files.validate_tree(self.root, live_sockets=witnesses)

    def test_socket_proof_types_foreign_paths_and_regular_files_are_rejected(self):
        path, identity = self.bind("runtime/keeper.sock")
        for invalid in [[], True, ()]:
            with self.subTest(mapping=invalid), self.assertRaises(files.InstanceError):
                files.validate_tree(self.root, live_sockets=invalid)
        for value in [(True, identity[1]), (-1, identity[1]), [*identity], (identity[0],), "socket"]:
            with self.subTest(value=value), self.assertRaises(files.InstanceError):
                files.validate_tree(self.root, live_sockets={path: value})
        with self.assertRaises(files.InstanceError):
            files.validate_tree(self.root, live_sockets={self.base / "outside.sock": identity})
        path.unlink()
        path.write_text("regular file")
        os.chmod(path, 0o600)
        metadata = path.stat()
        with self.assertRaises(files.InstanceError):
            files.validate_tree(self.root, live_sockets={path: (metadata.st_dev, metadata.st_ino)})

    def test_socket_replacement_private_ancestor_and_uid_are_checked(self):
        path, identity = self.bind("runtime/herdr/socket")
        path.unlink()
        _, replacement = self.bind("runtime/herdr/socket")
        self.assertNotEqual(identity, replacement)
        with self.assertRaises(files.InstanceError):
            files.validate_tree(self.root, live_sockets={path: identity})
        os.chmod(path.parent, 0o755)
        with self.assertRaises(files.InstanceError):
            files.validate_tree(self.root, live_sockets={path: replacement})
        os.chmod(path.parent, 0o700)
        with patch.object(files.os, "geteuid", return_value=os.geteuid() + 1):
            with self.assertRaises(files.InstanceError):
                files.validate_tree(self.root, live_sockets={path: replacement})

    def test_socket_symlink_never_authorizes_an_external_listener(self):
        outside = self.base / "outside"
        outside.mkdir(mode=0o700)
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.addCleanup(listener.close)
        external = outside / "socket"
        listener.bind(str(external))
        external_identity = external.stat()
        self.root.joinpath("runtime/herdr/socket").symlink_to(external)
        with self.assertRaises(files.InstanceError):
            files.validate_tree(self.root, live_sockets={self.root / "runtime/herdr/socket":
                                (external_identity.st_dev, external_identity.st_ino)})
        self.root.joinpath("runtime/herdr/socket").unlink()
        self.root.joinpath("runtime/herdr").rmdir()
        self.root.joinpath("runtime/herdr").symlink_to(outside, target_is_directory=True)
        with self.assertRaises(files.InstanceError):
            files.validate_tree(self.root, live_sockets={self.root / "runtime/herdr/socket":
                                (external_identity.st_dev, external_identity.st_ino)})
        self.assertTrue(external.is_socket())

    def test_manifest_live_validation_keeps_declared_schema_and_default_strictness(self):
        path, identity = self.bind("runtime/keeper.sock")
        with self.assertRaises(files.InstanceError):
            manifest.validate(self.base, self.instance_id, {}, check_ports=False)
        with patch.object(manifest, "worktree_identity", return_value=self.declaration["head"]):
            result = self.positive(manifest.validate, self.base, self.instance_id, {}, check_ports=False,
                                   live_sockets={path: identity})
        self.assertEqual(result, self.declaration)
        self.assertEqual(result["status"], "DECLARED")
        self.assertIs(result["live_ready"], False)

    def test_runtime_publication_is_fresh_private_bounded_and_conditionally_same_inode(self):
        identity = self.publish({"nonce": "first", "owned": []})
        path = self.root / "runtime/resources-owner.json"
        self.assertEqual(identity, (path.stat().st_dev, path.stat().st_ino))
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        with self.assertRaises(files.InstanceError):
            self.publish({"nonce": "unconditional overwrite"})
        self.assertEqual(self.publish({"nonce": "second"}, expected_identity=identity), identity)
        self.assertEqual(json.loads(path.read_text()), {"nonce": "second"})
        with self.assertRaises(files.InstanceError):
            self.publish({"value": "x" * 16384}, expected_identity=identity)
        self.assertEqual(json.loads(path.read_text()), {"nonce": "second"})
        with self.assertRaises(files.InstanceError):
            self.publish({"value": float("nan")}, expected_identity=identity)

    def test_runtime_publication_refuses_foreign_replacements_links_and_names(self):
        identity = self.publish({"owned": True})
        path = self.root / "runtime/resources-owner.json"
        path.unlink()
        path.write_text('{"foreign": true}\n')
        os.chmod(path, 0o600)
        with self.assertRaises(files.InstanceError):
            self.publish({"owned": True}, expected_identity=identity)
        self.assertEqual(json.loads(path.read_text()), {"foreign": True})
        current = (path.stat().st_dev, path.stat().st_ino)
        os.chmod(path, 0o644)
        with self.assertRaises(files.InstanceError):
            self.publish({"owned": True}, expected_identity=current)
        os.chmod(path, 0o600)
        alias = self.root / "runtime/hardlink"
        os.link(path, alias)
        with self.assertRaises(files.InstanceError):
            self.publish({"owned": True}, expected_identity=current)
        alias.unlink()
        path.unlink()
        outside = self.base / "outside.json"
        outside.write_text("foreign")
        path.symlink_to(outside)
        with self.assertRaises(files.InstanceError):
            self.publish({}, expected_identity=current)
        self.assertEqual(outside.read_text(), "foreign")
        path.unlink()
        for relative in ["receipt.json", "runtime/keeper-control.json", "runtime/other.json", "../escape"]:
            with self.subTest(relative=relative), self.assertRaises(files.InstanceError):
                files.publish_runtime(self.root, relative, {})

    def test_runtime_publication_supports_only_explicit_existing_live_sockets(self):
        path, identity = self.bind("runtime/keeper.sock")
        with self.assertRaises(files.InstanceError):
            self.publish({})
        result = self.publish({"alive": True}, live_sockets={path: identity})
        self.assertEqual(result[1], (self.root / "runtime/resources-owner.json").stat().st_ino)

    def test_publication_pins_original_fd_across_a_path_replacement(self):
        identity = self.publish({"owned": True})
        path = self.root / "runtime/resources-owner.json"
        truncate = os.ftruncate

        def replace_before_write(descriptor, size):
            path.unlink()
            path.write_text('{"foreign": true}\n')
            os.chmod(path, 0o600)
            truncate(descriptor, size)

        with patch.object(files.os, "ftruncate", side_effect=replace_before_write):
            with self.assertRaises(files.InstanceError):
                self.publish({"overwrite": True}, expected_identity=identity)
        self.assertEqual(json.loads(path.read_text()), {"foreign": True})

    def test_resource_spec_is_explicit_bounded_and_never_resolves_a_fallback(self):
        function = getattr(manifest, "resource_spec", None)
        self.assertTrue(callable(function), "resource spec API must exist")
        value = {"startup_timeout": 60, "stop_grace": 5, "max_aux": 2, "pg_pool": 4, "pg_reserve": 1}
        for key in ("postgres_bin", "initdb_bin", "psql_bin", "herdr_bin"):
            path = self.base / key
            path.write_text("#!/bin/sh\nexit 0\n")
            os.chmod(path, 0o700)
            value[key] = str(path)
        self.assertEqual(function(value), value)
        variants = [{**value, "extra": "unknown"}, {key: item for key, item in value.items() if key != "herdr_bin"}]
        for key, bad in [("postgres_bin", "postgres"), ("max_aux", 3), ("pg_pool", 5), ("pg_reserve", 0),
                         ("startup_timeout", True), ("startup_timeout", 601), ("startup_timeout", 10**1000),
                         ("startup_timeout", float("nan")), ("startup_timeout", float("inf")),
                         ("herdr_bin", "bad\0path"),
                         ("stop_grace", 0), ("stop_grace", 31)]:
            variants.append({**value, key: bad})
        for variant in variants:
            with self.subTest(variant=variant), self.assertRaises(files.InstanceError):
                function(variant)
        binary = Path(value["herdr_bin"])
        os.chmod(binary, 0o600)
        with self.assertRaises(files.InstanceError):
            function(value)
        os.chmod(binary, 0o700)
        alias = self.base / "binary-link"
        alias.symlink_to(binary)
        with self.assertRaises(files.InstanceError):
            function({**value, "herdr_bin": str(alias)})




if __name__ == "__main__":
    unittest.main()
