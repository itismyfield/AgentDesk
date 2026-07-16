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
            "/usr/bin/ssh -f -N -L 127.0.0.1:15432:/tmp/.s.PGSQL.5432 mac-mini",
            "/usr/bin/ssh -f -N -L 127.0.0.1:15432:127.0.0.1:5432 mac-mini",
        ]
        rejected = [
            "/usr/bin/ssh -N -R 15432:/tmp/.s.PGSQL.5432 mac-book",
            "/usr/bin/ssh -N -L 127.0.0.1:15433:/tmp/.s.PGSQL.5432 mac-mini",
            "/usr/bin/ssh -N -L 0.0.0.0:15432:/tmp/.s.PGSQL.5432 mac-mini",
            "/usr/bin/ssh -N -L127.0.0.1:15432:/tmp/.s.PGSQL.5432 mac-mini",
            "/usr/bin/ssh -N -L 127.0.0.1:15432:db:5432 host",
            "/usr/bin/ssh -N -L 127.0.0.1:15432:127.0.0.1:54321 mac-mini",
            "/usr/bin/ssh -N -L 127.0.0.1:15432:/tmp/.s.PGSQL.5432.bak mac-mini",
            "/usr/bin/ssh -N -L 127.0.0.1:15432:/tmp/.s.PGSQL.54321 mac-mini",
            "python monitor.py ssh -N -L 127.0.0.1:15432:/tmp/.s.PGSQL.5432 host",
            "/usr/bin/notssh -N -L 127.0.0.1:15432:/tmp/.s.PGSQL.5432 host",
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

    def test_wrapper_pins_unix_socket_forward_and_rejects_tcp_target(self):
        text = WRAPPER.read_text(encoding="utf-8")
        self.assertIn("-L 127.0.0.1:15432:/tmp/.s.PGSQL.5432", text)
        self.assertNotIn("-L 127.0.0.1:15432:127.0.0.1:5432", text)

    def test_remote_probe_rejects_unrestricted_arguments_and_ports(self):
        with tempfile.TemporaryDirectory() as tmp:
            config = Path(tmp) / "pg-tunnel.env"
            config.write_text("PG_TUNNEL_SSH_TARGET=mac-mini\n", encoding="utf-8")
            for argv in (
                ["--probe-remote", str(config)],
                ["--probe-remote", str(config), "15432"],
                ["--probe-remote", str(config), "1023"],
                ["--probe-remote", str(config), "65536"],
                ["--probe-remote", str(config), "not-a-port"],
                ["--probe-remote", str(config), "25432", "-oProxyCommand=bad"],
            ):
                with self.subTest(argv=argv):
                    p = subprocess.run(
                        [str(WRAPPER), *argv], capture_output=True, text=True
                    )
                    self.assertNotEqual(p.returncode, 0)

    def test_remote_probe_executes_only_exact_unix_socket_forward(self):
        text = WRAPPER.read_text(encoding="utf-8")
        self.assertIn('[ "$#" -eq 3 ] || die "usage: $0 --probe-remote CONFIG PORT"', text)
        self.assertIn('-L "127.0.0.1:$3:/tmp/.s.PGSQL.5432"', text)
        self.assertIn('"$PG_TUNNEL_SSH_TARGET"', text)

    def test_probe_cleanup_always_waits_after_term_and_kill(self):
        deploy = DEPLOY.read_text(encoding="utf-8")
        start = deploy.index("_cleanup_owned_pg_tunnel_preflight() {")
        end = deploy.index("_rollback_pg_tunnel_migration() {", start)
        cleanup = deploy[start:end]
        self.assertLess(cleanup.index('kill -TERM "$pid"'), cleanup.index('wait "$pid"'))
        self.assertLess(cleanup.index('kill -KILL "$pid"'), cleanup.index('wait "$pid"'))


class DeploymentWiringTests(unittest.TestCase):
    """Pin every load-bearing launchd/deploy invariant individually."""

    @staticmethod
    def _pg_block() -> str:
        deploy = DEPLOY.read_text(encoding="utf-8")
        start = deploy.index("PG_TUNNEL_LABEL=\"com.agentdesk.pg-tunnel\"")
        end = deploy.index("# #4381: a deploy restarts dcserver", start)
        return deploy[start:end]

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
        gate = block.index('[ -f "$PG_TUNNEL_CONFIG" ] || {')
        bootout = block.index(
            'launchctl bootout "$PG_TUNNEL_LAUNCHD_DOMAIN/$PG_TUNNEL_LABEL"'
        )
        bootstrap = block.index(
            'launchctl bootstrap "$PG_TUNNEL_LAUNCHD_DOMAIN" "$PG_TUNNEL_PLIST_PATH"'
        )
        self.assertLess(gate, bootout)
        self.assertLess(gate, bootstrap)
        self.assertIn("Supervisor NOT armed on this node", block)

    def test_block_is_before_release_stop_and_deploy_success(self):
        deploy = DEPLOY.read_text(encoding="utf-8")
        block_at = deploy.index('PG_TUNNEL_LABEL="com.agentdesk.pg-tunnel"')
        migrate_at = deploy.index("_migrate_pg_tunnel_before_release_stop", block_at)
        stop_at = deploy.index('echo "▸ Stopping release..."', migrate_at)
        self.assertLess(migrate_at, stop_at)
        self.assertLess(stop_at, deploy.index("DEPLOY_OK=1", stop_at))

    def _run_block(
        self,
        adk_rel: Path,
        home: Path,
        *,
        fail_probe: bool = False,
        fail_canonical: bool = False,
        job_loaded: bool = False,
        old_wrapper: str | None = None,
        old_plist: str | None = None,
        manual_kind: str = "none",
    ) -> tuple[subprocess.CompletedProcess, Path, Path]:
        fake_bin = home / "fake-bin"
        fake_bin.mkdir(parents=True)
        event_log = home / "events.log"
        launchctl_log = home / "launchctl.log"
        for name, body in {
            "launchctl": """#!/bin/sh
printf 'launchctl %s\\n' "$*" >> "$EVENT_LOG"
printf '%s\\n' "$*" >> "$LAUNCHCTL_LOG"
if [ "$1" = print ]; then
  if [ "${JOB_LOADED:-0}" = 1 ]; then exit 0; else exit 1; fi
fi
exit 0
""",
            "xattr": "#!/bin/sh\nexit 0\n",
            "sleep": "#!/bin/sh\nexec /bin/sleep 0.01\n",
            "psql": """#!/bin/sh
while [ ! -f "$PROBE_READY" ]; do /bin/sleep 0.01; done
printf 'psql %s\\n' "${PGDATABASE:-missing}" >> "$EVENT_LOG"
case "${PGDATABASE:-}" in
  *:15432/*) [ "${FAIL_CANONICAL:-0}" != 1 ] ;;
  *) [ "${FAIL_PROBE:-0}" != 1 ] ;;
esac
""",
            "ruby": """#!/bin/sh
printf 'ruby %s\\n' "$*" >> "$EVENT_LOG"
while [ "$#" -gt 3 ]; do shift; done
port=$1
output=$2
printf 'postgresql://agentdesk@127.0.0.1:%s/agentdesk?sslmode=require' "$port" > "$output"
""",
        }.items():
            path = fake_bin / name
            path.write_text(body, encoding="utf-8")
            path.chmod(0o755)
        repo = home / "repo"
        (repo / "scripts").mkdir(parents=True)
        wrapper = repo / "scripts/pg_tunnel.sh"
        wrapper.write_text(
            """#!/bin/sh
printf 'wrapper %s\\n' "$*" >> "$EVENT_LOG"
case "$1" in
  --check-config) grep -q '^PG_TUNNEL_SSH_TARGET=[A-Za-z0-9_.:@][A-Za-z0-9_.:@-]*$' "$2" ;;
  --probe-remote) trap 'printf "probe-term\\n" >> "$EVENT_LOG"; exit 0' TERM; : > "$PROBE_READY"; while :; do /bin/sleep 1; done ;;
  --canonical-kind) printf '%s\\n' "${MANUAL_KIND:-none}" ;;
  --restore-canonical) printf 'restore %s\\n' "$3" >> "$EVENT_LOG" ;;
  *) exit 1 ;;
esac
""",
            encoding="utf-8",
        )
        wrapper.chmod(0o755)
        if old_wrapper is not None:
            (adk_rel / "bin/pg-tunnel.sh").write_text(old_wrapper, encoding="utf-8")
        plist_path = home / "Library/LaunchAgents/com.agentdesk.pg-tunnel.plist"
        if old_plist is not None:
            plist_path.parent.mkdir(parents=True)
            plist_path.write_text(old_plist, encoding="utf-8")
        prelude = """
PG_TUNNEL_PREFLIGHT_PID=""
PG_TUNNEL_PREFLIGHT_DSN_FILE=""
PG_TUNNEL_PREFLIGHT_PASSWORD_FILE=""
PG_TUNNEL_ROLLBACK_ARMED=0
PG_TUNNEL_ROLLBACK_DIR=""
PG_TUNNEL_ROLLBACK_JOB_LOADED=0
PG_TUNNEL_ROLLBACK_MANUAL_KIND="none"
PG_TUNNEL_ROLLBACK_MANUAL_CONFIG=""
PG_TUNNEL_ROLLBACK_WRAPPER_SOURCE=""
_launchd_domain() { printf '%s\\n' gui/999999; }
"""
        script = (
            "set -euo pipefail\n"
            f"REPO={shlex.quote(str(repo))}\n"
            f"ADK_REL={shlex.quote(str(adk_rel))}\n"
            f"HOME={shlex.quote(str(home))}\n"
            f"EVENT_LOG={shlex.quote(str(event_log))}\n"
            f"LAUNCHCTL_LOG={shlex.quote(str(launchctl_log))}\n"
            f"FAIL_PROBE={int(fail_probe)}\n"
            f"FAIL_CANONICAL={int(fail_canonical)}\n"
            f"JOB_LOADED={int(job_loaded)}\n"
            f"MANUAL_KIND={shlex.quote(manual_kind)}\n"
            f"PROBE_READY={shlex.quote(str(home / 'probe.ready'))}\n"
            "DATABASE_URL=postgresql://agentdesk@db.internal:5432/agentdesk?sslmode=require\n"
            "export EVENT_LOG LAUNCHCTL_LOG FAIL_PROBE FAIL_CANONICAL JOB_LOADED MANUAL_KIND PROBE_READY DATABASE_URL\n"
            f"FAKE_BIN={shlex.quote(str(fake_bin))}\n"
            f"PATH={shlex.quote(str(fake_bin))}:/usr/bin:/bin:/usr/sbin:/sbin\n"
            "export PATH\n"
            "ruby() { \"$FAKE_BIN/ruby\" \"$@\"; }\n"
            "psql() { \"$FAKE_BIN/psql\" \"$@\"; }\n"
            "launchctl() { \"$FAKE_BIN/launchctl\" \"$@\"; }\n"
            "xattr() { \"$FAKE_BIN/xattr\" \"$@\"; }\n"
            "command -v psql >/dev/null || exit 97\n"
            "command -v ruby >/dev/null || exit 98\n"
            + prelude
            + "\n"
            + self._cleanup_helpers()
            + "\ntrap '_status=$?; _cleanup_owned_pg_tunnel_preflight; "
            "[ \"$_status\" -eq 0 ] || _rollback_pg_tunnel_migration' EXIT\n"
            + self._pg_block()
            + "\nprintf 'release-stop\\n' >> \"$EVENT_LOG\"\necho HARNESS-END\n"
        )
        p = subprocess.run(
            ["bash", "-c", script], capture_output=True, text=True, timeout=30
        )
        return p, launchctl_log, event_log

    @staticmethod
    def _cleanup_helpers() -> str:
        deploy = DEPLOY.read_text(encoding="utf-8")
        start = deploy.index("_cleanup_owned_pg_tunnel_preflight() {")
        end = deploy.index("_cleanup_on_exit() {", start)
        return deploy[start:end]

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
            p, launchctl_log, _ = self._run_block(adk, home)
            self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
            self.assertIn("HARNESS-END", p.stdout)
            self.assertTrue(launchctl_log.is_file())

            plist_path = home / "Library/LaunchAgents/com.agentdesk.pg-tunnel.plist"
            with plist_path.open("rb") as f:
                plist = plistlib.load(f)
            self.assertEqual(
                plist["ProgramArguments"],
                [
                    str(adk / "bin/pg-tunnel.sh"),
                    str(adk / "config/pg-tunnel.env"),
                    "-N",
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=10",
                    "-o",
                    "ServerAliveInterval=15",
                    "-o",
                    "ServerAliveCountMax=3",
                    "-o",
                    "ExitOnForwardFailure=yes",
                    "-L",
                    "127.0.0.1:15432:/tmp/.s.PGSQL.5432",
                ],
            )
            self.assertTrue(plist["KeepAlive"])
            self.assertEqual(plist["ThrottleInterval"], 10)

    def test_missing_machine_config_does_not_touch_launchd(self):
        with tempfile.TemporaryDirectory() as tmp:
            adk = Path(tmp) / "adk"
            for sub in ("bin", "config", "logs"):
                (adk / sub).mkdir(parents=True)
            home = Path(tmp) / "home"
            home.mkdir()
            p, launchctl_log, _ = self._run_block(adk, home)
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
            p, launchctl_log, _ = self._run_block(adk, home)
            self.assertNotEqual(p.returncode, 0)
            self.assertIn("config invalid", p.stdout)
            self.assertFalse(launchctl_log.exists())

    def test_remote_sql_failure_stops_before_canonical_install(self):
        with tempfile.TemporaryDirectory() as tmp:
            adk = Path(tmp) / "adk"
            for sub in ("bin", "config", "logs"):
                (adk / sub).mkdir(parents=True)
            (adk / "config/pg-tunnel.env").write_text(
                "PG_TUNNEL_SSH_TARGET=mac-mini\n", encoding="utf-8"
            )
            home = Path(tmp) / "home"
            home.mkdir()
            p, launchctl_log, event_log = self._run_block(
                adk, home, fail_probe=True
            )
            self.assertNotEqual(p.returncode, 0)
            events = event_log.read_text(encoding="utf-8")
            self.assertIn("probe-term", events)
            self.assertNotIn("bootstrap", events)
            self.assertNotIn("release-stop", events)
            self.assertFalse(launchctl_log.exists())

    def test_canonical_failure_restores_wrapper_plist_and_job(self):
        with tempfile.TemporaryDirectory() as tmp:
            adk = Path(tmp) / "adk"
            for sub in ("bin", "config", "logs"):
                (adk / sub).mkdir(parents=True)
            (adk / "config/pg-tunnel.env").write_text(
                "PG_TUNNEL_SSH_TARGET=mac-mini\n", encoding="utf-8"
            )
            home = Path(tmp) / "home"
            home.mkdir()
            p, _, event_log = self._run_block(
                adk,
                home,
                fail_canonical=True,
                job_loaded=True,
                old_wrapper="old-wrapper\n",
                old_plist="old-plist\n",
            )
            self.assertNotEqual(p.returncode, 0)
            self.assertEqual(
                (adk / "bin/pg-tunnel.sh").read_text(encoding="utf-8"),
                "old-wrapper\n",
            )
            self.assertEqual(
                (
                    home / "Library/LaunchAgents/com.agentdesk.pg-tunnel.plist"
                ).read_text(encoding="utf-8"),
                "old-plist\n",
            )
            events = event_log.read_text(encoding="utf-8")
            self.assertGreaterEqual(events.count("launchctl bootstrap"), 2)
            self.assertNotIn("release-stop", events)

    def test_canonical_failure_restores_manual_tcp_tunnel_with_source_wrapper(self):
        with tempfile.TemporaryDirectory() as tmp:
            adk = Path(tmp) / "adk"
            for sub in ("bin", "config", "logs"):
                (adk / sub).mkdir(parents=True)
            (adk / "config/pg-tunnel.env").write_text(
                "PG_TUNNEL_SSH_TARGET=mac-mini\n", encoding="utf-8"
            )
            home = Path(tmp) / "home"
            home.mkdir()
            p, _, event_log = self._run_block(
                adk,
                home,
                fail_canonical=True,
                old_wrapper="#!/bin/sh\nexit 99\n",
                manual_kind="tcp",
            )
            self.assertNotEqual(p.returncode, 0)
            self.assertIn("restore tcp", event_log.read_text(encoding="utf-8"))

    def test_success_order_is_remote_sql_then_canonical_sql_then_stop(self):
        with tempfile.TemporaryDirectory() as tmp:
            adk = Path(tmp) / "adk"
            for sub in ("bin", "config", "logs"):
                (adk / sub).mkdir(parents=True)
            (adk / "config/pg-tunnel.env").write_text(
                "PG_TUNNEL_SSH_TARGET=mac-mini\n", encoding="utf-8"
            )
            home = Path(tmp) / "home"
            home.mkdir()
            p, _, event_log = self._run_block(adk, home)
            events = event_log.read_text(encoding="utf-8")
            self.assertEqual(p.returncode, 0, p.stdout + p.stderr + events)
            self.assertIn("psql postgresql://", events, p.stdout + p.stderr + events)
            probe_sql = events.index("psql postgresql://")
            cleanup = events.index("probe-term", probe_sql)
            bootstrap = events.index("launchctl bootstrap", cleanup)
            canonical_sql = events.index(":15432/", bootstrap)
            stop = events.index("release-stop", canonical_sql)
            self.assertLess(probe_sql, cleanup)
            self.assertLess(cleanup, bootstrap)
            self.assertLess(bootstrap, canonical_sql)
            self.assertLess(canonical_sql, stop)


if __name__ == "__main__":
    unittest.main()
