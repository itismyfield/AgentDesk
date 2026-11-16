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
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT / "scripts") not in sys.path:
    sys.path.insert(0, str(ROOT / "scripts"))

from audit_maintainability.checks.module_layer_baseline import _id, encode_baseline


class ModuleLayerRatchetTest(unittest.TestCase):
    def setUp(self):
        try:
            self.ratchet = importlib.import_module("audit_maintainability.checks.module_layer_ratchet")
        except ModuleNotFoundError as exc:
            if exc.name != "audit_maintainability.checks.module_layer_ratchet":
                raise
            self.fail("blocking module comparator is not implemented")
        self.assertTrue(callable(getattr(self.ratchet, "compare", None)), "blocking comparison is not implemented")
        self.assertTrue(callable(getattr(self.ratchet, "reduced_baseline", None)), "reduction update is not implemented")

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
        with self.assertRaises(ValueError):
            self.ratchet.reduced_baseline(accepted, hidden, self.moves())
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

    def test_moved_or_zero_rule_reverse_debt_reappearing_under_another_kind_is_error(self):
        for accepted_kind, current_kind in (("import", "path"), ("path", "import")):
            for zero in (False, True):
                with self.subTest(accepted=accepted_kind, current=current_kind, zero=zero):
                    original, moved = self.edge(kind=accepted_kind), self.edge("moved", kind=accepted_kind)
                    accepted = self.snapshot([original])
                    if zero:
                        accepted = self.ratchet.reduced_baseline(accepted, self.snapshot(), self.moves())
                    moves = self.moves({"section": "reverse_dependencies", "old_id": original["id"], "new_id": moved["id"]})
                    candidate = self.snapshot(indeterminate=[self.edge("moved", kind=current_kind)])
                    self.assert_error(self.compare(accepted, candidate, moves), "indeterminate_dependencies")
                    with self.assertRaises(ValueError):
                        self.ratchet.reduced_baseline(accepted, candidate, moves)

    def test_measured_sources_reveal_a_glob_from_a_moved_reverse_source(self):
        original, moved = self.edge(), self.edge("moved")
        moves = self.moves({"section": "reverse_dependencies", "old_id": original["id"], "new_id": moved["id"]})
        glob = self.uncertainty("glob", 1)
        candidate = self.snapshot(uncertainties=[glob])
        self.assert_warning_only(self.compare(self.snapshot([original]), candidate, moves), "uncertainties")
        self.assert_error(self.ratchet.compare(self.snapshot([original]), candidate, moves,
                                               {glob["id"]: {"source": "src/lib.rs::crate::moved"}}), "uncertainties")

    def test_glob_reintroducing_a_zero_rule_is_error_but_an_unrelated_macro_warns(self):
        reverse = self.edge(count=1)
        cleared = self.ratchet.reduced_baseline(self.snapshot([reverse]), self.snapshot(), self.moves())
        glob, macro = self.uncertainty("glob", 1), self.uncertainty("macro", 1)
        evidence = {glob["id"]: {"source": "src/lib.rs::crate::lower", "path": "crate::upper::*"},
                    macro["id"]: {"source": "src/lib.rs::crate::lower", "path": "matches"}}
        self.assert_error(self.ratchet.compare(cleared, self.snapshot(uncertainties=[glob]), self.moves(), evidence),
                          "uncertainties", glob["id"])
        result = self.ratchet.compare(cleared, self.snapshot(uncertainties=[macro]), self.moves(), evidence)
        self.assert_warning_only(result, "uncertainties", macro["id"])
        relative = {glob["id"]: {"source": "src/lib.rs::crate::lower", "path": "super::upper::*"}}
        self.assert_error(self.ratchet.compare(cleared, self.snapshot(uncertainties=[glob]), self.moves(), relative),
                          "uncertainties", glob["id"])
        child = {glob["id"]: {"source": "src/lib.rs::crate::lower", "path": "crate::upper::utils::*"}}
        self.assert_warning_only(self.ratchet.compare(cleared, self.snapshot(uncertainties=[glob]), self.moves(), child),
                                 "uncertainties", glob["id"])
        kept = self.uncertainty("kept", 1)
        beside = {kept["id"]: {"source": "src/lib.rs::crate::lower", "path": "crate::upper::*"}}
        accepted = self.snapshot([reverse], uncertainties=[kept])
        dropped_named = self.snapshot(uncertainties=[kept])
        self.assert_error(self.ratchet.compare(accepted, dropped_named, self.moves(), beside), "reverse_dependencies")
        with self.assertRaises(ValueError):
            self.ratchet.reduced_baseline(accepted, dropped_named, self.moves(), beside)
        item = {glob["id"]: {"source": "src/lib.rs::crate::lower", "path": "crate::upper::hit"}}
        self.assert_error(self.ratchet.compare(cleared, self.snapshot(uncertainties=[glob]), self.moves(), item),
                          "uncertainties", glob["id"])

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

    def test_reduction_records_decreases_without_absorbing_warned_growth(self):
        reverse, mixed, added = self.edge(count=4), self.edge("mixed", count=1), self.edge("mixed", "new", count=2)
        accepted = self.snapshot([reverse], indeterminate=[mixed], uncertainties=[self.uncertainty("old")])
        candidate = self.snapshot([dict(reverse, count=3)], indeterminate=[dict(mixed, count=2), added],
                                  uncertainties=[self.uncertainty("old", 3), self.uncertainty("new", 1)],
                                  unclassified=2)
        updated = self.ratchet.reduced_baseline(accepted, candidate, self.moves())
        self.assertEqual(updated["reverse_dependencies"], [dict(reverse, count=3)])
        self.assertEqual(updated["indeterminate_dependencies"], [mixed])
        self.assertEqual(updated["uncertainties"], [self.uncertainty("old")])
        self.assertEqual((updated["unclassified_modules"], updated["indeterminate_edges"],
                          updated["indeterminate_occurrences"]), (1, 1, 1))
        rerun = self.compare(updated, candidate)
        self.assertEqual(rerun["errors"], [])
        self.assertEqual(len(rerun["warnings"]), 7, rerun)

    def test_reduction_removes_debt_and_freezes_zero_rule(self):
        old = self.edge()
        accepted, candidate = self.snapshot([old]), self.snapshot()
        result = self.compare(accepted, candidate)
        self.assertEqual(result["errors"], [])
        self.assertTrue(any(row.get("id") == old["id"] for row in result["decreases"]))
        updated = self.ratchet.reduced_baseline(accepted, candidate, self.moves())
        self.assertEqual(updated["reverse_dependencies"], [])
        self.assertTrue(any(row.get("section") == "reverse_dependencies" and row.get("id") == old["id"]
                            and row.get("count") == 0 for row in updated["zero_rules"]))
        self.assert_error(self.compare(updated, self.snapshot([dict(old, count=1)])),
                          "reverse_dependencies", old["id"])

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

    def test_mapped_reduction_retains_reviewed_identity_and_mapping(self):
        old, moved = self.edge("before", count=3), self.edge("after", count=1)
        moves = self.moves({"section": "reverse_dependencies", "old_id": old["id"], "new_id": moved["id"]})
        updated = self.ratchet.reduced_baseline(self.snapshot([old]), self.snapshot([moved]), moves)
        self.assertEqual(updated["reverse_dependencies"], [dict(old, count=1)])
        self.assertEqual(self.compare(updated, self.snapshot([moved]), moves)["errors"], [])
        self.assert_error(self.compare(updated, self.snapshot([moved])), "reverse_dependencies", moved["id"])

    def test_mapping_a_zero_rule_does_not_permit_reintroduced_debt(self):
        old, moved = self.edge("before"), self.edge("after", count=1)
        updated = self.ratchet.reduced_baseline(self.snapshot([old]), self.snapshot(), self.moves())
        moves = self.moves({"section": "reverse_dependencies", "old_id": old["id"], "new_id": moved["id"]})
        self.assert_error(self.compare(updated, self.snapshot([moved]), moves), "reverse_dependencies")

    def test_reduction_update_accepts_only_decreases_without_absorbing_new_debt(self):
        old = self.edge(count=4)
        accepted = self.snapshot([old])
        reduced = self.snapshot([dict(old, count=2)])
        updated = self.ratchet.reduced_baseline(accepted, reduced, self.moves())
        self.assertEqual(updated["reverse_dependencies"], [dict(old, count=2)])
        mixed = self.snapshot([dict(old, count=2), self.edge("new", count=1)])
        with self.assertRaises(ValueError):
            self.ratchet.reduced_baseline(accepted, mixed, self.moves())
        with self.assertRaises(ValueError):
            self.ratchet.reduced_baseline(accepted, self.snapshot([dict(old, count=5)]), self.moves())
        self.assertEqual(accepted["reverse_dependencies"], [old])

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

    def test_comparison_and_reduction_bytes_are_order_independent(self):
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
        resolved = self.snapshot()
        first_update = self.ratchet.reduced_baseline(accepted, resolved, self.moves())
        second_update = self.ratchet.reduced_baseline(reordered, resolved, self.moves())
        self.assertEqual(encode_baseline(first_update).encode(), encode_baseline(second_update).encode())


