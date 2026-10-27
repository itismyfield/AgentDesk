"""Offline tests for the opt-in mid-turn provider SIGKILL scenario."""

from __future__ import annotations

import contextlib
import itertools
import json
import signal
import sys
import tempfile
import unittest
from argparse import Namespace
from pathlib import Path
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts" / "e2e"))

import run_tui_relay as driver  # noqa: E402
from tui_relay import assertions, midturn_kill  # noqa: E402

CHANNEL = "41"
CLAUDE_ARGS = "/Users/u/.local/bin/claude --dangerously-skip-permissions --session-id s1"
CODEX_WRAPPER = "node /opt/homebrew/bin/codex --dangerously-bypass-hook-trust"
CODEX_NATIVE = "/opt/homebrew/lib/node_modules/@openai/codex/vendor/aarch64-apple-darwin/bin/codex --dangerously-bypass-hook-trust"
STARTED = "Tue Oct 27 15:09:20 2026"


class Host:
    """Runtime session records, tmux panes and a process table for one tmux E2E cell."""

    def __init__(self, tmp: Path, cell: str, *, channel: str = CHANNEL):
        self.cell, self.provider = cell, driver.cell_provider(cell)
        self.session = driver.cell_session_name(cell)
        self.runtime = tmp / "release" / "runtime"
        sessions = self.runtime / "sessions"
        sessions.mkdir(parents=True)
        (self.runtime / "dcserver.pid").write_text("7")
        self.base = sessions / f"agentdesk-abc-host-{self.session}"
        self.binding = self.runtime / "binding_contexts" / self.provider / "b1.json"
        self.binding.parent.mkdir(parents=True)
        self.write_binding(channel=channel)
        Path(f"{self.base}.owner").write_text(str(self.runtime.parent))
        Path(f"{self.base}.runtime-kind").write_text(f"{self.provider}_tui")
        script = ".claude-tui.sh" if self.provider == "claude" else ".sh"
        Path(f"{self.base}{script}").write_text(
            f"unset AGENTDESK_BINDING_CONTEXT\nexport AGENTDESK_BINDING_CONTEXT='{self.binding}'\nexec x\n")
        if self.provider == "claude":
            self.procs = {100: midturn_kill.Proc(100, 9, "S", STARTED, CLAUDE_ARGS)}
            self.children = {100: [300]}
            self.target = 100
        else:
            self.procs = {100: midturn_kill.Proc(100, 9, "S", STARTED, CODEX_WRAPPER),
                          101: midturn_kill.Proc(101, 100, "S", STARTED, CODEX_NATIVE)}
            self.children = {100: [300, 101]}
            self.target = 101
        self.procs[300] = midturn_kill.Proc(300, 100, "S", STARTED, "npm exec @modelcontextprotocol/server-brave-search")
        self.panes = [midturn_kill.Pane("%1", 100, False)]
        self.killed: list[tuple[int, int]] = []
        self.on_kill = None

    def write_binding(self, **overrides):
        binding = {"provider": self.provider, "tmux_session": self.session, "channel_id": int(CHANNEL),
                   "owner_runtime_root": str(self.runtime.parent), "created_at": "2026-10-27T06:00:00Z", **overrides}
        self.binding.write_text(json.dumps(binding))

    def kill(self, pid, sig):
        self.killed.append((pid, sig))
        self.procs.pop(pid, None)
        if self.on_kill:
            self.on_kill()

    def respawn(self, pid=500):
        args = CLAUDE_ARGS if self.provider == "claude" else CODEX_NATIVE
        self.procs[pid] = midturn_kill.Proc(pid, 9, "S", "Tue Oct 27 16:00:00 2026", args)
        self.children = {}
        self.panes = [midturn_kill.Pane("%2", pid, False)]

    @contextlib.contextmanager
    def active(self):
        with patch.object(midturn_kill, "_ps", side_effect=lambda pid: self.procs.get(pid)), \
             patch.object(midturn_kill, "_children", side_effect=lambda pid: list(self.children.get(pid, []))), \
             patch.object(midturn_kill, "_panes", side_effect=lambda session: list(self.panes) if session == self.session else []), \
             patch.object(midturn_kill, "_tmux_server_pid", return_value=9), \
             patch.object(midturn_kill, "_kill", side_effect=self.kill):
            yield

    def identify(self, channel=CHANNEL):
        with self.active():
            return midturn_kill.identify(driver, self.cell, channel, self.runtime)


class HostCase(unittest.TestCase):
    def host(self, cell, **kwargs):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        return Host(Path(tmp.name), cell, **kwargs)


