"""Mutation proof for scripts/check_reachability_row_independence.py (#5071 T4-B1).

The gate is the machine half of 4987 §-1.5 I14 and §9.4. A gate that only ever
runs green on the real tree proves nothing, so every check it makes is exercised
here against a synthetic repo root that reproduces exactly one defect at a time:

  * the three shapes an inflight dependency can take from inside the tree
    (`use` of a qualified path, a bare segment in a brace list, and an inline
    fully-qualified call with no `use` at all);
  * the shapes that must NOT trip it — the same text inside a comment or a
    string literal, because this tree's own module docs discuss "the inflight
    row" in prose and a lint that reds on its own documentation gets deleted;
  * the fail-closed path: source the shared neutralizer
    (`scripts/check_clippy_allow_ratchet.py`, reused rather than forked) cannot
    lex is reported, not silently passed;
  * every change-surface requirement: the section's existence, its canonical
    globs, its required statements, ghost paths, and a tree file no declared
    glob covers.

The live-repo cases at the end pin that the gate is actually wired into
`scripts/ci-script-checks.sh` and that the checked-in tree passes it.
"""

from __future__ import annotations

import importlib.util
import sys
import textwrap
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / "scripts" / "check_reachability_row_independence.py"
CI_SCRIPT = REPO_ROOT / "scripts" / "ci-script-checks.sh"

_SPEC = importlib.util.spec_from_file_location(
    "check_reachability_row_independence", SCRIPT_PATH
)
GATE = importlib.util.module_from_spec(_SPEC)
assert _SPEC.loader is not None
sys.modules[_SPEC.name] = GATE
_SPEC.loader.exec_module(GATE)

CLEAN_ROOT_FILE = """\
//! Reachability library root. Prose may discuss the inflight row freely.
#![allow(dead_code)]

pub(in crate::services::discord) mod discovery;
"""

CLEAN_CHILD = """\
//! Resolution ladder.

use std::path::Path;

pub(in crate::services::discord) fn resolve(path: &Path) -> bool {
    path.is_file()
}
"""

GOOD_SURFACE = """\
# Change Surfaces

### `relay_reachability`

- canonical_modules: `src/services/discord/health/reachability.rs` and
  everything under `src/services/discord/health/reachability/**`.
- companion edits: changing the obligation rule requires changing
  `scripts/relay_watchdog.py` in the same PR.
- non_guarantees: this is a source lint, not a type proof.

### `something_else`

- canonical_modules: `src/other.rs`.
"""


def _build_root(
    tmp: str,
    *,
    root_file: str = CLEAN_ROOT_FILE,
    child: str | None = CLEAN_CHILD,
    surface: str | None = GOOD_SURFACE,
    extra_files: dict[str, str] | None = None,
) -> Path:
    root = Path(tmp)
    tree = root / GATE.TREE_DIR
    tree.mkdir(parents=True, exist_ok=True)
    (root / GATE.TREE_ROOT_FILE).write_text(root_file, encoding="utf-8")
    if child is not None:
        (tree / "discovery.rs").write_text(child, encoding="utf-8")
    for rel, body in (extra_files or {}).items():
        target = root / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(body, encoding="utf-8")
    doc = root / GATE.CHANGE_SURFACES_DOC
    doc.parent.mkdir(parents=True, exist_ok=True)
    if surface is not None:
        doc.write_text(surface, encoding="utf-8")
    return root


def _kinds(violations) -> list[str]:
    return [violation.kind for violation in violations]


class CleanFixtureTest(unittest.TestCase):
    def test_clean_tree_and_surface_produce_no_violation(self) -> None:
        with TemporaryDirectory() as tmp:
            self.assertEqual(GATE.run(_build_root(tmp)), [])