class ModuleLayerBaselineUpdateTest(unittest.TestCase):
    edge = staticmethod(ModuleLayerRatchetTest.edge)
    uncertainty = staticmethod(ModuleLayerRatchetTest.uncertainty)
    snapshot = staticmethod(ModuleLayerRatchetTest.snapshot)
    moves = staticmethod(ModuleLayerRatchetTest.moves)

    def setUp(self):
        self.provider = importlib.import_module("audit_maintainability.checks.module_layer_baseline")
        self.assertTrue(callable(getattr(self.provider, "update_baseline", None)),
                        "guarded baseline update is not implemented")

    def test_file_update_applies_reductions_and_zero_rules(self):
        first, removed = self.edge("first", count=4), self.edge("removed", count=3)
        candidate = self.snapshot([dict(first, count=2)])
        with TemporaryDirectory() as temporary:
            path = Path(temporary) / "accepted.json"
            self.provider.write_baseline(path, self.snapshot([first, removed]))
            self.provider.update_baseline(path, candidate, self.moves())
            updated = json.loads(path.read_bytes())
            self.assertEqual(updated["reverse_dependencies"], [dict(first, count=2)])
            self.assertTrue(any(row["id"] == removed["id"] and row["count"] == 0
                                for row in updated["zero_rules"]))
            self.assertEqual(list(Path(temporary).iterdir()), [path])

    def test_rejected_file_update_preserves_original_bytes(self):
        old = self.edge(count=4)
        accepted = self.snapshot([old], uncertainties=[self.uncertainty()])
        candidates = (
            self.snapshot([dict(old, count=2), self.edge("new", count=1)], uncertainties=[self.uncertainty()]),
            self.snapshot([dict(old, count=5)], uncertainties=[self.uncertainty()]),
        )
        for candidate in candidates:
            with self.subTest(candidate=candidate), TemporaryDirectory() as temporary:
                path = Path(temporary) / "accepted.json"
                original = encode_baseline(accepted).encode().replace(b"\n", b"\r\n")
                path.write_bytes(original)
                with self.assertRaises(ValueError):
                    self.provider.update_baseline(path, candidate, self.moves())
                self.assertEqual(path.read_bytes(), original)
                self.assertEqual(list(Path(temporary).iterdir()), [path])

    def test_same_input_reduction_update_replays_identical_bytes(self):
        old = self.edge(count=4)
        candidate = self.snapshot([dict(old, count=2)])
        with TemporaryDirectory() as temporary:
            path = Path(temporary) / "accepted.json"
            self.provider.write_baseline(path, self.snapshot([old]))
            self.provider.update_baseline(path, candidate, self.moves())
            first = path.read_bytes()
            self.provider.update_baseline(path, candidate, self.moves())
            self.assertEqual(path.read_bytes(), first)

    def test_existing_update_lock_refuses_without_touching_either_file(self):
        old = self.edge()
        with TemporaryDirectory() as temporary:
            path = Path(temporary) / "accepted.json"
            self.provider.write_baseline(path, self.snapshot([old]))
            original = path.read_bytes()
            lock = path.with_name(path.name + ".lock")
            lock.write_bytes(b"another update owns this lock")
            with self.assertRaises(FileExistsError):
                self.provider.update_baseline(path, self.snapshot(), self.moves())
            self.assertEqual(path.read_bytes(), original)
            self.assertEqual(lock.read_bytes(), b"another update owns this lock")

    def test_competing_baseline_change_is_preserved_before_replace(self):
        old = self.edge()
        with TemporaryDirectory() as temporary:
            path = Path(temporary) / "accepted.json"
            self.provider.write_baseline(path, self.snapshot([old]))
            peer_bytes = b"a concurrently replaced accepted snapshot"
            with mock.patch.object(self.provider.os, "fsync", side_effect=lambda _fd: path.write_bytes(peer_bytes)):
                with self.assertRaisesRegex(ValueError, "changed"):
                    self.provider.update_baseline(path, self.snapshot(), self.moves())
            self.assertEqual(path.read_bytes(), peer_bytes)
            self.assertEqual(list(Path(temporary).iterdir()), [path])

    @staticmethod
    def legacy(payload):
        accepted = deepcopy(payload)
        accepted["schema_version"] = 2
        accepted.pop("indeterminate_dependencies")
        accepted.pop("zero_rules")
        return accepted

    def test_record_upgrade_preserves_all_existing_debt_and_adds_indeterminate_records(self):
        payload = self.snapshot([self.edge()], [self.edge("mixed")], [self.uncertainty()])
        accepted = self.legacy(payload)
        with TemporaryDirectory() as temporary:
            path = Path(temporary) / "accepted.json"
            self.provider.write_baseline(path, accepted)
            self.provider.update_baseline(path, payload, self.moves(), upgrade=True, legacy_records=deepcopy(payload))
            upgraded = json.loads(path.read_bytes())
            self.assertEqual(upgraded["schema_version"], 3)
            self.assertEqual(upgraded["indeterminate_dependencies"], payload["indeterminate_dependencies"])
            for name in ("reverse_dependencies", "uncertainties", "unclassified_modules",
                         "indeterminate_edges", "indeterminate_occurrences", "mapping_sha256"):
                self.assertEqual(upgraded[name], accepted[name])

    def test_record_upgrade_cannot_absorb_changed_counts_or_layer_mapping(self):
        payload = self.snapshot([self.edge()], [self.edge("mixed")], [self.uncertainty()])
        candidates = []
        for section in ("reverse_dependencies", "uncertainties"):
            for change in (-1, 1):
                changed = deepcopy(payload)
                changed[section][0]["count"] += change
                candidates.append(changed)
        changed = deepcopy(payload)
        changed["unclassified_modules"] += 1
        candidates.append(changed)
        changed = deepcopy(payload)
        changed["mapping_sha256"] = "3" * 64
        candidates.append(changed)
        for candidate in candidates:
            with self.subTest(candidate=candidate), TemporaryDirectory() as temporary:
                path = Path(temporary) / "accepted.json"
                self.provider.write_baseline(path, self.legacy(payload))
                original = path.read_bytes()
                with self.assertRaises(ValueError):
                    self.provider.update_baseline(path, candidate, self.moves(), upgrade=True, legacy_records=deepcopy(payload))
                self.assertEqual(path.read_bytes(), original)
                self.assertEqual(list(Path(temporary).iterdir()), [path])

    def test_record_upgrade_requires_schema_two_and_empty_movement_mapping(self):
        payload = self.snapshot([self.edge()])
        moved = self.edge("moved")
        nonempty = self.moves({"section": "reverse_dependencies", "old_id": payload["reverse_dependencies"][0]["id"],
                               "new_id": moved["id"]})
        for accepted, moves in ((payload, self.moves()), (self.legacy(payload), nonempty)):
            with self.subTest(accepted=accepted, moves=moves), TemporaryDirectory() as temporary:
                path = Path(temporary) / "accepted.json"
                self.provider.write_baseline(path, accepted)
                original = path.read_bytes()
                with self.assertRaises(ValueError):
                    self.provider.update_baseline(path, deepcopy(payload), moves, upgrade=True, legacy_records=deepcopy(payload))
                self.assertEqual(path.read_bytes(), original)

    def test_record_upgrade_requires_historical_individual_records(self):
        payload = self.snapshot(indeterminate=[self.edge("mixed")])
        with TemporaryDirectory() as temporary:
            path = Path(temporary) / "accepted.json"
            self.provider.write_baseline(path, self.legacy(payload))
            original = path.read_bytes()
            with self.assertRaisesRegex(ValueError, "historical"):
                self.provider.update_baseline(path, payload, self.moves(), upgrade=True)
            self.assertEqual(path.read_bytes(), original)

    def test_record_upgrade_refuses_indeterminate_retarget_with_equal_totals(self):
        historical = self.snapshot([self.edge()], [self.edge("mixed", "old")], [self.uncertainty()])
        current = self.snapshot([self.edge()], [self.edge("mixed", "replacement")], [self.uncertainty()])
        self.assertEqual(historical["indeterminate_edges"], current["indeterminate_edges"])
        self.assertEqual(historical["indeterminate_occurrences"], current["indeterminate_occurrences"])
        with TemporaryDirectory() as temporary:
            path = Path(temporary) / "accepted.json"
            self.provider.write_baseline(path, self.legacy(historical))
            original = path.read_bytes()
            with self.assertRaisesRegex(ValueError, "individual"):
                self.provider.update_baseline(path, current, self.moves(), upgrade=True,
                                              legacy_records=historical)
            self.assertEqual(path.read_bytes(), original)
            self.assertEqual(list(Path(temporary).iterdir()), [path])

    def test_upgrade_cli_replays_original_sha_instead_of_accepting_equal_totals(self):
        for retarget in (False, True):
            with self.subTest(retarget=retarget), TemporaryDirectory() as temporary:
                repo = Path(temporary) / "repo"
                repo.mkdir()
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
                source = ("mod lower { fn run() { crate::upper::call(); } }\n"
                          "mod upper { pub fn call() {} }\nmod spare { pub fn call() {} }\n")
                put("src/lib.rs", source)
                put("scripts/audit_maintainability/layers.toml", """schema_version = 1
rules = [{prefix="",layer="assembly",reason="fixture assembly"},
         {prefix="lower",layer="foundation",reason="fixture lower layer"}]
[layers]
foundation = 0
assembly = 1
""")
                put("scripts/audit_maintainability/layer_moves.toml", "schema_version = 1\nmoves = []\n")
                git("add", "src", "scripts", "Cargo.toml")
                git("commit", "-qm", "historical fixture")
                original_sha = git("rev-parse", "HEAD")
                snapshot_path = Path(temporary) / "original.json"
                command = [sys.executable, str(Path(self.provider.__file__).resolve()), "--repo", str(repo),
                           "--input-sha", "HEAD"]
                created = subprocess.run([*command, "--output", str(snapshot_path)], capture_output=True, text=True)
                self.assertEqual(created.returncode, 0, created.stderr)
                historical = json.loads(snapshot_path.read_bytes())
                self.assertEqual(historical["input_sha"], original_sha)
                self.assertEqual(historical["indeterminate_edges"], 1)
                accepted = Path(temporary) / "accepted.json"
                self.provider.write_baseline(accepted, self.legacy(historical))
                original = accepted.read_bytes()
                changed = source.replace("crate::upper::call()", "crate::spare::call()") if retarget else source + "pub fn harmless() {}\n"
                put("src/lib.rs", changed)
                git("add", "src/lib.rs")
                git("commit", "-qm", "current fixture")
                self.assertNotEqual(git("rev-parse", "HEAD"), original_sha)
                upgraded = subprocess.run([*command, "--output", str(accepted), "--upgrade-records"],
                                          capture_output=True, text=True)
                if retarget:
                    self.assertNotEqual(upgraded.returncode, 0)
                    self.assertIn("historical individual", upgraded.stderr)
                    self.assertEqual(accepted.read_bytes(), original)
                else:
                    self.assertEqual(upgraded.returncode, 0, upgraded.stderr)
                    updated = json.loads(accepted.read_bytes())
                    self.assertEqual(updated["schema_version"], 3)
                    self.assertEqual(updated["indeterminate_dependencies"], historical["indeterminate_dependencies"])
                    self.assertEqual(updated["input_sha"], git("rev-parse", "HEAD"))
                self.assertFalse(accepted.with_name(accepted.name + ".lock").exists())

    def test_cross_section_mapping_move_blocks_runner_and_preserves_accepted_file(self):
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
            put("scripts/audit_maintainability/layer_moves.toml", "schema_version = 1\nmoves = []\n")
            git("add", "src", "scripts", "Cargo.toml")
            git("commit", "-qm", "fixture before move")
            baseline = repo / self.provider.BASELINE
            generator = [sys.executable, str(scripts / "audit_maintainability/checks/module_layer_baseline.py"),
                         "--repo", str(repo), "--input-sha", "HEAD", "--output", str(baseline)]
            created = subprocess.run(generator, cwd=repo, capture_output=True, text=True)
            self.assertEqual(created.returncode, 0, created.stderr)
            accepted = json.loads(baseline.read_bytes())
            self.assertEqual(len(accepted["reverse_dependencies"]), 1)
            self.assertEqual(len(accepted["indeterminate_dependencies"]), 1)
            self.assertEqual(accepted["indeterminate_occurrences"], 1)
            original = baseline.read_bytes()
            # #8131 B: the accepted file stays on main beside the gate until B2; main is this commit.
            git("add", "scripts")
            git("commit", "-qm", "accepted baseline on main")
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
            put("scripts/audit_maintainability/layer_moves.toml",
                'schema_version=1\n[[moves]]\nsection="reverse_dependencies"\n'
                f'old_id="{reverse["id"]}"\nnew_id="{indeterminate["id"]}"\n')
            put("scripts/audit_maintainability/layer_approvals/8131-cross.toml",
                'schema_version = 1\nissue = 8131\nsummary = "cross-section fixture"\n'
                f'move = [{{ section = "reverse_dependencies", old_id = "{reverse["id"]}", '
                f'new_id = "{indeterminate["id"]}", reason = "retained implementation moved" }}]\n')
            git("add", "src", "scripts/audit_maintainability/layer_moves.toml", "scripts/audit_maintainability/layer_approvals")
            git("commit", "-qm", "move retained implementation over removed implementation")
            moved = subprocess.run(runner, cwd=repo, capture_output=True, text=True, env=env)
            refused_update = subprocess.run([*generator, "--update-reductions"], cwd=repo,
                                            capture_output=True, text=True)
            checks = json.loads(moved.stdout)["checks"]
            with self.subTest(operation="runner"):
                self.assertEqual(moved.returncode, 1, moved.stderr)
                self.assertTrue(any(row["severity"] == "error" and row["extra"].get("code") == "approval-move"
                                    for row in checks["module_layers"]["findings"]), checks["module_layers"]["findings"])
            with self.subTest(operation="update"):
                self.assertNotEqual(refused_update.returncode, 0, refused_update.stderr)
            with self.subTest(operation="accepted-bytes"):
                self.assertEqual(baseline.read_bytes(), original)
            self.assertFalse(any(row["severity"] == "error" for name, check in checks.items()
                                 if name != "module_layers" for row in check["findings"]))

    def test_cli_refuses_to_record_reverse_debt_hidden_behind_a_glob_import(self):
        with TemporaryDirectory() as temporary:
            repo = Path(temporary) / "repo"
            repo.mkdir()
            def git(*arguments):
                return subprocess.run(["git", "-C", str(repo), *arguments], capture_output=True,
                                      text=True, check=True).stdout.strip()
            def put(relative, text):
                path = repo / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text)
            def commit(message):
                git("add", "src", "scripts", "Cargo.toml")
                git("commit", "-qm", message)
            git("init", "-q")
            git("config", "user.name", "Fixture")
            git("config", "user.email", "fixture@example.invalid")
            put("Cargo.toml", '[package]\nname="fixture"\nversion="0.1.0"\nedition="2024"\n')
            put("src/lib.rs", "mod lower; mod upper;\n")
            put("src/lower.rs", "use crate::upper::call;\nfn run() { call(); }\n")
            put("src/upper.rs", "pub fn call() {}\n")
            put("scripts/audit_maintainability/layers.toml", """schema_version = 1
rules = [{prefix="",layer="assembly",reason="fixture assembly"},
         {prefix="lower",layer="foundation",reason="fixture lower layer"},
         {prefix="upper",layer="assembly",reason="fixture upper layer"}]
[layers]
foundation = 0
assembly = 1
""")
            put("scripts/audit_maintainability/layer_moves.toml", "schema_version = 1\nmoves = []\n")
            commit("fixture")
            output = Path(temporary) / "accepted.json"
            command = [sys.executable, str(Path(self.provider.__file__).resolve()), "--repo", str(repo),
                       "--input-sha", "HEAD", "--output", str(output)]
            created = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(created.returncode, 0, created.stderr)
            self.assertTrue(json.loads(output.read_bytes())["reverse_dependencies"])
            original = output.read_bytes()
            put("src/lower.rs", "use crate::upper::*;\nfn run() { call(); }\n")
            commit("glob")
            refused = subprocess.run([*command, "--update-reductions"], capture_output=True, text=True)
            self.assertNotEqual(refused.returncode, 0, refused.stdout)
            self.assertIn("uncertainties", refused.stderr)
            self.assertEqual(output.read_bytes(), original)
            # Clear the debt into a zero-rule, then reintroduce it through a glob.
            put("src/lower.rs", "fn run() {}\n")
            commit("clear")
            cleared = subprocess.run([*command, "--update-reductions"], capture_output=True, text=True)
            self.assertEqual(cleared.returncode, 0, cleared.stderr)
            self.assertTrue(json.loads(output.read_bytes())["zero_rules"])
            zero_rule = output.read_bytes()
            put("src/lower.rs", "use crate::upper::*;\nfn run() { call(); }\n")
            commit("reintroduce")
            reintroduced = subprocess.run([*command, "--update-reductions"], capture_output=True, text=True)
            self.assertNotEqual(reintroduced.returncode, 0, reintroduced.stdout)
            self.assertEqual(output.read_bytes(), zero_rule)

    def test_cli_refuses_a_reduction_while_a_kept_glob_still_names_the_target(self):
        with TemporaryDirectory() as temporary:
            repo = Path(temporary) / "repo"
            repo.mkdir()
            def git(*arguments):
                return subprocess.run(["git", "-C", str(repo), *arguments], capture_output=True,
                                      text=True, check=True).stdout.strip()
            def put(relative, text):
                path = repo / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text)
            def commit(message):
                git("add", "src", "scripts", "Cargo.toml")
                git("commit", "-qm", message)
            git("init", "-q")
            git("config", "user.name", "Fixture")
            git("config", "user.email", "fixture@example.invalid")
            put("Cargo.toml", '[package]\nname="fixture"\nversion="0.1.0"\nedition="2024"\n')
            put("src/lib.rs", "mod lower; mod upper;\n")
            put("src/lower.rs", "use crate::upper::{call, *};\nfn run() { call(); }\n")
            put("src/upper.rs", "pub fn call() {}\n")
            put("scripts/audit_maintainability/layers.toml", """schema_version = 1
rules = [{prefix="",layer="assembly",reason="fixture assembly"},
         {prefix="lower",layer="foundation",reason="fixture lower layer"},
         {prefix="upper",layer="assembly",reason="fixture upper layer"}]
[layers]
foundation = 0
assembly = 1
""")
            put("scripts/audit_maintainability/layer_moves.toml", "schema_version = 1\nmoves = []\n")
            commit("fixture")
            output = Path(temporary) / "accepted.json"
            command = [sys.executable, str(Path(self.provider.__file__).resolve()), "--repo", str(repo),
                       "--input-sha", "HEAD", "--output", str(output)]
            created = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(created.returncode, 0, created.stderr)
            self.assertTrue(json.loads(output.read_bytes())["reverse_dependencies"])
            original = output.read_bytes()
            put("src/lower.rs", "use crate::upper::*;\nfn run() { call(); }\n")
            commit("glob")
            refused = subprocess.run([*command, "--update-reductions"], capture_output=True, text=True)
            self.assertNotEqual(refused.returncode, 0, refused.stdout)
            self.assertIn("hidden-reverse", refused.stderr)
            self.assertEqual(output.read_bytes(), original)

    def test_cli_reduces_committed_fixture_and_refuses_a_replacement_debt(self):
        with TemporaryDirectory() as temporary:
            repo = Path(temporary) / "repo"
            repo.mkdir()
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
            put("src/lib.rs", "mod lower; mod upper; mod spare;\n")
            put("src/lower.rs", "fn run() { crate::upper::call(); crate::upper::call(); }\n")
            for module in ("upper", "spare"):
                put(f"src/{module}.rs", "pub fn call() {}\n")
            put("scripts/audit_maintainability/layers.toml", """schema_version = 1
rules = [{prefix="",layer="assembly",reason="fixture assembly"},
         {prefix="lower",layer="foundation",reason="fixture lower layer"},
         {prefix="upper",layer="assembly",reason="fixture upper layer"},
         {prefix="spare",layer="assembly",reason="fixture upper layer"}]
[layers]
foundation = 0
assembly = 1
""")
            put("scripts/audit_maintainability/layer_moves.toml", "schema_version = 1\nmoves = []\n")
            git("add", "src", "scripts", "Cargo.toml")
            git("commit", "-qm", "fixture")
            output = Path(temporary) / "accepted.json"
            command = [sys.executable, str(Path(self.provider.__file__).resolve()), "--repo", str(repo),
                       "--input-sha", "HEAD", "--output", str(output)]
            created = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(created.returncode, 0, created.stderr)
            self.assertEqual([row["count"] for row in json.loads(output.read_bytes())["reverse_dependencies"]], [2])
            put("src/lower.rs", "fn run() { crate::upper::call(); }\n")
            git("add", "src/lower.rs")
            git("commit", "-qm", "reduce fixture")
            reduced = subprocess.run([*command, "--update-reductions"], capture_output=True, text=True)
            self.assertEqual(reduced.returncode, 0, reduced.stderr)
            self.assertEqual([row["count"] for row in json.loads(output.read_bytes())["reverse_dependencies"]], [1])
            original = output.read_bytes()
            replay = subprocess.run([*command, "--update-reductions"], capture_output=True, text=True)
            self.assertEqual(replay.returncode, 0, replay.stderr)
            self.assertEqual(output.read_bytes(), original)
            put("src/lower.rs", "fn run() { crate::spare::call(); }\n")
            git("add", "src/lower.rs")
            git("commit", "-qm", "replacement fixture")
            refused = subprocess.run([*command, "--update-reductions"], capture_output=True, text=True)
            self.assertNotEqual(refused.returncode, 0)
            self.assertEqual(output.read_bytes(), original)
            self.assertFalse(output.with_name(output.name + ".lock").exists())