class Identification(HostCase):
    def test_claude_cell_targets_the_pane_provider(self):
        identity = self.host("claude-tui").identify()
        self.assertEqual((identity["pid"], identity["args"]), (100, CLAUDE_ARGS))

    def test_codex_cell_targets_the_native_child_not_the_node_wrapper(self):
        identity = self.host("codex-tui").identify()
        self.assertEqual((identity["pid"], identity["ppid"], identity["wrapper_pid"]), (101, 100, 100))

    def test_wrapper_without_exactly_one_provider_child_is_refused(self):
        host = self.host("codex-tui")
        host.procs[102] = midturn_kill.Proc(102, 100, "S", STARTED, CODEX_NATIVE)
        host.children[100].append(102)
        with self.assertRaisesRegex(midturn_kill.Refused, "provider child"):
            host.identify()
        host.children[100] = [300]
        with self.assertRaisesRegex(midturn_kill.Refused, "provider child"):
            host.identify()

    def test_a_session_bound_to_another_channel_or_session_is_refused(self):
        host = self.host("codex-tui")
        with self.assertRaisesRegex(midturn_kill.Refused, "binding"):
            host.identify(channel="42")
        for override in ({"tmux_session": "AgentDesk-codex-adk-cdx"}, {"owner_runtime_root": "/other"},
                         {"provider": "claude"}):
            with self.subTest(override=override):
                host.write_binding(**override)
                with self.assertRaisesRegex(midturn_kill.Refused, "binding"):
                    host.identify()

    def test_non_e2e_and_non_tmux_cells_are_refused(self):
        host = self.host("codex-tui")
        with patch.object(driver, "cell_default_agent", return_value="adk-codex-tui"):
            with self.assertRaisesRegex(midturn_kill.Refused, "non-E2E"):
                host.identify()
        for cell in ("codex-herdr", "claude-herdr", "codex-pipe", "claude-pipe"):
            with self.subTest(cell=cell), self.assertRaisesRegex(midturn_kill.Refused, "tmux provider"):
                midturn_kill.identify(driver, cell, CHANNEL, host.runtime)

    def test_dead_panes_and_protected_processes_are_refused(self):
        host = self.host("claude-tui")
        host.panes = [midturn_kill.Pane("%1", 100, True)]
        with self.assertRaisesRegex(midturn_kill.Refused, "pane"):
            host.identify()
        host.panes = [midturn_kill.Pane("%1", 100, False)]
        (host.runtime / "dcserver.pid").write_text("100")
        with self.assertRaisesRegex(midturn_kill.Refused, "protected"):
            host.identify()


def _mid_turn_record(host):
    record = {"provider_hold_states": [{"classification": "provider_hold_observed"}]}
    with host.active():
        midturn_kill.run_step(driver, {"identify_provider_process": {}}, cell=host.cell, channel_id=CHANNEL,
                              args=_args(host), record=record, window=None, refresh=None)
    return record


def _args(host, *, flag=True):
    return Namespace(midturn_kill=flag, base_url="http://offline.invalid", queue_runtime_root=str(host.runtime))


