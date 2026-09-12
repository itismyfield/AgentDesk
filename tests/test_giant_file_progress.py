from __future__ import annotations

import copy
import builtins
import importlib
import importlib.util
import io
import json
import subprocess
import sys
import tempfile
import unittest
from contextlib import contextmanager, redirect_stderr
from datetime import datetime, timedelta, timezone
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
        # The event triple is the whole input: origin/main is not consulted, so a
        # main advance past "base" cannot turn a legitimate candidate into a reject.
        self.assertTrue(PROGRESS.provenance_matches(
            "merge", "base", "head", ["merge", "base", "head"]))
        self.assertFalse(PROGRESS.provenance_matches(
            "merge", "base", "head", ["merge", "spoof", "head"]))
        self.assertTrue(PROGRESS.provenance_matches(
            "merge", "base", "head", ["merge", "base", "head"]))

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

P = PROGRESS
G = P.inventory
LEDGER_ROOT = "src/root.rs"
OTHER = "src/other.rs"
RETIRED = "src/retired.rs"
OLD, NEW = "2026-08-31", "2026-10-31"
STAMP = "2026-09-02T11:41:35Z"
NOW = datetime(2026, 9, 7, tzinfo=timezone.utc)


def marker(old=OLD, new=NEW):
    return f"# DEADLINE RESET {old} -> {new} on 2026-09-07 (#5742). The prior date\n# needs a bounded repair.\n"


def entry(path, deadline, history=""):
    return (f'[[entry]]\n# measured production LoC\n{history}file = "{path}"\n'
            f'decision = "shrink"\nowner = "team"\ndeadline = "{deadline}"\n'
            'decompose_issue = "#4712"\n\n')


def metadata(state="open", stamp=STAMP, closed=0, transitions=0):
    return {"schema_version": 2, "refreshed_at": stamp,
            "ratchets": {"closed_deadline_entries": closed, "transition_list_entries": transitions},
            "issues": [{"number": 4712, "state": state, "title": "decompose root",
                        "owners": ["team"], "files": [LEDGER_ROOT, OTHER]}]}


