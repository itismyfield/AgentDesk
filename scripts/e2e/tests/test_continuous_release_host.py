"""Release-host supervisor, smoke driver, transcript usage and the deploy hook of the continuous runner."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO))
from scripts.e2e.continuous import channel_lock, plan, release_host, runner, smoke_driver, summary, usage  # noqa: E402
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


def claude(message, **used):
    return {"type": "assistant", "message": {"id": message, "content": [{"type": "text", "text": "ok"}], "usage": used}}


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
                             "cells": {"claude-herdr": {"smoke_cell": "claude-tui", "channel_id": "20",
                                                        "deadline_s": 10, "turn_tokens": 100}}}}
        self.save()
        observed = mock.patch.object(release_host, "detail", return_value=DETAIL)
        self.detail = observed.start()
        self.addCleanup(observed.stop)

    def save(self, **changes):
        self.config.update(changes)
        self.path.write_text(json.dumps(self.config))


class Usage(Fixture):
    def test_claude_bills_each_api_message_once_and_only_bytes_the_run_appended(self):
        path = self.root / "transcript.jsonl"
        lines(path, claude("old", input_tokens=900, output_tokens=900))
        mark = usage.mark(path)
        lines(path, claude("m1", input_tokens=3, output_tokens=1, cache_read_input_tokens=10),
              claude("m1", input_tokens=3, output_tokens=5, cache_read_input_tokens=10),
              claude("m2", input_tokens=2, output_tokens=4, cache_creation_input_tokens=7))
        self.assertEqual(usage.measure("claude", [mark]),
                         {"input_tokens": 5, "output_tokens": 9, "cache_read_tokens": 10, "cache_write_tokens": 7})

    def test_codex_subtracts_the_session_total_from_before_the_run(self):
        def total(**used):
            return {"type": "event_msg", "payload": {"type": "token_count", "info": {"total_token_usage": used}}}
        path = self.root / "transcript.jsonl"
        lines(path, total(input_tokens=100, cached_input_tokens=40, output_tokens=10))
        mark = usage.mark(path)
        lines(path, total(input_tokens=200, cached_input_tokens=90, output_tokens=15),
              total(input_tokens=300, cached_input_tokens=140, output_tokens=25))
        self.assertEqual(usage.measure("codex", [mark]),
                         {"input_tokens": 100, "output_tokens": 15, "cache_read_tokens": 100, "cache_write_tokens": 0})

    def test_a_run_without_readable_usage_is_unknown_not_zero(self):
        path = self.root / "transcript.jsonl"
        lines(path, claude("old", input_tokens=1, output_tokens=1))
        self.assertIsNone(usage.measure("claude", [usage.mark(path)]))
        self.assertIsNone(usage.measure("codex", [{"path": str(self.root / "absent.jsonl"), "offset": 0}]))
        lines(path, claude("bad", input_tokens="many"))
        self.assertIsNone(usage.measure("claude", [{"path": str(path), "offset": 0}]))


class Driver(Fixture):
    def drive(self, budget=None, lease=True, harness=None, cell="claude-herdr", idle=0):
        """Run the driver entry point; `harness` edits what the fake relay harness leaves behind."""
        calls, smoke = [], smoke_driver._smoke()[0]  # noqa: SLF001
        transcript = self.root / "transcript.jsonl"

        def spawn(argv, **_kwargs):
            calls.append(argv)
            if "preflight" in argv:
                return subprocess.CompletedProcess(argv, idle, stdout="busy: turn" if idle else "idle")
            run_id, out = argv[argv.index("--run-id") + 1], Path(argv[argv.index("--output") + 1])
            markers = smoke.expected_markers(smoke_driver.SCENARIOS, "E-50", run_id)
            row = {"id": "E-50", "run_id": run_id, "cell": "claude-tui", "channel_id": "20", "status": "pass",
                   "real_provider_contacted": True, "marker_counts": dict.fromkeys(markers, 1)}
            state = {"row": row, "rc": 0, "inputs": markers, "rotate": False}
            (harness or (lambda _state: None))(state)
            lines(transcript, *[{"type": "user", "message": {"role": "user", "content": f"say {marker}"}}
                                for marker in state["inputs"]], claude("m1", input_tokens=4, output_tokens=6))
            (self.root / "server.log").write_text(("rotated\n" if state["rotate"] else "seed\n") + f"turn {SESSION} done\n")
            (out / "report.claude-tui.json").write_text(json.dumps(
                {"run_id": run_id, "cell": "claude-tui", "channel_id": "20", "coverage_class_violations": [], "scenarios": [row]}))
            return subprocess.CompletedProcess(argv, state["rc"])

        args = argparse.Namespace(scenario="E-50", cell=cell, run_id="run", profile="default", deploy_sha=None,
                                  budget=json.dumps(budget or {"turns": 3, "tokens": 300, "time_s": 10}))
        held = channel_lock.acquire(self.root / "state", RESOURCE)
        self.addCleanup(held.close)
        witness = {"fd": held.fd, "device": held.identity.st_dev, "inode": held.identity.st_ino}
        env = {channel_lock.LEASE_ENV: json.dumps(witness)} if lease else {}
        with mock.patch.dict(os.environ, env), mock.patch.object(smoke_driver, "_transcripts", return_value=str(transcript)):
            if not lease:
                os.environ.pop(channel_lock.LEASE_ENV, None)
            report = smoke_driver.run(args, self.config, spawn=spawn)
        held.close()
        return report, [argv for argv in calls if "preflight" not in argv]

    def test_passing_scenario_reports_turns_tokens_identity_and_the_host_the_server_admits(self):
        report, harness = self.drive()
        planned = next(row for row in plan.deploy_occurrences(plan.load()) if row["scenario"] == "E-50")
        self.assertEqual((report["status"], report["native_turns"], report["tokens"], report["budget_checked"]), ("PASS", 3, 10, 3))
        self.assertEqual(report["usage"], {"native_turns": 3, "input_tokens": 4, "output_tokens": 6,
                                           "cache_read_tokens": 0, "cache_write_tokens": 0})
        self.assertEqual(report["actual_occurrence_key"], summary.occurrence_key({**planned, **CONTRACT}, "default"))
        self.assertEqual((report["head"], report["boot_id"], report["instance_id"]), (HEAD, "boot-1", "release@host"))
        self.assertEqual((len(harness), report["real_provider_contacted"]), (1, True))
        self.assertTrue(all(Path(path).is_file() for path in report["evidence_paths"]))
        self.assertEqual(list((self.root / "state").rglob("server.log")), [])
        self.detail.return_value = {**DETAIL, "herdr": {"configured_channels": []}}
        self.assertEqual(self.drive()[0]["actual_occurrence_key"],
                         summary.occurrence_key({**planned, **CONTRACT, "host": "tmux"}, "default"))

    def test_refused_runs_send_no_prompt_and_charge_nothing(self):
        cases = [({"cell": "codex-tui"}, "CELL_NOT_ENABLED"), ({"lease": False}, "CHANNEL_LEASE_MISSING"),
                 ({"budget": {"turns": 2, "tokens": 300, "time_s": 10}}, "BUDGET_TOO_SMALL_FOR_SCENARIO"),
                 ({"budget": {"turns": 3, "tokens": 299, "time_s": 10}}, "BUDGET_TOO_SMALL_FOR_SCENARIO"),
                 ({"budget": {"turns": 3, "tokens": 300, "time_s": 9}}, "BUDGET_TOO_SMALL_FOR_SCENARIO"),
                 ({"idle": 1}, "CELL_NOT_IDLE: busy: turn")]
        for options, reason in cases:
            with self.subTest(reason=reason, options=options):
                report, harness = self.drive(**options)
                self.assertEqual((report["status"], report["reason"], harness), ("UNEVALUABLE", reason, []))
                self.assertEqual((report["native_turns"], report["tokens"], report["real_provider_contacted"]), (0, 0, False))

    def test_lost_observation_is_unevaluable_but_a_seen_duplicate_or_missing_body_is_fail(self):
        def timeout(state, duplicate=False):
            first, second, third = state["row"]["marker_counts"]
            state["row"].update(status="fail", reason="TimeoutError: timed out",
                                marker_counts={first: 2 if duplicate else 1, second: 0, third: None},
                                step_timings=[{"step": "send_discord_prompt"}, {"step": "wait_for_discord_text"}])
            state.update(rc=1, inputs=state["inputs"][:2])

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

        report, _ = self.drive(harness=timeout)
        self.assertEqual((report["status"], report["reason"], report["native_turns"]), ("UNEVALUABLE", "UNOBSERVED_TRANSPORT", 2))
        self.assertEqual(report["assertions"], [{"key": "turns_delivered_once", "status": "UNEVALUABLE", "complete": False}])
        self.assertEqual(self.drive(harness=lambda state: state.update(rotate=True))[0]["reason"], "UNOBSERVED_SERVER_LOG")
        for broken in (lambda state: timeout(state, duplicate=True), missing, unsent, refused, residue):
            report, _ = self.drive(harness=broken)
            self.assertEqual((report["status"], report["assertions"][0]["status"]), ("FAIL", "FAIL"))

    def test_delivered_prompt_without_its_native_input_makes_usage_unknown(self):
        report, _ = self.drive(harness=lambda state: state.update(inputs=state["inputs"][:2]))
        self.assertEqual((report["status"], report["native_turns"], report["tokens"]), ("PASS", None, None))


class Supervisor(Fixture):
    def requests(self):
        return sorted(path.stem for path in (self.root / "state" / "inbox").glob("*.json"))

    def test_off_observes_nothing_and_writes_nothing(self):
        self.save(enabled=False)
        before = sorted(path.name for path in self.root.iterdir())
        self.assertEqual(release_host.tick(self.path, now=1000), {"status": "DISABLED"})
        self.assertEqual(release_host.after_deploy(self.path, "deploy-1", HEAD), {"status": "DISABLED"})
        self.assertEqual((self.detail.call_count, sorted(path.name for path in self.root.iterdir())), (0, before))

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
        self.save(capabilities=[f"{kind}:{name}" for kind in ("usage", "turn_budget") for name in ("claude", "codex")],
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


class AfterDeploy(Fixture):
    def hook(self, *, config=True, python=None):
        home, tools = self.root / "home", self.root / "tools"
        (home / "Library").mkdir(parents=True, exist_ok=True)
        tools.mkdir(exist_ok=True)
        (tools / "launchctl").write_text(f"#!/bin/sh\necho \"$@\" >> {self.root}/launchctl.log\n[ \"$1\" != print ]\n")
        (tools / "python3").write_text(python or f"#!/bin/sh\nexec {sys.executable} \"$@\"\n")
        for tool in tools.iterdir():
            tool.chmod(0o755)
        env = {"HOME": str(home), "PATH": f"{tools}:/usr/bin:/bin",
               "AGENTDESK_E2E_CONTINUOUS_CONFIG": str(self.path if config else self.root / "absent.json")}
        done = subprocess.run(["bash", str(REPO / "scripts/e2e/continuous/after_deploy.sh"), str(REPO), HEAD, "stamp"],
                              env=env, capture_output=True, text=True, timeout=60)
        calls = self.root / "launchctl.log"
        return done, calls.read_text() if calls.exists() else "", home / "Library/LaunchAgents/com.agentdesk.e2e-continuous.plist"

    def test_without_the_machine_config_nothing_is_run_or_written(self):
        done, calls, plist = self.hook(config=False, python=f"#!/bin/sh\ntouch {self.root}/python-ran\n")
        self.assertEqual((done.returncode, done.stdout, calls), (0, "", ""))
        self.assertFalse(plist.exists() or (self.root / "python-ran").exists() or (self.root / "state").exists())

    def test_enabled_config_submits_the_deploy_request_and_loads_the_tick_job_and_off_does_neither(self):
        done, calls, plist = self.hook()
        self.assertEqual(done.returncode, 0)
        self.assertEqual(read_json(self.root / "state/inbox/deploy-stamp.json", {})["deploy_sha"], HEAD)
        text = plist.read_text()
        self.assertTrue(f"<string>{REPO}</string>" in text and str(self.path) in text and "__" not in text)
        self.assertIn("bootstrap", calls)
        for path in (plist, self.root / "launchctl.log", self.root / "state/inbox/deploy-stamp.json"):
            path.unlink()
        self.save(enabled=False)
        done, calls, plist = self.hook()
        self.assertIn('"status": "DISABLED"', done.stdout)
        self.assertFalse(plist.exists() or calls or (self.root / "state/inbox/deploy-stamp.json").exists())


if __name__ == "__main__":
    unittest.main()
