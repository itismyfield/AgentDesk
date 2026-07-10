"""Regression tests for the consumer-owned PostgreSQL tunnel (#4378)."""

from __future__ import annotations

import plistlib
import shlex
import subprocess
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
DEPLOY = REPO_ROOT / "scripts/deploy-release.sh"
WRAPPER = REPO_ROOT / "scripts/pg_tunnel.sh"


class WrapperSafetyTests(unittest.TestCase):
    def test_takeover_pattern_matches_only_exact_local_forward(self):
        text = WRAPPER.read_text(encoding="utf-8")
        line = next(
            line for line in text.splitlines() if line.startswith("TAKEOVER_PATTERN=")
        )
        pattern = shlex.split(line.split("=", 1)[1])[0]

        matching = [
            "/usr/bin/ssh -f -N -L 127.0.0.1:15432:127.0.0.1:5432 mac-mini",
            "ssh -N -o BatchMode=yes -L  127.0.0.1:15432:db:5432 host",
        ]
        rejected = [
            "/usr/bin/ssh -N -R 15432:127.0.0.1:5432 mac-book",
            "/usr/bin/ssh -N -L 127.0.0.1:15433:127.0.0.1:5432 mac-mini",
            "/usr/bin/ssh -N -L 0.0.0.0:15432:127.0.0.1:5432 mac-mini",
            "/usr/bin/ssh -N -L127.0.0.1:15432:127.0.0.1:5432 mac-mini",
            "python monitor.py ssh -N -L 127.0.0.1:15432:db:5432 host",
            "/usr/bin/notssh -N -L 127.0.0.1:15432:db:5432 host",
        ]
        for command in matching:
            with self.subTest(command=command):
                p = subprocess.run(
                    ["grep", "-Eq", pattern], input=command, text=True
                )
                self.assertEqual(p.returncode, 0)
        for command in rejected:
            with self.subTest(command=command):
                p = subprocess.run(
                    ["grep", "-Eq", pattern], input=command, text=True
                )
                self.assertNotEqual(p.returncode, 0)

    def test_pid_is_revalidated_with_same_pattern_before_each_signal(self):
        text = WRAPPER.read_text(encoding="utf-8")
        self.assertIn("still_matching_manual_tunnel \"$pid\" || continue", text)
        self.assertIn(
            'kill -0 "$pid" 2>/dev/null && still_matching_manual_tunnel "$pid"',
            text,
        )

    def test_takeover_completes_before_ssh_exec(self):
        text = WRAPPER.read_text(encoding="utf-8")
        self.assertLess(
            text.rindex("take_over_manual_tunnel"),
            text.index('exec /usr/bin/ssh "$@" "$PG_TUNNEL_SSH_TARGET"'),
        )

    def test_machine_config_requires_safe_ssh_target(self):
        with tempfile.TemporaryDirectory() as tmp:
            good = Path(tmp) / "good.env"
            good.write_text("PG_TUNNEL_SSH_TARGET=mac-mini\n", encoding="utf-8")
            p = subprocess.run(
                [str(WRAPPER), "--check-config", str(good)],
                capture_output=True,
                text=True,
            )
            self.assertEqual(p.returncode, 0, p.stdout + p.stderr)

            bad = Path(tmp) / "bad.env"
            bad.write_text("PG_TUNNEL_SSH_TARGET=-oProxyCommand=bad\n", encoding="utf-8")
            p = subprocess.run(
                [str(WRAPPER), "--check-config", str(bad)],
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(p.returncode, 0)

    def test_wrapper_refuses_noncanonical_launchd_arguments(self):
        with tempfile.TemporaryDirectory() as tmp:
            config = Path(tmp) / "pg-tunnel.env"
            config.write_text("PG_TUNNEL_SSH_TARGET=mac-mini\n", encoding="utf-8")
            p = subprocess.run(
                [str(WRAPPER), str(config), "-N"],
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(p.returncode, 0)
            self.assertIn("non-canonical ssh arguments", p.stderr)


class DeploymentWiringTests(unittest.TestCase):
    """Pin every load-bearing launchd/deploy invariant individually."""

    @staticmethod
    def _pg_block() -> str:
        lines = DEPLOY.read_text(encoding="utf-8").splitlines()
        start = next(
            i
            for i, line in enumerate(lines)
            if 'PG_TUNNEL_LABEL="com.agentdesk.pg-tunnel"' in line
        )
        end = next(
            i for i, line in enumerate(lines) if "PG tunnel staging FAILED" in line
        )
        return "\n".join(lines[start : end + 2])

    def test_ci_script_checks_runs_this_suite(self):
        ci = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn("tests.test_pg_tunnel", ci)

    def test_wrapper_is_registered_for_portable_path_lint(self):
        checker = (REPO_ROOT / "scripts/check-portable-paths.py").read_text(
            encoding="utf-8"
        )
        self.assertIn('"scripts/pg_tunnel.sh"', checker)

    def test_exit_on_forward_failure_is_pinned(self):
        self.assertIn(
            "<string>-o</string><string>ExitOnForwardFailure=yes</string>",
            self._pg_block(),
        )

    def test_server_alive_options_are_pinned(self):
        block = self._pg_block()
        self.assertIn(
            "<string>-o</string><string>ServerAliveInterval=15</string>", block
        )
        self.assertIn(
            "<string>-o</string><string>ServerAliveCountMax=3</string>", block
        )

    def test_connect_timeout_is_pinned(self):
        self.assertIn(
            "<string>-o</string><string>ConnectTimeout=10</string>",
            self._pg_block(),
        )

    def test_batch_mode_is_pinned(self):
        self.assertIn(
            "<string>-o</string><string>BatchMode=yes</string>", self._pg_block()
        )

    def test_keepalive_and_throttle_are_pinned(self):
        block = self._pg_block()
        self.assertIn("<key>KeepAlive</key><true/>", block)
        self.assertIn("<key>ThrottleInterval</key><integer>10</integer>", block)

    def test_plist_publish_is_atomic_mv(self):
        block = self._pg_block()
        self.assertIn('cat > "$PG_TUNNEL_PLIST_PATH.tmp"', block)
        self.assertIn(
            'mv -f "$PG_TUNNEL_PLIST_PATH.tmp" "$PG_TUNNEL_PLIST_PATH"', block
        )

    def test_machine_local_env_gates_bootout_and_bootstrap(self):
        block = self._pg_block()
        gate = block.index('if [ -f "$PG_TUNNEL_CONFIG" ]; then')
        bootout = block.index(
            'launchctl bootout "$LAUNCHD_DOMAIN/$PG_TUNNEL_LABEL"'
        )
        bootstrap = block.index(
            'launchctl bootstrap "$LAUNCHD_DOMAIN" "$PG_TUNNEL_PLIST_PATH"'
        )
        self.assertLess(gate, bootout)
        self.assertLess(gate, bootstrap)
        self.assertIn("Supervisor NOT armed on this node", block)

    def test_block_is_after_deploy_ok_and_before_manifest(self):
        deploy = DEPLOY.read_text(encoding="utf-8")
        block_at = deploy.index('PG_TUNNEL_LABEL="com.agentdesk.pg-tunnel"')
        self.assertLess(deploy.index("DEPLOY_OK=1"), block_at)
        self.assertLess(block_at, deploy.index("_write_release_source_manifest", block_at))

    def _run_block(self, adk_rel: Path, home: Path) -> tuple[subprocess.CompletedProcess, Path]:
        fake_bin = home / "fake-bin"
        fake_bin.mkdir(parents=True)
        launchctl_log = home / "launchctl.log"
        launchctl = fake_bin / "launchctl"
        launchctl.write_text(
            '#!/bin/sh\nprintf "%s\\n" "$*" >> "$LAUNCHCTL_LOG"\n',
            encoding="utf-8",
        )
        xattr = fake_bin / "xattr"
        xattr.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        launchctl.chmod(0o755)
        xattr.chmod(0o755)
        script = (
            "set -euo pipefail\n"
            f"REPO={shlex.quote(str(REPO_ROOT))}\n"
            f"ADK_REL={shlex.quote(str(adk_rel))}\n"
            f"HOME={shlex.quote(str(home))}\n"
            "LAUNCHD_DOMAIN=gui/999999\n"
            f"LAUNCHCTL_LOG={shlex.quote(str(launchctl_log))}\n"
            "export LAUNCHCTL_LOG\n"
            f"PATH={shlex.quote(str(fake_bin))}:/usr/bin:/bin:/usr/sbin:/sbin\n"
            "export PATH\n"
            + self._pg_block()
            + "\necho HARNESS-END\n"
        )
        p = subprocess.run(
            ["bash", "-c", script], capture_output=True, text=True, timeout=30
        )
        return p, launchctl_log

    def test_generated_plist_is_valid_and_round_trips_metachar_paths(self):
        with tempfile.TemporaryDirectory() as tmp:
            adk = Path(tmp) / "adk & <rel>"
            for sub in ("bin", "config", "logs"):
                (adk / sub).mkdir(parents=True)
            (adk / "config/pg-tunnel.env").write_text(
                "PG_TUNNEL_SSH_TARGET=mac-mini\n", encoding="utf-8"
            )
            home = Path(tmp) / "home & <operator>"
            home.mkdir()
            p, launchctl_log = self._run_block(adk, home)
            self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
            self.assertIn("HARNESS-END", p.stdout)
            self.assertTrue(launchctl_log.is_file())

            plist_path = home / "Library/LaunchAgents/com.agentdesk.pg-tunnel.plist"
            with plist_path.open("rb") as f:
                plist = plistlib.load(f)
            self.assertEqual(plist["ProgramArguments"][0], str(adk / "bin/pg-tunnel.sh"))
            self.assertEqual(plist["ProgramArguments"][1], str(adk / "config/pg-tunnel.env"))
            self.assertTrue(plist["KeepAlive"])
            self.assertEqual(plist["ThrottleInterval"], 10)

    def test_missing_machine_config_does_not_touch_launchd(self):
        with tempfile.TemporaryDirectory() as tmp:
            adk = Path(tmp) / "adk"
            for sub in ("bin", "config", "logs"):
                (adk / sub).mkdir(parents=True)
            home = Path(tmp) / "home"
            home.mkdir()
            p, launchctl_log = self._run_block(adk, home)
            self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
            self.assertIn("Supervisor NOT armed on this node", p.stdout)
            self.assertFalse(launchctl_log.exists())

    def test_invalid_machine_config_does_not_touch_launchd(self):
        with tempfile.TemporaryDirectory() as tmp:
            adk = Path(tmp) / "adk"
            for sub in ("bin", "config", "logs"):
                (adk / sub).mkdir(parents=True)
            (adk / "config/pg-tunnel.env").write_text(
                "PG_TUNNEL_SSH_TARGET=-unsafe\n", encoding="utf-8"
            )
            home = Path(tmp) / "home"
            home.mkdir()
            p, launchctl_log = self._run_block(adk, home)
            self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
            self.assertIn("config invalid", p.stdout)
            self.assertFalse(launchctl_log.exists())


if __name__ == "__main__":
    unittest.main()
