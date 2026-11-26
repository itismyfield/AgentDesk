"""Release-host supervisor, smoke driver, transcript usage and the deploy hook of the continuous runner."""
import argparse
import contextlib
import fcntl
import io
import json
import os
from pathlib import Path
import re
import select
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO))
from scripts.e2e.continuous import (budget, channel_lock, consumer, plan, publication, release_host, runner,  # noqa: E402
                                    smoke_driver, summary, turn_gate, usage)
from scripts.e2e.continuous.store import read_json, write_json  # noqa: E402
from scripts.e2e.tui_relay import discord  # noqa: E402

HEAD = "c" * 40
HERDR = {"admission": "open", "restart_required": False, "endpoints": {"ep": {"local": True}}, "configured_channels": ["20"],
         "input_holds": 0, "reconnect": {"channels": 1, "published": 1, "withheld": 0, "unknown": 0, "pending": 0}}
DETAIL = {"boot_incarnation": "boot-1", "status": "healthy", "fully_recovered": True, "degraded_reasons": [], "mailboxes": [],
          "tui_o_writer_readiness": [{"provider": "claude", "channel_id": "20", "accepts": True}], "herdr": HERDR,
          "release_source": {"node_hostname": "host", "deployed_repo_head": HEAD}}
RUNNING = {"channel": "20", "provider": "claude", "state": "bound", "input_hold": None, "pane": "provider_running",
           "launch_evidence": "recorded", "nonce": "n1"}
RESOURCE = {"guild": "1", "parents": ["20", "30"], "bot_set": ["4"]}
CONTRACT = {"busy_basis": None, "placement": None, "operation": "clear_and_turns", "occurrence": 1}
SESSION = "AgentDesk-claude-adk-claude-tui-2-e2e"
FOREIGN_RETENTION = "tui_o:reader_retention_stalled:30"
TARGET_RETENTION = "tui_o:reader_retention_stalled:20"
UNATTRIBUTED_RETENTION = "tui_o:reader_retention_stalled:unattributed"
BINDING = "tui_o:binding_missing:"
QUEUE = "provider:claude:pending_queue_depth:"
TARGET_BOX = {"provider": "claude", "channel_id": 20, "queue_depth": 0, "agent_turn_status": "idle"}
FOREIGN_BOX = {"provider": "claude", "channel_id": 40, "queue_depth": 1, "agent_turn_status": "idle"}


def degraded_detail(*reasons):
    return {"status": "degraded", "degraded_reasons": list(reasons)}


def lines(path, *rows):
    with open(path, "a", encoding="utf-8") as stream:
        stream.writelines(json.dumps(row) + "\n" for row in rows)


def asked(marker):
    return {"type": "user", "message": {"role": "user", "content": f"say {marker}"}}


def claude(message, stop="end_turn", **used):
    used = {"input_tokens": 1, "output_tokens": 2, "cache_read_input_tokens": 3, "cache_creation_input_tokens": 4, **used}
    return {"type": "assistant", "message": {"id": message, "stop_reason": stop, "usage": used,
                                             "content": [{"type": "text", "text": "ok"}]}}


def codex_turn(turn, marker, calls, drop=None, done=True):
    """A Codex native turn: its input, per API call a model output and its usage record, then its completion.

    The last call's output is the final answer; `drop` loses one call's record while the turn total still counts it.
    """
    rows, total = [{"type": "event_msg", "payload": {"type": "task_started", "turn_id": turn}},
                   {"type": "response_item", "payload": {"type": "message", "role": "user",
                                                         "content": [{"type": "input_text", "text": f"say {marker}"}]}}], {}
    for index, used in enumerate(calls):
        final = index == len(calls) - 1
        rows.append({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": []}
                     if final else {"type": "function_call", "name": "x", "call_id": "c"}})
        total = {key: total.get(key, 0) + used.get(key, 0) for key in used}
        if index != drop:
            rows.append({"type": "token_usage_record", "payload": {"turn_id": turn, "usage": used, "turn_token_usage": total}})
    return rows + ([{"type": "event_msg", "payload": {"type": "task_complete", "turn_id": turn}}] if done else [])


