"""Offline final-snapshot and per-turn completion contracts for bulk activation."""

import datetime as dt
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from tui_relay import assertions as a


def message(mid, content, *, author="relay", bot=True, second=None):
    row = {"id": str(mid), "content": content, "author": {"id": author, "bot": bot}, "type": 0}
    if second is not None:
        row["timestamp"] = f"2026-10-07T00:00:{second:02d}Z"
    return row


def window(*rows):
    result = a.Window("100")
    for row in rows:
        result.add(row)
    return result


def timed(*rows):
    result = window(*rows)
    for second in (0, 10):
        result.mark_prompt_sent(dt.datetime(2026, 10, 7, 0, 0, second, tzinfo=dt.timezone.utc))
    return result


class PlaceholderContracts(unittest.TestCase):
    def test_final_body_passes_with_deleted_edited_and_nonrelay_placeholders(self):
        value = window(message(101, "..."), message(102, "…"),
                       message(103, "...", author=a.OUR_BOT_ID),
                       message(104, "...", bot=False), message(105, "There is ... more."))
        value.deleted_ids.add("101")
        value.add(message(102, "real answer"))
        a.no_placeholder_left(value)

    def test_single_surviving_placeholder_fails(self):
        for content in ("...", " \n… \n", "🆕 새 세션 시작\n\n...",
                        "…\n\n-# ✅ 완료"):
            with self.subTest(content=content), self.assertRaisesRegex(a.AssertionError, "placeholders remain"):
                a.no_placeholder_left(window(message(101, content)))

    def test_final_placeholder_edit_is_not_hidden_by_prior_body(self):
        value = window(message(101, "answer"))
        value.add(message(101, "..."))
        with self.assertRaises(a.AssertionError):
            a.no_placeholder_left(value)


