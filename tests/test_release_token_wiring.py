"""Exercise Cargo observation at the release-script process boundary."""

import json
import os
from pathlib import Path
import shlex
import shutil
import tempfile
import unittest
from unittest import mock

from scripts.check_release_token_wiring import REPO, observe

VARIANTS = {
    "invocation_arguments": 'dry_cmd=(cargo clean)\n"${dry_cmd[@]}" --profile release',
    "bare_array_append": 'dry_cmd=(cargo)\ndry_cmd+=(clean --release)\n"${dry_cmd[@]}"',
    "length_index_append": 'dry_cmd=(cargo clean)\ndry_cmd[${#dry_cmd[@]}]=--release\n"${dry_cmd[@]}"',
    "same_line": 'cargo clean --release && python3 "$SCRIPT_DIR/build_token.py" -- cargo clean --release',
    "function": 'run_cleanup() { cargo "$@"; }\nrun_cleanup clean --release',
    "eval": 'dry_command=cargo\neval "$dry_command clean --release"',
}


class ReleaseTokenWiringTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="release-wiring-input-")
        self.addCleanup(temp.cleanup)
        self.repo = Path(temp.name)
        (self.repo / "scripts").mkdir()
        for name in ("build-release.sh", "deploy-release.sh", "build_token.py"):
            shutil.copy2(REPO / "scripts" / name, self.repo / "scripts" / name)

    def inject(self, script, command):
        source = (REPO / "scripts" / script).read_text()
        source = source.replace('. "$SCRIPT_DIR/_defaults.sh"',
                                '. "$SCRIPT_DIR/_defaults.sh"\n' + command, 1)
        (self.repo / "scripts" / script).write_text(source)
        return source

    def test_current_scripts_complete_all_cargo_phases_under_the_token(self):
        for script in ("build-release.sh", "deploy-release.sh"):
            for profile in ("release", "release-fast"):
                with self.subTest(script=script, profile=profile):
                    report = observe(REPO, script, profile)
                    self.assertEqual(report["errors"], [], report)
                    calls = report["cargo"]
                    self.assertTrue(all(c["held"] for c in calls if c["release"]), calls)
                    phases = {c["argv"][0] for c in calls}
                    self.assertIn("build", phases)
                    if script == "deploy-release.sh":
                        self.assertIn("clean", phases)
                        metadata = next(c for c in calls if c["argv"][0] == "metadata")
                        self.assertFalse(metadata["held"])
        for target in ("x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"):
            with self.subTest(target=target):
                self.assertEqual(observe(REPO, "build-release.sh", "release", target=target)["errors"], [])

    def test_indirect_release_calls_fail_where_the_old_scanner_passed(self):
        for script in ("build-release.sh", "deploy-release.sh"):
            for name, command in VARIANTS.items():
                with self.subTest(script=script, variant=name):
                    self.inject(script, command)
                    report = observe(self.repo, script, "release")
                    self.assertTrue(report["errors"], report)
                    self.assertTrue(all(e.startswith("release cargo outside build token:")
                                        for e in report["errors"]), report)
                    print(json.dumps({"script": script, "variant": name,
                                      "runtime": "FAIL", "reason": report["errors"]}))

    def test_cleanup_soft_failure_and_forged_markers_cannot_hide_unheld_calls(self):
        path = self.repo / "scripts/deploy-release.sh"
        source = path.read_text().replace(
            'ADK_BUILD_TOKEN_WAIT_TIMEOUT_SECS=60 python3 scripts/build_token.py -- "${clean_cmd[@]}"',
            'ADK_BUILD_TOKEN_HOLDER=forged "${clean_cmd[@]}"; false')
        path.write_text(source)
        report = observe(self.repo, "deploy-release.sh", "release-fast")
        self.assertEqual(len(report["errors"]), 1, report)
        self.assertIn("release cargo outside build token:", report["errors"][0])
        clean = next(c for c in report["cargo"] if c["argv"][0] == "clean")
        self.assertEqual(clean["holder"], "forged")
        self.assertFalse(clean["held"])
        self.assertIn("failed; continuing with staged release artifact", report["stdout"])

    def test_side_effect_stubs_refuse_escape_even_when_shell_ignores_errors(self):
        outside = self.repo / "must-not-be-created"
        self.inject("deploy-release.sh", f"""
mkdir {shlex.quote(str(outside))} || true
launchctl bootout dry-test || true
ssh dry-test invalid || true
kill -0 $$ || true
""")
        with mock.patch.dict(os.environ, {
                "HOME": str(self.repo), "AGENTDESK_ROOT_DIR": str(outside),
                "BASH_ENV": "/does-not-exist", "ADK_BUILD_TOKEN_HOLDER": "ambient-forgery"}):
            report = observe(self.repo, "deploy-release.sh", "release")
        self.assertFalse(outside.exists())
        self.assertEqual(len(report["errors"]), 1, report)
        reason = report["errors"][0]
        for diagnostic in ("dry safety:", "path outside dry root", "launchctl", "ssh", "builtin kill forbidden"):
            self.assertIn(diagnostic, reason)
        self.assertTrue(all(c["held"] for c in report["cargo"] if c["release"]))

    def test_early_success_exit_is_not_a_completed_observation(self):
        (self.repo / "scripts/build-release.sh").write_text("exit 0\n")
        report = observe(self.repo, "build-release.sh", "release")
        self.assertIn("dry execution incomplete: exit=0", report["errors"])
        self.assertIn("dry coverage: missing Cargo phases ['build']", report["errors"])


if __name__ == "__main__":
    unittest.main()
