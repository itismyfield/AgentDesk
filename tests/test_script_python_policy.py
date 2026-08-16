from pathlib import Path
import os
import subprocess
import tempfile
import textwrap
import unittest


REPO_ROOT = Path(__file__).resolve().parents[1]


class ScriptPythonPolicyTests(unittest.TestCase):
    def run_ci_script_checks_with_python(
        self, python: str
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["bash", "scripts/ci-script-checks.sh"],
            cwd=REPO_ROOT,
            env={**os.environ, "PYTHON": python},
            text=True,
            capture_output=True,
            check=False,
        )

    def write_fake_python(self, directory: str, body: str) -> Path:
        fake_python = Path(directory) / "fake-python"
        fake_python.write_text(
            "#!/usr/bin/env bash\n" + textwrap.dedent(body),
            encoding="utf-8",
        )
        fake_python.chmod(fake_python.stat().st_mode | 0o111)
        return fake_python

    def test_ci_script_checks_declares_python_311_runtime(self) -> None:
        script = (REPO_ROOT / "scripts" / "ci-script-checks.sh").read_text(
            encoding="utf-8"
        )

        self.assertIn('PYTHON="${PYTHON:-python3}"', script)
        self.assertIn("AgentDesk script checks require Python 3.11+", script)
        self.assertIn("sys.version_info < (3, 11)", script)
        for invocation in ("stdin", "file", "module"):
            self.assertIn(f"agentdesk-python-probe:{invocation}", script)
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

            result = self.run_ci_script_checks_with_python(str(fake_python))

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("require Python 3.11+", result.stderr)
        self.assertIn("Set PYTHON=/path/to/python3.11+", result.stderr)
        self.assertNotIn("=== PG audit guard ===", result.stdout)

    def test_ci_script_checks_rejects_successful_noop_python_before_body(self) -> None:
        true_path = next(
            (path for path in ("/usr/bin/true", "/bin/true") if Path(path).is_file()),
            None,
        )
        self.assertIsNotNone(true_path)

        result = self.run_ci_script_checks_with_python(str(true_path))

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("identity check for the stdin (-) invocation", result.stderr)
        self.assertIn("Expected exact stdout marker", result.stderr)
        self.assertNotIn("=== PG audit guard ===", result.stdout)

    def test_ci_script_checks_checks_file_and_module_invocation_markers(self) -> None:
        wrappers = {
            "file": """\
                if [ "${1:-}" = "-" ]; then
                  cat >/dev/null
                  echo "agentdesk-python-probe:stdin"
                fi
                exit 0
            """,
            "-m module": """\
                case "${1:-}" in
                  -)
                    cat >/dev/null
                    echo "agentdesk-python-probe:stdin"
                    ;;
                  -m)
                    ;;
                  *)
                    echo "agentdesk-python-probe:file"
                    ;;
                esac
                exit 0
            """,
        }

        for expected_invocation, wrapper in wrappers.items():
            with self.subTest(invocation=expected_invocation):
                with tempfile.TemporaryDirectory() as tmp:
                    fake_python = self.write_fake_python(tmp, wrapper)
                    result = self.run_ci_script_checks_with_python(str(fake_python))

                self.assertNotEqual(result.returncode, 0)
                self.assertIn(
                    f"identity check for the {expected_invocation} invocation",
                    result.stderr,
                )
                self.assertIn("Expected exact stdout marker", result.stderr)
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
