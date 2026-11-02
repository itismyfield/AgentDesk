"""Skip pin dual reader: legacy sets plus `.d` fragments, both writer gates and the sync writer."""

from __future__ import annotations

import ast
import importlib.util
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from scripts import check_legacy_supervision_census as census
from scripts import prepare_gate_review as prepare
from scripts import skip_pin_registry as registry

ROOT = Path(__file__).resolve().parents[1]
LEGACY = "scripts/test_only_module_skip_pin.py"
SPLIT = "scripts/test_only_module_skip_pin_paths.py"
FRAGMENTS = "scripts/test_only_module_skip_pin.d"
NAMES = ("PINNED_BASENAME_TEST_FILES", "PINNED_RESOLVER_TEST_ONLY_FILES")


def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


durable = _load("skip_pin_fragments_durable_gate", ROOT / "scripts/check_durable_frontier_writer_call_sites.py")
intake = _load("skip_pin_fragments_intake_gate", ROOT / "scripts/check_intake_outbox_done_writer_call_sites.py")


def block(name: str, paths) -> str:
    return f"{name} = frozenset(\n    {{\n" + "".join(f'        "{p}",\n' for p in paths) + "    }\n)\n"


def legacy_text(basename=(), resolver=()) -> str:
    return block(NAMES[0], basename) + block(NAMES[1], resolver)


def literal_sets(text: str) -> tuple[list[str], list[str]]:
    """Present legacy literals as Python evaluates them, element order kept."""
    found = {}
    for node in ast.parse(text).body:
        if isinstance(node, ast.Assign) and getattr(node.targets[0], "id", None) in NAMES:
            found[node.targets[0].id] = [element.value for element in node.value.args[0].elts]
    return found.get(NAMES[0], []), found.get(NAMES[1], [])


def write(root: Path, rel: str, text: str) -> None:
    path = root / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, "utf-8")


def snapshot(root: Path) -> dict[str, bytes]:
    return {p.relative_to(root).as_posix(): p.read_bytes()
            for p in sorted((root / "scripts").rglob("*")) if p.is_file()}


