import copy
import hashlib
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("census", ROOT / "scripts/check_tui_o_writer_census.py")
CENSUS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CENSUS)


class BodyProbeManifestTests(unittest.TestCase):
    def snapshot(self, data):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "scripts").mkdir()
            raw = json.dumps(data).encode()
            (root / "scripts/tui_o_body_probes.json").write_bytes(raw)
            result = CENSUS.body_probe_snapshot(root)
            self.assertEqual(result["census_hash"], hashlib.sha256(raw).hexdigest())
            return result

    def test_pending_is_computed_and_uses_the_authoritative_file(self):
        data = json.loads((ROOT / "scripts/tui_o_body_probes.json").read_text())
        self.assertFalse(self.snapshot(data)["coverage"])
        self.assertEqual(self.snapshot(data)["pending"], 0)
        self.assertTrue(self.snapshot(data)["instrumentation_complete"])
        complete = copy.deepcopy(data)
        for row in complete["probes"]:
            row["status"] = "VERIFIED"
        self.assertFalse(self.snapshot(complete)["coverage"])
        self.assertTrue(self.snapshot(complete)["instrumentation_complete"])
        complete["probes"][0]["status"] = "PENDING_0b1"
        self.assertFalse(self.snapshot(complete)["coverage"])
        self.assertFalse(self.snapshot(complete)["instrumentation_complete"])

    def test_empty_missing_duplicate_and_unknown_states_fail_closed(self):
        self.assertFalse(self.snapshot({"schema_version": 1, "probes": []})["coverage"])
        data = json.loads((ROOT / "scripts/tui_o_body_probes.json").read_text())
        for row in data["probes"]:
            row["status"] = "VERIFIED"
        for mutate in [
            lambda value: value.update(schema_version=True),
            lambda value: value["probes"].pop(),
            lambda value: value["probes"].append(value["probes"][0]),
            lambda value: value["probes"][0].update(status="READY_MAYBE"),
        ]:
            invalid = copy.deepcopy(data)
            mutate(invalid)
            state = self.snapshot(invalid)
            self.assertFalse(state["coverage"])
            self.assertTrue(state["error"])

        for index in range(3):
            invalid = copy.deepcopy(data)
            invalid["claim_probes"][index]["unexpected"] = "unreviewed"
            state = self.snapshot(invalid)
            self.assertFalse(state["coverage"], index)
            self.assertTrue(state["error"], index)

    def test_rust_expected_probe_rows_matches_the_census_total(self):
        import re
        source = (ROOT / "src/services/legacy_body_census/manifest.rs").read_text()
        expected = int(re.search(r"EXPECTED_PROBE_ROWS: usize = ([0-9]+);", source)[1])
        census_total = sum(count for path, census in CENSUS.CENSUS.items()
                           if census[1] == "CUT_D" or census[1].startswith("COV:")
                           or path in ("footer_view_reconciler/mod.rs", "turn_bridge/headless_delivery.rs")
                           for count in CENSUS.EXPECTED_PRIMITIVES[path].values())
        self.assertEqual(expected, census_total)

    def test_w02_fallback_post_is_body_and_only_two_seeds_are_nonbody(self):
        data = json.loads((ROOT / "scripts/tui_o_body_probes.json").read_text())
        prefix = "tmux_watcher/streaming_status_tick.rs:update_streaming_status_tick:"
        rows = {row["id"].removeprefix(prefix): row for row in data["probes"]
                if row["id"].startswith(prefix)}
        self.assertEqual(rows["send_channel_message*:3"]["classification"], "BODY_CAPABLE")
        self.assertEqual(rows["edit_channel_message*:3"]["classification"], "BODY_CAPABLE")
        for probe in ("send_channel_message*:1", "send_channel_message*:2"):
            self.assertEqual(rows[probe]["classification"], "NONBODY")


if __name__ == "__main__":
    unittest.main()
