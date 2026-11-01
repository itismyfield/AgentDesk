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
        complete = copy.deepcopy(data)
        for row in complete["probes"]:
            row["status"] = "VERIFIED"
        self.assertTrue(self.snapshot(complete)["coverage"])
        complete["probes"][0]["status"] = "PENDING_0b1"
        self.assertFalse(self.snapshot(complete)["coverage"])

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

    def test_w02_fallback_post_is_body_and_only_two_seeds_are_nonbody(self):
        data = json.loads((ROOT / "scripts/tui_o_body_probes.json").read_text())
        rows = {row["line"]: row for row in data["probes"]
                if row["path"].endswith("/tmux_watcher/streaming_status_tick.rs")}
        self.assertEqual(rows[925]["classification"], "BODY_CAPABLE")
        self.assertEqual(rows[922]["classification"], "BODY_CAPABLE")
        for line in (457, 775):
            self.assertEqual(rows[line]["classification"], "NONBODY")


if __name__ == "__main__":
    unittest.main()
