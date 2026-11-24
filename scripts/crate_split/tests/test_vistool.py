"""vistool: recorded rustc diagnostics -> definition + modifier, residue classes, glob safety, probe isolation.

`fixtures/vistool/mini` is a one-crate tree that partition.json splits into LOW (core, services::store) and
HIGH. diags-vis.json holds every visibility diagnostic a shadow run of it produced (paths under `<PROBE>/`);
diags-residue.json holds the residue a variant with a hard cut, a foreign impl and a test-only item produced.
The end-to-end case runs the real tool on a throwaway copy outside the repository (cargo required).
"""

import contextlib
import copy
import io
import json
import os
from pathlib import Path
import random
import shutil
import subprocess
import sys
import tempfile
import tomllib
from types import SimpleNamespace
import unittest
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
import vistool  # noqa: E402
import vistool_real  # noqa: E402
import vistool_testcfg  # noqa: E402

FX = HERE / "fixtures" / "vistool"
MINI = FX / "mini"
UPPER = Path("/nonexistent-vistool-upper")
MINI_HIGH = ["(root)", "app", "services", "services::web"]


def load(name):
    text = (FX / name).read_text("utf-8").replace("<PROBE>/lower", str(MINI)).replace("<PROBE>/upper", str(UPPER))
    diags = json.loads(text)
    for d in diags:
        d["_root"] = str(MINI if d["_side"] == "lower" else UPPER)
    return diags


def line_of(rel, text):
    lines = (MINI / rel).read_text("utf-8").split("\n")
    return next(i for i, l in enumerate(lines, 1) if l.strip() == text)


class Mini:
    layout = None

    @classmethod
    def get(cls):
        if cls.layout is None:
            cls.layout = vistool.Layout(MINI, vistool.load_partition(MINI / "partition.json"), "LOW")
        return cls.layout


def widener():
    return vistool.Widener(Mini.get(), MINI, UPPER), vistool.Resolver(MINI, UPPER, "src/probe_lower_lib.rs", "src/lib.rs")


def leak(descr, name, rel, text):
    """rustc's E0446 as the KRX shadow run recorded it on `impl FromStr for IntakeOutboxStatus { type Err = .. }`:
    the primary span is the associated type, the labelled one the declaration of the type it leaks."""
    at = line_of(rel, text)
    return {"level": "error", "code": {"code": "E0446"}, "message": f"{descr} type `{name}` in public interface",
            "children": [], "spans": [
                {"is_primary": True, "file_name": rel, "line_start": at + 3, "line_end": at + 3, "label": f"can't leak {descr} type"},
                {"is_primary": False, "file_name": rel, "line_start": at, "line_end": at, "label": f"`{name}` declared as {descr}"}]}


# message -> (file, declaration line as written, widened line)
EXPECTED = {
    "struct `Config` is private": ("src/core.rs", "pub(crate) struct Config {", "pub struct Config {"),
    "module `util` is private": ("src/core.rs", "pub(crate) mod util;", "pub mod util;"),
    "function `helper` is private": ("src/core.rs", "pub(crate) fn helper() -> u32 {", "pub fn helper() -> u32 {"),
    "struct `Thing` is private": ("src/services/store.rs", "pub(super) struct Thing {", "pub struct Thing {"),
    "tuple struct constructor `Shared` is private": ("src/core/util.rs", "pub(crate) struct Shared(pub(crate) u32);",
                                                     "pub(crate) struct Shared(pub u32);"),
    "cannot initialize a tuple struct which contains private fields": (
        "src/core.rs", "pub(crate) struct Id(pub(crate) u32);", "pub(crate) struct Id(pub u32);"),
    "method `limit` is private": ("src/core.rs", "pub(crate) fn limit(&self) -> u32 {", "    pub fn limit(&self) -> u32 {"),
    "associated function `new` is private": ("src/core.rs", "pub(crate) fn new(name: &str) -> Self {",
                                             "    pub fn new(name: &str) -> Self {"),
    "field `limit` of struct `Config` is private": ("src/core.rs", "pub(crate) limit: u32,", "    pub limit: u32,"),
    "field `0` of struct `core::util::Shared` is private": (
        "src/core/util.rs", "pub(crate) struct Shared(pub(crate) u32);", "pub(crate) struct Shared(pub u32);"),
    "field `lo` of struct `Span` is private": ("src/core.rs", "pub(crate) lo: u32,", "    pub lo: u32,"),
    "`Shared` is only public within the crate, and cannot be re-exported outside": (
        "src/core/util.rs", "pub(crate) struct Shared(pub(crate) u32);", "pub struct Shared(pub(crate) u32);"),
    "type `Token` is more private than the item `make_token`": ("src/core.rs", "pub(crate) struct Token;",
                                                               "pub struct Token;"),
}


# The E0364 was recorded once its `pub use` line had been widened; the source here is still the original,
# so resolving it also widens that line.
EXTRA = {"`Shared` is only public within the crate, and cannot be re-exported outside": (
    "src/core.rs", "pub(crate) use self::util::Shared;", "pub use self::util::Shared;")}


def mismatches():
    """Rows of EXPECTED the resolver gets wrong (an empty list is a pass)."""
    w, r = widener()
    by_msg = {d["message"]: d for d in load("diags-vis.json")}
    bad = []
    for msg, (rel, before, after) in sorted(EXPECTED.items()):
        d = by_msg[msg]
        edits, outcome = w.resolve(d, r, Path(d["_root"]))
        got = {k: vistool.apply_op(w.lines(k[0])[k[1]], op) for k, op in edits.items()}
        want = {(rel, line_of(rel, before) - 1): after}
        if msg in EXTRA:
            xrel, xbefore, xafter = EXTRA[msg]
            want[(xrel, line_of(xrel, xbefore) - 1)] = xafter
        if outcome != "edit" or got != want:
            bad.append((msg, outcome, edits))
    return bad