class CompletionContracts(unittest.TestCase):
    def test_one_completion_per_marked_turn(self):
        value = window(message(101, "[E2E:T:ONE]"), message(102, "-# ✅ 완료"),
                       message(103, "[E2E:T:TWO]"), message(104, "📦 응답 완료 · 1s"))
        a.completion_per_turn(value)
        a.completion_per_turn(value, marker="[E2E:T:TWO]")
        a.completion_per_turn(value, marker="TWO")

    def test_duplicate_first_and_missing_second_do_not_cancel(self):
        value = window(message(101, "[E2E:T:ONE]"), message(102, "✅ 완료"),
                       message(103, "✅ 완료"), message(104, "[E2E:T:TWO]"))
        with self.assertRaisesRegex(a.AssertionError, "expected 1, got 2"):
            a.completion_per_turn(value)
        with self.assertRaisesRegex(a.AssertionError, "expected 1, got 0"):
            a.completion_per_turn(value, marker="[E2E:T:TWO]")

    def test_prompt_windows_support_multiple_body_markers_and_unmarked_bodies(self):
        value = timed(message(101, "[E2E:T:PRE] [E2E:T:POST]", second=1),
                      message(102, "✅ 완료", second=2), message(103, "plain answer", second=11),
                      message(104, "✅ 완료", second=12))
        a.completion_per_turn(value)
        a.completion_per_turn(value, marker="[E2E:T:POST]")

    def test_prompt_window_without_body_or_completion_fails(self):
        value = timed(message(101, "answer", second=1), message(102, "✅ 완료", second=2))
        with self.assertRaisesRegex(a.AssertionError, "expected 1, got 0"):
            a.completion_per_turn(value)

    def test_completion_before_body_fails(self):
        value = window(message(101, "✅ 완료"), message(102, "[E2E:T:ONE]"))
        for marker in (None, "[E2E:T:ONE]"):
            with self.subTest(marker=marker), self.assertRaises(a.CompletionOrderError):
                a.completion_per_turn(value, marker=marker)

    def test_deleted_card_and_nonbot_or_driver_cards_are_excluded(self):
        value = window(message(101, "[E2E:T:ONE]"), message(102, "✅ 완료"),
                       message(103, "✅ 완료"), message(104, "✅ 완료", bot=False),
                       message(105, "✅ 완료", author=a.OUR_BOT_ID))
        value.deleted_ids.add("102")
        a.completion_per_turn(value, marker="[E2E:T:ONE]")
        value.deleted_ids.add("103")
        with self.assertRaisesRegex(a.AssertionError, "expected 1, got 0"):
            a.completion_per_turn(value)

    def test_exact_zero_and_two(self):
        value = window(message(101, "[E2E:T:ONE]"))
        a.completion_per_turn(value, exact=0, marker="[E2E:T:ONE]")
        for mid in (102, 103):
            value.add(message(mid, "✅ 완료"))
        a.completion_per_turn(value, exact=2, marker="[E2E:T:ONE]")
        with self.assertRaisesRegex(a.AssertionError, "expected 0, got 2"):
            a.completion_per_turn(value, exact=0)

    def test_missing_repeated_or_deleted_marker_is_not_attributable(self):
        for content in ("answer", "[E2E:T:ONE] [E2E:T:ONE]"):
            with self.subTest(content=content), self.assertRaisesRegex(a.AssertionError, "must occur once"):
                a.completion_per_turn(timed(message(101, content, second=1)), marker="[E2E:T:ONE]")
        value = window(message(101, "[E2E:T:ONE]"), message(102, "✅ 완료"))
        value.deleted_ids.add("101")
        with self.assertRaises(a.AssertionError):
            a.completion_per_turn(value, marker="[E2E:T:ONE]")

    def test_card_edits_are_one_id_and_marker_in_card_does_not_count_as_body(self):
        value = window(message(101, "[E2E:T:ONE]"), message(102, "✅ 완료"))
        value.add(message(102, "✅ 완료 updated"))
        a.completion_per_turn(value)
        with self.assertRaises(a.AssertionError):
            a.completion_per_turn(window(message(102, "✅ 완료 [E2E:T:ONE]")), marker="[E2E:T:ONE]")

    def test_ambiguous_fallback_and_missing_timestamp_fail_closed(self):
        for value in (window(message(101, "[E2E:T:PRE] [E2E:T:POST]")),
                      timed(message(101, "[E2E:T:ONE]")), window()):
            with self.subTest(value=value), self.assertRaises(a.AssertionError):
                a.completion_per_turn(value)

    def test_single_message_terminal_footer_is_one_completion(self):
        for footer in ("-# ✅ 완료", "-# ⠸ 완료", "-# ✅ 백그라운드 완료"):
            value = window(message(101, "[E2E:T:ONE]\n\n" + footer))
            with self.subTest(footer=footer):
                a.completion_per_turn(value)
                a.completion_per_turn(value, marker="[E2E:T:ONE]")
                value.add(message(102, "✅ 완료"))
                with self.assertRaisesRegex(a.AssertionError, "expected 1, got 2"):
                    a.completion_per_turn(value)

    def test_footer_like_prose_and_active_footer_are_not_completions(self):
        for suffix in ("\n\n-# ✅ 완료\nordinary prose", "\n\n-# ⠸ 진행 중", "\n-# ✅ 완료"):
            with self.subTest(suffix=suffix), self.assertRaises(a.AssertionError):
                a.completion_per_turn(window(message(101, "[E2E:T:ONE]" + suffix)))

    def test_inline_completion_includes_earlier_card_in_order_check(self):
        value = window(message(101, "✅ 완료"), message(102, "[E2E:T:ONE]\n\n-# ✅ 완료"))
        with self.assertRaises(a.CompletionOrderError):
            a.completion_per_turn(value, exact=2)

    def test_invalid_count_and_marker_are_rejected(self):
        for exact in (-1, 1.5, True, "1"):
            with self.subTest(exact=exact), self.assertRaises(a.AssertionError):
                a.completion_per_turn(window(), exact=exact)
        for marker in ("", [], 7):
            with self.subTest(marker=marker), self.assertRaises(a.AssertionError):
                a.completion_per_turn(window(), marker=marker)


if __name__ == "__main__":
    unittest.main()