class GiantFileLedgerRepairTest(unittest.TestCase):
    def fixture(self, moved=True, history=""):
        registrations = {LEDGER_ROOT: ("shrink", "team", OLD, "#4712", ""),
                         OTHER: ("shrink", "team", NEW, "#4712", "")}
        base = {"modules": {LEDGER_ROOT: 1200, OTHER: 1200, RETIRED: 860},
                "registrations": registrations, "overdue": [LEDGER_ROOT]}
        candidate = copy.deepcopy(base)
        if moved:
            candidate["registrations"][LEDGER_ROOT] = ("shrink", "team", NEW, "#4712", "")
            candidate["overdue"] = []
        prefix = 'grandfathered = []\ngrandfathered_baseline_paths = []\n'
        old = {"registry": prefix + entry(LEDGER_ROOT, OLD, history) + entry(OTHER, NEW),
               "transition": {RETIRED}, "ratchets": metadata(closed=1, transitions=1)["ratchets"],
               "pins": {RETIRED: 1048}}
        new = copy.deepcopy(old)
        if moved:
            new["registry"] = prefix + entry(LEDGER_ROOT, NEW, history + marker()) + entry(OTHER, NEW)
        facts = {"changed": {P.REGISTRY} if moved else {P.METADATA},
                 "authority_equal": True, "registry_equal": not moved,
                 "ledger_base": old, "ledger_candidate": new}
        return base, candidate, facts

    def assert_ledger(self, case, fragment=None):
        selector, errors = P.pr_evaluation(*case)
        self.assertEqual(selector, "pr_ledger_repair")
        if fragment is None:
            self.assertEqual(errors, [])
        else:
            self.assertIn(fragment, "; ".join(errors))

    def test_r2_04_transition_addition_and_count_preserving_swap(self):
        for paths in ({RETIRED, "src/new.rs"}, {"src/new.rs"}):
            with self.subTest(paths=paths):
                case = self.fixture(False)
                case[2]["ledger_candidate"]["transition"] = paths
                self.assert_ledger(case, "E6: transition paths added or replaced")

    def test_r2_03_measured_5744_retired_paths_allow_transition_cleanup(self):
        # Re-measured at base 5a3d16ef765b / head 769f7f0dd700: no source diff.
        retired = {"src/server/worker_registry.rs": 483,
                   "src/services/discord/outbound/turn_output_controller.rs": 996,
                   "src/services/discord/tui_direct_pending_start.rs": 933,
                   "src/services/discord/turn_finalizer.rs": 860}
        base, candidate, facts = self.fixture()
        for snapshot in (base, candidate):
            snapshot["modules"].update(retired)
            self.assertFalse(set(retired) & snapshot["registrations"].keys())
        facts["ledger_base"]["transition"] = set(retired)
        facts["ledger_base"]["ratchets"] = dict.fromkeys(G.GIANT_FILE_ISSUE_RATCHET_KEYS, 4)
        facts["ledger_candidate"]["transition"] = set()
        facts["ledger_candidate"]["ratchets"] = dict.fromkeys(G.GIANT_FILE_ISSUE_RATCHET_KEYS, 0)
        self.assert_ledger((base, candidate, facts))

    def test_e6_requires_retirement_in_both_snapshots_without_missing_loc_default(self):
        for side in (0, 1):
            for loc in (None, -1, 1000):
                with self.subTest(side=side, loc=loc):
                    base, candidate, facts = self.fixture(False)
                    facts["ledger_candidate"]["transition"] = set()
                    snapshot = (base, candidate)[side]
                    snapshot["modules"].pop(RETIRED)
                    if loc is not None:
                        snapshot["modules"][RETIRED] = loc
                    errors = P.ledger_repair_errors(base, candidate, facts, [])
                    self.assertIn("measured retirement in " + ("base", "candidate")[side], "; ".join(errors))

    def test_r2_05_and_06_history_cannot_move_disappear_or_lose_rationale(self):
        history = marker("2026-04-30", "2026-06-30") + marker("2026-06-30", OLD)
        for moved in (False, True):
            for mutation in ("header", "other", "delete", "edit", "rationale"):
                with self.subTest(moved=moved, mutation=mutation):
                    case = self.fixture(moved, history)
                    ledger = case[2]["ledger_candidate"]
                    first = marker("2026-04-30", "2026-06-30")
                    if mutation == "edit":
                        ledger["registry"] = ledger["registry"].replace("(#5742)", "(#5743)", 1)
                    elif mutation == "rationale":
                        ledger["registry"] = ledger["registry"].replace("# needs a bounded repair.\n", "", 1)
                    else:
                        ledger["registry"] = ledger["registry"].replace(first, "", 1)
                        if mutation == "header":
                            ledger["registry"] = first + ledger["registry"]
                        elif mutation == "other":
                            ledger["registry"] = ledger["registry"].replace(f'file = "{OTHER}"', first + f'file = "{OTHER}"')
                    self.assert_ledger(case, "E3")

    def test_r2_07_exactly_one_new_marker_per_moved_path_with_matching_dates(self):
        for replacement in ("", marker() * 2, marker("2026-08-30", NEW), marker(OLD, "2026-11-01")):
            with self.subTest(replacement=replacement):
                case = self.fixture()
                case[2]["ledger_candidate"]["registry"] = case[2]["ledger_candidate"]["registry"].replace(marker(), replacement)
                self.assert_ledger(case, "E3")
        case = self.fixture()
        ledger = case[2]["ledger_candidate"]
        ledger["registry"] = ledger["registry"].replace(marker(), "").replace(f'file = "{OTHER}"', marker() + f'file = "{OTHER}"')
        self.assert_ledger(case, "E3")
        self.assert_ledger(self.fixture())

    def test_r2_08_fourteen_paths_each_append_one_marker_at_92_day_limit(self):
        base, candidate, facts = self.fixture()
        for snapshot, deadline in ((base, NEW), (candidate, "2027-01-31")):
            snapshot["registrations"] = {f"src/{index}.rs": ("shrink", "team", deadline, "#4712", "") for index in range(14)}
            snapshot["modules"] = {path: 1200 for path in snapshot["registrations"]}
            snapshot["overdue"] = []
        for key, snapshot in (("ledger_base", base), ("ledger_candidate", candidate)):
            history = marker(NEW, "2027-01-31") if key == "ledger_candidate" else ""
            facts[key]["registry"] = "".join(entry(path, value[2], history) for path, value in snapshot["registrations"].items())
        self.assert_ledger((base, candidate, facts))

    def test_r2_09_pin_only_needs_no_deadline_marker(self):
        case = self.fixture(False, marker("2026-06-30", OLD))
        case[2]["changed"] = {P.GIANT_PIN}
        case[2]["ledger_candidate"]["pins"][RETIRED] = 860
        self.assert_ledger(case)

    def test_e7_e8_ratchets_and_pins_only_tighten_or_add_measured_pins(self):
        for key in G.GIANT_FILE_ISSUE_RATCHET_KEYS:
            case = self.fixture(False)
            case[2]["ledger_candidate"]["ratchets"][key] += 1
            self.assert_ledger(case, "E7")
        for pins in ({}, {RETIRED: 1049}, {RETIRED: 1048, LEDGER_ROOT: 1201}, {RETIRED: 1048, "src/missing.rs": 1}):
            case = self.fixture(False)
            case[2]["ledger_candidate"]["pins"] = pins
            self.assert_ledger(case, "E8")
        case = self.fixture(False)
        case[2]["ledger_candidate"]["pins"][LEDGER_ROOT] = 1200
        self.assert_ledger(case)

    def test_r2_13_two_extensions_pass_third_fails_even_with_history_laundering(self):
        base, candidate, facts = self.fixture()
        for snapshot in (base, candidate):
            snapshot["registrations"].pop(OTHER)
        for key in ("ledger_base", "ledger_candidate"):
            facts[key]["registry"] = facts[key]["registry"].replace(entry(OTHER, NEW), "")
        self.assert_ledger((base, candidate, facts))
        for old, new in ((NEW, "2027-01-31"), ("2027-01-31", "2027-04-30")):
            base = copy.deepcopy(candidate)
            facts["ledger_base"] = copy.deepcopy(facts["ledger_candidate"])
            candidate["registrations"][LEDGER_ROOT] = ("shrink", "team", new, "#4712", "")
            facts["ledger_candidate"]["registry"] = facts["ledger_candidate"]["registry"].replace(
                f'file = "{LEDGER_ROOT}"', marker(old, new) + f'file = "{LEDGER_ROOT}"').replace(
                f'deadline = "{old}"', f'deadline = "{new}"', 1)
            self.assert_ledger((base, candidate, facts), "E4" if new == "2027-04-30" else None)
        facts["ledger_candidate"]["registry"] = facts["ledger_candidate"]["registry"].replace(marker(), "")
        self.assert_ledger((base, candidate, facts), "E3")

    def test_l1_to_l4_selection_rejects_source_scope_and_non_deadline_registry_changes(self):
        mutations = [lambda b, c, f: f["changed"].add(LEDGER_ROOT),
                     lambda b, c, f: f.update(changed=set()),
                     lambda b, c, f: c["modules"].update({LEDGER_ROOT: 1199}),
                     lambda b, c, f: c["registrations"].pop(LEDGER_ROOT),
                     lambda b, c, f: c["registrations"].update({LEDGER_ROOT: ("keep", "team", "", "", "retained API")}),
                     lambda b, c, f: c["registrations"].update({LEDGER_ROOT: ("shrink", "other", NEW, "#4712", "")}),
                     lambda b, c, f: c["registrations"].update({LEDGER_ROOT: ("shrink", "team", "2026-07-31", "#4712", "")})]
        for mutate in mutations:
            case = self.fixture()
            mutate(*case)
            self.assertIsNone(P.ledger_repair_moves(*case))
        for replacement in ('grandfathered_baseline_paths = ["src/new.rs"]', 'grandfathered_baseline_paths = [\n"src/new.rs",\n]'):
            case = self.fixture()
            ledger = case[2]["ledger_candidate"]
            ledger["registry"] = ledger["registry"].replace("grandfathered_baseline_paths = []", replacement)
            self.assertIsNone(P.ledger_repair_moves(*case))

    def test_r2_15_marker_prose_ownership_and_actual_retirement_deletion(self):
        text = entry(LEDGER_ROOT, NEW, marker()) + entry(OTHER, NEW)
        records = P.deadline_markers(text)
        self.assertEqual(records[LEDGER_ROOT], [(OLD, NEW, "2026-09-07", 5742, marker())])
        remaining = P.without_entry(text, LEDGER_ROOT)
        self.assertEqual(remaining, entry(OTHER, NEW))
        self.assertEqual(P.deadline_markers(remaining), {OTHER: []})
        self.assertIsNone(P.without_entry(entry(LEDGER_ROOT, NEW, marker()).rstrip(), LEDGER_ROOT))
        case = self.fixture()
        case[2]["ledger_candidate"]["registry"] = case[2]["ledger_candidate"]["registry"].replace("# measured production LoC", "# corrected production LoC")
        self.assert_ledger(case)

    def test_malformed_unowned_duplicate_and_blank_separated_markers_fail_closed(self):
        invalid = [marker().replace("2026-09-07", "2026-02-30"), marker().replace("#5742", "#0"),
                   marker().replace("#5742", "#bad"), marker() + "\n"]
        for history in invalid:
            with self.subTest(history=history), self.assertRaises(G.ParseError):
                P.deadline_markers(entry(LEDGER_ROOT, NEW, history))
        for text in (marker() + entry(LEDGER_ROOT, NEW), entry(LEDGER_ROOT, NEW) + marker(),
                     entry(LEDGER_ROOT, NEW, marker()).replace('owner = "team"', f'file = "{LEDGER_ROOT}"')):
            with self.assertRaises(G.ParseError):
                P.deadline_markers(text)

    def test_r2_16_selected_ledger_error_never_falls_back_to_ordinary(self):
        case = self.fixture(False)
        case[2]["ledger_candidate"]["registry"] = case[2]["ledger_candidate"]["registry"].replace(f'file = "{LEDGER_ROOT}"', marker() + f'file = "{LEDGER_ROOT}"')
        self.assert_ledger(case, "E3")

    def test_r2_17_distinct_count_allows_singleton_replacement_by_91_days(self):
        case = self.fixture()
        case[1]["registrations"][LEDGER_ROOT] = ("shrink", "team", "2026-11-30", "#4712", "")
        case[2]["ledger_candidate"]["registry"] = case[2]["ledger_candidate"]["registry"].replace(marker(), marker(OLD, "2026-11-30")).replace(f'deadline = "{NEW}"', 'deadline = "2026-11-30"', 1)
        self.assert_ledger(case)
        case[1]["registrations"][LEDGER_ROOT] = ("shrink", "team", "2026-12-02", "#4712", "")
        case[2]["ledger_candidate"]["registry"] = case[2]["ledger_candidate"]["registry"].replace("2026-11-30", "2026-12-02")
        self.assert_ledger(case, "E1")
        case = self.fixture()
        case[0]["registrations"][OTHER] = ("shrink", "team", OLD, "#4712", "")
        case[1]["registrations"][OTHER] = case[0]["registrations"][OTHER]
        for key in ("ledger_base", "ledger_candidate"):
            case[2][key]["registry"] = case[2][key]["registry"].replace(entry(OTHER, NEW), entry(OTHER, OLD))
        self.assert_ledger(case, "E2")


