"""Run the repository generator through CI's static check entry point."""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import unittest
from collections import Counter
from collections.abc import Mapping
from pathlib import Path

import yaml


ROOT = Path(__file__).resolve().parents[1]
RUNNER = ROOT / "scripts/ci-script-checks.sh"
GENERATOR = ROOT / "scripts/generate_user_feature_map.py"
GUARD_TITLE = "User feature map static check"
CONTRACT_TITLE = "User feature map generation contract"


def feature_map_base(env: Mapping[str, str]) -> str:
    base = env.get("TEST_LANE_BASELINE_REF") or env.get("GFP_BASE_SHA")
    if not base:
        if any(env.get(key) for key in ("CI", "GITHUB_ACTIONS", "GFP_EVENT_NAME", "GITHUB_EVENT_NAME")):
            raise ValueError("CI feature map check requires TEST_LANE_BASELINE_REF or GFP_BASE_SHA")
        base = "origin/main"
    done = subprocess.run(
        ["git", "rev-parse", "--verify", f"{base}^{{commit}}"], cwd=ROOT,
        capture_output=True, text=True, timeout=30,
    )
    if done.returncode or not re.fullmatch(r"[0-9a-f]{40}\n?", done.stdout):
        raise ValueError(f"feature map base commit unavailable: {base}")
    return base


def block(title: str, shard: str) -> str:
    source = RUNNER.read_text(encoding="utf-8")
    start = source.index(f'if run_check {shard} "{title}"; then\n')
    next_check = source.find('\nif run_check ', start + 1)
    return source[start:next_check if next_check >= 0 else len(source)]


