"""Pure continuous summary evaluation: fixture reports never imply live coverage."""
import copy
import json
from pathlib import Path
import unittest

from scripts.e2e.continuous import summary

SHA = "a" * 40
IDENTITY = {"instance_id": "instance", "boot_id": "boot", "head": SHA,
            "binary_sha256": "b" * 64, "profile_hash": "c" * 64}


KEY = "E-1/claude/tmux/channel/sequential/announce_bot/none/none/default/delivery/1"


def report():
    return {"schema_version": 1, **IDENTITY, "run_id": "run", "suite": "R1", "profile": "default",
            "started_at": 100, "completed_at": 110, "expected": [KEY],
            "occurrences": [{"key": KEY, "actual_occurrence_key": KEY, "status": "PASS", "scenario": "E-1", "provider": "claude", "host": "tmux",
                             "surface": "channel", "state": "sequential", "origin": "announce_bot",
                             "busy_basis": None, "placement": None, "output_profile": "default",
                             "operation": "delivery", "occurrence": 1,
                             "assertions_expected": ["delivery"],
                             "assertions": [{"key": "delivery", "status": "PASS", "complete": True}],
                             "real_provider_contacted": True, "evidence_paths": ["raw.json"], **IDENTITY}],
            "result": "PASS", "usage_complete": True,
            "usage": {"claude": {"native_turns": 1, "input_tokens": 0, "output_tokens": 1,
                                 "cache_read_tokens": 0, "cache_write_tokens": 0}}, "attempts": []}


def status():
    return {"schema_version": 1, "enabled": True, "heartbeat_at": 115, "tick_interval_s": 10,
            "rotation_interval_s": 100, "max_run_s": 30, "tick_grace_s": 5,
            "last_completed_run": "run", "outbox_pending": 0, "next_due": 200}


