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
        responses = [{"bindings": [{"agentId": "worker", "channelId": "123", "provider": "claude"}]},
                     {"session_key": "claude/token/host:worker", "provider": "claude"},
                     {"raw_provider_session_id": "native-id"}]
        with patch.object(sc, "get_json", side_effect=responses) as get:
            result = sc.resolve_binding("http://local", channel_id="123", transcript_root=self.root)
        self.assertEqual(result["transcript_path"], str(project / "native-id.jsonl"))
        self.assertEqual(get.call_args_list[1].args[1], "/api/agents/worker/turn")
        self.assertIn("session_key=claude%2Ftoken%2Fhost%3Aworker", get.call_args.args[1])

    def test_ambiguous_binding_fails(self):
        with patch.object(sc, "get_json", return_value={"bindings": []}), self.assertRaises(ValueError):
            sc.resolve_binding("http://local", agent_id="missing")

    def test_pagination_uses_existing_read_client(self):
        first = [message(i) for i in range(1, 101)]
        with patch.object(sc.DiscordClient, "fetch_messages", side_effect=[first, [message(101)]]) as fetch:
            self.assertEqual(len(sc.fetch_messages("http://local", "123")), 101)
        self.assertEqual(fetch.call_args.kwargs["after_id"], "100")
        with patch.object(sc.DiscordClient, "fetch_messages", return_value=first), self.assertRaises(ValueError):
            sc.fetch_messages("http://local", "123", max_pages=1)

    def test_cli_nonzero_failure_and_read_only_get(self):
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
        with patch.object(sc.urllib.request, "urlopen") as urlopen:
            urlopen.return_value.__enter__.return_value = io.StringIO('{}')
            self.assertEqual(sc.get_json("http://local", "/api/docs"), {})
        self.assertEqual(urlopen.call_args.args[0].method, "GET")


if __name__ == "__main__":
    unittest.main()