class RowIndependenceDetectionTest(unittest.TestCase):
    """The three ways the tree can reach the inflight row from inside itself."""

    def test_qualified_use_of_inflight_is_flagged(self) -> None:
        body = textwrap.dedent(
            """\
            use crate::services::discord::inflight::InflightTurnState;

            fn probe(_state: InflightTurnState) {}
            """
        )
        with TemporaryDirectory() as tmp:
            violations = GATE.run(_build_root(tmp, child=body))
        self.assertEqual(_kinds(violations), ["inflight-path"])

    def test_bare_inflight_segment_in_a_brace_list_is_flagged(self) -> None:
        """`use ...::{inflight, health};` never spells `inflight::`."""

        body = "use crate::services::discord::{health, inflight};\n"
        with TemporaryDirectory() as tmp:
            violations = GATE.run(_build_root(tmp, child=body))
        self.assertEqual(_kinds(violations), ["inflight-import"])

    def test_trailing_inflight_segment_is_flagged(self) -> None:
        """`use ...::inflight;` never spells `inflight::` either."""

        body = "use crate::services::discord::inflight;\n"
        with TemporaryDirectory() as tmp:
            violations = GATE.run(_build_root(tmp, child=body))
        self.assertEqual(_kinds(violations), ["inflight-import"])

    def test_renamed_inflight_import_is_flagged(self) -> None:
        """An `as` rename laundering the module is still visible from here."""

        body = "use crate::services::discord::inflight as rows;\n"
        with TemporaryDirectory() as tmp:
            violations = GATE.run(_build_root(tmp, child=body))
        self.assertEqual(_kinds(violations), ["inflight-import"])

    def test_inline_fully_qualified_path_without_a_use_is_flagged(self) -> None:
        """A `use`-only lint is bypassed by writing the path at the call site."""

        body = textwrap.dedent(
            """\
            fn probe() {
                let _ = crate::services::discord::inflight::load_row();
            }
            """
        )
        with TemporaryDirectory() as tmp:
            violations = GATE.run(_build_root(tmp, child=body))
        self.assertEqual(_kinds(violations), ["inflight-path"])

    def test_module_root_file_is_scanned_too(self) -> None:
        """`health.rs` + `health/` layout: the root is a sibling, not a mod.rs."""

        with TemporaryDirectory() as tmp:
            violations = GATE.run(
                _build_root(
                    tmp,
                    root_file="use crate::services::discord::inflight::Row;\n",
                )
            )
        self.assertEqual(_kinds(violations), ["inflight-path"])
        self.assertTrue(violations[0].path.endswith("reachability.rs"))

    def test_nested_child_module_is_scanned(self) -> None:
        with TemporaryDirectory() as tmp:
            violations = GATE.run(
                _build_root(
                    tmp,
                    extra_files={
                        f"{GATE.TREE_DIR}/nested/deep.rs": (
                            "use crate::services::discord::inflight::Row;\n"
                        )
                    },
                )
            )
        self.assertEqual(_kinds(violations), ["inflight-path"])