class RecordedDiagnostics(unittest.TestCase):
    def test_every_recorded_code_is_present(self):
        codes = {(d["code"] or {}).get("code") for d in load("diags-vis.json")}
        self.assertLessEqual({"E0603", "E0624", "E0616", "E0451", "E0364", "E0423", "E0532", "private_interfaces"}, codes)

    def test_definition_and_modifier_per_code(self):
        self.assertEqual(mismatches(), [])

    def test_e0616_without_span_needs_a_unique_struct(self):
        w, r = widener()
        d = next(d for d in load("diags-vis.json") if d["message"] == "field `x` of struct `Point` is private")
        self.assertIsNone(vistool.definition_span(d))
        with mock.patch.object(w, "unique_struct", return_value=[("src/core.rs", 1), ("src/core/util.rs", 0)]):
            edits, outcome = w.resolve(d, r, UPPER)
        self.assertEqual(edits, {})
        self.assertEqual(outcome[0], "unresolved")
        self.assertIn("defined 2 times", outcome[1])

    def test_e0532_pointing_into_upper_is_not_edited(self):
        w, r = widener()
        d = next(d for d in load("diags-vis.json") if (d["code"] or {}).get("code") == "E0532")
        edits, outcome = w.resolve(d, r, UPPER)
        self.assertEqual(edits, {})
        self.assertNotEqual(outcome, "edit")

    def test_same_input_same_patch(self):
        patches = []
        for seed in (1, 2, 3):
            diags = [d for d in load("diags-vis.json") if vistool.is_vis(d) or d["_side"] == "lower"]
            random.Random(seed).shuffle(diags)
            w, r = widener()
            ops = {}
            for d in diags:
                edits, outcome = w.resolve(d, r, Path(d["_root"]))
                if outcome == "edit":
                    for key, op in edits.items():
                        ops.setdefault(key, set()).add(op)
            for (rel, i), group in ops.items():  # Widener.apply would also write the probe copies
                w.lines(rel)[i] = vistool.apply_ops(w.lines(rel)[i], group)
            patches.append(vistool.unified_patch(w.original, w.text))
        self.assertEqual(len(set(patches)), 1)
        self.assertIn("+pub struct Config {", patches[0])
        # Shared gets both the E0364 item widening and the constructor's field widening on one line.
        self.assertIn("+pub struct Shared(pub u32);", patches[0])

    def test_residue_classes(self):
        w, r = widener()
        restricted = vistool.restricted_defs(Mini.get(), w.lines)
        got = {}
        for d in load("diags-residue.json"):
            if vistool.is_vis(d) or (d["code"] or {}).get("code") in vistool.VIS_LINTS:
                got[d["message"][:30]] = w.resolve(d, r, Path(d["_root"]))[1][0]
            else:
                got[d["message"][:30]] = vistool.classify_residue(d, restricted)
        self.assertEqual(got, {
            "mismatched types": "cut_residue",
            "cannot define inherent `impl` ": "impl_residue",
            "cannot find function `fixture_": "test_cfg",
            "type `Widget` is more private ": "cut_residue",
            "method `doubled` is private": "cut_residue",
        })

    def test_glob_hidden_and_macro_paths(self):
        restricted = {"helper": [False], "fixture_value": [True]}
        hidden = {"code": {"code": "E0425"}, "message": "cannot find function `helper` in this scope", "_test": True}
        test_only = {"code": {"code": "E0425"}, "message": "cannot find function `fixture_value` in this scope",
                     "_test": True}
        macro = {"code": None, "message": "cannot find macro `log_queue` in module `crate`"}
        self.assertEqual(vistool.classify_residue(hidden, restricted), "glob_hidden")
        self.assertEqual(vistool.classify_residue(test_only, restricted), "test_cfg")
        self.assertEqual(vistool.classify_residue(macro, restricted), "macro_path")
        exported = {"code": {"code": "E0433"}, "message": "failed to resolve: could not find `log_q` in the crate root"}
        self.assertEqual(vistool.classify_residue(exported, {"log_q": ["macro"]}), "macro_path")
        self.assertEqual(vistool.classify_residue({"code": {"code": "E0408"}, "message": "variable `Modal` is not "
                                                   "bound in all patterns"}, restricted), "glob_hidden")
        self.assertEqual(vistool.classify_residue({"code": {"code": "E0599"}, "message": "x", "_test": False},
                                                  restricted), "unclassified")

    def glob_follow(self, sources, sides=None):
        """resolve() of `struct Deep is private` whose note points at line 1 of src/a/mod.rs."""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            files = {}
            for rel, text in sources.items():
                (root / rel).parent.mkdir(parents=True, exist_ok=True)
                (root / rel).write_text(text)
                files[rel] = tuple(Path(rel).with_suffix("").parts[1:])
                if files[rel][-1] == "mod":
                    files[rel] = files[rel][:-1]
            by_module = {m: f for f, m in files.items()}
            layout = SimpleNamespace(
                crate="LOW", x_files=set(files), module_at=lambda rel, line: files[rel],
                side_of=lambda rel, line=None: (sides or {}).get(rel, "x"),
                locate=lambda m: (by_module[m], 1, 10 ** 9) if m in by_module else None)
            w = vistool.Widener(layout, root, UPPER)
            r = vistool.Resolver(root, UPPER, "src/probe_lower_lib.rs", "src/lib.rs")
            diag = {"code": {"code": "E0603"}, "message": "struct `Deep` is private", "spans": [],
                    "children": [{"message": "the struct `Deep` is defined here",
                                  "spans": [{"file_name": str(root / "src/a/mod.rs"), "line_start": 1, "line_end": 1}]}]}
            return w.resolve(diag, r, UPPER)

    def test_already_pub_glob_reexport_is_followed(self):
        # `pub use inner::*;` re-exports only what is visible; the note points at the glob line.
        got = self.glob_follow({"src/a/mod.rs": "pub use inner::*;\nmod inner;\n",
                                "src/a/inner.rs": "pub(crate) struct Deep;\n"})
        self.assertEqual(got, ({("src/a/inner.rs", 0): ("item",)}, "edit"))

    def test_glob_follow_leaves_private_and_unrelated_globs(self):
        # P1-3: `use super::*;`, an unrelated `pub(crate) use other::*;` and a fn-local `use Shape::*;`
        # stay as written when the module declares the name itself.
        got = self.glob_follow({"src/a/mod.rs": "pub use inner::*;\nmod inner;\n",
                                "src/a/inner.rs": "use super::*;\npub(crate) use other::*;\nfn f() {\n"
                                                  "    use Shape::*;\n}\npub(crate) struct Deep;\n",
                                "src/a/inner/other.rs": "pub(crate) struct Unrelated;\n"})
        self.assertEqual(got, ({("src/a/inner.rs", 5): ("item",)}, "edit"))

    def test_glob_follow_widens_only_the_glob_that_holds_the_name(self):
        got = self.glob_follow({"src/a/mod.rs": "pub use inner::*;\nmod inner;\n",
                                "src/a/inner.rs": "use super::*;\npub(crate) use empty::*;\npub(crate) use sub::*;\n",
                                "src/a/inner/empty.rs": "pub(crate) struct Other;\n",
                                "src/a/inner/sub.rs": "pub(crate) struct Deep;\n"})
        self.assertEqual(got, ({("src/a/inner.rs", 2): ("item",), ("src/a/inner/sub.rs", 0): ("item",)}, "edit"))

    def test_reexport_of_an_above_item_is_not_widened(self):
        # P1-2: X's `pub(crate) use crate::app::AboveSecret;` must stay; `pub use` of an above
        # pub(crate) item breaks lower (E0364) and the single crate alike.
        got = self.glob_follow({"src/a/mod.rs": "pub(crate) use crate::app::Deep;\n",
                                "src/app.rs": "pub(crate) struct Deep;\n"}, sides={"src/app.rs": "above"})
        self.assertEqual(got[0], {})
        self.assertEqual(got[1][0], "cut_residue")

    def test_through_use_follows_an_alias(self):
        # VT1 fix1 review P2-2: `{orig as Deep}` is looked up as `orig` behind the path.
        got = self.glob_follow({"src/a/mod.rs": "pub(crate) use self::inner::{Orig as Deep};\nmod inner;\n",
                                "src/a/inner.rs": "pub(crate) struct Orig;\n"})
        self.assertEqual(got, ({("src/a/inner.rs", 0): ("item",), ("src/a/mod.rs", 0): ("item",)}, "edit"))

    def test_reexport_error_naming_the_original_of_an_alias(self):
        # KRX round 2: E0364 on `pub use super::{advance_checked as advance_codex_offset_checked};` names
        # `advance_checked` (the original), not the alias.
        got = self.glob_follow({"src/a/mod.rs": "pub use self::inner::{Deep as Alias};\nmod inner;\n",
                                "src/a/inner.rs": "pub(crate) struct Deep;\n"})
        self.assertEqual(got, ({("src/a/inner.rs", 0): ("item",)}, "edit"))

    def test_through_use_follows_nested_braces(self):
        got = self.glob_follow({"src/a/mod.rs": "pub(crate) use self::{Other, inner::{Deep}};\nmod inner;\n"
                                                "pub(crate) struct Other;\n",
                                "src/a/inner.rs": "pub(crate) struct Deep;\n"})
        self.assertEqual(got, ({("src/a/inner.rs", 0): ("item",), ("src/a/mod.rs", 0): ("item",)}, "edit"))

    def test_alias_of_an_above_item_is_cut_residue(self):
        got = self.glob_follow({"src/a/mod.rs": "pub(crate) use crate::{app::{Secret as Deep}};\n",
                                "src/app.rs": "pub(crate) struct Secret;\n"}, sides={"src/app.rs": "above"})
        self.assertEqual((got[0], got[1][0]), ({}, "cut_residue"))

    def test_through_use_follows_a_module_reexport(self):
        # KRX round 2: `crate::a::moved::Deep` where `a` re-exports `moved` from its child `inner`.
        got = self.glob_follow({"src/a/mod.rs": "pub(crate) use crate::a::moved::Deep;\nmod inner;\n"
                                                "pub(crate) use self::inner::moved;\n",
                                "src/a/inner.rs": "pub(crate) mod moved;\n",
                                "src/a/inner/moved.rs": "pub(crate) struct Deep;\n"})
        self.assertEqual(got, ({("src/a/mod.rs", 0): ("item",), ("src/a/mod.rs", 2): ("item",),
                                ("src/a/inner.rs", 0): ("item",), ("src/a/inner/moved.rs", 0): ("item",)}, "edit"))

    def test_through_use_follows_a_module_behind_a_glob(self):
        # KRX `tui_prompt_dedupe`: `pub use runtime_binding::*;` re-exports `pub(crate) mod pane_registration;`.
        got = self.glob_follow({"src/a/mod.rs": "pub(crate) use crate::a::moved::Deep;\nmod inner;\n"
                                                "pub use inner::*;\n",
                                "src/a/inner.rs": "pub(crate) mod moved;\n",
                                "src/a/inner/moved.rs": "pub(crate) struct Deep;\n"})
        self.assertEqual(got, ({("src/a/mod.rs", 0): ("item",), ("src/a/inner.rs", 0): ("item",),
                                ("src/a/inner/moved.rs", 0): ("item",)}, "edit"))

    def test_module_reexport_through_an_above_file_is_cut_residue(self):
        # VT2 review P1-6: LOW `crate::app::moved::Deep` where UP `app` re-exports LOW's module: no op on app.rs.
        got = self.glob_follow({"src/a/mod.rs": "pub(crate) use crate::app::moved::Deep;\npub(crate) mod moved;\n",
                                "src/a/moved.rs": "pub(crate) struct Deep;\n",
                                "src/app.rs": "pub(crate) use crate::a::moved;\n"}, sides={"src/app.rs": "above"})
        self.assertEqual((got[0], got[1][0]), ({}, "cut_residue"))

    def test_through_use_of_a_module_from_another_crate(self):
        # `crate::a::host::Kind` where `a` has `pub(crate) use adk_base::host;` (an extracted member's public item).
        got = self.glob_follow({"src/a/mod.rs": "pub(crate) use crate::a::host::Deep;\npub(crate) use adk_base::host;\n"})
        self.assertEqual(got, ({("src/a/mod.rs", 0): ("item",), ("src/a/mod.rs", 1): ("item",)}, "edit"))

    def test_restricted_reexport_of_an_already_pub_chain_is_widened(self):
        # KRX `platform/shell.rs`: `pub(crate) use crate::services::git::{git_branch_name, ..}` whose target is
        # `pub use branch_resolver::{..}` over `pub fn`: only the restricted front line needs `pub`.
        got = self.glob_follow({"src/a/mod.rs": "pub(crate) use self::inner::{Other, Deep};\nmod inner;\n",
                                "src/a/inner.rs": "pub use self::deep::Deep;\npub mod deep;\npub struct Other;\n",
                                "src/a/inner/deep.rs": "pub struct Deep;\n"})
        self.assertEqual(got, ({("src/a/mod.rs", 0): ("item",)}, "edit"))

    def test_use_entries_flatten_the_tree(self):
        self.assertEqual(vistool.use_entries("pub(crate) use crate::a::{b::{C as D, self}, e::*, F};"),
                         [(("crate", "a", "b"), "C", "D"), (("crate", "a"), "b", "b"), (("crate", "a", "e"), "*", "*"),
                          (("crate", "a"), "F", "F")])
        self.assertEqual(vistool.use_entries("use ::std::fmt;"), [(("std",), "fmt", "fmt")])

    def test_use_line_must_bind_the_name(self):
        lines = ["use std::fmt;", "pub(crate) struct Foo;"]
        self.assertEqual(vistool.find_item_line(lines, 0, 1, "Foo", "item"), 1)

    def test_lint_type_path_uses_last_segment(self):
        lint = {"code": {"code": "private_interfaces"}, "message": "type `probe::SessionProbe<T>` is more private"}
        self.assertEqual(vistool.message_target(lint), ("item", "SessionProbe", ""))

    def test_mutant_ignoring_defined_here_is_caught(self):
        with mock.patch.object(vistool, "definition_span", lambda diag, marks=None: None):
            self.assertNotEqual(mismatches(), [])


