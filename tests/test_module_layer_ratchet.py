"""Synthetic contracts for blocking module debt comparisons and reduction updates."""
from __future__ import annotations

from copy import deepcopy
import importlib
import json
import os
import shutil
from pathlib import Path
import sys
import subprocess
from tempfile import TemporaryDirectory
import unittest

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT / "scripts") not in sys.path:
    sys.path.insert(0, str(ROOT / "scripts"))

from audit_maintainability.checks.module_layer_baseline import _id


class ModuleLayerRatchetTest(unittest.TestCase):
    def setUp(self):
        try:
            self.ratchet = importlib.import_module("audit_maintainability.checks.module_layer_ratchet")
        except ModuleNotFoundError as exc:
            if exc.name != "audit_maintainability.checks.module_layer_ratchet":
                raise
            self.fail("blocking module comparator is not implemented")
        self.assertTrue(callable(getattr(self.ratchet, "compare", None)), "blocking comparison is not implemented")

    @staticmethod
    def edge(source="lower", target="upper", count=2, kind="path"):
        source = "src/lib.rs::crate::" + source
        target = "src/lib.rs::crate::" + target
        return {"id": _id("edge", source, target, kind), "source": source,
                "target": target, "kind": kind, "count": count}

    @staticmethod
    def uncertainty(label="unresolved", count=2, source=None):
        row = {"id": _id("uncertainty", label), "count": count}
        return row if source is None else {**row, "source": "src/lib.rs::crate::" + source}

    @staticmethod
    def moves(*records):
        return {"schema_version": 1, "moves": list(records)}

    @staticmethod
    def snapshot(reverse=(), indeterminate=(), uncertainties=(), unclassified=1):
        return {
            "schema_version": 3, "count_unit_version": 1,
            "input_sha": "1" * 40, "source_grain": "module",
            "edge_count_unit": "static reference occurrences",
            "generation_command": "synthetic fixture", "mapping_sha256": "2" * 64,
            "reverse_dependencies": deepcopy(list(reverse)),
            "indeterminate_dependencies": deepcopy(list(indeterminate)),
            "uncertainties": deepcopy(list(uncertainties)),
            "unclassified_modules": unclassified,
            "indeterminate_edges": len(indeterminate),
            "indeterminate_occurrences": sum(row["count"] for row in indeterminate),
            "configuration_errors": [], "zero_rules": [],
        }

    def compare(self, accepted, candidate, moves=None):
        return self.ratchet.compare(accepted, candidate, self.moves() if moves is None else moves)

    def assert_warning_only(self, result, section=None, record_id=None):
        self.assertEqual(result["errors"], [], result)
        self.assertTrue(any(section is None or (row.get("section") == section
                                                and (record_id is None or row.get("id") == record_id))
                            for row in result["warnings"]), result)

    def assert_error(self, result, section=None, record_id=None):
        self.assertTrue(result["errors"], result)
        if section is not None:
            self.assertTrue(any(row.get("section") == section
                                and (record_id is None or row.get("id") == record_id)
                                for row in result["errors"]), result)

    def test_existing_debt_is_informational_without_mutating_inputs(self):
        accepted = self.snapshot([self.edge()], [self.edge("mixed", "upper")], [self.uncertainty()])
        candidate = deepcopy(accepted)
        candidate["input_sha"] = "3" * 40
        originals = deepcopy((accepted, candidate))
        result = self.compare(accepted, candidate)
        self.assertEqual(result["errors"], [])
        self.assertEqual(result["decreases"], [])
        self.assertEqual((accepted, candidate), originals)

    def test_new_reverse_identifier_is_error(self):
        added = self.edge("lower", "other")
        result = self.compare(self.snapshot(), self.snapshot([added]))
        self.assert_error(result, "reverse_dependencies", added["id"])

    def test_existing_reference_occurrence_increase_is_error(self):
        original = self.edge(count=2)
        grown = dict(original, count=3)
        self.assert_error(self.compare(self.snapshot([original]), self.snapshot([grown])),
                          "reverse_dependencies", original["id"])

    def test_decrease_does_not_offset_another_identifiers_increase(self):
        first, second = self.edge("a", count=4), self.edge("b", count=1)
        accepted = self.snapshot([first, second])
        candidate = self.snapshot([dict(first, count=1), dict(second, count=2)])
        result = self.compare(accepted, candidate)
        self.assert_error(result, "reverse_dependencies", second["id"])
        self.assertTrue(result["decreases"])

    def test_decrease_does_not_offset_a_new_identifier(self):
        old, added = self.edge("old", count=10), self.edge("new", count=1)
        self.assert_error(self.compare(self.snapshot([old]), self.snapshot([added])),
                          "reverse_dependencies", added["id"])

    def test_known_debt_becoming_indeterminate_is_error(self):
        original, resolved = self.edge(), self.edge("previously_unknown")
        accepted = self.snapshot([original], indeterminate=[resolved])
        for moved in (False, True):
            with self.subTest(mapped_move=moved):
                indeterminate = self.edge("moved") if moved else original
                candidate = self.snapshot(indeterminate=[indeterminate])
                moves = self.moves({"section": "reverse_dependencies", "old_id": original["id"],
                                    "new_id": indeterminate["id"]}) if moved else self.moves()
                self.assertEqual(accepted["indeterminate_edges"], candidate["indeterminate_edges"])
                self.assertEqual(accepted["indeterminate_occurrences"], candidate["indeterminate_occurrences"])
                result = self.compare(accepted, candidate, moves)
                self.assert_error(result, "indeterminate_dependencies", indeterminate["id"])
                self.assertFalse(any(row.get("section") == "reverse_dependencies" and row.get("id") == original["id"]
                                     for row in result["decreases"]), result)

    def test_reverse_debt_turning_into_an_unresolved_glob_reference_is_error(self):
        reverse = self.edge(count=1)
        accepted = self.snapshot([reverse])
        hidden = self.snapshot(uncertainties=[self.uncertainty("glob", 1, source="lower")])
        self.assert_error(self.compare(accepted, hidden), "uncertainties")
        unrelated = self.snapshot(uncertainties=[self.uncertainty("glob", 1, source="other")])
        result = self.compare(accepted, unrelated)
        self.assert_warning_only(result, "uncertainties")
        self.assertTrue(any(row.get("id") == reverse["id"] for row in result["decreases"]), result)

    def test_reverse_debt_changing_reference_kind_into_indeterminate_is_error(self):
        for accepted_kind, current_kind in (("import", "path"), ("path", "import")):
            with self.subTest(accepted=accepted_kind, current=current_kind):
                reverse = self.edge(kind=accepted_kind)
                kept = self.snapshot([reverse], indeterminate=[self.edge(kind=current_kind, count=1)])
                self.assert_error(self.compare(self.snapshot([reverse]), kept), "indeterminate_dependencies")
                moved = self.snapshot(indeterminate=[self.edge(kind=current_kind)])
                self.assert_error(self.compare(self.snapshot([reverse]), moved), "indeterminate_dependencies")

    def test_moved_reverse_debt_reappearing_under_another_kind_is_error(self):
        for accepted_kind, current_kind in (("import", "path"), ("path", "import")):
            with self.subTest(accepted=accepted_kind, current=current_kind):
                original, moved = self.edge(kind=accepted_kind), self.edge("moved", kind=accepted_kind)
                moves = self.moves({"section": "reverse_dependencies", "old_id": original["id"], "new_id": moved["id"]})
                candidate = self.snapshot(indeterminate=[self.edge("moved", kind=current_kind)])
                self.assert_error(self.compare(self.snapshot([original]), candidate, moves), "indeterminate_dependencies")

    def test_measured_sources_reveal_a_glob_from_a_moved_reverse_source(self):
        original, moved = self.edge(), self.edge("moved")
        moves = self.moves({"section": "reverse_dependencies", "old_id": original["id"], "new_id": moved["id"]})
        glob = self.uncertainty("glob", 1)
        candidate = self.snapshot(uncertainties=[glob])
        self.assert_warning_only(self.compare(self.snapshot([original]), candidate, moves), "uncertainties")
        self.assert_error(self.ratchet.compare(self.snapshot([original]), candidate, moves,
                                               {glob["id"]: {"source": "src/lib.rs::crate::moved"}}), "uncertainties")

    def test_glob_naming_an_accepted_reverse_target_is_error_but_an_unrelated_macro_warns(self):
        reverse = self.edge(count=1)
        accepted = self.snapshot([reverse])
        glob, macro = self.uncertainty("glob", 1), self.uncertainty("macro", 1)
        with_glob, with_macro = (self.snapshot([reverse], uncertainties=[row]) for row in (glob, macro))
        evidence = {glob["id"]: {"source": "src/lib.rs::crate::lower", "path": "crate::upper::*"},
                    macro["id"]: {"source": "src/lib.rs::crate::lower", "path": "matches"}}
        self.assert_error(self.ratchet.compare(accepted, with_glob, self.moves(), evidence), "uncertainties", glob["id"])
        result = self.ratchet.compare(accepted, with_macro, self.moves(), evidence)
        self.assert_warning_only(result, "uncertainties", macro["id"])
        relative = {glob["id"]: {"source": "src/lib.rs::crate::lower", "path": "super::upper::*"}}
        self.assert_error(self.ratchet.compare(accepted, with_glob, self.moves(), relative), "uncertainties", glob["id"])
        child = {glob["id"]: {"source": "src/lib.rs::crate::lower", "path": "crate::upper::utils::*"}}
        self.assert_warning_only(self.ratchet.compare(accepted, with_glob, self.moves(), child), "uncertainties", glob["id"])
        kept = self.uncertainty("kept", 1)
        beside = {kept["id"]: {"source": "src/lib.rs::crate::lower", "path": "crate::upper::*"}}
        dropped_named = self.snapshot(uncertainties=[kept])
        self.assert_error(self.ratchet.compare(self.snapshot([reverse], uncertainties=[kept]), dropped_named,
                                               self.moves(), beside), "reverse_dependencies")
        item = {glob["id"]: {"source": "src/lib.rs::crate::lower", "path": "crate::upper::hit"}}
        self.assert_error(self.ratchet.compare(accepted, with_glob, self.moves(), item), "uncertainties", glob["id"])

    def test_changed_indeterminate_target_warns_even_with_equal_totals(self):
        old, replacement = self.edge("mixed", "old"), self.edge("mixed", "new")
        accepted, candidate = self.snapshot(indeterminate=[old]), self.snapshot(indeterminate=[replacement])
        self.assertEqual(accepted["indeterminate_edges"], candidate["indeterminate_edges"])
        self.assertEqual(accepted["indeterminate_occurrences"], candidate["indeterminate_occurrences"])
        self.assert_warning_only(self.compare(accepted, candidate), "indeterminate_dependencies", replacement["id"])

    def test_existing_indeterminate_occurrence_increase_warns(self):
        old = self.edge("mixed", count=2)
        self.assert_warning_only(self.compare(self.snapshot(indeterminate=[old]),
                                       self.snapshot(indeterminate=[dict(old, count=3)])),
                          "indeterminate_dependencies", old["id"])

    def test_unclassified_count_increase_warns(self):
        self.assert_warning_only(self.compare(self.snapshot(unclassified=1), self.snapshot(unclassified=2)),
                                 "unclassified_modules")

    def test_indeterminate_aggregate_count_without_records_is_invalid(self):
        for name in ("indeterminate_edges", "indeterminate_occurrences"):
            with self.subTest(field=name):
                candidate = self.snapshot()
                candidate[name] += 1
                self.assert_error(self.compare(self.snapshot(), candidate), "configuration")

    def test_uncertainty_new_identifiers_and_occurrence_increases_warn(self):
        old, added = self.uncertainty("old"), self.uncertainty("new", 1)
        for candidate, identifier in (([dict(old, count=3)], old["id"]), ([old, added], added["id"])):
            with self.subTest(identifier=identifier):
                self.assert_warning_only(self.compare(self.snapshot(uncertainties=[old]),
                                                      self.snapshot(uncertainties=candidate)),
                                         "uncertainties", identifier)

    def test_explicit_old_to_new_mapping_keeps_the_same_debt(self):
        old, moved = self.edge("before"), self.edge("after")
        moves = self.moves({"section": "reverse_dependencies", "old_id": old["id"], "new_id": moved["id"]})
        result = self.compare(self.snapshot([old]), self.snapshot([moved]), moves)
        self.assertEqual(result["errors"], [])
        self.assertEqual(result["decreases"], [])

    def test_move_outside_mapping_is_a_new_error(self):
        old, moved, unlisted = self.edge("before"), self.edge("after"), self.edge("unlisted")
        moves = self.moves({"section": "reverse_dependencies", "old_id": old["id"], "new_id": moved["id"]})
        self.assert_error(self.compare(self.snapshot([old]), self.snapshot([unlisted]), moves),
                          "reverse_dependencies", unlisted["id"])

    def test_mapping_cannot_name_an_identifier_missing_from_the_baseline(self):
        absent, current = self.edge("absent"), self.edge("current")
        moves = self.moves({"section": "reverse_dependencies", "old_id": absent["id"], "new_id": current["id"]})
        for candidate in (self.snapshot(), self.snapshot([current])):
            with self.subTest(candidate=candidate):
                self.assert_error(self.compare(self.snapshot(), candidate, moves))

    def test_mapping_must_be_one_to_one_and_unambiguous(self):
        old_a, old_b, moved_a, moved_b = (self.edge(name) for name in ("a", "b", "moved_a", "moved_b"))
        def move(old, new):
            return {"section": "reverse_dependencies", "old_id": old["id"], "new_id": new["id"]}
        cases = (
            ([move(old_a, moved_a), move(old_a, moved_b)], [moved_a, moved_b]),
            ([move(old_a, moved_a), move(old_b, moved_a)], [moved_a]),
            ([move(old_a, moved_a)], [old_a, moved_a]),
            ([dict(move(old_a, moved_a), new_id="not-an-id")], [moved_a]),
        )
        for entries, current in cases:
            with self.subTest(entries=entries):
                self.assert_error(self.compare(self.snapshot([old_a, old_b]), self.snapshot(current), self.moves(*entries)))

    def test_mapping_cannot_target_another_sections_positive_identifier(self):
        reverse, indeterminate = self.edge("lower", count=1), self.edge("mixed", count=1)
        accepted = self.snapshot([reverse], [indeterminate])
        candidate = self.snapshot(indeterminate=[indeterminate])
        moves = self.moves({"section": "reverse_dependencies", "old_id": reverse["id"],
                            "new_id": indeterminate["id"]})
        self.assert_error(self.compare(accepted, candidate, moves))

    def test_mapping_destinations_must_be_unique_across_sections(self):
        reverse, indeterminate, combined = (self.edge(name, count=1) for name in ("lower", "mixed", "combined"))
        accepted = self.snapshot([reverse], [indeterminate])
        candidate = self.snapshot(indeterminate=[combined])
        moves = self.moves({"section": "reverse_dependencies", "old_id": reverse["id"], "new_id": combined["id"]},
                           {"section": "indeterminate_dependencies", "old_id": indeterminate["id"],
                            "new_id": combined["id"]})
        self.assert_error(self.compare(accepted, candidate, moves))

    def test_mapping_cannot_target_a_zero_rule_in_another_section(self):
        resolved, indeterminate = self.edge("resolved", count=1), self.edge("mixed", count=1)
        accepted = self.snapshot(indeterminate=[indeterminate])
        accepted["zero_rules"] = [{**resolved, "section": "reverse_dependencies", "count": 0}]
        candidate = self.snapshot(indeterminate=[resolved])
        moves = self.moves({"section": "indeterminate_dependencies", "old_id": indeterminate["id"],
                            "new_id": resolved["id"]})
        self.assert_error(self.compare(accepted, candidate, moves))

    def test_configuration_errors_and_unsupported_schemas_cannot_pass(self):
        for target in ("accepted", "candidate"):
            for modification in ({"configuration_errors": ["broken mapping"]}, {"schema_version": 2},
                                 {"count_unit_version": 99}, {"source_grain": "item"}):
                with self.subTest(target=target, modification=modification):
                    accepted, candidate = self.snapshot(), self.snapshot()
                    (accepted if target == "accepted" else candidate).update(modification)
                    self.assert_error(self.compare(accepted, candidate))

    def test_duplicate_or_invalid_record_counts_cannot_pass(self):
        original = self.edge()
        for records in ([original, original], [dict(original, count=-1)], [dict(original, count=0)],
                        [dict(original, count=True)]):
            with self.subTest(records=records):
                self.assert_error(self.compare(self.snapshot([original]), self.snapshot(records)))

    def test_comparison_is_order_independent(self):
        first, second = self.edge("a", count=3), self.edge("b", count=2)
        known, unknown = self.uncertainty("a"), self.uncertainty("b")
        accepted = self.snapshot([first, second], uncertainties=[known, unknown])
        candidate = self.snapshot([dict(first, count=1), self.edge("new", count=1)],
                                  uncertainties=[known, dict(unknown, count=3)])
        reordered, reordered_candidate = deepcopy(accepted), deepcopy(candidate)
        for section in ("reverse_dependencies", "uncertainties"):
            reordered[section].reverse()
            reordered_candidate[section].reverse()
        left = json.dumps(self.compare(accepted, candidate), sort_keys=True, ensure_ascii=False).encode()
        right = json.dumps(self.compare(reordered, reordered_candidate), sort_keys=True, ensure_ascii=False).encode()
        self.assertEqual(left, right)