class FalsePositiveTest(unittest.TestCase):
    """Prose and literals are not dependencies."""

    def test_inflight_in_line_and_doc_comments_is_not_flagged(self) -> None:
        body = textwrap.dedent(
            """\
            //! Nothing here may import crate::services::discord::inflight::Row.
            /// See inflight::store for the row this module refuses to read.
            // use crate::services::discord::inflight::Row;

            fn probe() {}
            """
        )
        with TemporaryDirectory() as tmp:
            self.assertEqual(GATE.run(_build_root(tmp, child=body)), [])

    def test_inflight_in_a_block_comment_is_not_flagged(self) -> None:
        body = textwrap.dedent(
            """\
            /* use crate::services::discord::inflight::Row;
               /* nested */
            */
            fn probe() {}
            """
        )
        with TemporaryDirectory() as tmp:
            self.assertEqual(GATE.run(_build_root(tmp, child=body)), [])

    def test_inflight_in_string_literals_is_not_flagged(self) -> None:
        body = textwrap.dedent(
            """\
            fn probe() -> &'static str {
                let _raw = r#"use crate::services::discord::inflight::Row;"#;
                "inflight::store is not imported here"
            }
            """
        )
        with TemporaryDirectory() as tmp:
            self.assertEqual(GATE.run(_build_root(tmp, child=body)), [])

    def test_a_lifetime_does_not_derail_the_neutralizer(self) -> None:
        """`'a` is not a char literal; mis-lexing it would blank real code."""

        body = textwrap.dedent(
            """\
            struct Holder<'a> {
                name: &'a str,
            }

            fn probe(_h: Holder<'_>) {
                let _ = crate::services::discord::inflight::load_row();
            }
            """
        )
        with TemporaryDirectory() as tmp:
            violations = GATE.run(_build_root(tmp, child=body))
        self.assertEqual(_kinds(violations), ["inflight-path"])

    def test_a_similarly_named_module_is_not_flagged(self) -> None:
        body = "use crate::services::discord::inflight_heartbeat_sweeper::sweep;\n"
        with TemporaryDirectory() as tmp:
            self.assertEqual(GATE.run(_build_root(tmp, child=body)), [])

    def test_a_file_that_does_not_lex_fails_closed(self) -> None:
        """Text the neutralizer could not lex is not scannable, so it is not
        silently declared clean — the shared pre-pass reports the ambiguity and
        this gate turns it into a violation."""

        body = "/* unterminated block comment\nfn probe() {}\n"
        with TemporaryDirectory() as tmp:
            violations = GATE.run(_build_root(tmp, child=body))
        self.assertEqual(_kinds(violations), ["unlexable"])


