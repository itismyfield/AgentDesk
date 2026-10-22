"""Terminal summaries are completion evidence, never answer or durable Body evidence."""

import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts" / "e2e"))

from tui_relay import assertions, durable_delivery, turn_summary  # noqa: E402


def message(content, message_id="10"):
    return {"id": str(message_id), "content": content, "type": 0,
            "author": {"id": "summary-relay-fixture", "bot": True}}


def window(*rows):
    result = assertions.Window("0")
    for row in rows:
        result.add(row)
    return result


class TurnSummaryTests(unittest.TestCase):
    MARKER = "[E2E:SUMMARY:ONE]"
    SUMMARIES = (
        "-# ✅ 7분 14초 · 📦 45%",
        "-# ⏹ 중단 · 2분 3초 · 📦 41%",
        "-# ⚠️ 오류 · 12초",
    )

    def test_canonical_outcome_duration_and_context_matrix_is_recognized(self):
        durations = (None, "0초", "59초", "1분", "1분 1초", "59분 59초",
                     "1시간 0분", "5124095576030시간 25분")
        for outcome in ("✅", "⏹ 중단", "⚠️ 오류"):
            for duration in durations:
                for percent in (None, 0, 45, 100):
                    content = f"-# {outcome}"
                    if outcome == "✅" and duration is None and percent is None:
                        content += " 완료"
                    if duration is not None:
                        content += (" " if outcome == "✅" else " · ") + duration
                    if percent is not None:
                        content += f" · 📦 {percent}%"
                    with self.subTest(content=content):
                        self.assertLessEqual(len(content.encode("utf-16-le")) // 2, 80)
                        self.assertTrue(turn_summary.is_turn_summary(content))
                        self.assertEqual(turn_summary.turn_summary_body(message(content)), content)

    def test_recognizer_rejects_mixed_prose_newlines_bad_percent_and_overbudget(self):
        oversized = "-# ✅ " + "9" * 81 + "초"
        self.assertIsNotNone(turn_summary.TURN_SUMMARY_PATTERN.fullmatch(oversized),
                             "the budget fixture must otherwise have valid syntax")
        rejected = (
            "", "-# ✅", "-# ✅ · 7분 14초 · 📦 45%", "-# ⏹ 중단 2분 3초",
            "-# ✅ 완료 · 📦 45%", "-# ✅ 1초 · 📦 101%", "-# ✅ 1초 · 📦 255%",
            "-# ✅ 1초 · 📦 -1%", "-# ✅ 1초 · 📦 45.5%", "-# ✅ 1초 · 📦 01%",
            "-# ✅ 1주", "-# ✅ 1초 · 도구 실행 중", "-# ✅ 완료\n",
            "\n-# ✅ 완료", "answer\n-# ✅ 1초", "-# ✅ 1초\nanswer",
            "```\n-# ✅ 1초\n```", "-# ✅ 1초\t", oversized,
        )
        for content in rejected:
            with self.subTest(content=content):
                self.assertFalse(turn_summary.is_turn_summary(content))
                self.assertIsNone(turn_summary.turn_summary_body(message(content)))
        for content in (None, False, [], {}):
            with self.subTest(content=content):
                self.assertFalse(turn_summary.is_turn_summary(content))
                self.assertIsNone(turn_summary.turn_summary_body({"content": content}))

    def test_summary_words_cannot_satisfy_answer_markers_until_a_real_body_arrives(self):
        value = window(message("-# ✅ 완료", "10"), message(self.SUMMARIES[0], "11"))
        for row in value.raw_messages:
            self.assertIsNone(assertions.relay_body(row))
        for marker in ("완료", "7분", "45%"):
            with self.subTest(marker=marker):
                self.assertEqual(assertions.relay_marker_hits(value, marker=marker), 0)
                with self.assertRaises(assertions.AssertionError):
                    assertions.text_present(value, needle=marker)
        value.add(message("답변 본문: 완료, 7분, 45%", "12"))
        for marker in ("완료", "7분", "45%"):
            assertions.text_present(value, needle=marker)
            self.assertEqual(assertions.relay_marker_hits(value, marker=marker), 1)

    def test_body_then_one_summary_satisfies_completion_for_every_outcome(self):
        for content in self.SUMMARIES:
            with self.subTest(content=content):
                value = window(message(self.MARKER), message(content, "11"))
                assertions.completion_per_turn(value, exact=1, marker=self.MARKER)
                assertions.completion_per_turn(value, exact=1)

    def test_two_summary_ids_fail_exactly_one_completion_even_with_identical_text(self):
        for content in self.SUMMARIES:
            with self.subTest(content=content):
                value = window(message(self.MARKER), message(content, "11"), message(content, "12"))
                with self.assertRaisesRegex(assertions.AssertionError, "expected 1, got 2 final"):
                    assertions.completion_per_turn(value, exact=1, marker=self.MARKER)

    def test_summary_before_body_fails_the_completion_order_contract(self):
        for content in self.SUMMARIES:
            with self.subTest(content=content):
                value = window(message(content, "9"), message(self.MARKER, "10"))
                with self.assertRaises(assertions.CompletionOrderError):
                    assertions.completion_per_turn(value, exact=1, marker=self.MARKER)

    def test_identical_summary_text_is_valid_for_two_distinct_turns_and_ids(self):
        summary = self.SUMMARIES[0]
        value = window(message("[E2E:SUMMARY:FIRST]", "10"), message(summary, "11"),
                       message("[E2E:SUMMARY:SECOND]", "20"), message(summary, "21"))
        assertions.completion_per_turn(value, exact=1)
        assertions.no_duplicate_content(value)
        self.assertEqual([row["id"] for row in value.raw_messages if turn_summary.turn_summary_body(row)],
                         ["11", "21"])

    def test_summary_in_either_edit_snapshot_counts_as_observed_duplicate_completion(self):
        for field in ("before", "after"):
            for content in self.SUMMARIES:
                with self.subTest(field=field, content=content):
                    value = window(message(self.MARKER), message("retired status", "11"),
                                   message(content, "12"))
                    update = {"id": "11", "before": "retired status", "after": "retired status"}
                    update[field] = content
                    value.message_updates.append(update)
                    observed = {row["id"]: row for row in value.raw_messages}
                    self.assertEqual(assertions.edited_completion_candidates(value, observed), {"11"})
                    with self.assertRaisesRegex(assertions.AssertionError, "expected 1, got 2 observed"):
                        assertions.completion_per_turn(value, exact=1, marker=self.MARKER)

    def test_a_summary_does_not_hide_a_surviving_ellipsis_placeholder(self):
        for placeholder in ("...", "…", " \u200b…\ufeff "):
            with self.subTest(placeholder=placeholder):
                value = window(message(placeholder), message(self.SUMMARIES[0], "11"))
                with self.assertRaisesRegex(assertions.AssertionError, "placeholders remain"):
                    assertions.no_placeholder_left(value)
                value.add(message("real answer", "10"))
                assertions.no_placeholder_left(value)

    def test_default_durable_body_probe_rejects_a_posted_tool_kind_summary(self):
        with tempfile.TemporaryDirectory() as temporary:
            runtime = Path(temporary) / "runtime"
            channel = Path(temporary) / "o_store" / "99"
            channel.mkdir(parents=True)
            (channel / "init").write_text("{}", encoding="utf-8")
            prepared = {"type": "prepared", "serial": 0, "piece_index": 0,
                        "payload": self.SUMMARIES[0], "anchor_id": 1, "epoch": 1,
                        "unit_key": {"channel_id": 99, "provider": "claude",
                                     "native_key": "answer:0#turn_end", "kind": "tool"}}
            posted = {"type": "posted", "serial": 0, "msg_id": 222}
            (channel / "ledger.jsonl").write_text(
                "".join(json.dumps({"entry": row}) + "\n" for row in (prepared, posted)),
                encoding="utf-8")
            query = dict(provider="claude", channel_id="99", message_id="222")
            body_probe = durable_delivery.scan_records(runtime, **query)
            self.assertEqual(body_probe["status"], "failed", body_probe)
            self.assertEqual(body_probe["unit_kind"], "tool")
            self.assertEqual(body_probe["expected_kind"], "body")
            tool_probe = durable_delivery.scan_records(runtime, expected_kind="tool", **query)
            self.assertEqual(tool_probe["status"], "evaluated", tool_probe)


if __name__ == "__main__":
    unittest.main()