class ModuleLayerRunnerTest(unittest.TestCase):
    def test_cross_section_move_approval_blocks_the_audit_runner(self):
        with TemporaryDirectory() as temporary:
            repo = Path(temporary) / "repo"
            scripts = repo / "scripts"
            scripts.mkdir(parents=True)
            for path in (ROOT / "scripts").glob("*.py"):
                shutil.copy2(path, scripts / path.name)
            shutil.copytree(ROOT / "scripts/audit_maintainability", scripts / "audit_maintainability",
                            ignore=shutil.ignore_patterns("__pycache__", "baselines"))
            (scripts / "ci").mkdir()
            for filename in ("rust_test_sources.py", "merge_candidate.py"):
                shutil.copy2(ROOT / "scripts/ci" / filename, scripts / "ci" / filename)
            def git(*arguments):
                return subprocess.run(["git", "-C", str(repo), *arguments], capture_output=True,
                                      text=True, check=True).stdout.strip()
            def put(relative, text):
                path = repo / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text)
            git("init", "-q")
            git("config", "user.name", "Fixture")
            git("config", "user.email", "fixture@example.invalid")
            put("Cargo.toml", '[package]\nname="fixture"\nversion="0.1.0"\nedition="2024"\n')
            put("src/lib.rs", "mod lower; mod mixed; mod upper;\n")
            put("src/lower.rs", "pub fn retained() { crate::upper::hit(); }\n")
            put("src/mixed.rs", "pub fn removed() { crate::upper::hit(); }\n")
            put("src/upper.rs", "pub fn hit() {}\n")
            put("scripts/audit_maintainability/layers.toml", """schema_version = 1
rules = [{prefix="",layer="assembly",reason="fixture assembly"},
         {prefix="lower",layer="foundation",reason="fixture lower layer"},
         {prefix="upper",layer="assembly",reason="fixture upper layer"}]
unclassified = [{prefix="mixed",reason="fixture unclassified responsibility"}]
[layers]
foundation = 0
assembly = 1
""")
            git("add", "src", "scripts", "Cargo.toml")
            git("commit", "-qm", "fixture before move")
            snapshot = Path(temporary) / "snapshot.json"
            generator = [sys.executable, str(scripts / "audit_maintainability/checks/module_layer_baseline.py"),
                         "--repo", str(repo), "--input-sha", "HEAD", "--output", str(snapshot)]
            created = subprocess.run(generator, cwd=repo, capture_output=True, text=True)
            self.assertEqual(created.returncode, 0, created.stderr)
            accepted = json.loads(snapshot.read_bytes())
            self.assertEqual(len(accepted["reverse_dependencies"]), 1)
            self.assertEqual(len(accepted["indeterminate_dependencies"]), 1)
            git("update-ref", "refs/remotes/origin/main", "HEAD")
            env = {k: v for k, v in os.environ.items()
                   if not k.startswith(("GITHUB_", "GIT_")) and k != "MODULE_LAYER_BASE_REF"}
            runner = [sys.executable, str(scripts / "audit_maintainability.py"), "--check", "--format", "json"]
            before = subprocess.run(runner, cwd=repo, capture_output=True, text=True, env=env)
            self.assertEqual(before.returncode, 0, before.stderr)
            reverse = accepted["reverse_dependencies"][0]
            indeterminate = accepted["indeterminate_dependencies"][0]
            put("src/mixed.rs", (repo / "src/lower.rs").read_text())
            (repo / "src/lower.rs").unlink()
            put("src/lib.rs", "mod mixed; mod upper;\n")
            put("scripts/audit_maintainability/layer_approvals/8131-cross.toml",
                'schema_version = 1\nissue = 8131\nsummary = "cross-section fixture"\n'
                f'move = [{{ section = "reverse_dependencies", old_id = "{reverse["id"]}", '
                f'new_id = "{indeterminate["id"]}", reason = "retained implementation moved" }}]\n')
            git("add", "src", "scripts/audit_maintainability/layer_approvals")
            git("commit", "-qm", "move retained implementation over removed implementation")
            moved = subprocess.run(runner, cwd=repo, capture_output=True, text=True, env=env)
            checks = json.loads(moved.stdout)["checks"]
            self.assertEqual(moved.returncode, 1, moved.stderr)
            self.assertTrue(any(row["severity"] == "error" and row["extra"].get("code") == "approval-move"
                                for row in checks["module_layers"]["findings"]), checks["module_layers"]["findings"])
            self.assertFalse(any(row["severity"] == "error" for name, check in checks.items()
                                 if name != "module_layers" for row in check["findings"]))

if __name__ == "__main__":
    unittest.main()
