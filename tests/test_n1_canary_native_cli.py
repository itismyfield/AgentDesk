"""Native oracle integration through the actual CLI, using captured-file fixtures only."""
import unittest

from tests import test_n1_canary_oracle as input_helpers
from tests.test_n1_canary_native import encode, FIXTURES


class NativeCLI(unittest.TestCase):
    setUp = input_helpers.Fixture.setUp
    save = input_helpers.Fixture.save
    cli = input_helpers.Fixture.cli

    def use_native(self, raw):
        self.values["transcript"] = raw
        self.manifest["scope"].update(session_id="oracle-native-session", fiftieth_offset=len(raw), closure_offset=len(raw))
        self.manifest["scope"]["source"]["session_id"] = "oracle-native-session"
        self.manifest["source_digests"]["window_sha256"] = input_helpers.digest(raw)
        self.save()

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