class Patch(unittest.TestCase):
    def test_patch_applies_at_end_of_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            (root / "a.rs").write_text("fn a() {}\npub(crate) fn b() {}\n")
            (root / "b.rs").write_text("pub(crate) fn c() {}")
            original = {"a.rs": ["fn a() {}", "pub(crate) fn b() {}", ""], "b.rs": ["pub(crate) fn c() {}"]}
            final = {"a.rs": ["fn a() {}", "pub fn b() {}", ""], "b.rs": ["pub fn c() {}"]}
            (root / "p.patch").write_text(vistool.unified_patch(original, final))
            subprocess.run(["git", "-C", str(root), "apply", "p.patch"], check=True)
            self.assertEqual((root / "a.rs").read_text(), "fn a() {}\npub fn b() {}\n")
            self.assertEqual((root / "b.rs").read_text(), "pub fn c() {}")


class Widen(unittest.TestCase):
    def test_modifier_only(self):
        cases = {
            "pub(crate) async fn run() {": "pub async fn run() {",
            "    pub(super) const unsafe fn f() {}": "    pub const unsafe fn f() {}",
            "pub(in crate::a) struct S;": "pub struct S;",
            "fn private() {}": "pub fn private() {}",
            "    pub(crate) use self::x::{A, B};": "    pub use self::x::{A, B};",
            "pub(self) mod inner;": "pub mod inner;",
        }
        for before, after in cases.items():
            self.assertEqual(vistool.widen_line(before), after, before)
        for already in ("pub fn x() {}", "macro_rules! m {", "let x = 1;"):
            self.assertIsNone(vistool.widen_line(already), already)

    def test_tuple_fields(self):
        self.assertEqual(vistool.widen_tuple_fields("struct P(u32, pub(crate) HashMap<K, V>);", "P"),
                         "struct P(pub u32, pub HashMap<K, V>);")
        self.assertEqual(vistool.widen_tuple_fields("pub struct P(u32, u8);", "P", only=1), "pub struct P(u32, pub u8);")
        self.assertIsNone(vistool.widen_tuple_fields("pub struct P(", "P"))

    def test_self_classification(self):
        ok = ("src/a.rs", 1, "pub(crate) fn a() {", "pub fn a() {")
        bad = ("src/a.rs", 2, "pub(crate) fn a() {", "pub fn b() {")
        self.assertEqual(vistool.self_classify([ok]), [])
        self.assertEqual(vistool.self_classify([ok, bad]), [bad])


def glob_layout():
    files = {"src/lib.rs": (), "src/a/mod.rs": ("a",), "src/a/b.rs": ("a", "b"), "src/a/x.rs": ("a", "x"),
             "src/c.rs": ("c",), "src/a/b/tests.rs": ("a", "b", "tests")}
    text = {"src/c.rs": ["use crate::a::b::*;"], "src/a/x.rs": ["use super::b::*;"],
            "src/a/b/tests.rs": ["use super::*;"], "src/lib.rs": [], "src/a/mod.rs": [], "src/a/b.rs": []}
    layout = SimpleNamespace(module_of=files, file_of={m: f for f, m in files.items()})
    layout.module_at = lambda rel, line: layout.module_of[rel]
    layout.locate = lambda module: None
    return layout, lambda rel: text[rel]


def edit(before):
    return {"file": "src/a/b.rs", "line": 1, "before": before, "after": vistool.widen_line(before),
            "bindings": vistool.edit_bindings(before, [before], 0)}


class GlobSafety(unittest.TestCase):
    def risks(self, before):
        layout, read = glob_layout()
        return vistool.glob_check([edit(before)], layout, read)

    def test_prelude_name_fails(self):
        risks = self.risks("struct Error;")
        self.assertTrue(risks and all(r["fail"] for r in risks))
        self.assertEqual({r["importer"] for r in risks}, {"src/c.rs:1", "src/a/x.rs:1"})

    def test_trait_fails(self):
        self.assertTrue(all(r["fail"] for r in self.risks("pub(super) trait Helper {}")))

    def test_plain_name_is_risk_only(self):
        risks = self.risks("pub(super) struct Thing;")
        self.assertEqual([(r["importer"], r["fail"]) for r in risks], [("src/c.rs:1", False)])

    def test_crate_visible_and_descendants_are_silent(self):
        self.assertEqual(self.risks("pub(crate) struct Error;"), [])
        layout, read = glob_layout()
        layout.module_of = {k: v for k, v in layout.module_of.items() if k != "src/c.rs" and k != "src/a/x.rs"}
        self.assertEqual(vistool.glob_check([edit("enum Option {}")], layout, read), [])

    def test_restricted_glob_reexport_widening_fails_closed(self):
        # P1-3: the names a widened `pub(super) use inner::*;` brings are unknown here (a trait, a prelude name).
        risks = self.risks("pub(super) use inner::*;")
        self.assertTrue(risks and all(r["fail"] and r["kind"] == "glob" for r in risks))
        self.assertEqual(self.risks("pub(crate) use inner::*;"), [])

    def test_glob_inside_braces_is_scanned(self):
        # VT1 review P2-4 (4): `use crate::a::{b::*, x::Y};` imports a::b's names too.
        layout, read = glob_layout()
        text = {"src/c.rs": ["use crate::a::{x::Y, b::*};"], "src/a/x.rs": [], "src/a/b/tests.rs": [], "src/lib.rs": [],
                "src/a/mod.rs": [], "src/a/b.rs": []}
        risks = vistool.glob_check([edit("pub(super) struct Thing;")], layout, lambda rel: text[rel])
        self.assertEqual([r["importer"] for r in risks], ["src/c.rs:1"])
        imports = vistool.glob_imports(layout, lambda rel: {"src/c.rs": ["use crate::a::{", "    b::*,", "};"]}.get(rel, []))
        self.assertEqual(imports, [(("c",), "src/c.rs", 1, ("a", "b"))])

    def test_mutant_without_glob_scan_is_caught(self):
        with mock.patch.object(vistool, "glob_imports", lambda layout, read: []):
            self.assertFalse(any(r["fail"] for r in self.risks("struct Error;")))


