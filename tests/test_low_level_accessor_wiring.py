"""The accessor ratchet reaches fatal PR and local static-check boundaries."""

from __future__ import annotations

import ast
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[1]
AGGREGATE = ROOT / "scripts/ci-script-checks.sh"
TEST_TITLE = "Low-level accessor ratchet tests"


def check_body(title: str) -> str:
    text = AGGREGATE.read_text()
    match = re.search(rf'^if run_check (\w+) "{re.escape(title)}"; then\n', text, re.M)
    if not match:
        raise AssertionError(f"unregistered script check: {title}")
    rest = text[match.end():]
    return rest[:rest.index("\nfi\n")]


class LowLevelAccessorWiringTests(unittest.TestCase):
    def test_audit_module_is_registered(self) -> None:
        tree = ast.parse((ROOT / "scripts/audit_maintainability.py").read_text())
        modules = next(ast.literal_eval(node.value) for node in tree.body
                       if isinstance(node, ast.Assign)
                       and any(isinstance(target, ast.Name) and target.id == "CHECK_MODULES"
                               for target in node.targets))
        self.assertIn("audit_maintainability.checks.low_level_accessors", modules)

    def test_fixture_suite_is_in_guards_and_failure_is_fatal(self) -> None:
        text = AGGREGATE.read_text()
        self.assertIn(f'if run_check guards "{TEST_TITLE}"; then', text)
        body = check_body(TEST_TITLE)
        modules = ("tests.test_low_level_accessors", "tests.test_low_level_accessor_baseline",
                   "tests.test_low_level_accessor_wiring")
        for module in modules:
            self.assertIn(f'"$PYTHON" -m unittest {module}', body)
            self.assert_fatal(body, ["-m", "unittest", module])

    def test_audit_check_mode_failure_is_fatal(self) -> None:
        body = check_body("Maintainability audit")
        self.assertIn('"$PYTHON" scripts/audit_maintainability.py --check', body)
        self.assert_fatal(body, ["scripts/audit_maintainability.py", "--check"])

    def assert_fatal(self, body: str, expected: list[str]) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            probe, journal = root / "python-probe", root / "argv"
            probe.write_text('#!/bin/bash\nprintf "%s\\n" "$@" > "$JOURNAL"\n'
                             '[ "${*: -1}" != "$FAIL_ON" ] || exit 23\n')
            probe.chmod(0o755)
            result = subprocess.run(["bash", "-c", "set -euo pipefail\n" + body], cwd=root,
                                    env={"PATH": os.environ["PATH"], "PYTHON": str(probe),
                                         "JOURNAL": str(journal), "FAIL_ON": expected[-1]},
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, 23, result.stderr)
            self.assertEqual(journal.read_text().splitlines(), expected)

    def test_required_pr_jobs_always_run_guard_and_audit_shards(self) -> None:
        jobs = yaml.safe_load((ROOT / ".github/workflows/ci-pr.yml").read_text())["jobs"]
        for job_name, shard in (("scripts", "cargo"), ("scripts_guards", "guards")):
            job = jobs[job_name]
            self.assertEqual(job["if"], "${{ !github.event.pull_request.draft && !cancelled() }}")
            step = next(row for row in job["steps"] if row.get("name") == "Run script checks")
            self.assertEqual(step["run"], "./scripts/ci-script-checks.sh")
            self.assertNotIn("if", step)
            self.assertEqual(step["env"]["SCRIPT_CHECK_SHARD"], shard)


if __name__ == "__main__":
    unittest.main()