class GiantFileLedgerIntegrationTest(unittest.TestCase):
    def files(self, *, stamp=STAMP, state="open", transition=False, deadline=OLD, history=""):
        return {LEDGER_ROOT: "pub fn production() {}\n" * 1200,
                P.REGISTRY: 'grandfathered = []\ngrandfathered_baseline_paths = []\n' + entry(LEDGER_ROOT, deadline, history),
                P.METADATA: json.dumps(metadata(state, stamp, int(state == "closed"), int(transition))),
                P.TRANSITION: LEDGER_ROOT + "\n" if transition else "# no transitions\n",
                P.GIANT_PIN: f'[giant_file_ratchet]\n"{LEDGER_ROOT}" = 1200\n',
                P.EVALUATOR: Path(P.__file__).read_text(encoding="utf-8")}

    @staticmethod
    def write(root, files):
        for path, text in files.items():
            target = root / path
            target.parent.mkdir(parents=True, exist_ok=True)
            if isinstance(text, bytes):
                target.write_bytes(text)
            else:
                target.write_text(text, encoding="utf-8")

    def evaluate(self, before, after, now=NOW):
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(G, "now_utc", return_value=now), redirect_stderr(io.StringIO()):
            roots = [Path(directory) / name for name in ("base", "candidate")]
            for root, files in zip(roots, (before, after)):
                self.write(root, files)
            snapshots = [G.giant_file_snapshot(root, evaluation_date=now.date()) for root in roots]
            facts = {"changed": {path for path in before.keys() | after.keys() if before.get(path) != after.get(path)},
                     "ledger_base": P.load_ledger(roots[0]), "ledger_candidate": P.load_ledger(roots[1]),
                     "rename_copy": False, "additions": 4, "bootstrap": False,
                     "moved": {}, "children": {}, "numstat": {}, "statuses": {}, "registry_exact": False,
                     "authority_equal": True, "registry_equal": before[P.REGISTRY] == after[P.REGISTRY]}
            return P.pr_evaluation(*snapshots, facts)

    def test_r2_01_state_and_ratchet_changes_cannot_delete_live_transition(self):
        before = self.files(state="closed", transition=True)
        after = self.files(stamp="2026-09-07T00:00:00Z")
        selector, errors = self.evaluate(before, after)
        self.assertEqual(selector, "pr_ledger_repair")
        self.assertIn("E6", "; ".join(errors))

    def test_r2_02_metadata_first_does_not_launder_later_transition_deletion(self):
        before = self.files(state="closed", transition=True)
        intermediate = self.files(transition=True)
        self.assertEqual(self.evaluate(before, intermediate), ("pr_ledger_repair", []))
        selector, errors = self.evaluate(intermediate, self.files())
        self.assertEqual(selector, "pr_ledger_repair")
        self.assertIn("E6", "; ".join(errors))

    def test_r2_10_fresh_metadata_only_and_deadline_repair_pass_real_snapshot(self):
        before = self.files()
        for after in (self.files(stamp="2026-09-07T00:00:00Z"), self.files(deadline=NEW, history=marker())):
            self.assertEqual(self.evaluate(before, after), ("pr_ledger_repair", []))

    def test_r2_11_exact_30_day_boundary_and_invalid_timestamps(self):
        boundary = datetime(2026, 10, 2, 11, 41, 35, tzinfo=timezone.utc)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, self.files())
            with mock.patch.object(G, "now_utc", return_value=boundary):
                G.giant_file_snapshot(root)
            for now, stamp, error in ((boundary + timedelta(microseconds=1), STAMP, "older than 30 days"),
                                      (NOW, "2026-09-08T00:00:00Z", "future"), (NOW, "invalid", "YYYY-MM-DD")):
                self.write(root, self.files(stamp=stamp))
                with mock.patch.object(G, "now_utc", return_value=now), self.assertRaisesRegex(G.ParseError, error):
                    G.giant_file_snapshot(root)

    def run_main(self, before, after, now, *, candidate="merge", base="base",
                 head="head", origin="base", checkout=None, parents=None):
        env = {"GFP_EVENT_NAME": "pull_request", "GFP_REPOSITORY": "itismyfield/AgentDesk",
               "GFP_HEAD_REPOSITORY": "itismyfield/AgentDesk", "GFP_CANDIDATE_SHA": candidate,
               "GFP_BASE_SHA": base, "GFP_HEAD_SHA": head}
        lineage = [candidate, base, head] if parents is None else parents
        def git(*args, **kwargs):
            if args[0] in {"status", "fetch"}:
                return ""
            if args[0] == "rev-list":
                return " ".join(lineage) + "\n"
            raise AssertionError(args)
        def oid(ref, suffix="commit"):
            return {"HEAD": checkout or candidate, "origin/main": origin}.get(ref, ref)
        facts = {"changed": {path for path in before.keys() | after.keys() if before.get(path) != after.get(path)}, "additions": 4,
                 "numstat": {}, "statuses": {}}
        with tempfile.TemporaryDirectory() as directory:
            evidence = Path(directory) / "evidence.json"
            with mock.patch.dict(P.os.environ, env, clear=True), mock.patch.multiple(
                    P, EVIDENCE=evidence, git=git, oid=oid,
                    archive=lambda ref, root: self.write(root, before if ref == base else after),
                    diff_facts=lambda *_: copy.deepcopy(facts), movement_ledger=lambda *_: {}), mock.patch.object(
                    G, "now_utc", return_value=now), mock.patch.object(G, "today_utc", return_value=now.date()), mock.patch.object(
                    P, "pr_evaluation", wraps=P.pr_evaluation) as evaluation, redirect_stderr(io.StringIO()):
                rc = P.main()
            return rc, json.loads(evidence.read_text()), evaluation.call_count

    def test_r2_12_stale_base_fails_before_selector_fresh_base_passes(self):
        now = datetime(2026, 10, 2, 11, 41, 35, 1, tzinfo=timezone.utc)
        after = self.files(stamp="2026-10-02T11:41:35Z")
        rc, evidence, calls = self.run_main(self.files(), after, now)
        self.assertEqual((rc, calls), (2, 0))
        self.assertIn("older than 30 days", evidence["reason"])
        rc, evidence, calls = self.run_main(self.files(stamp="2026-09-07T00:00:00Z"), after, now)
        self.assertEqual((rc, calls, evidence["selector"]), (0, 1, "pr_ledger_repair"), evidence)

    def test_main_records_deadline_repair_without_false_source_retirement(self):
        rc, evidence, calls = self.run_main(self.files(), self.files(deadline=NEW, history=marker()), NOW)
        self.assertEqual((rc, calls, evidence["selector"]), (0, 1, "pr_ledger_repair"))
        self.assertEqual(evidence["retired"], [])

    def repair_pair(self):
        return self.files(), self.files(deadline=NEW, history=marker())

    def test_unrelated_main_advance_does_not_invalidate_the_same_candidate(self):
        """An identical, legitimate candidate must not fail merely because main moved.

        Same (candidate, base, head, parents); only the origin/main tip differs.
        Both runs must reach the same verdict with byte-identical attribution.
        """
        before, after = self.repair_pair()
        pinned_rc, pinned, pinned_calls = self.run_main(before, after, NOW, origin="base")
        moved_rc, moved, moved_calls = self.run_main(
            before, after, NOW, origin="4d1e0fa11adcb2e0main-moved-on-without-us")
        self.assertEqual((pinned_rc, pinned_calls), (0, 1), pinned)
        self.assertEqual((moved_rc, moved_calls), (0, 1), moved)
        for field in ("selector", "verdict", "reason", "merge_sha", "event_base_sha",
                      "head_sha", "merge_first_parent", "base_tree", "candidate_tree"):
            self.assertEqual(pinned[field], moved[field], field)
        self.assertNotIn("observed_origin_main_sha", moved)

    def test_spoofed_triple_or_wrong_checkout_is_still_rejected(self):
        """Retained bindings: base spoof, head spoof, and candidate != checked-out HEAD."""
        self.assertFalse(P.provenance_matches("merge", "base", "head", ["merge", "spoof", "head"]))
        self.assertFalse(P.provenance_matches("merge", "base", "head", ["merge", "base", "spoof"]))
        before, after = self.repair_pair()
        for lineage in (["merge", "spoof", "head"], ["merge", "base", "spoof"],
                        ["spoof", "base", "head"], ["merge", "base"]):
            rc, evidence, calls = self.run_main(before, after, NOW, parents=lineage)
            self.assertEqual((rc, calls), (2, 0), evidence)
            self.assertEqual(evidence["reason"],
                             "event/base/head/merge object provenance mismatch", lineage)
        rc, evidence, calls = self.run_main(before, after, NOW, checkout="some-other-commit")
        self.assertEqual((rc, calls), (2, 0), evidence)
        self.assertEqual(evidence["reason"], "candidate SHA is not checked-out HEAD")

    def test_evidence_attribution_is_per_candidate_and_never_shared(self):
        """A new integration candidate gets its own evidence; the old one is not reusable.

        The procedural rule ("a changed candidate invalidates the previous CI
        result") is enforceable only because every attribution field names the
        candidate it was produced from. Pin that: no field bleeds between runs.
        """
        before, after = self.repair_pair()
        attribution = ("merge_sha", "merge_first_parent", "head_sha", "event_base_sha",
                       "base_tree", "candidate_tree")
        first = self.run_main(before, after, NOW, candidate="cand1", base="base1", head="head1")[1]
        second = self.run_main(before, after, NOW, candidate="cand2", base="base2", head="head2")[1]
        self.assertEqual(tuple(first[field] for field in attribution),
                         ("cand1", "base1", "head1", "base1", "base1", "cand1"), first)
        self.assertEqual(tuple(second[field] for field in attribution),
                         ("cand2", "base2", "head2", "base2", "base2", "cand2"), second)
        for field in attribution:
            self.assertNotEqual(first[field], second[field], field)

    def test_r2_14_exhausted_keep_reclassification_and_overdue_remain_blocked(self):
        history = marker("2026-04-30", "2026-06-30") + marker("2026-06-30", OLD)
        before = self.files(history=history)
        after = copy.deepcopy(before)
        after[P.REGISTRY] = after[P.REGISTRY].replace('decision = "shrink"', 'decision = "keep"').replace(
            f'deadline = "{OLD}"\n', '').replace('decompose_issue = "#4712"', 'keep_reason = "retained API"')
        selector, errors = self.evaluate(before, after)
        self.assertEqual(selector, "pr_strict_progress")
        self.assertTrue(errors)
        module = G.ModuleEntry(LEDGER_ROOT, LEDGER_ROOT, 1200, 1200, 0, ("giant-file",))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, before)
            with mock.patch.multiple(G, GIANT_FILE_REGISTRY=root / P.REGISTRY,
                                     GIANT_FILE_ISSUE_METADATA=root / P.METADATA,
                                     GIANT_FILE_CLOSED_ISSUE_TRANSITION_LIST=root / P.TRANSITION), mock.patch.object(
                    G, "now_utc", return_value=NOW), self.assertRaisesRegex(G.ParseError, "overdue"):
                G.build_giant_registrations([module], evaluation_date=NOW.date())

    def assert_pin_input_failure(self, before, after, context):
        rc, evidence, calls = self.run_main(before, after, NOW)
        self.assertEqual((rc, calls), (2, 0), evidence)
        for key, expected in {"selector": "pr_strict_progress", "selected": True,
                              "ordinary_problem_count": 1, "verdict": "fail"}.items():
            self.assertEqual(evidence[key], expected, evidence)
        self.assertIn("invalid giant-file pins", evidence["reason"])
        self.assertIn(context, evidence["reason"])

    def test_r3_real_pin_representations_and_invalid_inputs_fail_in_both_archives(self):
        canonical = (P.ROOT / P.GIANT_PIN).read_text(encoding="utf-8")
        pins = P.parse_cap_table(canonical, "giant_file_ratchet")
        first, cap = next(iter(pins.items()))
        representations = [
            canonical.replace("[giant_file_ratchet]", '["giant_file_ratchet"]'),
            canonical.replace("[giant_file_ratchet]", "[ giant_file_ratchet ]"),
            canonical.replace(f'\n"{first}" =', f"\n'{first}' =", 1),
            "[giant_file_ratchet]\n" + "".join(f"'{path}' = {cap}\n" for path, cap in pins.items()),
            canonical.replace(f'\n"{first}" = {cap}', f'\n"{first}" = +{cap}', 1),
        ]
        for text in representations:
            self.assertEqual(P.parse_cap_table(text, "giant_file_ratchet"), pins)
        invalid = representations + [
            "", "# only a comment\n", "[giant_file_ratchet]\n", "[other]\n",
            '[giant_file_ratchet\n', '[giant_file_ratchet]\n[giant_file_ratchet]\n',
            '[giant_file_ratchet]\n"x" = 1000\n"x" = 1000\n',
            '[giant_file_ratchet]\n"x" = 1000\n"x" = 1001\n',
            '[giant_file_ratchet]\n" src/a.rs " = 1000\n',
            '[giant_file_ratchet]\n"a#b.rs" = 1000\n',
            '[giant_file_ratchet]\n"src/a.rs" = 1_000\n',
            '[giant_file_ratchet]\n' + r'"src/a\u002Ers" = 1000' + '\n',
            *[f'[giant_file_ratchet]\n"x" = {value}\n' for value in
              ("true", "0", "-1", '"1000"', "1.5")],
            None, b"\xff",
        ]
        for side in ("base", "candidate"):
            for index, text in enumerate(invalid):
                with self.subTest(side=side, case=index):
                    before, after = self.files(), self.files(stamp="2026-09-07T00:00:00Z")
                    before[P.GIANT_PIN] = after[P.GIANT_PIN] = canonical
                    target = before if side == "base" else after
                    if text is None:
                        target.pop(P.GIANT_PIN)
                    else:
                        target[P.GIANT_PIN] = text
                    self.assert_pin_input_failure(before, after, f"({side})")
        # The default worktree and candidate remain valid when only base is
        # representation-corrupt: omitting the archived base path must fail.
        from audit_maintainability.checks import giant_file_ratchet
        self.assertEqual(giant_file_ratchet.load_giant_baseline(), pins)
        # Inventory tests replace sys.modules; check production import identity
        # in a fresh interpreter rather than asserting their artificial state.
        subprocess.run([sys.executable, "-B", "-c",
                        "import sys; sys.path.insert(0, 'scripts')\n"
                        "import giant_file_progress as p\n"
                        "from audit_maintainability.checks import giant_files\n"
                        "assert giant_files._INVENTORY is p.inventory\n"],
                       cwd=P.ROOT, check=True, capture_output=True, text=True)

    def test_r3_effective_superset_cannot_be_normalized_away(self):
        from audit_maintainability.checks.giant_file_ratchet import load_giant_baseline
        prefix = f'[giant_file_ratchet]\n"{LEDGER_ROOT}" = 1200\n'
        for section in ("[ other ]", '["other"]', '[a."b"]'):
            before, after = self.files(), self.files()
            before[P.GIANT_PIN] = prefix + f'{section}\n"{OTHER}" = 1200\n'
            after[P.GIANT_PIN] = prefix
            self.assertEqual(P.parse_cap_table(before[P.GIANT_PIN], "giant_file_ratchet"),
                             P.parse_cap_table(after[P.GIANT_PIN], "giant_file_ratchet"))
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                self.write(root, before)
                self.assertEqual(set(load_giant_baseline(root / P.GIANT_PIN)), {LEDGER_ROOT, OTHER})
                # Unpadded [other] is recognized and is NOT an injection.
                self.write(root, {P.GIANT_PIN: before[P.GIANT_PIN].replace(section, "[other]")})
                self.assertEqual(set(load_giant_baseline(root / P.GIANT_PIN)), {LEDGER_ROOT})
            self.assert_pin_input_failure(before, after, "(base)")

    def test_r3_loader_import_is_lazy_and_failures_produce_evidence(self):
        original_import = builtins.__import__
        for failure in (ImportError("loader unavailable"), SyntaxError("loader syntax"),
                        RuntimeError("loader initialization")):
            def import_with_failure(name, *args, **kwargs):
                if name == "audit_maintainability.checks.giant_file_ratchet":
                    raise failure
                return original_import(name, *args, **kwargs)
            with mock.patch.object(builtins, "__import__", side_effect=import_with_failure):
                spec = importlib.util.spec_from_file_location("lazy_progress_probe", P.__file__)
                spec.loader.exec_module(importlib.util.module_from_spec(spec))
                self.assert_pin_input_failure(self.files(), self.files(stamp="2026-09-07T00:00:00Z"),
                                              str(failure))

    def test_r3_real_loaders_preserve_pin_removal_growth_and_measurement_guards(self):
        before = self.files()
        before[RETIRED] = "pub fn production() {}\n" * 860
        before[P.GIANT_PIN] = f'[giant_file_ratchet]\n"{LEDGER_ROOT}" = 1300\n'
        for entries, error in (
                ({RETIRED: 860}, "removed"),
                ({LEDGER_ROOT: 1301}, "increased"),
                ({LEDGER_ROOT: 1300, RETIRED: 861}, "measured"),
                ({LEDGER_ROOT: 1300, "src/missing.rs": 1000}, "measured"),
                ({LEDGER_ROOT: 1200, RETIRED: 860}, None)):
            after = copy.deepcopy(before)
            after[P.GIANT_PIN] = "[giant_file_ratchet]\n" + "".join(
                f'"{path}" = {cap}\n' for path, cap in entries.items())
            selector, errors = self.evaluate(before, after)
            self.assertEqual(selector, "pr_ledger_repair")
            if error:
                self.assertIn(error, "; ".join(errors))
            else:
                self.assertEqual(errors, [])


if __name__ == "__main__":
    unittest.main()