class Summary(unittest.TestCase):
    def test_complete_live_report_is_pass_and_formats_counts(self):
        view = summary.consume(report(), status(), head=SHA, now=120)
        self.assertEqual(view["check"]["result"], "PASS")
        self.assertEqual((view["check"]["live"], view["check"]["expected"],
                          view["check"]["assertions"], view["check"]["assertions_expected"]), (1, 1, 1, 1))
        check, monitor = summary.format_lines(view)
        self.assertEqual(check, f"E2E_CHECK suite=R1 head={SHA} result=PASS live=1/1 assertions=1/1 age_s=10 stale=0 reason=none")
        self.assertIn("E2E_MONITOR enabled=1 heartbeat_age_s=5 last_run=run pending_alerts=0 next_due=200", monitor)

    def test_zero_skip_fixture_missing_assertion_timeout_and_unknown_usage_are_not_pass(self):
        variants = []
        zero = report(); zero.update(expected=[], occurrences=[]); variants.append((zero, "NO_LIVE_OCCURRENCES"))
        for field, value, reason in [("status", "SKIPPED", "SKIPPED"), ("fixture", True, "FIXTURE"),
                                     ("dry_run", True, "FIXTURE"), ("timeout", True, "TIMEOUT"),
                                     ("dirty_residue", True, "DIRTY_RESIDUE"),
                                     ("real_provider_contacted", False, "NO_PROVIDER_CONTACT"),
                                     ("evidence_paths", [], "MISSING_EVIDENCE")]:
            value_report = report(); value_report["occurrences"][0][field] = value
            variants.append((value_report, reason))
        missing = report(); missing["occurrences"][0]["assertions"] = []; variants.append((missing, "MISSING_ASSERTIONS"))
        measured = summary.evaluate(missing)
        self.assertEqual((measured["live"], measured["assertions"], measured["assertions_expected"]), (1, 0, 1))
        no_required = report(); no_required["occurrences"][0]["assertions_expected"] = []; variants.append((no_required, "MISSING_ASSERTIONS"))
        incomplete = report(); incomplete["occurrences"][0]["assertions"][0]["complete"] = False
        variants.append((incomplete, "MISSING_ASSERTIONS"))
        usage = report(); usage["usage"]["claude"]["native_turns"] = None; variants.append((usage, "USAGE_UNKNOWN"))
        flag = report(); flag["usage_complete"] = False; variants.append((flag, "USAGE_UNKNOWN"))
        for counter in (True, -1, 0.5):
            invalid = report(); invalid["usage"]["claude"]["native_turns"] = counter
            variants.append((invalid, "USAGE_UNKNOWN"))
        no_provider = report(); no_provider["usage"] = {}; variants.append((no_provider, "USAGE_UNKNOWN"))
        for value_report, reason in variants:
            with self.subTest(reason=reason):
                result = summary.evaluate(value_report, head=SHA)
                self.assertNotEqual(result["result"], "PASS")
                self.assertEqual(result["reason"], reason)

    def test_expected_occurrence_identity_and_duplicate_assertions_cannot_be_forged(self):
        changed = report(); changed["expected"].append("missing")
        self.assertEqual(summary.evaluate(changed)["result"], "PARTIAL")
        for field in IDENTITY:
            changed = report(); changed["occurrences"][0][field] = "wrong"
            self.assertEqual(summary.evaluate(changed)["reason"], "IDENTITY_MISMATCH")
        changed = report(); changed["occurrences"].append(copy.deepcopy(changed["occurrences"][0]))
        self.assertEqual(summary.evaluate(changed)["reason"], "DUPLICATE_OCCURRENCE")
        changed = report(); changed["occurrences"][0]["assertions"].append(copy.deepcopy(changed["occurrences"][0]["assertions"][0]))
        self.assertEqual(summary.evaluate(changed)["reason"], "MISSING_ASSERTIONS")
        changed = report(); changed["binary_sha256"] = None
        self.assertEqual(summary.evaluate(changed)["reason"], "MISSING_IDENTITY")
        changed = report(); changed["head"] = "z" * 40
        self.assertEqual(summary.evaluate(changed)["reason"], "MISSING_IDENTITY")

    def test_old_head_schema_future_time_stale_and_no_data_are_explicit(self):
        self.assertEqual(summary.consume(None, status(), head=SHA, now=120)["check"]["result"], "NO_DATA")
        value = report(); value["schema_version"] = 2
        self.assertEqual(summary.consume(value, status(), head=SHA, now=120)["check"]["reason"], "UNSUPPORTED_SCHEMA")
        self.assertEqual(summary.consume(report(), status(), head="d" * 40, now=120)["check"]["reason"], "STALE_HEAD")
        value = report(); value["completed_at"] = 121
        self.assertEqual(summary.consume(value, status(), head=SHA, now=120)["check"]["reason"], "FUTURE_TIMESTAMP")
        value = status(); value["heartbeat_at"] = 121
        self.assertEqual(summary.consume(report(), value, head=SHA, now=120)["check"]["reason"], "FUTURE_TIMESTAMP")
        value = status(); value["heartbeat_at"] = 300
        self.assertEqual(summary.consume(report(), value, head=SHA, now=300)["check"]["reason"], "STALE")
        value = status(); value["heartbeat_at"] = 100
        self.assertEqual(summary.consume(report(), value, head=SHA, now=120)["check"]["reason"], "HEARTBEAT_STALE")
        value = status(); value["enabled"] = False
        disabled = summary.consume(report(), value, head=SHA, now=120)
        self.assertEqual(disabled["check"]["result"], "DISABLED")
        self.assertEqual(disabled["monitor"]["heartbeat_age_s"], 5)

    def test_failures_and_failed_attempts_are_preserved(self):
        value = report(); value["occurrences"][0]["assertions"][0]["status"] = "FAIL"
        self.assertEqual(summary.evaluate(value)["result"], "FAIL")
        value = report(); value["attempts"] = [{"result": "FAIL", "classification": "ENV_SUSPECT"}]
        self.assertEqual(summary.evaluate(value)["result"], "RECOVERED_ENV")
        value["attempts"] = [{"result": "FAIL"}]
        self.assertEqual(summary.evaluate(value)["result"], "FLAKY")
        value = report(); value["result"] = "FAIL"
        self.assertEqual(summary.evaluate(value)["result"], "FAIL")

    def test_builder_preserves_input_report_and_unknowns_without_mutation(self):
        value = report(); before = copy.deepcopy(value)
        value["bundle"] = value.pop("suite")
        built = summary.build(value, IDENTITY)
        self.assertEqual(built["schema_version"], 1)
        self.assertEqual(built["suite"], "R1")
        self.assertEqual(summary.evaluate(built)["result"], "PASS")
        self.assertEqual(value["occurrences"], before["occurrences"])
        self.assertNotIn("suite", value)
        value["usage"]["claude"]["native_turns"] = None
        self.assertIsNone(summary.build(value, IDENTITY)["usage"]["claude"]["native_turns"])
        value["occurrences"][0]["origin"] = "controlled_ingress"
        built = summary.build(value, IDENTITY)
        self.assertEqual(built["result"], "PARTIAL")

    def test_planned_keys_and_required_assertions_cannot_be_replaced_by_driver(self):
        manifest = json.loads((Path(__file__).resolve().parents[3] / "docs/e2e-continuous/bundles.schema1.json").read_text())
        self.assertEqual(list(summary.KEY_FIELDS), manifest["execution_contract"]["occurrence_key_fields"])
        row = report()["occurrences"][0]
        row["key"] = "forged/driver/key"
        self.assertEqual(summary.occurrence_key(row), KEY)
        for field in ("state", "origin", "busy_basis", "placement", "output_profile", "operation", "occurrence"):
            changed = copy.deepcopy(row); changed[field] = "different"
            self.assertNotEqual(summary.occurrence_key(changed), KEY)
        built = summary.build(report(), {**IDENTITY, "required_assertions": {"E-1": ["delivery", "ordering"]}})
        self.assertEqual(built["result"], "UNEVALUABLE")
        self.assertEqual(built["occurrences"][0]["assertions_expected"], ["delivery", "ordering"])
        altered = report(); altered["occurrences"][0]["key"] = "forged/driver/key"; altered["expected"] = ["forged/driver/key"]
        self.assertEqual(summary.evaluate(altered)["reason"], "OCCURRENCE_KEY_MISMATCH")
        unknown = report(); unknown["occurrences"][0].pop("origin")
        unknown["occurrences"][0]["key"] = summary.occurrence_key(unknown["occurrences"][0])
        unknown["expected"] = [unknown["occurrences"][0]["key"]]
        self.assertEqual(summary.evaluate(unknown)["reason"], "MISSING_OCCURRENCE_CONTRACT")
        base = report()["occurrences"][0]
        base.pop("provider"); base.pop("host"); base.pop("output_profile"); base["cell"] = "claude-tui"
        self.assertEqual(summary.occurrence_key(base), KEY)

    def test_actual_occurrence_witness_must_match_the_planned_qualifiers(self):
        value = report()
        self.assertEqual(summary.evaluate(value)["result"], "PASS")
        for actual in (None, "E-1/claude/tmux/channel/sequential/controlled_ingress/none/none/default/delivery/1"):
            value["occurrences"][0]["actual_occurrence_key"] = actual
            evaluated = summary.evaluate(value)
            self.assertEqual(evaluated["result"], "UNEVALUABLE")
            self.assertEqual(evaluated["reason"], "OCCURRENCE_WITNESS_MISMATCH")
            self.assertEqual((evaluated["live"], evaluated["assertions"]), (0, 0))
            self.assertEqual(value["occurrences"][0]["key"], KEY)

    def test_malformed_input_is_unknown_without_exceptions_or_claimed_pass(self):
        malformed = report(); malformed["occurrences"][0]["assertions"][0]["key"] = []
        for value in ([], malformed, {**report(), "schema_version": True}, {"schema_version": 1}, {**report(), "occurrences": [None]},
                      {**report(), "expected": [None]}, {**report(), "usage": {"claude": None}}):
            self.assertNotEqual(summary.evaluate(value)["result"], "PASS")
        for value in (None, [], {"schema_version": 2}, {**status(), "schema_version": True}, {**status(), "heartbeat_at": "broken"}):
            self.assertEqual(summary.consume(report(), value, head=SHA, now=120)["check"]["result"], "UNKNOWN")


if __name__ == "__main__":
    unittest.main()
