"""vistool: recorded rustc diagnostics -> definition + modifier, residue classes, glob safety, probe isolation.

`fixtures/vistool/mini` is a one-crate tree that partition.json splits into LOW (core, services::store) and
HIGH. diags-vis.json holds every visibility diagnostic a shadow run of it produced (paths under `<PROBE>/`);
diags-residue.json holds the residue a variant with a hard cut, a foreign impl and a test-only item produced.
The end-to-end case runs the real tool on a throwaway copy outside the repository (cargo required).
"""

import copy
import json
from pathlib import Path
import random
import shutil
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
import vistool  # noqa: E402

FX = HERE / "fixtures" / "vistool"
MINI = FX / "mini"
UPPER = Path("/nonexistent-vistool-upper")


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
        self.assertEqual(vistool.classify_residue({"code": {"code": "E0599"}, "message": "x", "_test": False},
                                                  restricted), "unclassified")

    def test_mutant_ignoring_defined_here_is_caught(self):
        with mock.patch.object(vistool, "definition_span", lambda diag, marks=None: None):
            self.assertNotEqual(mismatches(), [])


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

    def test_mutant_without_glob_scan_is_caught(self):
        with mock.patch.object(vistool, "glob_imports", lambda layout, read: []):
            self.assertFalse(any(r["fail"] for r in self.risks("struct Error;")))


class Partition(unittest.TestCase):
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
        self.assertEqual({k: len(v) for k, v in p["crates"].items()}, {"KRX": 82, "KE": 88, "DL": 85, "ROOT": 46})

    def test_manifests(self):
        text = '[package]\nname = "agentdesk"\n\n[workspace]\nmembers = ["crates/x"]\n\n[lib]\npath = "src/lib.rs"\n\n' \
               '[[bin]]\nname = "agentdesk"\npath = "src/main.rs"\n\n[dependencies]\nx = { path = "crates/x" }\n'
        low = vistool.lower_manifest(text, "src/probe_lower_lib.rs")
        self.assertIn('name = "probe_lower"', low)
        self.assertNotIn("[[bin]]", low)
        self.assertIn('path = "src/probe_lower_lib.rs"', low)
        up = vistool.upper_manifest(text)
        self.assertIn('probe_lower = { path = "../lower" }', up)
        self.assertIn('x = { path = "../lower/crates/x" }', up)
        self.assertIn("members = []", up)
        self.assertIn("[[bin]]", up)


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
            status = subprocess.run(git[:3] + ["status", "--porcelain", "--ignored=no", "--", "src", "Cargo.toml"],
                                    capture_output=True, text=True, check=True).stdout
            self.assertEqual(status, "")
            self.assertEqual((repo / "src/lib.rs").read_text("utf-8"), (MINI / "src/lib.rs").read_text("utf-8"))

    def test_test_gated_and_path_mods(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "src").mkdir()
            (root / "Cargo.toml").write_text('[package]\nname = "t"\n')
            (root / "src/lib.rs").write_text('#[cfg(test)]\nmod low_tests;\n#[path = "w/x.rs"]\nmod low;\nmod high;\n'
                                             '#[allow(dead_code)]\npub(crate) mod block {\n    mod low;\n    pub(crate) mod a;\n}\n')
            layout = SimpleNamespace(leaves=["block", "low", "low_tests"], file_of={(): "src/lib.rs"},
                                     module_of={"src/lib.rs": ()})
            vistool.transform_upper(root, layout)
            self.assertEqual((root / "src/lib.rs").read_text(),
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


@unittest.skipUnless(shutil.which("cargo") and shutil.which("git"), "cargo and git required")
class EndToEnd(unittest.TestCase):
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