def split(low, high, path=None, **more):
    crates = {"LOW": low, "HIGH": high, **more}
    with tempfile.TemporaryDirectory() as tmp:
        path = path or Path(tmp) / "p"
        path.write_text(json.dumps(dict(base="", order=list(crates), crates=crates)))
        return vistool.load_partition(path)


class Partition(unittest.TestCase):
    def test_deep_key_wins_over_its_ancestor_group(self):
        keys = {"discord::out", "discord::out::deep", "config::a", "config::a::b"}
        self.assertEqual([vistool.group_path(k) for k in ("discord::o::d", "c::a::b")], [("services", "discord", "o", "d"), ("c", "a", "b")])
        # The longest key wins, by whole segments: a sibling named like the deep key stays where it was.
        of = lambda path: vistool.group_of(tuple(path.split("::")), keys)
        got = [of("config::a::b::c")] + [of("services::discord::" + t) for t in ("out::deep::x", "out::deep_x", "out_x::deep")]
        self.assertEqual(got, ["config::a::b", "discord::out::deep", "discord::out", "discord::out_x"])
        high = MINI_HIGH
        cut = split(["core::util", "services::store"], high + ["core"])
        low, top = vistool.Layout(MINI, cut, "LOW"), vistool.Layout(MINI, cut, "HIGH")
        self.assertEqual((low.crate_of_file["src/core/util.rs"], low.crate_of_file["src/core.rs"]), ("LOW", "HIGH"))
        self.assertEqual((low.side_of("src/core/util.rs", 1), low.leaves), ("x", ["core::util", "services::store"]))
        # Once the ancestor is below too it goes to upper whole, the deep key inside it.
        self.assertEqual(top.leaves, ["app", "core", "services::store", "services::web"])
        both = vistool.Layout(MINI, split(["core", "core::util", "services::store"], high), "LOW")
        self.assertEqual((both.leaves, both.crate_of_file), (Mini.get().leaves, Mini.get().crate_of_file))

    def test_conflicting_keys_are_rejected(self):
        for low, high, why in ((["core::util"], ["core::util"], "bad_partition: core::util in LOW and HIGH"),
                               (["services::discord::outbound"], ["discord::outbound"], "is not a group key"),
                               (["core"], ["core::util"], "under core of the earlier crate LOW")):
            self.assertRaisesRegex(vistool.VisFail, why, split, low, high)

    def test_group_keys(self):
        self.assertEqual(vistool.group_of(("services", "discord")), "discord::(mod.rs)")
        self.assertEqual(vistool.group_of(("services", "discord", "inflight", "x")), "discord::inflight")
        self.assertEqual(vistool.group_of(("services", "tui_o", "a")), "services::tui_o")
        self.assertEqual(vistool.group_of(("config", "x")), "config")
        self.assertEqual(vistool.group_of(()), "(root)")
        for g in ("discord::(mod.rs)", "discord::inflight", "services::tui_o", "config", "services"):
            self.assertEqual(vistool.group_of(vistool.group_path(g)), g)

    def test_repo_partition(self):
        p = vistool.load_partition(HERE.parent / "partition.json")
        self.assertRegex(p["base"], r"^[0-9a-f]{40}$")
        self.assertEqual(p["order"], ["KRX", "KE", "DL", "ROOT"])
        self.assertEqual({k: len(v) for k, v in p["crates"].items()}, {"KRX": 87, "KE": 86, "DL": 85, "ROOT": 46})

    def test_manifests(self):
        text = '[package]\nname = "agentdesk"\n\n[workspace]\nmembers = ["crates/x"]\n\n[lib]\npath = "src/lib.rs"\n\n' \
               '[[bin]]\nname = "agentdesk"\npath = "src/main.rs"\n\n[dependencies]\nx = { path = "crates/x" }\n\n' \
               '[dev-dependencies]\nx = { path = "crates/x", features = ["test-support"] }\ntower = "0.5"\n'
        low = vistool.lower_manifest(text, "src/probe_lower_lib.rs")
        self.assertIn('name = "probe_lower"', low)
        self.assertNotIn("[[bin]]", low)
        self.assertIn('path = "src/probe_lower_lib.rs"', low)
        up = vistool.upper_manifest(text)
        self.assertIn('x = { path = "../lower/crates/x" }', up)
        self.assertIn("members = []", up)
        self.assertIn("[[bin]]", up)
        low, up = tomllib.loads(low), tomllib.loads(up)
        self.assertEqual((low["features"], low["dependencies"], "dev-dependencies" in low),
                         ({"test-support": []}, {"x": {"path": "crates/x"}}, False))
        self.assertEqual(low["target"]["cfg(any(unix, not(unix)))"]["dependencies"],
                         {"x": {"path": "crates/x", "features": ["test-support"]}, "tower": "0.5"})
        self.assertEqual(up["dependencies"]["probe_lower"], {"path": "../lower", "features": ["test-support"]})
        kept = vistool.lower_manifest('[package]\nname = "a"\n\n[features]\ntest-support = ["dep:x"]\n', "l.rs")
        self.assertEqual(tomllib.loads(kept)["features"], {"test-support": ["dep:x"]})

    def test_multiline_members_are_emptied(self):
        # VT1 review P2-4 (1): a wrapped `members = [...]` must not keep the members in upper.
        text = '[package]\nname = "agentdesk"\n\n[workspace]\nmembers = [\n    "crates/a",\n    "crates/b",\n]\n' \
               'resolver = "3"\n\n[dependencies]\na = { path = "crates/a" }\n'
        up = vistool.upper_manifest(text)
        self.assertIn("members = []\nresolver", up)
        self.assertNotIn('"crates/b"', up)


def fixture_repo(root):
    repo = root / "mini"
    shutil.copytree(MINI, repo)
    git = ["git", "-C", str(repo), "-c", "user.name=vistool", "-c", "user.email=vistool@example.invalid"]
    subprocess.run(git[:3] + ["init", "-q"], check=True)
    subprocess.run(git[:3] + ["add", "-A"], check=True)
    subprocess.run(git + ["commit", "-qm", "fixture"], check=True)
    return repo, git