class UserFeatureMapStatic(unittest.TestCase):
    def test_repository_generator_check_runs_and_inventory_is_nonempty(self) -> None:
        base = feature_map_base(os.environ)
        done = subprocess.run(
            [sys.executable, str(GENERATOR), "--check", "--base-ref", base], cwd=ROOT,
            capture_output=True, text=True, timeout=60,
        )
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        report = json.loads((ROOT / "docs/generated/user-feature-coverage.json").read_text())
        self.assertEqual(report["schema_version"], 1)
        self.assertGreater(report["applicable"], 0)
        self.assertEqual(report["covered"] + report["missing"], report["applicable"])
        self.assertTrue(report["by_feature"])
        rows = yaml.safe_load((ROOT / "docs/user-feature-map.rows.yaml").read_text())
        expected = {feature["feature_id"]: feature for feature in rows["features"]}
        self.assertEqual(set(report["by_feature"]), set(expected))
        for feature_id, summary in report["by_feature"].items():
            with self.subTest(feature=feature_id):
                self.assertIs(type(summary["needs_live"]), bool)
                self.assertEqual(summary["needs_live"], expected[feature_id].get("needs_live", True))
                names = summary["test_names"]
                self.assertIsInstance(names, list)
                self.assertTrue(all(isinstance(name, str) and name.strip() for name in names))
                self.assertEqual(len(names), len(set(names)))
                self.assertCountEqual(names, expected[feature_id].get("test_names", []))
                self.assertIsInstance(summary["scenario_references"], list)
                if all(entry["status"] == "unsupported" for entry in summary["tuples"]):
                    self.assertEqual(summary["covered"], 0)
        scenarios = ROOT / "tests/e2e/tui_relay/scenarios"
        active = [yaml.safe_load(path.read_text()) for path in sorted(scenarios.glob("*.yaml"))]
        counts = Counter(item["coverage_class"] for item in active)
        self.assertEqual(report["scenario_counts"], {
            "active": len(active), "live": counts["live"], "fixture": counts["fixture"],
            "unsupported-known-gap": counts["unsupported-known-gap"],
            "retired": len(list((scenarios / "retired").glob("*.yaml"))),
        })
        self.assertGreater(counts["live"], 0)
        print(f"feature-map static: applicable={report['applicable']} scenarios={dict(counts)}")

    def test_guards_single_line_is_extracted_by_native_static_precheck(self) -> None:
        extracted = subprocess.run([
            "awk", '/^if run_check /{g=($3=="guards")} g && /^"\\$PYTHON" -m unittest [A-Za-z_0-9.]+ *$/ {print $NF}',
            str(RUNNER),
        ], cwd=ROOT, capture_output=True, text=True, timeout=30)
        self.assertEqual(extracted.returncode, 0, extracted.stderr)
        self.assertEqual(extracted.stdout.splitlines().count("tests.test_user_feature_map_static"), 1)
        self.assertIn('"$PYTHON" -m unittest tests.test_user_feature_map_static\n',
                      block(GUARD_TITLE, "guards"))

    def test_contracts_register_generator_and_complete_red_suite(self) -> None:
        source = block(CONTRACT_TITLE, "contracts")
        self.assertIn('"$PYTHON" scripts/generate_user_feature_map.py --check --base-ref "$feature_map_base"', source)
        self.assertIn('"$PYTHON" -m unittest tests.test_generate_user_feature_map', source)
        self.assertIn("TEST_LANE_BASELINE_REF", source)
        self.assertIn("GFP_BASE_SHA", source)
        self.assertIn("origin/main", source)
        self.assertIn("exit 1", source)

    def test_ci_jobs_select_registered_shards_and_supply_explicit_base(self) -> None:
        jobs = yaml.safe_load((ROOT / ".github/workflows/ci-pr.yml").read_text())["jobs"]
        for job_id, shard, title in (("scripts_guards", "guards", GUARD_TITLE),
                                     ("scripts_contracts", "contracts", CONTRACT_TITLE)):
            with self.subTest(job=job_id):
                job = jobs[job_id]
                run = next(step for step in job["steps"] if step.get("name") == "Run script checks")
                self.assertEqual(run["run"], "./scripts/ci-script-checks.sh")
                self.assertEqual(run["env"]["SCRIPT_CHECK_SHARD"], shard)
                self.assertTrue(run["env"].get("TEST_LANE_BASELINE_REF") or run["env"].get("GFP_BASE_SHA"))
                self.assertIn(job_id, jobs["required_contexts"]["needs"])
                done = subprocess.run(["bash", str(RUNNER), "--shard", shard], cwd=ROOT,
                                      env={**os.environ, "PYTHON": sys.executable, "SCRIPT_CHECK_LIST": "1"},
                                      capture_output=True, text=True, timeout=60)
                self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
                self.assertEqual(done.stdout.splitlines().count(f"{shard}\t{title}"), 1)
                self.assertTrue(any("pyyaml" in str(step.get("run", "")) for step in job["steps"]))

    def test_base_resolution_is_explicit_in_ci_and_checked_locally(self) -> None:
        self.assertEqual(feature_map_base({}), "origin/main")
        self.assertEqual(feature_map_base({"GFP_BASE_SHA": "origin/main", "CI": "true"}), "origin/main")
        self.assertEqual(feature_map_base({"TEST_LANE_BASELINE_REF": "HEAD", "GFP_BASE_SHA": "missing"}), "HEAD")
        for marker in ("CI", "GITHUB_ACTIONS", "GFP_EVENT_NAME", "GITHUB_EVENT_NAME"):
            with self.subTest(marker=marker), self.assertRaisesRegex(ValueError, "requires"):
                feature_map_base({marker: "true"})
        with self.assertRaisesRegex(ValueError, "unavailable"):
            feature_map_base({"TEST_LANE_BASELINE_REF": "refs/heads/feature-map-nonexistent-base"})

    def test_generator_rejects_unavailable_base_instead_of_skipping(self) -> None:
        done = subprocess.run(
            [sys.executable, str(GENERATOR), "--check", "--base-ref", "refs/heads/feature-map-nonexistent-base"],
            cwd=ROOT, capture_output=True, text=True, timeout=60,
        )
        self.assertNotEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn("base", (done.stdout + done.stderr).lower())


if __name__ == "__main__":
    unittest.main()
