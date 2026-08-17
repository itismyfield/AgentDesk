"""Mutation proof for scripts/check_reachability_row_independence.py (#5071 T4-B1).

The gate is the machine half of 4987 §-1.5 I14 and §9.4. A gate that only ever
runs green on the real tree proves nothing, so every check it makes is exercised
here against a synthetic repo root that reproduces exactly one defect at a time:

  * the three shapes an inflight dependency can take from inside the tree
    (`use` of a qualified path, a bare segment in a brace list, and an inline
    fully-qualified call with no `use` at all);
  * the two shapes that reach the row from OUTSIDE the tree's own text — a
    third file re-exporting it under a name that never says "inflight"
    (directly, renamed, globbed, and one hop further on), and an `include!`
    pulling a file the directory scan does not own into the tree — together
    with the negative controls that keep the closure from cascading: a private
    `use` publishes nothing, a re-export path that merely contains "inflight"
    inside a longer identifier is not the module, and a tainted leaf does not
    taint the siblings sharing its brace list;
  * the same launder route split across two items of one file — a private
    `use ...::inflight as alias;` that publishes nothing, plus a `pub use
    self::alias::…` that publishes a name whose own path never says
    "inflight" — in both binding forms, with the controls that keep the
    file-local rewrite from following an alias the re-export does not name and
    from chasing a binding cycle forever;
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
`scripts/ci-script-checks.sh`, that the checked-in tree passes it, and that its
third-file half is not inert here — the real `src/` does contain re-exports of
the inflight module, so the closure has something to close over.
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


class ThirdFileReExportTest(unittest.TestCase):
    """The launder route: the row arrives under a name that never says "inflight".

    Every case pairs the bypass with the control that the same tree text is
    clean once the third file stops re-exporting the row — otherwise the test
    would pass on a gate that simply forbids the name outright.
    """

    LAUNDERER = "src/services/discord/rows.rs"
    SECOND_HOP = "src/services/discord/ledger_view.rs"

    def test_a_renamed_third_file_re_export_is_flagged(self) -> None:
        child = textwrap.dedent(
            """\
            use crate::services::discord::rows::Row;

            fn probe(_row: Row) {}
            """
        )
        launderer = (
            "pub(crate) use crate::services::discord::inflight::"
            "InflightTurnState as Row;\n"
        )
        with TemporaryDirectory() as tmp:
            violations = GATE.run(
                _build_root(tmp, child=child, extra_files={self.LAUNDERER: launderer})
            )
        self.assertEqual(set(_kinds(violations)), {"laundered-inflight-name"})
        self.assertIn(self.LAUNDERER, violations[0].detail)

    def test_the_same_tree_text_is_clean_without_the_re_export(self) -> None:
        """The control: the gate forbids the laundered ROUTE, not the word `Row`."""

        child = textwrap.dedent(
            """\
            use crate::services::discord::rows::Row;

            fn probe(_row: Row) {}
            """
        )
        launderer = "pub(crate) use crate::services::discord::registry::Row;\n"
        with TemporaryDirectory() as tmp:
            self.assertEqual(
                GATE.run(
                    _build_root(tmp, child=child, extra_files={self.LAUNDERER: launderer})
                ),
                [],
            )

    def test_a_second_hop_under_a_third_name_is_flagged(self) -> None:
        """`inflight::X as Row` then `rows::Row as Ledger`: the closure follows."""

        with TemporaryDirectory() as tmp:
            violations = GATE.run(
                _build_root(
                    tmp,
                    child="use crate::services::discord::ledger_view::Ledger;\n",
                    extra_files={
                        self.LAUNDERER: (
                            "pub(crate) use crate::services::discord::inflight::"
                            "InflightTurnState as Row;\n"
                        ),
                        self.SECOND_HOP: (
                            "pub(crate) use crate::services::discord::rows::Row "
                            "as Ledger;\n"
                        ),
                    },
                )
            )
        self.assertEqual(_kinds(violations), ["laundered-inflight-name"])
        self.assertIn(self.SECOND_HOP, violations[0].detail)

    def test_a_glob_re_export_forbids_the_laundering_module_itself(self) -> None:
        """`pub use inflight::*;` publishes a set this scan cannot enumerate, so
        the module holding it becomes the forbidden segment instead."""

        with TemporaryDirectory() as tmp:
            violations = GATE.run(
                _build_root(
                    tmp,
                    child="use crate::services::discord::rows::InflightTurnState;\n",
                    extra_files={
                        self.LAUNDERER: (
                            "pub(crate) use crate::services::discord::inflight::*;\n"
                        )
                    },
                )
            )
        self.assertEqual(_kinds(violations), ["laundered-inflight-module"])

    def test_a_private_use_in_a_third_file_publishes_nothing(self) -> None:
        """A non-`pub` import is not a route: no name leaves that file."""

        launderer = (
            "use crate::services::discord::inflight::InflightTurnState as Row;\n"
            "fn hold(_row: Row) {}\n"
        )
        with TemporaryDirectory() as tmp:
            self.assertEqual(
                GATE.run(
                    _build_root(
                        tmp,
                        child="use crate::services::discord::rows::Row;\n",
                        extra_files={self.LAUNDERER: launderer},
                    )
                ),
                [],
            )

    def test_inflight_inside_a_longer_identifier_is_not_the_module(self) -> None:
        """`clear_inflight_state` re-exported from its own module is not a
        launder of `inflight::`; seeding on the substring would taint the crate."""

        launderer = "pub(crate) use self::store::clear_inflight_state;\n"
        child = textwrap.dedent(
            """\
            use crate::services::discord::rows::clear_inflight_state;

            fn probe() {
                clear_inflight_state();
            }
            """
        )
        with TemporaryDirectory() as tmp:
            self.assertEqual(
                GATE.run(
                    _build_root(tmp, child=child, extra_files={self.LAUNDERER: launderer})
                ),
                [],
            )

    def test_a_tainted_leaf_does_not_taint_its_brace_list_siblings(self) -> None:
        """`use self::store::{Handle, clear_row};` re-exports one laundered name
        and one unrelated one. Taking the item whole would cascade `Handle`."""

        extra = {
            self.LAUNDERER: (
                "pub(crate) use crate::services::discord::inflight::clear_row;\n"
            ),
            self.SECOND_HOP: "pub(crate) use self::store::{Handle, clear_row};\n",
        }
        with TemporaryDirectory() as tmp:
            self.assertEqual(
                GATE.run(
                    _build_root(
                        tmp,
                        child="use crate::services::discord::ledger_view::Handle;\n",
                        extra_files=extra,
                    )
                ),
                [],
            )
        with TemporaryDirectory() as tmp:
            violations = GATE.run(
                _build_root(
                    tmp,
                    child="use crate::services::discord::ledger_view::clear_row;\n",
                    extra_files=extra,
                )
            )
        self.assertEqual(_kinds(violations), ["laundered-inflight-name"])

    def test_a_third_file_that_does_not_lex_fails_closed(self) -> None:
        """The launder half has the same fail-closed floor as the tree scan: a
        file whose re-exports cannot be read is reported, not assumed clean."""

        launderer = (
            "/* unterminated block comment\n"
            "pub(crate) use crate::services::discord::inflight::Row;\n"
        )
        with TemporaryDirectory() as tmp:
            violations = GATE.run(
                _build_root(tmp, extra_files={self.LAUNDERER: launderer})
            )
        self.assertEqual(_kinds(violations), ["unlexable-reexport-source"])

    def test_a_re_export_inside_the_tree_is_reported_once_as_an_import(self) -> None:
        """The tree is excluded from the launder scan: its own `pub use` of the
        row is an `inflight-import`, not that plus a laundered-name echo."""

        with TemporaryDirectory() as tmp:
            violations = GATE.run(
                _build_root(
                    tmp,
                    child=(
                        "pub(crate) use crate::services::discord::inflight::"
                        "InflightTurnState as Row;\n"
                    ),
                )
            )
        self.assertEqual(_kinds(violations), ["inflight-path"])


class FileLocalAliasLaunderTest(unittest.TestCase):
    """A `pub use` laid over a PRIVATE alias bound in the same file.

    Neither item spells the row's path in full: the private `use` names
    `inflight` but publishes nothing, and the `pub use` publishes a name but
    routes through `self::`, so a closure that only matches segments of `pub
    use` paths sees no `inflight` and lets the name out. Both forms compile
    (`pub(in crate::services::discord)` is a `pub` this gate counts), so both
    are real routes, not hypotheticals.
    """

    LAUNDERER = "src/services/discord/rows.rs"
    CHILD = "use crate::services::discord::rows::Row;\n\nfn probe(_row: Row) {}\n"

    def test_a_re_export_over_a_renamed_private_alias_is_flagged(self) -> None:
        """`use ...::inflight as private_rows;` + `pub use self::private_rows::…`."""

        launderer = (
            "use crate::services::discord::inflight as private_rows;\n"
            "pub(in crate::services::discord) use self::private_rows::"
            "InflightTurnState as Row;\n"
        )
        with TemporaryDirectory() as tmp:
            violations = GATE.run(
                _build_root(
                    tmp, child=self.CHILD, extra_files={self.LAUNDERER: launderer}
                )
            )
        self.assertEqual(set(_kinds(violations)), {"laundered-inflight-name"})
        self.assertIn(self.LAUNDERER, violations[0].detail)

    def test_a_re_export_over_a_bare_use_binding_is_flagged(self) -> None:
        """The no-`as` form binds a name too: `use path::store;` binds `store`.

        The binding has to name a MODULE inside inflight rather than the row
        itself — re-exporting a private `use` binding of an ITEM is E0364, so
        that shape is not a route; going THROUGH the binding to an item the
        re-export's visibility already permits is, and it never spells
        `inflight` in the `pub use` path.
        """

        launderer = (
            "use crate::services::discord::inflight::store;\n"
            "pub(in crate::services::discord) use self::store::"
            "InflightTurnState as Row;\n"
        )
        with TemporaryDirectory() as tmp:
            violations = GATE.run(
                _build_root(
                    tmp, child=self.CHILD, extra_files={self.LAUNDERER: launderer}
                )
            )
        self.assertEqual(set(_kinds(violations)), {"laundered-inflight-name"})
        self.assertIn(self.LAUNDERER, violations[0].detail)

    def test_a_re_export_over_an_unrelated_alias_in_the_same_file_is_clean(self) -> None:
        """The control: the rewrite follows the alias the `pub use` names, not
        every alias in a file that happens to mention the row somewhere."""

        launderer = textwrap.dedent(
            """\
            use crate::services::discord::inflight as private_rows;
            use crate::services::discord::registry as safe_rows;

            pub(in crate::services::discord) use self::safe_rows::Row;

            fn hold(_row: private_rows::InflightTurnState) {}
            """
        )
        with TemporaryDirectory() as tmp:
            self.assertEqual(
                GATE.run(
                    _build_root(
                        tmp, child=self.CHILD, extra_files={self.LAUNDERER: launderer}
                    )
                ),
                [],
            )

    def test_an_alias_of_an_alias_resolves(self) -> None:
        """Chained file-local bindings are followed to a fixpoint; stopping at
        one hop would leave the same bypass one `use` line further out."""

        launderer = (
            "use crate::services::discord::inflight as first_hop;\n"
            "use self::first_hop as second_hop;\n"
            "pub(in crate::services::discord) use self::second_hop::"
            "InflightTurnState as Row;\n"
        )
        with TemporaryDirectory() as tmp:
            violations = GATE.run(
                _build_root(
                    tmp, child=self.CHILD, extra_files={self.LAUNDERER: launderer}
                )
            )
        self.assertEqual(set(_kinds(violations)), {"laundered-inflight-name"})

    def test_a_self_referential_binding_terminates(self) -> None:
        """A cycle in the binding map must not hang the gate. This text does not
        compile, but a lexical scan is pointed at whatever is on disk and has to
        terminate on it — the chase is bounded by the paths it has visited."""

        launderer = (
            "use self::looped as looped;\n"
            "pub(in crate::services::discord) use self::looped::Row;\n"
        )
        with TemporaryDirectory() as tmp:
            self.assertEqual(
                GATE.run(
                    _build_root(
                        tmp, child=self.CHILD, extra_files={self.LAUNDERER: launderer}
                    )
                ),
                [],
            )


class IncludeExpansionTest(unittest.TestCase):
    """`include!` makes another file's text part of the tree, so it is scanned."""

    OFF_TREE = "src/services/discord/health/reachability_extra.rs"
    OFF_TREE_SECOND = "src/services/discord/health/reachability_deeper.rs"
    INCLUDE = 'include!("../reachability_extra.rs");\n'

    def test_an_included_file_that_reaches_the_row_is_flagged(self) -> None:
        with TemporaryDirectory() as tmp:
            violations = GATE.run(
                _build_root(
                    tmp,
                    child=self.INCLUDE,
                    extra_files={
                        self.OFF_TREE: (
                            "use crate::services::discord::inflight::Row;\n"
                        )
                    },
                )
            )
        self.assertEqual(_kinds(violations), ["inflight-path"])
        self.assertTrue(violations[0].path.endswith("reachability_extra.rs"))

    def test_the_same_off_tree_file_is_invisible_without_the_include(self) -> None:
        """The control: `rglob` never sees it, so the include is what pulls it in."""

        with TemporaryDirectory() as tmp:
            self.assertEqual(
                GATE.run(
                    _build_root(
                        tmp,
                        extra_files={
                            self.OFF_TREE: (
                                "use crate::services::discord::inflight::Row;\n"
                            )
                        },
                    )
                ),
                [],
            )

    def test_includes_are_followed_transitively(self) -> None:
        with TemporaryDirectory() as tmp:
            violations = GATE.run(
                _build_root(
                    tmp,
                    child=self.INCLUDE,
                    extra_files={
                        self.OFF_TREE: 'include!("reachability_deeper.rs");\n',
                        self.OFF_TREE_SECOND: (
                            "fn probe() {\n"
                            "    let _ = crate::services::discord::inflight"
                            "::load_row();\n"
                            "}\n"
                        ),
                    },
                )
            )
        self.assertEqual(_kinds(violations), ["inflight-path"])
        self.assertTrue(violations[0].path.endswith("reachability_deeper.rs"))

    def test_a_clean_included_file_passes(self) -> None:
        with TemporaryDirectory() as tmp:
            self.assertEqual(
                GATE.run(
                    _build_root(
                        tmp,
                        child=self.INCLUDE,
                        extra_files={self.OFF_TREE: "fn probe() {}\n"},
                    )
                ),
                [],
            )

    def test_an_include_that_does_not_resolve_fails_closed(self) -> None:
        with TemporaryDirectory() as tmp:
            violations = GATE.run(
                _build_root(tmp, child='include!("../nowhere.rs");\n')
            )
        self.assertEqual(_kinds(violations), ["unresolved-include"])

    def test_an_include_argument_that_is_not_a_literal_fails_closed(self) -> None:
        """A path a macro assembles cannot be resolved, so it is not waved past."""

        with TemporaryDirectory() as tmp:
            violations = GATE.run(
                _build_root(tmp, child='include!(concat!("a", "b.rs"));\n')
            )
        self.assertEqual(_kinds(violations), ["unreadable-include"])

    def test_a_commented_out_include_is_not_followed(self) -> None:
        with TemporaryDirectory() as tmp:
            self.assertEqual(
                GATE.run(_build_root(tmp, child='// include!("../nowhere.rs");\n')), []
            )


class UseTreeExpansionTest(unittest.TestCase):
    """The expander the launder closure reads re-exports through."""

    def test_nested_braces_expand_to_one_leaf_each(self) -> None:
        leaves = GATE.expand_use_tree(" crate::a::{b::{c, d as e}, f::*, self} ")
        self.assertEqual(
            [(path, name) for path, name in leaves],
            [
                (("crate", "a", "b", "c"), "c"),
                (("crate", "a", "b", "d"), "e"),
                (("crate", "a", "f", "*"), None),
                (("crate", "a"), "a"),
            ],
        )


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

    def test_the_live_launder_closure_is_not_vacuous(self) -> None:
        """The third-file half has to be doing work on THIS repo, not only on the
        synthetic roots above. `src/services/discord/mod.rs` re-exports inflight
        items today; if that ever stops being true this assertion has outlived
        the fact it pins and should be deleted, not relaxed."""

        laundered, violations = GATE.collect_launderers(REPO_ROOT)
        self.assertEqual([violation.render() for violation in violations], [])
        self.assertTrue(
            laundered,
            "no pub use under src/ re-exports the inflight module, so the "
            "laundered-name half of this gate is inert against the real tree",
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
