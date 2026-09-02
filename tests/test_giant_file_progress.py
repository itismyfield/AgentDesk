from __future__ import annotations

import copy
import importlib
import json
import subprocess
import sys
import tempfile
import unittest
from contextlib import contextmanager
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
PROGRESS = importlib.import_module("giant_file_progress")
ROOT_FILE = "src/services/discord/turn_finalizer.rs"
CHILD_FILE = "src/services/discord/turn_finalizer/terminal_handler.rs"
SURVIVOR = "src/server/worker_registry.rs"
SURVIVOR_CHILD = "src/server/worker_registry/slice.rs"
PIN_FILE = "tests/test_delivery_journal_raw_writer.py"
META_ROOT = ("shrink", "discord-finalizer", "2026-08-31", "#4712", "")
META_SURVIVOR = ("shrink", "server-runtime", "2026-08-31", "#4710", "")

def occurrences(path, count):
    return tuple((path, line) for line in range(1, count + 1))


class GiantFileProgressTest(unittest.TestCase):
    @staticmethod
    def fixture():
        base = {"overdue": [ROOT_FILE, SURVIVOR],
                "modules": {ROOT_FILE: 1048, SURVIVOR: 1200},
                "registrations": {ROOT_FILE: META_ROOT, SURVIVOR: META_SURVIVOR}}
        candidate = {"overdue": [SURVIVOR],
                     "modules": {ROOT_FILE: 860, CHILD_FILE: 178, SURVIVOR: 1200},
                     "registrations": {SURVIVOR: META_SURVIVOR}}
        facts = {"changed": set(PROGRESS.BOOTSTRAP_PATHS), "additions": 716,
                 "numstat": {}, "binary": set(), "statuses": {}, "rename_copy": False,
                 "bootstrap": True, "children": {ROOT_FILE: [CHILD_FILE]},
                 "moved": {ROOT_FILE: occurrences(CHILD_FILE, 100)}, "authority_equal": True,
                 "registry_equal": False, "registry_exact": True}
        return base, candidate, facts

    def reject(self, mutate, fragment):
        base, candidate, facts = copy.deepcopy(self.fixture())
        mutate(base, candidate, facts)
        errors = PROGRESS.progress_errors(base, candidate, facts)
        self.assertTrue(any(fragment in error for error in errors), errors)

    def ordinary_fixture(self):
        base, candidate, facts = copy.deepcopy(self.fixture())
        candidate = copy.deepcopy(base)
        facts.update(changed={PROGRESS.EVALUATOR, "tests/test_giant_file_progress.py"},
                     additions=180, authority_equal=True, registry_equal=True)
        return base, candidate, facts

    def partial_fixture(self, shrink=200):
        base, _candidate, facts = copy.deepcopy(self.fixture())
        candidate = copy.deepcopy(base)
        candidate["modules"].update({SURVIVOR: 1200 - shrink, SURVIVOR_CHILD: shrink})
        facts.update(bootstrap=False, changed={SURVIVOR, SURVIVOR_CHILD},
                     additions=shrink, children={SURVIVOR: [SURVIVOR_CHILD]},
                     moved={SURVIVOR: occurrences(SURVIVOR_CHILD, shrink)},
                     registry_equal=True, registry_exact=False)
        return base, candidate, facts

    @contextmanager
    def movement_repository(self, base_files, candidate_files):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            def run(*args):
                return subprocess.run(["git", *args], cwd=repo, check=True,
                                      capture_output=True, text=True).stdout.strip()
            run("init", "-q")
            run("config", "user.email", "giant-progress@example.invalid")
            run("config", "user.name", "Giant Progress Test")
            for path, text in base_files.items():
                target = repo / path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(text, encoding="utf-8")
            run("add", "-A")
            run("commit", "-qm", "base")
            base = run("rev-parse", "HEAD")
            for path in set(base_files) - set(candidate_files):
                (repo / path).unlink()
            for path, text in candidate_files.items():
                target = repo / path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(text, encoding="utf-8")
            run("add", "-A")
            run("commit", "-qm", "candidate")
            original = PROGRESS.ROOT
            PROGRESS.ROOT = repo
            try:
                yield base, run("rev-parse", "HEAD")
            finally:
                PROGRESS.ROOT = original

    def test_valid_retirement_progress(self):
        base, candidate, facts = self.fixture()
        self.assertEqual(PROGRESS.pr_evaluation(base, candidate, facts),
                         ("pr_strict_progress", []))
        base["overdue"] = [ROOT_FILE]
        candidate["overdue"] = []
        self.assertEqual(PROGRESS.progress_errors(base, candidate, facts), [])
        base, candidate, facts = self.fixture()
        candidate.update(overdue=[],
                         modules={ROOT_FILE: 860, CHILD_FILE: 178,
                                  SURVIVOR: 900, SURVIVOR_CHILD: 300},
                         registrations={})
        facts.update(bootstrap=False,
                     changed={PROGRESS.REGISTRY, ROOT_FILE, CHILD_FILE,
                              SURVIVOR, SURVIVOR_CHILD}, additions=478,
                     children={ROOT_FILE: [CHILD_FILE],
                               SURVIVOR: [SURVIVOR_CHILD]},
                     moved={ROOT_FILE: occurrences(CHILD_FILE, 178),
                            SURVIVOR: occurrences(SURVIVOR_CHILD, 300)})
        self.assertEqual(PROGRESS.progress_errors(base, candidate, facts), [])

    def test_ordinary_no_regression_accepts_any_base_debt(self):
        base, candidate, facts = self.ordinary_fixture()
        self.assertEqual(PROGRESS.movement_ledger(
            "base", "candidate", set(), {}, {}, {}), {})
        self.assertEqual(PROGRESS.pr_evaluation(base, candidate, facts),
                         ("pr_ordinary_no_regression", []))
        candidate["overdue"] = [*base["overdue"], "src/future.rs"]
        self.assertIn("ordinary PR changed overdue debt", "; ".join(
            PROGRESS.pr_evaluation(base, candidate, facts)[1]))
        candidate["overdue"] = list(base["overdue"])
        candidate["modules"][SURVIVOR] = 1201
        self.assertIn("new or growing giant", "; ".join(
            PROGRESS.pr_evaluation(base, candidate, facts)[1]))
        candidate["modules"][SURVIVOR] = 1200
        facts["registry_equal"] = False
        self.assertIn("registry changed", "; ".join(
            PROGRESS.pr_evaluation(base, candidate, facts)[1]))
        facts.update(registry_equal=True, authority_equal=False)
        self.assertIn("frozen authority", "; ".join(
            PROGRESS.pr_evaluation(base, candidate, facts)[1]))

    def test_progress_selection_and_partial_threshold(self):
        base, candidate, facts = self.partial_fixture(200)
        self.assertEqual(PROGRESS.pr_evaluation(base, candidate, facts),
                         ("pr_strict_progress", []))
        base, candidate, facts = self.partial_fixture(199)
        self.assertEqual(PROGRESS.pr_evaluation(base, candidate, facts)[0],
                         "pr_strict_progress")
        self.assertIn("neither retirement nor 200-line partial progress", "; ".join(
            PROGRESS.pr_evaluation(base, candidate, facts)[1]))

    def test_provenance_rejects_base_spoof(self):
        self.assertFalse(PROGRESS.provenance_matches(
            "merge", "base", "head", "stale", ["merge", "base", "head"]))
        self.assertFalse(PROGRESS.provenance_matches(
            "merge", "base", "head", "base", ["merge", "spoof", "head"]))
        self.assertTrue(PROGRESS.provenance_matches(
            "merge", "base", "head", "base", ["merge", "base", "head"]))

    def test_rename_and_copy_are_not_progress(self):
        self.reject(lambda b, c, f: f.update(rename_copy=True), "rename/copy")

    def test_retained_metadata_and_authority_are_frozen(self):
        self.reject(lambda b, c, f: c["registrations"].update(
            {SURVIVOR: ("shrink", "fake", "2099-01-01", "#9", "")}),
            "retained metadata")
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
        self.reject(lambda b, c, f: c["modules"].pop(ROOT_FILE), "same-path progress")
        self.reject(lambda b, c, f: c["modules"].update(
            {CHILD_FILE: 1000}), "bounded derived child")
        self.reject(lambda b, c, f: f["moved"].update(
            {ROOT_FILE: ()}), "moved production")
        self._assert_test_only_move_cannot_prove_production_progress()

    def _assert_test_only_move_cannot_prove_production_progress(self):
        root, child = "src/root.rs", "src/root/child.rs"
        production = [f"pub fn production_{line}() {{}}" for line in range(1200)]
        test_block = (["#[cfg(test)]", "mod tests {"]
                      + [f"fn moved_test_{line}() {{}}" for line in range(20)]
                      + ["}"])
        base_files = {root: "\n".join(production + test_block) + "\n"}
        candidate_files = {root: "\n".join(production[:800]) + "\n",
                           child: "\n".join(test_block) + "\n"}
        root_production = PROGRESS.production_line_numbers(base_files[root], 1200)
        self.assertNotIn(1201, root_production)
        self.assertEqual(PROGRESS.production_line_numbers(candidate_files[child], 0), set())
        with self.movement_repository(base_files, candidate_files) as (base_ref, candidate_ref):
            ledger = PROGRESS.movement_ledger(
                base_ref, candidate_ref, {root}, {root: [child]},
                {root: 1200}, {root: 800, child: 0})
        self.assertGreaterEqual(len({line.strip() for line in test_block}), 20)
        self.assertEqual(ledger, {root: ()})
        base = {"overdue": [root], "modules": {root: 1200},
                "registrations": {root: META_ROOT}}
        candidate = {"overdue": [root], "modules": {root: 800, child: 0},
                     "registrations": {root: META_ROOT}}
        facts = {"changed": {root, child}, "additions": len(test_block),
                 "numstat": {}, "binary": set(), "statuses": {},
                 "rename_copy": False, "bootstrap": False,
                 "children": {root: [child]}, "moved": ledger,
                 "authority_equal": True, "registry_equal": True,
                 "registry_exact": False}
        self.assertIn("moved production code", "; ".join(
            PROGRESS.progress_errors(base, candidate, facts)))

    def _assert_nested_roots_share_no_destination_occurrence_credit(self):
        outer, inner, child = "src/a.rs", "src/a/b.rs", "src/a/b/shared.rs"
        shared = [f"pub fn shared_{line}() {{}}" for line in range(20)]
        outer_unique = [f"pub fn outer_{line}() {{}}" for line in range(1180)]
        inner_unique = [f"pub fn inner_{line}() {{}}" for line in range(1180)]
        non_move = [f"pub fn new_{line}() {{}}" for line in range(810)]
        base_files = {outer: "\n".join(shared + outer_unique) + "\n",
                      inner: "\n".join(shared + inner_unique) + "\n"}
        candidate_files = {outer: "\n".join(outer_unique[:800]) + "\n",
                           inner: "\n".join(inner_unique[:800]) + "\n",
                           child: "\n".join(shared + non_move) + "\n"}
        roots = {outer, inner}
        children = {outer: [child], inner: [child]}
        with self.movement_repository(base_files, candidate_files) as (base_ref, candidate_ref):
            ledger = PROGRESS.movement_ledger(
                base_ref, candidate_ref, roots, children,
                {outer: 1200, inner: 1200},
                {outer: 800, inner: 800, child: 830})
        self.assertEqual([len(ledger[root]) for root in sorted(roots)], [20, 0])
        self.assertEqual(len({item for items in ledger.values() for item in items}), 20)
        base = {"overdue": sorted(roots), "modules": {outer: 1200, inner: 1200},
                "registrations": {outer: META_ROOT, inner: META_SURVIVOR}}
        candidate = {"overdue": [],
                     "modules": {outer: 800, inner: 800, child: 830},
                     "registrations": {}}
        facts = {"changed": {PROGRESS.REGISTRY, outer, inner, child},
                 "additions": 830, "numstat": {}, "binary": set(),
                 "statuses": {}, "rename_copy": False, "bootstrap": False,
                 "children": children, "moved": ledger,
                 "authority_equal": True, "registry_equal": False,
                 "registry_exact": True}
        errors = "; ".join(PROGRESS.progress_errors(base, candidate, facts))
        self.assertIn("800 non-moved additions", errors)
        self.assertIn("moved production code", errors)

    def test_pin_rederivation_paths_are_narrow(self):
        base, candidate, facts = self.fixture()
        facts.update(bootstrap=False,
                     changed={PROGRESS.REGISTRY, ROOT_FILE, CHILD_FILE, PIN_FILE},
                     numstat={PIN_FILE: (2, 1)}, statuses={PIN_FILE: "M"})
        self.assertEqual(PROGRESS.progress_errors(base, candidate, facts), [])
        self.reject(lambda b, c, f: (f.update(bootstrap=False), f["changed"].add(
            "scripts/pin.py")), "changed-path closure")
        self.reject(lambda b, c, f: (f.update(bootstrap=False), f["changed"].add(
            "tests/test_giant_file_progress.py")), "changed-path closure")
        def too_large(_base, _candidate, facts):
            facts.update(bootstrap=False,
                         changed={PROGRESS.REGISTRY, ROOT_FILE, CHILD_FILE, PIN_FILE},
                         numstat={PIN_FILE: (9, 1)}, statuses={PIN_FILE: "M"})
        self.reject(too_large, "changed-path closure")
        def four(_base, _candidate, facts):
            extras = {f"tests/test_pin_{i}.py" for i in range(4)}
            facts.update(bootstrap=False,
                         changed={PROGRESS.REGISTRY, ROOT_FILE, CHILD_FILE, *extras},
                         numstat={path: (1, 1) for path in extras},
                         statuses={path: "M" for path in extras})
        self.reject(four, "more than 3")
        def new_file(_base, _candidate, facts):
            facts.update(bootstrap=False,
                         changed={PROGRESS.REGISTRY, ROOT_FILE, CHILD_FILE, PIN_FILE},
                         numstat={PIN_FILE: (2, 0)}, statuses={PIN_FILE: "A"})
        self.reject(new_file, "changed-path closure")
        facts["binary"] = {PIN_FILE}
        self.assertIn("changed-path closure", "; ".join(
            PROGRESS.progress_errors(base, candidate, facts)))

    def test_generated_and_maintenance_doc_extras_are_narrow(self):
        base, candidate, facts = self.fixture()
        core = {PROGRESS.REGISTRY, ROOT_FILE, CHILD_FILE}
        facts.update(bootstrap=False, statuses={}, numstat={})
        facts["changed"] = core | {"ARCHITECTURE.md"}
        facts["statuses"]["ARCHITECTURE.md"] = "M"
        facts["numstat"]["ARCHITECTURE.md"] = (500, 500)
        self.assertEqual(PROGRESS.progress_errors(base, candidate, facts), [])

        facts["changed"] = core | {"docs/generated/not-an-inventory.md"}
        facts["statuses"] = {"docs/generated/not-an-inventory.md": "M"}
        facts["numstat"] = {"docs/generated/not-an-inventory.md": (1, 1)}
        self.assertIn("changed-path closure", "; ".join(
            PROGRESS.progress_errors(base, candidate, facts)))

        maintenance = "docs/agent-maintenance/discord-outbound-migration.md"
        facts["changed"] = core | {maintenance}
        facts["statuses"] = {maintenance: "M"}
        facts["numstat"] = {maintenance: (40, 40)}
        self.assertEqual(PROGRESS.progress_errors(base, candidate, facts), [])
        facts["numstat"] = {maintenance: (41, 1)}
        self.assertIn("changed-path closure", "; ".join(
            PROGRESS.progress_errors(base, candidate, facts)))

        facts["changed"] = core | {"docs/other.md"}
        facts["statuses"] = {"docs/other.md": "M"}
        facts["numstat"] = {"docs/other.md": (1, 1)}
        self.assertIn("changed-path closure", "; ".join(
            PROGRESS.progress_errors(base, candidate, facts)))

        maintenance_files = {
            f"docs/agent-maintenance/extra-{index}.md" for index in range(4)}
        facts["changed"] = core | maintenance_files
        facts["statuses"] = {path: "M" for path in maintenance_files}
        facts["numstat"] = {path: (1, 1) for path in maintenance_files}
        self.assertIn("more than 3 maintenance", "; ".join(
            PROGRESS.progress_errors(base, candidate, facts)))

    def test_non_moved_additions_cap(self):
        base, candidate, facts = self.partial_fixture(900)
        base["modules"][SURVIVOR] = 1900
        candidate["modules"][SURVIVOR] = 1000
        facts.update(additions=900,
                     moved={SURVIVOR: occurrences(SURVIVOR_CHILD, 900)})
        self.assertEqual(PROGRESS.progress_errors(base, candidate, facts), [])
        facts["moved"] = {SURVIVOR: ()}
        self.assertIn("800 non-moved additions", "; ".join(
            PROGRESS.progress_errors(base, candidate, facts)))
        self._assert_nested_roots_share_no_destination_occurrence_credit()

    def test_metadata_optional_and_transition_frozen(self):
        base, candidate, facts = self.ordinary_fixture()
        facts["changed"].add(PROGRESS.METADATA)
        self.assertEqual(PROGRESS.pr_evaluation(base, candidate, facts)[1], [])
        base, candidate, facts = self.partial_fixture(200)
        facts["changed"].add(PROGRESS.METADATA)
        self.assertEqual(PROGRESS.progress_errors(base, candidate, facts), [])
        facts["authority_equal"] = False
        self.assertIn("frozen authority", "; ".join(
            PROGRESS.progress_errors(base, candidate, facts)))
        self.assertNotIn(PROGRESS.METADATA, PROGRESS.FROZEN)

    def test_diff_bounds_and_bootstrap_closure_are_exact(self):
        self.reject(lambda b, c, f: f.update(additions=901, moved={ROOT_FILE: ()}),
                    "800 non-moved additions")
        self.reject(lambda b, c, f: f["changed"].add(
            "docs/fake.md"), "changed-path closure")
        self.assertEqual(len(PROGRESS.BOOTSTRAP_PATHS), 16)

    def test_main_records_debt_without_absolute_zero_requirement(self):
        payload = PROGRESS.main_record({"overdue": [ROOT_FILE]})
        self.assertEqual(payload, {"overdue": [ROOT_FILE], "overdue_count": 1})
        self.assertEqual(PROGRESS.main_record({"overdue": []}),
                         {"overdue": [], "overdue_count": 0})

    def test_registry_helper_and_evidence_are_deterministic(self):
        registry = ('[[entry]]\n# reason\nfile = "src/a.rs"\nowner = "x"\n\n'
                    '[[entry]]\nfile = "src/b.rs"\n')
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
