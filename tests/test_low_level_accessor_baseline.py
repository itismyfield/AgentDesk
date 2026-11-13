"""Official baseline updates cannot purchase new accessor sites."""
from __future__ import annotations

import importlib
import json
import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT / "scripts") not in sys.path:
    sys.path.insert(0, str(ROOT / "scripts"))
SCRIPT = ROOT / "scripts/audit_maintainability/checks/low_level_accessor_baseline.py"
BASELINE = Path("scripts/audit_maintainability/baselines/generated/low_level_accessors")


class LowLevelAccessorBaselineTest(unittest.TestCase):
    def setUp(self):
        try:
            self.generator = importlib.import_module("audit_maintainability.checks.low_level_accessor_baseline")
        except ModuleNotFoundError as exc:
            if exc.name != "audit_maintainability.checks.low_level_accessor_baseline":
                raise
            self.fail("official accessor baseline generator is not implemented")
        self.check = importlib.import_module("audit_maintainability.checks.low_level_accessors")
        self.temp = TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name).resolve()
        self.write("src/lib.rs", "mod consumer;\n")
        self.write("src/consumer.rs", "fn old() { herdr_endpoint(channel); }\n")

    def write(self, name, text):
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    def command(self, mode):
        return subprocess.run([sys.executable, str(SCRIPT), "--repo", str(self.repo), mode],
                              cwd=ROOT, text=True, capture_output=True, check=False)

    def initialize(self):
        result = self.command("--initialize")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def baseline_bytes(self):
        return {path.relative_to(self.repo).as_posix(): path.read_bytes()
                for path in sorted((self.repo / BASELINE).glob("*.json"))}

    def assert_rejected_without_write(self, mode):
        before = self.baseline_bytes()
        result = self.command(mode)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.baseline_bytes(), before)
        return result

    def git_baseline(self):
        rows = self.check.scan(self.repo)
        self.generator.write_baselines(self.repo, rows)
        def git(*args):
            return subprocess.check_output(["git", "-C", str(self.repo), *args], text=True).strip()
        git("init", "-q")
        git("config", "user.name", "Accessor Fixture")
        git("config", "user.email", "fixture@example.invalid")
        git("config", "commit.gpgsign", "false")
        git("add", "src", "scripts")
        git("commit", "-qm", "approved fixture baseline")
        base = git("rev-parse", "HEAD")
        git("update-ref", "refs/remotes/origin/main", base)
        return base, rows

    def test_hand_edited_approval_and_source_growth_fail_cli_history_check(self):
        base, _ = self.git_baseline()
        self.write("src/consumer.rs", """
            fn old() { herdr_endpoint(channel); }
            fn added() { herdr_endpoint(other); }
        """)
        self.generator.write_baselines(self.repo, self.check.scan(self.repo))
        for configured_base in (None, base):
            with self.subTest(configured_base=configured_base), mock.patch.dict(os.environ):
                os.environ.pop("GFP_BASE_SHA", None)
                if configured_base:
                    os.environ["GFP_BASE_SHA"] = configured_base
                result = self.assert_rejected_without_write("--check-baseline")
                self.assertIn("approval baseline grew against PR base", result.stderr)

    def test_history_allows_reduction_and_same_hash_move_but_rejects_hash_or_count_growth(self):
        base, rows = self.git_baseline()
        self.assertFalse(self.generator.baseline_growth(self.repo, [], base_ref=base))
        moved = [{**row, "path": "src/moved.rs", "enclosing_fn": "moved"} for row in rows]
        self.assertFalse(self.generator.baseline_growth(self.repo, moved, base_ref=base))
        for changes in ({"normalized_hash": "f" * 64}, {"count": rows[0]["count"] + 1}):
            with self.subTest(changes=changes):
                candidate = [{**rows[0], **changes}, *rows[1:]]
                self.assertTrue(self.generator.baseline_growth(self.repo, candidate, base_ref=base))

    def test_initialize_generates_every_rule_and_loads_exact_sites(self):
        self.initialize()
        snapshots = self.baseline_bytes()
        self.assertEqual({Path(path).stem for path in snapshots}, set(self.check.RULES))
        for encoded in snapshots.values():
            record = json.loads(encoded)
            self.assertEqual(record["schema_version"], 1)
            rule = self.check.RULES[record["rule"]]
            self.assertEqual(record["accessor"], rule["accessor"])
            self.assertEqual(record["owned_paths"], list(rule["owned_paths"]))
            self.assertIsInstance(record["sites"], list)
        self.assertEqual(self.generator.load_baselines(self.repo), self.check.scan(self.repo))

    def test_write_baselines_roundtrip(self):
        rows = self.check.scan(self.repo)
        self.generator.write_baselines(self.repo, rows)
        self.assertEqual(self.generator.load_baselines(self.repo), rows)

    def test_initialize_refuses_to_overwrite_approved_baselines(self):
        self.initialize()
        self.assert_rejected_without_write("--initialize")

    def test_check_baseline_accepts_current_rows_and_ignores_line_only_drift(self):
        self.initialize()
        before = self.baseline_bytes()
        self.write("src/consumer.rs", "\n// source moved down\nfn old() { herdr_endpoint(channel); }\n")
        result = self.command("--check-baseline")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.baseline_bytes(), before)

    def test_check_baseline_rejects_stale_deleted_site_until_reduced(self):
        self.initialize()
        self.write("src/consumer.rs", "fn old() { common_policy(channel); }\n")
        self.assert_rejected_without_write("--check-baseline")
        result = self.command("--reduce")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.generator.load_baselines(self.repo), [])
        self.assertEqual(self.command("--check-baseline").returncode, 0)

    def test_reduce_accepts_count_decrease(self):
        self.write("src/consumer.rs", "fn old() { herdr_endpoint(channel); herdr_endpoint(channel); }\n")
        self.initialize()
        self.write("src/consumer.rs", "fn old() { herdr_endpoint(channel); }\n")
        result = self.command("--reduce")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(sum(row["count"] for row in self.generator.load_baselines(self.repo)), 1)

    def test_reduce_rejects_new_site_changed_hash_and_copy_without_writing(self):
        self.initialize()
        for source in ("fn added() { herdr_endpoint(channel); }",
                       "fn old() { herdr_endpoint(other); }",
                       "fn old() { herdr_endpoint(channel); herdr_endpoint(channel); }"):
            with self.subTest(source=source):
                self.write("src/consumer.rs", source)
                self.assert_rejected_without_write("--reduce")

    def test_relabel_moves_path_and_function_and_prints_old_to_new_mapping(self):
        self.initialize()
        self.write("src/lib.rs", "mod relocated;\n")
        (self.repo / "src/consumer.rs").rename(self.repo / "src/relocated.rs")
        self.write("src/relocated.rs", "fn renamed() { herdr_endpoint(channel); }\n")
        self.assert_rejected_without_write("--reduce")
        result = self.command("--relabel")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("src/consumer.rs", result.stdout)
        self.assertIn("src/relocated.rs", result.stdout)
        self.assertIn("old", result.stdout)
        self.assertIn("renamed", result.stdout)
        self.assertEqual(self.generator.load_baselines(self.repo), self.check.scan(self.repo))

    def test_relabel_rejects_hash_change_and_count_growth_without_writing(self):
        self.initialize()
        for source in ("fn moved() { herdr_endpoint(other); }",
                       "fn moved() { herdr_endpoint(channel); herdr_endpoint(channel); }"):
            with self.subTest(source=source):
                self.write("src/consumer.rs", source)
                self.assert_rejected_without_write("--relabel")

    def test_relabel_rejects_equal_total_hash_multiplicity_exchange(self):
        self.write("src/consumer.rs", "fn old() { herdr_endpoint(a); herdr_endpoint(a); herdr_endpoint(b); }")
        self.initialize()
        self.write("src/consumer.rs", "fn moved() { herdr_endpoint(a); herdr_endpoint(b); herdr_endpoint(b); }")
        self.assert_rejected_without_write("--relabel")

    def test_missing_baseline_file_fails_closed(self):
        self.initialize()
        path = next((self.repo / BASELINE).glob("*.json"))
        path.unlink()
        self.assert_rejected_without_write("--check-baseline")
        self.assert_rejected_without_write("--reduce")

    def test_invalid_json_or_schema_fails_closed(self):
        self.initialize()
        path = self.repo / BASELINE / "herdr_endpoint.json"
        original = path.read_text()
        for text in ("not valid JSON", json.dumps({**json.loads(original), "schema_version": 2})):
            with self.subTest(text=text):
                path.write_text(text, encoding="utf-8")
                self.assert_rejected_without_write("--check-baseline")
                self.assert_rejected_without_write("--relabel")


if __name__ == "__main__":
    unittest.main()
