"""Fixture-only coverage for the read-only source comparator."""

import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from . import source_compare as sc
from .assertions import OUR_BOT_ID


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


if __name__ == "__main__":
    unittest.main()
