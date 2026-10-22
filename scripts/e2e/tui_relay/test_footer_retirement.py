"""Offline footer-retirement oracles and executable scenario contracts."""

import pathlib
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts/e2e"))

import run_tui_relay as driver
from tui_relay import assertions


def message(content, mid="10"):
    return {"id": mid, "content": content, "author": {"id": "provider", "bot": True}}


def window(*rows):
    value = assertions.Window("1")
    for row in rows:
        value.add(row)
    return value


class FooterRetirementTests(unittest.TestCase):
    def test_raw_panels_and_embedded_footer_are_rejected(self):
        for content in ("...", "…", "Processing...", "🟢 진행 중", "✅ 응답 완료", "📦 응답 완료 · 1분",
                        "answer\n\n-# ✅ 완료", "-# ⠸ 진행 중", "-# Tasks\n-# └ Bash ✓",
                        "-# 턴 시작 : anonymized", "-# ⏱ 5초"):
            with self.subTest(content=content), self.assertRaisesRegex(assertions.AssertionError, "retired status chrome"):
                assertions.no_status_chrome(window(message(content)))

    def test_both_edit_snapshots_are_checked_even_when_final_message_is_clean(self):
        for field in ("before", "after"):
            for content in ("answer\n\n-# ✅ 완료\n-# Tasks", "...", "…"):
                with self.subTest(field=field, content=content):
                    value = window(message("clean answer"))
                    update = {"id": "10", "before": "clean answer", "after": "clean answer"}
                    update[field] = content
                    value.message_updates.append(update)
                    with self.assertRaisesRegex(assertions.AssertionError, field + " snapshot"):
                        assertions.no_status_chrome(value)

    def test_deleted_and_orphan_update_chrome_cannot_disappear_from_evidence(self):
        value = window(message("Processing..."))
        value.deleted_ids.add("10")
        value.messages.clear()
        with self.assertRaises(assertions.AssertionError):
            assertions.no_status_chrome(value)
        value = window()
        value.message_updates.append({"id": "unknown", "before": "-# Tasks", "after": "answer"})
        with self.assertRaises(assertions.AssertionError):
            assertions.no_status_chrome(value)

    def test_plain_subtext_prose_cards_and_standalone_summaries_are_allowed(self):
        for content in ("⠋ 계속 처리 중", "-# foo", "The work is 진행 중; Processing... is quoted prose.",
                        "-# ▸ Bash  echo Processing... 진행 중  ✓", "-# ✅ 1분", "-# ✅ 완료",
                        "-# ⏹ 중단", "-# ⚠️ 오류 · 5초"):
            with self.subTest(content=content):
                assertions.no_status_chrome(window(message(content)))

    def test_driver_prompts_are_excluded_without_hiding_other_raw_messages(self):
        row = message("-# Tasks")
        row["author"]["id"] = assertions.OUR_BOT_ID
        value = window(row)
        value.message_updates.append({"id": "10", "before": "Processing...", "after": "-# Tasks"})
        assertions.no_status_chrome(value)

    def test_standalone_summary_with_exact_outcome_header_follows_body(self):
        for header, content in (("-# ✅", "-# ✅ 1분"), ("-# ⏹ 중단", "-# ⏹ 중단"),
                                ("-# ⚠️ 오류", "-# ⚠️ 오류 · 5초")):
            with self.subTest(content=content):
                assertions.turn_summary_after_body(window(message("[BODY]"), message(content, "11")),
                                                   body_marker="[BODY]", header=header)

    def test_mixed_footer_prefix_and_wrong_outcome_are_not_terminal_summaries(self):
        for content in ("answer\n\n-# ✅ 완료", "-# ✅ 완료\n-# Tasks", "✅ 응답 완료", "-# ⚠️ 오류"):
            with self.subTest(content=content), self.assertRaises(assertions.AssertionError):
                assertions.turn_summary_after_body(window(message("[BODY]"), message(content, "11")),
                                                   body_marker="[BODY]")

    def test_summary_before_body_and_duplicate_summary_ids_fail(self):
        for value in (window(message("-# ✅ 1분", "9"), message("[BODY]")),
                      window(message("[BODY]"), message("-# ✅ 1분", "11"), message("-# ✅ 1분", "12"))):
            with self.subTest(value=value), self.assertRaises(assertions.AssertionError):
                assertions.turn_summary_after_body(value, body_marker="[BODY]")

    def test_card_marker_cannot_stand_in_for_the_body(self):
        value = window(message("-# ▸ Bash  echo [BODY]  ✓"), message("-# ✅ 1분", "11"))
        with self.assertRaises(assertions.AssertionError):
            assertions.turn_summary_after_body(value, body_marker="[BODY]")

    def test_inline_order_requires_success_between_distinct_before_and_after_bodies(self):
        rows = [message("[BEFORE]", "10"), message("-# ▸ Bash  cargo check  ✓", "11"),
                message("[AFTER]", "12"), message("-# ✅ 1분", "13")]
        options = {"before_marker": "[BEFORE]", "body_marker": "[AFTER]", "tool_label": "Bash"}
        assertions.turn_summary_after_body(window(*rows), **options)
        for order in ((1, 0, 2, 3), (0, 2, 1, 3)):
            changed = [{**rows[index], "id": str(10 + position)} for position, index in enumerate(order)]
            with self.subTest(order=order), self.assertRaises(assertions.AssertionError):
                assertions.turn_summary_after_body(window(*changed), **options)
        for status in ("·", "✗", "?"):
            with self.subTest(status=status), self.assertRaises(assertions.AssertionError):
                assertions.turn_summary_after_body(window(rows[0], message("-# ▸ Bash  cargo check  " + status, "11"),
                                                         *rows[2:]), **options)

    def test_output_owner_requires_an_evaluated_receipt_and_exact_mode(self):
        value = window(message("[BODY]"), message("-# ✅ 1분", "11"))
        for owner in ("o", "legacy"):
            spec = {"turn_summary_after_body": {"body_marker": "[BODY]", "output_owner": owner}}
            driver.run_assertion(spec, window=value, record={"durable_record_probe": {
                "status": "evaluated", "output_owner": owner}})
            for probe in ({}, {"status": "unevaluable", "output_owner": owner},
                          {"status": "evaluated", "output_owner": "legacy" if owner == "o" else "o"}):
                with self.subTest(owner=owner, probe=probe), self.assertRaises(driver.HarnessEvidenceError):
                    driver.run_assertion(spec, window=value, record={"durable_record_probe": probe})

    def test_runner_registers_strict_new_assertion_keys(self):
        driver.validate_scenario_schema({"steps": [], "assertions": [
            {"no_status_chrome": True}, {"turn_summary_after_body": {"body_marker": "[BODY]"}}]})
        with self.assertRaises(ValueError):
            driver.validate_scenario_schema({"steps": [], "assertions": [{"no_status_chrome": False}]})

    def test_panel_only_scenarios_are_retired_and_migrations_keep_body_oracles(self):
        directory = ROOT / "tests/e2e/tui_relay/scenarios"
        for number in (32, 33, 34):
            self.assertEqual(list(directory.glob(f"E-{number}-*.yaml")), [])
        for number in (1, 9, 15, 22, 23, 35, 52):
            scenario = driver.yaml.safe_load(next(directory.glob(f"E-{number}-*.yaml")).read_text())
            driver.validate_scenario_schema(scenario)
            self.assertIn({"no_status_chrome": True}, scenario["assertions"])
            self.assertTrue(any("completion_per_turn" in spec for spec in scenario["assertions"]))
            self.assertFalse(any("requires_feature" in spec or "status_panel_after_body" in spec
                                 or "single_status_panel" in spec for spec in scenario["assertions"]))
            self.assertTrue(any("wait_for_discord_text" in step for step in scenario["steps"]))

    def test_new_live_scenarios_require_actual_owner_and_existing_primitives(self):
        directory = ROOT / "tests/e2e/tui_relay/scenarios"
        for number, owner in ((60, "o"), (61, "o"), (62, "legacy")):
            scenario = driver.yaml.safe_load(next(directory.glob(f"E-{number}-*.yaml")).read_text())
            driver.validate_scenario_schema(scenario)
            self.assertTrue(scenario["durable_delivery_probe"])
            self.assertEqual(scenario["cells"], ["claude-pipe", "claude-tui"])
            summary = next(spec["turn_summary_after_body"] for spec in scenario["assertions"]
                           if "turn_summary_after_body" in spec)
            self.assertEqual(summary["output_owner"], owner)
            self.assertIn({"no_status_chrome": True}, scenario["assertions"])
            if number == 61:
                self.assertTrue(scenario["destructive"])
                self.assertTrue(any("restart_dcserver" in step for step in scenario["steps"]))


if __name__ == "__main__":
    unittest.main()
