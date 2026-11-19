"""Release-host supervisor, smoke driver, transcript usage and the deploy hook of the continuous runner."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO))
from scripts.e2e.continuous import (channel_lock, consumer, plan, publication, release_host, runner,  # noqa: E402
                                    smoke_driver, summary, turn_gate, usage)
from scripts.e2e.continuous.store import read_json, write_json  # noqa: E402

HEAD = "c" * 40
DETAIL = {"boot_incarnation": "boot-1", "herdr": {"configured_channels": ["20"]},
          "release_source": {"node_hostname": "host", "deployed_repo_head": HEAD}}
RESOURCE = {"guild": "1", "parents": ["20", "30"], "bot_set": ["4"]}
CONTRACT = {"busy_basis": None, "placement": None, "operation": "clear_and_turns", "occurrence": 1}
SESSION = "AgentDesk-claude-adk-claude-tui-2-e2e"


def lines(path, *rows):
    with open(path, "a", encoding="utf-8") as stream:
        stream.writelines(json.dumps(row) + "\n" for row in rows)


def asked(marker):
    return {"type": "user", "message": {"role": "user", "content": f"say {marker}"}}


def claude(message, stop="end_turn", **used):
    used = {"input_tokens": 1, "output_tokens": 2, "cache_read_input_tokens": 3, "cache_creation_input_tokens": 4, **used}
    return {"type": "assistant", "message": {"id": message, "stop_reason": stop, "usage": used,
                                             "content": [{"type": "text", "text": "ok"}]}}


def codex(kind, **payload):
    if kind == "total":
        payload = {"type": "token_count", "info": {"total_token_usage": payload}}
    elif kind == "asked":
        payload = {"type": "message", "role": "user", "content": [{"type": "input_text", "text": payload["marker"]}]}
    return {"type": "response_item" if kind == "asked" else "event_msg", "payload": payload or {"type": kind}}


class Fixture(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="continuous-release-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        for name in ("binary", "server.log", "deploy.lock", "transcript.jsonl"):
            (self.root / name).write_text("seed\n" if name == "server.log" else "")
        self.path = self.root / "config.json"
        self.config = {
            "enabled": True, "state_dir": str(self.root / "state"), "resource": RESOURCE, "core_interval_s": 100,
            "driver": [sys.executable, str(self.root / "driver.py")], "occurrence_contracts": {"E-50": CONTRACT, "E-51": CONTRACT},
            "release_host": {"base_url": "http://unused.test", "binary": str(self.root / "binary"),
                             "runtime_root": str(self.root), "server_log": str(self.root / "server.log"),
                             "deploy_lock": str(self.root / "deploy.lock"),
                             "cells": {"claude-herdr": {"smoke_cell": "claude-tui", "channel_id": "20", "deadline_s": 10,
                                                        "turn_tokens": 100, "usage_wait_s": 0}}}}
        self.save()
        observed = mock.patch.object(release_host, "detail", return_value=DETAIL)
        self.detail = observed.start()
        self.addCleanup(observed.stop)

    def save(self, **changes):
        self.config.update(changes)
        self.path.write_text(json.dumps(self.config))


class Usage(Fixture):
    def setUp(self):
        super().setUp()
        self.log = self.root / "transcript.jsonl"

    def test_claude_turn_is_billed_once_from_its_own_bytes_after_its_answer_finished(self):
        lines(self.log, asked("T0"), claude("old", input_tokens=900))
        mark = usage.mark(self.log)
        lines(self.log, asked("T1"), claude("m1", stop="tool_use", output_tokens=1), claude("m1", stop="tool_use", output_tokens=5))
        self.assertIsNone(usage.turn("claude", [mark], "T1"), "an answer still in flight is not a bill")
        lines(self.log, claude("m2"))
        self.assertEqual(usage.turn("claude", [mark], "T1"),
                         {"input_tokens": 2, "output_tokens": 7, "cache_read_tokens": 6, "cache_write_tokens": 8})
        self.assertIsNone(usage.turn("claude", [mark], "T2"), "usage without this turn's input belongs to another turn")
        mark = usage.mark(self.log)
        lines(self.log, asked("T2"), {"type": "assistant", "message": {"id": "m3", "stop_reason": "end_turn", "usage": {}}})
        self.assertIsNone(usage.turn("claude", [mark], "T2"), "a missing counter is not zero")

    def test_codex_turn_is_the_session_total_after_the_task_minus_the_total_before_it(self):
        lines(self.log, codex("total", input_tokens=100, cached_input_tokens=40, output_tokens=10))
        mark = usage.mark(self.log)
        lines(self.log, codex("asked", marker="T1"), codex("task_complete"))
        self.assertIsNone(usage.turn("codex", [mark], "T1"), "a finished task without its token count")
        lines(self.log, codex("asked", marker="T1"), codex("total", input_tokens=300, cached_input_tokens=140, output_tokens=25))
        self.assertIsNone(usage.turn("codex", [mark], "T1"), "a token count before the task finished")
        lines(self.log, codex("task_complete"))
        self.assertEqual(usage.turn("codex", [mark], "T1"),
                         {"input_tokens": 100, "output_tokens": 15, "cache_read_tokens": 100, "cache_write_tokens": 0})
        lines(self.log, codex("asked", marker="T2"), codex("total", input_tokens=400, output_tokens=30), codex("task_complete"))
        self.assertIsNone(usage.turn("codex", [usage.mark(self.root / "binary"), {"path": str(self.log), "offset": 0}], "T2"))

    def test_a_turn_answered_in_a_new_session_adds_nothing_from_the_session_it_left(self):
        lines(self.log, asked("T1"), claude("m1"))
        fresh = self.root / "fresh.jsonl"
        lines(fresh, asked("T2"), claude("m2"))
        marks = [usage.mark(self.log), {"path": str(fresh), "offset": 0}]
        self.assertEqual(sum(usage.turn("claude", marks, "T2").values()), 10)
        self.assertIsNone(usage.turn("claude", [*marks, {"path": str(self.root / "absent.jsonl"), "offset": 0}], "T2"))


class Posted:
    """Stands in for the HTTP transport under the real Discord client and records what was posted."""
    def __init__(self):
        self.sent = []

    def __call__(self, request, timeout=None):
        self.sent.append(json.loads(request.data)["content"])
        return self

    def __enter__(self):
        return self

    def __exit__(self, *_error):
        return False

    def read(self):
        return b"{}"


class Driver(Fixture):
    def drive(self, budget=None, lease=True, harness=None, cell="claude-herdr", idle=0, answers=(), prompts=3):
        """Run the driver entry point around a fake harness; `answers[i]` makes turn i 'silent' or 'huge'."""
        calls, posted, smoke = [], Posted(), smoke_driver._smoke()[0]  # noqa: SLF001
        discord, transcript, self.writer = smoke.cell_driver.discord, self.root / "transcript.jsonl", None

        def spawn(argv, **_kwargs):
            calls.append(argv)
            return subprocess.CompletedProcess(argv, idle, stdout="busy: turn" if idle else "idle")

        def main():
            run_id, out = sys.argv[sys.argv.index("--run-id") + 1], Path(sys.argv[sys.argv.index("--output") + 1])
            markers = smoke.expected_markers(smoke_driver.SCENARIOS, "E-50", run_id)
            row = {"id": "E-50", "run_id": run_id, "cell": "claude-tui", "channel_id": "20", "status": "pass",
                   "real_provider_contacted": True, "marker_counts": dict.fromkeys(markers)}
            state, client = {"row": row, "rc": 0, "rotate": False}, discord.DiscordClient(base_url="http://unused.test")
            try:
                channel_lock.inherited()
                self.writer = os.getpid()
                for index, marker in enumerate(markers[:prompts]):
                    if index == 2:
                        client.send("20", "!clear now")
                    client.send("20", f"say {marker}")
                    row["marker_counts"][marker] = 1
                    kind = dict(enumerate(answers)).get(index)
                    lines(transcript, asked(marker), *([] if kind == "silent" else [
                        claude(f"{run_id}-{index}", output_tokens=500 if kind == "huge" else 2)]))
            except turn_gate.Stop as stop:
                row.update(status="fail", reason=f"harness: {stop}")
                state["rc"] = 1
            (harness or (lambda _state: None))(state)
            log = self.root / "server.log"
            log.write_text(("rotated\n" if state["rotate"] else log.read_text()) + f"turn {SESSION} done\n")
            (out / "report.claude-tui.json").write_text(json.dumps(
                {"run_id": run_id, "cell": "claude-tui", "channel_id": "20", "coverage_class_violations": [], "scenarios": [row]}))
            return state["rc"]

        args = argparse.Namespace(scenario="E-50", cell=cell, run_id="run", profile="default", deploy_sha=None,
                                  budget=json.dumps(budget or {"turns": 3, "tokens": 300, "time_s": 10}))
        held = channel_lock.acquire(self.root / "state", RESOURCE)
        self.addCleanup(held.close)
        witness = {"fd": held.fd, "device": held.identity.st_dev, "inode": held.identity.st_ino}
        env = {channel_lock.LEASE_ENV: json.dumps(witness)} if lease else {}
        with mock.patch.dict(os.environ, env), mock.patch.object(smoke_driver, "_transcripts", return_value=str(transcript)), \
                mock.patch.object(smoke.cell_driver, "main", main), mock.patch.object(discord.urllib.request, "urlopen", posted):
            if not lease:
                os.environ.pop(channel_lock.LEASE_ENV, None)
            report = smoke_driver.run(args, self.config, spawn=spawn)
        held.close()
        return report, [text for text in posted.sent if not text.startswith("!")], calls

    def test_passing_scenario_reports_turns_tokens_identity_and_the_host_the_server_admits(self):
        report, prompts, spawned = self.drive()
        planned = next(row for row in plan.deploy_occurrences(plan.load()) if row["scenario"] == "E-50")
        self.assertEqual((report["status"], report["native_turns"], report["tokens"], report["budget_checked"]), ("PASS", 3, 30, 3))
        self.assertEqual(report["usage"], {"native_turns": 3, "input_tokens": 3, "output_tokens": 6,
                                           "cache_read_tokens": 9, "cache_write_tokens": 12})
        self.assertEqual(report["actual_occurrence_key"], summary.occurrence_key({**planned, **CONTRACT}, "default"))
        self.assertEqual((report["head"], report["boot_id"], report["instance_id"]), (HEAD, "boot-1", "release@host"))
        # The input writer is this lease-holding process: no harness child exists to outlive it.
        self.assertEqual((len(prompts), self.writer, [argv for argv in spawned if "preflight" not in argv]), (3, os.getpid(), []))
        self.assertTrue(report["real_provider_contacted"] and all(Path(path).is_file() for path in report["evidence_paths"]))
        self.assertEqual(list((self.root / "state").rglob("server.log")), [])
        self.detail.return_value = {**DETAIL, "herdr": {"configured_channels": []}}
        self.assertEqual(self.drive()[0]["actual_occurrence_key"],
                         summary.occurrence_key({**planned, **CONTRACT, "host": "tmux"}, "default"))

    def test_no_input_follows_a_turn_whose_real_usage_is_unknown_or_over_its_limit(self):
        report, prompts, _ = self.drive(answers=["silent"])
        self.assertEqual((len(prompts), report["status"], report["reason"]), (1, "UNEVALUABLE", "TURN_USAGE_UNKNOWN"))
        self.assertEqual((report["native_turns"], report["tokens"], report["usage"]["input_tokens"]), (None, None, None))
        report, prompts, _ = self.drive(answers=["huge"])
        self.assertEqual((len(prompts), report["status"], report["reason"]), (1, "UNEVALUABLE", "TURN_OVER_BUDGET"))
        self.assertEqual((report["native_turns"], report["tokens"], report["budget_checked"]), (1, 508, 1))
        report, prompts, _ = self.drive(answers=[None, None, "silent"])
        self.assertEqual((len(prompts), report["status"], report["reason"], report["tokens"]), (3, "UNEVALUABLE", "USAGE_UNKNOWN", None))

    def test_refused_runs_send_no_prompt_and_charge_nothing(self):
        cases = [({"cell": "codex-herdr"}, "CELL_NOT_ENABLED"), ({"lease": False}, "CHANNEL_LEASE_MISSING"),
                 ({"budget": {"turns": 2, "tokens": 300, "time_s": 10}}, "BUDGET_TOO_SMALL_FOR_SCENARIO"),
                 ({"budget": {"turns": 3, "tokens": 299, "time_s": 10}}, "BUDGET_TOO_SMALL_FOR_SCENARIO"),
                 ({"budget": {"turns": 3, "tokens": 300, "time_s": 9}}, "BUDGET_TOO_SMALL_FOR_SCENARIO"),
                 ({"idle": 1}, "CELL_NOT_IDLE: busy: turn")]
        for options, reason in cases:
            with self.subTest(reason=reason, options=options):
                report, prompts, _ = self.drive(**options)
                self.assertEqual((report["status"], report["reason"], prompts, self.writer), ("UNEVALUABLE", reason, [], None))
                self.assertEqual((report["native_turns"], report["tokens"], report["real_provider_contacted"]), (0, 0, False))

    def test_lost_observation_is_unevaluable_but_a_seen_duplicate_or_missing_body_is_fail(self):
        def timeout(state, duplicate=False):
            first, second, _third = state["row"]["marker_counts"]
            state["row"].update(status="fail", reason="TimeoutError: timed out",
                                step_timings=[{"step": "send_discord_prompt"}, {"step": "wait_for_discord_text"}])
            state["row"]["marker_counts"].update({first: 2 if duplicate else 1, second: 0})
            state["rc"] = 1

        def missing(state):
            timeout(state)
            state["row"]["reason"] = "wait_for_discord_text: marker not seen before the deadline"

        def unsent(state):
            timeout(state)
            state["row"]["step_timings"].append({"step": "send_discord_prompt"})

        def refused(state):
            timeout(state)
            state["row"]["input_refusals"] = [{"content": "busy"}]

        def residue(state):
            state["row"]["teardown_error"] = "session left busy"
            state["rotate"] = True

        report, _, _ = self.drive(harness=timeout, prompts=2)
        self.assertEqual((report["status"], report["reason"], report["native_turns"]), ("UNEVALUABLE", "UNOBSERVED_TRANSPORT", 2))
        self.assertEqual(report["assertions"], [{"key": "turns_delivered_once", "status": "UNEVALUABLE", "complete": False}])
        self.assertEqual(self.drive(harness=lambda state: state.update(rotate=True))[0]["reason"], "UNOBSERVED_SERVER_LOG")
        for broken in (lambda state: timeout(state, duplicate=True), missing, unsent, refused, residue):
            report, _, _ = self.drive(harness=broken, prompts=2 if broken is not residue else 3)
            self.assertEqual((report["status"], report["assertions"][0]["status"]), ("FAIL", "FAIL"))


class Supervisor(Fixture):
    def requests(self):
        return sorted(path.stem for path in (self.root / "state" / "inbox").glob("*.json"))

    def test_off_observes_nothing_and_only_turns_a_status_that_shows_enabled_off(self):
        self.save(enabled=False)
        before = sorted(path.name for path in self.root.iterdir())
        self.assertEqual(release_host.tick(self.path, now=1000), {"status": "DISABLED"})
        self.assertEqual(release_host.after_deploy(self.path, "deploy-1", HEAD), {"status": "DISABLED"})
        self.assertEqual(sorted(path.name for path in self.root.iterdir()), before, "a machine that never ran stays untouched")
        state, pending = self.root / "state", {"schema_version": 1, "observed": {}, "events": {"e": {"status": "pending"}}}
        write_json(state / "outbox.json", pending)
        for now in (1100, 1200):  # The second round is a run that was in flight at the switch publishing enabled again.
            self.assertEqual(publication.heartbeat(state, {**self.config, "enabled": True}, now - 50)["outbox_pending"], 1)
            release_host.tick(self.path, now=now)
            shown = consumer.view(state / "status.json", head=HEAD, now=now + 5000)
            self.assertEqual((shown["check"]["result"], shown["monitor"]["enabled"], shown["monitor"]["pending_alerts"]), ("DISABLED", 0, 0))
        release_host.tick(self.path, now=1300)
        self.assertEqual(read_json(state / "status.json", {})["heartbeat_at"], 1200, "an off status is not rewritten")
        self.assertEqual((self.detail.call_count, read_json(state / "outbox.json", {})), (0, pending))

    def test_core_request_is_one_per_head_and_slot_and_yields_to_a_waiting_deploy_or_fresh_result(self):
        with mock.patch.object(runner, "tick", return_value={"status": "NOT_DUE"}) as ticked:
            release_host.tick(self.path, now=1000)
            release_host.tick(self.path, now=1050)
            self.assertEqual(self.requests(), [f"core-{HEAD[:12]}-10"])
            self.assertEqual(ticked.call_args.kwargs["overrides"]["head"], HEAD)
            (self.root / "state" / "inbox" / f"core-{HEAD[:12]}-10.json").unlink()
            release_host.after_deploy(self.path, "deploy-1", HEAD)
            release_host.tick(self.path, now=1000)
            self.assertEqual(self.requests(), ["deploy-1"])
            (self.root / "state" / "inbox" / "deploy-1.json").unlink()
            write_json(self.root / "state" / "summary.json", {"head": HEAD, "suite": "P", "completed_at": 950})
            release_host.tick(self.path, now=1000)
            self.assertEqual(self.requests(), [])
            release_host.tick(self.path, now=1050)
            self.assertEqual(self.requests(), [f"core-{HEAD[:12]}-10"])

    def test_deploy_in_progress_waits_visibly_and_an_unobservable_server_goes_stale(self):
        write_json(self.root / "state" / "status.json", {"heartbeat_at": 1})
        self.detail.side_effect = OSError("refused")
        with mock.patch.object(runner, "tick") as ticked, open(self.root / "deploy.lock") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            self.assertEqual(release_host.tick(self.path, now=1000), {"status": "DEFERRED_DEPLOY"})
            fcntl.flock(lock, fcntl.LOCK_UN)
            self.assertEqual(read_json(self.root / "state" / "status.json", {})["heartbeat_at"], 1000)
            self.assertEqual(release_host.tick(self.path, now=2000), {"status": "HOST_UNAVAILABLE"})
            self.assertEqual((ticked.call_count, self.requests()), (0, []))
        self.assertEqual(read_json(self.root / "state" / "status.json", {})["heartbeat_at"], 1000)

    def test_supervised_tick_runs_the_core_bundle_under_the_observed_identity(self):
        seen = release_host.identity(self.config["release_host"], DETAIL)
        (self.root / "driver.py").write_text(
            "import json\nprint(json.dumps({'schema_version': 2, 'status': 'PASS', 'native_turns': 1, 'tokens': 1,"
            " 'budget_checked': 1, 'usage': {'native_turns': 1, 'input_tokens': 1, 'output_tokens': 0,"
            f" 'cache_read_tokens': 0, 'cache_write_tokens': 0}}, **{seen!r}}}))\n")
        amount = {"turns": 3, "tokens": 300, "time_s": 100}
        self.save(capabilities=["E-51/herdr", *(f"{kind}:{name}" for kind in ("usage", "turn_budget") for name in ("claude", "codex"))],
                  reservation={"P": {"claude": amount, "codex": amount}},
                  budget={name: {f"daily_{key}": value for key, value in amount.items()} for name in ("claude", "codex")})
        result = release_host.tick(self.path, now=1000)
        state = self.root / "state"
        published = read_json(state / "summary.json", {})
        self.assertEqual(result["run_id"], published["run_id"])
        self.assertEqual({key: published[key] for key in seen}, seen)
        self.assertEqual((published["suite"], [row["head"] for row in published["occurrences"][:2]]), ("P", [HEAD, HEAD]))
        self.assertEqual(read_json(state / "status.json", {})["rotation_interval_s"], 100)
        self.assertEqual(read_json(state / "requests.json", {}), {f"core-{HEAD[:12]}-10": published["result"]})
        self.assertEqual(release_host.tick(self.path, now=1010), {"status": "NOT_DUE"})
        self.assertEqual(read_json(state / "status.json", {})["heartbeat_at"], 1010)


class AfterDeploy(Fixture):
    def hook(self, *, config=True, python=None, launchctl="", limit="20"):
        home, tools = self.root / "home", self.root / "tools"
        (home / "Library").mkdir(parents=True, exist_ok=True)
        tools.mkdir(exist_ok=True)
        (tools / "launchctl").write_text(f"#!/bin/sh\necho \"$@\" >> {self.root}/launchctl.log\n{launchctl}[ \"$1\" != print ]\n")
        (tools / "python3").write_text(python or f"#!/bin/sh\nexec {sys.executable} \"$@\"\n")
        for tool in tools.iterdir():
            tool.chmod(0o755)
        env = {"HOME": str(home), "PATH": f"{tools}:/usr/bin:/bin",
               "AGENTDESK_E2E_CONTINUOUS_CONFIG": str(self.path if config else self.root / "absent.json")}
        done = subprocess.run(["bash", str(REPO / "scripts/e2e/continuous/after_deploy.sh"), str(REPO), HEAD, "stamp", limit],
                              env=env, capture_output=True, text=True, timeout=60)
        calls = self.root / "launchctl.log"
        return done, calls.read_text() if calls.exists() else "", home / "Library/LaunchAgents/com.agentdesk.e2e-continuous.plist"

    def test_without_the_machine_config_nothing_is_run_or_written(self):
        done, calls, plist = self.hook(config=False, python=f"#!/bin/sh\ntouch {self.root}/python-ran\n")
        self.assertEqual((done.returncode, done.stdout, calls), (0, "", ""))
        self.assertFalse(plist.exists() or (self.root / "python-ran").exists() or (self.root / "state").exists())

    def test_enabled_config_submits_the_deploy_request_and_loads_the_tick_job_and_switching_off_is_reported(self):
        done, calls, plist = self.hook()
        self.assertEqual(done.returncode, 0)
        self.assertEqual(read_json(self.root / "state/inbox/deploy-stamp.json", {})["deploy_sha"], HEAD)
        text = plist.read_text()
        self.assertTrue(f"<string>{REPO}</string>" in text and str(self.path) in text and "__" not in text)
        self.assertIn("bootstrap", calls)
        # Switched off with the job, the request and an enabled status all still in place.
        publication.heartbeat(self.root / "state", self.config, time.time())
        (self.root / "state/inbox/deploy-stamp.json").unlink()
        self.save(enabled=False)
        done, later, _ = self.hook()
        self.assertIn('"status": "DISABLED"', done.stdout)
        self.assertEqual((later, (self.root / "state/inbox/deploy-stamp.json").exists()), (calls, False))
        shown = consumer.view(self.root / "state/status.json", head=HEAD, now=time.time() + 5000)
        self.assertEqual((shown["check"]["result"], shown["monitor"]["enabled"]), ("DISABLED", 0))

    def test_a_stalled_step_is_cut_off_at_the_time_limit_and_does_not_outlive_the_deploy(self):
        stall = f"echo $$ > {self.root}/stalled.pid\nexec /bin/sleep 30\n"
        for options in ({"python": "#!/bin/sh\n" + stall}, {"launchctl": f"[ \"$1\" = print ] && {{ {stall.replace(chr(10), '; ')}}}\n"}):
            with self.subTest(stalled=list(options)):
                started = time.monotonic()
                done, _, _ = self.hook(limit="1", **options)
                self.assertEqual(done.returncode, 0)
                self.assertIn("did not finish within 1s", done.stdout)
                self.assertLess(time.monotonic() - started, 15)
                with self.assertRaises(ProcessLookupError):
                    os.kill(int((self.root / "stalled.pid").read_text()), 0)
                (self.root / "stalled.pid").unlink()


if __name__ == "__main__":
    unittest.main()
