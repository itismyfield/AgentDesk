import copy
from pathlib import Path
import unittest
import json
import subprocess

ROOT = Path(__file__).resolve().parents[1]
STEP = {"name": "Production PR cap", "shell": "bash", "run": "bash scripts/pr_cap_check.sh", "env": {"PR_CAP_CI": "1", "PR_CAP_MODE": "${{ vars.PR_CAP_MODE || 'enforce' }}", "BASH_ENV": "/dev/null"}}


def validate(document):
    job = document["jobs"]["scripts"]
    steps = job["steps"]
    caps = [s for s in steps if s.get("name") == STEP["name"]]
    return caps == [STEP] and not any(k in job for k in ("if", "continue-on-error")) and steps.index(STEP) == next(i for i, s in enumerate(steps) if s.get("name") == "Run script checks") + 1


class CapWiringTest(unittest.TestCase):
    def setUp(self):
        self.document = json.loads(subprocess.check_output(["ruby", "-ryaml", "-rjson", "-e", "puts JSON.generate(YAML.load_file(ARGV[0]))", str(ROOT / ".github/workflows/ci-pr.yml")]))

    def test_clean_wiring(self):
        self.assertTrue(validate(self.document))

    def test_deleted_conditional_muted_and_reordered_fail(self):
        for key, value in (("if", "false"), ("continue-on-error", True), ("run", "true"), ("env", {})):
            document = copy.deepcopy(self.document)
            step = next(s for s in document["jobs"]["scripts"]["steps"] if s.get("name") == STEP["name"])
            step[key] = value
            self.assertFalse(validate(document))
        document = copy.deepcopy(self.document)
        steps = document["jobs"]["scripts"]["steps"]
        steps.remove(STEP)
        self.assertFalse(validate(document))
        steps.insert(0, STEP)
        self.assertFalse(validate(document))

    def test_interstitial_environment_writer_fails(self):
        document = copy.deepcopy(self.document)
        steps = document["jobs"]["scripts"]["steps"]
        steps.insert(steps.index(STEP), {"run": 'echo PR_CAP_CI=0 >> "$GITHUB_ENV"'})
        self.assertFalse(validate(document))