class ChangeSurfaceOwnershipTest(unittest.TestCase):
    def test_missing_surface_section_fails(self) -> None:
        with TemporaryDirectory() as tmp:
            violations = GATE.run(_build_root(tmp, surface="# Change Surfaces\n"))
        self.assertEqual(_kinds(violations), ["missing-surface"])

    def test_missing_doc_fails(self) -> None:
        with TemporaryDirectory() as tmp:
            violations = GATE.run(_build_root(tmp, surface=None))
        self.assertEqual(_kinds(violations), ["missing-doc"])

    def test_missing_canonical_glob_fails_and_leaves_files_unowned(self) -> None:
        surface = GOOD_SURFACE.replace(
            "everything under `src/services/discord/health/reachability/**`.",
            "nothing else.",
        )
        with TemporaryDirectory() as tmp:
            violations = GATE.run(_build_root(tmp, surface=surface))
        self.assertEqual(sorted(set(_kinds(violations))), ["uncovered-file", "unowned-tree"])

    def test_missing_required_statement_fails(self) -> None:
        surface = GOOD_SURFACE.replace(
            "- non_guarantees: this is a source lint, not a type proof.\n", ""
        )
        with TemporaryDirectory() as tmp:
            violations = GATE.run(_build_root(tmp, surface=surface))
        self.assertEqual(_kinds(violations), ["missing-marker"])

    def test_missing_watchdog_companion_requirement_fails(self) -> None:
        surface = GOOD_SURFACE.replace("`scripts/relay_watchdog.py`", "the watchdog")
        with TemporaryDirectory() as tmp:
            violations = GATE.run(_build_root(tmp, surface=surface))
        self.assertEqual(_kinds(violations), ["missing-marker"])

    def test_required_statement_survives_wrapping_and_emphasis(self) -> None:
        """The doc wraps its prose; a marker must not pin the wrap column."""

        surface = GOOD_SURFACE.replace(
            "- non_guarantees: this is a source lint, not a type proof.",
            "- non_guarantees: this is a source **lint, not a type\n  proof**.",
        )
        with TemporaryDirectory() as tmp:
            self.assertEqual(GATE.run(_build_root(tmp, surface=surface)), [])

    def test_ghost_path_without_a_future_slice_marker_fails(self) -> None:
        surface = GOOD_SURFACE.replace(
            "- companion edits:",
            "- also: `src/services/discord/outbound/receipt_index.rs`.\n- companion edits:",
        )
        with TemporaryDirectory() as tmp:
            violations = GATE.run(_build_root(tmp, surface=surface))
        self.assertEqual(_kinds(violations), ["ghost-path"])

    def test_ghost_path_marked_as_a_future_slice_passes(self) -> None:
        surface = GOOD_SURFACE.replace(
            "- companion edits:",
            "- also: `src/services/discord/outbound/receipt_index.rs`, which is\n"
            "  not yet on disk (T4-B3).\n- companion edits:",
        )
        with TemporaryDirectory() as tmp:
            self.assertEqual(GATE.run(_build_root(tmp, surface=surface)), [])

    def test_future_slice_marker_does_not_exempt_an_unmarked_peer_path(self) -> None:
        surface = GOOD_SURFACE.replace(
            "- companion edits:",
            "- also: `src/services/discord/outbound/receipt_index.rs`, which is\n"
            "  not yet on disk (T4-B3), and\n"
            "  `src/services/discord/outbound/unrelated_missing.rs`.\n"
            "- companion edits:",
        )
        with TemporaryDirectory() as tmp:
            violations = GATE.run(_build_root(tmp, surface=surface))
        self.assertEqual(_kinds(violations), ["ghost-path"])
        self.assertIn("unrelated_missing.rs", violations[0].detail)

    def test_future_slice_marker_still_exempts_the_path_it_follows(self) -> None:
        surface = GOOD_SURFACE.replace(
            "- companion edits:",
            "- also: `src/services/discord/outbound/future_index.rs` belongs to\n"
            "  a later slice and is not yet on disk (T4-B4).\n"
            "- companion edits:",
        )
        with TemporaryDirectory() as tmp:
            self.assertEqual(GATE.run(_build_root(tmp, surface=surface)), [])

    def test_a_tree_file_no_glob_covers_is_reported(self) -> None:
        surface = GOOD_SURFACE.replace(
            "everything under `src/services/discord/health/reachability/**`.",
            "everything under `src/services/discord/health/reachability/sub/**`.",
        )
        with TemporaryDirectory() as tmp:
            violations = GATE.run(_build_root(tmp, surface=surface))
        self.assertEqual(sorted(set(_kinds(violations))), ["uncovered-file", "unowned-tree"])

    def test_a_deleted_tree_reports_itself_instead_of_passing_vacuously(self) -> None:
        """An empty scan set must never look like a clean scan."""

        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            doc = root / GATE.CHANGE_SURFACES_DOC
            doc.parent.mkdir(parents=True, exist_ok=True)
            doc.write_text(GOOD_SURFACE, encoding="utf-8")
            violations = GATE.run(root)
        self.assertIn("missing-tree", _kinds(violations))


class LiveRepoTest(unittest.TestCase):
    def test_checked_in_tree_passes_the_gate(self) -> None:
        self.assertEqual([v.render() for v in GATE.run(REPO_ROOT)], [])

    def test_gate_is_wired_into_the_aggregate_script_checks_runner(self) -> None:
        """A gate nobody runs is a comment (#4255)."""

        text = CI_SCRIPT.read_text(encoding="utf-8")
        self.assertIn(
            '"$PYTHON" scripts/check_reachability_row_independence.py', text
        )
        self.assertIn(
            '"$PYTHON" -m unittest tests.test_reachability_row_independence', text
        )

    def test_module_root_and_directory_are_both_in_the_scan_set(self) -> None:
        scanned = {
            path.relative_to(REPO_ROOT).as_posix() for path in GATE.tree_files(REPO_ROOT)
        }
        self.assertIn(GATE.TREE_ROOT_FILE, scanned)
        self.assertTrue(
            any(name.startswith(f"{GATE.TREE_DIR}/") for name in scanned),
            f"the directory half of the tree is missing from {sorted(scanned)}",
        )


if __name__ == "__main__":
    unittest.main()