class Probe(unittest.TestCase):
    def test_transform_stays_in_probe(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, git = fixture_repo(Path(tmp))
            layout = vistool.Layout(repo, vistool.load_partition(repo / "partition.json"), "LOW")
            probe = repo / "target" / "crate_split_probe" / "LOW"
            vistool.extract_head(repo, probe / "lower")
            vistool.extract_head(repo, probe / "upper")
            lib = vistool.transform_lower(probe / "lower", layout)
            rewired = vistool.transform_upper(probe / "upper", layout)
            lower_lib = (probe / "lower" / lib).read_text("utf-8")
            self.assertIn("pub mod core;", lower_lib)
            self.assertIn("pub mod services;", lower_lib)
            self.assertIn("pub mod store;", (probe / "lower/src/services/mod.rs").read_text("utf-8"))
            self.assertIn('name = "probe_lower"', (probe / "lower/Cargo.toml").read_text("utf-8"))
            self.assertIn("pub use probe_lower::core;", (probe / "upper/src/lib.rs").read_text("utf-8"))
            self.assertIn("pub use probe_lower::services::store;",
                          (probe / "upper/src/services/mod.rs").read_text("utf-8"))
            self.assertEqual(rewired, ["src/lib.rs:core", "src/services/mod.rs:store"])
            self.assertIn("[features]\ntest-support = []", (probe / "lower/Cargo.toml").read_text("utf-8"))
            deep = vistool.Layout(repo, split(["core::util"], ["(root)", "app", "core", "services"]), "LOW")
            for side in ("lower", "upper"):
                vistool.extract_head(repo, probe / "deep" / side)
            vistool.transform_lower(probe / "deep/lower", deep)
            self.assertEqual(vistool.transform_upper(probe / "deep/upper", deep), ["src/core.rs:util"])
            self.assertIn("pub mod util;", (probe / "deep/lower/src/core.rs").read_text("utf-8"))
            self.assertIn("pub use probe_lower::core::util;", (probe / "deep/upper/src/core.rs").read_text("utf-8"))
            # A key naming a module its group already owns: lower keeps the group's files as HEAD has them.
            same = vistool.Layout(repo, split(["core", "core::util", "services::store"], MINI_HIGH), "LOW")
            vistool.extract_head(repo, probe / "same")
            vistool.transform_lower(probe / "same", same)
            self.assertEqual((probe / "same/src/core.rs").read_text("utf-8"), (repo / "src/core.rs").read_text("utf-8"))
            status = subprocess.run(git[:3] + ["status", "--porcelain", "--ignored=no", "--", "src", "Cargo.toml"],
                                    capture_output=True, text=True, check=True).stdout
            self.assertEqual(status, "")
            self.assertEqual((repo / "src/lib.rs").read_text("utf-8"), (MINI / "src/lib.rs").read_text("utf-8"))

    def test_deep_key_goes_alone_and_its_ancestor_goes_whole(self):
        # The repo shape: an early crate owns one module inside a later crate's group.
        bridge = "src/services/discord/turn_bridge"
        tree = {"Cargo.toml": '[package]\nname = "t"\nversion = "0.1.0"\nedition = "2024"\n',
                "src/lib.rs": "mod app;\nmod services;\n", "src/app.rs": "", "src/services/mod.rs": "mod discord;\n",
                "src/services/discord/mod.rs": "mod turn_bridge;\n", f"{bridge}/tap.rs": "",
                f"{bridge}/mod.rs": "pub(in crate::services::discord) mod stale_resume;\nmod tap;\n",
                f"{bridge}/stale_resume.rs": ""}
        with tempfile.TemporaryDirectory() as tmp:
            for rel, text in tree.items():
                for side in ("repo", "LOW/lower", "LOW/upper", "HIGH/lower", "HIGH/upper"):
                    (Path(tmp) / side / rel).parent.mkdir(parents=True, exist_ok=True)
                    (Path(tmp) / side / rel).write_text(text)
            cut = split(["discord::turn_bridge::stale_resume"], ["discord::(mod.rs)", "discord::turn_bridge", "services"],
                        ROOT=["(root)", "app"])
            got = {}
            for crate in ("LOW", "HIGH"):
                layout = vistool.Layout(Path(tmp) / "repo", cut, crate)
                root = Path(tmp) / crate
                vistool.transform_lower(root / "lower", layout)
                got[crate] = (sorted(Path(f).name for f in layout.x_files), layout.leaves,
                              vistool.transform_upper(root / "upper", layout),
                              (root / "lower" / bridge / "mod.rs").read_text().split("\n")[0])
            path = "probe_lower::services::discord::turn_bridge"
            self.assertEqual(got["LOW"], (["stale_resume.rs"], ["discord::turn_bridge::stale_resume"],
                                          [f"{bridge}/mod.rs:stale_resume"], "pub mod stale_resume;"))
            self.assertEqual((Path(tmp) / "LOW/upper" / bridge / "mod.rs").read_text(),
                             f"pub use {path}::stale_resume;\nmod tap;\n")
            # Later the whole group goes, the deep key inside it, and its files stay as HEAD has them.
            self.assertEqual(got["HIGH"], (["mod.rs", "mod.rs", "mod.rs", "tap.rs"], ["discord::turn_bridge"],
                                           ["src/services/discord/mod.rs:turn_bridge"], tree[f"{bridge}/mod.rs"].split("\n")[0]))
            self.assertEqual((Path(tmp) / "HIGH/upper/src/services/discord/mod.rs").read_text(), f"pub use {path};\n")
            self.assertEqual((Path(tmp) / "HIGH/lower/src/services/discord/mod.rs").read_text(), "pub mod turn_bridge;\n")

    def test_test_gated_and_path_mods(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "src").mkdir()
            (root / "Cargo.toml").write_text('[package]\nname = "t"\n')
            gated = '#[cfg(any(test, feature = "test-support"))]\n'
            other = '#[cfg(any(test, feature = "optional"))]\n'
            (root / "src/lib.rs").write_text(other + "mod low_opt;\n" + gated + 'mod low_gated;\n'
                                             '#[cfg(test)]\nmod low_tests;\n#[path = "w/x.rs"]\nmod low;\nmod high;\n'
                                             '#[allow(dead_code)]\npub(crate) mod block {\n    mod low;\n    pub(crate) mod a;\n}\n')
            layout = SimpleNamespace(leaves=["block", "low", "low_gated", "low_opt", "low_tests"],
                                     file_of={(): "src/lib.rs"}, module_of={"src/lib.rs": ()})
            vistool.transform_upper(root, layout)
            # Lower builds without `test` and without `optional`: the re-export stays only where lower has the module.
            lower_has = 'any(any(), feature = "optional")'
            self.assertEqual((root / "src/lib.rs").read_text(),
                             other + f"#[cfg({lower_has})]\npub use probe_lower::low_opt;\n"
                             + other + f"#[cfg(not({lower_has}))]\nmod low_opt {{}}\n"
                             + gated + "pub use probe_lower::low_gated;\n"
                             "#[cfg(test)]\nmod low_tests {}\npub use probe_lower::low;\nmod high;\n"
                             "#[allow(dead_code)]\npub use probe_lower::block;\n")

    def test_inline_block_lines_belong_to_the_block(self):
        text = "mod a;\npub(crate) mod tui_o {\n    pub(crate) mod alarm;\n    #[cfg(test)]\n    mod t {\n        fn x() {}\n    }\n}\n"
        blocks = vistool.inline_blocks(text)
        self.assertEqual(vistool.block_at(blocks, 1)[2], ())
        self.assertEqual(vistool.block_at(blocks, 3)[2], ("tui_o",))
        self.assertEqual(vistool.block_at(blocks, 6)[2], ("tui_o", "t"))
        lines = text.split("\n")
        self.assertEqual(vistool.decl_hits(lines, "alarm", ("services", "tui_o"), ("services",)), [2])
        self.assertEqual(vistool.decl_hits(lines, "alarm", ("services",), ("services",)), [])
        self.assertEqual(vistool.decl_hits(lines, "tui_o", ("services",), ("services",)), [1])


class LoopGuard(unittest.TestCase):
    def scripted(self, rounds):
        """The tool on the mini fixture, cargo replaced by one (lower, upper) diagnostic list per round."""
        calls = []

        def fake(cargo, cwd, args, target_dir, own_root, manifest=None):
            calls.append(cwd.name)
            return [{"_test": False, **d, "_root": str(cwd)} for d in rounds[calls.count("lower") - 1][cwd.name == "upper"]], 0.1

        with tempfile.TemporaryDirectory() as tmp:
            repo, git = fixture_repo(Path(tmp))
            with mock.patch.object(vistool, "run_check", fake):
                ok, line, manifest = vistool.vistool(repo, repo / "partition.json", "LOW", out_dir=Path(tmp) / "out",
                                                     apply=True, log=lambda *a, **k: None)
            status = subprocess.run(git[:3] + ["status", "--porcelain"], capture_output=True, text=True).stdout
            return ok, line, calls, status, (Path(tmp) / "out" / "vistool-LOW.patch").exists(), manifest

    def test_unrepaired_lower_visibility_error_fails_closed(self):
        # VT1 fix1 review P2-1: a lower visibility error that resolves to no edit keeps upper from running,
        # so the run must stop as lower_probe_broken (not no_progress, never VISTOOL_OK).
        broken = {"level": "error", "code": {"code": "E0603"}, "message": "struct `Gone` is private", "spans": [],
                  "children": []}
        ok, line, calls, status, patched, _ = self.scripted([([broken], [])])
        self.assertFalse(ok)
        self.assertTrue(line.startswith("VISTOOL_FAIL lower_probe_broken "), line)
        self.assertEqual((calls, patched, status), (["lower"], False, ""))

    def test_unresolved_generated_reexport_fails_closed(self):
        # As test residue it would pass and, standing as an error, hide the field-privacy pass behind it.
        gone = {"level": "error", "code": {"code": "E0432"}, "message": "unresolved import `probe_lower::core`",
                "children": [], "_test": True,
                "spans": [{"is_primary": True, "file_name": "src/lib.rs", "line_start": 3, "line_end": 3,
                           "text": [{"text": "pub use probe_lower::core;"}]}]}
        ok, line, *_ = self.scripted([([], [gone])])
        self.assertEqual((ok, line.split(" crate=")[0]), (False, "VISTOOL_FAIL upper_probe_broken"))


    def test_leaked_type_goes_as_far_as_the_interface_and_no_further(self):
        token = leak("crate-private", "Token", "src/core.rs", "pub(crate) struct Token;")
        thing = lambda descr: leak(descr, "Thing", "src/services/store.rs", "pub(super) struct Thing {")
        ok, line, *_, manifest = self.scripted([([token, thing("restricted")], []), ([thing("crate-private")], []), ([], [])])
        self.assertTrue(ok, (line, manifest.get("fail")))
        self.assertEqual([(e["iter"], e["after"]) for e in manifest["edits"]],
                         [(1, "pub struct Token;"), (1, "pub(crate) struct Thing {"), (2, "pub struct Thing {")])
        # A crate-wide interface: rustc stops reporting, and the type stays at `pub(crate)`.
        ok, line, *_, manifest = self.scripted([([thing("restricted")], []), ([], [])])
        self.assertEqual((ok, [e["after"] for e in manifest["final_edits"]]), (True, ["pub(crate) struct Thing {"]))
        # No declaration to widen: the lower error stands, it is never residue.
        gone = leak("crate-private", "Gone", "src/core.rs", "pub(crate) struct Token;")
        ok, line, *_ = self.scripted([([gone], [])])
        self.assertEqual((ok, line.split(" crate=")[0]), (False, "VISTOOL_FAIL lower_probe_broken"))


class TestCfgList(unittest.TestCase):
    def test_recorded_test_cfg_diagnostic_becomes_a_row(self):
        w, r = widener()
        diag = next(d for d in load("diags-residue.json") if "fixture_value" in d["message"])
        self.assertEqual(vistool.classify_residue(diag, vistool.restricted_defs(Mini.get(), w.lines)), "test_cfg")
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "list.json"
            summary = vistool_testcfg.write_list(out, [diag], r, Mini.get(), w.lines, {"crate": "LOW"})
            doc = json.loads(out.read_text())
        self.assertEqual(summary, {"file": "list.json", "count": 1, "resolved": 1})
        self.assertEqual(doc["by_code"], {"E0425": 1})
        self.assertEqual(doc["items"], [{
            "use_site": "src/app.rs:41", "use_source": "", "item": "crate::core::fixture_value",
            "name": "fixture_value", "definition": "src/core.rs:52", "definition_from": "rustc",
            "definition_candidates": 0, "code": "E0425",
            "message": "cannot find function `fixture_value` in module `crate::core`"}])

    def test_item_paths_per_code(self):
        cases = [("E0432", "unresolved import `crate::a::helper`", ("helper", "crate::a::helper")),
                 ("E0433", "failed to resolve: could not find `fixtures` in `support`", ("fixtures", "support::fixtures")),
                 ("E0412", "cannot find type `Rig` in module `crate::a`", ("Rig", "crate::a::Rig")),
                 ("E0599", "no function or associated item named `for_test` found for struct `Store` in the current "
                           "scope", ("for_test", "Store::for_test")),
                 ("E0425", "cannot find function `h` in this scope", ("h", "h"))]
        for code, msg, want in cases:
            self.assertEqual(vistool_testcfg.item_path({"code": {"code": code}, "message": msg}), want, msg)

    def test_gated_declarations_by_scan(self):
        src = ["pub(crate) fn plain() {}", "#[cfg(all(test, unix))]", "mod tests {", "    pub(crate) fn h() {}", "}",
               "#[cfg(test)]", "impl Store {", "    pub(crate) fn for_test() {}", "}", "#[cfg(any(test, feature = \"x\"))]",
               "pub(crate) fn either() {}", "#[cfg(test)]", "pub(crate) fn h() {}"]
        layout = SimpleNamespace(x_files={"src/a.rs"}, test_only=set(), module_at=lambda rel, line: ("a",))
        defs = vistool_testcfg.gated_defs(layout, lambda rel: src)
        self.assertEqual({k: [x[1] for x in v] for k, v in defs.items()},
                         {"tests": [3], "h": [4, 13], "for_test": [8]})
        self.assertEqual(vistool_testcfg.definition("for_test", "Store::for_test", defs)[0][1], 8)
        self.assertEqual(vistool_testcfg.definition("h", "h", defs), (None, 2))
        # An inline `mod test_support {` line belongs to the block it opens; the path names its parent.
        mods = {"test_support": [("src/a.rs", 3, ("a", "test_support")), ("src/b.rs", 9, ("b", "test_support"))]}
        self.assertEqual(vistool_testcfg.definition("test_support", "crate::b::test_support", mods)[0][1], 9)


def real_repo(root, extra=None):
    repo = root / "realws"
    shutil.copytree(FX / "realws", repo)
    for rel, text in (extra or {}).items():
        (repo / rel).parent.mkdir(parents=True, exist_ok=True)
        (repo / rel).write_text(text)
    git = ["git", "-C", str(repo), "-c", "user.name=vistool", "-c", "user.email=vistool@example.invalid"]
    subprocess.run(git[:3] + ["init", "-q"], check=True)
    subprocess.run(git[:3] + ["add", "-A"], check=True)
    subprocess.run(git + ["commit", "-qm", "fixture"], check=True)
    return repo, git


class RealMode(unittest.TestCase):
    def run_real(self, lower, upper=".", extra=None):
        with tempfile.TemporaryDirectory() as tmp:
            repo, _ = real_repo(Path(tmp), extra)
            with mock.patch.object(vistool, "run_check", side_effect=AssertionError("cargo must not run")):
                return vistool_real.real(repo, lower, upper, out_dir=Path(tmp) / "out", log=lambda *a, **k: None)

    def test_missing_member_fails_without_falling_back_to_shadow(self):
        for lower, upper, extra in (("crates/nope", "upper", None), ("upper", "upper", None),
                                    ("stray", "upper", {"stray/Cargo.toml": '[package]\nname = "stray"\n',
                                                        "stray/src/lib.rs": ""})):
            ok, line, manifest = self.run_real(lower, upper, extra)
            self.assertFalse(ok)
            self.assertTrue(line.startswith("VISTOOL_FAIL no_member mode=real "), line)
            self.assertEqual(manifest["rounds"], [])

    def test_cli_real_mode_needs_a_member(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, _ = real_repo(Path(tmp))
            for argv in (["--real", str(repo), "--out-dir", tmp],
                         ["--real", str(repo), "--lower", "crates/adk-krx", "--out-dir", tmp]):
                buf = io.StringIO()
                with contextlib.redirect_stdout(buf), contextlib.redirect_stderr(io.StringIO()), \
                        mock.patch.object(vistool, "run_check", side_effect=AssertionError("cargo must not run")):
                    self.assertEqual(vistool.main(argv), 2)
                self.assertTrue(buf.getvalue().startswith("VISTOOL_FAIL no_member mode=real "), buf.getvalue())

    def test_symlinked_workspace_path(self):
        # VT2 fix1 review P1-5: macOS temp dirs are symlinks (/var -> /private/var); a symlinked repo path
        # must resolve like the real one.
        with tempfile.TemporaryDirectory() as tmp:
            repo, _ = real_repo(Path(tmp))
            link = Path(tmp) / "ws_link"
            link.symlink_to(repo, target_is_directory=True)
            self.assertEqual(sorted(vistool_real.workspace_members(link)), ["lower", "upper"])
            members = vistool_real.workspace_members(link)
            layout = vistool_real.RealLayout(link, members, "lower", "upper", "lower")
            self.assertEqual(layout.side_of("lower/src/core.rs", 3), "x")
            res = vistool_real.RealResolver(link, members, "lower", "upper")
            self.assertEqual(res.rel("lower/src/core.rs", link), ("lower", "lower/src/core.rs"))

    def test_layout_and_resolver_split_by_member(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, _ = real_repo(Path(tmp))
            members = vistool_real.workspace_members(repo)
            self.assertEqual(sorted(members), ["lower", "upper"])
            layout = vistool_real.RealLayout(repo, members, "lower", "upper", "lower")
            self.assertEqual(layout.x_files, {"lower/src/lib.rs", "lower/src/core.rs"})
            self.assertEqual(layout.module_of["lower/src/core.rs"], ("core",))
            self.assertEqual(layout.side_of("lower/src/core.rs", 3), "x")
            self.assertEqual(layout.side_of("upper/src/lib.rs", 3), "above")
            self.assertIsNone(layout.side_of("Cargo.toml"))
            res = vistool_real.RealResolver(repo, members, "lower", "upper")
            self.assertEqual(res.rel("lower/src/core.rs", repo), ("lower", "lower/src/core.rs"))
            self.assertEqual(res.rel("upper/src/lib.rs", repo), ("upper", "upper/src/lib.rs"))
            self.assertEqual(res.rel("/rustc/x/lib.rs", repo)[0], None)

    def test_residue_left_after_convergence_fails(self):
        probe = vistool_real.RealProbe(Path("/nonexistent"), "lower", "upper")
        probe.accept({"residue": {k: {"count": 0} for k in vistool.RESIDUES}})
        with self.assertRaises(vistool.VisFail) as caught:
            probe.accept({"residue": dict({k: {"count": 0} for k in vistool.RESIDUES}, test_cfg={"count": 2})})
        self.assertEqual((caught.exception.reason, caught.exception.detail), ("residue", "test_cfg=2"))


# Wrappers and incremental settings of the caller (CI's global RUSTC_WRAPPER=sccache, cargo-shim's
# CARGO_INCREMENTAL=1, a ~/.cargo/config.toml rustc-wrapper) must not reach the throwaway fixture build.
CLEAN_CARGO_ENV = {"RUSTC_WRAPPER": "", "CARGO_BUILD_RUSTC_WRAPPER": "", "RUSTC_WORKSPACE_WRAPPER": "",
                   "CARGO_INCREMENTAL": "0", "CARGO_TARGET_DIR": ""}


# Opt-in: the Script checks guards shard runs without a Rust toolchain contract; lanes and the cargo
# shard set AGENTDESK_VISTOOL_E2E=1.
@unittest.skipUnless(os.environ.get("AGENTDESK_VISTOOL_E2E") == "1" and shutil.which("cargo") and shutil.which("git"),
                     "set AGENTDESK_VISTOOL_E2E=1 (cargo and git required)")
@mock.patch.dict(os.environ, CLEAN_CARGO_ENV)
class EndToEnd(unittest.TestCase):
    def test_upward_reexport_stays_and_the_run_completes(self):
        # P1-2: an X re-export of an above pub(crate) item is cut residue; every other widening still lands.
        with tempfile.TemporaryDirectory() as tmp:
            repo, git = fixture_repo(Path(tmp))
            with open(repo / "src/app.rs", "a") as f:
                f.write("\npub(crate) struct AboveSecret;\n")
            with open(repo / "src/core.rs", "a") as f:
                f.write("\npub(crate) use crate::app::AboveSecret;\n")
            web = (repo / "src/services/web.rs").read_text()
            (repo / "src/services/web.rs").write_text(
                web.replace("    made.size\n", "    let _secret = crate::core::AboveSecret;\n    made.size\n"))
            subprocess.run(git + ["commit", "-qam", "upward re-export"], check=True)
            out = Path(tmp) / "out"
            ok, line, manifest = vistool.vistool(repo, repo / "partition.json", "LOW", out_dir=out, apply=True,
                                                 log=lambda *a, **k: None)
            self.assertTrue(ok, (line, manifest.get("fail"), manifest.get("unresolved")))
            # The upper residue (E0603 on AboveSecret) masks rustc's field-privacy pass, so `Span.lo` (E0451)
            # is the one widening of the clean run that cannot show up here; the manifest says so.
            self.assertRegex(line, r" edits=21 cut_residue=1 ")
            self.assertTrue(manifest["privacy_pass_masked"])
            self.assertNotIn("pub lo: u32", (repo / "src/core.rs").read_text())
            self.assertIn("pub(crate) use crate::app::AboveSecret;", (repo / "src/core.rs").read_text())
            check = subprocess.run(["cargo", "check", "--all-targets", "--quiet"], cwd=repo, capture_output=True,
                                   text=True, env=dict(os.environ, CARGO_TARGET_DIR=str(Path(tmp) / "t")))
            self.assertEqual(check.returncode, 0, check.stderr[-2000:])

    def test_real_workspace_converges_and_is_idempotent(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, git = real_repo(Path(tmp))
            out = Path(tmp) / "out"
            ok, line, manifest = vistool_real.real(repo, "lower", "upper", out_dir=out, apply=True,
                                                   log=lambda *a, **k: None)
            self.assertTrue(ok, (line, manifest.get("fail"), manifest.get("unresolved"), manifest.get("residue")))
            self.assertRegex(line, r"^VISTOOL_OK mode=real crate=lower upper=upper base=[0-9a-f]{40} iters=\d+ "
                                   r"files=2 edits=5 residue=0 glob_risk=0$")
            self.assertTrue(manifest["equiv"].startswith("class=vis"), manifest["equiv"])
            core = (repo / "lower/src/core.rs").read_text()
            for widened in ("pub struct Config {", "    pub limit: u32,", "    pub fn new() -> Self {",
                            "pub fn helper() -> u32 {", "#[cfg(test)]\npub(crate) fn fixture_value"):
                self.assertIn(widened, core)
            self.assertIn("pub mod core;", (repo / "lower/src/lib.rs").read_text())
            diff = subprocess.run(git[:3] + ["diff", "--stat", "--", "upper", "Cargo.toml"], capture_output=True, text=True)
            self.assertEqual(diff.stdout, "")
            self.assertEqual(json.loads((out / "vistool-real-lower-test-cfg.json").read_text())["count"], 0)
            self.assertFalse((repo / "target" / "crate_split_probe").exists())
            check = subprocess.run(["cargo", "check", "--workspace", "--all-targets", "--quiet"], cwd=repo,
                                   capture_output=True, text=True, env=dict(os.environ, CARGO_TARGET_DIR=str(Path(tmp) / "t")))
            self.assertEqual(check.returncode, 0, check.stderr[-2000:])
            subprocess.run(git + ["commit", "-qam", "widened"], check=True)
            ok, line, _ = vistool_real.real(repo, "lower", "upper", out_dir=out, log=lambda *a, **k: None)
            self.assertTrue(ok, line)
            self.assertIn(" iters=1 files=0 edits=0 residue=0 ", line)

    def test_real_test_cfg_residue_fails_with_the_list(self):
        upper = (FX / "realws/upper/src/lib.rs").read_text().replace(
            "assert_eq!(super::total(), 5);", "assert_eq!(super::total() + lower::core::fixture_value(), 12);")
        with tempfile.TemporaryDirectory() as tmp:
            repo, git = real_repo(Path(tmp), {"upper/src/lib.rs": upper})
            out = Path(tmp) / "out"
            ok, line, manifest = vistool_real.real(repo, "lower", "upper", out_dir=out, apply=True,
                                                   log=lambda *a, **k: None)
            self.assertFalse(ok)
            self.assertTrue(line.startswith("VISTOOL_FAIL residue mode=real crate=lower "), line)
            self.assertEqual(manifest["fail"]["detail"], "test_cfg=1")
            doc = json.loads((out / "vistool-real-lower-test-cfg.json").read_text())
            self.assertEqual([(i["use_site"], i["item"], i["definition"], i["definition_from"], i["code"])
                              for i in doc["items"]],
                             [("upper/src/lib.rs:11", "lower::core::fixture_value", "lower/src/core.rs:16", "rustc",
                               "E0425")])
            self.assertIn("| test_cfg | 1 | T/M test-support |", "\n".join(vistool_real.residue_table(manifest)))
            status = subprocess.run(git[:3] + ["status", "--porcelain"], capture_output=True, text=True).stdout
            self.assertEqual(status, "")
            self.assertTrue((out / "vistool-real-lower-partial.patch").read_text())

    def test_reexport_through_an_above_module_leaves_upper_untouched(self):
        # VT2 review P1-6: LOW -> UP module re-export -> LOW definition; --apply must not edit the UP file.
        with tempfile.TemporaryDirectory() as tmp:
            repo, git = fixture_repo(Path(tmp))
            with open(repo / "src/app.rs", "a") as f:
                f.write("\npub(crate) use crate::core::util;\n")
            with open(repo / "src/core.rs", "a") as f:
                f.write("\npub(crate) use crate::app::util::tidy as app_tidy;\n")
            web = (repo / "src/services/web.rs").read_text()
            (repo / "src/services/web.rs").write_text(
                web.replace("    made.size\n", "    made.size + crate::core::app_tidy(1)\n"))
            subprocess.run(git + ["commit", "-qam", "re-export through app"], check=True)
            app = (repo / "src/app.rs").read_text()
            ok, line, manifest = vistool.vistool(repo, repo / "partition.json", "LOW", out_dir=Path(tmp) / "out",
                                                 apply=True, log=lambda *a, **k: None)
            self.assertTrue(ok, (line, manifest.get("fail"), manifest.get("unresolved")))
            self.assertEqual((repo / "src/app.rs").read_text(), app)
            self.assertNotIn("src/app.rs", {e["file"] for e in manifest["final_edits"]})
            self.assertEqual(manifest["residue"]["cut_residue"]["count"], 1, line)

    def test_gated_item_behind_a_dev_dependency_is_widened(self):
        # Lower builds with `test-support` on, so a gated item an upper test reads is a visibility edit.
        with tempfile.TemporaryDirectory() as tmp:
            repo, git = fixture_repo(Path(tmp))
            for rel, text in (("dd/Cargo.toml", '[package]\nname = "dd"\nversion = "0.1.0"\nedition = "2024"\n'),
                              ("dd/src/lib.rs", "pub const SEVEN: u32 = 7;\n"),
                              ("Cargo.toml", '\n[dev-dependencies]\ndd = { path = "dd" }\n'),
                              ("src/core.rs", '\n#[cfg(any(test, feature = "test-support"))]\n'
                                              "pub(crate) fn gated() -> u32 {\n    dd::SEVEN\n}\n"
                                              '\n#[cfg(any(test, feature = "test-support"))]\n'
                                              "pub(crate) struct Hidden;\n"
                                              '\n#[cfg(any(test, feature = "test-support"))]\n'
                                              "pub fn exposes() -> Hidden {\n    Hidden\n}\n"),
                              ("src/app.rs", "\n#[cfg(test)]\nmod gated_tests {\n    #[test]\n    fn reads_gated() {\n"
                                             "        assert_eq!(crate::core::gated(), 7);\n    }\n}\n")):
                (repo / rel).parent.mkdir(parents=True, exist_ok=True)
                with open(repo / rel, "a") as f:
                    f.write(text)
            subprocess.run(git[:3] + ["add", "-A"], check=True)
            subprocess.run(git + ["commit", "-qm", "gated"], check=True)
            ok, line, manifest = vistool.vistool(repo, repo / "partition.json", "LOW", out_dir=Path(tmp) / "out",
                                                 apply=True, log=lambda *a, **k: None)
            self.assertTrue(ok and " test_cfg=0 " in line, (line, manifest.get("fail")))
            self.assertIn("pub fn gated() -> u32 {", (repo / "src/core.rs").read_text())
            # Upper never names `Hidden`: only lower's own check, built with the feature, reports it.
            self.assertIn("pub struct Hidden;", (repo / "src/core.rs").read_text())

    def test_nested_deep_key_and_feature_gated_deep_leaf(self):
        gated = (("src/core.rs", '\n#[cfg(any(test, feature = "test-support"))]\npub(crate) mod gated;\n'),
                 ("src/core/gated.rs", "pub(crate) fn hidden() -> u32 {\n    7\n}\n"),
                 ("src/app.rs", "\n#[cfg(test)]\nmod gated_tests {\n    #[test]\n    fn reads_gated() {\n"
                                "        assert_eq!(crate::core::gated::hidden(), 7);\n    }\n}\n"))
        # A group only another feature (or `test`) opens: lower has no such module, and a test-only field use
        # behind it must still be found. `sup` is the control `test-support` opens in lower.
        other = (("src/lib.rs", '\n#[cfg(any(test, feature = "optional"))]\nmod opt;\n'
                                '#[cfg(any(test, feature = "test-support"))]\nmod sup;\n'),
                 ("src/opt.rs", ""), ("src/sup.rs", "pub fn one() -> u32 {\n    1\n}\n"),
                 ("src/core.rs", "\npub struct Probe {\n    pub(crate) depth: u32,\n}\n"),
                 ("src/app.rs", "\n#[cfg(test)]\nmod sup_tests {\n    #[test]\n    fn reads_sup() {\n"
                                "        let _probe = crate::core::Probe { depth: 2 };\n"
                                "        assert_eq!(crate::sup::one(), 1);\n    }\n}\n"))
        # HIGH names LOW's `Status`; once that is `pub`, the error type its `FromStr` impl names leaks (E0446).
        leaked = (("src/services/store.rs", "\npub(super) enum Status {\n    On,\n}\n\npub(super) struct BadStatus;\n\n"
                                            "impl std::str::FromStr for Status {\n    type Err = BadStatus;\n\n"
                                            "    fn from_str(_: &str) -> Result<Self, Self::Err> {\n"
                                            "        Ok(Status::On)\n    }\n}\n"),
                  ("src/services/web.rs", "\npub(super) fn parses() -> bool {\n"
                                          "    \"on\".parse::<super::store::Status>().is_ok()\n}\n"))
        # (LOW, HIGH, files appended to the fixture, (summary part, file, widened line)) of a run that applies.
        for low, high, extra, want in (
                (["core", "core::util", "services::store"], MINI_HIGH, (), None),
                (["core::gated"], MINI_HIGH + ["core", "services::store"], gated,
                 (" edits=1 cut_residue=0 test_cfg=0 ", "src/core/gated.rs", "pub fn hidden() -> u32 {")),
                (["core", "opt", "services::store", "sup"], MINI_HIGH, other,
                 (" cut_residue=0 test_cfg=0 ", "src/core.rs", "    pub depth: u32,")),
                (["core", "services::store"], MINI_HIGH, leaked,
                 (" cut_residue=0 test_cfg=0 ", "src/services/store.rs", "pub struct BadStatus;"))):
            with self.subTest(low=low), tempfile.TemporaryDirectory() as tmp:
                repo, git = fixture_repo(Path(tmp))
                for rel, text in extra:
                    with open(repo / rel, "a") as f:
                        f.write(text)
                subprocess.run(git[:3] + ["add", "-A"], check=True)
                subprocess.run(git + ["commit", "-qm", "deep", "--allow-empty"], check=True)
                split(low, high, path=Path(tmp) / "partition.json")
                out = Path(tmp) / "out"
                ok, line, manifest = vistool.vistool(repo, Path(tmp) / "partition.json", "LOW", out_dir=out,
                                                     apply=True, log=lambda *a, **k: None)
                self.assertTrue(ok, (line, manifest.get("fail"), manifest.get("unresolved")))
                if not extra:
                    # A key naming a module the group already owns changes nothing.
                    self.assertEqual((out / "vistool-LOW.patch").read_text("utf-8"),
                                     (FX / "mini.expected.patch").read_text("utf-8"))
                    continue
                # A leaf `test-support` opens is re-exported from lower, so its item is a visibility edit.
                self.assertIn(want[0], line)
                self.assertIn(want[2], (repo / want[1]).read_text())

    def test_fixture_shadow_run(self):
        # Outside the repository on purpose: a throwaway fixture crate, not an agentdesk build.
        with tempfile.TemporaryDirectory() as tmp:
            repo, git = fixture_repo(Path(tmp))
            out = Path(tmp) / "out"
            ok, line, manifest = vistool.vistool(repo, repo / "partition.json", "LOW", out_dir=out, apply=True,
                                                 log=lambda *a, **k: None)
            self.assertTrue(ok, (line, manifest.get("fail"), manifest.get("unresolved")))
            self.assertRegex(line, r"^VISTOOL_OK crate=LOW base=[0-9a-f]{40} iters=\d+ files=3 edits=22 "
                                   r"cut_residue=0 test_cfg=0 impl_residue=0 glob_risk=2$")
            expected = (FX / "mini.expected.patch").read_text("utf-8")
            check = subprocess.run(git[:3] + ["apply", "--check", "-R", str(out / "vistool-LOW.patch")],
                                   capture_output=True, text=True)
            self.assertEqual(check.returncode, 0, check.stderr)  # applied already, so it must reverse cleanly
            self.assertEqual((out / "vistool-LOW.patch").read_text("utf-8"), expected)
            self.assertFalse((repo / "target" / "crate_split_probe").exists())
            self.assertFalse(manifest["privacy_pass_masked"])
            subprocess.run(git + ["commit", "-qam", "widened"], check=True)
            ok, line, manifest = vistool.vistool(repo, repo / "partition.json", "LOW", out_dir=out,
                                                 log=lambda *a, **k: None)
            self.assertTrue(ok, line)
            self.assertIn(" iters=1 files=0 edits=0 ", line)


if __name__ == "__main__":
    unittest.main()
