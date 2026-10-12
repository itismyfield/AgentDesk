import hashlib
import importlib.util
import json
import os
import shlex
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


class PostgresMigrationChecksumGuardGitTest(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp_dir.cleanup)
        self.root = Path(self.temp_dir.name) / "repo"
        self.root.mkdir()
        self.migrations_dir = self.root / "migrations" / "postgres"
        self.migrations_dir.mkdir(parents=True)
        self.migration = self.migrations_dir / "0001_initial_schema.sql"
        self.original = b"CREATE TABLE example (id BIGINT PRIMARY KEY);\n"
        self.changed = b"CREATE TABLE example (id BIGINT PRIMARY KEY, name TEXT);\n"
        self.migration.write_bytes(self.original)
        self.manifest = self.migrations_dir / "immutable-checksums.json"
        self.allowlist = self.migrations_dir / "checksum-repair-allowlist.json"
        self.allowlist.write_text('{"version": 1, "repairs": []}\n', encoding="utf-8")
        self.write_manifest()
        self.git("init", "-b", "main")
        self.git("config", "user.name", "Migration guard test")
        self.git("config", "user.email", "migration-guard@example.invalid")
        self.commit("Initial protected migration")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.base_commit = self.git("rev-parse", "HEAD")
        self.git("switch", "-c", "feature")
        (self.root / "feature.txt").write_text("PR work\n", encoding="utf-8")
        self.commit("Branch PR before main advances")

    def git(self, *args, root=None):
        result = subprocess.run(
            [
                "git",
                "-c",
                "commit.gpgsign=false",
                "-c",
                f"core.hooksPath={os.devnull}",
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.safecrlf=false",
                "-C",
                str(root or self.root),
                *args,
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        return result.stdout.strip()

    def commit(self, message):
        self.git("add", "--all")
        self.git("commit", "-m", message)

    def write_manifest(self):
        entries = [
            {
                "path": path.relative_to(self.root).as_posix(),
                "version": int(path.name[:4]),
                "sha256": sha256(path.read_bytes()),
            }
            for path in sorted(self.migrations_dir.glob("[0-9][0-9][0-9][0-9]_*.sql"))
        ]
        self.manifest.write_text(
            json.dumps({"version": 1, "protected_migrations": entries}, indent=2) + "\n",
            encoding="utf-8",
        )

    def advance_main(self):
        self.git("switch", "main")
        (self.migrations_dir / "0002_add_example_name.sql").write_bytes(
            b"ALTER TABLE example ADD COLUMN name TEXT;\n"
        )
        self.write_manifest()
        self.commit("Main adds a migration after the PR branches")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.git("switch", "feature")

    def run_guard(self, *, root=None, base_ref=None, require_base=False):
        env = os.environ.copy()
        env.pop("AGENTDESK_MIGRATION_GUARD_BASE_REF", None)
        if base_ref is not None:
            env["AGENTDESK_MIGRATION_GUARD_BASE_REF"] = base_ref
        command = [sys.executable, str(SCRIPT_PATH), "--root", str(root or self.root)]
        if require_base:
            command.append("--require-base")
        return subprocess.run(
            command,
            env=env,
            check=False,
            capture_output=True,
            text=True,
        )

    def test_old_pr_passes_after_main_adds_new_migration(self):
        self.advance_main()

        result = self.run_guard()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("1 migrations protected", result.stdout)

    def test_pr_deleting_existing_migration_and_manifest_entry_fails(self):
        self.advance_main()
        self.migration.unlink()
        self.write_manifest()
        self.commit("PR deletes its existing protected migration")

        result = self.run_guard()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("protected migration from base manifest was deleted", result.stderr)
        self.assertIn("0001_initial_schema.sql", result.stderr)

    def test_pr_editing_existing_migration_and_regenerating_manifest_fails(self):
        self.advance_main()
        self.migration.write_bytes(self.changed)
        self.write_manifest()
        self.commit("PR tries to bless its migration edit")

        result = self.run_guard()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("immutable manifest entry changed relative to base", result.stderr)
        self.assertIn(sha256(self.original), result.stderr)
        self.assertIn(sha256(self.changed), result.stderr)

    def test_new_migration_added_on_both_branches_with_different_checksum_fails(self):
        self.advance_main()
        changed = b"ALTER TABLE example ADD COLUMN name VARCHAR(100);\n"
        (self.migrations_dir / "0002_add_example_name.sql").write_bytes(changed)
        self.write_manifest()
        self.commit("PR independently adds the same migration path with other contents")

        result = self.run_guard()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("immutable manifest entry changed relative to base", result.stderr)
        self.assertIn("0002_add_example_name.sql", result.stderr)
        self.assertIn(sha256(changed), result.stderr)

    def test_shallow_history_without_merge_base_fails_closed(self):
        self.advance_main()
        clone = Path(self.temp_dir.name) / "shallow-clone"
        self.git(
            "clone", "--depth=1", "--branch", "feature", self.root.as_uri(), str(clone)
        )
        self.git(
            "fetch", "--depth=1", "origin", "main:refs/remotes/origin/main", root=clone
        )
        self.assertEqual(self.git("rev-parse", "--is-shallow-repository", root=clone), "true")
        merge_base = subprocess.run(
            ["git", "-C", str(clone), "merge-base", "HEAD", "origin/main"],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(merge_base.returncode, 0)

        result = self.run_guard(root=clone)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("merge-base", result.stderr)

    def single_ref_shallow_checkout(self):
        checkout = Path(self.temp_dir.name) / "single-ref-checkout"
        checkout.mkdir()
        self.git("init", root=checkout)
        self.git(
            "fetch",
            "--no-tags",
            "--depth=1",
            self.root.as_uri(),
            "+feature:refs/remotes/pull/1/merge",
            root=checkout,
        )
        self.git("checkout", "--detach", "refs/remotes/pull/1/merge", root=checkout)
        self.assertEqual(
            self.git("rev-parse", "--is-shallow-repository", root=checkout), "true"
        )
        self.assertEqual(
            self.git("for-each-ref", "--format=%(refname)", root=checkout),
            "refs/remotes/pull/1/merge",
        )
        return checkout

    def test_single_ref_shallow_checkout_without_base_ref_warns_like_release(self):
        checkout = self.single_ref_shallow_checkout()

        result = self.run_guard(root=checkout)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            "WARNING: origin/main:migrations/postgres/immutable-checksums.json "
            "unavailable; enforcing current checksum manifest only",
            result.stderr,
        )
        self.assertIn("1 migrations protected", result.stdout)

    def test_single_ref_shallow_checkout_without_base_ref_fails_when_required(self):
        checkout = self.single_ref_shallow_checkout()

        result = self.run_guard(root=checkout, require_base=True)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("origin/main does not resolve to a commit", result.stderr)
        self.assertNotIn("enforcing current checksum manifest only", result.stderr)
        self.assertNotIn("guard passed", result.stdout)

    def test_single_ref_shallow_release_fallback_still_rejects_checksum_drift(self):
        checkout = self.single_ref_shallow_checkout()
        (checkout / "migrations/postgres/0001_initial_schema.sql").write_bytes(self.changed)

        result = self.run_guard(root=checkout)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("enforcing current checksum manifest only", result.stderr)
        self.assertIn("checksum drift without repair allowlist", result.stderr)
        self.assertNotIn("guard passed", result.stdout)

    def test_base_tip_equal_to_merge_base_passes(self):
        self.assertEqual(self.git("merge-base", "HEAD", "origin/main"), self.base_commit)
        self.assertEqual(self.git("rev-parse", "origin/main"), self.base_commit)

        result = self.run_guard()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("1 migrations protected", result.stdout)

    def test_base_tip_equal_to_merge_base_still_rejects_checksum_drift(self):
        self.migration.write_bytes(self.changed)
        self.commit("PR changes a protected migration")

        result = self.run_guard()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum drift without repair allowlist", result.stderr)
        self.assertIn("existing migration changed relative to base", result.stderr)

    def test_env_base_ref_override_uses_its_merge_base(self):
        self.advance_main()
        self.git("update-ref", "refs/remotes/upstream/trunk", "origin/main")
        self.git("update-ref", "-d", "refs/remotes/origin/main")

        result = self.run_guard(base_ref="upstream/trunk")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("1 migrations protected", result.stdout)

    def test_git_baseline_preserves_repair_allowlist(self):
        self.advance_main()
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
                            "reason": "fresh-install schema needs an explicit checksum repair",
                            "covered_by_migration": "0002_add_example_name.sql",
                            "repair_doc": "docs/postgres-migration-checksum-repair.md",
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )
        self.commit("PR allows a documented checksum repair")

        result = self.run_guard()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("base drift repair allowlisted by #2919", result.stdout)

    def test_missing_base_ref_fails_closed(self):
        result = self.run_guard(base_ref="missing/base", require_base=True)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing/base does not resolve to a commit", result.stderr)
        self.assertNotIn("guard passed", result.stdout)

    def test_missing_base_manifest_fails_closed(self):
        self.git("switch", "main")
        self.manifest.unlink()
        self.commit("Main has no readable checksum baseline")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.git("switch", "feature")

        result = self.run_guard()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cannot verify the migration baseline", result.stderr)
        self.assertNotIn("guard passed", result.stdout)


class PostgresMigrationChecksumGuardInvocationTest(unittest.TestCase):
    def test_ci_guards_require_git_baseline(self):
        script = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(encoding="utf-8")
        marker = 'if run_check guards "Postgres migration checksum guard"; then\n'
        self.assertIn(marker, script)
        block = script.split(marker, 1)[1].split("\nfi", 1)[0]
        invocations = [
            shlex.split(line)
            for line in block.splitlines()
            if "scripts/check_postgres_migration_checksums.py" in line
        ]
        self.assertEqual(
            invocations,
            [["$PYTHON", "scripts/check_postgres_migration_checksums.py", "--require-base"]],
        )


if __name__ == "__main__":
    unittest.main()
