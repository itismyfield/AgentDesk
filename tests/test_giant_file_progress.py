from __future__ import annotations

import copy
import importlib
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
PROGRESS = importlib.import_module("giant_file_progress")
ROOT_FILE = "src/services/discord/turn_finalizer.rs"
CHILD_FILE = "src/services/discord/turn_finalizer/terminal_handler.rs"
SURVIVOR = "src/server/worker_registry.rs"
META_ROOT = ("shrink", "discord-finalizer", "2026-08-31", "#4712", "")
META_SURVIVOR = ("shrink", "server-runtime", "2026-08-31", "#4710", "")


class GiantFileProgressTest(unittest.TestCase):
    @staticmethod
    def fixture():
        base = {
            "overdue": [ROOT_FILE, SURVIVOR],
            "modules": {ROOT_FILE: 1048, SURVIVOR: 1200},
            "registrations": {ROOT_FILE: META_ROOT, SURVIVOR: META_SURVIVOR},
        }
        candidate = {
            "overdue": [SURVIVOR],
            "modules": {ROOT_FILE: 860, CHILD_FILE: 178, SURVIVOR: 1200},
            "registrations": {SURVIVOR: META_SURVIVOR},
        }
        facts = {
            "changed": set(PROGRESS.BOOTSTRAP_PATHS),
            "additions": 716,
            "rename_copy": False,
            "bootstrap": True,
            "children": {ROOT_FILE: [CHILD_FILE]},
            "moved": {ROOT_FILE: 100},
            "authority_equal": True,
            "registry_exact": True,
        }
        return base, candidate, facts

    def reject(self, mutate, fragment):
        base, candidate, facts = copy.deepcopy(self.fixture())
        mutate(base, candidate, facts)
        errors = PROGRESS.progress_errors(base, candidate, facts)
        self.assertTrue(any(fragment in error for error in errors), errors)

    def test_valid_progress_and_final_retirement(self):
        base, candidate, facts = self.fixture()
        self.assertEqual(PROGRESS.progress_errors(base, candidate, facts), [])
        base["overdue"] = [ROOT_FILE]
        candidate["overdue"] = []
        self.assertEqual(PROGRESS.progress_errors(base, candidate, facts), [])

    def test_overdue_set_must_strictly_shrink(self):
        self.reject(lambda b, c, f: b.update(overdue=[]), "proper subset")
        self.reject(lambda b, c, f: c.update(overdue=list(b["overdue"])), "proper subset")
        def substitute(base, candidate, _facts):
            candidate["overdue"] = [SURVIVOR, "src/new.rs"]
            candidate["modules"]["src/new.rs"] = 1000
        self.reject(substitute, "proper subset")

    def test_provenance_rejects_base_spoof(self):
        self.assertFalse(PROGRESS.provenance_matches(
            "merge", "base", "head", "stale", ["merge", "base", "head"]))
        self.assertFalse(PROGRESS.provenance_matches(
            "merge", "base", "head", "base", ["merge", "spoof", "head"]))
        self.assertTrue(PROGRESS.provenance_matches(
            "merge", "base", "head", "base", ["merge", "base", "head"]))

    def test_rename_and_copy_are_not_retirement(self):
        self.reject(lambda b, c, f: f.update(rename_copy=True), "rename/copy")

    def test_retained_metadata_and_authority_are_frozen(self):
        def metadata(_base, candidate, _facts):
            candidate["registrations"][SURVIVOR] = (
                "shrink", "fake", "2099-01-01", "#9", "")
        self.reject(metadata, "retained metadata")
        self.reject(lambda b, c, f: f.update(authority_equal=False), "frozen authority")

    def test_registry_retirement_is_exact(self):
        self.reject(lambda b, c, f: c["registrations"].update(
            {ROOT_FILE: META_ROOT}), "registry entry")
        self.reject(lambda b, c, f: f.update(registry_exact=False), "exact retired-entry")
        generator = PROGRESS.inventory
        originals = (generator.load_giant_file_registry,
                     generator.load_giant_file_issue_metadata,
                     generator.load_giant_file_closed_issue_transition_list,
                     generator.load_giant_file_issue_ratchets)
        generator.load_giant_file_registry = lambda: ([], [], [])
        generator.load_giant_file_issue_metadata = lambda: {}
        generator.load_giant_file_closed_issue_transition_list = lambda: {ROOT_FILE}
        generator.load_giant_file_issue_ratchets = lambda: {
            "closed_deadline_entries": 1, "transition_list_entries": 1}
        module = generator.ModuleEntry(ROOT_FILE, ROOT_FILE, 860, 860, 0, ())
        try:
            self.assertEqual(generator.build_giant_registrations(
                [module], allow_overdue=True), [])
            with self.assertRaises(generator.ParseError):
                generator.build_giant_registrations([module])
        finally:
            (generator.load_giant_file_registry,
             generator.load_giant_file_issue_metadata,
             generator.load_giant_file_closed_issue_transition_list,
             generator.load_giant_file_issue_ratchets) = originals

    def test_new_or_growing_giants_are_rejected(self):
        self.reject(lambda b, c, f: c["modules"].update(
            {"src/future.rs": 1000}), "new or growing giant")
        self.reject(lambda b, c, f: c["modules"].update(
            {SURVIVOR: 1201}), "new or growing giant")

    def test_same_path_child_and_movement_are_required(self):
        self.reject(lambda b, c, f: c["modules"].pop(ROOT_FILE), "same-path retirement")
        self.reject(lambda b, c, f: c["modules"].update(
            {CHILD_FILE: 1000}), "bounded derived child")
        self.reject(lambda b, c, f: f["moved"].update(
            {ROOT_FILE: 0}), "moved production")

    def test_diff_bounds_and_bootstrap_closure_are_exact(self):
        self.reject(lambda b, c, f: f.update(additions=801), "800 additions")
        self.reject(lambda b, c, f: f["changed"].add(
            "docs/fake.md"), "changed-path closure")
        self.assertEqual(len(PROGRESS.BOOTSTRAP_PATHS), 16)

    def test_main_requires_absolute_zero_debt(self):
        self.assertFalse(PROGRESS.main_clean({"overdue": [ROOT_FILE]}))
        self.assertTrue(PROGRESS.main_clean({"overdue": []}))

    def test_registry_helper_and_evidence_are_deterministic(self):
        registry = (
            '[[entry]]\n# reason\nfile = "src/a.rs"\nowner = "x"\n\n'
            '[[entry]]\nfile = "src/b.rs"\n'
        )
        self.assertEqual(PROGRESS.without_entry(registry, "src/a.rs"),
                         '[[entry]]\nfile = "src/b.rs"\n')
        self.assertIsNone(PROGRESS.without_entry(registry, "src/missing.rs"))
        with tempfile.TemporaryDirectory() as directory:
            original = PROGRESS.EVIDENCE
            PROGRESS.EVIDENCE = Path(directory) / "evidence.json"
            try:
                PROGRESS.write_evidence({"schema": 1, "verdict": "progress-pass"})
                text = PROGRESS.EVIDENCE.read_text(encoding="utf-8")
            finally:
                PROGRESS.EVIDENCE = original
        pairs = json.loads(text, object_pairs_hook=lambda values: values)
        self.assertEqual(pairs, [("schema", 1), ("verdict", "progress-pass")])


if __name__ == "__main__":
    unittest.main()
