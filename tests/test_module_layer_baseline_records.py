"""Synthetic coverage for comparison records in the module-layer baseline."""
from __future__ import annotations

from copy import deepcopy
import json
from pathlib import Path
import re
import sys
import unittest


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from audit_maintainability.checks import module_layer_baseline as snapshots


class IndeterminateBaselineRecordsTest(unittest.TestCase):
    def test_readme_activation_command_matches_canonical_input_sha(self):
        canonical = json.loads((ROOT / snapshots.BASELINE).read_text("utf-8"))
        readme = (ROOT / snapshots.BASELINE).with_name("README.md").read_text("utf-8")
        match = re.search(r"--input-sha ([^\s`]+)", readme)
        self.assertIsNotNone(match)
        self.assertEqual(match.group(1), canonical["input_sha"])

    @staticmethod
    def _node(name):
        return "src/lib.rs::crate::" + name

    def _fixture(self, unknown="mixed_a"):
        lower, upper, mixed_a, mixed_b = map(self._node, ("lower", "upper", "mixed_a", "mixed_b"))
        edges = [
            {"source": lower, "target": upper, "kind": "path", "sites": ["src/lower.rs:3"]},
            {"source": lower, "target": self._node(unknown), "kind": "import", "sites": ["src/lower.rs:4"]},
        ]
        graph = {
            "nodes": [lower, upper, mixed_a, mixed_b], "edges": edges, "sccs": [],
            "coverage": {
                "complete": False, "cfg_scope": "all-non-test-cfg-union", "target_os": "all",
                "unresolved_references": [{"source": lower, "path": "unknown::run", "site": "src/lower.rs:8",
                                           "reason": "unknown path root"}],
                "cfg_uncertainties": [], "read_failures": [], "module_errors": [],
            },
        }
        mapping = {"schema_version": 1, "layers": {"foundation": 1, "assembly": 5},
                   "rules": [{"prefix": "lower", "layer": "foundation"},
                             {"prefix": "upper", "layer": "assembly"}]}
        occurrences = [{key: edge[key] for key in ("source", "target", "kind")} | {"count": count}
                       for edge, count in zip(edges, (2, 3))]
        return graph, mapping, occurrences

    def _snapshot(self, fixture):
        return snapshots.baseline(*fixture[:2], "a" * 40, fixture[2])

    def test_retargeting_an_indeterminate_edge_changes_its_record_without_changing_totals(self):
        before = self._snapshot(self._fixture("mixed_a"))
        after = self._snapshot(self._fixture("mixed_b"))
        self.assertEqual(before["indeterminate_edges"], after["indeterminate_edges"])
        self.assertEqual(before["indeterminate_occurrences"], after["indeterminate_occurrences"])
        self.assertNotEqual(before, after)
        old, new = (payload["indeterminate_dependencies"][0] for payload in (before, after))
        self.assertNotEqual(old["id"], new["id"])
        self.assertEqual(old["count"], new["count"])

    def test_indeterminate_records_keep_identity_and_occurrence_count_without_diagnostics(self):
        fixture = self._fixture()
        payload = self._snapshot(fixture)
        projected = snapshots.project(*fixture)
        self.assertEqual(payload["schema_version"], 3)
        self.assertEqual(payload["zero_rules"], [])
        self.assertEqual(payload["count_unit_version"], 1)
        records = payload["indeterminate_dependencies"]
        self.assertEqual(records, [{key: row[key] for key in ("id", "source", "target", "kind", "count")}
                                   for row in projected["indeterminate_edges"]])
        self.assertEqual(sum(row["count"] for row in records), payload["indeterminate_occurrences"])
        self.assertTrue(all(set(row) == {"id", "source", "target", "kind", "count"} for row in records))
        for section in ("reverse_dependencies", "uncertainties"):
            self.assertEqual({row["id"]: row["count"] for row in payload[section]},
                             {row["id"]: row["count"] for row in projected[section]})

    def test_record_bytes_are_deterministic_and_ignore_evidence_locations(self):
        fixture = self._fixture()
        moved = deepcopy(fixture)
        moved[0]["nodes"].reverse()
        moved[0]["edges"].reverse()
        moved[1]["rules"].reverse()
        moved[2].reverse()
        for edge in moved[0]["edges"]:
            edge["sites"] = ["/tmp/another-worktree/" + site.rsplit(":", 1)[0] + ":99"
                             for site in edge["sites"]]
        moved[0]["coverage"]["unresolved_references"][0]["site"] = "/tmp/another-worktree/src/lower.rs:99"
        before = snapshots.encode_baseline(self._snapshot(fixture)).encode("utf-8")
        after = snapshots.encode_baseline(self._snapshot(moved)).encode("utf-8")
        self.assertEqual(before, after)

    def test_each_indeterminate_record_occupies_one_json_line(self):
        fixture = self._fixture()
        graph, _mapping, occurrences = fixture
        duplicate = dict(graph["edges"][1], target=self._node("mixed_b"))
        graph["edges"].append(duplicate)
        occurrences.append({key: duplicate[key] for key in ("source", "target", "kind")} | {"count": 1})
        payload = self._snapshot(fixture)
        lines = snapshots.encode_baseline(payload).splitlines()
        self.assertEqual(json.loads("\n".join(lines)), payload)
        for row in payload["indeterminate_dependencies"]:
            record_lines = [line for line in lines if row["id"] in line]
            self.assertEqual(len(record_lines), 1)
            self.assertEqual(record_lines[0].count('"id"'), 1)


if __name__ == "__main__":
    unittest.main()
