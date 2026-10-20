"""Tool cards remain visible to leak checks without satisfying answer markers."""

import importlib
import sys
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts" / "e2e"))

import run_tui_relay as driver  # noqa: E402
from tui_relay import assertions  # noqa: E402


def message(content, message_id="1", author_id="999"):
    return {"id": message_id, "content": content, "type": 0,
            "author": {"id": author_id, "bot": True}}


class ToolCardRelayTests(unittest.TestCase):
    MARKER = "[E2E:TOOL-CARD:ANSWER]"

    def test_card_marker_cannot_satisfy_body_assertions(self):
        card = message(f"-# ▸ Bash  echo {self.MARKER}  ✓")
        window = assertions.Window("0")
        window.add(card)
        self.assertIsNone(assertions.relay_body(card))
        self.assertEqual(assertions.relay_marker_hits(window, marker=self.MARKER), 0)
        with self.assertRaises(assertions.AssertionError):
            assertions.text_present(window, needle=self.MARKER)
        window.add(message(self.MARKER, "2"))
        assertions.text_present(window, needle=self.MARKER)
        self.assertEqual(assertions.relay_marker_hits(window, marker=self.MARKER), 1)

    def test_classifier_accepts_only_nonempty_all_card_messages(self):
        cards = importlib.import_module("tui_relay.tool_cards")
        for content in ["-# ▸ Bash  cargo test  ✓", "-# ▸ 도구 14개",
                        "\n-# ▸ Read  a.rs  ?\n\n-# ▸ Edit  b.rs  ✗\n"]:
            with self.subTest(content=content):
                self.assertTrue(cards.is_tool_card(content))
                self.assertEqual(cards.tool_card_body(message(content)), content)
        for content in ["", " \n\t", "ordinary answer", "-# ordinary subtext",
                        "-# ▸ Bash  test  ✓\nactual answer", "```\n-# ▸ Read  a.rs  ✓\n```"]:
            with self.subTest(content=content):
                self.assertFalse(cards.is_tool_card(content))
                self.assertIsNone(cards.tool_card_body(message(content)))

    def test_banner_card_is_bodyless_but_mixed_prose_is_preserved(self):
        card = f"-# ▸ Bash  echo {self.MARKER}  ✓"
        self.assertIsNone(assertions.relay_body(message(f"기존 세션 복원\n\n{card}")))
        mixed = f"An example:\n{card}\nThe answer follows."
        self.assertEqual(assertions.relay_body(message(mixed)), mixed)

    def test_card_leaks_are_checked_even_when_status_words_hide_the_relay_body(self):
        for chrome in ("No response requested.", "Continue from where you left off."):
            window = assertions.Window("0")
            window.add(message(f"-# ▸ Bash  echo {chrome} 진행 중  ✓"))
            with self.assertRaisesRegex(assertions.AssertionError, "auto-resume prompt chrome"):
                assertions.no_resume_prompt_chrome(window)

    def test_driver_and_human_card_shaped_prompts_do_not_trigger_leak_checks(self):
        window = assertions.Window("0")
        window.add(message("-# ▸ Bash  No response requested.  ✓", author_id=assertions.OUR_BOT_ID))
        human = message("-# ▸ Bash  Continue from where you left off.  ✓", "2")
        human["author"]["bot"] = False
        window.add(human)
        assertions.no_resume_prompt_chrome(window)

    def test_runner_wait_ignores_card_markers_until_the_actual_body(self):
        for author in (None, "999"):
            with self.subTest(author=author):
                card = message(f"-# ▸ Bash  echo {self.MARKER}  ✓")
                body = message(self.MARKER, "2")
                client = Mock(base_url="http://unused.invalid")
                client.fetch_messages.side_effect = [[card], [card, body]]
                clock = [0.0]
                def sleep(seconds):
                    clock[0] += seconds
                with patch.object(driver.time, "monotonic", side_effect=lambda: clock[0]), \
                     patch.object(driver.time, "sleep", side_effect=sleep), \
                     patch.object(driver, "_raise_if_tui_prompt_stuck_while_idle"):
                    found, observed = driver.wait_for_discord_text_with_tui_idle_draft_guard(
                        client=client, channel_id="99", cell="claude-tui", after_id="0",
                        needle=self.MARKER, prompt=None, thread_channel_id=None,
                        timeout_s=15.0, debug_label="tool card", author_id=author)
                self.assertEqual(found, body)
                self.assertEqual(observed, [card, body])


if __name__ == "__main__":
    unittest.main()