class KillReverification(HostCase):
    def kill(self, host, record, *, busy=("agent_turn_status=active",), flag=True):
        with host.active(), patch.object(midturn_kill, "_mailbox_state", return_value=(1, list(busy))), \
             patch.object(midturn_kill.time, "sleep"):
            midturn_kill.run_step(driver, {"kill_provider_process": {"ok_marker": "[OK]"}}, cell=host.cell,
                                  channel_id=CHANNEL, args=_args(host, flag=flag), record=record, window=None, refresh=None)

    def test_kill_signals_only_the_reverified_provider_pid(self):
        for cell in ("claude-tui", "codex-tui"):
            with self.subTest(cell=cell):
                host = self.host(cell)
                record = _mid_turn_record(host)
                self.kill(host, record)
                self.assertEqual(host.killed, [(host.target, signal.SIGKILL)])
                self.assertEqual(record["midturn_kill"]["capabilities"]["provider_killed"], "pass")

    def test_reused_pid_or_changed_session_is_refused_without_a_signal(self):
        changes = {
            "pid reused": lambda h: h.procs.__setitem__(h.target, h.procs[h.target].__class__(
                h.target, h.procs[h.target].ppid, "S", "Tue Oct 27 17:00:00 2026", h.procs[h.target].args)),
            "cmdline changed": lambda h: h.procs.__setitem__(h.target, h.procs[h.target].__class__(
                h.target, h.procs[h.target].ppid, "S", STARTED, "/usr/bin/python3 other.py")),
            "pane replaced": lambda h: setattr(h, "panes", [midturn_kill.Pane("%9", 100, False)]),
            "rebound": lambda h: h.write_binding(channel_id=99),
        }
        for label, change in changes.items():
            with self.subTest(change=label):
                host = self.host("codex-tui")
                record = _mid_turn_record(host)
                change(host)
                with self.assertRaises(midturn_kill.Refused):
                    self.kill(host, record)
                self.assertEqual(host.killed, [])

    def test_kill_requires_the_flag_and_a_live_mid_turn(self):
        host = self.host("claude-tui")
        cases = {
            "flag": dict(flag=False),
            "idle mailbox": dict(busy=()),
        }
        for label, kwargs in cases.items():
            with self.subTest(case=label):
                record = _mid_turn_record(host)
                with self.assertRaises(midturn_kill.Refused):
                    self.kill(host, record, **kwargs)
        record = _mid_turn_record(host)
        record["provider_hold_states"] = [{"classification": "fast_terminal_completion_before_hold"}]
        with self.assertRaisesRegex(midturn_kill.Refused, "mid-turn"):
            self.kill(host, record)
        self.assertEqual(host.killed, [])

    def test_a_provider_that_survives_the_signal_fails(self):
        host = self.host("claude-tui")
        record = _mid_turn_record(host)
        survivor = host.procs[100]
        host.on_kill = lambda: host.procs.__setitem__(100, survivor)
        with patch.object(midturn_kill.time, "monotonic", side_effect=itertools.count(0.0, 5.0)):
            with self.assertRaisesRegex(assertions.AssertionError, "survived"):
                self.kill(host, record)


def _msg(mid, content, author):
    return {"id": str(mid), "content": content, "author": {"id": author, "bot": True}, "type": 0}


class SettlementJudgment(unittest.TestCase):
    CLAUDE = assertions.provider_bot_id("claude")

    def judge(self, rows, *, busy=(), health_error=None, deleted=()):
        return midturn_kill.judge(rows, deleted_ids=set(deleted), exclude_ids=set(), mailbox_busy=list(busy),
                                  health_error=health_error, marker="[OK]", provider_author=self.CLAUDE)

    def test_a_live_processing_placeholder_is_stuck_even_when_the_mailbox_is_idle(self):
        body = _msg(20, "[OK]", self.CLAUDE)
        for live in ("⠋ 계속 처리 중", "-# ⠸ 진행 중 — tool", "Processing...", "…", "-# ⠸ 도구 실행 중 (Bash)"):
            with self.subTest(live=live):
                verdict = self.judge([body, _msg(21, live, self.CLAUDE)])
                self.assertEqual(verdict["turn"], "stuck")
        self.assertEqual(self.judge([body, _msg(21, "-# ⚠️ 오류 · provider exited", self.CLAUDE)])["turn"], "settled")
        self.assertEqual(self.judge([body, _msg(21, "⠋ 계속 처리 중", self.CLAUDE)], deleted={"21"})["turn"], "settled")

    def test_a_busy_mailbox_or_unreadable_health_is_not_settled(self):
        body = _msg(20, "[OK]", self.CLAUDE)
        self.assertEqual(self.judge([body], busy=["inflight_state_present=true"])["turn"], "stuck")
        self.assertEqual(self.judge([body], health_error="URLError")["turn"], "stuck")
        self.assertEqual(self.judge([body])["turn"], "settled")

    def test_pre_kill_output_counts_only_a_provider_authored_body(self):
        notify = "1481522187197218816"
        for rows in ([_msg(20, "Reply with [OK]", assertions.OUR_BOT_ID)], [_msg(20, "[OK]", notify)],
                     [_msg(20, "-# ⌨️ [OK]", self.CLAUDE)]):
            with self.subTest(rows=rows):
                self.assertEqual(self.judge(rows)["pre_kill_output"], "lost")
        self.assertEqual(self.judge([_msg(20, "[OK]", self.CLAUDE)])["pre_kill_output"], "delivered")
        self.assertEqual(self.judge([_msg(20, "[OK]", self.CLAUDE)], deleted={"20"})["pre_kill_output"], "lost")


