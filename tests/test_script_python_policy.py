from pathlib import Path
import os
import subprocess
import tempfile
import textwrap
import unittest


REPO_ROOT = Path(__file__).resolve().parents[1]


class ScriptPythonPolicyTests(unittest.TestCase):
    def test_ci_script_checks_declares_python_311_runtime(self) -> None:
        script = (REPO_ROOT / "scripts" / "ci-script-checks.sh").read_text(
            encoding="utf-8"
        )

        self.assertIn('PYTHON="${PYTHON:-python3}"', script)
        self.assertIn("AgentDesk script checks require Python 3.11+", script)
        self.assertIn("sys.version_info < (3, 11)", script)
        self.assertIn('"$PYTHON" scripts/check_hotfile_ratchet.py', script)
        self.assertIn("tests.test_script_python_policy", script)
        self.assertIn('"$PYTHON" scripts/audit_maintainability.py --check', script)

    def test_ci_script_checks_fails_before_body_on_unsupported_python(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            fake_python = Path(tmp) / "python3.10"
            fake_python.write_text(
                textwrap.dedent(
                    """\
                    #!/usr/bin/env bash
                    echo "ERROR: AgentDesk script checks require Python 3.11+; $0 is Python 3.10.0." >&2
                    echo "Set PYTHON=/path/to/python3.11+ or put python3.11+ first on PATH." >&2
                    exit 1
                    """
                ),
                encoding="utf-8",
            )
            fake_python.chmod(fake_python.stat().st_mode | 0o111)

            result = subprocess.run(
                ["bash", "scripts/ci-script-checks.sh"],
                cwd=REPO_ROOT,
                env={**os.environ, "PYTHON": str(fake_python)},
                text=True,
                capture_output=True,
                check=False,
            )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("require Python 3.11+", result.stderr)
        self.assertIn("Set PYTHON=/path/to/python3.11+", result.stderr)
        self.assertNotIn("=== PG audit guard ===", result.stdout)

    def test_hotfile_ratchet_guards_before_tomllib_import(self) -> None:
        source = (REPO_ROOT / "scripts" / "check_hotfile_ratchet.py").read_text(
            encoding="utf-8"
        )

        self.assertIn("MIN_PYTHON = (3, 11)", source)
        self.assertLess(
            source.index("sys.version_info < MIN_PYTHON"),
            source.index("import tomllib"),
        )
        self.assertIn("requires Python 3.11+", source)
        self.assertIn("for stdlib tomllib", source)

    def test_ci_script_check_jobs_pin_python_311(self) -> None:
        for rel in (
            ".github/workflows/ci-main.yml",
            ".github/workflows/ci-pr.yml",
            ".github/workflows/ci-nightly.yml",
        ):
            with self.subTest(workflow=rel):
                workflow = (REPO_ROOT / rel).read_text(encoding="utf-8")
                self.assertIn("Setup Python for script checks", workflow)
                self.assertIn('python-version: "3.11"', workflow)


if __name__ == "__main__":
    unittest.main()
