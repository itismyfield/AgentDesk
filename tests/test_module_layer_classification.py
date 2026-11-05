"""Admission and mapping-only reduction contracts on committed synthetic trees."""
from copy import deepcopy
import csv
import io
import json
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from audit_maintainability.checks import module_layer_baseline as snapshots
from audit_maintainability.checks import module_layer_ratchet as ratchet
from audit_maintainability.checks.module_layers import analyze


class ClassificationAdmissionTest(unittest.TestCase):
    def setUp(self):
        self.temporary = TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name)
        self.git("init", "-q")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        self.put("Cargo.toml", '[package]\nname="fixture"\nversion="0.1.0"\nedition="2021"\n')
        self.put("src/lib.rs", "pub mod lower; pub mod upper; pub mod mixed; pub mod other;\n")
        self.put("src/lower.rs", "pub fn run() { crate::upper::hit(); crate::mixed::hit(); }\n")
        self.put("src/upper.rs", "pub fn hit() {}\n")
        self.put("src/mixed.rs", "pub fn hit() {}\n")
        self.put("src/other.rs", "pub fn hit() {}\n")
        self.put(snapshots.MAPPING, self.mapping())
        self.put(ratchet.MOVES, "schema_version=1\nmoves=[]\n")
        self.commit()
        self.anchor = self.git("rev-parse", "HEAD")
        self.path = self.repo / snapshots.BASELINE
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.accepted = self.measure()
        self.path.write_text(snapshots.encode_baseline(self.accepted))
        self.put(snapshots.MAPPING, self.mapping(admit=True))

    def put(self, relative, body):
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body)

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.repo), *args], text=True).strip()

    def commit(self):
        self.git("add", "Cargo.toml", "src", "scripts")
        self.git("commit", "-qm", "fixture")

    @staticmethod
    def mapping(admit=False, lower="foundation", upper="assembly"):
        rules = [("", "assembly"), ("lower", lower), ("upper", upper)]
        if admit:
            rules.append(("mixed", "adapter"))
        return ('schema_version=1\n[layers]\nfoundation=1\nadapter=2\nassembly=5\n'
                + "".join(f'[[rules]]\nprefix="{prefix}"\nlayer="{layer}"\nreason="fixture"\n'
                          for prefix, layer in rules))

    def measure(self):
        occurrences = []
        graph = analyze(self.repo, occurrence_projection=occurrences)
        return snapshots.baseline(graph, snapshots.load_mapping(self.repo),
                                  self.git("rev-parse", "HEAD"), occurrences)

    def cli(self, *extra, mode="--admit-classification", previous=None, ok=True):
        command = [sys.executable, str(ROOT / snapshots.GENERATOR.removeprefix("python3 ")),
                   "--repo", str(self.repo), "--input-sha", self.git("rev-parse", "HEAD"),
                   "--output", str(self.path), mode]
        if mode == "--admit-classification":
            command += ["--previous-mapping-rev", previous or self.anchor]
        result = subprocess.run([*command, *extra], capture_output=True, text=True)
        self.assertEqual(result.returncode == 0, ok, result.stdout + result.stderr)
        return result

    def refuse(self, *args, **kwargs):
        original = self.path.read_bytes()
        result = self.cli(*args, ok=False, **kwargs)
        self.assertEqual(self.path.read_bytes(), original)
        self.assertFalse(self.path.with_suffix(".json.lock").exists())
        return result

    def compare(self, accepted):
        return ratchet.compare(accepted, self.measure(), {"schema_version": 1, "moves": []})

    def test_exact_indeterminate_transition_keeps_existing_reverse_ceiling(self):
        old_reverse = deepcopy(self.accepted["reverse_dependencies"])
        old_indeterminate = {r["id"]: r for r in self.accepted["indeterminate_dependencies"]}
        candidate = self.measure()
        added = [r for r in candidate["reverse_dependencies"] if r not in old_reverse]
        self.assertEqual(len(added), 1)
        self.cli()
        updated = json.loads(self.path.read_bytes())
        self.assertTrue(all(row in updated["reverse_dependencies"] for row in old_reverse))
        self.assertEqual({r["id"] for r in updated["reverse_dependencies"]},
                         {r["id"] for r in old_reverse + added})
        for row in added:
            self.assertEqual(row, old_indeterminate[row["id"]])
            self.assertIn({**row, "section": "indeterminate_dependencies", "count": 0}, updated["zero_rules"])
        self.assertEqual(self.compare(updated)["errors"], [])

    def test_known_layer_change_is_refused(self):
        self.put(snapshots.MAPPING, self.mapping(admit=True, lower="adapter"))
        self.assertIn("known", self.refuse().stderr)

    def test_all_130_existing_reverse_records_and_316_occurrences_are_preserved(self):
        mapping = snapshots.load_mapping(self.repo)
        previous = deepcopy(mapping)
        previous["rules"] = [row for row in previous["rules"] if row["prefix"] != "mixed"]
        node = lambda name: "src/lib.rs::crate::" + name
        edges = [{"source": node(f"lower::leaf{i}"), "target": node("upper"),
                  "kind": "path", "sites": [f"src/lower.rs:{i + 1}"]} for i in range(130)]
        edges.append({"source": node("lower::leaf0"), "target": node("mixed"),
                      "kind": "path", "sites": ["src/lower.rs:200"]})
        graph = {"nodes": sorted({e[k] for e in edges for k in ("source", "target")}),
                 "edges": edges, "sccs": [], "coverage": {"complete": True}}
        occurrences = [{k: edge[k] for k in ("source", "target", "kind")} | {"count": count}
                       for edge, count in zip(edges, [3] * 56 + [2] * 74 + [1])]
        accepted = snapshots.baseline(graph, previous, self.anchor, occurrences)
        current = snapshots.baseline(graph, mapping, self.anchor, occurrences)
        self.assertEqual((len(accepted["reverse_dependencies"]),
                          sum(row["count"] for row in accepted["reverse_dependencies"])), (130, 316))
        admitted, _rows = snapshots._admit_classification(accepted, current,
            {"schema_version": 1, "moves": []}, graph, occurrences, mapping, previous)
        result = ratchet.reduced_baseline(admitted, current, {"schema_version": 1, "moves": []})
        self.assertEqual([row for row in result["reverse_dependencies"] if row["id"] in
                          {r["id"] for r in accepted["reverse_dependencies"]}], accepted["reverse_dependencies"])

    def test_layer_rank_change_is_refused(self):
        self.put(snapshots.MAPPING, self.mapping(admit=True).replace("foundation=1", "foundation=0"))
        self.assertIn("rank", self.refuse().stderr)

    def test_previous_mapping_hash_mismatch_is_refused(self):
        self.put(snapshots.MAPPING, self.mapping().replace('reason="fixture"', 'reason="other evidence"'))
        self.commit()
        wrong_anchor = self.git("rev-parse", "HEAD")
        self.put(snapshots.MAPPING, self.mapping(admit=True))
        self.assertIn("hash", self.refuse(previous=wrong_anchor).stderr)

    def test_source_anchor_mismatch_and_untracked_source_are_refused(self):
        self.put("src/mixed.rs", "pub fn hit() {} pub fn new() {}\n")
        self.assertIn("source tree", self.refuse().stderr)
        self.put("src/mixed.rs", "pub fn hit() {}\n")
        self.put("src/untracked.rs", "pub fn extra() {}\n")
        self.assertIn("untracked", self.refuse().stderr)

    def test_previous_projection_error_cannot_be_admitted(self):
        self.put("src/lower.rs", "pub fn run() { crate::upper::hit(); crate::upper::hit(); crate::mixed::hit(); }\n")
        self.commit()
        self.assertIn("previous projection", self.refuse().stderr)

    def test_failures_preserve_crlf_baseline_bytes_and_report(self):
        self.path.write_bytes(self.path.read_bytes().replace(b"\n", b"\r\n"))
        report = self.repo / "report.tsv"
        report.write_text("previous evidence")
        self.put(snapshots.MAPPING, self.mapping(admit=True, upper="adapter"))
        self.refuse("--report", str(report))
        self.assertEqual(report.read_text(), "previous evidence")

    def test_admitted_edge_growth_is_an_increase_error(self):
        self.cli()
        updated = json.loads(self.path.read_bytes())
        self.put("src/lower.rs", "pub fn run() { crate::upper::hit(); crate::mixed::hit(); crate::mixed::hit(); }\n")
        errors = self.compare(updated)["errors"]
        self.assertTrue(any(r["code"] == "increase" and r["section"] == "reverse_dependencies" for r in errors))

    def test_reverting_mapping_cannot_hide_admitted_debt(self):
        self.cli()
        updated = json.loads(self.path.read_bytes())
        self.put(snapshots.MAPPING, self.mapping())
        self.assertTrue(self.compare(updated)["errors"])

    def test_other_warning_growth_is_not_absorbed(self):
        self.put("src/lower.rs", "pub fn run() { crate::upper::hit(); crate::mixed::hit(); crate::other::hit(); unknown!(); }\n")
        self.put("src/new_unknown.rs", "pub fn hit() {}\n")
        self.put("src/new_unknown2.rs", "pub fn hit() {}\n")
        self.put("src/lib.rs", (self.repo / "src/lib.rs").read_text() + "pub mod new_unknown; pub mod new_unknown2;\n")
        self.commit()
        self.cli()
        updated = json.loads(self.path.read_bytes())
        self.assertEqual(updated["unclassified_modules"], self.accepted["unclassified_modules"])
        self.assertLess(updated["unclassified_modules"], self.measure()["unclassified_modules"])
        self.assertFalse(any(r["target"].endswith("::other") for r in updated["indeterminate_dependencies"]))
        self.assertEqual(updated["uncertainties"], self.accepted["uncertainties"])

    def test_same_input_replay_is_refused(self):
        self.cli()
        self.assertIn("hash", self.refuse().stderr)

    def test_mapping_only_reduction_requires_matching_reclassify_prefix(self):
        self.put(snapshots.MAPPING, self.mapping(lower="assembly"))
        for prefix in (None, "low", "other"):
            args = [] if prefix is None else ["--reclassify", prefix]
            self.assertIn("reclassify", self.refuse(*args, mode="--update-reductions").stderr)
        self.cli("--reclassify", "lower", mode="--update-reductions")
        updated = json.loads(self.path.read_bytes())
        self.assertEqual(updated["reverse_dependencies"], [])
        self.assertIn("--reclassify lower", updated["generation_command"])

    def test_actual_edge_deletion_and_occurrence_decrease_are_allowed(self):
        for source in ("pub fn run() { crate::mixed::hit(); }\n",
                       "pub fn run() { crate::upper::hit(); crate::mixed::hit(); }\n"):
            with self.subTest(source=source):
                self.put(snapshots.MAPPING, self.mapping())
                self.put("src/lower.rs", "pub fn run() { crate::upper::hit(); crate::upper::hit(); crate::mixed::hit(); }\n")
                self.commit()
                self.path.write_text(snapshots.encode_baseline(self.measure()))
                self.put("src/lower.rs", source)
                self.commit()
                self.cli(mode="--update-reductions")
                self.assertLess(sum(r["count"] for r in json.loads(self.path.read_bytes())["reverse_dependencies"]), 2)

    def test_existing_reverse_growth_and_zero_rule_revival_are_refused(self):
        for zero in (False, True):
            with self.subTest(zero=zero):
                accepted = deepcopy(self.accepted)
                row = accepted["reverse_dependencies"][0]
                if zero:
                    accepted["reverse_dependencies"] = []
                    accepted["zero_rules"].append({**row, "section": "reverse_dependencies", "count": 0})
                else:
                    self.put("src/lower.rs", "pub fn run() { crate::upper::hit(); crate::upper::hit(); crate::mixed::hit(); }\n")
                    self.commit()
                self.path.write_text(snapshots.encode_baseline(accepted))
                self.refuse()

    def test_report_records_absent_accepted_count_and_growth(self):
        self.put("src/other.rs", "pub fn hit() { crate::mixed::hit(); crate::mixed::hit(); }\n")
        mapping = self.mapping(admit=True) + '[[rules]]\nprefix="other"\nlayer="foundation"\nreason="fixture"\n'
        self.put(snapshots.MAPPING, mapping)
        self.commit()
        report = self.repo / "report.tsv"
        self.cli("--report", str(report))
        rows = list(csv.DictReader(io.StringIO(report.read_text()), delimiter="\t"))
        added = next(row for row in rows if row["source"].endswith("::other"))
        self.assertEqual(int(added["accepted_indeterminate_count"]), 0)
        self.assertEqual(int(added["grown_since_accepted"]), 2)
        self.assertEqual(sum(int(r["accepted_indeterminate_count"]) for r in rows), 1)
        self.assertEqual(sum(int(r["grown_since_accepted"]) for r in rows), 2)

    def test_dry_run_is_reviewable_without_changing_baseline(self):
        original = self.path.read_bytes()
        report = self.repo / "report.tsv"
        result = self.cli("--dry-run", "--report", str(report))
        self.assertEqual(self.path.read_bytes(), original)
        projected = json.loads(result.stdout)
        self.assertEqual(len(projected["reverse_dependencies"]), 2)
        self.cli()
        self.assertEqual(result.stdout.encode(), self.path.read_bytes())

    def test_empty_admission_matches_ordinary_reduction_bytes(self):
        self.put(snapshots.MAPPING, self.mapping() + '[[rules]]\nprefix="other"\nlayer="adapter"\nreason="fixture"\n')
        admitted = self.cli("--dry-run").stdout
        reduced = self.cli("--dry-run", mode="--update-reductions").stdout
        self.assertEqual(admitted, reduced)

    def test_movement_conflict_and_report_over_baseline_are_refused(self):
        candidate = self.measure()
        added = next(r for r in candidate["reverse_dependencies"] if r not in self.accepted["reverse_dependencies"])
        old = self.accepted["reverse_dependencies"][0]
        self.put(ratchet.MOVES, ('schema_version=1\n[[moves]]\nsection="reverse_dependencies"\n'
                                f'old_id="{old["id"]}"\nnew_id="{added["id"]}"\n'))
        self.refuse()
        self.put(ratchet.MOVES, "schema_version=1\nmoves=[]\n")
        self.refuse("--report", str(self.path))

    def test_report_hardlink_cannot_truncate_accepted_bytes(self):
        report = self.repo / "alias.tsv"
        report.hardlink_to(self.path)
        self.refuse("--report", str(report))
        self.assertEqual(report.read_bytes(), self.path.read_bytes())


if __name__ == "__main__":
    unittest.main()
