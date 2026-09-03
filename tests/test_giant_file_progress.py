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
from unittest import mock

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


class GuardRepinTest(unittest.TestCase):
    PY = "scripts/check_delivery_journal_raw_writer.py"; MAP = "scripts/check_durable_frontier_writer_call_sites.py"
    SH = "scripts/run_relay_authority_mutations.sh"; JSON = "scripts/relay_authority_contract_targets.json"
    C = "changed-path closure is not exact"

    def _patch(self, path, old, new):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            def run(*args):
                return subprocess.run(["git", *args], cwd=repo, check=True, capture_output=True, text=True).stdout.strip()
            run("init", "-q"); run("config", "user.email", "guard@example.invalid")
            run("config", "user.name", "Guard Repin Test")
            target = repo / path; target.parent.mkdir(parents=True); target.write_bytes(old)
            run("add", "-A"); run("commit", "-qm", "base"); base = run("rev-parse", "HEAD")
            target.write_bytes(new); run("add", "-A"); run("commit", "-qm", "candidate")
            candidate, original = run("rev-parse", "HEAD"), PROGRESS.ROOT
            try:
                PROGRESS.ROOT = repo
                facts = PROGRESS.diff_facts(base, candidate); patch = PROGRESS.git(
                    "diff", "-U0", "--no-renames", base, candidate, "--", path, binary=True)
            finally:
                PROGRESS.ROOT = original
        return patch, facts
    def _case(self, changes, root=ROOT_FILE, child=CHILD_FILE):
        base = {"overdue": [root, SURVIVOR], "modules": {root: 1048, SURVIVOR: 1200}, "registrations": {root: META_ROOT, SURVIVOR: META_SURVIVOR}}
        candidate = {"overdue": [SURVIVOR], "modules": {root: 860, child: 178, SURVIVOR: 1200}, "registrations": {SURVIVOR: META_SURVIVOR}}
        facts = {"changed": {PROGRESS.REGISTRY, root, child, *changes}, "additions": 200,
                 "numstat": {}, "binary": set(), "statuses": {child: "A"}, "rename_copy": False,
                 "bootstrap": False, "children": {root: [child]}, "moved": {root: occurrences(child, 100)},
                 "authority_equal": True, "registry_equal": False, "registry_exact": True, "guard_repin_patches": {}}
        for path, (old, new) in changes.items():
            patch, observed = self._patch(path, old, new)
            facts["numstat"][path] = observed["numstat"].get(path); facts["statuses"][path] = observed["statuses"].get(path)
            facts["binary"].update(observed["binary"]); facts["guard_repin_patches"][path] = patch
        return base, candidate, facts
    def _expect(self, changes, expected=None, root=ROOT_FILE, child=CHILD_FILE, tweak=None):
        base, candidate, facts = self._case(changes, root, child)
        if tweak: tweak(base, candidate, facts)
        if expected is None:
            path = next(iter(changes))
            expected = [f"guard repin is not a pure root→child path substitution: {path}", self.C]
        self.assertEqual(PROGRESS.pr_evaluation(base, candidate, facts), ("pr_strict_progress", expected))
    def _line(self, root, child, prefix=b'X="', suffix=b'"\n'):
        return (prefix + root + suffix, prefix + child + suffix)
    def test_guard_repin_accepts_giant2_normalized_replay(self):
        root = b"src/services/discord/session_relay_sink.rs"
        child = b"src/services/discord/session_relay_sink/delivery.rs"
        py = (b'    ("sink direct family (referenced / edit / split / long-chunk receipt)", "' + root + b'", "deliver_response"),\n', b'    ("sink direct family (referenced / edit / split / long-chunk receipt)", "' + child + b'", "deliver_response"),\n')
        rows = b"".join(b'        "' + root + b'": ' + n + b',\n' for n in (b"1", b"1", b"3", b"1"))
        moved = b"".join(b'        "' + child + b'": ' + n + b',\n' for n in (b"1", b"1", b"3", b"1"))
        changes = {self.PY: py, self.MAP: (rows, moved),
                   self.SH: self._line(root, child, b'readonly SESSION_RELAY_SINK="'),
                   self.JSON: self._line(root, child, b'      "file": "', b'",\n')}
        self._expect(changes, [], root.decode(), child.decode())
    def test_guard_repin_accepts_standalone_json(self):
        self._expect({self.JSON: self._line(ROOT_FILE.encode(), CHILD_FILE.encode(), b'      "file": "', b'",\n')}, [])
    def test_guard_repin_accepts_byte_and_f_string_prefixes(self):
        for prefix in (b'b"', b'f"', b'r"'):
            with self.subTest(prefix=prefix):
                self._expect({self.PY: self._line(ROOT_FILE.encode(), CHILD_FILE.encode(), prefix)}, [])
        root, child = "src/quo'te.rs", "src/quo'te/child.rs"
        self._expect({self.PY: self._line(root.encode(), child.encode())}, [], root, child)
    def test_guard_repin_accepts_triple_quoted_inner_pair(self):
        self._expect({self.PY: self._line(ROOT_FILE.encode(), CHILD_FILE.encode(), b'X="""', b'"""\n')}, [])
    def test_guard_repin_accepts_multiple_occurrences_to_one_child(self):
        root, child = ROOT_FILE.encode(), CHILD_FILE.encode()
        self._expect({self.PY: (b'A="' + root + b'" B="' + root + b'"\n',
                               b'A="' + child + b'" B="' + child + b'"\n')}, [])
    def test_guard_repin_accepts_unchanged_crlf_terminator(self):
        self._expect({self.JSON: self._line(ROOT_FILE.encode(), CHILD_FILE.encode(), suffix=b'"\r\n')}, [])
    def test_guard_repin_rejects_composite_quoted_contents(self):
        root, child = ROOT_FILE.encode(), CHILD_FILE.encode()
        composites = [b"@rev", b"~", b"+tail", b"%tail", b"\\tail", b"*", b"?",
                      b"=tail", b":12", "é".encode(), b"\r", b".bak", b"_old"]
        cases = [(b'"' + root + extra + b'"\n', b'"' + child + extra + b'"\n') for extra in composites]
        cases += [(b'"' + root.replace(b"/", b"\\/") + b'"\n', b'"' + child.replace(b"/", b"\\/") + b'"\n'),
                  (b'"' + root.replace(b"/", b"%2F") + b'"\n', b'"' + child.replace(b"/", b"%2F") + b'"\n'),
                  self._line(root, child + b"*"),
                  (b'X="' + root + b'" E="old%2Froot"\n',
                   b'X="' + child + b'" E="new%2Fchild"\n')]
        for old, new in cases:
            with self.subTest(old=old): self._expect({self.JSON: (old, new)})
    def test_guard_repin_rejects_mismatched_and_escaped_quotes(self):
        root, child = ROOT_FILE.encode(), CHILD_FILE.encode()
        for old, new in ((b'"' + root + b"'\n", b'"' + child + b"'\n"),
                         (b'\\"' + root + b'\\"\n', b'\\"' + child + b'\\"\n')):
            with self.subTest(old=old): self._expect({self.SH: (old, new)})
    def test_guard_repin_rejects_residual_unquoted_root_on_same_line(self):
        root, child = ROOT_FILE.encode(), CHILD_FILE.encode()
        cases = [(b'X="' + root + b'" ' + root + b'\n', b'X="' + child + b'" ' + root + b'\n'),
                 (b'X="' + root + b'"\n', b'X="' + child + b'" Y="' + child + b'"\n'),
                 (b'X="' + root + b'" E="old%2Froot"\n',
                  b'X="' + child + b'" E="new%2Fchild"\n')]
        for old, new in cases:
            with self.subTest(new=new): self._expect({self.SH: (old, new)})
    def test_guard_repin_rejects_unquoted_pin(self):
        self._expect({self.SH: (b"PIN=" + ROOT_FILE.encode() + b"\n",
                                b"PIN=" + CHILD_FILE.encode() + b"\n")})
    def test_guard_repin_rejects_lf_to_crlf(self):
        old, new = self._line(ROOT_FILE.encode(), CHILD_FILE.encode())
        self._expect({self.JSON: (old, new[:-1] + b"\r\n")})
    def test_guard_repin_rejects_no_final_newline_marker(self):
        self._expect({self.JSON: self._line(ROOT_FILE.encode(), CHILD_FILE.encode(), suffix=b'"')})
    def test_guard_repin_rejects_non_allowed_perfect_substitutions(self):
        pair = self._line(ROOT_FILE.encode(), CHILD_FILE.encode())
        for path in ("scripts/clippy_allow_occurrences.json", "scripts/check_log_key_drift.py"):
            with self.subTest(path=path): self._expect({path: pair}, [self.C])
    def test_guard_repin_rejects_preexisting_modified_child(self):
        changes = {self.PY: self._line(ROOT_FILE.encode(), CHILD_FILE.encode())}
        self._expect(changes, tweak=lambda _b, _c, f: f["statuses"].update({CHILD_FILE: "M"}))
        base, candidate, facts = self._case(changes)
        for loc, present in ((999, False), (1000, True)):
            with self.subTest(loc=loc, present=present):
                locations = dict(candidate["modules"])
                if not present: locations.pop(CHILD_FILE)
                else: locations[CHILD_FILE] = loc
                self.assertEqual(PROGRESS.matching_guard_repin_destinations(self.PY,
                    facts["guard_repin_patches"][self.PY], {ROOT_FILE}, facts["children"], facts["statuses"], locations, set()), [])
    def test_guard_repin_rejects_retiring_destination(self):
        changes = {self.PY: self._line(ROOT_FILE.encode(), CHILD_FILE.encode())}
        base, candidate, facts = self._case(changes); leaf = CHILD_FILE[:-3] + "/leaf.rs"
        base["overdue"].insert(1, CHILD_FILE); base["modules"][CHILD_FILE] = 1200
        base["registrations"][CHILD_FILE] = META_ROOT; candidate["modules"].update({CHILD_FILE: 800, leaf: 100})
        facts["changed"].add(leaf); facts["statuses"][leaf] = "A"
        facts["children"] = {ROOT_FILE: [CHILD_FILE], CHILD_FILE: [leaf]}
        facts["moved"][CHILD_FILE] = occurrences(leaf, 100)
        self.assertEqual(PROGRESS.matching_guard_repin_destinations(self.PY, facts["guard_repin_patches"][self.PY],
            {ROOT_FILE, CHILD_FILE}, facts["children"], facts["statuses"], candidate["modules"], set()), [])
        self.assertEqual(PROGRESS.progress_errors(base, candidate, facts), [f"guard repin is not a pure root→child path substitution: {self.PY}", self.C])
    def test_guard_repin_rejects_registered_destination(self):
        def registered(base, _candidate, _facts): base["registrations"][CHILD_FILE] = META_ROOT
        self._expect({self.PY: self._line(ROOT_FILE.encode(), CHILD_FILE.encode())}, tweak=registered)
    def test_guard_repin_rejects_child_owned_by_other_root(self):
        root2, child2 = SURVIVOR, SURVIVOR_CHILD
        changes = {self.PY: self._line(ROOT_FILE.encode(), child2.encode())}
        base, candidate, facts = self._case(changes); base["overdue"] = [ROOT_FILE, root2]
        candidate["overdue"] = []; candidate["modules"].update({root2: 800, child2: 200})
        facts["changed"].update({root2, child2}); facts["statuses"][child2] = "A"
        facts["children"] = {ROOT_FILE: [CHILD_FILE], root2: [child2]}
        facts["moved"][root2] = occurrences(child2, 100); candidate["registrations"] = {}
        self.assertEqual(PROGRESS.progress_errors(base, candidate, facts), [f"guard repin is not a pure root→child path substitution: {self.PY}", self.C])
    def test_guard_repin_rejects_two_destinations_in_one_file(self):
        root, a, b = ROOT_FILE.encode(), CHILD_FILE.encode(), b"src/services/discord/turn_finalizer/other.rs"
        changes = {self.MAP: (b'A="' + root + b'"\nB="' + root + b'"\n',
                              b'A="' + a + b'"\nB="' + b + b'"\n')}
        def two(_base, candidate, facts):
            name = b.decode(); candidate["modules"][name] = 10
            facts["children"][ROOT_FILE].append(name); facts["statuses"][name] = "A"; facts["changed"].add(name)
        self._expect(changes, tweak=two)
    def test_guard_repin_rejects_reordered_k_by_k_hunk(self):
        root, child = ROOT_FILE.encode(), CHILD_FILE.encode()
        self._expect({self.MAP: (b'A="' + root + b'"\nB="' + root + b'"\n',
                                 b'B="' + child + b'"\nA="' + child + b'"\n')})
    def test_guard_repin_rejects_context_record(self):
        changes = {self.PY: self._line(ROOT_FILE.encode(), CHILD_FILE.encode())}
        base, candidate, facts = self._case(changes); patch = facts["guard_repin_patches"][self.PY]
        lines = patch.split(b"\n"); index = next(i for i, line in enumerate(lines) if line.startswith(b"@@ "))
        lines.insert(index + 1, b" context"); facts["guard_repin_patches"][self.PY] = b"\n".join(lines)
        self.assertEqual(PROGRESS.pr_evaluation(base, candidate, facts)[1], [f"guard repin is not a pure root→child path substitution: {self.PY}", self.C])
    def test_guard_repin_rejects_more_than_eight_pairs(self):
        root, child = ROOT_FILE.encode(), CHILD_FILE.encode()
        lines = lambda value: b"".join(str(i).encode() + b'="' + value + b'"\n' for i in range(9))
        changes = {self.JSON: (lines(root), lines(child))}
        self._expect(changes, [self.C])
        base, candidate, facts = self._case({self.PY: self._line(root, child)})
        for name, mutate in (("deleted", lambda f: f["statuses"].update({self.PY: "D"})), ("added", lambda f: f["statuses"].update({self.PY: "A"})),
                             ("binary", lambda f: f["binary"].add(self.PY)), ("unequal", lambda f: f["numstat"].update({self.PY: (2, 1)})),
                             ("missing", lambda f: f["numstat"].pop(self.PY))):
            with self.subTest(name=name):
                clone = copy.deepcopy(facts); mutate(clone)
                self.assertEqual(PROGRESS.pr_evaluation(base, candidate, clone)[1], [self.C])
    def test_guard_repin_invalid_bytes_fail_closed_and_write_evidence(self):
        old, new = self._line(ROOT_FILE.encode(), CHILD_FILE.encode())
        patch, _ = self._patch(self.SH, old, new[:-1] + b"\xff\n")
        rc, payload = self._main_failure(patch)
        self.assertEqual(rc, 2); self.assertEqual(payload["selector"], "pr_strict_progress")
        self.assertEqual(payload["verdict"], "fail")
        self.assertEqual(payload["reason"], f"guard repin is not a pure root→child path substitution: {self.SH}; {self.C}")
    def test_guard_repin_rejects_nonretirement_partial_progress(self):
        changes = {self.SH: self._line(ROOT_FILE.encode(), CHILD_FILE.encode())}
        base, candidate, facts = self._case(changes)
        candidate["overdue"] = list(base["overdue"]); candidate["registrations"] = dict(base["registrations"])
        candidate["modules"][ROOT_FILE] = 848
        facts["changed"].discard(PROGRESS.REGISTRY); facts["registry_equal"] = True
        self.assertEqual(PROGRESS.pr_evaluation(base, candidate, facts)[1], [self.C])
    def test_guard_repin_rejects_bootstrap(self):
        pair = self._line(ROOT_FILE.encode(), CHILD_FILE.encode())
        self._expect({self.SH: pair}, [self.C], tweak=lambda _b, _c, f: f.update(bootstrap=True))
    def test_guard_repin_rejects_variable_rename_with_repin(self):
        root, child = ROOT_FILE.encode(), CHILD_FILE.encode()
        self._expect({self.SH: (b'OLD="' + root + b'"\necho "$OLD"\n',
                                b'NEW="' + child + b'"\necho "$NEW"\n')})
    def test_guard_repin_rejects_root_plus_child_balanced_rewrite(self):
        root, child = ROOT_FILE.encode(), CHILD_FILE.encode()
        self._expect({self.PY: (b'X="' + root + b'"\n', b'X="' + root + b'" "' + child + b'"\n')})
    def test_guard_repin_rejects_only_one_of_two_occurrences(self):
        root, child = ROOT_FILE.encode(), CHILD_FILE.encode()
        self._expect({self.SH: (b'A="' + root + b'" B="' + root + b'"\n',
                                b'A="' + child + b'" B="' + root + b'"\n')})
    def test_guard_repin_rejects_path_plus_count_change(self):
        root, child = ROOT_FILE.encode(), CHILD_FILE.encode()
        self._expect({self.MAP: (b'"' + root + b'": 1,\n', b'"' + child + b'": 2,\n')})
    def test_guard_repin_wraps_unexpected_helper_exception(self):
        path = self.PY; patch, _ = self._patch(path, *self._line(ROOT_FILE.encode(), CHILD_FILE.encode()))
        with mock.patch.object(PROGRESS, "whole_quoted_literal_pattern", side_effect=ValueError("injected")):
            with self.assertRaisesRegex(RuntimeError, "^guard repin byte proof failed$") as direct:
                PROGRESS.pure_guard_repin(patch, ROOT_FILE.encode(), CHILD_FILE.encode())
            self.assertIsInstance(direct.exception.__cause__, ValueError)
            with self.assertRaisesRegex(RuntimeError, f"^guard repin proof failed: {path}$") as outer:
                PROGRESS.matching_guard_repin_destinations(path, patch, {ROOT_FILE},
                    {ROOT_FILE: [CHILD_FILE]}, {CHILD_FILE: "A"}, {CHILD_FILE: 178}, set())
            self.assertIsInstance(outer.exception.__cause__, RuntimeError)
            self.assertIsInstance(outer.exception.__cause__.__cause__, ValueError)
            rc, payload = self._main_failure(patch, path)
        self.assertEqual(rc, 2); self.assertEqual(payload["reason"], f"guard repin proof failed: {path}")
    def _main_failure(self, patch, path=None):
        path = path or self.SH; changes = {path: self._line(ROOT_FILE.encode(), CHILD_FILE.encode())}
        base, candidate, facts = self._case(changes)
        facts["numstat"] = {path: (1, 1)}; facts["statuses"][path] = "M"
        def archive(ref, destination):
            scripts = destination / "scripts"; scripts.mkdir()
            (destination / PROGRESS.EVALUATOR).write_bytes(Path(PROGRESS.__file__).read_bytes())
            registry = f'[[entry]]\nfile = "{ROOT_FILE}"\n\n' if ref == "base" else ""
            (destination / PROGRESS.REGISTRY).write_text(registry, encoding="utf-8")
        def oid(ref, suffix="commit"):
            if ref == "HEAD" or ref == "merge": return "merge"
            if ref == "origin/main" or ref == "base": return "base"
            if ref == "head": return "head"
            if ref.endswith(":" + PROGRESS.REGISTRY): return "base-reg" if ref.startswith("base") else "merge-reg"
            return "same"
        def git(*args, binary=False):
            if args[0] == "status" or args[0] == "fetch": return b"" if binary else ""
            if args[0] == "rev-list": return "merge base head\n"
            if args[0] == "diff" and binary: return patch
            raise AssertionError(args)
        def snapshot(root, evaluation_date=None): return candidate if root.name == "candidate" else base
        env = {"GFP_EVENT_NAME": "pull_request", "GFP_REPOSITORY": "itismyfield/AgentDesk",
               "GFP_HEAD_REPOSITORY": "itismyfield/AgentDesk", "GFP_CANDIDATE_SHA": "merge",
               "GFP_BASE_SHA": "base", "GFP_HEAD_SHA": "head"}
        with tempfile.TemporaryDirectory() as directory:
            evidence = Path(directory) / "evidence.json"
            with mock.patch.dict(PROGRESS.os.environ, env, clear=True), mock.patch.multiple(
                    PROGRESS, EVIDENCE=evidence, archive=archive, oid=oid, git=git,
                    diff_facts=lambda _b, _c: copy.deepcopy(facts),
                    movement_ledger=lambda *_a: facts["moved"]), mock.patch.object(
                    PROGRESS.inventory, "giant_file_snapshot", side_effect=snapshot):
                rc = PROGRESS.main()
            return rc, json.loads(evidence.read_text(encoding="utf-8"))
if __name__ == "__main__":
    unittest.main()
