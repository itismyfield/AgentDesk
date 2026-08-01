"""Regression tests for the consumer-owned PostgreSQL tunnel (#4378)."""

from __future__ import annotations

import os
import plistlib
import re
import shlex
import signal
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

    def test_restricted_takeover_mode_accepts_no_extra_arguments(self):
        text = WRAPPER.read_text(encoding="utf-8")
        self.assertIn('    --take-over-canonical)', text)
        self.assertIn(
            '[ "$#" -eq 1 ] || die "usage: $0 --take-over-canonical"', text
        )

    def test_production_takeover_owns_term_kill_and_final_refusal(self):
        text = WRAPPER.read_text(encoding="utf-8")
        start = text.index("take_over_manual_tunnel() {")
        end = text.index("validate_ssh_args() {", start)
        helper = text[start:end]
        self.assertLess(
            helper.index('still_matching_manual_tunnel "$pid" || continue'),
            helper.index('/bin/kill -TERM "$pid"'),
        )
        self.assertLess(
            helper.index('/bin/kill -TERM "$pid"'),
            helper.index('/bin/kill -KILL "$pid"'),
        )
        self.assertLess(
            helper.index('/bin/kill -KILL "$pid"'),
            helper.index('die "residual manual tunnel pid=$pid did not exit'),
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
        expected = "-L 127.0.0.1:15432:/tmp/.s.PGSQL.5432"
        self.assertIn(expected, text)
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
        """Static contract: every owned-probe signal is followed by a reap."""
        deploy = DEPLOY.read_text(encoding="utf-8")
        start = deploy.index("_cleanup_owned_pg_tunnel_preflight() {")
        end = deploy.index("_rollback_pg_tunnel_migration() {", start)
        cleanup = deploy[start:end]
        preflight_end = deploy.index("_reset_pg_tunnel_rollback_state() {", start)
        preflight = deploy[start:preflight_end]
        self.assertLess(cleanup.index('kill -TERM "$pid"'), cleanup.index('wait "$pid"'))
        self.assertLess(cleanup.index('kill -KILL "$pid"'), cleanup.index('wait "$pid"'))
        self.assertIn('while kill -0 "$pid" 2>/dev/null && [ "$attempts" -lt 25 ]; do', preflight)
        self.assertEqual(
            re.findall(r"sleep 0\.2(?![0-9])", preflight), ["sleep 0.2"]
        )


class DeploymentWiringTests(unittest.TestCase):
    """Pin every load-bearing launchd/deploy invariant individually."""

    @staticmethod
    def _strip_shell_comments(text: str) -> str:
        """Remove physical-line comments without hiding code after quoted ``#``."""
        token = re.compile(
            r''' '(?:[^'\r\n]*)' | "(?:\\[^\r\n]|[^"\\\r\n])*" |
                \\[^\r\n] | (?m:^\#[^\r\n]*) | (?<=[ \t])\#[^\r\n]* ''',
            re.VERBOSE,
        )

        def replace(match: re.Match[str]) -> str:
            value = match.group(0)
            return "" if value.lstrip(" \t").startswith("#") else value

        return token.sub(replace, text)

    @staticmethod
    def _pg_block() -> str:
        deploy = DEPLOY.read_text(encoding="utf-8")
        start = deploy.index("PG_TUNNEL_LABEL=\"com.agentdesk.pg-tunnel\"")
        # The runtime PID snapshot now follows the reversible tunnel migration
        # and precedes the candidate database migration/restart boundary.
        end = deploy.index('LOCK_FILE="$ADK_REL/runtime/dcserver.lock"', start)
        return deploy[start:end]

    def test_ci_script_checks_runs_this_suite(self):
        ci = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn("tests.test_pg_tunnel", ci)

    def test_signal_traps_match_exact_physical_line_contract(self):
        """Enforce the deliberately narrow static trap contract.

        The static contract covers literal ``trap`` commands that start a
        physical line and cleanup-handler definitions that start a physical line
        or immediately follow ``;``, ``&``, or ``|``.  For those definitions,
        whitespace, newlines, and comments between ``()`` and ``{`` are covered.
        This is not a Bash parser.  Trap tokens elsewhere on a line, function
        indirection such as ``_disarm_exit``, commands assembled in variables,
        ``builtin``/``command``/``eval`` dispatch, and other multi-segment
        definition syntax are outside the static contract.
        All three static scans (this physical-line/definition scan,
        ``test_trap_function_shadowing_is_absent``, and the post-registration
        tail guard) remove text from ``#`` through each physical line end before
        matching, while preserving simple quoted spans so a real forbidden form
        after a quoted ``#`` is not hidden.  This is not a shell parser and does
        not identify heredoc bodies.  A harmless command-looking heredoc line
        without a comment can therefore still be reported as a false positive;
        that is an accepted fail-closed tradeoff.

        The dynamic INT/TERM/EXIT backstop executes only the production slice
        that starts at ``_cleanup_owned_pg_tunnel_preflight`` and ends immediately
        before ``_self_hosted_release_session``.  Trap-state changes outside that
        slice are covered only by the static contract and inherit the limitations
        above.  The post-registration tail guard applies the same physical-line
        literal-``trap`` prefix check (``trap`` followed by whitespace or
        end-of-line) and ignores full-line and trailing comments.
        That tail contains detached-child paths where clearing an inherited trap
        with ``trap - EXIT`` can be legitimate; if such a use is needed, this
        contract must be changed explicitly rather than treating it as
        dynamically covered.

        Inside this boundary, exact ordered trap lines, unique cleanup-handler
        definitions, reset/registration order, and the preflight-call strip set
        are pinned.  A line-leading query such as ``trap -p INT`` is therefore
        also rejected by the exact contract.
        """
        deploy = self._strip_shell_comments(
            DEPLOY.read_text(encoding="utf-8")
        )
        deploy_lines = deploy.splitlines()

        trap_rows = [
            (line_number, line)
            for line_number, line in enumerate(deploy_lines, 1)
            if re.match(r"^[ \t]*trap[ \t]", line)
        ]
        # The reset commands are deliberately indented as function-body lines.
        # The raw expected literals themselves enforce that the three
        # registrations remain at column 0.
        expected_traps = [
            "    trap - EXIT",
            "    trap '' INT TERM",
            "trap _cleanup_on_exit EXIT",
            "trap '_handle_cleanup_signal 130' INT",
            "trap '_handle_cleanup_signal 143' TERM",
        ]
        observed_traps = [text for _line_number, text in trap_rows]
        self.assertEqual(
            observed_traps,
            expected_traps,
            "expected exactly five trap lines in this order; observed rows="
            + repr([f"line {line_number}: {text}" for line_number, text in trap_rows]),
        )

        def definition_rows(name: str) -> list[tuple[int, str]]:
            spaced_parens = r"[ \t]*\([ \t]*\)"
            definition_anchor = r"(?:^|[;&|]\s*)[ \t]*"
            brace_gap = r"(?:[ \t]*(?:#[^\n]*)?\n)*[ \t]*"
            patterns = (
                re.compile(
                    definition_anchor
                    + re.escape(name)
                    + spaced_parens
                    + brace_gap
                    + r"\{",
                    re.MULTILINE,
                ),
                re.compile(
                    definition_anchor
                    + r"function[ \t]+"
                    + re.escape(name)
                    + r"(?:"
                    + spaced_parens
                    + r")?"
                    + brace_gap
                    + r"\{",
                    re.MULTILINE,
                ),
            )
            rows: list[tuple[int, str]] = []
            for pattern in patterns:
                for match in pattern.finditer(deploy):
                    line_number = deploy.count("\n", 0, match.start()) + 1
                    rows.append((line_number, match.group(0).strip()))
            rows.sort(key=lambda row: row[0])
            return rows

        cleanup_definitions = definition_rows("_cleanup_on_exit")
        signal_definitions = definition_rows("_handle_cleanup_signal")
        self.assertEqual(
            len(cleanup_definitions),
            1,
            "expected one _cleanup_on_exit() definition; observed rows="
            + repr(cleanup_definitions),
        )
        self.assertEqual(
            len(signal_definitions),
            1,
            "expected one _handle_cleanup_signal() definition; observed rows="
            + repr(signal_definitions),
        )
        cleanup_definition_line = cleanup_definitions[0][0]
        signal_definition_line = signal_definitions[0][0]

        reset_rows = {
            text: line_number
            for line_number, text in trap_rows
            if text in {"    trap - EXIT", "    trap '' INT TERM"}
        }
        registration_texts = [
            "trap _cleanup_on_exit EXIT",
            "trap '_handle_cleanup_signal 130' INT",
            "trap '_handle_cleanup_signal 143' TERM",
        ]
        registration_rows = [
            (line_number, text)
            for line_number, text in trap_rows
            if text in registration_texts
        ]
        self.assertEqual(
            set(reset_rows),
            {"    trap - EXIT", "    trap '' INT TERM"},
            "expected both cleanup reset lines; observed rows=" + repr(trap_rows),
        )
        self.assertEqual(
            len(registration_rows),
            3,
            "expected three cleanup registration rows; observed rows="
            + repr(registration_rows),
        )
        first_registration_line = min(
            line_number for line_number, _text in registration_rows
        )
        self.assertLess(
            max(cleanup_definition_line, signal_definition_line),
            first_registration_line,
            "the last cleanup-handler definition must be the contract definition "
            "before registrations: "
            f"cleanup={cleanup_definition_line}, signal={signal_definition_line}, "
            f"registrations={registration_rows}",
        )
        for reset_text in ("    trap - EXIT", "    trap '' INT TERM"):
            with self.subTest(reset=reset_text):
                reset_line = reset_rows[reset_text]
                self.assertGreater(
                    reset_line,
                    cleanup_definition_line,
                    "cleanup reset must follow the cleanup definition: "
                    f"definition={cleanup_definition_line}, reset={reset_line}",
                )
                self.assertLess(
                    reset_line,
                    signal_definition_line,
                    "cleanup reset must precede the signal-handler definition: "
                    f"reset={reset_line}, signal_definition={signal_definition_line}",
                )
                self.assertLess(
                    reset_line,
                    min(line_number for line_number, _text in registration_rows),
                    "cleanup reset must precede all signal registrations: "
                    f"reset={reset_line}, registrations={registration_rows}",
                )

        expected_call_name = "_cleanup_owned_pg_tunnel_preflight"
        call_rows = []
        candidate_call_rows = []
        call_pattern = re.compile(
            r"^([A-Za-z_][A-Za-z0-9_]*)(?:[ \t]*(?:;|&&|\|\||\||&))?$"
        )
        for line_number, line in enumerate(deploy_lines, 1):
            stripped = line.strip()
            match = call_pattern.fullmatch(stripped)
            if match is None:
                continue
            name = match.group(1)
            if name == expected_call_name:
                call_rows.append((line_number, stripped))
            if "cleanup_owned_pg_tunnel_preflight" in name:
                candidate_call_rows.append((line_number, stripped))

        expected_call_set = {expected_call_name}
        observed_call_set = {text for _line_number, text in candidate_call_rows}
        self.assertEqual(
            observed_call_set,
            expected_call_set,
            "preflight call strip set mismatch; observed rows="
            + repr(candidate_call_rows),
        )
        self.assertEqual(
            len(call_rows),
            6,
            "expected six _cleanup_owned_pg_tunnel_preflight calls; observed rows="
            + repr(call_rows),
        )
        self.assertEqual(
            len(candidate_call_rows),
            6,
            "expected six preflight-shaped call rows; observed rows="
            + repr(candidate_call_rows),
        )

    def test_trap_function_shadowing_is_absent(self):
        """Reject Bash ``trap`` function declarations, including spaced parens."""
        deploy = self._strip_shell_comments(
            DEPLOY.read_text(encoding="utf-8")
        )
        definition_anchor = r"(?:^|[;&|]\s*)[ \t]*"
        shadow_patterns = (
            re.compile(
                definition_anchor + r"trap[ \t]*\([ \t]*\)", re.MULTILINE
            ),
            re.compile(
                definition_anchor + r"function[ \t]+trap\b", re.MULTILINE
            ),
        )
        for pattern in shadow_patterns:
            with self.subTest(pattern=pattern.pattern):
                self.assertEqual(
                    pattern.findall(deploy),
                    [],
                    "trap function shadowing is outside the supported contract: "
                    + pattern.pattern,
                )

    def test_no_line_leading_trap_follows_production_registrations(self):
        """Apply the narrow line-leading trap contract after registrations.

        The dynamic harness safely executes through the three registrations but
        cannot execute the remaining deploy flow.  From that boundary through
        EOF, only a literal ``trap`` command at physical line start is forbidden.
        Comment text is removed before that lexical check, including trailing
        comments; indirect, assembled, builtin, command, and eval forms remain
        outside the declared static contract.
        """
        deploy = self._strip_shell_comments(
            DEPLOY.read_text(encoding="utf-8")
        )
        boundary = "trap '_handle_cleanup_signal 143' TERM"
        tail = deploy[deploy.index(boundary) + len(boundary) :]
        forbidden = re.compile(r"^[ \t]*trap(?:[ \t]|$)")
        rows = [
            (line_number, line)
            for line_number, line in enumerate(tail.splitlines(), 1)
            if forbidden.search(line)
        ]
        self.assertEqual(
            rows,
            [],
            "line-leading trap commands after production registration are "
            "outside the dynamic backstop; tail-relative rows=" + repr(rows),
        )

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

    def test_database_migration_precedes_restart_request_and_release_stop(self):
        deploy = DEPLOY.read_text(encoding="utf-8")
        stage_at = deploy.index('echo "▸ Staging signed binary from $SOURCE_BINARY..."')
        tunnel_at = deploy.index("_migrate_pg_tunnel_before_release_stop", stage_at)
        migration_at = deploy.index(
            'if ! "$STAGED_BINARY" release-migrate-postgres; then', tunnel_at
        )
        restart_at = deploy.index("request_restart_drain_mode_or_fail", migration_at)
        persistence_at = deploy.index("wait_for_restart_persistence_or_fail", restart_at)
        stop_at = deploy.index('echo "▸ Stopping release..."', persistence_at)
        self.assertLess(stage_at, tunnel_at)
        self.assertLess(tunnel_at, migration_at)
        self.assertLess(migration_at, restart_at)
        self.assertLess(restart_at, persistence_at)
        self.assertLess(persistence_at, stop_at)

    def test_database_migration_failure_cannot_request_restart_or_bootout(self):
        deploy = DEPLOY.read_text(encoding="utf-8")
        migration_at = deploy.index(
            'if ! "$STAGED_BINARY" release-migrate-postgres; then'
        )
        failure_exit_at = deploy.index("    exit 1", migration_at)
        restart_at = deploy.index("request_restart_drain_mode_or_fail", migration_at)
        stop_at = deploy.index('echo "▸ Stopping release..."', restart_at)
        migration_failure = deploy[migration_at:failure_exit_at]
        self.assertLess(failure_exit_at, restart_at)
        self.assertLess(restart_at, stop_at)
        self.assertNotIn("request_restart_drain_mode_or_fail", migration_failure)
        self.assertNotIn("restart_pending", migration_failure)
        self.assertNotIn("launchctl bootout", migration_failure)
        self.assertNotIn("clear_restart_drain_mode", migration_failure)
        self.assertIn("the existing runtime remains active", migration_failure)

    def test_database_migration_failure_preserves_live_runtime_without_marker(self):
        deploy = DEPLOY.read_text(encoding="utf-8")
        start = deploy.index('LOCK_FILE="$ADK_REL/runtime/dcserver.lock"')
        end = deploy.index("# Migration 0100 is now a forward-only binary floor", start)
        migration_block = deploy[start:end]

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            runtime = root / "runtime"
            runtime.mkdir()
            events = root / "events.log"
            health = runtime / "health"
            health.write_text("healthy\n", encoding="utf-8")
            candidate = root / "candidate"
            candidate.write_text(
                "#!/bin/sh\nprintf 'migration:%s\\n' \"$*\" >> \"$EVENT_LOG\"\nexit 1\n",
                encoding="utf-8",
            )
            candidate.chmod(0o755)
            old_runtime = subprocess.Popen(["sleep", "30"])
            try:
                (runtime / "dcserver.lock").write_text(
                    f"{old_runtime.pid}\n", encoding="utf-8"
                )
                script = (
                    "set -euo pipefail\n"
                    f"ADK_REL={shlex.quote(str(root))}\n"
                    f"STAGED_BINARY={shlex.quote(str(candidate))}\n"
                    f"EVENT_LOG={shlex.quote(str(events))}\n"
                    "PLIST_REL=com.agentdesk.release\n"
                    "REL_PORT=8791\n"
                    "export EVENT_LOG\n"
                    "request_restart_drain_mode_or_fail() { "
                    "printf 'restart-request\\n' >> \"$EVENT_LOG\"; }\n"
                    "launchctl() { printf 'bootout:%s\\n' \"$*\" >> \"$EVENT_LOG\"; }\n"
                    + migration_block
                )
                result = subprocess.run(
                    ["bash", "-c", script], capture_output=True, text=True, timeout=10
                )
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIsNone(old_runtime.poll(), "old runtime exited on migration failure")
                self.assertEqual(health.read_text(encoding="utf-8"), "healthy\n")
                self.assertFalse((runtime / "restart_pending").exists())
                self.assertEqual(
                    events.read_text(encoding="utf-8"),
                    "migration:release-migrate-postgres\n",
                )
            finally:
                old_runtime.terminate()
                old_runtime.wait(timeout=5)

    def _run_block(
        self,
        adk_rel: Path,
        home: Path,
        *,
        fail_probe: bool = False,
        fail_canonical: bool = False,
        fail_rollback_readiness: bool = False,
        fail_backup_copy: bool = False,
        fail_kind_snapshot: bool = False,
        fail_restore_bootstrap: bool = False,
        fail_manual_restore: bool = False,
        fail_listener_absence: bool = False,
        rollback_listener_checks: int = 0,
        rollback_ready_attempt: int = 1,
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
            "probe": """#!/usr/bin/env python3
import os
import signal


def stop(_signum, _frame):
    with open(os.environ["EVENT_LOG"], "a", encoding="utf-8") as handle:
        handle.write("probe-term\\n")
    raise SystemExit(0)


signal.signal(signal.SIGTERM, stop)
def expire(_signum, _frame):
    raise SystemExit(124)


signal.signal(signal.SIGALRM, expire)
signal.alarm(8)
open(os.environ["PROBE_READY"], "a", encoding="utf-8").close()
while True:
    signal.pause()
""",
            "launchctl": """#!/bin/sh
printf 'launchctl %s\\n' "$*" >> "$EVENT_LOG"
printf '%s\\n' "$*" >> "$LAUNCHCTL_LOG"
if [ "$1" = print ]; then
  if [ "${JOB_LOADED:-0}" = 1 ]; then exit 0; else exit 1; fi
fi
if [ "$1" = bootout ]; then
  bootout_count=$(grep -c '^bootout ' "$LAUNCHCTL_LOG" || true)
  [ "$bootout_count" -lt 2 ] || : > "$ROLLBACK_BOOTOUT"
fi
if [ "$1" = bootstrap ] && [ -f "$ROLLBACK_BOOTOUT" ]; then
  [ "${FAIL_RESTORE_BOOTSTRAP:-0}" != 1 ] || exit 1
  : > "$ROLLBACK_READY"
fi
exit 0
""",
            "xattr": "#!/bin/sh\nexit 0\n",
            "lsof": """#!/bin/sh
printf 'lsof %s\n' "$*" >> "$EVENT_LOG"
if [ "${FAIL_LISTENER_ABSENCE:-0}" = 1 ]; then exit 0; fi
if [ -f "$ROLLBACK_BOOTOUT" ] && [ "${ROLLBACK_LISTENER_CHECKS:-0}" -gt 0 ]; then
  current=0
  if [ -f "$ROLLBACK_LISTENER_COUNT" ]; then
    IFS= read -r current < "$ROLLBACK_LISTENER_COUNT"
  fi
  current=$((current + 1))
  printf '%s\n' "$current" > "$ROLLBACK_LISTENER_COUNT"
  [ "$current" -gt "$ROLLBACK_LISTENER_CHECKS" ] || exit 0
fi
exit 1
""",
            "sleep": "#!/bin/sh\nexec /bin/sleep 0.01\n",
            "psql": """#!/bin/sh
while [ ! -f "$PROBE_READY" ]; do /bin/sleep 0.01; done
printf 'psql host=%s hostaddr=%s port=%s user=%s db=%s sslmode=%s timeout=%s passfile=%s\\n' \
  "${PGHOST:-missing}" "${PGHOSTADDR:-missing}" "${PGPORT:-missing}" \
  "${PGUSER:-missing}" "${PGDATABASE:-missing}" "${PGSSLMODE:-missing}" \
  "${PGCONNECT_TIMEOUT:-missing}" "${PGPASSFILE:+set}" >> "$EVENT_LOG"
[ "${PGHOST:-}" = db.internal ] || exit 81
[ "${PGHOSTADDR:-}" = 127.0.0.1 ] || exit 87
[ "${PGUSER:-}" = agentdesk ] || exit 82
[ "${PGDATABASE:-}" = agentdesk ] || exit 83
[ "${PGSSLMODE:-}" = require ] || exit 84
[ -n "${PGPASSFILE:-}" ] && [ -s "$PGPASSFILE" ] || exit 85
case "${PGPORT:-}" in
  15432)
    if [ ! -f "$ROLLBACK_READY" ]; then
      printf 'psql-state canonical\n' >> "$EVENT_LOG"
      [ "${FAIL_CANONICAL:-0}" != 1 ] || exit 1
    else
      printf 'psql-state rollback\n' >> "$EVENT_LOG"
      [ "${FAIL_ROLLBACK_READINESS:-0}" != 1 ] || exit 1
      rollback_attempt=0
      if [ -f "$ROLLBACK_PSQL_COUNT" ]; then
        IFS= read -r rollback_attempt < "$ROLLBACK_PSQL_COUNT"
      fi
      rollback_attempt=$((rollback_attempt + 1))
      printf '%s\n' "$rollback_attempt" > "$ROLLBACK_PSQL_COUNT"
      [ "$rollback_attempt" -ge "${ROLLBACK_READY_ATTEMPT:-1}" ] || exit 1
    fi
    ;;
  "") exit 86 ;;
  *) [ "${FAIL_PROBE:-0}" != 1 ] ;;
esac
""",
            "ruby": """#!/bin/sh
printf 'ruby invoked\\n' >> "$EVENT_LOG"
while [ "$#" -gt 3 ]; do shift; done
port=$1
output_dir=$2
password_output=$3
printf '%s' db.internal > "$output_dir/PGHOST"
printf '%s' 127.0.0.1 > "$output_dir/PGHOSTADDR"
printf '%s' "$port" > "$output_dir/PGPORT"
printf '%s' agentdesk > "$output_dir/PGUSER"
printf '%s' agentdesk > "$output_dir/PGDATABASE"
printf '%s' require > "$output_dir/PGSSLMODE"
printf '%s\\n' 'db.internal:'"$port"':agentdesk:agentdesk:s3cr3t' > "$password_output"
""",
            "cp": """#!/bin/sh
if [ "${FAIL_BACKUP_COPY:-0}" = 1 ] && [ "$1" = -p ]; then
  exit 1
fi
exec /bin/cp "$@"
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
  --probe-remote) exec "$FAKE_BIN/probe" ;;
  --canonical-kind) [ "${FAIL_KIND_SNAPSHOT:-0}" != 1 ] || exit 1; printf '%s\\n' "${MANUAL_KIND:-none}" ;;
  --take-over-canonical) printf 'takeover\\n' >> "$EVENT_LOG" ;;
  --restore-canonical)
    printf 'restore %s\\n' "$3" >> "$EVENT_LOG"
    [ "${FAIL_MANUAL_RESTORE:-0}" != 1 ] || exit 1
    : > "$ROLLBACK_READY"
    ;;
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
PG_TUNNEL_PREFLIGHT_CONNINFO_DIR=""
PG_TUNNEL_PREFLIGHT_PASSWORD_FILE=""
PG_TUNNEL_ROLLBACK_ARMED=0
PG_TUNNEL_ROLLBACK_DIR=""
PG_TUNNEL_ROLLBACK_JOB_LOADED=0
PG_TUNNEL_ROLLBACK_MANUAL_KIND="none"
PG_TUNNEL_ROLLBACK_MANUAL_CONFIG=""
PG_TUNNEL_ROLLBACK_WRAPPER_SOURCE=""
_launchd_domain() { printf '%s\\n' gui/999999; }
"""
        rollback_state = home / "rollback-state.txt"
        script = (
            "set -euo pipefail\n"
            f"REPO={shlex.quote(str(repo))}\n"
            f"ADK_REL={shlex.quote(str(adk_rel))}\n"
            f"HOME={shlex.quote(str(home))}\n"
            f"EVENT_LOG={shlex.quote(str(event_log))}\n"
            f"LAUNCHCTL_LOG={shlex.quote(str(launchctl_log))}\n"
            f"FAIL_PROBE={int(fail_probe)}\n"
            f"FAIL_CANONICAL={int(fail_canonical)}\n"
            f"FAIL_ROLLBACK_READINESS={int(fail_rollback_readiness)}\n"
            f"FAIL_BACKUP_COPY={int(fail_backup_copy)}\n"
            f"FAIL_KIND_SNAPSHOT={int(fail_kind_snapshot)}\n"
            f"FAIL_RESTORE_BOOTSTRAP={int(fail_restore_bootstrap)}\n"
            f"FAIL_MANUAL_RESTORE={int(fail_manual_restore)}\n"
            f"FAIL_LISTENER_ABSENCE={int(fail_listener_absence)}\n"
            f"ROLLBACK_LISTENER_CHECKS={rollback_listener_checks}\n"
            f"ROLLBACK_READY_ATTEMPT={rollback_ready_attempt}\n"
            f"JOB_LOADED={int(job_loaded)}\n"
            f"MANUAL_KIND={shlex.quote(manual_kind)}\n"
            f"PROBE_READY={shlex.quote(str(home / 'probe.ready'))}\n"
            f"ROLLBACK_STATE={shlex.quote(str(rollback_state))}\n"
            f"ROLLBACK_BOOTOUT={shlex.quote(str(home / 'rollback.bootout'))}\n"
            f"ROLLBACK_READY={shlex.quote(str(home / 'rollback.ready'))}\n"
            f"ROLLBACK_LISTENER_COUNT={shlex.quote(str(home / 'rollback-listener.count'))}\n"
            f"ROLLBACK_PSQL_COUNT={shlex.quote(str(home / 'rollback-psql.count'))}\n"
            "DATABASE_URL=postgresql://agentdesk:s3cr3t@db.internal:5432/agentdesk?sslmode=require\n"
            "export EVENT_LOG LAUNCHCTL_LOG FAIL_PROBE FAIL_CANONICAL FAIL_ROLLBACK_READINESS FAIL_BACKUP_COPY FAIL_KIND_SNAPSHOT FAIL_RESTORE_BOOTSTRAP FAIL_MANUAL_RESTORE FAIL_LISTENER_ABSENCE ROLLBACK_LISTENER_CHECKS ROLLBACK_READY_ATTEMPT JOB_LOADED MANUAL_KIND PROBE_READY ROLLBACK_BOOTOUT ROLLBACK_READY ROLLBACK_LISTENER_COUNT ROLLBACK_PSQL_COUNT DATABASE_URL\n"
            f"FAKE_BIN={shlex.quote(str(fake_bin))}\n"
            f"PATH={shlex.quote(str(fake_bin))}:/usr/bin:/bin:/usr/sbin:/sbin\n"
            "export FAKE_BIN PATH\n"
            "ruby() { \"$FAKE_BIN/ruby\" \"$@\"; }\n"
            "psql() { \"$FAKE_BIN/psql\" \"$@\"; }\n"
            "launchctl() { \"$FAKE_BIN/launchctl\" \"$@\"; }\n"
            "xattr() { \"$FAKE_BIN/xattr\" \"$@\"; }\n"
            "cp() { \"$FAKE_BIN/cp\" \"$@\"; }\n"
            "command -v psql >/dev/null || exit 97\n"
            "command -v ruby >/dev/null || exit 98\n"
            + prelude
            + "\n"
            + self._cleanup_helpers()
            + "\ntrap '_status=$?; _cleanup_owned_pg_tunnel_preflight; "
            "if [ \"$_status\" -ne 0 ]; then _rollback_pg_tunnel_migration || true; fi; "
            "printf \"armed=%s backup=%s\\n\" \"$PG_TUNNEL_ROLLBACK_ARMED\" \"$PG_TUNNEL_ROLLBACK_DIR\" > \"$ROLLBACK_STATE\"' EXIT\n"
            + self._pg_block()
            + "\nprintf 'release-stop\\n' >> \"$EVENT_LOG\"\necho HARNESS-END\n"
        )
        p = subprocess.run(
            ["bash", "-c", script], capture_output=True, text=True, timeout=30
        )
        return p, launchctl_log, event_log

    @staticmethod
    def _block_failure_diagnostics(
        p: subprocess.CompletedProcess, home: Path
    ) -> str:
        files = []
        for path in sorted(home.rglob("*"), key=lambda item: str(item)):
            kind = "dir" if path.is_dir() else "file"
            files.append(f"{path.relative_to(home)} [{kind}]")
        listing = "\n".join(files) if files else "<empty>"
        safe_logs = []
        for name in ("events.log", "launchctl.log"):
            path = home / name
            if path.is_file():
                safe_logs.append(f"{name}:\n{path.read_text(encoding='utf-8')}")
        log_output = "\n".join(safe_logs) if safe_logs else "<none>"
        return (
            f"returncode={p.returncode}\n"
            f"stdout:\n{p.stdout}\n"
            f"stderr:\n{p.stderr}\n"
            f"home files:\n{listing}\n"
            f"safe harness logs:\n{log_output}"
        )

    @staticmethod
    def _cleanup_helpers() -> str:
        deploy = DEPLOY.read_text(encoding="utf-8")
        start = deploy.index("_cleanup_owned_pg_tunnel_preflight() {")
        end = deploy.index("_cleanup_on_exit() {", start)
        return deploy[start:end]

    @staticmethod
    def _cleanup_sleep_literal() -> str:
        deploy = DEPLOY.read_text(encoding="utf-8")
        start = deploy.index("_cleanup_owned_pg_tunnel_preflight() {")
        end = deploy.index("_reset_pg_tunnel_rollback_state() {", start)
        preflight = deploy[start:end]
        matches = re.findall(
            r"sleep[ \t]+([0-9]+(?:\.[0-9]+)?)(?![0-9])", preflight
        )
        if len(matches) != 1:
            raise AssertionError(
                "expected one cleanup sleep literal in deploy preflight: "
                f"{matches!r}"
            )
        return matches[0]

    def _run_signal_cleanup(self, signal_name: str) -> tuple[int, str]:
        """Run one real signal path with bounded child and parent lifetimes.

        Code-path budget from measured runs: a typical successful path is
        ~0.2–1.3 s.  Five isolated runs of the old 60 x 0.05 s signal guard
        averaged 7.01 s on this host; added to readiness near 4 s, that agrees
        with the observed ~10.75 s failure and disproves the former 8.25 s bound.
        The reduced 20-iteration guard averaged 2.37 s, giving a conservative
        serial estimate of ``4 + 2.37 + 1.25 = 7.62 s`` including maximum EXIT
        cleanup.  The external 10 s timeout still fails closed and reaps the
        process group, but scheduler delay can make it fire before the intended
        internal diagnostic.
        """
        signal_status = {"INT": 130, "TERM": 143}[signal_name]
        cleanup_sleep = self._cleanup_sleep_literal()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fake_bin = root / "fake-bin"
            fake_bin.mkdir()
            event_log = root / "events.log"
            (fake_bin / "sleep").write_text(
                "#!/bin/sh\n"
                f'if [ "${{1:-}}" = {cleanup_sleep} ]; then\n'
                '  printf "scaled-sleep=%s\\n" "$1" >> "$EVENT_LOG"\n'
                "  exec /bin/sleep 0.05\n"
                "fi\n"
                "exec /bin/sleep 0.01\n",
                encoding="utf-8",
            )
            (fake_bin / "sleep").chmod(0o755)
            probe = root / "probe.py"
            probe.write_text(
                "#!/usr/bin/env python3\n"
                "import os\n"
                "import signal\n"
                "\n"
                "def stop(_signum, _frame):\n"
                "    os.kill(int(os.environ['PARENT_PID']), signal.SIGTERM)\n"
                "    with open(os.environ['EVENT_LOG'], 'a', encoding='utf-8') as handle:\n"
                "        handle.write('probe-term\\n')\n"
                "    raise SystemExit(0)\n"
                "\n"
                "signal.signal(signal.SIGTERM, stop)\n"
                "def expire(_signum, _frame):\n"
                "    raise SystemExit(124)\n"
                "\n"
                "signal.signal(signal.SIGALRM, expire)\n"
                "signal.alarm(8)\n"
                "with open(os.environ['EVENT_LOG'], 'a', encoding='utf-8') as handle:\n"
                "    handle.write('probe-ready\\n')\n"
                "while True:\n"
                "    signal.pause()\n",
                encoding="utf-8",
            )
            probe.chmod(0o755)
            script = (
                "set -euo pipefail\n"
                f"EVENT_LOG={shlex.quote(str(event_log))}\n"
                f"PATH={shlex.quote(str(fake_bin))}:/usr/bin:/bin:/usr/sbin:/sbin\n"
                "export EVENT_LOG PATH\n"
                "PG_TUNNEL_PREFLIGHT_PID=\"\"\n"
                "PG_TUNNEL_PREFLIGHT_CONNINFO_DIR=\"\"\n"
                "PG_TUNNEL_PREFLIGHT_PASSWORD_FILE=\"\"\n"
                "PG_TUNNEL_ROLLBACK_ARMED=0\n"
                "ROLLBACK_ARMED=0\n"
                "DEPLOY_OK=0\n"
                "STAGED_BINARY=\"\"\n"
                "POLICIES_STAGED=\"\"\n"
                "LAUNCHD_MIGRATED_STAGED=\"\"\n"
                "RELEASE_ROOT_SCRIPTS_STAGED=\"\"\n"
                "DEPLOY_MKDIR_LOCK_DIR=\"\"\n"
                "_rollback_release_binary() { :; }\n"
                "_finalize_detached_helper() { printf 'cleanup=%s\\n' \"$1\" >> \"$EVENT_LOG\"; }\n"
                + self._production_cleanup_handlers()
                + f"\nPARENT_PID=$$ {shlex.quote(str(probe))} &\n"
                "PG_TUNNEL_PREFLIGHT_PID=$!\n"
                "probe_ready_deadline=$((SECONDS + 4))\n"
                "while ! grep -q '^probe-ready$' \"$EVENT_LOG\"; do\n"
                "    if [ \"$SECONDS\" -ge \"$probe_ready_deadline\" ]; then\n"
                "        printf 'probe-ready timeout at SECONDS=%s (deadline=%s)\\n' \"$SECONDS\" \"$probe_ready_deadline\" >&2\n"
                "        exit 98\n"
                "    fi\n"
                "    /bin/sleep 0.01\n"
                "done\n"
                f"kill -{signal_name} $$\n"
                "signal_wait_attempts=0\n"
                "while :; do\n"
                "    if [ \"$signal_wait_attempts\" -ge 20 ]; then\n"
                "        printf 'signal cleanup timeout after %s attempts\\n' \"$signal_wait_attempts\" >&2\n"
                "        exit 97\n"
                "    fi\n"
                "    /bin/sleep 0.05\n"
                "    signal_wait_attempts=$((signal_wait_attempts + 1))\n"
                "done\n"
            )
            process = subprocess.Popen(
                ["bash", "-c", script],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=True,
            )
            try:
                stdout, stderr = process.communicate(timeout=10)
            except subprocess.TimeoutExpired as exc:
                # The probe is a child of the harness.  Kill its process group
                # and synchronously communicate again so no orphan survives a
                # failed bounded run.
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                stdout, stderr = process.communicate()
                self.fail(
                    "signal cleanup harness timed out after 10 s; "
                    f"stdout={stdout or exc.stdout!r} stderr={stderr or exc.stderr!r}"
                )
            p = subprocess.CompletedProcess(
                process.args, process.returncode, stdout, stderr
            )
            events = event_log.read_text(encoding="utf-8") if event_log.exists() else ""
            self.assertEqual(p.returncode, signal_status, p.stdout + p.stderr + events)
            return p.returncode, events

    def test_cleanup_sleep_shim_scales_preflight_delay_deterministically(self):
        """Exercise the fake sleep once without relying on a busy-wait race."""
        cleanup_sleep = self._cleanup_sleep_literal()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fake_bin = root / "fake-bin"
            fake_bin.mkdir()
            event_log = root / "events.log"
            (fake_bin / "sleep").write_text(
                "#!/bin/sh\n"
                f'if [ "${{1:-}}" = {cleanup_sleep} ]; then\n'
                '  printf "scaled-sleep=%s\\n" "$1" >> "$EVENT_LOG"\n'
                "  exec /bin/sleep 0.05\n"
                "fi\n"
                "exec /bin/sleep 0.01\n",
                encoding="utf-8",
            )
            (fake_bin / "sleep").chmod(0o755)
            script = (
                f"EVENT_LOG={shlex.quote(str(event_log))}\n"
                f"PATH={shlex.quote(str(fake_bin))}:/usr/bin:/bin\n"
                "export EVENT_LOG PATH\n"
                f"sleep {cleanup_sleep}\n"
            )
            p = subprocess.run(
                ["/bin/sh", "-c", script], capture_output=True, text=True, timeout=5
            )
            events = event_log.read_text(encoding="utf-8") if event_log.exists() else ""
            self.assertEqual(p.returncode, 0, p.stdout + p.stderr + events)
            self.assertEqual(events, f"scaled-sleep={cleanup_sleep}\n")

    @staticmethod
    def _production_cleanup_handlers() -> str:
        deploy = DEPLOY.read_text(encoding="utf-8")
        start = deploy.index("_cleanup_owned_pg_tunnel_preflight() {")
        end = deploy.index("_self_hosted_release_session() {", start)
        return deploy[start:end]

    def test_int_cleanup_reaps_probe_once_and_returns_130(self):
        status, events = self._run_signal_cleanup("INT")
        self.assertEqual(status, 130, events)
        self.assertEqual(events.count("probe-term"), 1, events)
        self.assertEqual(events.count("cleanup=130"), 1, events)

    def test_term_cleanup_reaps_probe_once_and_returns_143(self):
        status, events = self._run_signal_cleanup("TERM")
        self.assertEqual(status, 143, events)
        self.assertEqual(events.count("probe-term"), 1, events)
        self.assertEqual(events.count("cleanup=143"), 1, events)

    def test_cleanup_traps_register_without_test_environment(self):
        """Observe production registrations with all harness-only vars unset."""
        script = (
            "set -euo pipefail\n"
            + self._production_cleanup_handlers()
            + "\nprintf 'EXIT=<%s>\\n' \"$(builtin trap -p EXIT)\"\n"
            + "printf 'INT=<%s>\\n' \"$(builtin trap -p INT)\"\n"
            + "printf 'TERM=<%s>\\n' \"$(builtin trap -p TERM)\"\n"
            + "builtin trap - EXIT INT TERM\n"
        )
        process = subprocess.run(
            ["/bin/bash", "-c", script],
            capture_output=True,
            text=True,
            timeout=10,
            env={"PATH": "/usr/bin:/bin"},
        )
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertEqual(
            process.stdout,
            "EXIT=<trap -- '_cleanup_on_exit' EXIT>\n"
            "INT=<trap -- '_handle_cleanup_signal 130' SIGINT>\n"
            "TERM=<trap -- '_handle_cleanup_signal 143' SIGTERM>\n",
        )

    def test_exit_cleanup_runs_once_and_masks_int_during_cleanup(self):
        """Exercise the production EXIT registration, including a heredoc backstop.

        Each case executes the production cleanup/registration slice and exits
        normally.  The second case sends INT from the cleanup body itself; the
        reset lines must already be owned by ``_cleanup_on_exit`` so the marker
        is not duplicated by signal-handler re-entry.  This behavioral check is
        also what catches an EXIT registration that a physical-line scan sees
        only because it is hidden inside a heredoc.
        """

        def run_exit_case(inject_int: bool) -> tuple[int, str]:
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                event_log = root / "events.log"
                injected_marker = root / "injected.marker"
                script = (
                    "set -euo pipefail\n"
                    f"EVENT_LOG={shlex.quote(str(event_log))}\n"
                    f"INJECT_INT={int(inject_int)}\n"
                    f"INJECTED_MARKER={shlex.quote(str(injected_marker))}\n"
                    "export EVENT_LOG INJECT_INT INJECTED_MARKER\n"
                    "PG_TUNNEL_PREFLIGHT_PID=\"\"\n"
                    "PG_TUNNEL_PREFLIGHT_CONNINFO_DIR=\"\"\n"
                    "PG_TUNNEL_PREFLIGHT_PASSWORD_FILE=\"\"\n"
                    "PG_TUNNEL_ROLLBACK_ARMED=0\n"
                    "PG_TUNNEL_ROLLBACK_DIR=\"\"\n"
                    "PG_TUNNEL_ROLLBACK_JOB_LOADED=0\n"
                    "PG_TUNNEL_ROLLBACK_MANUAL_KIND=none\n"
                    "PG_TUNNEL_ROLLBACK_MANUAL_CONFIG=\"\"\n"
                    "PG_TUNNEL_ROLLBACK_WRAPPER_SOURCE=\"\"\n"
                    "ROLLBACK_ARMED=0\n"
                    "DEPLOY_OK=0\n"
                    "STAGED_BINARY=\"\"\n"
                    "POLICIES_STAGED=\"\"\n"
                    "LAUNCHD_MIGRATED_STAGED=\"\"\n"
                    "RELEASE_ROOT_SCRIPTS_STAGED=\"\"\n"
                    "DEPLOY_MKDIR_LOCK_DIR=\"\"\n"
                    "_rollback_release_binary() { :; }\n"
                    + self._production_cleanup_handlers()
                    + "\n_cleanup_owned_pg_tunnel_preflight() {\n"
                    + "    printf 'cleanup-enter\\n' >> \"$EVENT_LOG\"\n"
                    + "    if [ \"$INJECT_INT\" = 1 ] && [ ! -e \"$INJECTED_MARKER\" ]; then\n"
                    + "        : > \"$INJECTED_MARKER\"\n"
                    + "        kill -INT $$\n"
                    + "    fi\n"
                    + "}\n"
                    + "_finalize_detached_helper() {\n"
                    + "    printf 'cleanup-marker=%s\\n' \"$1\" >> \"$EVENT_LOG\"\n"
                    + "}\n"
                    + "exit 0\n"
                )
                process = subprocess.run(
                    ["bash", "-c", script], capture_output=True, text=True, timeout=10
                )
                events = (
                    event_log.read_text(encoding="utf-8")
                    if event_log.exists()
                    else ""
                )
                return process.returncode, events

        for inject_int in (False, True):
            with self.subTest(inject_int=inject_int):
                status, events = run_exit_case(inject_int)
                self.assertEqual(status, 0, events)
                self.assertEqual(events.count("cleanup-enter"), 1, events)
                self.assertEqual(events.count("cleanup-marker=0"), 1, events)

    def test_probe_uses_individual_libpq_environment_without_dsn_argv(self):
        block = self._pg_block()
        self.assertIn("local -a psql_env=(env -u DATABASE_URL)", block)
        self.assertIn('psql_env+=(-u "$name")', block)
        self.assertIn('psql_env+=("$name=$value")', block)
        self.assertIn('psql_env+=("PGCONNECT_TIMEOUT=5")', block)
        self.assertIn('psql_env+=("PGPASSFILE=$password_file")', block)
        self.assertIn('psql_env+=("PGPASSFILE=/dev/null")', block)
        self.assertIn("YAML.safe_load(File.read(config_path), aliases: true)", block)
        self.assertIn("RUBY\n        return $?\n    fi", block)
        self.assertNotIn("YAML.safe_load_file", block)
        self.assertNotIn('PGDATABASE="$(<', block)

    def _run_production_probe(self, database_url: str) -> tuple[int, str]:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fake_bin = root / "fake-bin"
            fake_bin.mkdir()
            event_log = root / "events.log"
            psql = fake_bin / "psql"
            psql.write_text(
                "#!/bin/sh\n"
                "printf 'timeout=%s\\n' \"${PGCONNECT_TIMEOUT:-missing}\" >> \"$EVENT_LOG\"\n"
                "exit 0\n",
                encoding="utf-8",
            )
            psql.chmod(0o755)
            deploy = DEPLOY.read_text(encoding="utf-8")
            start = deploy.index("_pg_write_probe_conninfo() {")
            end = deploy.index("_migrate_pg_tunnel_before_release_stop() {", start)
            helper = deploy[start:end]
            script = (
                "set -euo pipefail\n"
                f"ADK_REL={shlex.quote(str(root))}\n"
                f"DATABASE_URL={shlex.quote(database_url)}\n"
                f"EVENT_LOG={shlex.quote(str(event_log))}\n"
                f"PATH={shlex.quote(str(fake_bin))}:/usr/bin:/bin:/usr/sbin:/sbin\n"
                "PG_TUNNEL_PREFLIGHT_CONNINFO_DIR=\"\"\n"
                "PG_TUNNEL_PREFLIGHT_PASSWORD_FILE=\"\"\n"
                "export DATABASE_URL EVENT_LOG PATH\n"
                + helper
                + "\n_pg_sql_probe 25432\n"
            )
            p = subprocess.run(
                ["bash", "-c", script], capture_output=True, text=True, timeout=10
            )
            events = event_log.read_text(encoding="utf-8") if event_log.exists() else ""
            return p.returncode, events

    def test_probe_defaults_connect_timeout_when_dsn_omits_it(self):
        status, events = self._run_production_probe(
            "postgresql://agentdesk@db.internal/agentdesk"
        )
        self.assertEqual(status, 0, events)
        self.assertEqual(events, "timeout=5\n")

    def test_probe_preserves_explicit_connect_timeout(self):
        status, events = self._run_production_probe(
            "postgresql://agentdesk@db.internal/agentdesk?connect_timeout=17"
        )
        self.assertEqual(status, 0, events)
        self.assertEqual(events, "timeout=17\n")

    def test_production_ruby_parses_ipv6_escaped_uri_and_pgpass(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            output_dir = root / "conninfo"
            output_dir.mkdir()
            password_file = root / "pgpass"
            deploy = DEPLOY.read_text(encoding="utf-8")
            start = deploy.index("_pg_write_probe_conninfo() {")
            end = deploy.index("_pg_sql_probe() {", start)
            helper = deploy[start:end]
            uri = (
                "postgresql://user%3Aname:p%5C%3Aa%2Bss@"
                "[2001:db8::1]:5432/db%2Bname?"
                "sslmode=verify-full&application_name=a+b"
            )
            script = (
                "set -euo pipefail\n"
                f"ADK_REL={shlex.quote(str(root))}\n"
                f"DATABASE_URL={shlex.quote(uri)}\n"
                "export DATABASE_URL\n"
                + helper
                + f"\n_pg_write_probe_conninfo 25432 {shlex.quote(str(output_dir))} "
                f"{shlex.quote(str(password_file))}\n"
            )
            p = subprocess.run(
                ["bash", "-c", script], capture_output=True, text=True, timeout=10
            )
            self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
            expected = {
                "PGHOST": "2001:db8::1",
                "PGHOSTADDR": "127.0.0.1",
                "PGPORT": "25432",
                "PGUSER": "user:name",
                "PGDATABASE": "db+name",
                "PGSSLMODE": "verify-full",
                "PGAPPNAME": "a+b",
            }
            for name, value in expected.items():
                with self.subTest(name=name):
                    self.assertEqual(
                        (output_dir / name).read_text(encoding="utf-8"), value
                    )
                    self.assertEqual((output_dir / name).stat().st_mode & 0o777, 0o600)
            self.assertEqual(
                password_file.read_text(encoding="utf-8"),
                r"2001\:db8\:\:1:25432:db+name:user\:name:p\\\:a+ss" + "\n",
            )
            self.assertEqual(password_file.stat().st_mode & 0o777, 0o600)

    def test_production_ruby_without_password_leaves_pgpass_empty(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            output_dir = root / "conninfo"
            output_dir.mkdir()
            password_file = root / "pgpass"
            password_file.touch(mode=0o600)
            deploy = DEPLOY.read_text(encoding="utf-8")
            start = deploy.index("_pg_write_probe_conninfo() {")
            end = deploy.index("_pg_sql_probe() {", start)
            helper = deploy[start:end]
            script = (
                "set -euo pipefail\n"
                f"ADK_REL={shlex.quote(str(root))}\n"
                "DATABASE_URL=postgresql://agentdesk@db.internal/agentdesk\n"
                "export DATABASE_URL\n"
                + helper
                + f"\n_pg_write_probe_conninfo 25432 {shlex.quote(str(output_dir))} "
                f"{shlex.quote(str(password_file))}\n"
            )
            p = subprocess.run(
                ["bash", "-c", script], capture_output=True, text=True, timeout=10
            )
            self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
            self.assertEqual(password_file.read_bytes(), b"")

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

    def test_loaded_job_without_restorable_files_fails_before_bootout(self):
        with tempfile.TemporaryDirectory() as tmp:
            adk = Path(tmp) / "adk"
            for sub in ("bin", "config", "logs"):
                (adk / sub).mkdir(parents=True)
            (adk / "config/pg-tunnel.env").write_text(
                "PG_TUNNEL_SSH_TARGET=mac-mini\n", encoding="utf-8"
            )
            home = Path(tmp) / "home"
            home.mkdir()
            p, _, event_log = self._run_block(adk, home, job_loaded=True)
            self.assertNotEqual(p.returncode, 0)
            self.assertIn("lacks restorable wrapper/plist snapshots", p.stdout)
            events = event_log.read_text(encoding="utf-8")
            self.assertNotIn("launchctl bootout", events)
            self.assertNotIn("takeover", events)
            self.assertNotIn("release-stop", events)

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
            diagnostics = self._block_failure_diagnostics(p, home)
            self.assertNotEqual(p.returncode, 0, diagnostics)
            self.assertIn(
                "restore tcp", event_log.read_text(encoding="utf-8"), diagnostics
            )
            self.assertEqual(
                (home / "rollback-state.txt").read_text(encoding="utf-8"),
                "armed=0 backup=\n",
                diagnostics,
            )

    def test_rollback_waits_for_listener_absence_before_manual_restore(self):
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
                rollback_listener_checks=2,
                old_wrapper="#!/bin/sh\nexit 99\n",
                manual_kind="tcp",
            )
            events = event_log.read_text(encoding="utf-8")
            self.assertNotEqual(p.returncode, 0)
            rollback_bootout = events.rindex("launchctl bootout")
            second_listener = events.index("lsof ", events.index("lsof ", rollback_bootout) + 1)
            restore = events.index("restore tcp", second_listener)
            self.assertLess(rollback_bootout, second_listener)
            self.assertLess(second_listener, restore)

    def test_rollback_refuses_restore_while_new_listener_survives(self):
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
                fail_listener_absence=True,
                old_wrapper="#!/bin/sh\nexit 99\n",
                manual_kind="tcp",
            )
            events = event_log.read_text(encoding="utf-8")
            self.assertNotEqual(p.returncode, 0)
            self.assertIn("refusing restore bind race", p.stderr)
            self.assertNotIn("restore tcp", events)
            self.assertIn("Manual recovery", p.stderr)
            self._assert_rollback_material_retained(home)

    def test_snapshot_failure_does_not_touch_live_tunnel(self):
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
                fail_backup_copy=True,
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
            self.assertNotIn("launchctl bootout", events)
            self.assertNotIn("restore ", events)
            self.assertNotIn("release-stop", events)
            self.assertEqual(
                (home / "rollback-state.txt").read_text(encoding="utf-8"),
                "armed=0 backup=\n",
            )

    def test_manual_state_snapshot_failure_does_not_arm_or_install(self):
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
                fail_kind_snapshot=True,
                old_wrapper="old-wrapper\n",
            )
            self.assertNotEqual(p.returncode, 0)
            self.assertIn("Failed to snapshot existing manual PG tunnel state", p.stdout)
            self.assertEqual(
                (adk / "bin/pg-tunnel.sh").read_text(encoding="utf-8"),
                "old-wrapper\n",
            )
            events = event_log.read_text(encoding="utf-8")
            self.assertNotIn("launchctl bootout", events)
            self.assertNotIn("launchctl bootstrap", events)
            self.assertNotIn("release-stop", events)
            self.assertEqual(
                (home / "rollback-state.txt").read_text(encoding="utf-8"),
                "armed=0 backup=\n",
            )

    def test_rollback_bootstrap_failure_retains_recovery_material(self):
        with tempfile.TemporaryDirectory() as tmp:
            adk = Path(tmp) / "adk"
            for sub in ("bin", "config", "logs"):
                (adk / sub).mkdir(parents=True)
            (adk / "config/pg-tunnel.env").write_text(
                "PG_TUNNEL_SSH_TARGET=mac-mini\n", encoding="utf-8"
            )
            home = Path(tmp) / "home"
            home.mkdir()
            p, _, _ = self._run_block(
                adk,
                home,
                fail_canonical=True,
                fail_restore_bootstrap=True,
                job_loaded=True,
                old_wrapper="old-wrapper\n",
                old_plist="old-plist\n",
            )
            self.assertNotEqual(p.returncode, 0)
            self.assertIn("Failed to restart previous PG tunnel launchd job", p.stderr)
            self._assert_rollback_material_retained(home)

    def test_manual_restore_failure_retains_recovery_material(self):
        with tempfile.TemporaryDirectory() as tmp:
            adk = Path(tmp) / "adk"
            for sub in ("bin", "config", "logs"):
                (adk / sub).mkdir(parents=True)
            (adk / "config/pg-tunnel.env").write_text(
                "PG_TUNNEL_SSH_TARGET=mac-mini\n", encoding="utf-8"
            )
            home = Path(tmp) / "home"
            home.mkdir()
            p, _, _ = self._run_block(
                adk,
                home,
                fail_canonical=True,
                fail_manual_restore=True,
                old_wrapper="old-wrapper\n",
                manual_kind="tcp",
            )
            self.assertNotEqual(p.returncode, 0)
            self.assertIn("Failed to restart previous manual PG tunnel", p.stderr)
            self._assert_rollback_material_retained(home)

    def test_new_node_rollback_accepts_restored_listener_absence(self):
        with tempfile.TemporaryDirectory() as tmp:
            adk = Path(tmp) / "adk"
            for sub in ("bin", "config", "logs"):
                (adk / sub).mkdir(parents=True)
            (adk / "config/pg-tunnel.env").write_text(
                "PG_TUNNEL_SSH_TARGET=mac-mini\n", encoding="utf-8"
            )
            home = Path(tmp) / "home"
            home.mkdir()
            p, _, event_log = self._run_block(adk, home, fail_canonical=True)
            self.assertNotEqual(p.returncode, 0)
            self.assertEqual(
                (home / "rollback-state.txt").read_text(encoding="utf-8"),
                "armed=0 backup=\n",
            )
            events = event_log.read_text(encoding="utf-8")
            self.assertIn("lsof -nP -a -iTCP@127.0.0.1:15432 -sTCP:LISTEN", events)

    def test_new_node_rollback_retains_material_if_listener_survives(self):
        with tempfile.TemporaryDirectory() as tmp:
            adk = Path(tmp) / "adk"
            for sub in ("bin", "config", "logs"):
                (adk / sub).mkdir(parents=True)
            (adk / "config/pg-tunnel.env").write_text(
                "PG_TUNNEL_SSH_TARGET=mac-mini\n", encoding="utf-8"
            )
            home = Path(tmp) / "home"
            home.mkdir()
            p, _, _ = self._run_block(
                adk, home, fail_canonical=True, fail_listener_absence=True
            )
            self.assertNotEqual(p.returncode, 0)
            self.assertIn("listener survived rollback bootout", p.stderr)
            self._assert_rollback_material_retained(home)

    def test_rollback_readiness_failure_retains_recovery_material(self):
        with tempfile.TemporaryDirectory() as tmp:
            adk = Path(tmp) / "adk"
            for sub in ("bin", "config", "logs"):
                (adk / sub).mkdir(parents=True)
            (adk / "config/pg-tunnel.env").write_text(
                "PG_TUNNEL_SSH_TARGET=mac-mini\n", encoding="utf-8"
            )
            home = Path(tmp) / "home"
            home.mkdir()
            p, _, _ = self._run_block(
                adk,
                home,
                fail_canonical=True,
                fail_rollback_readiness=True,
                old_wrapper="old-wrapper\n",
                manual_kind="tcp",
            )
            self.assertNotEqual(p.returncode, 0)
            self.assertIn("did not become SQL-ready", p.stderr)
            self._assert_rollback_material_retained(home)

    def test_launchd_rollback_readiness_covers_throttle_window(self):
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
                rollback_ready_attempt=45,
                job_loaded=True,
                old_wrapper="old-wrapper\n",
                old_plist="old-plist\n",
            )
            events = event_log.read_text(encoding="utf-8")
            diagnostics = self._block_failure_diagnostics(p, home)
            self.assertNotEqual(p.returncode, 0, diagnostics)
            rollback_count = home / "rollback-psql.count"
            if not rollback_count.is_file():
                self.fail(diagnostics)
            rollback_attempts = int(rollback_count.read_text(encoding="utf-8"))
            self.assertEqual(rollback_attempts, 45, events + "\n" + diagnostics)
            self.assertEqual(
                (home / "rollback-state.txt").read_text(encoding="utf-8"),
                "armed=0 backup=\n",
                diagnostics,
            )

    @staticmethod
    def _assert_rollback_material_retained(home: Path) -> None:
        state = (home / "rollback-state.txt").read_text(encoding="utf-8").strip()
        armed, backup = state.split(" backup=", 1)
        if armed != "armed=1":
            raise AssertionError(state)
        backup_path = Path(backup)
        if not backup_path.is_dir():
            raise AssertionError(f"rollback backup missing: {backup_path}")
        if not (backup_path / "config").is_file():
            raise AssertionError(f"rollback config missing: {backup_path}")

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
            self.assertIn(
                "psql host=db.internal hostaddr=127.0.0.1 port=15432 "
                "user=agentdesk db=agentdesk sslmode=require timeout=5 passfile=set",
                events,
                p.stdout + p.stderr + events,
            )
            probe_sql = events.index("psql host=db.internal hostaddr=127.0.0.1 port=")
            cleanup = events.index("probe-term", probe_sql)
            takeover = events.index("takeover", cleanup)
            bootstrap = events.index("launchctl bootstrap", takeover)
            canonical_sql = events.index("port=15432", bootstrap)
            stop = events.index("release-stop", canonical_sql)
            self.assertLess(probe_sql, cleanup)
            self.assertLess(cleanup, takeover)
            self.assertLess(takeover, bootstrap)
            self.assertLess(bootstrap, canonical_sql)
            self.assertLess(canonical_sql, stop)


if __name__ == "__main__":
    unittest.main()
