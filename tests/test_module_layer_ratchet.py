"""Synthetic contracts for blocking module debt comparisons and reduction updates."""
from __future__ import annotations

from copy import deepcopy
import importlib
import json
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
    def uncertainty(label="unresolved", count=2):
        return {"id": _id("uncertainty", label), "count": count}

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

    def test_changed_indeterminate_target_is_error_even_with_equal_totals(self):
        old, replacement = self.edge("mixed", "old"), self.edge("mixed", "new")
        accepted, candidate = self.snapshot(indeterminate=[old]), self.snapshot(indeterminate=[replacement])
        self.assertEqual(accepted["indeterminate_edges"], candidate["indeterminate_edges"])
        self.assertEqual(accepted["indeterminate_occurrences"], candidate["indeterminate_occurrences"])
        self.assert_error(self.compare(accepted, candidate), "indeterminate_dependencies", replacement["id"])

    def test_existing_indeterminate_occurrence_increase_is_error(self):
        old = self.edge("mixed", count=2)
        self.assert_error(self.compare(self.snapshot(indeterminate=[old]),
                                       self.snapshot(indeterminate=[dict(old, count=3)])),
                          "indeterminate_dependencies", old["id"])

    def test_unclassified_count_increase_is_error(self):
        self.assert_error(self.compare(self.snapshot(unclassified=1), self.snapshot(unclassified=2)))

    def test_indeterminate_aggregate_count_increase_is_error(self):
        for name in ("indeterminate_edges", "indeterminate_occurrences"):
            with self.subTest(field=name):
                candidate = self.snapshot()
                candidate[name] += 1
                self.assert_error(self.compare(self.snapshot(), candidate))

    def test_uncertainty_new_identifiers_and_occurrence_increases_are_errors(self):
        old, added = self.uncertainty("old"), self.uncertainty("new", 1)
        for candidate, identifier in (([dict(old, count=3)], old["id"]), ([old, added], added["id"])):
            with self.subTest(identifier=identifier):
                self.assert_error(self.compare(self.snapshot(uncertainties=[old]),
                                               self.snapshot(uncertainties=candidate)), "uncertainties", identifier)

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
            self.snapshot([dict(old, count=2)], uncertainties=[self.uncertainty(count=3)]),
            self.snapshot([dict(old, count=2)], uncertainties=[self.uncertainty()], unclassified=2),
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
            shutil.copy2(ROOT / "scripts/ci/rust_test_sources.py", scripts / "ci/rust_test_sources.py")
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
            runner = [sys.executable, str(scripts / "audit_maintainability.py"), "--check", "--format", "json"]
            before = subprocess.run(runner, cwd=repo, capture_output=True, text=True)
            self.assertEqual(before.returncode, 0, before.stderr)
            reverse = accepted["reverse_dependencies"][0]
            indeterminate = accepted["indeterminate_dependencies"][0]
            put("src/mixed.rs", (repo / "src/lower.rs").read_text())
            (repo / "src/lower.rs").unlink()
            put("src/lib.rs", "mod mixed; mod upper;\n")
            put("scripts/audit_maintainability/layer_moves.toml",
                'schema_version=1\n[[moves]]\nsection="reverse_dependencies"\n'
                f'old_id="{reverse["id"]}"\nnew_id="{indeterminate["id"]}"\n')
            git("add", "src", "scripts/audit_maintainability/layer_moves.toml")
            git("commit", "-qm", "move retained implementation over removed implementation")
            moved = subprocess.run(runner, cwd=repo, capture_output=True, text=True)
            refused_update = subprocess.run([*generator, "--update-reductions"], cwd=repo,
                                            capture_output=True, text=True)
            checks = json.loads(moved.stdout)["checks"]
            with self.subTest(operation="runner"):
                self.assertEqual(moved.returncode, 1, moved.stderr)
                self.assertTrue(any(row["severity"] == "error" for row in checks["module_layers"]["findings"]))
            with self.subTest(operation="update"):
                self.assertNotEqual(refused_update.returncode, 0, refused_update.stderr)
            with self.subTest(operation="accepted-bytes"):
                self.assertEqual(baseline.read_bytes(), original)
            self.assertFalse(any(row["severity"] == "error" for name, check in checks.items()
                                 if name != "module_layers" for row in check["findings"]))

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


if __name__ == "__main__":
    unittest.main()
