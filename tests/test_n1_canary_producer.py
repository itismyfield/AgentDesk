"""Producer approvals and unknown witnesses use hand-defined CLI evidence."""
import copy
import json
import re
import subprocess
import unittest

from tests import test_n1_canary_audit as fixture


class ProducerCLI(unittest.TestCase):
    setUp = fixture.AuditCLI.setUp
    save = fixture.AuditCLI.save
    capture = fixture.AuditCLI.capture
    refresh_history = fixture.AuditCLI.refresh_history
    run_cli = fixture.AuditCLI.run_cli
    append_log_rows = fixture.AuditCLI.append_log_rows

    def approval(self):
        runtime = self.manifest["runtime"]
        return dict(build_commit=runtime["build_commit"], executable_sha256=runtime["executable_sha256"],
                    event_schema=2, unknown_outcome_produced=True, actual_confirmation_produced=True)

    def witness(self, **changes):
        value = dict(event="n1_outcome_unknown", seq=52, schema_version=2, observed_at_us=100,
                     boot_id=self.manifest["runtime"]["boot_id"], channel_id=123, provider="codex",
                     source_id=copy.deepcopy(self.manifest["scope"]["source"]), witness_offset=100,
                     logical_turn_key=None, reason="anonymous_terminal")
        value.update(changes)
        return value

    def append_unknown(self, rows):
        self.append_log_rows(rows)
        self.values["end_snapshot"]["counters"]["unknown_outcome_events"] = len(rows)

    def test_t9_approved_pair_and_shipped_table(self):
        approved = self.run_cli(0)
        self.assertEqual(approved["observation"]["unknown_outcome_events"], 0)
        self.assertEqual(approved["observation"]["unknown_witnesses_distinct"], 0)
        shipped = self.run_cli(2, supported=False)
        self.assertEqual(shipped["reasons"], ["unknown_outcome_unproduced"])
        self.assertIsNone(shipped["observation"]["unknown_outcome_events"])
        self.assertEqual(json.loads(fixture.PACKAGE.joinpath("compatibility.json").read_text())["producer_capabilities"], [])

    def test_t9_unapproved_pair_or_capability_never_becomes_zero(self):
        original = self.approval()
        for field, value in (("build_commit", "0" * 40), ("executable_sha256", "0" * 64),
                             ("event_schema", 1), ("unknown_outcome_produced", False),
                             ("actual_confirmation_produced", False)):
            with self.subTest(field=field):
                self.producer_capabilities = [{**original, field: value}]
                result = self.run_cli(2)
                self.assertIn("unknown_outcome_unproduced", result["reasons"])
                self.assertIsNone(result["observation"]["unknown_outcome_events"])
        self.producer_capabilities = []
        self.assertIn("unknown_outcome_unproduced", self.run_cli(2)["reasons"])

    def test_t9_capability_shape_is_strict(self):
        for edit in ("missing", "extra", "bool_schema", "bad_digest", "string_capability"):
            approval = self.approval()
            if edit == "missing":
                approval.pop("actual_confirmation_produced")
            elif edit == "extra":
                approval["future_field"] = True
            elif edit == "bool_schema":
                approval["event_schema"] = True
            elif edit == "bad_digest":
                approval["executable_sha256"] = "bad"
            else:
                approval["unknown_outcome_produced"] = "true"
            self.producer_capabilities = [approval]
            with self.subTest(edit=edit):
                self.assertIn("producer_capabilities_invalid", self.run_cli(2)["reasons"])

    def test_t9_binary_capture_still_requires_identity_proof(self):
        self.values["executable"] = b"different captured binary"
        result = self.run_cli(2)
        self.assertIn("build_identity_unverified", result["reasons"])
        self.assertIn("unknown_outcome_unproduced", result["reasons"])
        self.assertIsNone(result["observation"]["unknown_outcome_events"])

    def test_t9_consistent_capture_of_other_binary_or_old_build_is_unapproved(self):
        self.producer_capabilities = [self.approval()]
        self.values["executable"] = b"another binary with a consistent capture"
        self.manifest["runtime"]["executable_sha256"] = fixture.digest(self.values["executable"])
        self.values["process_proof"]["executable_sha256"] = self.manifest["runtime"]["executable_sha256"]
        result = self.run_cli(2)
        self.assertNotIn("build_identity_unverified", result["reasons"])
        self.assertIn("unknown_outcome_unproduced", result["reasons"])
        self.manifest["runtime"]["build_commit"] = "0" * 40
        self.values["process_proof"]["build_commit"] = "0" * 40
        self.assertIn("unknown_outcome_unproduced", self.run_cli(2)["reasons"])

    def test_t10_unknown_reread_is_one_witness_two_raw_events(self):
        self.append_unknown([self.witness(), self.witness(seq=53, observed_at_us=101)])
        result = self.run_cli(2)
        self.assertIn("unknown_outcome_present", result["reasons"])
        self.assertNotIn("audit_event_unsupported", result["reasons"])
        self.assertEqual(result["observation"]["unknown_outcome_events"], 2)
        self.assertEqual(result["observation"]["unknown_witnesses_distinct"], 1)
        self.assertEqual(result["turns"]["completed_distinct"], 50)

    def test_t10_counter_only_missing_log_and_regression_are_unknown(self):
        self.values["end_snapshot"]["counters"]["unknown_outcome_events"] = 1
        result = self.run_cli(2)
        self.assertIn("unknown_counter_mismatch", result["reasons"])
        self.assertIsNone(result["observation"]["unknown_outcome_events"])
        self.append_unknown([self.witness()])
        self.values["end_snapshot"]["counters"]["unknown_outcome_events"] = 0
        self.assertIn("unknown_counter_mismatch", self.run_cli(2)["reasons"])
        self.values["baseline"]["counters"]["unknown_outcome_events"] = 2
        self.assertIn("unknown_counter_mismatch", self.run_cli(2)["reasons"])

    def test_t10_sequence_gap_and_mixed_boot_do_not_report_numbers(self):
        self.append_unknown([self.witness()])
        original = copy.deepcopy(self.values["log"])
        for field, value in (("seq", 54), ("seq", None), ("boot_id", "22222222-2222-4222-8222-222222222222")):
            self.values["log"] = copy.deepcopy(original)
            self.values["log"][-1][field] = value
            with self.subTest(field=field):
                result = self.run_cli(2)
                self.assertIn("observation_sequence_incomplete", result["reasons"])
                self.assertIsNone(result["observation"]["unknown_outcome_events"])

    def test_required_guards_producer_recipe_executes_tests_and_mutants(self):
        script = (fixture.ENTRY.parent / "ci-script-checks.sh").read_text()
        block = re.search(r'if run_check guards "N1 canary producer capability contracts"; then\n(.*?)\nfi', script, re.S)
        self.assertIsNotNone(block)
        recipe = block.group(1)
        self.assertIn('"$PYTHON" -m unittest tests.test_n1_canary_producer', recipe)
        self.assertIn('"$PYTHON" tests/run_n1_canary_producer_mutations.py', recipe)
        self.assertTrue((fixture.ENTRY.parents[1] / "tests/run_n1_canary_producer_mutations.py").is_file())
        stub = self.base / "record-python"
        recorded = self.base / "commands"
        stub.write_text('#!/usr/bin/env bash\nprintf "%s\\n" "$*" >> "$N1_RECORDED"\n')
        stub.chmod(0o700)
        import os
        process = subprocess.run(["bash", "-euc", recipe], env={**os.environ, "PYTHON": str(stub), "N1_RECORDED": str(recorded)},
                                 capture_output=True, text=True, timeout=10)
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(recorded.read_text().splitlines(), ["-m unittest tests.test_n1_canary_producer",
                                                           "tests/run_n1_canary_producer_mutations.py"])

    def test_mutation_runner_noop_is_a_survivor_with_green_control(self):
        from contextlib import redirect_stdout
        from io import StringIO
        from unittest.mock import patch
        from tests import run_n1_canary_producer_mutations as runner
        before = '"$PYTHON" -m unittest tests.test_n1_canary_producer'
        case = ("producer-ci-noop", "ci-script-checks.sh", before, before,
                "test_required_guards_producer_recipe_executes_tests_and_mutants")
        output = StringIO()
        with patch.object(runner, "MUTANTS", [case]), redirect_stdout(output):
            survived = runner.main()
        self.assertTrue(survived, output.getvalue())
        self.assertIn("control GREEN", output.getvalue())
        self.assertIn("producer-ci-noop: SURVIVED/INVALID", output.getvalue())

    def test_mutation_runner_rejects_a_broken_control(self):
        from contextlib import redirect_stdout
        from io import StringIO
        from unittest.mock import patch
        from tests import run_n1_canary_producer_mutations as runner
        before = '"$PYTHON" -m unittest tests.test_n1_canary_producer'
        case = ("broken-ci-control", "ci-script-checks.sh", before, before,
                "test_required_guards_producer_recipe_executes_tests_and_mutants")
        copy_file = runner.shutil.copy2

        def omit_runner(source, destination):
            if source.name != "run_n1_canary_producer_mutations.py":
                return copy_file(source, destination)

        with patch.object(runner, "MUTANTS", [case]), patch.object(runner.shutil, "copy2", omit_runner), redirect_stdout(StringIO()):
            with self.assertRaisesRegex(AssertionError, "control not GREEN"):
                runner.main()

    def test_t10_unknown_shape_source_and_logical_scope_are_strict(self):
        self.append_unknown([self.witness()])
        original = copy.deepcopy(self.values["log"][-1])
        logical = dict(provider="codex", channel_id=123, provider_session_id="native-session", native_turn_id="uncertain")
        for changes in ({"witness_offset": True}, {"witness_offset": self.manifest["scope"]["closure_offset"] + 1},
                        {"reason": ""}, {"extra": "field"}, {"source_id": {**original["source_id"], "ino": 99}},
                        {"logical_turn_key": {**logical, "provider_session_id": "other"}},
                        {"logical_turn_key": {**logical, "native_turn_id": 42}}):
            self.values["log"][-1] = {**original, **changes}
            with self.subTest(changes=changes):
                self.assertIn("unknown_witness_invalid", self.run_cli(2)["reasons"])

    def test_t10_resume_cannot_erase_prebaseline_unknown(self):
        witness = self.witness(seq=2, observed_at_us=25)
        for row in self.values["log"][1:]:
            row["seq"] += 1
        self.values["log"].insert(1, witness)
        self.append_log_rows([])
        self.values["baseline"]["seq_high_water"] = 2
        self.values["baseline"]["counters"]["unknown_outcome_events"] = 1
        self.values["end_snapshot"]["counters"]["unknown_outcome_events"] = 1
        result = self.run_cli(2)
        self.assertIn("unknown_outcome_present", result["reasons"])
        self.assertEqual(result["observation"]["unknown_outcome_events"], 0)
        self.assertEqual(result["observation"]["unknown_witnesses_distinct"], 1)

    def test_prebaseline_unknown_without_witness_is_incomplete(self):
        self.run_cli(0)
        for row in self.values["log"]:
            row["seq"] += 1
        self.values["log"][0]["confirmation_window_start"]["seq"] = 1
        self.manifest["observation"].update(seq_start=2, confirmation_seq=2)
        self.append_log_rows([])
        for name in ("baseline", "end_snapshot"):
            self.values[name]["counters"]["unknown_outcome_events"] = 1
            self.values[name]["first_confirmation"]["seq"] = 1
        self.values["baseline"]["seq_high_water"] = 2
        missing = self.run_cli(2)
        self.assertIn("unknown_history_incomplete", missing["reasons"])
        self.assertTrue(missing["observation"]["sequence_complete"])
        self.assertIsNone(missing["observation"]["unknown_outcome_events"])
        self.assertIsNone(missing["observation"]["unknown_witnesses_distinct"])
        self.assertEqual(missing["failures"], [])
        self.assertIn("unknown_outcome_unproduced", self.run_cli(2, supported=False)["reasons"])
        self.values["log"].insert(0, self.witness(seq=1, observed_at_us=5, witness_offset=0))
        self.manifest["observation"]["seq_start"] = 1
        restored = self.run_cli(2)
        self.assertNotIn("unknown_history_incomplete", restored["reasons"])
        self.assertIn("unknown_outcome_present", restored["reasons"])
        self.assertEqual(restored["observation"]["unknown_outcome_events"], 0)
        self.assertEqual(restored["observation"]["unknown_witnesses_distinct"], 1)

    def test_t10_named_unknown_conflicts_with_completed_not_resolved_by_it(self):
        logical = copy.deepcopy(self.values["log"][1]["logical_turn_key"])
        self.append_unknown([self.witness(logical_turn_key=logical, reason="edge_turn")])
        self.assertIn("unknown_outcome_conflict", self.run_cli(2)["reasons"])

    def test_t10_resume_retains_unresolved_witness_before_confirmation(self):
        prefix = self.values["transcript"].splitlines(keepends=True)[0]
        self.manifest["scope"]["baseline_offset"] = len(prefix)
        self.manifest["source_digests"] = dict(prefix_sha256=fixture.digest(prefix),
            window_sha256=fixture.digest(self.values["transcript"][len(prefix):]))
        for row in self.values["log"]:
            row["seq"] += 1
        self.values["log"][0]["confirmation_window_start"]["seq"] = 1
        self.values["log"].insert(0, self.witness(seq=1, observed_at_us=5, witness_offset=0))
        self.manifest["observation"]["confirmation_seq"] = 2
        self.append_log_rows([])
        for name in ("baseline", "end_snapshot"):
            self.values[name]["counters"]["unknown_outcome_events"] = 1
            self.values[name]["first_confirmation"]["seq"] = 1
        self.values["baseline"]["seq_high_water"] = 2
        result = self.run_cli(2)
        self.assertIn("unknown_outcome_present", result["reasons"])
        self.assertEqual(result["observation"]["unknown_witnesses_distinct"], 1)

    def test_t8_49_native_plus_logical_reread_never_counts_fifty(self):
        self.values["transcript"] = self.values["transcript"].replace(b't49', b't48').replace(b'b49', b'b48').replace(b'body 49', b'body 48')
        self.manifest["source_digests"]["window_sha256"] = fixture.digest(self.values["transcript"])
        proof = self.inputs[-1]
        proof["physical_observation_key"]["native_turn_id"] = "t48"
        proof["native_record_digest"] = self.inputs[-2]["native_record_digest"]
        last = self.values["log"][-1]
        last["logical_turn_key"] = copy.deepcopy(self.values["log"][-2]["logical_turn_key"])
        last["physical_observation_key"]["native_turn_id"] = "t48"
        self.values["o_ledger"] = self.values["o_ledger"][:-2]
        self.messages.pop()
        self.refresh_history()
        result = self.run_cli(2)
        self.assertIn("native_sample_under_50", result["reasons"])
        self.assertEqual(result["turns"]["completed_native_distinct"], 49)
        self.assertIsNone(result["turns"]["completed_distinct"])

    def test_t8_physical_producer_reread_is_not_a_new_completed_turn(self):
        row = copy.deepcopy(self.values["log"][-1])
        row.update(seq=52, observed_at_us=100)
        self.append_log_rows([row])
        result = self.run_cli(0)
        self.assertEqual(self.values["end_snapshot"]["counters"]["closed_events"], 51)
        self.assertEqual(result["turns"]["completed_distinct"], 50)

    def test_t11_missing_native_production_even_with_fifty_raw_events(self):
        last = self.values["log"][-1]
        duplicate = copy.deepcopy(self.values["log"][-2])
        duplicate.update(seq=last["seq"], observed_at_us=last["observed_at_us"])
        self.values["log"][-1] = duplicate
        self.assertIn("native_event_missing", self.run_cli(2)["reasons"])

    def test_t11_only_49_production_events_is_unknown(self):
        self.values["log"].pop()
        self.append_log_rows([])
        self.assertIn("native_event_missing", self.run_cli(2)["reasons"])

    def test_t12_denied_confirmation_capture_stays_unknown(self):
        self.values["log"].pop(0)
        self.manifest["observation"].update(seq_start=2, actual_confirmed_at_us=None)
        for name in ("baseline", "end_snapshot"):
            self.values[name].update(confirmation_observed=False, first_confirmation=None)
        self.values["baseline"]["seq_high_water"] = 2
        self.values["baseline"]["counters"]["closed_events"] = 1
        result = self.run_cli(2)
        self.assertIn("confirmation_boundary_unknown", result["reasons"])
        self.assertIsNone(result["confirmation"]["actual_confirmed_at_us"])

    def test_t12_denied_capture_does_not_rewrite_legacy_owner(self):
        self.values["log"][0]["event"] = "retirement_denied"
        self.values["legacy"]["owner"] = dict(writer="legacy", turn_mode="legacy", identity=self.manifest["binding"]["identity"])
        self.values["baseline"].update(confirmation_observed=False, first_confirmation=None)
        self.values["end_snapshot"].update(confirmation_observed=False, first_confirmation=None)
        original = copy.deepcopy(self.values["legacy"])
        result = self.run_cli(2)
        self.assertIn("confirmation_boundary_unknown", result["reasons"])
        self.assertIn("o_owner_unverified", result["reasons"])
        self.assertEqual(json.loads((self.base / self.manifest["files"]["legacy"]["path"]).read_bytes()), original)

    def test_supported_unavailable_drop_and_open_incomplete_keep_unknown_null(self):
        original = copy.deepcopy(self.values["end_snapshot"])
        for changes in ({"status": "unavailable"}, {"dropped_events": 1}, {"open_attempts_complete": False}):
            self.values["end_snapshot"] = {**original, **changes}
            with self.subTest(changes=changes):
                result = self.run_cli(2)
                self.assertIsNone(result["observation"]["unknown_outcome_events"])

    def test_actual_confirmation_is_receipt_time_with_inclusive_bounds(self):
        event = self.values["log"][0]
        event["observed_at_us"] = 1000
        for actual in (10, 15, 1000):
            event["actual_confirmed_at_us"] = actual
            self.manifest["observation"]["actual_confirmed_at_us"] = actual
            result = self.run_cli(0)
            self.assertEqual(result["confirmation"], dict(event_observed_at_us=1000, actual_confirmed_at_us=actual))
        event["actual_confirmed_at_us"] = 1001
        self.manifest["observation"]["actual_confirmed_at_us"] = 1001
        self.assertIn("confirmation_boundary_unknown", self.run_cli(2)["reasons"])


if __name__ == "__main__":
    unittest.main()
