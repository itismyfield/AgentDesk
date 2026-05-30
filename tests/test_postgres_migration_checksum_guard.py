import hashlib
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / "scripts" / "check_postgres_migration_checksums.py"


def load_guard_module():
    spec = importlib.util.spec_from_file_location("migration_checksum_guard", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class PostgresMigrationChecksumGuardTest(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.root = Path(self.temp_dir.name)
        self.migrations_dir = self.root / "migrations" / "postgres"
        self.migrations_dir.mkdir(parents=True)
        self.migration = self.migrations_dir / "0001_initial_schema.sql"
        self.original = b"CREATE TABLE example (id BIGINT PRIMARY KEY);\n"
        self.changed = b"CREATE TABLE example (id BIGINT PRIMARY KEY, name TEXT);\n"
        self.migration.write_bytes(self.original)
        self.manifest = self.migrations_dir / "immutable-checksums.json"
        self.allowlist = self.migrations_dir / "checksum-repair-allowlist.json"
        self.allowlist.write_text('{"version": 1, "repairs": []}\n', encoding="utf-8")
        self.guard = load_guard_module()
        payload = self.guard.manifest_payload(self.guard.find_migrations(self.root))
        self.manifest.write_text(json.dumps(payload, indent=2), encoding="utf-8")

    def tearDown(self):
        self.temp_dir.cleanup()

    def run_guard(self, *extra_args):
        return subprocess.run(
            [
                sys.executable,
                str(SCRIPT_PATH),
                "--root",
                str(self.root),
                "--manifest",
                str(self.manifest),
                "--allowlist",
                str(self.allowlist),
                "--base-ref",
                "",
                *extra_args,
            ],
            check=False,
            capture_output=True,
            text=True,
        )

    def test_fails_when_protected_migration_checksum_drifts_without_allowlist(self):
        self.migration.write_bytes(self.changed)

        result = self.run_guard()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum drift without repair allowlist", result.stderr)
        self.assertIn("0001_initial_schema.sql", result.stderr)
        self.assertIn(sha256(self.original), result.stderr)
        self.assertIn(sha256(self.changed), result.stderr)

    def test_allows_drift_only_when_repair_allowlist_matches_old_and_new_checksum(self):
        self.migration.write_bytes(self.changed)
        self.allowlist.write_text(
            json.dumps(
                {
                    "version": 1,
                    "repairs": [
                        {
                            "path": "migrations/postgres/0001_initial_schema.sql",
                            "old_sha256": sha256(self.original),
                            "new_sha256": sha256(self.changed),
                            "issue": "#2919",
                            "reason": "fresh-install schema was edited after live DBs applied version 1",
                            "covered_by_migration": "0002_add_example_name.sql",
                            "repair_doc": "docs/postgres-migration-checksum-repair.md",
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )

        result = self.run_guard()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ALLOWLISTED", result.stdout)

    def test_base_manifest_prevents_blessing_existing_file_edit_by_updating_lockfile(self):
        base_manifest = self.root / "base-immutable-checksums.json"
        base_manifest.write_text(self.manifest.read_text(encoding="utf-8"), encoding="utf-8")
        self.migration.write_bytes(self.changed)
        updated_payload = self.guard.manifest_payload(self.guard.find_migrations(self.root))
        self.manifest.write_text(json.dumps(updated_payload, indent=2), encoding="utf-8")

        result = self.run_guard("--base-manifest", str(base_manifest))

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("immutable manifest entry changed relative to base", result.stderr)
        self.assertIn(sha256(self.original), result.stderr)
        self.assertIn(sha256(self.changed), result.stderr)


if __name__ == "__main__":
    unittest.main()