class S05GeneratorHistoryTest(unittest.TestCase):
    def setUp(self):
        from audit_maintainability.checks import module_layer_baseline as snapshots
        self.provider = snapshots
        self.fixture = json.loads((ROOT / "tests/fixtures/module_layer_s05.json").read_text())
        self.temporary = TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name)
        self.git("init", "-q")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        for name, body in self.fixture["files"].items():
            self.put(name, body)
        self.put(snapshots.MAPPING, self.fixture["mapping"])
        self.put("scripts/audit_maintainability/layer_moves.toml", "schema_version=1\nmoves=[]\n")
        self.commit()
        self.previous = self.git("rev-parse", "HEAD")
        self.path = self.repo / snapshots.BASELINE
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.accepted = self.measure()
        self.assertEqual(len(self.accepted["reverse_dependencies"]), 1)
        self.assertEqual(self.accepted["reverse_dependencies"][0]["count"], 1)
        self.accepted["uncertainties"] = []
        self.save()
        self.put("src/services/termination_audit/host_terminate.rs", self.fixture["moved"])
        self.put(snapshots.MAPPING, self.fixture["mapping"] + self.fixture["rules"])
        self.commit()

    def put(self, name, body):
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body)

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.repo), *args], text=True).strip()

    def commit(self):
        self.git("add", "Cargo.toml", "src", self.provider.MAPPING,
                 "scripts/audit_maintainability/layer_moves.toml")
        self.git("commit", "-qm", "fixture")

    def measure(self):
        from audit_maintainability.checks.module_layers import analyze
        occurrences = []
        graph = analyze(self.repo, occurrence_projection=occurrences)
        return self.provider.baseline(graph, self.provider.load_mapping(self.repo),
                                      self.git("rev-parse", "HEAD"), occurrences)

    def save(self):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.path.write_text(encode_baseline(self.accepted))

    def cli(self, mode="--update-reductions", previous=True, ok=True, *extra):
        command = [sys.executable, self.provider.__file__, "--repo", str(self.repo),
                   "--input-sha", "HEAD", "--output", str(self.path), mode]
        if previous:
            command += ["--previous-mapping-rev", self.previous]
        for prefix in ("contract", "herdr_terminate::contract"):
            command += ["--reclassify", "services::termination_audit::host_terminate::" + prefix]
        original = self.path.read_bytes()
        result = subprocess.run([*command, *extra], text=True, capture_output=True)
        self.assertEqual(result.returncode == 0, ok, result.stdout + result.stderr)
        if not ok:
            self.assertEqual(self.path.read_bytes(), original)
            self.assertFalse(self.path.with_name(self.path.name + ".lock").exists())
        return result

    def assert_recorded(self, result):
        updated = json.loads(self.path.read_bytes())
        self.assertEqual(updated["reverse_dependencies"], [])
        from audit_maintainability.checks.module_layer_baseline import _id
        identity = _id("uncertainty", "reference", "src/lib.rs::crate::services::session_host::herdr_gate",
                       "tracing::warn", "macro expansion not evaluated")
        self.assertEqual(updated["uncertainties"], [{"id": identity, "count": 2}])
        self.assertIn(identity, result.stderr)
        self.assertIn(self.previous, result.stderr)
        self.assertIn("previous_count=2 current_count=2", result.stderr)
        self.assertIn("--previous-mapping-rev " + self.previous, updated["generation_command"])
        from audit_maintainability.checks import module_layer_ratchet as ratchet
        self.assertEqual(ratchet.compare(updated, self.measure(), {"schema_version": 1, "moves": []})["errors"], [])

    def test_s05_reduction_records_only_proven_uncertainty(self):
        self.assert_recorded(self.cli())

    def test_s05_reduction_defaults_to_accepted_source_revision(self):
        self.assert_recorded(self.cli(previous=False))

    def test_s05_classification_admission_uses_previous_source_modules(self):
        self.assert_recorded(self.cli(mode="--admit-classification"))

    def test_new_macro_is_refused(self):
        path = "src/services/session_host/herdr_gate.rs"
        self.put(path, self.fixture["files"][path] + 'pub fn new() { tracing::error!("new"); }\n')
        self.commit()
        self.assertIn("baseline reduction refused", self.cli(ok=False).stderr)

    def test_new_unresolved_reference_is_refused(self):
        path = "src/services/session_host/herdr_gate.rs"
        self.put(path, self.fixture["files"][path] + 'pub fn new() { crate::missing::run(); }\n')
        self.commit()
        self.assertIn("baseline reduction refused", self.cli(ok=False).stderr)

    def test_previous_count_increase_is_refused(self):
        path = "src/services/session_host/herdr_gate.rs"
        self.put(path, self.fixture["files"][path] + 'pub fn new() { tracing::warn!("third"); }\n')
        self.commit()
        self.assertIn("baseline reduction refused", self.cli(ok=False).stderr)

    def test_zero_rule_reappearance_is_refused(self):
        from audit_maintainability.checks.module_layer_baseline import _id
        self.accepted["zero_rules"] = [{"id": _id("uncertainty", "reference",
            "src/lib.rs::crate::services::session_host::herdr_gate", "tracing::warn",
            "macro expansion not evaluated"), "count": 0, "section": "uncertainties"}]
        self.save()
        self.assertIn("zero-rule", self.cli(ok=False).stderr)

    def test_existing_module_layer_change_is_refused(self):
        self.put(self.provider.MAPPING, self.fixture["mapping"] + self.fixture["rules"]
                 + '[[rules]]\nprefix="services::session_host::herdr_gate"\nlayer="assembly"\nreason="mutant"\n')
        self.assertIn("known layer", self.cli(mode="--admit-classification", ok=False).stderr)

    def test_existing_positive_ceiling_cannot_grow(self):
        current = self.measure()
        self.accepted["uncertainties"] = [{**row, "count": 1}
            for row in current["uncertainties"] if row["count"] == 2]
        self.save()
        self.assertIn("increase", self.cli(ok=False).stderr)

    def test_larger_historical_count_records_only_current_count(self):
        path = "src/services/session_host/herdr_gate.rs"
        self.git("checkout", "-q", "--detach", self.previous)
        self.put(path, self.fixture["files"][path] + 'pub fn earlier() { tracing::warn!("third"); }\n')
        self.commit()
        self.previous = self.git("rev-parse", "HEAD")
        self.accepted["input_sha"] = self.previous
        self.save()
        self.put(path, self.fixture["files"][path])
        self.put("src/services/termination_audit/host_terminate.rs", self.fixture["moved"])
        self.put(self.provider.MAPPING, self.fixture["mapping"] + self.fixture["rules"])
        self.commit()
        result = self.cli()
        updated = json.loads(self.path.read_bytes())
        self.assertEqual([row["count"] for row in updated["uncertainties"]], [2])
        self.assertIn("previous_count=3 current_count=2", result.stderr)

    def test_incomplete_previous_source_graph_is_refused(self):
        self.git("checkout", "-q", "--detach", self.previous)
        self.put("src/lib.rs", "pub mod services; pub mod missing;\n")
        self.commit()
        self.previous = self.git("rev-parse", "HEAD")
        self.put("src/lib.rs", "pub mod services;\n")
        self.put("src/services/termination_audit/host_terminate.rs", self.fixture["moved"])
        self.put(self.provider.MAPPING, self.fixture["mapping"] + self.fixture["rules"])
        self.commit()
        self.assertIn("previous source graph is incomplete", self.cli(ok=False).stderr)

    def test_dry_run_proves_records_without_changing_accepted_bytes(self):
        original = self.path.read_bytes()
        result = self.cli("--update-reductions", True, True, "--dry-run")
        self.assertEqual(self.path.read_bytes(), original)
        self.assertEqual([row["count"] for row in json.loads(result.stdout)["uncertainties"]], [2])
        self.assertIn("previous_count=2 current_count=2", result.stderr)

    def test_nonancestor_revision_is_refused(self):
        current = self.git("rev-parse", "HEAD")
        self.git("checkout", "-qb", "side", self.previous)
        self.put("src/side.rs", "pub fn side() {}\n")
        self.commit()
        self.previous = self.git("rev-parse", "HEAD")
        self.git("checkout", "-q", "--detach", current)
        self.assertIn("ancestor", self.cli(ok=False).stderr)

    def test_current_revision_cannot_prove_its_own_uncertainty(self):
        self.previous = self.git("rev-parse", "HEAD")
        self.assertIn("ancestor", self.cli(ok=False).stderr)

    def test_removed_module_cannot_evade_known_layer_preservation(self):
        self.put("src/services/session_host/mod.rs", "pub mod renamed;\n")
        self.put("src/services/session_host/renamed.rs",
                 self.fixture["files"]["src/services/session_host/herdr_gate.rs"])
        (self.repo / "src/services/session_host/herdr_gate.rs").unlink()
        self.put(self.provider.MAPPING, self.fixture["mapping"] + self.fixture["rules"]
                 + '[[rules]]\nprefix="services::session_host::herdr_gate"\nlayer="assembly"\nreason="mutant"\n')
        self.commit()
        self.assertIn("known layer", self.cli(mode="--admit-classification", ok=False).stderr)

    def test_new_module_cannot_admit_a_new_reverse_dependency(self):
        self.put("src/services/session_host/mod.rs", "pub mod herdr_gate; pub mod new;\n")
        self.put("src/services/session_host/new.rs", "pub fn run() { crate::services::unrelated::keep(); }\n")
        self.put(self.provider.MAPPING, self.fixture["mapping"] + self.fixture["rules"]
                 + '[[rules]]\nprefix="services::session_host::new"\nlayer="adapter"\nreason="fixture"\n')
        self.commit()
        self.assertIn("previous projection", self.cli(mode="--admit-classification", ok=False).stderr)

    def test_shared_prefix_override_preserves_other_crate_debt(self):
        self.git("checkout", "-q", "--detach", self.previous)
        mapping = self.fixture["mapping"].replace(
            '[[rules]]\nprefix="services::termination_audit"\nlayer="application"\nreason="fixture"\n',
            '[[rules]]\nprefix="services::termination_audit::host_terminate::herdr_terminate"\nlayer="application"\nreason="fixture"\n')
        mapping += '[[rules]]\nprefix="services::termination_audit::host_terminate::contract"\nlayer="assembly"\nreason="shared rule"\n'
        mapping += '[[rules]]\nprefix="services::lower"\nlayer="application"\nreason="fixture"\n'
        other = ('pub mod services { pub mod lower { pub fn run() { BODY } } '
                 'pub mod termination_audit { pub mod host_terminate { pub mod contract { pub struct Value; } } } }\n')
        self.put("src/bin/other.rs", other.replace("BODY", ""))
        self.put(self.provider.MAPPING, mapping)
        self.commit()
        self.previous = self.git("rev-parse", "HEAD")
        self.accepted = self.measure()
        self.accepted["uncertainties"] = []
        self.save()
        self.put("src/services/termination_audit/host_terminate.rs", self.fixture["moved"])
        self.put(self.provider.MAPPING, mapping + self.fixture["rules"].replace(
            '[[rules]]\n', '[[rules]]\nroot="src/lib.rs"\n'))
        self.put("src/bin/other.rs", other.replace("BODY",
            "let _ = crate::services::termination_audit::host_terminate::contract::Value;"))
        self.commit()
        self.assertIn("previous projection", self.cli(mode="--admit-classification", ok=False).stderr)

    def _nested_new_modules(self, caller_layer, new_layer):
        self.put("src/lib.rs", "pub mod lower; pub mod parent;\n")
        self.put("src/lower.rs", "pub fn run() {}\n")
        self.put("src/parent.rs", "pub struct Existing;\n")
        mapping = ('schema_version=1\n[layers]\nadapter=1\napplication=2\nassembly=3\n'
                   '[[rules]]\nprefix=""\nlayer="assembly"\nreason="fixture"\n'
                   f'[[rules]]\nprefix="lower"\nlayer="{caller_layer}"\nreason="fixture"\n'
                   '[[rules]]\nprefix="parent"\nlayer="application"\nreason="fixture"\n')
        self.put(self.provider.MAPPING, mapping)
        self.commit()
        self.previous = self.git("rev-parse", "HEAD")
        self.accepted = self.measure()
        self.save()
        self.put("src/parent.rs", "pub struct Existing; pub mod new { pub mod child { pub struct Value; } }\n")
        self.put("src/lower.rs", "pub fn run(_: crate::parent::new::child::Value) {}\n")
        mapping += ('[[unclassified]]\nprefix="parent::new"\nreason="fixture"\n' if new_layer is None else
                    f'[[rules]]\nprefix="parent::new"\nlayer="{new_layer}"\nreason="fixture"\n')
        mapping += '[[rules]]\nprefix="parent::new::child"\nlayer="application"\nreason="fixture"\n'
        self.put(self.provider.MAPPING, mapping)
        self.commit()

    def test_new_parent_cannot_hide_a_new_child_reverse_edge(self):
        self._nested_new_modules("adapter", None)
        self.assertEqual(self.accepted["reverse_dependencies"], [])
        self.assertEqual(len(self.measure()["reverse_dependencies"]), 1)
        original = self.path.read_bytes()
        self.assertIn("previous projection", self.cli(mode="--admit-classification", ok=False).stderr)
        self.assertEqual(self.path.read_bytes(), original)
        self.assertFalse(self.path.with_name(self.path.name + ".lock").exists())

    def test_new_parent_cannot_invent_a_child_reverse_edge(self):
        self._nested_new_modules("application", "assembly")
        self.assertEqual(self.measure()["reverse_dependencies"], [])
        self.cli(mode="--admit-classification")
        self.assertEqual(json.loads(self.path.read_bytes())["reverse_dependencies"], [])
        self.assertFalse(self.path.with_name(self.path.name + ".lock").exists())

    def test_changed_macro_argument_cannot_hide_reverse_debt(self):
        before = ('macro_rules! call { ($module:ident) => { crate::$module::run() }; }\n'
                  'pub mod high { pub fn run() -> u8 { 1 } }\n'
                  'pub mod low { pub fn run() -> u8 { 1 } '
                  'pub fn f() -> u8 { crate::high::run() + call!(low) } }\n')
        self.put("src/lib.rs", before)
        self.put(self.provider.MAPPING,
                 'schema_version=1\n[layers]\nadapter=1\napplication=2\n'
                 '[[rules]]\nprefix=""\nlayer="adapter"\nreason="fixture"\n'
                 '[[rules]]\nprefix="low"\nlayer="adapter"\nreason="fixture"\n'
                 '[[rules]]\nprefix="high"\nlayer="application"\nreason="fixture"\n')
        self.commit()
        self.previous = self.git("rev-parse", "HEAD")
        self.accepted = self.measure()
        self.assertEqual(len(self.accepted["reverse_dependencies"]), 1)
        self.accepted["uncertainties"] = []
        self.save()
        self.put("src/lib.rs", before.replace("crate::high::run() + call!(low)", "call!(high) + 1"))
        self.commit()
        self.assertEqual(self.measure()["reverse_dependencies"], [])
        self.assertIn("baseline reduction refused", self.cli(ok=False).stderr)

    def test_same_macro_tokens_allow_comments_whitespace_and_line_moves(self):
        path = "src/services/session_host/herdr_gate.rs"
        body = self.fixture["files"][path].replace('tracing::warn!("first")',
            'tracing /* comment */ :: warn ! (\n "first"\n)')
        body = body.replace('tracing::warn!("second")', 'tracing::warn!(/* nested /* comment */ */ "second")')
        self.put(path, "\n// Move the source locations.\n" + body)
        self.commit()
        self.assert_recorded(self.cli())

    def test_kept_glob_cannot_hide_reverse_debt_even_with_history(self):
        # The prior glob proves presence, but it still names the disappearing debt's target.
        path = "src/services/session_host/herdr_gate.rs"
        body = self.fixture["files"][path] + "use crate::services::termination_audit::host_terminate::herdr_terminate::*;\n"
        self.git("checkout", "-q", "--detach", self.previous)
        self.put(path, body)
        self.commit()
        self.previous = self.git("rev-parse", "HEAD")
        self.accepted = self.measure()
        self.accepted["uncertainties"] = []
        self.save()
        self.put(path, body.replace("pub fn convert(_: crate::services::termination_audit::host_terminate::herdr_terminate::HerdrTerminateResult)", "pub fn convert()"))
        self.commit()
        self.assertIn("hidden-reverse", self.cli(ok=False).stderr)


if __name__ == "__main__":
    unittest.main()