class Fixture(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="continuous-release-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        for name in ("server.log", "deploy.lock", "transcript.jsonl"):
            (self.root / name).write_text("seed\n" if name == "server.log" else "")
        # The release binary answers `herdr status` from a file the test controls.
        (self.root / "binary").write_text(f"#!/bin/sh\nexec cat {self.root}/herdr-status.json\n")
        (self.root / "binary").chmod(0o755)
        write_json(self.root / "herdr-status.json", {"executions": [RUNNING], "other_input_holds": []})
        self.path = self.root / "config.json"
        self.config = {
            "enabled": True, "state_dir": str(self.root / "state"), "resource": RESOURCE, "core_interval_s": 100,
            "driver": [sys.executable, str(self.root / "driver.py")], "occurrence_contracts": {"E-50": CONTRACT, "E-51": CONTRACT},
            "release_host": {"base_url": "http://127.0.0.1:9", "binary": str(self.root / "binary"),
                             "runtime_root": str(self.root), "server_log": str(self.root / "server.log"),
                             "deploy_lock": str(self.root / "deploy.lock"),
                             "cells": {name: {"channel_id": "20", "deadline_s": 10, "turn_tokens": 100, "usage_wait_s": 0}
                                       for name in ("claude-herdr", "claude-tui")}}}
        self.save()
        observed = mock.patch.object(release_host, "detail", return_value=DETAIL)
        self.detail = observed.start()
        self.addCleanup(observed.stop)
        # The smoke and harness read the same server snapshot through their own HTTP helper.
        api = mock.patch.object(smoke_driver._smoke()[0].cell_driver, "_read_api_json",  # noqa: SLF001
                                side_effect=lambda *_args, **_kwargs: (200, self.detail.return_value))
        api.start()
        self.addCleanup(api.stop)

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
        mark = usage.mark(self.log)
        result = {"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "u0", "content": "ok"}]}}
        lines(self.log, asked("T3"), result, claude("m4"))
        self.assertIsNone(usage.turn("claude", [mark], "T3"), "a tool result whose API call was not recorded")

    def test_codex_turn_is_its_own_usage_records_through_the_final_answer(self):
        call = {"input_tokens": 100, "cached_input_tokens": 40, "output_tokens": 5}

        def measured(calls, **options):
            self.log.write_text("")
            lines(self.log, *codex_turn("old", "T0", [{**call, "input_tokens": 900}]))
            mark = usage.mark(self.log)
            # A later turn already started when the settle reads must not count toward this one.
            lines(self.log, *codex_turn("t1", "T1", calls, **options), *codex_turn("t2", "T2", [call]))
            return usage.turn("codex", [mark], "T1")

        self.assertEqual(measured([call, call]), {"input_tokens": 120, "output_tokens": 10, "cache_read_tokens": 80, "cache_write_tokens": 0})
        self.assertIsNone(measured([call, call], done=False), "a turn that has not finished")
        self.assertIsNone(measured([call, call], drop=1), "the final answer's usage record is missing")
        self.assertIsNone(measured([call, call], drop=0), "a tool call's usage record is missing")
        self.assertIsNone(measured([call, {"input_tokens": 1, "output_tokens": 1}]), "a missing counter is not zero")

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
    def setUp(self):
        super().setUp()
        self.now = 1000.0
        self.enterContext(mock.patch.object(budget.time, "monotonic", side_effect=lambda: self.now))

    def drive(self, budget_left=None, lease=True, harness=None, cell="claude-herdr", answers=(), prompts=3,
              scenario="E-50", stray=None, host="herdr", herdr=None, cli=True, detail_fields=None,
              before_final_observe=None, profile="default"):
        """Run the driver with the real smoke preflight and a fake harness that keeps going after a refused input.

        `answers[i]` makes turn i 'silent' or 'huge'; `stray` is an extra prompt after the first turn; `!clear`
        moves the cell to a new session transcript. `host="tmux"` plans and observes E-50 on a tmux cell.
        """
        calls, posted, smoke = [], Posted(), smoke_driver._smoke()[0]  # noqa: SLF001
        discord, driver, self.writer = smoke.cell_driver.discord, smoke.cell_driver, None
        session = [self.root / "transcript.jsonl"]
        detail = {**DETAIL, "herdr": {**HERDR, **(herdr or {})}, **(detail_fields or {})}
        self.config.pop("manifest", None)
        if host == "tmux":
            manifest = plan.load()
            next(row for row in manifest["bundles"]["P"]["occurrences"] if row["scenario"] == "E-50").update(
                host="tmux", cell="claude-tui")
            write_json(self.root / "manifest.json", manifest)
            self.config["manifest"], cell = str(self.root / "manifest.json"), "claude-tui"
            detail["herdr"]["configured_channels"] = []
        self.detail.return_value = detail
        if not cli:
            (self.root / "herdr-status.json").unlink()

        def spawn(argv, **_kwargs):
            calls.append(argv)
            shown = io.StringIO()
            with contextlib.redirect_stdout(shown):
                code = smoke.main(argv[2:])
            return subprocess.CompletedProcess(argv, code, stdout=shown.getvalue())

        def main():
            args = driver.parse_args()
            markers = smoke.expected_markers(smoke_driver.SCENARIOS, "E-50", args.run_id)
            row = {"id": "E-50", "run_id": args.run_id, "cell": args.cell, "channel_id": "20", "status": "pass",
                   "real_provider_contacted": True, "marker_counts": dict.fromkeys(markers)}
            state, client = {"row": row, "rc": 0, "rotate": False}, discord.DiscordClient(base_url="http://127.0.0.1:9")
            channel_lock.inherited()
            self.writer = os.getpid()
            print("[e2e] harness output")
            if args.cell.endswith("-herdr"):
                row["herdr_observations"] = [driver.herdr.observe(driver, args, cold=True)]
            for index, marker in enumerate(markers[:prompts]):
                try:
                    if index == 2:
                        client.send("20", "!clear now")
                        session.append(self.root / f"session-{len(posted.sent)}.jsonl")
                    client.send("20", f"say {marker}")
                    row["marker_counts"][marker] = 1
                    kind = dict(enumerate(answers)).get(index)
                    lines(session[-1], asked(marker), *([] if kind == "silent" else [
                        claude(f"{args.run_id}-{index}", output_tokens=500 if kind == "huge" else 2, cache_read_input_tokens=500 if kind == "cached" else 3)]))
                    if stray and index == 0:
                        client.send("20", stray)
                except turn_gate.Stop as stop:
                    row.update(status="fail", reason=f"harness: {stop}")
                    state["rc"] = 1
            if args.cell.endswith("-herdr"):
                if before_final_observe:
                    before_final_observe(detail)
                row["herdr_observations"].append(driver.herdr.observe(driver, args))
            (harness or (lambda _state: None))(state)
            log = self.root / "server.log"
            log.write_text(("rotated\n" if state["rotate"] else log.read_text()) + f"turn {SESSION} done\n")
            (Path(args.output) / f"report.{args.cell}.json").write_text(json.dumps(
                {"run_id": args.run_id, "cell": args.cell, "channel_id": "20", "coverage_class_violations": [], "scenarios": [row]}))
            return state["rc"]

        args = argparse.Namespace(scenario=scenario, cell=cell, run_id="run", profile=profile, deploy_sha=None,
                                  budget=json.dumps(budget_left or {"turns": 3, "tokens": 300, "time_s": 10}))
        held = channel_lock.acquire(self.root / "state", RESOURCE)
        self.addCleanup(held.close)
        witness = {"fd": held.fd, "device": held.identity.st_dev, "inode": held.identity.st_ino}
        env = {channel_lock.LEASE_ENV: json.dumps(witness)} if lease else {}
        with mock.patch.dict(os.environ, env), mock.patch.object(smoke_driver, "_transcripts", side_effect=lambda *_: str(session[-1])), \
                mock.patch.object(driver, "main", main), mock.patch.object(discord.urllib.request, "urlopen", posted):
            if not lease:
                os.environ.pop(channel_lock.LEASE_ENV, None)
            report = smoke_driver.run(args, self.config, spawn=spawn)
        held.close()
        return report, [text for text in posted.sent if not text.startswith("!")], calls

    def test_preflight_refusal_furnishes_bounded_attributed_evidence(self):
        report, prompts, _ = self.drive(detail_fields=degraded_detail(
            TARGET_RETENTION, "relay_verdict_unknown_antigravity_30", "opaque:_private_fixture"))
        self.assertEqual((report["status"], prompts, report["native_turns"]), ("UNEVALUABLE", [], 0))
        witness = report.get("preflight_failure", {})
        self.assertEqual({k: witness.get(k) for k in ("stage", "http_status", "status", "fully_recovered")},
                         {"stage": "detail_readiness", "http_status": 200, "status": "degraded", "fully_recovered": True})
        self.assertCountEqual(witness["reasons"], [
            {"code": "tui_o:reader_retention_stalled", "scope": "target"},
            {"code": "relay_verdict_unknown", "scope": "foreign"},
            {"code": "unclassified", "scope": "unknown"},
        ])
        self.assertNotIn("_private_fixture", json.dumps(report))

    def test_release_herdr_policy_allows_proven_foreign_reasons_through_both_entrypoints(self):
        unknown = "turn_presence_unknown:codex:other:session_identity_unresolved:hosted_active_or_unknown:1:80"
        detail = {
            **degraded_detail(FOREIGN_RETENTION, "relay_verdict_unknown_antigravity_30", unknown),
            "turn_presence_unknown": [
                {"channel_id": 30, "provider": "codex", "agent_id": "other", "reason": "session_identity_unresolved",
                 "hold_leaf": "hosted_active_or_unknown", "elapsed_secs": 80, "unknown_since": "2026-11-20T00:00:00Z"},
            ],
        }
        report, prompts, _ = self.drive(detail_fields=detail)
        self.assertEqual((report["status"], len(prompts), report["native_turns"]), ("PASS", 3, 3))
        self.assertEqual(report["assertions"], [{"key": "turns_delivered_once", "status": "PASS", "complete": True}])
        row = read_json(Path(report["evidence_paths"][0]), {})["scenarios"][0]
        self.assertEqual(len(row["herdr_observations"]), 2)
        for observation in row["herdr_observations"]:
            self.assertEqual(observation["readiness"]["ignored_degraded_reasons_unattributed"], [])

    def test_release_allows_binding_and_bounded_queue_proven_foreign(self):
        codex_queue = "provider:codex:pending_queue_depth:1"
        for reasons, foreign in (([BINDING + "40"], FOREIGN_BOX), ([QUEUE + "1"], FOREIGN_BOX),
                                 ([BINDING + "40", QUEUE + "1"], FOREIGN_BOX),
                                 ([codex_queue], {**FOREIGN_BOX, "provider": "codex"})):
            with self.subTest(reasons=reasons):
                report, prompts, _ = self.drive(detail_fields={
                    **degraded_detail(*reasons), "mailboxes": [TARGET_BOX, foreign]})
                self.assertEqual((report["status"], len(prompts), report["native_turns"]), ("PASS", 3, 3))
                row = read_json(Path(report["evidence_paths"][0]), {})["scenarios"][0]
                self.assertEqual(len(row["herdr_observations"]), 2)
                for observation in row["herdr_observations"]:
                    self.assertEqual(observation["readiness"]["ignored_degraded_reasons"], reasons)

    def test_release_blocks_child_reasons_from_either_end_of_thread_proof(self):
        group = "turn_presence_unknown:claude:other:session_identity_unresolved:none:2:80"
        presence = [{"channel_id": channel, "provider": "claude", "agent_id": "other",
                     "reason": "session_identity_unresolved", "hold_leaf": None, "elapsed_secs": 80,
                     "unknown_since": "2026-11-20T00:00:00Z"} for channel in (40, 41)]
        parent = {**TARGET_BOX, "relay_health": {"thread_channel_id": 40}}
        child = {**FOREIGN_BOX, "relay_health": {"parent_channel_id": 20}}
        for reason, code, boxes in (
            (group, "turn_presence_unknown", [parent]),
            ("relay_verdict_unknown_codex_40", "relay_verdict_unknown", [TARGET_BOX, child]),
            (BINDING + "40", "tui_o:binding_missing", [parent]),
            (QUEUE + "1", "provider:pending_queue_depth", [TARGET_BOX, child]),
        ):
            with self.subTest(reason=reason):
                detail = {**degraded_detail(reason), "mailboxes": boxes}
                if reason == group:
                    detail["turn_presence_unknown"] = presence
                report, prompts, _ = self.drive(detail_fields=detail)
                self.assertEqual((report["status"], prompts, report["native_turns"]), ("UNEVALUABLE", [], 0))
                self.assertIn({"code": code, "scope": "target"}, report["preflight_failure"]["reasons"])

    def test_release_rejects_unproven_queue_and_binding_or_busy_target(self):
        cases = [
            ("target-binding", [BINDING + "20"], [TARGET_BOX], {}),
            ("noncanonical-binding", [BINDING + "040"], [TARGET_BOX], {}),
            ("overflow-binding", [BINDING + str(2**64)], [TARGET_BOX], {}),
            ("above-cap", [QUEUE + "2"], [TARGET_BOX, {**FOREIGN_BOX, "queue_depth": 2}], {}),
            ("target-queue", [QUEUE + "1"], [{**TARGET_BOX, "queue_depth": 1}], {}),
            ("sum-mismatch", [QUEUE + "1"], [TARGET_BOX, {**FOREIGN_BOX, "queue_depth": 2}], {}),
            ("duplicate-box", [QUEUE + "1"], [TARGET_BOX, FOREIGN_BOX, {**FOREIGN_BOX, "channel_id": "40", "queue_depth": 0}], {}),
            ("duplicate-reason", [QUEUE + "1"] * 2, [TARGET_BOX, FOREIGN_BOX], {}),
            ("missing-mailboxes", [QUEUE + "1"], None, {}),
            ("missing-target", [QUEUE + "1"], [FOREIGN_BOX], {}),
            ("bad-depth", [QUEUE + "1"], [TARGET_BOX, {**FOREIGN_BOX, "queue_depth": True}], {}),
            ("mailbox-read-failure", [QUEUE + "1", "mailbox_snapshot_unavailable"], [TARGET_BOX, FOREIGN_BOX],
             {"mailbox_observation_failures": [{"channel_id": 41}]}),
            ("incomplete-read-proof", [QUEUE + "1"], [TARGET_BOX, FOREIGN_BOX],
             {"mailbox_observation_failures": [{"channel_id": 41}]}),
            ("busy-target", [BINDING + "40", QUEUE + "1"], [{**TARGET_BOX, "agent_turn_status": "active"}, FOREIGN_BOX], {}),
            ("global-degradation", [BINDING + "40", QUEUE + "1", "db_slow"], [TARGET_BOX, FOREIGN_BOX], {}),
        ]
        for label, reasons, mailboxes, extra in cases:
            with self.subTest(label=label):
                report, prompts, _ = self.drive(detail_fields={
                    **degraded_detail(*reasons), "mailboxes": mailboxes, **extra})
                self.assertEqual((report["status"], prompts, report["real_provider_contacted"]), ("UNEVALUABLE", [], False))
                if label == "busy-target":
                    self.assertEqual(report["preflight_failure"]["stage"], "busy")

    def test_release_rechecks_queue_scope_after_the_first_observation(self):
        def queue_moves_to_target(detail):
            detail["mailboxes"] = [{**TARGET_BOX, "queue_depth": 1}, {**FOREIGN_BOX, "queue_depth": 0}]

        report, prompts, _ = self.drive(detail_fields={
            **degraded_detail(QUEUE + "1"), "mailboxes": [TARGET_BOX, FOREIGN_BOX]},
            before_final_observe=queue_moves_to_target)
        self.assertEqual(len(prompts), 3)
        self.assertEqual(report["status"], "FAIL")

    def test_live_reason_shapes_share_preflight_policy_with_direct_harness(self):
        smoke = smoke_driver._smoke()[0]
        driver = smoke.cell_driver
        target, relay, binding, queue, presence = (
            1509350778043895902, 1574279895679504388, 1473922824350601297, 70, 71)
        reasons = ["relay_verdict_unknown_claude_1574279895679504388",
                   "tui_o:binding_missing:1473922824350601297", "provider:claude:pending_queue_depth:1",
                   "turn_presence_unknown:claude:ch-ad:session_identity_unresolved:hosted_active_or_unknown:1:602",
                   "turn_presence_unknown:claude:ch-dd:session_identity_unresolved:hosted_active_or_unknown:1:596"]
        codes = ("relay_verdict_unknown", "tui_o:binding_missing", "provider:pending_queue_depth", "turn_presence_unknown")
        cell = "codex-herdr"
        self.config["release_host"]["cells"][cell] = {
            **self.config["release_host"]["cells"]["claude-herdr"], "channel_id": str(target)}
        write_json(self.root / "herdr-status.json", {
            "executions": [{**RUNNING, "provider": "codex", "channel": str(target)}], "other_input_holds": []})
        scenario = {"id": "E-51", "agent_mode": "real_live", "coverage_class": "live", "steps": []}
        for child, code in ((None, None), *zip((relay, binding, queue, presence), codes),
                            (target, "turn_presence_unknown"), ("missing", "turn_presence_unknown")):
            with self.subTest(child=child):
                target_box = {**TARGET_BOX, "provider": "codex", "channel_id": target}
                if code and child not in (target, "missing"):
                    target_box["relay_health"] = {"thread_channel_id": child}
                detail = {**degraded_detail(*reasons), "mailboxes": [target_box, {**FOREIGN_BOX, "channel_id": queue}],
                          "tui_o_writer_readiness": [{"provider": "codex", "channel_id": str(target), "accepts": True}],
                          "turn_presence_unknown": [] if child == "missing" else [
                              {"channel_id": owner, "provider": "claude", "agent_id": agent, "elapsed_secs": age,
                               "reason": "session_identity_unresolved", "hold_leaf": "hosted_active_or_unknown",
                               "unknown_since": "2026-11-22T00:00:00Z"}
                              for owner, agent, age in [(target if child == target else presence, "ch-ad", 602),
                                                        (72, "ch-dd", 596)]]}
                herdr = {"configured_channels": [str(target)]}
                with mock.patch.object(smoke_driver, "_harness", return_value=2) as harness:
                    report, _, _ = self.drive(scenario="E-51", cell=cell, detail_fields=detail, herdr=herdr)
                self.assertEqual(harness.called, code is None, report)

                out = self.root / "direct"
                argv = ["run_tui_relay.py", "--cell", cell, "--filter", "E-51", "--channel-id", str(target),
                        "--base-url", "http://127.0.0.1:9", "--herdr-endpoint", "ep",
                        "--herdr-status-bin", self.config["release_host"]["binary"],
                        "--herdr-foreign-degraded-only", "--output", str(out)]
                client = mock.Mock()
                client.send_control.return_value = {"id": "100"}
                client.fetch_messages.return_value = []
                with mock.patch.object(sys, "argv", argv), mock.patch.object(driver.lease, "DEFAULT_LEASE_DIR", self.root), \
                        mock.patch.object(driver, "load_scenarios", return_value=[scenario]), \
                        mock.patch.object(driver.discord, "DiscordClient", return_value=client), \
                        mock.patch.object(driver.time, "sleep"), contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(driver.main(), 0 if code is None else 1)
                row = read_json(out / f"report.{cell}.json", {})["scenarios"][0]
                if code is None:
                    self.assertEqual(row["herdr_readiness_waiver"]["ignored_degraded_reasons"], sorted(reasons))
                else:
                    expected = {"code": code, "scope": "unknown" if child == "missing" else "target"}
                    self.assertIn(expected, report["preflight_failure"]["reasons"])
                    self.assertIn(expected, row["observation_failure"]["reasons"])
                    client.send_control.assert_not_called()

    def publish_fixture_report(self, report):
        identity = release_host.identity(self.config["release_host"], DETAIL)
        config = {**self.config, **identity, "required_assertions": {"E-50": [smoke_driver.ASSERTION]}}
        row = {**plan.deploy_occurrences(plan.load())[0], **CONTRACT}
        lease = mock.Mock()
        lease.spawn.return_value = subprocess.CompletedProcess([], 0, stdout=json.dumps(report))
        record = {**identity, "run_id": "fixture", "request": {}, "profile": "default", "occurrences": [row]}
        runner._execute(lease, config, record, row, {"turns": 3, "tokens": 300, "time_s": 10})
        return summary.build(record, config)

    def assert_harness_observation(self, stage, read, turns, *, duplicate=False):
        smoke = smoke_driver._smoke()[0]
        driver, sent, messages, reads = smoke.cell_driver, [], [], 0
        transcript = self.root / "transcript.jsonl"
        status_file = self.root / "herdr-status.json"
        markers = [f"[E2E:E50:{{run_id}}:{label}]" for label in ("T1", "T2", "AFTER_CLEAR")]
        scenario = {"id": "E-50", "agent_mode": "real_live", "coverage_class": "live", "report_marker_counts": markers,
                    "steps": [step for marker in markers for step in
                              ({"send_discord_prompt": marker}, {"wait_for_discord_text": marker})]}

        def health(*_args, **_kwargs):
            nonlocal reads
            reads += 1
            if reads == read:
                if stage == "health_read":
                    return 503, {}
                if turns == 0:
                    status_file.write_text("{")  # CLI output reaches the real JSON parser.
                else:
                    status_file.unlink()  # The fixture CLI now exits nonzero.
            return 200, DETAIL

        def send(_client, channel, content):
            driver.discord.input_gate(content)
            sent.append(content)
            lines(transcript, asked(content), claude(str(len(sent))))
            for _ in range(2 if duplicate and len(sent) == 3 else 1):
                messages.append({"id": str(len(messages) + 101), "channel_id": channel, "content": content,
                                 "author": {"id": driver.assertions.provider_bot_id("claude"), "bot": True}})
            return {"id": str(len(messages) + 101)}

        def preflight(argv, **_kwargs):
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(smoke.main(argv[2:]), 0)
            return subprocess.CompletedProcess(argv, 0)

        args = argparse.Namespace(scenario="E-50", cell="claude-herdr", run_id="harness", profile="default",
                                  deploy_sha=None, budget=json.dumps({"turns": 3, "tokens": 300, "time_s": 10}))
        with channel_lock.acquire(self.root / "state", RESOURCE) as held, contextlib.ExitStack() as stack:
            env = {channel_lock.LEASE_ENV: json.dumps({"fd": held.fd, "device": held.identity.st_dev,
                                                      "inode": held.identity.st_ino})}
            for patch in (mock.patch.dict(os.environ, env),
                          mock.patch.object(driver, "load_scenarios", return_value=[scenario]),
                          mock.patch.object(driver, "_read_api_json", side_effect=health),
                          mock.patch.object(driver.lease, "DEFAULT_LEASE_DIR", self.root),
                          mock.patch.object(driver.time, "sleep", return_value=None),
                          mock.patch.object(driver.discord.urllib.request, "urlopen", side_effect=AssertionError("fixture only")),
                          mock.patch.object(driver.discord.DiscordClient, "send", send),
                          mock.patch.object(driver.discord.DiscordClient, "send_control", return_value={"id": "100"}),
                          mock.patch.object(driver.discord.DiscordClient, "fetch_messages", side_effect=lambda *_a, **_kw: messages[:]),
                          mock.patch.object(smoke_driver, "_transcripts", return_value=str(transcript))):
                stack.enter_context(patch)
            report = smoke_driver.run(args, self.config, spawn=preflight)
        value = self.publish_fixture_report(report)
        row = read_json(Path(report["evidence_paths"][0]), {})["scenarios"][0]
        self.assertEqual(reads, read, row.get("reason"))
        self.assertEqual((len(sent), report["native_turns"]), (turns, turns))
        expected = ("FAIL", "FUNCTIONAL_FAILURE") if duplicate else ("UNEVALUABLE", "SERVER_UNOBSERVABLE")
        self.assertEqual((value["result"], value["reason"]), expected)
        self.assertEqual(report["status"], expected[0])
        self.assertEqual(row.get("observation_failure", {}).get("stage"), stage)
        self.assertEqual(value["occurrences"][0]["observation_failure"], row["observation_failure"])
        if duplicate:
            self.assertIn(2, row["marker_counts"].values())
        else:
            self.assertEqual(report["reason"], "SERVER_UNOBSERVABLE")

    def test_harness_health_503_before_first_prompt(self):
        self.assert_harness_observation("health_read", read=3, turns=0)

    def test_harness_invalid_cli_json_before_first_prompt(self):
        self.assert_harness_observation("herdr_status", read=3, turns=0)

    def test_harness_health_loss_after_three_turns(self):
        self.assert_harness_observation("health_read", read=4, turns=3)

    def test_harness_cli_loss_after_three_turns(self):
        self.assert_harness_observation("herdr_status", read=4, turns=3)

    def test_harness_duplicate_with_observation_loss_keeps_functional_failure(self):
        self.assert_harness_observation("herdr_status", read=4, turns=3, duplicate=True)

    def test_mailbox_read_failure_is_unobservable_but_target_queue_is_busy(self):
        for why, depth, expected in (("mailbox_snapshot_unavailable", 0, "SERVER_UNOBSERVABLE"),
                                     (QUEUE + "1", 1, "CELL_NOT_IDLE")):
            with self.subTest(reason=why):
                report, prompts, _ = self.drive(detail_fields={
                    **degraded_detail(why), "mailboxes": [{**TARGET_BOX, "queue_depth": depth}]})
                value = self.publish_fixture_report(report)
                self.assertEqual((value["result"], value["reason"], prompts), ("UNEVALUABLE", expected, []))
                if depth == 0:
                    self.assertEqual(report["preflight_failure"]["stage"], "mailbox_read")

    def test_final_identity_read_loss_is_unobservable_after_three_turns(self):
        for duplicate, expected in ((False, ("UNEVALUABLE", "SERVER_UNOBSERVABLE")),
                                    (True, ("FAIL", "FUNCTIONAL_FAILURE"))):
            with self.subTest(duplicate=duplicate):
                def lose_identity(state):
                    self.detail.side_effect = OSError("fixture identity read failure")
                    if duplicate:
                        counts = state["row"]["marker_counts"]
                        counts[next(iter(counts))] = 2

                self.detail.side_effect = None
                report, prompts, _ = self.drive(harness=lose_identity)
                self.assertEqual((len(prompts), report["native_turns"]), (3, 3))
                self.assertEqual(report["status"], expected[0])
                self.assertEqual(report["assertions"][0]["complete"], duplicate)
                if not duplicate:
                    self.assertEqual(report["reason"], expected[1])
                value = self.publish_fixture_report(report)
                self.assertEqual((value["result"], value["reason"]), expected)

    def test_cli_input_hold_is_busy_and_cli_read_failure_is_observability(self):
        write_json(self.root / "herdr-status.json",
                   {"executions": [{**RUNNING, "input_hold": {"recorded_at": "now"}}], "other_input_holds": []})
        for cli, stage in ((True, "busy"), (False, "herdr_status")):
            with self.subTest(stage=stage):
                report, prompts, _ = self.drive(cli=cli)
                self.assertEqual((report["status"], prompts, report["preflight_failure"]["stage"]),
                                 ("UNEVALUABLE", [], stage))

    def test_non_200_second_health_read_reports_observability(self):
        smoke = smoke_driver._smoke()[0]  # noqa: SLF001
        out = io.StringIO()
        with mock.patch.object(smoke.cell_driver, "_read_api_json", side_effect=[(200, DETAIL), (503, {})]), \
                contextlib.redirect_stdout(out):
            self.assertEqual(smoke.main(["preflight", "--failure-json", "--base-url", "http://127.0.0.1:9",
                                         "--cell", "claude-herdr", "--channel-id", "20"]), 2)
        self.assertEqual(json.loads(out.getvalue()), {"stage": "health_read", "http_status": 503})

    def test_release_herdr_policy_refuses_target_and_unattributed_reasons(self):
        for reason in (TARGET_RETENTION, UNATTRIBUTED_RETENTION, "relay_verdict_unknown_codex_20",
                       "turn_presence_unknown:codex:other:session_identity_unresolved:hosted_active_or_unknown:1:80"):
            with self.subTest(reason=reason):
                report, prompts, _ = self.drive(detail_fields=degraded_detail(reason))
                self.assertEqual((report["status"], prompts, report["real_provider_contacted"]), ("UNEVALUABLE", [], False))

    def test_release_herdr_policy_revalidates_after_initial_foreign_reason(self):
        def target_appears(detail):
            detail["degraded_reasons"] = [TARGET_RETENTION]

        report, prompts, _ = self.drive(detail_fields=degraded_detail(FOREIGN_RETENTION),
                                        before_final_observe=target_appears)
        self.assertEqual(len(prompts), 3)
        self.assertNotEqual(report["status"], "PASS")

    def test_herdr_profile_without_opt_in_requires_healthy_detail(self):
        report, prompts, _ = self.drive(detail_fields=degraded_detail(FOREIGN_RETENTION), profile="other")
        self.assertEqual((report["status"], prompts, report["native_turns"]), ("UNEVALUABLE", [], 0))

    def test_passing_scenario_reports_turns_tokens_identity_and_the_host_the_server_admits(self):
        stdout = os.fstat(1)
        report, prompts, spawned = self.drive()
        # The harness wrote to its log; the driver's own stdout still carries the report to the runner.
        self.assertTrue(os.path.samestat(os.fstat(1), stdout) and "[e2e] harness output" in Path(report["evidence_paths"][1]).read_text())
        planned = next(row for row in plan.deploy_occurrences(plan.load()) if row["scenario"] == "E-50")
        self.assertEqual((report["status"], report["native_turns"], report["tokens"], report["budget_checked"]), ("PASS", 3, 21, 3))
        self.assertEqual(report["usage"], {"native_turns": 3, "input_tokens": 3, "output_tokens": 6,
                                           "cache_read_tokens": 9, "cache_write_tokens": 12})
        self.assertEqual(report["actual_occurrence_key"], summary.occurrence_key({**planned, **CONTRACT}, "default"))
        self.assertEqual((report["head"], report["boot_id"], report["instance_id"]), (HEAD, "boot-1", "release@host"))
        # A Herdr channel is proven idle and judged on Herdr detail and CLI evidence, not the legacy mailbox.
        self.assertIn("herdr=detail+CLI", report["verdict"])
        # The input writer is this lease-holding process: no harness child exists to outlive it.
        self.assertEqual((len(prompts), self.writer, [argv for argv in spawned if "preflight" not in argv]), (3, os.getpid(), []))
        self.assertTrue(report["real_provider_contacted"] and all(Path(path).is_file() for path in report["evidence_paths"]))
        report, _, _ = self.drive(harness=lambda state: state["row"].pop("herdr_observations"))
        self.assertEqual(report["status"], "FAIL", "a Herdr result without its evidence is not a pass")
        report, _, _ = self.drive(host="tmux")
        self.assertEqual((report["status"], list((self.root / "state").rglob("server.log"))), ("PASS", []))
        self.assertEqual(report["actual_occurrence_key"], summary.occurrence_key({**planned, **CONTRACT, "host": "tmux"}, "default"))

    def test_no_input_follows_a_turn_whose_real_usage_is_unknown_or_over_its_limit(self):
        report, prompts, _ = self.drive(answers=["silent"])
        self.assertEqual((len(prompts), report["status"], report["reason"]), (1, "UNEVALUABLE", "TURN_USAGE_UNKNOWN"))
        self.assertEqual((report["native_turns"], report["tokens"], report["usage"]["input_tokens"]), (None, None, None))
        report, prompts, _ = self.drive(answers=["huge"])
        self.assertEqual((len(prompts), report["status"], report["reason"]), (1, "UNEVALUABLE", "TURN_OVER_BUDGET"))
        self.assertEqual((report["native_turns"], report["tokens"], report["budget_checked"]), (1, 505, 1))
        report, prompts, _ = self.drive(answers=["cached"] * 3)  # Cache reads stay in usage but are not charged.
        self.assertEqual((len(prompts), report["status"], report["tokens"], report["usage"]["cache_read_tokens"]), (3, "PASS", 21, 1500))
        # The last turn is settled after the scenario passed; its unknown or over-limit usage still holds the result.
        report, prompts, _ = self.drive(answers=[None, None, "silent"])
        self.assertEqual((len(prompts), report["status"], report["reason"], report["tokens"]), (3, "UNEVALUABLE", "TURN_USAGE_UNKNOWN", None))
        report, prompts, _ = self.drive(answers=[None, None, "huge"])
        self.assertEqual((len(prompts), report["status"], report["reason"], report["tokens"]), (3, "UNEVALUABLE", "TURN_OVER_BUDGET", 519))
        report, prompts, _ = self.drive(budget_left={"turns": 3, "tokens": 300, "time_s": 10, "deadline": self.now - 1})
        self.assertEqual((prompts, report["reason"], report["native_turns"]), ([], "TURN_BUDGET_EXHAUSTED", 0))
        for stray in ("an unplanned prompt", "!stop"):
            report, prompts, _ = self.drive(stray=stray)
            self.assertEqual((len(prompts), report["reason"], report["native_turns"]), (1, "UNPLANNED_PROVIDER_INPUT", 1))

    def gated(self, provider, lookup, deadline, *sends):
        """Post through the real Discord client behind a fresh gate; a send may write the transcript after it."""
        discord, posted = smoke_driver._smoke()[0].cell_driver.discord, Posted()  # noqa: SLF001
        gate = turn_gate.TurnGate(budget.TurnGuard({"turns": 3, "tokens": 300, "time_s": 10, "deadline": deadline}),
                                  provider, ["M1", "M2"], 100, lookup, 0)
        with mock.patch.object(discord, "input_gate", gate.before_send), mock.patch.object(discord.urllib.request, "urlopen", posted):
            for content, after in sends:
                with contextlib.suppress(turn_gate.Stop):
                    discord.DiscordClient(base_url="http://127.0.0.1:9").send("1", content)
                    after()
        return posted.sent, gate.withheld

    def test_no_input_is_sent_after_the_deadline_even_when_the_lookup_or_a_command_comes_late(self):
        log, deadline = str(self.root / "transcript.jsonl"), self.now + 1

        def lookup():
            self.now = deadline
            return log

        late = self.gated("claude", lookup, deadline, ("say M1", lambda: None))
        expired = self.gated("claude", lambda: log, deadline - 1, ("!clear now", lambda: None))
        self.assertEqual((late, expired), (([], "TURN_BUDGET_EXHAUSTED"), ([], "TURN_BUDGET_EXHAUSTED")))

    def test_a_codex_turn_missing_its_final_usage_or_over_its_limit_stops_the_next_prompt(self):
        log, call = self.root / "rollout.jsonl", {"input_tokens": 30, "cached_input_tokens": 0, "output_tokens": 5}
        for calls, drop, reason in (([call, call], 1, "TURN_USAGE_UNKNOWN"), ([call] * 4, None, "TURN_OVER_BUDGET")):
            log.write_text("")
            sent = self.gated("codex", lambda: str(log), self.now + 10,
                              ("say M1", lambda: lines(log, *codex_turn("t1", "M1", calls, drop=drop))), ("say M2", lambda: None))
            self.assertEqual(sent, (["say M1"], reason))

    def test_refused_runs_send_no_prompt_and_charge_nothing(self):
        cases = [({"cell": "codex-herdr"}, "CELL_NOT_ENABLED"), ({"lease": False}, "CHANNEL_LEASE_MISSING"),
                 ({"budget_left": {"turns": 2, "tokens": 300, "time_s": 10}}, "BUDGET_TOO_SMALL_FOR_SCENARIO"),
                 ({"budget_left": {"turns": 3, "tokens": 299, "time_s": 10}}, "BUDGET_TOO_SMALL_FOR_SCENARIO"),
                 ({"budget_left": {"turns": 3, "tokens": 300, "time_s": 9}}, "BUDGET_TOO_SMALL_FOR_SCENARIO"),
                 ({"herdr": {"input_holds": 1}}, "CELL_NOT_IDLE"), ({"cli": False}, "CELL_NOT_IDLE"),
                 ({"herdr": {"configured_channels": []}}, "HOST_MISMATCH: planned=herdr observed=tmux"),
                 ({"herdr": {"endpoints": {"a": {"local": True}, "b": {"local": True}}}}, "HERDR_ENDPOINT_AMBIGUOUS"),
                 ({"scenario": "E-63"}, "SCENARIO_NOT_GATED"), ({"scenario": "E-64"}, "SCENARIO_NOT_GATED")]
        for options, reason in cases:
            with self.subTest(reason=reason, options=options):
                report, prompts, _ = self.drive(**options)
                self.assertEqual((report["status"], report["reason"][:len(reason)], prompts, self.writer), ("UNEVALUABLE", reason, [], None))
                self.assertEqual((report["native_turns"], report["tokens"], report["real_provider_contacted"]), (0, 0, False))
                write_json(self.root / "herdr-status.json", {"executions": [RUNNING], "other_input_holds": []})

    def test_lost_observation_is_unevaluable_but_a_seen_or_unsent_failure_is_fail(self):
        def timeout(state, duplicate=False, reason="TimeoutError: timed out"):
            first, second, _third = state["row"]["marker_counts"]
            state["row"].update(status="fail", reason=reason,
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

        def withheld(state):
            *_, last = state["row"]["marker_counts"]
            state["row"]["marker_counts"][last] = 0
            state["row"].update(status="fail", reason="wait_for_discord_text: marker not seen before the deadline")
            state["rc"] = 1

        report, _, _ = self.drive(harness=timeout, prompts=2, host="tmux")
        self.assertEqual((report["status"], report["reason"], report["native_turns"]), ("UNEVALUABLE", "UNOBSERVED_TRANSPORT", 2))
        stalled = f"FetchTransportTimeout: {discord.FetchTransportTimeout(4, TimeoutError('timed out'), [])}"
        self.assertEqual(self.drive(harness=lambda state: timeout(state, reason=stalled), prompts=2, host="tmux")[0]["reason"], "UNOBSERVED_TRANSPORT")
        self.assertEqual(report["assertions"], [{"key": "turns_delivered_once", "status": "UNEVALUABLE", "complete": False}])
        self.assertEqual(self.drive(harness=lambda state: state.update(rotate=True), host="tmux")[0]["reason"], "UNOBSERVED_SERVER_LOG")
        for broken in (lambda state: timeout(state, duplicate=True), missing, unsent, refused, residue):
            report, _, _ = self.drive(harness=broken, prompts=2 if broken is not residue else 3, host="tmux")
            self.assertEqual((report["status"], report["assertions"][0]["status"]), ("FAIL", "FAIL"))
        report, _, _ = self.drive(harness=lambda state: timeout(state, duplicate=True), answers=["silent"], host="tmux")
        self.assertEqual((report["status"], report["tokens"]), ("FAIL", None), "a budget stop does not hide a seen duplicate")
        # Every prompt went out and the last body never came; its missing usage does not soften that.
        report, prompts, _ = self.drive(harness=withheld, answers=[None, None, "silent"])
        self.assertEqual((len(prompts), report["status"], report["tokens"]), (3, "FAIL", None))


class Supervisor(Fixture):
    def test_scenario_capability_opens_that_scenario_only_past_the_provider_wide_gap(self):
        manifest, e51, e1 = plan.load(), {"scenario": "E-51", "cell": "codex-herdr"}, {"scenario": "E-1", "cell": "codex-herdr"}
        self.assertIsNone(plan.gate(manifest, e51, ["E-51/herdr"]))
        for row, granted in ((e51, []), (e1, ["E-51/herdr"]), ({"scenario": "E-13", "cell": "claude-pipe"}, ["E-13/pipe"])):
            self.assertEqual(plan.gate(manifest, row, granted)[0], "BLOCKED_CAPABILITY", (row, granted))

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
    def hook(self, *, config=True, python=None, launchctl="", limit="20", pass_fds=()):
        home, tools = self.root / "home", self.root / "tools"
        (home / "Library").mkdir(parents=True, exist_ok=True)
        tools.mkdir(exist_ok=True)
        (tools / "launchctl").write_text(f"#!/bin/bash\necho \"$@\" >> {self.root}/launchctl.log\n{launchctl}[ \"$1\" != print ]\n")
        (tools / "python3").write_text(python or f"#!/bin/sh\nexec {sys.executable} \"$@\"\n")
        for tool in tools.iterdir():
            tool.chmod(0o755)
        env = {"HOME": str(home), "PATH": f"{tools}:/usr/bin:/bin",
               "AGENTDESK_E2E_CONTINUOUS_CONFIG": str(self.path if config else self.root / "absent.json")}
        done = subprocess.run(["bash", str(REPO / "scripts/e2e/continuous/after_deploy.sh"), str(REPO), HEAD, "stamp", limit],
                              env=env, capture_output=True, text=True, timeout=60, pass_fds=pass_fds)
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
        # Placeholders are __UPPER__ tokens; the temp root itself may contain "__".
        self.assertTrue(f"<string>{REPO}</string>" in text and str(self.path) in text and not re.search(r"__[A-Z]+__", text))
        self.assertIn("bootstrap", calls)
        # Switched off with the job, the request and an enabled status all still in place.
        publication.heartbeat(self.root / "state", self.config, 1000)
        (self.root / "state/inbox/deploy-stamp.json").unlink()
        self.save(enabled=False)
        done, later, _ = self.hook()
        self.assertIn('"status": "DISABLED"', done.stdout)
        self.assertEqual((later, (self.root / "state/inbox/deploy-stamp.json").exists()), (calls, False))
        shown = consumer.view(self.root / "state/status.json", head=HEAD, now=6000)
        self.assertEqual((shown["check"]["result"], shown["monitor"]["enabled"]), ("DISABLED", 0))

    def test_a_stalled_step_is_cut_off_at_the_time_limit_and_does_not_outlive_the_deploy(self):
        tools = self.root / "tools"
        tools.mkdir()
        watchdog_limit = self.root / "watchdog-limit"
        submitted = "#!/bin/sh\necho '{\"status\": \"SUBMITTED\"}'\n"
        for stage in ("python", "launchctl"):
            with self.subTest(stage=stage), contextlib.ExitStack() as pipes:
                ready_read, ready_write = os.pipe()
                hold_read, hold_write = os.pipe()
                exit_read, exit_write = os.pipe()
                ready = pipes.enter_context(os.fdopen(ready_read, "rb"))
                entering = pipes.enter_context(os.fdopen(ready_write, "wb"))
                held = pipes.enter_context(os.fdopen(hold_read, "rb"))
                pipes.enter_context(os.fdopen(hold_write, "wb"))
                exited = pipes.enter_context(os.fdopen(exit_read, "rb"))
                writer = pipes.enter_context(os.fdopen(exit_write, "wb"))
                (tools / "sleep").write_text(
                    f'#!/bin/bash\nexec {entering.fileno()}>&-\nread -r entered <&{ready.fileno()} || exit 1\n'
                    f'printf "%s\\n" "$1" > "{watchdog_limit}"\n'
                )
                stall = f"echo ready >&{entering.fileno()}; exec /bin/cat <&{held.fileno()}"
                options = {"python": "#!/bin/bash\n" + stall} if stage == "python" else {
                    "python": submitted, "launchctl": f'[ "$1" = print ] && {{ {stall}; }}\n',
                }
                pass_fds = tuple(pipe.fileno() for pipe in (ready, entering, held, writer))
                done, _, _ = self.hook(limit="3", pass_fds=pass_fds, **options)
                writer.close()
                self.assertEqual(done.returncode, 0, done.stderr)
                self.assertIn("did not finish within 3s", done.stdout)
                self.assertEqual(watchdog_limit.read_text(), "3\n")
                watchdog_limit.unlink()
                # EOF proves the stalled process closed its inherited descriptor even if its PID is being reaped.
                self.assertTrue(select.select([exited], [], [], 15)[0], "the stalled process kept its descriptor")
                self.assertEqual(exited.read(1), b"")


if __name__ == "__main__":
    unittest.main()
