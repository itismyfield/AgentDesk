"""Hand-authored native identities and piece boundaries, independent of oracle output."""
import copy
import hashlib
import importlib
import json
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests/fixtures/n1_canary_o2"
sys.path.insert(0, str(ROOT / "scripts"))
from n1_canary import Unknown


def encode(rows):
    return b"".join(json.dumps(row, ensure_ascii=False, separators=(",", ":")).encode() + b"\n" for row in rows)


def scope(provider, raw, baseline=0):
    return dict(provider=provider, channel_id=123, session_id="oracle-native-session",
                source=dict(session_id="oracle-native-session", path="/fixture/native.jsonl", dev=1, ino=2),
                baseline_offset=baseline, fiftieth_offset=len(raw), closure_offset=len(raw))


def api(test, name):
    test.assertTrue((ROOT / "scripts/n1_canary" / (name + ".py")).is_file(), "independent oracle module missing: " + name)
    return importlib.import_module("n1_canary." + name)


class NativeContracts(unittest.TestCase):
    def fixture(self, provider):
        raw = (FIXTURES / (provider + ".jsonl")).read_bytes()
        return raw, scope(provider, raw)

    def test_hand_defined_turn_and_unit_sets_for_both_providers(self):
        module = api(self, "turns")
        expected = json.loads((FIXTURES / "expected.json").read_text())
        for provider, want in expected.items():
            with self.subTest(provider=provider):
                raw, window = self.fixture(provider)
                result = module.reconstruct(raw, window)
                for field in ("completed_native_distinct", "logical_reobservations", "aborted_distinct", "unit_reobservations"):
                    self.assertEqual(result["turns"][field], want[field])
                self.assertIsNone(result["turns"]["completed_distinct"])
                self.assertEqual([u["unit_key"]["native_key"] for u in result["units"]], want["unit_keys"])
                self.assertEqual([u["excluded"] for u in result["units"] if u["excluded"]], want["excluded"])
                hashes = [p["payload_hash"] for u in result["units"] for p in u["pieces"]]
                self.assertEqual(hashes, [hashlib.sha256(t.encode()).hexdigest() for t in want["piece_texts"]])
                self.assertIn("native_origin_unverified", result["reasons"])

    def test_prefix_ids_and_straddling_baseline_do_not_become_new_turns(self):
        module = api(self, "turns")
        raw, window = self.fixture("codex")
        window["baseline_offset"] = sum(len(x) for x in raw.splitlines(keepends=True)[:8])
        result = module.reconstruct(raw, window)
        self.assertEqual(result["turns"]["completed_native_distinct"], 2)
        self.assertEqual(result["turns"]["logical_reobservations"], 1)
        self.assertEqual([t["logical_turn_key"]["native_turn_id"] for t in result["candidates"]], ["t-3", "t-4"])
        window["baseline_offset"] = sum(len(x) for x in raw.splitlines(keepends=True)[:2])
        self.assertEqual(module.reconstruct(raw, window)["turns"]["completed_native_distinct"], 2)

    def test_physical_and_logical_keys_are_separate(self):
        module = api(self, "turns")
        raw, window = self.fixture("codex")
        result = module.reconstruct(raw, window)
        observations = [x for x in result["observations"] if x["logical_turn_key"]["native_turn_id"] == "t-1"]
        self.assertEqual(len(observations), 2)
        self.assertEqual(observations[0]["logical_turn_key"], observations[1]["logical_turn_key"])
        self.assertNotEqual(observations[0]["physical_observation_key"], observations[1]["physical_observation_key"])
        parsed = api(self, "native").parse(raw, window)
        again = module.derive([*parsed, parsed[0]], window)
        self.assertEqual(again["turns"]["completed_native_distinct"], 3)
        self.assertEqual(again["turns"]["physical_reobservations"], 1)
        self.assertEqual(len(again["observations"]), 5)

    def test_same_id_changed_body_hidden_tool_outcome_or_record_order_is_unknown(self):
        module = api(self, "turns")
        raw, _ = self.fixture("codex")
        rows = [json.loads(x) for x in raw.splitlines()]
        for change in ("body", "hidden_tool", "outcome", "order", "native_record_id"):
            mutated = copy.deepcopy(rows)
            if change == "body":
                mutated[14]["payload"]["content"][0]["text"] = "changed"
            elif change == "hidden_tool":
                mutated[16]["payload"]["output"] = "changed but excluded"
            elif change == "outcome":
                mutated[17]["payload"]["type"] = "turn_aborted"
            elif change == "order":
                mutated[15], mutated[16] = mutated[16], mutated[15]
            else:
                mutated[14]["uuid"] = "changed native row identity"
            data = encode(mutated)
            with self.subTest(change=change), self.assertRaises(Unknown):
                module.reconstruct(data, scope("codex", data))

    def test_timestamp_only_reobservation_is_supported_normalization(self):
        module = api(self, "turns")
        raw, _ = self.fixture("codex")
        rows = [json.loads(x) for x in raw.splitlines()]
        rows[14]["timestamp"] = "2026-10-13T00:01:00Z"
        data = encode(rows)
        self.assertEqual(module.reconstruct(data, scope("codex", data))["turns"]["completed_native_distinct"], 3)

    def test_unknown_records_partial_json_and_unknown_blocks_do_not_get_skipped(self):
        module = api(self, "turns")
        raw, _ = self.fixture("codex")
        rows = [json.loads(x) for x in raw.splitlines()]
        variants = [raw[:-1], raw + b'{"type":', raw + b'broken\n', raw + b'{"type":"future_record"}\n',
                    encode([*rows[:2], {"type":"event_msg", "payload":{"type":"future_event"}}, *rows[2:]])]
        unknown = copy.deepcopy(rows)
        unknown[4]["payload"]["content"][0]["type"] = "future_text"
        variants.append(encode(unknown))
        for data in variants:
            with self.subTest(data=data[-40:]), self.assertRaises(Unknown):
                module.reconstruct(data, scope("codex", data))

    def test_anonymous_mismatched_incomplete_and_announced_unsealed_turns_are_unknown(self):
        module = api(self, "turns")
        raw, _ = self.fixture("codex")
        rows = [json.loads(x) for x in raw.splitlines()]
        variants = []
        for index, field, value in ((1, "turn_id", None), (7, "turn_id", "other")):
            changed = copy.deepcopy(rows[:8])
            changed[index]["payload"][field] = value
            variants.append(encode(changed))
        variants.extend([encode(rows[:7]), encode([*rows[:4], rows[7]])])
        for data in variants:
            with self.subTest(data=data[-60:]), self.assertRaises(Unknown):
                module.reconstruct(data, scope("codex", data))

    def test_unit_key_conflict_across_different_turns_is_unknown(self):
        module = api(self, "turns")
        raw, _ = self.fixture("codex")
        rows = [json.loads(x) for x in raw.splitlines()]
        rows[22]["payload"]["id"] = "b-1"
        data = encode(rows)
        with self.assertRaises(Unknown):
            module.reconstruct(data, scope("codex", data))

    def test_50_unique_native_completions_keep_origin_and_final_audits_pending(self):
        module = api(self, "turns")
        rows = [{"type":"session_meta", "payload":{"id":"oracle-native-session"}}]
        for n in range(50):
            rows.extend([{"type":"event_msg", "payload":{"type":"task_started", "turn_id":f"t{n}"}},
                         {"type":"response_item", "payload":{"type":"message", "role":"assistant", "id":f"b{n}", "content":[{"type":"output_text", "text":f"body {n}"}]}},
                         {"type":"event_msg", "payload":{"type":"task_complete", "turn_id":f"t{n}"}}])
        data = encode(rows)
        result = module.reconstruct(data, scope("codex", data))
        self.assertEqual(result["turns"]["completed_native_distinct"], 50)
        self.assertEqual(len(result["candidates"]), 50)
        self.assertEqual(result["reasons"], ["native_origin_unverified"])
        self.assertIsNone(result["turns"]["completed_distinct"])
        self.assertEqual(len(result["units"]), 50)

    def test_session_rotation_and_non_record_baseline_are_unknown(self):
        module = api(self, "turns")
        raw, window = self.fixture("codex")
        rows = [json.loads(x) for x in raw.splitlines()]
        rows[0]["payload"]["id"] = "other-session"
        changed = encode(rows)
        with self.assertRaises(Unknown):
            module.reconstruct(changed, scope("codex", changed))
        window["baseline_offset"] = 1
        with self.assertRaises(Unknown):
            module.reconstruct(raw, window)

    def test_fiftieth_completion_boundary_is_checked(self):
        module = api(self, "turns")
        rows = [{"type":"session_meta", "payload":{"id":"oracle-native-session"}}]
        for n in range(51):
            rows.extend([{"type":"event_msg", "payload":{"type":"task_started", "turn_id":f"t{n}"}},
                         {"type":"event_msg", "payload":{"type":"task_complete", "turn_id":f"t{n}"}}])
        data = encode(rows)
        window = scope("codex", data)
        result = module.reconstruct(data, window)
        self.assertIn("native_sample_boundary_mismatch", result["reasons"])
        self.assertEqual(len(result["candidates"]), 50)
        window["fiftieth_offset"] = len(encode(rows[:101]))
        result = module.reconstruct(data, window)
        self.assertNotIn("native_sample_boundary_mismatch", result["reasons"])
        self.assertIn("native_no_publishable_units", result["reasons"])

    def test_claude_synthetic_chrome_api_error_and_missing_index_policies(self):
        module = api(self, "native")
        base = dict(type="assistant", sessionId="oracle-native-session", uuid="s1",
                    message={"id":"m", "model":"<synthetic>", "content":[{"type":"text", "text":"No response requested."}]})
        item = module.claude(base)["items"][0]
        self.assertEqual(item["native_key"], "s1")
        self.assertEqual(item["excluded"], "claude_synthetic_resume_chrome")
        error = {**base, "isApiErrorMessage": True}
        self.assertIsNone(module.claude(error)["items"][0]["excluded"])
        error_rows = [{"type":"user", "sessionId":"oracle-native-session", "uuid":"u", "message":{"content":"input"}},
                      error, {"type":"system", "subtype":"turn_duration", "sessionId":"oracle-native-session", "durationMs":1}]
        raw = encode(error_rows)
        with self.assertRaises(Unknown):
            api(self, "turns").reconstruct(raw, scope("claude", raw))
        ordinary = copy.deepcopy(base)
        ordinary["message"]["model"] = "model"
        with self.assertRaises(Unknown):
            module.claude(ordinary)
        ordinary["apiBlockIndex"] = 0
        self.assertIsNone(module.claude(ordinary)["items"][0]["excluded"])

    def test_claude_sidechain_rows_are_outside_the_supported_sample(self):
        raw, _ = self.fixture("claude")
        rows = [json.loads(x) for x in raw.splitlines()][:7]
        rows[1]["isSidechain"] = True
        data = encode(rows)
        with self.assertRaises(Unknown):
            api(self, "turns").reconstruct(data, scope("claude", data))

    def test_malformed_excluded_content_is_not_silently_accepted(self):
        module = api(self, "turns")
        raw, _ = self.fixture("claude")
        rows = [json.loads(x) for x in raw.splitlines()][:7]
        rows[5]["message"]["content"][0]["content"] = 42
        data = encode(rows)
        with self.assertRaises(Unknown):
            module.reconstruct(data, scope("claude", data))
        raw, _ = self.fixture("codex")
        rows = [json.loads(x) for x in raw.splitlines()][:8]
        rows.insert(2, {"type":"response_item", "payload":{"type":"reasoning", "id":"r", "summary":42}})
        data = encode(rows)
        with self.assertRaises(Unknown):
            module.reconstruct(data, scope("codex", data))


class SplitContracts(unittest.TestCase):
    def test_hand_defined_split_boundaries_utf16_and_fences(self):
        module = api(self, "policy")
        expand = lambda parts: "".join(text * count for text, count in parts)
        for case in json.loads((FIXTURES / "split.json").read_text()):
            with self.subTest(case=case["name"]):
                pieces = module.split(expand(case["text"]))
                self.assertEqual(pieces, [expand(p) for p in case["pieces"]])
                self.assertTrue(all(0 < len(p.encode("utf-16-le")) // 2 <= 2000 for p in pieces))

    def test_payload_trim_is_rust_white_space_and_hash_is_utf8(self):
        module = api(self, "policy")
        unit = dict(native_key="b", kind="body", payload="\x1c 본문 \x1c", excluded=None)
        value = module.plan(unit, {"channel_id":123, "provider":"codex"})
        self.assertEqual(value["pieces"][0]["payload_hash"], hashlib.sha256("\x1c 본문 \x1c".encode()).hexdigest())
        self.assertEqual(value["pieces"][0]["piece_index"], 0)


if __name__ == "__main__":
    unittest.main()
