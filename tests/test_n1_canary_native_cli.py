"""Native oracle integration through the actual CLI, using captured-file fixtures only."""
import copy
import json
import unittest

from tests import test_n1_canary_oracle as input_helpers
from tests.test_n1_canary_native import encode, FIXTURES


class NativeCLI(unittest.TestCase):
    setUp = input_helpers.Fixture.setUp
    save = input_helpers.Fixture.save
    cli = input_helpers.Fixture.cli

    def use_native(self, raw, provider="codex", baseline=0):
        self.values["transcript"] = raw
        self.manifest["scope"].update(provider=provider, session_id="oracle-native-session", baseline_offset=baseline,
                                      fiftieth_offset=len(raw), closure_offset=len(raw))
        self.manifest["policies"]["provider_format"] = provider + "-jsonl-v1"
        self.manifest["scope"]["source"]["session_id"] = "oracle-native-session"
        for binding in self.values["bindings"]:
            binding["provider"] = provider
        self.manifest["source_digests"] = {"prefix_sha256": input_helpers.digest(raw[:baseline]),
                                            "window_sha256": input_helpers.digest(raw[baseline:])}
        self.save()

    def codex_turn(self):
        return [json.loads(line) for line in (FIXTURES / "codex.jsonl").read_bytes().splitlines()[:8]]

    def assert_native_unknown(self, result, reason):
        self.assertIsNone(result["turns"]["completed_native_distinct"])
        self.assertNotIn("native_units", result)
        self.assertIn(reason, result["reasons"])

    def test_codex_excluded_output_shapes_fail_closed_in_actual_cli(self):
        for output in (42, None, {"type": "future_output"}, [{"type": "future_output", "text": "opaque"}]):
            with self.subTest(output=output):
                rows = self.codex_turn()
                rows[6]["payload"]["output"] = output
                self.use_native(encode(rows))
                self.assert_native_unknown(self.cli(), "native_record_unsupported")

    def test_codex_missing_arguments_fail_closed_in_actual_cli(self):
        rows = self.codex_turn()
        del rows[5]["payload"]["arguments"]
        self.use_native(encode(rows))
        self.assert_native_unknown(self.cli(), "native_record_unsupported")

    def test_codex_subtype_required_fields_are_not_replaced_by_tool_payload(self):
        variants = [(5, "arguments", 42), (5, "name", None), (5, "name", "missing"), (5, "call_id", None),
                    (6, "output", "missing"), (6, "call_id", None)]
        for index, field, value in variants:
            with self.subTest(index=index, field=field):
                rows = self.codex_turn()
                if value == "missing":
                    del rows[index]["payload"][field]
                else:
                    rows[index]["payload"][field] = value
                self.use_native(encode(rows))
                result = self.cli()
                self.assertIsNone(result["turns"]["completed_native_distinct"])
                self.assertNotIn("native_units", result)
        for value in (None, 42, {}, "missing"):
            with self.subTest(custom_input=value):
                rows = self.codex_turn()
                rows[5]["payload"]["type"] = "custom_tool_call"
                if value != "missing":
                    rows[5]["payload"]["input"] = value
                self.use_native(encode(rows))
                self.assert_native_unknown(self.cli(), "native_record_unsupported")

    def test_valid_codex_subtypes_keep_native_completion_and_exclusion(self):
        for custom in (False, True):
            for output in ("one", [{"type": "input_text", "text": "one"}]):
                with self.subTest(custom=custom, output=output):
                    rows = self.codex_turn()
                    if custom:
                        rows[5]["payload"]["type"] = "custom_tool_call"
                        rows[5]["payload"]["input"] = rows[5]["payload"].pop("arguments")
                        rows[6]["payload"]["type"] = "custom_tool_call_output"
                    rows[6]["payload"]["output"] = output
                    self.use_native(encode(rows))
                    result = self.cli()
                    self.assertEqual(result["turns"]["completed_native_distinct"], 1)
                    self.assertEqual(len(result["native_units"]), 3)
                    self.assertEqual(result["native_units"][-1]["excluded"], "codex_tool_output")
                    self.assertNotIn("native_record_unsupported", result["reasons"])

    def test_codex_content_item_arrays_require_supported_items(self):
        for kind in ("function_call_output", "custom_tool_call_output"):
            for output in ([42], [{"type": "input_text"}], [{"type": "input_text", "text": None}],
                           [{"type": "input_image", "image_url": "fixture://image"}],
                           [{"type": "input_text", "text": "one"}, {"type": "future_output"}]):
                with self.subTest(kind=kind, output=output):
                    rows = self.codex_turn()
                    rows[6]["payload"].update(type=kind, output=output)
                    self.use_native(encode(rows))
                    self.assert_native_unknown(self.cli(), "native_record_unsupported")

    def test_tool_search_subtypes_validate_their_own_required_fields(self):
        call = {"type": "tool_search_call", "id": "search-1", "call_id": "search-call",
                "execution": "client", "arguments": {"query": "fixture"}}
        output = {"type": "tool_search_output", "call_id": "search-call", "status": "completed",
                  "execution": "client", "tools": []}
        rows = self.codex_turn()
        rows[5]["payload"], rows[6]["payload"] = call, output
        self.use_native(encode(rows))
        self.assertEqual(self.cli()["turns"]["completed_native_distinct"], 1)
        for index, field in ((5, "execution"), (5, "arguments"), (6, "status"), (6, "execution"), (6, "tools")):
            changed = copy.deepcopy(rows)
            del changed[index]["payload"][field]
            with self.subTest(index=index, field=field):
                self.use_native(encode(changed))
                self.assert_native_unknown(self.cli(), "native_record_unsupported")
        for tools in (42, None, {}, [42]):
            changed = copy.deepcopy(rows)
            changed[6]["payload"]["tools"] = tools
            with self.subTest(tools=tools):
                self.use_native(encode(changed))
                self.assert_native_unknown(self.cli(), "native_record_unsupported")

    def test_claude_tail_reobservation_cannot_create_a_new_native_turn(self):
        lines = (FIXTURES / "claude.jsonl").read_bytes().splitlines(keepends=True)
        prefix = b"".join(lines[:7])
        raw = prefix + lines[2] + lines[6]
        for baseline in (0, len(prefix)):
            with self.subTest(baseline=baseline):
                self.use_native(raw, "claude", baseline)
                result = self.cli()
                self.assertIn(result["turns"]["completed_native_distinct"], (None, 0 if baseline else 1))
                self.assert_native_unknown(result, "native_uuid_owner_conflict")

    def test_49_claude_tail_reobservations_cannot_inflate_one_completion_to_50(self):
        rows = [json.loads(line) for line in (FIXTURES / "claude.jsonl").read_bytes().splitlines()[:7]]
        assistants = []
        for n in range(49):
            row = copy.deepcopy(rows[2])
            row["uuid"], row["message"]["id"] = f"a-{n}", f"m-{n}"
            assistants.append(row)
        original = [*rows[:2], *assistants, rows[6]]
        self.use_native(encode(original), "claude")
        result = self.cli()
        self.assertEqual(result["turns"]["completed_native_distinct"], 1)
        self.assertEqual(len(result["native_units"]), 49)
        self.use_native(encode([*original, *(row for assistant in assistants for row in (assistant, rows[6]))]), "claude")
        result = self.cli()
        self.assertIn(result["turns"]["completed_native_distinct"], (None, 1))
        self.assert_native_unknown(result, "native_uuid_owner_conflict")

    def test_complete_claude_reobservation_and_fresh_autonomous_uuid_keep_expected_counts(self):
        lines = (FIXTURES / "claude.jsonl").read_bytes().splitlines(keepends=True)
        original = b"".join(lines[:7])
        for baseline, count in ((0, 1), (len(original), 0)):
            with self.subTest(baseline=baseline):
                self.use_native(original + b"".join(lines[7:13]), "claude", baseline)
                result = self.cli()
                self.assertEqual(result["turns"]["completed_native_distinct"], count)
                self.assertEqual(result["turns"]["logical_reobservations"], 1)
        self.use_native(original + b"".join(lines[16:18]), "claude")
        result = self.cli()
        self.assertEqual(result["turns"]["completed_native_distinct"], 2)
        self.assertEqual(result["turns"]["logical_reobservations"], 0)
        self.assertNotIn("native_uuid_owner_conflict", result["reasons"])

    def test_actual_cli_derives_native_counts_without_pass(self):
        self.use_native((FIXTURES / "codex.jsonl").read_bytes())
        result = self.cli()
        self.assertEqual(result["turns"]["completed_native_distinct"], 3)
        self.assertIsNone(result["turns"]["completed_distinct"])
        self.assertEqual(len(result["native_units"]), 6)
        self.assertIn("stages_pending:o3", result["reasons"])
        self.assertNotIn("stages_pending:o2,o3", result["reasons"])

    def test_unknown_record_prevents_numeric_completion_output(self):
        raw = (FIXTURES / "codex.jsonl").read_bytes() + b'{"type":"future_record"}\n'
        self.use_native(raw)
        result = self.cli()
        self.assertIsNone(result["turns"]["completed_native_distinct"])
        self.assertTrue(any(r.startswith("native_") and r not in {"native_origin_unverified", "native_sample_under_50"} for r in result["reasons"]))
        self.assertNotIn("native_units", result)

    def test_fifty_completions_still_leave_origin_and_audit_unknown(self):
        rows = [{"type":"session_meta", "payload":{"id":"oracle-native-session"}}]
        for n in range(50):
            rows.extend([{"type":"event_msg", "payload":{"type":"task_started", "turn_id":f"t{n}"}},
                         {"type":"response_item", "payload":{"type":"message", "role":"assistant", "id":f"b{n}", "content":[{"type":"output_text", "text":f"body {n}"}]}},
                         {"type":"event_msg", "payload":{"type":"task_complete", "turn_id":f"t{n}"}}])
        self.use_native(encode(rows))
        result = self.cli()
        self.assertEqual(result["turns"]["completed_native_distinct"], 50)
        self.assertIsNone(result["turns"]["completed_distinct"])
        self.assertIn("native_origin_unverified", result["reasons"])
        self.assertIn("stages_pending:o3", result["reasons"])
        self.assertFalse(result["evidence_complete"])

    def test_pinned_normalization_is_required_and_versions_are_not_approximated(self):
        del self.manifest["policies"]["normalization"]
        self.save()
        self.assertIn("manifest_invalid", self.cli()["reasons"])
        self.manifest["policies"]["normalization"] = "nearby-v2"
        self.save()
        self.assertIn("manifest_invalid", self.cli()["reasons"])


if __name__ == "__main__":
    unittest.main()