class Preflight(HostCase):
    def preflight(self, host, busy=()):
        with patch.object(midturn_kill, "_mailbox_state", return_value=(1, list(busy))):
            record = {}
            midturn_kill.run_step(driver, {"preflight_e2e_channel": {}}, cell=host.cell, channel_id=CHANNEL,
                                  args=_args(host), record=record, window=None, refresh=None)
            return record["midturn_kill"]["preflight"]

    def test_the_newest_session_binding_must_name_the_channel_before_any_send(self):
        host = self.host("codex-tui")
        old = host.binding.with_name("b0.json")
        old.write_text(json.dumps({"tmux_session": host.session, "channel_id": 99, "created_at": "2026-10-01T00:00:00Z",
                                   "owner_runtime_root": str(host.runtime.parent)}))
        self.assertEqual(self.preflight(host)["binding_context"], str(host.binding))
        old.write_text(json.dumps({"tmux_session": host.session, "channel_id": 99, "created_at": "2026-10-28T00:00:00Z",
                                   "owner_runtime_root": str(host.runtime.parent)}))
        with self.assertRaisesRegex(midturn_kill.Refused, "binding"):
            self.preflight(host)
        old.unlink()
        host.binding.unlink()
        with self.assertRaisesRegex(midturn_kill.Refused, "binding"):
            self.preflight(host)

    def test_a_busy_cell_is_refused_before_any_send(self):
        with self.assertRaisesRegex(midturn_kill.Refused, "not idle"):
            self.preflight(self.host("claude-tui"), busy=["queue_depth=1"])


class SettlementPolling(HostCase):
    def test_one_idle_poll_between_busy_polls_does_not_settle_the_turn(self):
        host = self.host("claude-tui")
        window = assertions.Window(setup_marker_id="1")
        window.add(_msg(20, "[OK]", assertions.provider_bot_id("claude")))
        polls = iter([(1, [])] + [(1, ["agent_turn_status=active"])] * 1000)
        clock = [0.0]
        with patch.object(midturn_kill, "_mailbox_state", side_effect=lambda *a: next(polls)), \
             patch.object(midturn_kill.time, "monotonic", side_effect=lambda: clock[0]), \
             patch.object(midturn_kill.time, "sleep", side_effect=lambda t: clock.__setitem__(0, clock[0] + t)), \
             self.assertRaisesRegex(assertions.AssertionError, "did not settle"):
            midturn_kill.run_step(driver, {"await_killed_turn_settled": {"ok_marker": "[OK]", "timeout_s": 60}},
                                  cell="claude-tui", channel_id=CHANNEL, args=_args(host), record={},
                                  window=window, refresh=lambda: None)


class Replacement(unittest.TestCase):
    KILLED = {"pid": 101, "started": STARTED}

    def test_the_next_turn_needs_a_new_provider_and_a_body_after_the_follow_up(self):
        new = {"pid": 500, "started": "Tue Oct 27 16:00:00 2026"}
        self.assertEqual(midturn_kill.replacement(self.KILLED, new, body_ids=[31], followup_id=30)["status"], "pass")
        same = {"pid": 101, "started": STARTED}
        with self.assertRaisesRegex(assertions.AssertionError, "same provider"):
            midturn_kill.replacement(self.KILLED, same, body_ids=[31], followup_id=30)
        with self.assertRaisesRegex(assertions.AssertionError, "after the follow-up"):
            midturn_kill.replacement(self.KILLED, new, body_ids=[29], followup_id=30)
        reused_pid = {"pid": 101, "started": "Tue Oct 27 16:00:00 2026"}
        self.assertEqual(midturn_kill.replacement(self.KILLED, reused_pid, body_ids=[31], followup_id=30)["status"], "pass")


class Gating(unittest.TestCase):
    def test_yaml_scenarios_cannot_carry_the_kill_steps(self):
        for key in midturn_kill.STEPS:
            with self.subTest(step=key), self.assertRaises(ValueError):
                driver.validate_scenario_schema({"steps": [{key: {}}], "assertions": []})

    def test_default_runs_never_load_the_kill_scenario(self):
        for cell in driver.SUPPORTED_CELLS:
            for scenario in driver.load_scenarios(ROOT / "tests/e2e/tui_relay/scenarios", cell=cell):
                self.assertFalse(any(midturn_kill.owns(step) for step in scenario.get("steps") or []))
        scenario = midturn_kill.scenario("r" * 32, "codex-tui")
        self.assertTrue(driver.is_destructive(scenario))
        self.assertTrue(any("kill_provider_process" in step for step in scenario["steps"]))

    def test_cli_accepts_only_tmux_cells_without_a_filter(self):
        base = ["run_tui_relay.py", "--channel-id", CHANNEL, "--midturn-kill"]
        for extra in (["--cell", "codex-herdr"], ["--cell", "claude-pipe"], ["--cell", "codex-tui", "--filter", "E-1"]):
            with self.subTest(extra=extra), patch.object(sys, "argv", base + extra), \
                 patch("sys.stderr"), self.assertRaises(SystemExit):
                driver.parse_args()
        with patch.object(sys, "argv", base + ["--cell", "codex-tui"]):
            parsed = driver.parse_args()
        # The force-cancel reset would tear down the idle tmux session before the kill.
        self.assertEqual((parsed.midturn_kill, parsed.reset_before_each), (True, False))


