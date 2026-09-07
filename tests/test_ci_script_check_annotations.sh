#!/usr/bin/env bash
# #5757: execute the real preflight/reporting prefix in isolated fixtures only.
# The existing tests/*.sh CI loop discovers this suite automatically.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
"${PYTHON:-python3}" - "$SCRIPT_DIR/../scripts/ci-script-checks.sh" <<'PY'
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile
import unittest


source = Path(os.environ.get("CI_SCRIPT_CHECKS_TEST_SOURCE", sys.argv[1])).read_text()
# Cut before the first real check; never run the aggregate, even on a red tree.
prefix, separator, _ = source.partition(
    '\nif command -v shellcheck >/dev/null 2>&1; then\n'
)
if not separator:
    raise SystemExit("could not isolate the script-checks reporting prefix")


def decode(value, properties=False):
    # GitHub runner ActionCommand.TryParseV2: split before unescaping and decode
    # '%' last, so literal '%0A' is not accidentally converted to a newline.
    mappings = [("%0D", "\r"), ("%0A", "\n")]
    if properties:
        mappings += [("%3A", ":"), ("%2C", ",")]
    for encoded, plain in mappings + [("%25", "%")]:
        value = value.replace(encoded, plain)
    return value


class ScriptCheckAnnotations(unittest.TestCase):
    def run_fixture(self, body, summary="file", initial=""):
        with tempfile.TemporaryDirectory(prefix="script-check-annotations-") as tmp:
            root = Path(tmp)
            summary_path = root / "summary.md"
            env = {**os.environ, "PYTHON": sys.executable, "TMPDIR": tmp}
            # Never append expected failures to the enclosing CI step's summary.
            env.pop("GITHUB_STEP_SUMMARY", None)
            if summary == "file":
                env["GITHUB_STEP_SUMMARY"] = str(summary_path)
                if initial:
                    summary_path.write_text(initial)
            elif summary == "unwritable":
                env["GITHUB_STEP_SUMMARY"] = str(root)  # EISDIR even as root.
            elif summary == "empty":
                env["GITHUB_STEP_SUMMARY"] = ""
            result = subprocess.run(
                ["bash", "-c", prefix + "\ndeclare -F banner >/dev/null || exit 98\n" + body],
                cwd=root, env=env,
                capture_output=True, text=True, timeout=15,
            )
            summary_text = summary_path.read_text() if summary_path.exists() else None
        # Negative fixture workflow commands stay captured, out of the CI log.
        return result, summary_text

    def assert_failure(self, result, name, status):
        self.assertEqual(result.returncode, status, result.stderr)
        commands = [line for line in result.stdout.splitlines() if line.startswith("::")]
        self.assertEqual(len(commands), 1, repr(result.stdout))
        command, message = commands[0][2:].split("::", 1)
        kind, raw_properties = command.split(" ", 1)
        self.assertEqual(kind, "error")
        properties = dict(item.split("=", 1) for item in raw_properties.split(","))
        self.assertEqual(set(properties), {"title"})
        self.assertEqual(decode(properties["title"], properties=True), name)
        self.assertEqual(decode(message), f"FAIL: {name}")
        self.assertNotIn("UNREACHABLE", result.stdout)

    def test_all_check_banners_use_helper(self):
        self.assertIsNone(re.search(r'(?m)^\s*echo [\'"]=== ', source))
        self.assertIsNotNone(re.search(r'(?m)^\s*banner "PG audit guard"$', source))
        self.assertIsNotNone(re.search(r'(?m)^\s*banner "Maintainability audit"$', source))

    def test_current_banner_and_success_output(self):
        result, summary = self.run_fixture(
            'banner "first check"\ntrue\nbanner "second check"\ntrue'
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "=== first check ===\n=== second check ===\n")
        self.assertIsNone(summary)
        result, summary = self.run_fixture(
            'banner "first check"\ntrue\nbanner "second check"\nbash -c "exit 7"'
        )
        self.assert_failure(result, "second check", 7)
        self.assertEqual(summary, "FAIL: second check\n")

    def test_failure_shapes_preserve_status_and_report_once(self):
        cases = {
            "command": 'bash -c "exit 7"',
            "explicit exit": "exit 9",
            "pipeline": 'bash -c "exit 11" | cat',
            "function": 'check() { bash -c "exit 13"; echo UNREACHABLE; }; check',
            "function pipeline": 'check() { bash -c "exit 17"; }; check | cat',
            "subshell": '(bash -c "exit 19"; echo UNREACHABLE)',
            "command substitution": 'value=$(bash -c "exit 23")',
            "aggregated failure": 'FAILED=0\nfalse || FAILED=29\nexit "$FAILED"',
        }
        for (name, body), status in zip(cases.items(), (7, 9, 11, 13, 17, 19, 23, 29)):
            with self.subTest(name=name):
                result, summary = self.run_fixture(
                    f"banner {shlex.quote(name)}\n{body}\necho UNREACHABLE"
                )
                self.assert_failure(result, name, status)
                self.assertEqual(summary, f"FAIL: {name}\n")

    def test_missing_or_unwritable_summary_keeps_original_failure(self):
        for summary_mode in ("unset", "empty", "unwritable"):
            with self.subTest(summary=summary_mode):
                result, summary = self.run_fixture(
                    'banner "summary optional"\nbash -c "exit 31"', summary=summary_mode
                )
                self.assert_failure(result, "summary optional", 31)
                self.assertIsNone(summary)

    def test_success_does_not_touch_optional_summary(self):
        for summary_mode in ("unset", "empty", "unwritable"):
            with self.subTest(summary=summary_mode):
                result, summary = self.run_fixture(
                    'banner "pass"\nexit 0', summary=summary_mode
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, "=== pass ===\n")
                self.assertEqual(result.stderr, "")
                self.assertIsNone(summary)

    def test_summary_appends_and_success_preserves_existing_content(self):
        initial = "existing step content\n"
        result, summary = self.run_fixture('banner "pass"\ntrue', initial=initial)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("::error", result.stdout)
        self.assertEqual(summary, initial)
        result, summary = self.run_fixture('banner "fail"\nfalse', initial=initial)
        self.assert_failure(result, "fail", 1)
        self.assertEqual(summary, initial + "FAIL: fail\n")

    def test_annotation_property_and_message_escaping(self):
        name = "check::phase, title=other 100% %0A %3A\r\ncontinued"
        result, summary = self.run_fixture(f"banner {shlex.quote(name)}\nfalse")
        self.assert_failure(result, name, 1)
        self.assertEqual(summary, "FAIL: " + name.replace("\r", " ").replace("\n", " ") + "\n")

    def test_unknown_name_before_first_banner(self):
        result, summary = self.run_fixture("false")
        self.assert_failure(result, "unknown", 1)
        self.assertEqual(summary, "FAIL: unknown\n")

    def test_expected_negative_fixtures_do_not_report(self):
        result, summary = self.run_fixture('''
banner "known negative fixtures"
if false; then exit 90; fi
if ! bash -c "exit 2"; then :; fi
false || true
false && exit 91
check() { false; return 4; }
if check; then exit 92; fi
if value=$(bash -c "exit 3"); then exit 93; fi
bash -c "exit 5" | cat || true
cat < <(false)
true
''')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "=== known negative fixtures ===\n")
        self.assertIsNone(summary)


unittest.main(argv=[sys.argv[0]])
PY
