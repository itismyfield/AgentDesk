"""Fixture-only coverage for the read-only source comparator."""

import contextlib
import io
import json
import sys
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

from . import source_compare as sc
from .assertions import OUR_BOT_ID

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import run_tui_relay as driver  # noqa: E402


MARKER = "[E2E:S5:fixture:ONE]"


def message(mid, text=MARKER, *, author="relay", bot=True):
    return {"id": str(mid), "content": text, "author": {"id": author, "bot": bot}}


class SourceCompareTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.path = self.root / "native.jsonl"
        self.write([self.user()])
        self.messages = [message(1, author=OUR_BOT_ID), message(2)]

    def user(self, text=MARKER, **extras):
        return {"type": "user", "message": {"content": text}, **extras}

    def write(self, rows):
        self.path.write_text("".join(json.dumps(row) + "\n" for row in rows))

    def compare(self, messages=None):
        return sc.compare(self.path, self.messages if messages is None else messages,
                          run_id="fixture")[MARKER]

    def test_ok(self):
        self.assertEqual(self.compare(), dict(native_user_count=1, discord_input_mirror_count=1,
                                             relay_body_count=1, direct_input_notice_count=0, verdict="ok"))

    def test_lost_native_input(self):
        self.write([])
        self.assertEqual(self.compare()["verdict"], "lost")

    def test_lost_input_mirror(self):
        self.assertEqual(self.compare([message(2)])["verdict"], "lost")

    def test_duplicate_native(self):
        self.write([self.user(), self.user()])
        self.assertEqual(self.compare()["verdict"], "duplicated")

    def test_duplicate_discord_surfaces(self):
        for duplicate in [message(3), message(3, author=OUR_BOT_ID)]:
            with self.subTest(duplicate=duplicate):
                self.assertEqual(self.compare(self.messages + [duplicate])["verdict"], "duplicated")

    def test_missing_relay(self):
        self.assertEqual(self.compare(self.messages[:1])["verdict"], "missing_relay")

    def test_direct_notice_is_not_relay_or_mirror(self):
        notice = message(1, f"터미널에 직접 주입된 입력 (tmux : `session`):\n```text\n{MARKER}\n```")
        result = self.compare([notice, message(2)])
        self.assertEqual((result["verdict"], result["direct_input_notice_count"],
                          result["discord_input_mirror_count"], result["relay_body_count"]), ("ok", 1, 0, 1))
        self.assertEqual(self.compare([notice])["verdict"], "missing_relay")
        self.assertEqual(self.compare([notice, {**notice, "id": "3"}, message(2)])["verdict"], "duplicated")

    def test_explicit_webhook_mirror_author_is_not_relay(self):
        result = sc.compare(self.path, [message(1, author="webhook"), message(2)],
                            run_id="fixture", input_mirror_author_ids=("webhook",))[MARKER]
        self.assertEqual((result["discord_input_mirror_count"], result["relay_body_count"],
                          result["verdict"]), (1, 1, "ok"))

    def test_real_human_is_input(self):
        self.assertEqual(self.compare([message(1, bot=False), message(2)])["verdict"], "ok")

    def test_tool_results_assistant_and_meta_are_not_inputs(self):
        self.write([self.user(), self.user(isMeta=True), self.user(isCompactSummary=True),
                    {"type": "assistant", "message": {"content": MARKER}},
                    self.user([{"type": "tool_result", "content": MARKER},
                               {"type": "text", "text": MARKER}])])
        self.assertEqual(self.compare()["native_user_count"], 1)

    def test_identified_task_notification_is_not_native_user_input(self):
        self.write([self.user(), self.user("<task-notification><task-id>task</task-id>"
            "<tool-use-id>tool</tool-use-id><status>completed</status><summary>" + MARKER +
            "</summary></task-notification>")])
        self.assertEqual(self.compare()["native_user_count"], 1)

    def test_codex_response_item_is_counted_once_not_event_mirror(self):
        self.write([{"type": "event_msg", "payload": {"type": "user_message", "message": MARKER}},
                    {"type": "response_item", "payload": {"type": "message", "role": "user",
                     "content": [{"type": "input_text", "text": MARKER}]}},
                    {"type": "response_item", "payload": {"type": "message", "role": "assistant",
                     "content": [{"type": "output_text", "text": MARKER}]}}])
        self.assertEqual(self.compare()["verdict"], "ok")

    def test_repeated_marker_in_one_input_is_one_input(self):
        self.write([self.user(MARKER + " " + MARKER)])
        self.assertEqual(self.compare()["native_user_count"], 1)

    def test_same_message_id_edits_not_duplicates(self):
        self.assertEqual(self.compare(self.messages + [message(2)])["verdict"], "ok")
        self.assertEqual(self.compare(self.messages + [message(2, "...")])["verdict"], "missing_relay")

    def test_chrome_marker_is_not_relay(self):
        self.assertEqual(self.compare([self.messages[0], message(2, "✅ " + MARKER)])["verdict"], "missing_relay")

    def test_multiple_markers_and_named_regex(self):
        other = MARKER.replace("ONE", "TWO")
        self.write([self.user(), self.user(other)])
        result = sc.compare(self.path, self.messages + [message(3, other, author=OUR_BOT_ID)],
                            marker_regex=r"(?P<marker>\[E2E:S5:fixture:\w+\])")
        self.assertEqual([row["verdict"] for row in result.values()], ["ok", "missing_relay"])

    def test_empty_or_malformed_evidence_fails(self):
        for kwargs in [{"run_id": "absent"}, {"marker_regex": ".*"}]:
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                sc.compare(self.path, self.messages, **kwargs)
        self.path.write_text('{"type":')
        with self.assertRaisesRegex(ValueError, "invalid transcript"):
            self.compare()

    def test_resolves_documented_session_key_and_exact_file(self):
        project = self.root / "project"
        project.mkdir()
        (project / "native-id.jsonl").write_text("")
        identity = {"agent_id": "worker", "channel_id": "123", "provider": "claude",
                    "session_key": "claude/token/host:worker", "raw_provider_session_id": "native-id"}
        for selector, kwargs in [("123", {"channel_id": "123"}), ("worker", {"agent_id": "worker"})]:
            with patch.object(sc, "get_json", return_value=identity) as get:
                result = sc.resolve_binding("http://local", transcript_root=self.root, **kwargs)
            self.assertEqual(result["transcript_path"], str(project / "native-id.jsonl"))
            get.assert_called_once_with("http://local", f"/api/agents/{selector}/session-evidence")

    def test_missing_empty_unsafe_raw_identity_never_uses_legacy_fallback(self):
        project = self.root / "project"
        project.mkdir()
        (project / "native-id.jsonl").write_text(self.path.read_text())
        for sid in [None, "", "../native-id", "native-id ", 123]:
            identity = {"agent_id": "worker", "channel_id": "123", "provider": "claude",
                        "session_key": "key", "raw_provider_session_id": sid,
                        "session_id": "native-id", "claude_session_id": "native-id"}
            with self.subTest(sid=sid), patch.object(sc, "get_json", return_value=identity), \
                 patch.object(sc, "fetch_messages", return_value=self.messages) as fetch, \
                 contextlib.redirect_stderr(io.StringIO()) as err, contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(sc.main(["--agent-id", "worker", "--run-id", "fixture",
                    "--transcript-root", str(self.root)]), 2)
                self.assertIn("raw_provider_session_id", err.getvalue())
                fetch.assert_not_called()

    def test_mismatched_snapshot_fails_closed(self):
        with patch.object(sc, "get_json", return_value={"agent_id": "other", "channel_id": "999"}), self.assertRaises(ValueError):
            sc.resolve_binding("http://local", agent_id="worker", channel_id="123")

    def test_unknown_marker_bearing_native_shapes_fail_closed(self):
        unknowns = [{"type": "future_type", "text": MARKER},
                    {"type": "user", "message": [MARKER]},
                    {"type": "user", "message": {"content": [{"type": "text", "value": MARKER}]}},
                    {"type": "user", "message": {"content": [{"type": "text", "text": [MARKER]}]}},
                    {"type": "response_item", "payload": [MARKER]},
                    {"type": "event_msg", "payload": [MARKER]},
                    {"type": "system", "subtype": "future_subtype", "text": MARKER},
                    {"type": "user", "message": {"content": [{"type": "future_input", "text": MARKER}]}},
                    {"type": "response_item", "payload": {"type": "future_input", "text": MARKER}},
                    {"type": "response_item", "payload": {"type": "message", "role": "user",
                        "content": [{"type": "future_input", "text": MARKER}]}},
                    {"type": "event_msg", "payload": {"type": "future_event", "text": MARKER}}]
        for row in unknowns:
            self.write([self.user(), row])
            with self.subTest(row=row), self.assertRaisesRegex(ValueError, "unsupported marker-bearing"):
                self.compare()
            binding = {"channel_id": "123", "transcript_path": str(self.path)}
            with patch.object(sc, "resolve_binding", return_value=binding), \
                 patch.object(sc, "fetch_messages", return_value=self.messages), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(sc.main(["--agent-id", "worker", "--run-id", "fixture"]), 2)
        self.write([self.user(), {"type": "future_type", "text": "unrelated"}])
        self.assertEqual(self.compare()["verdict"], "ok")

    def test_pagination_uses_existing_read_client(self):
        first = [message(i) for i in range(1, 101)]
        with patch.object(sc.DiscordClient, "fetch_messages", side_effect=[first, [message(101)]]) as fetch:
            self.assertEqual(len(sc.fetch_messages("http://local", "123")), 101)
        self.assertEqual(fetch.call_args.kwargs["after_id"], "100")
        with patch.object(sc.DiscordClient, "fetch_messages", return_value=first), self.assertRaises(ValueError):
            sc.fetch_messages("http://local", "123", max_pages=1)

    def test_cli_marker_verdict_exit_codes(self):
        binding = {"channel_id": "123", "transcript_path": str(self.path)}
        with patch.object(sc, "resolve_binding", return_value=binding), \
             patch.object(sc, "fetch_messages", return_value=self.messages), \
             contextlib.redirect_stdout(io.StringIO()) as out:
            self.assertEqual(sc.main(["--agent-id", "worker", "--run-id", "fixture"]), 0)
        self.assertEqual(json.loads(out.getvalue())["markers"][MARKER]["verdict"], "ok")
        self.write([])
        with patch.object(sc, "resolve_binding", return_value=binding), \
             patch.object(sc, "fetch_messages", return_value=self.messages), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(sc.main(["--agent-id", "worker", "--run-id", "fixture"]), 1)

    def main_rc(self, messages, *extra):
        binding = {"channel_id": "123", "transcript_path": str(self.path)}
        with patch.object(sc, "resolve_binding", return_value=binding), \
             patch.object(sc, "fetch_messages", return_value=messages), contextlib.redirect_stdout(io.StringIO()):
            return sc.main(["--agent-id", "worker", "--run-id", "fixture", *extra])

    def test_claude_last_prompt_copy_is_ignored_not_counted(self):
        # Shapes observed in the Phase A Claude transcript: with and without the prompt copy.
        copies = [{"type": "last-prompt", "lastPrompt": "[User: x] " + MARKER, "leafUuid": "a", "sessionId": "s"},
                  {"type": "last-prompt", "leafUuid": "b", "sessionId": "s"}]
        self.write([self.user()] + copies + copies[:1])
        self.assertEqual(self.compare()["verdict"], "ok")
        self.assertEqual(self.main_rc(self.messages), 0)
        for row in [{**copies[0], "message": {"content": MARKER}}, {**copies[0], "lastPrompt": [MARKER]},
                    {"type": "last-prompt", "sessionId": MARKER}, {"type": "last-prompt", "lastPrompt": MARKER},
                    # Only lastPrompt is a prompt copy; exact-shape metadata must not hide a marker.
                    {"type": "last-prompt", "leafUuid": MARKER, "sessionId": "s"},
                    {"type": "last-prompt", "leafUuid": "leaf", "sessionId": MARKER},
                    {**copies[0], "leafUuid": MARKER}, {**copies[0], "lastPrompt": "x", "sessionId": MARKER},
                    {**copies[1], "leafUuid": [MARKER]}, {**copies[0], "sessionId": {"probe": MARKER}}]:
            self.write([self.user(), row])
            with self.subTest(row=row), self.assertRaisesRegex(ValueError, "unsupported marker-bearing"):
                self.compare()
        self.assertEqual(self.main_rc(self.messages), 2)  # the nested metadata marker reaches the entry point

    def test_text_command_marker_is_command_not_lost(self):
        command = message(1, "!clear " + MARKER, author=OUR_BOT_ID)
        self.write([])
        self.assertEqual(self.compare([command])["verdict"], "command")
        self.assertEqual(self.main_rc([command]), 0)
        # A command that reached the provider or relay is a defect, and plain prompts stay strict.
        self.write([self.user("!clear " + MARKER)])
        self.assertEqual(self.compare([command])["verdict"], "command_forwarded")
        self.assertEqual(self.main_rc([command]), 1)
        self.write([])
        self.assertEqual(self.compare([message(1, "clear " + MARKER, author=OUR_BOT_ID)])["verdict"], "lost")
        self.assertEqual(self.compare([command, message(3, "!clear " + MARKER, author=OUR_BOT_ID)])["verdict"],
                         "duplicated")

    def test_deliver_input_without_mirror_is_known_gap_but_relay_loss_stays_strict(self):
        header = ('[Headless trigger context]\nsource: e2e\nmetadata: {"human_input":{"origin_id":"%s"}}\n\n'
                  "[User: e2e:1 (ID: 1)] reply %s" % (MARKER, MARKER))
        codex = {"type": "response_item", "payload": {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": header}]}}
        claude = self.user('\n\n<pasted_content id="e1">\n' + header + '\n</pasted_content id="e1">\n')
        body = [message(2)]
        for row in (codex, claude):
            self.write([row])
            with self.subTest(row=row["type"]):
                result = self.compare(body)
                self.assertEqual((result["verdict"], result["known_gap"]), ("known_gap", sc.DELIVER_MIRROR_GAP))
                self.assertEqual(self.main_rc(body), 0)
                self.assertEqual(self.compare([])["verdict"], "missing_relay")
                self.assertEqual(self.main_rc([]), 1)
                self.assertEqual(self.compare(body + [message(3)])["verdict"], "duplicated")
        queued = self.user("[User: owner (ID: 77)] reply " + MARKER)
        self.write([queued])
        result = sc.compare(self.path, body, run_id="fixture", deliver_author_ids=("77",))[MARKER]
        self.assertEqual(result["verdict"], "known_gap")
        self.assertEqual((self.main_rc(body, "--deliver-author-id", "77"), self.main_rc(body)), (0, 1))
        self.assertEqual(sc.compare(self.path, [], run_id="fixture", deliver_author_ids=("77",))[MARKER]["verdict"],
                         "missing_relay")
        for text in (header.replace("human_input", "other_input"), "[User: e2e] reply " + MARKER,
                     "[User: owner (ID: 77)] reply " + MARKER):
            self.write([self.user(text)])
            with self.subTest(text=text[:30]):
                self.assertEqual(self.compare(body)["verdict"], "lost")
        self.assertEqual(sc.compare(self.path, body, run_id="fixture", deliver_author_ids=("78",))[MARKER]["verdict"],
                         "lost")

    def test_json_transport_uses_get_without_claiming_server_purity(self):
        with patch.object(sc.urllib.request, "urlopen") as urlopen:
            urlopen.return_value.__enter__.return_value = io.StringIO('{}')
            self.assertEqual(sc.get_json("http://local", "/api/docs"), {})
        self.assertEqual(urlopen.call_args.args[0].method, "GET")


class AutonomousEvidenceTests(unittest.TestCase):
    write = SourceCompareTests.write
    user = SourceCompareTests.user
    ARMED, AUTO = "[E2E:S4:fixture:ARMED]", "[E2E:S4:fixture:AUTO]"

    def setUp(self):
        SourceCompareTests.setUp(self)
        self.rows = [
            {"type": "user", "uuid": "prompt", "message": {"content": self.ARMED + " " + self.AUTO}},
            {"type": "assistant", "uuid": "bash", "parentUuid": "prompt", "message": {"content": [
                {"type": "tool_use", "id": "tool-1", "name": "Bash", "input": {"run_in_background": True}}]}},
            {"type": "user", "uuid": "result", "parentUuid": "bash", "toolUseResult": {"backgroundTaskId": "task-1"},
             "message": {"content": [{"type": "tool_result", "tool_use_id": "tool-1", "content": "running"}]}},
            {"type": "assistant", "uuid": "armed", "parentUuid": "result",
             "message": {"content": [{"type": "text", "text": self.ARMED}], "stop_reason": "end_turn"}},
            {"type": "user", "uuid": "notice", "parentUuid": "armed", "isMeta": True, "message": {"content":
             "<task-notification><task-id>task-1</task-id><tool-use-id>tool-1</tool-use-id>"
             "<status>completed</status></task-notification>"}},
            {"type": "assistant", "uuid": "auto", "parentUuid": "notice",
             "message": {"content": [{"type": "text", "text": self.AUTO}], "stop_reason": "end_turn"}}]

    def evidence(self):
        self.write(self.rows)
        return sc.autonomous_background_turn(self.path, armed_marker=self.ARMED, auto_marker=self.AUTO)

    def test_s4_linked_native_turns(self):
        self.assertEqual(self.evidence(), {"bash_tool_use_id": "tool-1", "task_id": "task-1",
            "armed_uuid": "armed", "notification_uuid": "notice", "auto_uuid": "auto"})
        self.rows.append({"type": "user", "message": {"content": "later unrelated input"}})
        self.assertEqual(self.evidence()["auto_uuid"], "auto")

    def test_s4_system_notification_and_textual_background_identity(self):
        self.rows[2].pop("toolUseResult")
        self.rows[2]["message"]["content"][0]["content"] = "Command running in background with ID: task-1."
        self.rows[4] = {"type": "system", "subtype": "task_notification", "uuid": "notice",
            "parentUuid": "armed", "task_id": "task-1", "tool_use_id": "tool-1", "status": "completed"}
        self.assertEqual(self.evidence()["task_id"], "task-1")

    def test_s4_non_meta_structured_notice_and_turn_duration_terminal(self):
        self.rows[4].pop("isMeta")
        self.assertEqual(self.evidence()["task_id"], "task-1")
        self.rows[3]["message"]["stop_reason"] = None
        self.rows[4]["parentUuid"] = "duration"
        self.rows.insert(4, {"type": "system", "subtype": "turn_duration", "uuid": "duration", "parentUuid": "armed"})
        self.assertEqual(self.evidence()["task_id"], "task-1")

    def test_s4_explicit_nonterminal_and_unlinked_duration_cannot_prove_end(self):
        self.rows[3]["message"]["stop_reason"] = "tool_use"
        self.rows[4]["parentUuid"] = "duration"
        duration = {"type": "system", "subtype": "turn_duration", "uuid": "duration", "parentUuid": "armed"}
        self.rows.insert(4, duration)
        with self.assertRaisesRegex(ValueError, "terminate"):
            self.evidence()
        self.rows[3]["message"]["stop_reason"] = None
        duration["parentUuid"] = "bash"
        with self.assertRaisesRegex(ValueError, "terminate"):
            self.evidence()

    def test_s4_missing_duplicate_or_foreground_bash_fails(self):
        import copy
        baseline = copy.deepcopy(self.rows)
        for kind in ["missing", "duplicate", "foreground"]:
            self.rows = copy.deepcopy(baseline)
            if kind == "missing":
                self.rows[1]["message"]["content"] = []
            elif kind == "duplicate":
                self.rows[1]["message"]["content"] *= 2
            else:
                self.rows[1]["message"]["content"][0]["input"]["run_in_background"] = False
            with self.subTest(kind=kind), self.assertRaisesRegex(ValueError, "background Bash"):
                self.evidence()

    def test_s4_notification_identity_order_spoof_and_parent_fail(self):
        import copy
        baseline = copy.deepcopy(self.rows)
        for kind in ["wrong_task", "wrong_tool", "not_completed", "missing_result_identity", "quoted_notice", "early_notice", "broken_parent", "non_terminal", "initiating_auto", "duplicate_auto", "manual_auto"]:
            self.rows = copy.deepcopy(baseline)
            if kind in ("wrong_task", "wrong_tool", "not_completed"):
                old, new = {"wrong_task": ("task-1", "other"), "wrong_tool": ("tool-1", "other"), "not_completed": ("completed", "running")}[kind]
                self.rows[4]["message"]["content"] = self.rows[4]["message"]["content"].replace(old, new)
            elif kind == "missing_result_identity":
                self.rows[2].pop("toolUseResult")
            elif kind == "quoted_notice":
                self.rows[4]["message"]["content"] = "Human quoting: " + self.rows[4]["message"]["content"]
            elif kind == "early_notice":
                self.rows[3], self.rows[4] = self.rows[4], self.rows[3]
            elif kind == "broken_parent":
                self.rows[5]["parentUuid"] = "armed"
            elif kind == "non_terminal":
                self.rows[3]["message"]["stop_reason"] = "tool_use"
            elif kind == "initiating_auto":
                self.rows[3]["message"]["content"][0]["text"] += self.AUTO
                self.rows[5]["message"]["content"] = []
            elif kind == "duplicate_auto":
                self.rows[5]["message"]["content"][0]["text"] *= 2
            elif kind == "manual_auto":
                self.rows.insert(5, {"type": "user", "message": {"content": "say AUTO now"}})
            with self.subTest(kind=kind), self.assertRaises(ValueError):
                self.evidence()

    def test_s4_cli_uses_native_evidence_without_discord_fetch(self):
        self.evidence()
        binding = {"provider": "claude", "channel_id": "123", "transcript_path": str(self.path)}
        with patch.object(sc, "resolve_binding", return_value=binding), patch.object(sc, "fetch_messages") as fetch, contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(sc.main(["--agent-id", "worker", "--run-id", "fixture", "--autonomous-background-turn"]), 0)
            fetch.assert_not_called()


class AutonomousDispatchTests(unittest.TestCase):
    write = SourceCompareTests.write
    user = SourceCompareTests.user
    ARMED, AUTO = AutonomousEvidenceTests.ARMED, AutonomousEvidenceTests.AUTO

    def setUp(self):
        AutonomousEvidenceTests.setUp(self)
        project = self.root / "project"
        project.mkdir()
        self.path = project / "native-session.jsonl"
        self.identity = {"agent_id": "adk-claude-tui-e2e", "channel_id": "123", "provider": "claude",
                         "session_key": "claude:123", "raw_provider_session_id": "native-session"}
        self.context = driver.ObservationContext("http://fixture", "123", "claude", self.root)
        self.spec = {"autonomous_background_turn": {"armed_marker": "[E2E:S4:{run_id}:ARMED]",
                                                    "auto_marker": "[E2E:S4:{run_id}:AUTO]"}}

    def dispatch(self):
        self.write(self.rows)
        record = {}
        with patch.object(driver.source_compare, "get_json", return_value=self.identity) as get:
            driver.run_assertion(self.spec, window=driver.assertions.Window("100"), record=record,
                                 run_id="fixture", observation_context=self.context)
        get.assert_called_once_with("http://fixture", "/api/agents/123/session-evidence")
        return record["autonomous_background_turn"]

    def test_actual_dispatch_records_linked_native_evidence(self):
        evidence = self.dispatch()
        self.assertEqual({k: evidence[k] for k in ("bash_tool_use_id", "task_id", "armed_uuid",
                                                  "notification_uuid", "auto_uuid")},
                         {"bash_tool_use_id": "tool-1", "task_id": "task-1", "armed_uuid": "armed",
                          "notification_uuid": "notice", "auto_uuid": "auto"})
        self.assertEqual(evidence["binding"]["transcript_path"], str(self.path))

    def test_native_evidence_survives_final_result_json(self):
        evidence = self.dispatch()
        result = {"assertions": []}
        driver._merge_record_into_result(result, {"autonomous_background_turn": evidence})
        emitted = json.loads(json.dumps(result))["autonomous_background_turn"]
        for key in ("bash_tool_use_id", "task_id", "armed_uuid", "notification_uuid", "auto_uuid"):
            self.assertEqual(emitted[key], evidence[key])
        self.assertEqual(emitted["binding"]["transcript_path"], str(self.path))

    def test_actual_dispatch_rejects_missing_notification(self):
        self.rows.pop(4)
        with self.assertRaisesRegex(driver.HarnessEvidenceError, "matching completion notice"):
            self.dispatch()

    def test_actual_dispatch_rejects_wrong_task_and_tool_identity(self):
        original = self.rows[4]["message"]["content"]
        for identifier in ("task-1", "tool-1"):
            self.rows[4]["message"]["content"] = original.replace(identifier, "wrong-id")
            with self.subTest(identifier=identifier), self.assertRaisesRegex(driver.HarnessEvidenceError, "matching completion notice"):
                self.dispatch()

    def test_actual_dispatch_rejects_same_turn_auto(self):
        self.rows[3]["message"]["content"][0]["text"] += " " + self.AUTO
        self.rows[5]["message"]["content"] = []
        with self.assertRaises(driver.HarnessEvidenceError):
            self.dispatch()

    def test_actual_dispatch_rejects_foreground_bash(self):
        self.rows[1]["message"]["content"][0]["input"]["run_in_background"] = False
        with self.assertRaisesRegex(driver.HarnessEvidenceError, "background Bash"):
            self.dispatch()

    def test_actual_dispatch_requires_context_and_exercised_provider(self):
        with self.assertRaisesRegex(driver.HarnessEvidenceError, "observation context"):
            driver.run_assertion(self.spec, window=driver.assertions.Window("100"), record={})
        self.identity["provider"] = "codex"
        with self.assertRaises(driver.HarnessEvidenceError):
            self.dispatch()

    def test_run_one_cell_passes_observation_context_to_actual_dispatch(self):
        from argparse import Namespace
        self.write(self.rows)
        client = Mock(base_url="http://fixture")
        client.send_control.return_value = {"id": "100"}
        client.fetch_messages.return_value = []
        scenario = {"id": "native-proof", "agent_mode": "none", "coverage_class": "fixture",
                    "steps": [], "assertions": [self.spec]}
        args = Namespace(queue_runtime_root=self.root, transcript_root=self.root, final_refetches=1)
        with patch.object(driver.source_compare, "get_json", return_value=self.identity), \
             patch.object(driver.time, "sleep"), patch.object(driver, "assert_cell_idle", return_value={}):
            record = driver.run_one_cell(scenario=scenario, cell="claude-tui", channel_id="123",
                                         client=client, run_id="fixture", dry_run=False, args=args)
        self.assertEqual(record["autonomous_background_turn"]["task_id"], "task-1")
        self.assertTrue(record["assertions"][0]["passed"])

    def test_all_scenario_cells_load_without_network(self):
        scenarios = Path(__file__).resolve().parents[3] / "tests/e2e/tui_relay/scenarios"
        with patch.object(driver.urllib.request, "urlopen", side_effect=AssertionError("unexpected network")):
            loaded = {cell: driver.load_scenarios(scenarios, cell=cell) for cell in driver.SUPPORTED_CELLS}
        self.assertTrue(all(loaded.values()))
        self.assertIn("E-53", {scenario["id"] for scenario in loaded["claude-tui"]})
        self.assertIn("E-36", {scenario["id"] for scenario in loaded["claude-tui"]})

    def test_health_and_autonomous_schema_still_reject_unknown_options(self):
        for kind, action, params in (("steps", "assert_health", {"require_status": ["healthy"]}),
                                     ("assertions", "autonomous_background_turn", self.spec["autonomous_background_turn"])):
            driver.validate_scenario_schema({kind: [{action: params}]})
            with self.subTest(action=action), self.assertRaisesRegex(ValueError, "unsupported"):
                driver.validate_scenario_schema({kind: [{action: {**params, "unexpected": True}}]})


class FinalSnapshotDriverTests(unittest.TestCase):
    MARKER = "[E2E:T:fixture:ONE]"

    def run_surface(self, *, change=None, pages=None, specs=None, initial=None):
        from argparse import Namespace
        state = initial if initial is not None else [message(101, self.MARKER + "\n\n-# ✅ 완료")]
        client = Mock(base_url="http://offline.invalid")
        client.send_control.return_value = {"id": "100"}
        requests = []

        def fetch(channel, *, after_id, limit):
            requests.append(after_id)
            if pages is not None:
                return pages(after_id)
            return [row.copy() for row in state if int(row["id"]) > int(after_id)]

        def idle(**kwargs):
            if change == "footer":
                state[0] = message(101, self.MARKER)
            elif change == "delete":
                state.clear()
            elif change == "panel":
                state[:] = [row for row in state if row["id"] != "102"]
            return {"status": "idle"}

        client.fetch_messages.side_effect = fetch
        scenario = {"id": "surface-proof", "agent_mode": "none", "coverage_class": "fixture",
                    "steps": [], "report_marker_counts": [self.MARKER],
                    "assertions": specs if specs is not None else [{"completion_per_turn": {"exact": 1, "marker": self.MARKER}}]}
        args = Namespace(queue_runtime_root="/offline-denied", final_refetches=1)
        with patch.object(driver.time, "sleep"), patch.object(driver, "assert_cell_idle", side_effect=idle):
            record = driver.run_one_cell(scenario=scenario, cell="claude-tui", channel_id="123",
                                        client=client, run_id="fixture", dry_run=False, args=args)
        return record, requests

    def test_idle_footer_loss_is_rejected_by_actual_dispatch(self):
        with self.assertRaisesRegex(driver.ScenarioStepAssertionError, "completion"):
            self.run_surface(change="footer")

    def test_idle_deleted_inline_message_is_rejected(self):
        with self.assertRaisesRegex(driver.ScenarioStepAssertionError, "completion"):
            self.run_surface(change="delete")

    def test_unchanged_idle_surface_passes_without_duplicate_results(self):
        record, requests = self.run_surface()
        self.assertEqual(requests, ["100", "100"])
        self.assertEqual(len(record["assertions"]), 2)
        self.assertTrue(record["revalidated_after_idle"][0]["passed"])
        self.assertEqual(record["marker_counts"][self.MARKER], 1)

    def test_full_page_is_followed_until_snapshot_is_complete(self):
        rows = [message(mid, "plain body") for mid in range(101, 201)]
        rows += [message(201, self.MARKER + "\n\n-# ✅ 완료")]
        record, requests = self.run_surface(pages=lambda cursor: [row for row in rows if int(row["id"]) > int(cursor)][:100])
        self.assertEqual(requests, ["100", "200", "100", "200"])
        self.assertEqual(record["raw_count"], 101)

    def test_nonadvancing_full_page_fails_closed(self):
        rows = [message(mid, "plain body") for mid in range(101, 201)]
        with self.assertRaisesRegex(driver.HarnessEvidenceError, "pagination did not advance"):
            self.run_surface(pages=lambda cursor: rows)

    def test_nonadvancing_short_page_fails_closed_by_actual_dispatch(self):
        first = [message(101, self.MARKER + "\n\n-# ✅ 완료")]
        first += [message(mid, "plain body") for mid in range(102, 201)]
        with self.assertRaisesRegex(driver.HarnessEvidenceError, "pagination did not advance"):
            self.run_surface(pages=lambda cursor: first if cursor == "100" else first[:1])

    def test_advancing_short_page_passes_actual_dispatch(self):
        first = [message(101, self.MARKER + "\n\n-# ✅ 완료")]
        first += [message(mid, "plain body") for mid in range(102, 201)]
        last = [message(mid, "plain body") for mid in range(201, 204)]
        record, requests = self.run_surface(pages=lambda cursor: first if cursor == "100" else last)
        self.assertEqual(requests, ["100", "200", "100", "200"])
        self.assertTrue(record["revalidated_after_idle"][0]["passed"])
        self.assertEqual(record["raw_count"], 103)

    def test_cursor_violations_in_short_and_full_pages_fail_closed(self):
        for size in (2, 100):
            for bad_id in ("100", "99"):
                rows = [message(101, self.MARKER + "\n\n-# ✅ 완료")]
                rows += [message(mid, "plain body") for mid in range(102, 100 + size)]
                rows.append(message(bad_id, "plain body"))
                with self.subTest(size=size, bad_id=bad_id), self.assertRaisesRegex(
                    driver.HarnessEvidenceError, "pagination did not advance"
                ):
                    self.run_surface(pages=lambda cursor: rows)

    def test_nonnumeric_ids_in_short_and_full_pages_fail_closed(self):
        for size in (2, 100):
            for bad_id in ("not-a-snowflake", "²"):
                rows = [message(101, self.MARKER + "\n\n-# ✅ 완료")]
                rows += [message(mid, "plain body") for mid in range(102, 100 + size)]
                rows.append(message(bad_id, "plain body"))
                with self.subTest(size=size, bad_id=bad_id), self.assertRaises(driver.HarnessEvidenceError):
                    self.run_surface(pages=lambda cursor: rows)


    def test_deleted_status_panel_cannot_pass_historical_raw_assertions(self):
        specs = [{"status_panel_after_body": {"body_marker": self.MARKER}},
                 {"single_status_panel": {}},
                 {"completion_chrome_after_body": {"body_marker": self.MARKER, "required": True}}]
        for spec in specs:
            with self.subTest(spec=spec), self.assertRaises(driver.ScenarioStepAssertionError) as caught:
                self.run_surface(change="panel", specs=[spec],
                                 initial=[message(101, self.MARKER), message(102, "✅ 완료")])
            record = caught.exception.record
            self.assertEqual(record["raw_count"], 1)
            self.assertEqual(record["recent_raw"][0]["id"], "101")

    def test_deleted_body_updates_final_counts_and_report(self):
        with self.assertRaises(driver.ScenarioStepAssertionError) as caught:
            self.run_surface(change="delete", specs=[{"raw_text_present": self.MARKER}])
        record = caught.exception.record
        self.assertEqual(record["marker_counts"][self.MARKER], 0)
        self.assertEqual((record["raw_count"], record["relay_count"]), (0, 0))
        self.assertEqual(record["recent_raw"], [])
        result = {"assertions": []}
        driver._merge_record_into_result(result, record)
        self.assertFalse(result["revalidated_after_idle"][0]["passed"])

    def test_actual_captured_full_page_is_not_mistaken_for_complete(self):
        from argparse import Namespace
        rows = [message(mid, "plain body") for mid in range(100, 200)]
        response = Mock(status=200, headers={}, read=Mock(return_value=json.dumps(rows).encode()))
        response.__enter__ = Mock(return_value=response)
        response.__exit__ = Mock(return_value=False)
        client = driver.discord.DiscordClient("http://offline.invalid", captures=[], capture_after_id="99")
        scenario = {"id": "captured-full", "agent_mode": "none", "coverage_class": "fixture",
                    "steps": [], "assertions": []}
        with patch.object(driver.urllib.request, "urlopen", return_value=response) as get, \
             patch.object(driver.discord.DiscordClient, "send_control", return_value={"id": "100"}), \
             patch.object(driver.time, "sleep"), \
             self.assertRaisesRegex(driver.HarnessEvidenceError, "captured snapshot incomplete"):
            driver.run_one_cell(scenario=scenario, cell="claude-tui", channel_id="123", client=client,
                                run_id="fixture", dry_run=False,
                                args=Namespace(queue_runtime_root="/offline-denied", final_refetches=1))
        self.assertEqual(get.call_count, 1)
        self.assertEqual(len(client.captures[0]["pages"][0]["messages"]), 100)


if __name__ == "__main__":
    unittest.main()
