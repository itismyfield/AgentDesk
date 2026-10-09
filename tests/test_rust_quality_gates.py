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
python3 scripts/check_clippy_warning_count.py --input target/clippy-observation/diagnostics.jsonl --output target/clippy-observation/report.json --baseline scripts/clippy_warning_baseline.json --base-ref HEAD^1
"""
PR_RUN = RUN.replace("--base-ref HEAD^1", '--base-ref "$CLIPPY_BASE_REF"')


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

    def test_pr_existing_lint_enforces_same_ceiling(self):
        document = json.loads(subprocess.check_output(["ruby", "-ryaml", "-rjson", "-e", "puts JSON.generate(YAML.load_file(ARGV[0]))", str(ROOT / ".github/workflows/ci-pr.yml")]))
        job = document["jobs"]["lint"]
        lint = [s for s in job["steps"] if s.get("name") == "just lint"]
        self.assertEqual(lint, [{"name": "just lint", "shell": "bash", "run": PR_RUN, "env": {"CLIPPY_BASE_REF": "${{ github.event.pull_request.base.sha }}"}}])
        self.assertEqual(job["steps"][0], {"uses": "actions/checkout@v4", "with": {"fetch-depth": 0}})

    def test_main_step_executes_same_argv_and_preserves_failures(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "scripts").mkdir()
            shutil.copyfile(ROOT / "scripts/check_clippy_warning_count.py", root / "scripts/check_clippy_warning_count.py")
            shutil.copyfile(ROOT / "scripts/clippy_warning_baseline.json", root / "scripts/clippy_warning_baseline.json")
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            for value in ("parent", "head"):
                subprocess.run(["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.com", "commit", "--allow-empty", "-qm", value], cwd=root, check=True)
            (root / "bin").mkdir()
            cargo = root / "bin/cargo"
            cargo.write_text('#!/bin/bash\nif [[ "$*" == "clippy -V" ]]; then\n echo "clippy 0.1.94 (e408947bf 2026-03-25)"\n exit 0\nfi\nprintf "%s\\n" "$*" > argv\nprintf "%s\\n" "$PAYLOAD"\nexit "$RC"\n')
            cargo.chmod(0o755)
            rustc = root / "bin/rustc"
            rustc.write_text('#!/bin/bash\nprintf "%s" "$RUSTC_VERSION"\n')
            rustc.chmod(0o755)
            from tests.test_clippy_warning_count import RUSTC
            env = dict(os.environ, PATH=str(root / "bin") + ":" + os.environ["PATH"], RUSTC_VERSION=RUSTC, RUNNER_OS="Linux")
            from tests.test_clippy_warning_count import stream
            warning = {"reason": "compiler-message", "message": {"level": "warning"}}
            finished = {"reason": "build-finished", "success": True}
            for run in (RUN, PR_RUN):
                env["CLIPPY_BASE_REF"] = "HEAD^1"
                for payload, rc, expected in ((stream(*([warning] * 952), finished), 0, 0), (stream(*([warning] * 953), finished), 0, 1), ("garbage", 0, 1), ("garbage", 17, 17)):
                    with self.subTest(run=run, cargo_rc=rc, gate_rc=expected):
                        env.update(PAYLOAD=payload, RC=str(rc))
                        report = root / "target/clippy-observation/report.json"
                        report.unlink(missing_ok=True)
                        result = subprocess.run(["bash", "-e", "-c", run], cwd=root, env=env, capture_output=True, text=True)
                        self.assertEqual(result.returncode, expected, result.stderr)
                        self.assertEqual((root / "argv").read_text().strip(), "clippy --workspace --all-targets --all-features --message-format=json -- -W clippy::all")
                        if payload == "garbage" and rc == 0:
                            self.assertFalse(json.loads(report.read_text())["valid"])
                        if rc == 17:
                            self.assertFalse(report.exists())

    def test_configured_denies_are_documented(self):
        import tomllib
        lints = tomllib.loads((ROOT / "Cargo.toml").read_text())["lints"]["clippy"]
        doc = (ROOT / "docs/ci/rust-quality-gates.md").read_text()
        self.assertEqual(sum(v == "deny" for v in lints.values()), 8)
        for lint, level in lints.items():
            if level == "deny":
                self.assertIn("`" + lint + "`", doc)