class DriverRun(HostCase):
    """The whole scenario through run_scenario: dispatch, report keys and verdicts."""

    def run_cell(self, cell, *, settle=True, deliver=True):
        host = self.host(cell)
        bot = assertions.provider_bot_id(host.provider)
        rows, ids = [], iter(range(10, 1000))
        scenario = midturn_kill.scenario("r" * 32, cell)
        step = {key: value for item in scenario["steps"] for key, value in item.items()}
        ok = step["wait_for_provider_hold_state"]["ok_marker"]
        nxt = [s["send_prompt"] for s in scenario["steps"] if "send_prompt" in s][-1].rsplit(" ", 1)[-1]
        held = []

        def add(content):
            rows.append(_msg(next(ids), content, bot))

        def send_prompt(channel, prompt, **kwargs):
            sent = next(ids)
            if ok in prompt:
                held.append(sent)
                add("⠋ 계속 처리 중")
                if deliver:
                    add(ok)
            else:
                host.respawn()
                add(nxt)
            return {"id": str(sent), "message_id": str(sent)}

        def on_kill():
            if settle:
                rows[0]["content"] = "-# ⚠️ 오류 · provider exited"
        host.on_kill = on_kill
        client = MagicMock(base_url="http://offline.invalid")
        client.send_control.side_effect = lambda channel, content: {"id": str(next(ids))}
        client.send_prompt.side_effect = send_prompt
        client.fetch_messages.side_effect = lambda channel, after_id=None, limit=100: [
            dict(r) for r in rows if int(r["id"]) > int(after_id or 0)]
        args = Namespace(cell=cell, channel_id=CHANNEL, dry_run=False, base_url="http://offline.invalid",
                         reset_before_each=False, hard_reset_session_each=False, allow_destructive=True,
                         queue_runtime_root=str(host.runtime), final_refetches=1, thread_channel_id=None,
                         midturn_kill=True)
        clock = [0.0]
        mailbox = lambda *a, **kw: (1, ["agent_turn_status=active"] if held and not host.killed else [])  # noqa: E731
        with host.active(), patch.dict("os.environ", {"AGENTDESK_E2E_ALLOW_DESTRUCTIVE": "1"}), \
             patch.object(driver.time, "monotonic", side_effect=lambda: clock[0]), \
             patch.object(driver.time, "sleep", side_effect=lambda t: clock.__setitem__(0, clock[0] + t)), \
             patch.object(driver, "wait_for_provider_hold_state", return_value={"classification": "provider_hold_observed"}), \
             patch.object(midturn_kill, "_mailbox_state", side_effect=mailbox), \
             patch.object(driver, "assert_cell_idle", return_value={"status": "idle"}), \
             patch.object(driver, "_raise_if_tui_prompt_stuck_while_idle"), \
             patch.object(driver, "_collect_wait_timeout_diagnostics", return_value={}):
            result = driver.run_scenario(scenario, args=args, client=client, run_id="r" * 32)
        return host, result

    def test_a_settled_kill_with_a_new_provider_passes(self):
        for cell in ("claude-tui", "codex-tui"):
            with self.subTest(cell=cell):
                host, result = self.run_cell(cell)
                self.assertEqual(result["status"], "pass", result.get("reason"))
                self.assertEqual(host.killed, [(host.target, signal.SIGKILL)])
                self.assertEqual(result["midturn_kill"]["capabilities"], {
                    "provider_killed": "pass", "turn_settled": "pass", "pre_kill_output_delivered": "pass",
                    "next_turn_new_provider": "pass"})

    def test_a_placeholder_left_processing_fails_as_stuck(self):
        host, result = self.run_cell("codex-tui", settle=False)
        self.assertEqual(result["status"], "fail")
        self.assertIn("did not settle", result["reason"])
        self.assertEqual(result["midturn_kill"]["capabilities"]["turn_settled"], "fail")
        self.assertEqual(len(host.killed), 1)

    def test_pre_kill_output_that_never_reaches_discord_fails_as_lost(self):
        _, result = self.run_cell("claude-tui", deliver=False)
        self.assertEqual(result["status"], "fail")
        self.assertIn("provider bot", result["reason"])
        self.assertEqual(result["midturn_kill"]["capabilities"]["pre_kill_output_delivered"], "fail")


if __name__ == "__main__":
    unittest.main()