class TempRoot(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        (self.root / FRAGMENTS).mkdir(parents=True)
        write(self.root, FRAGMENTS + "/README.md", "guide\n")

    def fragment(self, name: str, *rows: str) -> None:
        write(self.root, f"{FRAGMENTS}/{name}", "".join(row + "\n" for row in rows))


class LegacyCompatibilityTests(TempRoot):
    def test_split_legacy_reads_its_literal_and_the_module_uses_only_resolver_pins(self) -> None:
        text = (ROOT / SPLIT).read_text("utf-8")
        rows, problems = registry.legacy_rows(text, SPLIT)
        self.assertEqual(problems, [])
        basename, resolver = literal_sets(text)
        self.assertEqual(len(rows), len(basename) + len(resolver))
        by_kind = {kind: frozenset(r.key for r in rows if r.payload == kind) for kind in registry.KINDS}
        self.assertEqual(by_kind, {"basename": frozenset(basename), "resolver": frozenset(resolver)})
        merged = registry.load(ROOT).frozensets()
        self.assertTrue(by_kind["basename"] <= merged[0] and by_kind["resolver"] <= merged[1])
        self.assertTrue(all(row.location.startswith(SPLIT + ":") for row in rows))
        pin = durable._SKIP_PIN
        self.assertEqual(pin.PINNED_RESOLVER_TEST_ONLY_FILES, merged[1])
        self.assertEqual(pin.PINNED_TEST_ONLY_MODULE_FILES, merged[1])

    def test_unsplit_resolver_only_fixture_needs_no_basename_literal(self) -> None:
        write(self.root, LEGACY, block(NAMES[1], ["src/helper.rs"]))
        self.assertEqual(registry.load(self.root).frozensets(), (frozenset(), {"src/helper.rs"}))

    def test_split_literal_is_selected_without_parsing_or_rewriting_its_delegate(self) -> None:
        write(self.root, LEGACY, "PINNED_RESOLVER_TEST_ONLY_FILES = delegate.paths\n")
        write(self.root, SPLIT, block(NAMES[1], ["src/helper.rs"]))
        before = snapshot(self.root)
        self.assertEqual(registry.load(self.root).frozensets(), (frozenset(), {"src/helper.rs"}))
        self.assertEqual(snapshot(self.root), before)

    def test_legacy_only_new_only_and_mixed_layouts_build_the_same_pin(self) -> None:
        basename = ["src/a_tests.rs", "src/b_tests.rs", "src/tests.rs"]
        resolver = ["src/helper.rs", "src/other.rs"]
        text = legacy_text(basename, resolver)
        expected = (frozenset(basename), frozenset(resolver))
        rows = sorted([*(f"basename | {p}" for p in basename), *(f"resolver | {p}" for p in resolver)],
                      key=lambda row: row.partition(" | ")[2])
        layouts = {
            "legacy-only": (text, {}),
            "new-only": (legacy_text(), {"mac-mini-1-a.txt": rows[::3], "mac-mini-1-b.txt": rows[1::3],
                                         "mac-book-2-c.txt": rows[2::3]}),
            "mixed": (legacy_text(basename[::2], resolver[1::2]),
                      {"mac-mini-1-a.txt": [r for r in rows if r.partition(" | ")[2] in
                                            {*basename[1::2], *resolver[::2]}]}),
        }
        for name, (legacy, fragments) in layouts.items():
            with self.subTest(layout=name):
                shutil.rmtree(self.root / FRAGMENTS)
                (self.root / FRAGMENTS).mkdir()
                write(self.root, LEGACY, legacy)
                for file, lines in fragments.items():
                    self.fragment(file, *lines)
                self.assertEqual(registry.load(self.root).frozensets(), expected)

    def test_legacy_order_is_allowed_but_a_fragment_must_be_sorted(self) -> None:
        write(self.root, LEGACY, legacy_text(["src/z_tests.rs", "src/a_tests.rs"]))
        self.assertEqual(registry.load(self.root).frozensets()[0], {"src/z_tests.rs", "src/a_tests.rs"})
        self.fragment("mac-mini-1-p1.txt", "basename | src/y_tests.rs", "basename | src/b_tests.rs")
        with self.assertRaisesRegex(RuntimeError, r"mac-mini-1-p1\.txt:2: rows must be unique and sorted"):
            registry.load(self.root)


class FailClosedTests(TempRoot):
    def assert_red(self, *needles: str) -> None:
        with self.assertRaises(RuntimeError) as raised:
            registry.load(self.root)
        for needle in needles:
            self.assertIn(needle, str(raised.exception))

    def test_duplicates_and_cross_kind_conflicts_name_both_locations(self) -> None:
        write(self.root, LEGACY, legacy_text(["src/a_tests.rs"], ["src/helper.rs"]))
        self.fragment("mac-mini-1-p1.txt", "basename | src/a_tests.rs", "basename | src/helper.rs")
        self.assert_red(
            f"{FRAGMENTS}/mac-mini-1-p1.txt:1: duplicate of {LEGACY}:3 for `src/a_tests.rs`",
            f"{FRAGMENTS}/mac-mini-1-p1.txt:2: conflicts with {LEGACY}:8 for `src/helper.rs`",
        )

    def test_the_same_row_in_two_fragments_is_a_duplicate(self) -> None:
        write(self.root, LEGACY, legacy_text())
        self.fragment("mac-mini-1-p1.txt", "basename | src/a_tests.rs")
        self.fragment("mac-book-2-p1.txt", "basename | src/a_tests.rs")
        self.assert_red(f"{FRAGMENTS}/mac-mini-1-p1.txt:1: duplicate of {FRAGMENTS}/mac-book-2-p1.txt:1")

    def test_a_legacy_copy_the_frozenset_would_drop_is_a_duplicate(self) -> None:
        write(self.root, LEGACY, legacy_text(["src/a_tests.rs", "src/a_tests.rs"]))
        self.assert_red(f"{LEGACY}:4: duplicate of {LEGACY}:3")

    def test_split_duplicates_and_cross_kind_conflicts_name_the_split_origin(self) -> None:
        write(self.root, LEGACY, "PINNED_RESOLVER_TEST_ONLY_FILES = delegate.paths\n")
        write(self.root, SPLIT, block(NAMES[1], ["src/a.rs", "src/helper.rs"]))
        self.fragment("mac-mini-1-p1.txt", "resolver | src/a.rs", "basename | src/helper.rs")
        self.assert_red(
            f"{FRAGMENTS}/mac-mini-1-p1.txt:1: duplicate of {SPLIT}:3 for `src/a.rs`",
            f"{FRAGMENTS}/mac-mini-1-p1.txt:2: conflicts with {SPLIT}:4 for `src/helper.rs`",
        )

    def test_split_duplicate_literal_rows_and_assignments_fail(self) -> None:
        write(self.root, SPLIT, block(NAMES[1], ["src/a.rs", "src/a.rs"]))
        self.assert_red(f"{SPLIT}:4: duplicate of {SPLIT}:3")
        write(self.root, SPLIT, block(NAMES[1], []) * 2)
        self.assert_red(f"{NAMES[1]} must be assigned once, found 2")

    def test_non_literal_or_malformed_legacy_entries_fail(self) -> None:
        write(self.root, LEGACY, legacy_text(["src/a_tests.rs"]).replace(
            '"src/a_tests.rs"', '"src/" + "a_tests.rs", "src/../b_tests.rs"'))
        self.assert_red("entries must be string literals", "`src/../b_tests.rs` is no normalized")
        write(self.root, LEGACY, legacy_text() + block(NAMES[0], []))
        self.assert_red(f"{NAMES[0]} must be assigned once, found 2")

    def test_a_misnamed_fragment_fails_instead_of_hiding_its_rows(self) -> None:
        write(self.root, LEGACY, legacy_text(["src/a_tests.rs"]))
        self.fragment("mac-mini-1-p1.text", "resolver | src/a_tests.rs")
        self.assert_red(f"{FRAGMENTS}/mac-mini-1-p1.text: a fragment name must read")

    def test_malformed_fragment_rows_fail(self) -> None:
        write(self.root, LEGACY, legacy_text())
        self.fragment("mac-mini-1-p1.txt", "basename|src/a_tests.rs", "module | src/b.rs",
                      "basename | lib/c_tests.rs", "basename | src/d tests.rs")
        self.assert_red(":1: expected `basename", ":2: expected `basename",
                        ":3: `lib/c_tests.rs` is no normalized", ":4: `src/d tests.rs` is no normalized")

    def test_invalid_fragments_fail_the_pin_import_in_a_fresh_process(self) -> None:
        for name in ("test_only_module_skip_pin.py", "skip_pin_registry.py", "rust_lex.py",
                     "registration_fragments.py", "generate_inventory_docs.py"):
            shutil.copy2(ROOT / "scripts" / name, self.root / "scripts" / name)
        write(self.root, SPLIT, block(NAMES[1], ["src/helper.rs"]))
        self.fragment("mac-mini-1-base.txt", "basename | src/a_tests.rs")
        self.fragment("mac-mini-1-p1.txt", "resolver | src/b.rs")
        probe = [sys.executable, "-c", "import sys; sys.path.insert(0, 'scripts'); "
                 "import test_only_module_skip_pin as p; print(*sorted(p.PINNED_TEST_ONLY_MODULE_FILES))"]
        result = subprocess.run(probe, cwd=self.root, capture_output=True, text=True)
        self.assertEqual((result.returncode, result.stdout.split()), (0, ["src/b.rs", "src/helper.rs"]))
        self.fragment("mac-book-2-p1.txt", "basename | src/b.rs")
        result = subprocess.run(probe, cwd=self.root, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("conflicts with", result.stderr)


class WriterGateVerdictTests(TempRoot):
    """Both writer gates judge whatever pin the copied module builds from one layout."""

    def setUp(self) -> None:
        super().setUp()
        write(self.root, "src/main.rs", "mod prod;\n#[cfg(test)]\nmod helper;\nfn main() {}\n")
        for rel in ("src/prod.rs", "src/helper.rs", "src/alpha_tests.rs", "src/beta/tests.rs"):
            write(self.root, rel, "fn fixture() {}\n")
        for name in ("skip_pin_registry.py", "registration_fragments.py", "generate_inventory_docs.py", "rust_lex.py"):
            shutil.copy2(ROOT / "scripts" / name, self.root / "scripts" / name)
        self.real = (ROOT / LEGACY).read_text("utf-8")
        self.loads = 0

    def verdicts(self, resolver=(), **fragments):
        shutil.rmtree(self.root / FRAGMENTS)
        (self.root / FRAGMENTS).mkdir()
        write(self.root, LEGACY, self.real)
        write(self.root, SPLIT, block(NAMES[1], resolver))
        for name, rows in fragments.items():
            self.fragment(name.replace("_", "-") + ".txt", *rows)
        self.loads += 1
        pin = _load(f"skip_pin_layout_{self.loads}", self.root / LEGACY).PINNED_TEST_ONLY_MODULE_FILES
        zero = {"EXPECTED_CALL_SITES": {s: {} for s in durable.EXPECTED_CALL_SITES},
                "EXPECTED_RAW_ATOMIC_MUTATIONS": {}, "EXPECTED_BARE_REFERENCES": {},
                "EXPECTED_PINNED_USE_ALIASES": {}, "EXPECTED_DEFINITION_FILE_COUNTS": {}}
        with patch.multiple(durable, **zero):
            return (durable.check(self.root, pinned_test_only_files=pin),
                    intake.check(self.root, {}, pinned_test_only_files=pin))

    def test_layouts_give_one_verdict_and_a_missing_or_stale_row_fails_both_gates(self) -> None:
        legacy_only = self.verdicts(["src/helper.rs"])
        self.assertTrue(all(ok for ok, _ in legacy_only), legacy_only)
        new_only = self.verdicts(mac_mini_1_p1=["basename | src/alpha_tests.rs", "basename | src/beta/tests.rs"],
                                 mac_book_2_p1=["resolver | src/helper.rs"])
        mixed = self.verdicts(["src/helper.rs"],
                              mac_mini_1_p1=["basename | src/alpha_tests.rs"])
        self.assertEqual(new_only, legacy_only)
        self.assertEqual(mixed, legacy_only)
        missing = self.verdicts()
        stale = self.verdicts(["src/helper.rs"], mac_mini_1_p1=["resolver | src/gone.rs"])
        for verdicts, needle in ((missing, "scan-only (newly skipped): src/helper.rs"),
                                 (stale, "pin-only (no longer skipped): src/gone.rs")):
            for ok, message in verdicts:
                self.assertFalse(ok)
                self.assertIn(needle, message)
                self.assertIn("scripts/test_only_module_skip_pin.d/", message)

    def test_removing_a_pinned_helpers_test_cfg_fails_both_writer_gates(self) -> None:
        pinned = ["src/helper.rs"]
        before = self.verdicts(pinned)
        self.assertTrue(all(ok for ok, _ in before), before)
        write(self.root, "src/main.rs", "mod prod;\nmod helper;\nfn main() {}\n")
        after = self.verdicts(pinned)
        for ok, message in after:
            self.assertFalse(ok, "a formerly test-only helper must enter the writer scan")
            self.assertIn("pin-only (no longer skipped): src/helper.rs", message)


class SyncWriterTests(TempRoot):
    def setUp(self) -> None:
        super().setUp()
        self.lib("legacy_helper", "frag_helper")
        for name in ("prod", "legacy_helper", "frag_helper", "old_tests"):
            write(self.root, f"src/{name}.rs", "fn fixture() {}\n")
        write(self.root, LEGACY, legacy_text(["src/old_tests.rs"], ["src/legacy_helper.rs"]))
        self.fragment("mac-mini-1-p1.txt", "# reviewed fixture", "resolver | src/frag_helper.rs")

    def lib(self, *test_only: str, production=("prod",)) -> None:
        write(self.root, "src/lib.rs", "".join(f"mod {m};\n" for m in production)
              + "".join(f"#[cfg(test)]\nmod {m};\n" for m in test_only))

    def sync(self, promoted=(), fragment=None, *, deleted=()):
        return census.sync_skips(self.root, promoted, deleted_paths=deleted, fragment=fragment)

    def test_a_clean_tree_writes_nothing(self) -> None:
        before = snapshot(self.root)
        self.assertEqual(self.sync(), ([], []))
        self.assertEqual(snapshot(self.root), before)

    def test_an_unnamed_sync_gives_each_new_path_its_own_fragment_and_keeps_the_legacy_bytes(self) -> None:
        write(self.root, "src/new_helper.rs", "")
        write(self.root, "src/other_helper.rs", "")
        self.lib("legacy_helper", "frag_helper", "new_helper", "other_helper")
        legacy = (self.root / LEGACY).read_bytes()
        manual, written = self.sync()
        self.assertEqual(manual, [])
        self.assertEqual(written, sorted(f"{FRAGMENTS}/{registry.auto_fragment(p)}"
                                         for p in ("src/new_helper.rs", "src/other_helper.rs")))
        self.assertEqual((self.root / written[0]).read_text().count("\n"), 1)
        self.assertEqual((self.root / LEGACY).read_bytes(), legacy)
        self.assertIn("src/new_helper.rs", registry.load(self.root).rows)
        before = snapshot(self.root)
        self.assertEqual(self.sync(), ([], []))
        self.assertEqual(snapshot(self.root), before)

    def test_a_named_sync_inserts_sorted_rows_and_keeps_comments(self) -> None:
        write(self.root, "src/a_tests.rs", "")
        write(self.root, "src/zz_tests.rs", "")
        for name in ("a_helper", "new_helper", "zz_helper"):
            write(self.root, f"src/{name}.rs", "")
        self.lib("legacy_helper", "frag_helper", "a_helper", "new_helper", "zz_helper")
        legacy = (self.root / LEGACY).read_bytes()
        self.assertEqual(self.sync(fragment="mac-mini-1-p1"), ([], [f"{FRAGMENTS}/mac-mini-1-p1.txt"]))
        self.assertEqual((self.root / FRAGMENTS / "mac-mini-1-p1.txt").read_text(), (
            "# reviewed fixture\nresolver | src/a_helper.rs\nresolver | src/frag_helper.rs\n"
            "resolver | src/new_helper.rs\nresolver | src/zz_helper.rs\n"
        ))
        self.assertEqual((self.root / LEGACY).read_bytes(), legacy)

    def test_new_basename_rule_files_need_no_sync_rows(self) -> None:
        write(self.root, "src/new_tests.rs", "")
        write(self.root, "src/deep/tests.rs", "")
        before = snapshot(self.root)
        self.assertEqual(self.sync(), ([], []))
        self.assertEqual(snapshot(self.root), before)
        self.assertNotIn("src/new_tests.rs", registry.load(self.root).rows)

    def test_registry_sync_itself_never_adds_a_new_basename_row(self) -> None:
        before = snapshot(self.root)
        pin = registry.load(self.root)
        measured = {path: row.payload for path, row in pin.rows.items()}
        measured["src/new_tests.rs"] = "basename"
        self.assertEqual(registry.sync(self.root, measured, set(measured)), ([], []))
        self.assertEqual(snapshot(self.root), before)

    def test_a_legacy_promotion_returns_its_manual_step_and_writes_nothing(self) -> None:
        self.lib("frag_helper", production=("prod", "legacy_helper"))
        write(self.root, "src/new_tests.rs", "")
        before = snapshot(self.root)
        line = legacy_text(["src/old_tests.rs"], ["src/legacy_helper.rs"]).splitlines().index(
            '        "src/legacy_helper.rs",') + 1
        manual, written = self.sync(["src/legacy_helper.rs"], "mac-mini-1-p1")
        self.assertEqual(written, [])
        self.assertEqual(len(manual), 1)
        self.assertTrue(manual[0].startswith(f"{LEGACY}:{line}: delete the legacy pin `src/legacy_helper.rs` by hand"))
        self.assertEqual(snapshot(self.root), before)
        result = subprocess.run([sys.executable, str(ROOT / "scripts/check_legacy_supervision_census.py"),
                                 "--sync-skips", "--promote", "src/legacy_helper.rs"],
                                cwd=self.root, capture_output=True, text=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(f"{LEGACY}:{line}: delete the legacy pin", result.stderr)
        self.assertEqual(snapshot(self.root), before)

    def test_a_fragment_promotion_deletes_only_its_row(self) -> None:
        self.fragment("mac-mini-1-p1.txt", "# reviewed fixture", "resolver | src/frag_helper.rs",
                      "basename | src/kept_tests.rs")
        write(self.root, "src/kept_tests.rs", "")
        self.lib("legacy_helper", production=("prod", "frag_helper"))
        legacy = (self.root / LEGACY).read_bytes()
        self.assertEqual(self.sync(["src/frag_helper.rs"]), ([], [f"{FRAGMENTS}/mac-mini-1-p1.txt"]))
        self.assertEqual((self.root / FRAGMENTS / "mac-mini-1-p1.txt").read_text(),
                         "# reviewed fixture\nbasename | src/kept_tests.rs\n")
        (self.root / "src/kept_tests.rs").unlink()
        write(self.root, "src/lib.rs", "mod prod;\nmod frag_helper;\n#[cfg(test)]\nmod legacy_helper;\nmod kept;\n")
        write(self.root, "src/kept.rs", "")
        self.fragment("mac-mini-1-p1.txt", "resolver | src/kept.rs")
        self.assertEqual(self.sync(["src/kept.rs"]), ([], [f"{FRAGMENTS}/mac-mini-1-p1.txt"]))
        self.assertFalse((self.root / FRAGMENTS / "mac-mini-1-p1.txt").exists())
        self.assertEqual((self.root / LEGACY).read_bytes(), legacy)

    def test_a_fragment_deletion_removes_only_its_row_and_then_the_empty_fragment(self) -> None:
        self.fragment("mac-mini-1-p1.txt", "# reviewed fixture", "resolver | src/frag_helper.rs",
                      "resolver | src/kept.rs")
        write(self.root, "src/kept.rs", "")
        (self.root / "src/frag_helper.rs").unlink()
        self.lib("legacy_helper", "kept")
        legacy = (self.root / LEGACY).read_bytes()
        self.assertEqual(self.sync(deleted=["src/frag_helper.rs"]),
                         ([], [f"{FRAGMENTS}/mac-mini-1-p1.txt"]))
        self.assertEqual((self.root / FRAGMENTS / "mac-mini-1-p1.txt").read_text(),
                         "# reviewed fixture\nresolver | src/kept.rs\n")
        (self.root / "src/kept.rs").unlink()
        self.lib("legacy_helper")
        self.assertEqual(self.sync(deleted=["src/kept.rs"]), ([], [f"{FRAGMENTS}/mac-mini-1-p1.txt"]))
        self.assertFalse((self.root / FRAGMENTS / "mac-mini-1-p1.txt").exists())
        self.assertEqual((self.root / LEGACY).read_bytes(), legacy)

    def test_a_split_legacy_deletion_is_manual_and_never_rewrites_its_delegate(self) -> None:
        write(self.root, LEGACY, "PINNED_RESOLVER_TEST_ONLY_FILES = delegate.paths\n")
        write(self.root, SPLIT, block(NAMES[1], ["src/legacy_helper.rs"]))
        (self.root / "src/legacy_helper.rs").unlink()
        write(self.root, "src/new_helper.rs", "")
        self.lib("frag_helper", "new_helper")
        before = snapshot(self.root)
        manual, written = self.sync(fragment="mac-mini-1-p1", deleted=["src/legacy_helper.rs"])
        self.assertEqual(written, [])
        self.assertEqual(manual, [f"{SPLIT}:3: delete the legacy pin `src/legacy_helper.rs` by hand; "
                                  f"--sync-skips never rewrites {SPLIT}"])
        self.assertEqual(snapshot(self.root), before)

    def test_a_split_legacy_promotion_is_manual_and_never_rewrites_its_delegate(self) -> None:
        write(self.root, LEGACY, "PINNED_RESOLVER_TEST_ONLY_FILES = delegate.paths\n")
        write(self.root, SPLIT, block(NAMES[1], ["src/legacy_helper.rs"]))
        self.lib("frag_helper", production=("prod", "legacy_helper"))
        before = snapshot(self.root)
        manual, written = self.sync(["src/legacy_helper.rs"])
        self.assertEqual(written, [])
        self.assertEqual(len(manual), 1)
        self.assertTrue(manual[0].startswith(f"{SPLIT}:3: delete the legacy pin"))
        self.assertEqual(snapshot(self.root), before)

    def test_deletion_refusals_leave_every_file_unchanged(self) -> None:
        for deleted, needle in ((["src/frag_helper.rs"], "deletion path still exists"),
                                (["src/absent.rs"], "deletion is not pinned")):
            with self.subTest(deleted=deleted):
                before = snapshot(self.root)
                with self.assertRaisesRegex(RuntimeError, needle):
                    self.sync(deleted=deleted)
                self.assertEqual(snapshot(self.root), before)
        (self.root / "src/frag_helper.rs").unlink()
        (self.root / "src/frag_helper.rs").symlink_to("missing.rs")
        before = snapshot(self.root)
        with self.assertRaisesRegex(RuntimeError, "symlink"):
            self.sync(deleted=["src/frag_helper.rs"])
        self.assertEqual(snapshot(self.root), before)

    def test_refusals_leave_every_file_unchanged(self) -> None:
        self.lib("legacy_helper", production=("prod", "frag_helper"))
        write(self.root, "src/new_tests.rs", "")
        cases = {
            "missing": (["src/frag_helper.rs", "src/absent.rs"], None, "promotion path missing: src/absent.rs"),
            "still test-only": (["src/legacy_helper.rs"], None, "promotion remains test-only"),
            "not pinned": (["src/frag_helper.rs", "src/prod.rs"], None, "promotion is not pinned: src/prod.rs"),
            "removal without promotion": ([], None, "refusing pin removal"),
            "fragment name": (["src/frag_helper.rs"], "Mac Mini", "must read"),
        }
        for name, (promoted, fragment, needle) in cases.items():
            with self.subTest(case=name):
                before = snapshot(self.root)
                with self.assertRaisesRegex(RuntimeError, needle):
                    self.sync(promoted, fragment)
                self.assertEqual(snapshot(self.root), before)
        self.fragment("mac-book-2-p1.txt", "resolver | src/legacy_helper.rs")
        before = snapshot(self.root)
        with self.assertRaisesRegex(RuntimeError, "duplicate of"):
            self.sync(["src/frag_helper.rs"])
        self.assertEqual(snapshot(self.root), before)

    def test_the_cli_without_a_fragment_never_rewrites_the_legacy_file(self) -> None:
        write(self.root, "src/new_helper.rs", "")
        self.lib("legacy_helper", "frag_helper", "new_helper")
        legacy = (self.root / LEGACY).read_bytes()
        result = subprocess.run([sys.executable, str(ROOT / "scripts/check_legacy_supervision_census.py"),
                                 "--sync-skips"], cwd=self.root, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        name = registry.auto_fragment("src/new_helper.rs")
        self.assertEqual(result.stdout.strip(), f"skip pin fragment written: {FRAGMENTS}/{name}")
        self.assertEqual((self.root / FRAGMENTS / name).read_text(), "resolver | src/new_helper.rs\n")
        self.assertEqual((self.root / LEGACY).read_bytes(), legacy)

    def test_review_preparation_passes_a_named_fragment_to_the_sync_only(self) -> None:
        with patch.object(prepare.subprocess, "run", return_value=SimpleNamespace(returncode=0)) as run:
            self.assertEqual(prepare.prepare(ROOT, "mac-mini-1-p1"), 0)
        tails = [call.args[0][2:] for call in run.call_args_list]
        self.assertEqual(tails[0], ["--sync-skips", "--fragment", "mac-mini-1-p1"])
        self.assertNotIn("--fragment", [arg for tail in tails[1:] for arg in tail])


if __name__ == "__main__":
    unittest.main()
