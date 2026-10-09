"""Exercise Cargo observation at the release-script process boundary."""

import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from scripts.check_release_token_wiring import REPO, observe
from scripts import build_token as bt

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


class SccacheConsumerTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="sccache-consumer-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.brew = self.root / "late-brew"
        self.brew.mkdir()
        self.executable(self.brew / "sccache", "#!/bin/sh\nexit 0\n")
        self.output = self.root / "env.json"
        self.env = {"HOME": str(self.root), "PATH": f"{self.bin}:/usr/bin:/bin",
                    "ENV_OUTPUT": str(self.output)}

    def executable(self, path, source):
        path.write_text(source)
        path.chmod(0o700)

    def test_campaign_inherits_the_ambient_environment_and_preserves_the_parent(self):
        command = [sys.executable, "-c",
                   "import json,os;json.dump(dict(os.environ),open(os.environ['ENV_OUTPUT'],'w'))"]
        with mock.patch.dict(os.environ, self.env, clear=True), \
                mock.patch.object(bt, "_HOMEBREW_BIN", str(self.brew)):
            before = dict(os.environ)
            self.assertEqual(bt.run(command, path=str(self.root / "token")), 0)
            self.assertEqual(dict(os.environ), before)
        seen = json.loads(self.output.read_text())
        self.assertEqual(seen["RUSTC_WRAPPER"], str(self.brew / "sccache"))
        self.assertEqual(seen["PATH"], f"{self.brew}:{self.env['PATH']}")
        self.assertEqual(seen["SCCACHE_DIR"], str(self.root / ".cache/sccache"))
        self.assertEqual(seen["SCCACHE_CACHE_SIZE"], "40G")
        self.assertEqual(seen["SCCACHE_IDLE_TIMEOUT"], "0")

    def test_release_consumers_preserve_shell_activation_or_the_empty_opt_out_pair(self):
        scripts = self.root / "scripts"
        scripts.mkdir()
        defaults = (REPO / "scripts/_defaults.sh").read_text()
        self.assertEqual(defaults.count('local homebrew_bin="/opt/homebrew/bin"'), 1)
        (scripts / "_defaults.sh").write_text(defaults.replace(
            'local homebrew_bin="/opt/homebrew/bin"',
            f'local homebrew_bin="{self.root}/missing-brew"') +
            '\n_preflight_resource_contention() { return 0; }\n')
        driver = self.root / "driver.py"
        driver.write_text(f"""
import os, sys
from pathlib import Path
sys.path.insert(0, {str(REPO / 'scripts')!r})
import build_token as bt
assert Path(sys.argv[1]).resolve() == Path.cwd() / 'scripts/build_token.py'
assert sys.argv[2] == '--'
assert 'cargo' in sys.argv[3:]
bt._HOMEBREW_BIN = {str(self.brew)!r}
real_open = os.open
def sealed_open(path, *args, **kwargs):
    assert 'adk-build-token.lock' not in str(path), 'canonical token forbidden'
    return real_open(path, *args, **kwargs)
os.open = sealed_open
raise SystemExit(bt.run(sys.argv[3:], path={str(self.root / 'token')!r}))
""")
        self.executable(self.bin / "cargo", f"#!{sys.executable}\n" +
                        "import json,os;json.dump(dict(os.environ),open(os.environ['ENV_OUTPUT'],'w'))\n")
        for script, profile in (("build-release.sh", "release"),
                                ("deploy-release.sh", "release"),
                                ("deploy-release.sh", "release-fast")):
            source = (REPO / "scripts" / script).read_text()
            start = 'export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-40G}"'
            end = ('echo "[2/3] Dashboard' if script == "build-release.sh" else
                   '# Rebuild dashboard so deploy never ships a stale dist.')
            self.assertEqual(source.count(start), 1)
            self.assertEqual(source.count(end), 1)
            fragment = source.split(start)[1].split(end)[0]
            scenario = self.root / "scenario.sh"
            scenario.write_text(f"""set -euo pipefail
REPO={shlex.quote(str(self.root))}
SCRIPT_DIR="$REPO/scripts"
cd "$REPO"
. "$SCRIPT_DIR/_defaults.sh"
PYTHON=python3
python3() {{ {shlex.quote(sys.executable)} {shlex.quote(str(driver))} "$@"; }}
BUILD_PROFILE={profile}
DEPLOY_BUILD_PROFILE={profile}
DEPLOY_LOCK_TIMEOUT_SECS=5
TARGET=aarch64-apple-darwin
CARGO_TARGET_ARGS=(--target "$TARGET")
_ensure_dashboard_dependencies() {{ :; }}
_check_repo_remote_freshness() {{ :; }}
_external_artifact_would_skip_o_writer() {{ return 1; }}
_resolve_default_release_binary() {{ echo "$REPO/artifact"; }}
""" + start + fragment + "\n:\n")
            for installed in (False, True):
                sccache = self.bin / "sccache"
                if installed:
                    self.executable(sccache, "#!/bin/sh\nexit 0\n")
                else:
                    sccache.unlink(missing_ok=True)
                self.output.unlink(missing_ok=True)
                with self.subTest(script=script, profile=profile, installed=installed):
                    result = subprocess.run(["/bin/bash", str(scenario)], env=self.env,
                                            text=True, capture_output=True, timeout=10)
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    seen = json.loads(self.output.read_text())
                    self.assertEqual(seen["PATH"], self.env["PATH"])
                    self.assertEqual(seen["SCCACHE_CACHE_SIZE"], "40G")
                    if installed:
                        self.assertEqual(seen["RUSTC_WRAPPER"], str(sccache))
                        self.assertEqual(seen["SCCACHE_DIR"], str(self.root / ".cache/sccache"))
                        self.assertEqual(seen["SCCACHE_IDLE_TIMEOUT"], "0")
                        self.assertNotIn("CARGO_BUILD_RUSTC_WRAPPER", seen)
                    else:
                        self.assertEqual(seen["RUSTC_WRAPPER"], "")
                        self.assertEqual(seen["CARGO_BUILD_RUSTC_WRAPPER"], "")
                        self.assertNotIn("SCCACHE_DIR", seen)
                        self.assertNotIn("SCCACHE_IDLE_TIMEOUT", seen)


if __name__ == "__main__":
    unittest.main()
