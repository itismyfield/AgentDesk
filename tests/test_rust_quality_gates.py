import copy
from pathlib import Path
import unittest
import json
import subprocess
import os
import tempfile
import shutil

ROOT = Path(__file__).resolve().parents[1]
RUN = """set -o pipefail
mkdir -p target/clippy-observation
cargo clippy --workspace --all-targets --all-features --message-format=json -- -W clippy::all | tee target/clippy-observation/diagnostics.jsonl
if ! python3 scripts/check_clippy_warning_count.py --input target/clippy-observation/diagnostics.jsonl --output target/clippy-observation/report.json; then
  echo '::warning::Clippy observation invalid; no warning baseline can be derived'
fi
"""


def valid(document):
    job = document["jobs"]["lint"]
    steps = job["steps"]
    lint = [s for s in steps if s.get("name") == "just lint"]
    upload = [s for s in steps if s.get("name") == "Upload Clippy observation"]
    return lint == [{"name": "just lint", "shell": "bash", "run": RUN}] and upload == [{"name": "Upload Clippy observation", "uses": "actions/upload-artifact@v4", "with": {"name": "clippy-observation-${{ github.sha }}", "path": "target/clippy-observation/", "if-no-files-found": "error"}}] and not any(k in job for k in ("if", "continue-on-error"))


class QualityWiringTest(unittest.TestCase):
    def setUp(self):
        self.document = json.loads(subprocess.check_output(["ruby", "-ryaml", "-rjson", "-e", "puts JSON.generate(YAML.load_file(ARGV[0]))", str(ROOT / ".github/workflows/ci-main.yml")]))

    def test_clean(self):
        self.assertTrue(valid(self.document))
        aggregate = (ROOT / "scripts/ci-script-checks.sh").read_text()
        self.assertIn('"$PYTHON" scripts/check_dead_code_allow_ratchet.py', aggregate)

    def test_weakening_observation_fails(self):
        for replacement in ("true", RUN.replace("set -o pipefail", "true"), RUN.replace("--all-targets", ""), RUN.replace("-- -W clippy::all", "-- -A warnings")):
            document = copy.deepcopy(self.document)
            next(s for s in document["jobs"]["lint"]["steps"] if s.get("name") == "just lint")["run"] = replacement
            self.assertFalse(valid(document))

    def test_strict_not_required(self):
        for filename in ("ci-pr.yml", "ci-main.yml"):
            self.assertNotIn("run: just lint-strict", (ROOT / ".github/workflows" / filename).read_text())

    def test_main_step_executes_same_argv_and_preserves_failures(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "scripts").mkdir()
            shutil.copyfile(ROOT / "scripts/check_clippy_warning_count.py", root / "scripts/check_clippy_warning_count.py")
            (root / "bin").mkdir()
            cargo = root / "bin/cargo"
            cargo.write_text('#!/bin/bash\nprintf "%s\\n" "$*" > argv\nprintf "%s\\n" "$PAYLOAD"\nexit "$RC"\n')
            cargo.chmod(0o755)
            env = dict(os.environ, PATH=str(root / "bin") + ":" + os.environ["PATH"])
            for payload, rc in ((json.dumps({"reason": "build-finished", "success": True}), 0), ("garbage", 0), ("garbage", 17)):
                env.update(PAYLOAD=payload, RC=str(rc))
                result = subprocess.run(["bash", "-e", "-c", RUN], cwd=root, env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, rc, result.stderr)
                self.assertEqual((root / "argv").read_text().strip(), "clippy --workspace --all-targets --all-features --message-format=json -- -W clippy::all")
                if payload == "garbage" and rc == 0:
                    self.assertFalse(json.loads((root / "target/clippy-observation/report.json").read_text())["valid"])

    def test_configured_denies_are_documented(self):
        import tomllib
        lints = tomllib.loads((ROOT / "Cargo.toml").read_text())["lints"]["clippy"]
        doc = (ROOT / "docs/ci/rust-quality-gates.md").read_text()
        self.assertEqual(sum(v == "deny" for v in lints.values()), 8)
        for lint, level in lints.items():
            if level == "deny":
                self.assertIn("`" + lint + "`", doc)
