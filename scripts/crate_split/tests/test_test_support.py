"""test_support_codemod.py, test_support_verify.py and the `test-support` cfg equivalence (#7353 KRX-TS)."""

import io
import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from contextlib import redirect_stdout
from unittest import mock
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
sys.path.insert(0, str(HERE.parents[1] / "ci"))
import equiv_class  # noqa: E402
import rust_test_sources as rts  # noqa: E402
import test_support_codemod as codemod  # noqa: E402
import test_support_verify as verify  # noqa: E402

TS = 'any(test, feature = "test-support")'
PARTITION = {"base": "fixture", "order": ["KRX", "DL"], "crates": {"KRX": ["core", "decoy"], "DL": ["(root)", "app"]}}
CORE = """\
pub(crate) struct Point;

#[cfg(test)]
pub(crate) fn helper() {}

    #[cfg(all(test, unix))] // trailing note stays
    pub(crate) fn unix_helper() {}

#[cfg(test)]
#[cfg(unix)]
pub(crate) fn two_line() {}

#[cfg(test)]
pub(crate) struct Guard;

#[cfg(test)]
impl Drop for Guard {
    fn drop(&mut self) {}
}

#[cfg(test)]
impl Guard {
    pub(crate) fn set() -> Self {
        Guard
    }
}

pub(crate) fn render() -> String {
    format!(
        "{}", 1
    )
}

#[cfg(test)]
pub(crate) fn after_macro() {}

#[cfg(test)]
mod spy {
    pub(crate) struct Spy;
}

#[cfg(test)]
pub(crate) use spy::Spy;

#[cfg(test)]
pub(crate) fn kept() {}

#[cfg(test)]
pub(crate) fn unused() {}

#[cfg(test)]
impl Point {
    pub(crate) fn probes_mut(&mut self) {}
}

#[cfg(test)]
pub(crate) mod fixtures {
    #[cfg(test)]
    pub(crate) fn seeded() {}
}
"""
APP = """\
#[cfg(test)]
mod tests {
    use crate::core::{helper, kept, two_line, unix_helper, Guard, Point};
    use crate::core::fixtures::seeded;
    use crate::core::Spy;

    #[test]
    fn uses() {
        helper();
        unix_helper();
        two_line();
        kept();
        seeded();
        let _g = Guard;
        let _s = Guard::set();
        crate::core::Point::probe();
        Point.probes_mut();
    }
}
"""
# A private namesake in another module's tests: `Point::probe` in app must not reach it (it would open `tests`).
DECOY = """\
#[cfg(test)]
mod tests {
    pub(crate) struct Point;

    impl Point {
        pub(crate) fn probe() {}
    }

    #[test]
    fn t() {}
}
"""
# Three targets of one re-export under different gates (Sol krxts-1 P1 shape: cutover.rs:26-29).
REEXPORT = """
pub(crate) mod gate {
    #[cfg(test)]
    pub(crate) fn first() {}

    #[cfg(all(test, unix))]
    pub(crate) fn second() {}

    #[cfg(test)]
    pub(crate) fn third() {}
}

#[cfg(test)]
pub(crate) use gate::{
    first, second, third,
};
"""
# claims_judged shape: a reader, the static it reads and the gated statement that records into it.
BUNDLE = """
pub(crate) fn record(value: u64) {
    #[cfg(test)]
    RECORDS.lock().unwrap().push(value);
    let _ = value;
}

#[cfg(test)]
static RECORDS: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn judged() -> Vec<u64> {
    RECORDS.lock().unwrap().clone()
}
"""
# A namesake of `helper` as macro tokens and raw string text: data, not attributes.
FAKE = """
pub(crate) fn text() -> &'static str {
    stringify! {
        #[cfg(test)]
        pub(crate) fn helper() {}
    }
}

pub(crate) const RAW: &str = r#"
#[cfg(test)]
pub(crate) fn helper() {}
"#;
"""
COMPOUND = """
#[cfg(all(test, test))]
pub(crate) fn twice() {}

#[cfg(not(all(test, unix)))]
pub(crate) fn plat() {}

#[cfg(all(test, unix))]
pub(crate) fn plat() {}

#[cfg_attr(not(test), cfg(unix))]
pub(crate) fn attr_twin() {}

#[cfg(test)]
pub(crate) fn attr_twin() {}
"""
# A re-export whose module exists but whose target is a static inside a gated `thread_local!`.
TLS_REEXPORT = """
pub(crate) mod clear {
    #[cfg(test)]
    thread_local! {
        pub(crate) static AFTER: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
    }
}

#[cfg(test)]
pub(crate) use clear::AFTER;
"""
# A glob target and a macro-built path: the tool cannot follow them, so the declaration holds whole.
GLOB_USE = """#[cfg(test)]
pub(crate) mod extra {
    pub(crate) fn fourth() {}
}

#[cfg(test)]
pub(crate) use self::{gate::first, extra::*};
"""
MACRO_USE = """macro_rules! reexport {
    ($name:ident) => {
        pub(crate) use $crate::core::gate::$name;
    };
}
"""
HELD_DROP = """
#[cfg(test)]
pub(crate) struct Restore;

#[cfg(all(test, test))]
impl Drop for Restore {
    fn drop(&mut self) {}
}
"""
TWO_TYPES = """
pub(crate) struct Alpha;
pub(crate) struct Beta;

impl Alpha {
    #[cfg(test)]
    pub(crate) fn probe() {}
}

impl Beta {
    #[cfg(not(all(test, unix)))]
    pub(crate) fn probe() {}
}
"""
TESTS_MOD = """
#[cfg(test)]
pub(crate) mod tests {
    pub(crate) fn rig() {}

    #[test]
    fn inner() {}
}
"""
TWIN = """
#[cfg(not(test))]
pub(crate) fn registry() {}

#[cfg(test)]
pub(crate) fn registry() {}
"""


def exclusions(*items, bundles=()):
    """Entries in src/core.rs: a label (one item gated `test`) or (label, [cfg of each item it must match])."""
    rows = [(i, ["test"]) if isinstance(i, str) else i for i in items]
    return {"entries": [{"file": "src/core.rs", "item": i, "cfg": c, "reason": "fixture"} for i, c in rows],
            "bundles": [{"name": n, "file": "src/core.rs",
                         "items": [{"item": i, "cfg": "test"} if isinstance(i, str) else {"item": i[0], "cfg": i[1]}
                                   for i in m]} for n, m in bundles]}


class Fixture:
    """A git repository with one KRX module (core) and one DL module (app) whose test uses core's gates."""

    def __init__(self, root, core=CORE, app=APP, exclude=("fn kept",), bundles=()):
        self.repo = Path(root) / "repo"
        files = {"Cargo.toml": '[package]\nname = "fx"\nversion = "0.1.0"\nedition = "2021"\n',
                 "src/lib.rs": "mod app;\nmod core;\nmod decoy;\n", "src/core.rs": core, "src/app.rs": app,
                 "src/decoy.rs": DECOY,
                 "scripts/crate_split/partition.json": json.dumps(PARTITION),
                 verify.EXCLUDE: json.dumps(exclusions(*exclude, bundles=bundles))}
        for rel, text in files.items():
            (self.repo / rel).parent.mkdir(parents=True, exist_ok=True)
            (self.repo / rel).write_text(text)
        self.git("init", "-q")
        self.commit("base")

    def git(self, *args):
        cmd = ["git", "-C", str(self.repo), "-c", "user.name=t", "-c", "user.email=t@t", "-c", "core.hooksPath=/dev/null"]
        return subprocess.run([*cmd, *args], check=True, capture_output=True, text=True).stdout.strip()

    def commit(self, message):
        self.git("add", "-A")
        self.git("commit", "-q", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD")

    def codemod(self, *args):
        out = io.StringIO()
        with redirect_stdout(out):
            rc = codemod.main(["--tree", str(self.repo), "--exclude", str(self.repo / verify.EXCLUDE),
                               "--bundles", str(self.repo / verify.BUNDLES), *args])
        return rc, out.getvalue()

    def verify(self, base, head, compile_package=None):
        out = io.StringIO()
        with redirect_stdout(out):
            rc = verify.verify(self.repo, base, head, compile_package)
        return rc, out.getvalue()

    def core(self):
        return (self.repo / "src/core.rs").read_text()


class Rewrite(unittest.TestCase):
    def test_line_rules(self):
        cases = [("#[cfg(test)]", f"#[cfg({TS})]"),
                 ("    #[cfg(test)] // why", f"    #[cfg({TS})] // why"),
                 ("#[cfg(all(test, unix))]", f"#[cfg(all({TS}, unix))]"),
                 ('#[cfg(all(unix, test, target_os = "linux"))]', f'#[cfg(all(unix, {TS}, target_os = "linux"))]'),
                 ("#[cfg(any(test, unix))]", None), ("#[cfg(not(test))]", None), ("#[cfg(unix)]", None),
                 (f"#[cfg({TS})]", None), (f"#[cfg(all({TS}, unix))]", None),
                 ("// #[cfg(test)]", None), ("#[allow(dead_code)] // only under #[cfg(test)]", None),
                 ("#[cfg(any(test, all(unix, test)))]", None)]
        for line, want in cases:
            self.assertEqual(codemod.rewrite(line), want, line)
            # The verifier's own rule agrees on every rewrite the codemod makes.
            if want:
                self.assertTrue(verify.allowed(line, want), line)

    def test_verifier_rule_rejects_near_misses(self):
        old = "    #[cfg(all(test, unix))]"
        for new in (f"    #[cfg(all({TS}, unix ))]", f"   #[cfg(all({TS}, unix))]",
                    f'    #[cfg(all(test, any(unix, feature = "test-support")))]',
                    f"    #[cfg(all({TS}, unix))] ", old.replace("test", "tests")):
            self.assertFalse(verify.allowed(old, new), new)
        self.assertFalse(verify.allowed("#[cfg(any(test, unix))]", f"#[cfg(any({TS}, unix))]"))
        self.assertFalse(verify.allowed("#[cfg(not(test))]", f"#[cfg(not({TS}))]"))
        self.assertFalse(verify.allowed("let test = 1;", f"let {TS} = 1;"))


class Codemod(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def test_three_rules_companion_exclusion_and_idempotence(self):
        fx = Fixture(self.tmp.name)
        rc, out = fx.codemod()
        self.assertEqual(rc, 0, out)
        # helper, unix_helper, two_line, Guard (+ its Drop), impl Guard (`Guard::set` on a gated type), kept
        # (excluded), impl Point, fixtures mod + seeded, the `use spy::Spy` re-export and the `mod spy` it reads.
        # decoy's private namesake stays out.
        self.assertIn("scanned=12 excluded=1 (listed=1 deferred=0) subs=11 files=1 companions=1 pinned=0", out)
        core = fx.core()
        for want in (f"#[cfg({TS})]\npub(crate) fn helper()",
                     f"    #[cfg(all({TS}, unix))] // trailing note stays\n",
                     f"#[cfg({TS})]\n#[cfg(unix)]\npub(crate) fn two_line()",
                     f"#[cfg({TS})]\nimpl Drop for Guard", f"#[cfg({TS})]\nimpl Point {{",
                     f"#[cfg({TS})]\nimpl Guard {{", "#[cfg(test)]\npub(crate) fn after_macro()",
                     f"#[cfg({TS})]\nmod spy {{", f"#[cfg({TS})]\npub(crate) use spy::Spy;",
                     f"#[cfg({TS})]\npub(crate) mod fixtures {{\n    #[cfg({TS})]\n    pub(crate) fn seeded()",
                     "#[cfg(test)]\npub(crate) fn kept()", "#[cfg(test)]\npub(crate) fn unused()"):
            self.assertIn(want, core)
        before = core
        rc, out = fx.codemod()
        self.assertEqual((rc, fx.core()), (0, before), out)
        self.assertIn("subs=0", out)
        self.assertEqual(fx.codemod("--check")[0], 0)

    def test_count_and_check_write_nothing(self):
        fx = Fixture(self.tmp.name)
        self.assertEqual(fx.codemod("--count")[0], 0)
        self.assertEqual(fx.codemod("--check")[0], 1)
        self.assertEqual(fx.core(), CORE)

    def test_an_excluded_parent_gate_defers_the_items_under_it(self):
        fx = Fixture(self.tmp.name, exclude=("fn kept", "mod fixtures"))
        rc, out = fx.codemod()
        self.assertIn("scanned=12 excluded=3 (listed=2 deferred=1) subs=9", out)
        self.assertIn("#[cfg(test)]\npub(crate) mod fixtures {\n    #[cfg(test)]\n    pub(crate) fn seeded()", fx.core())

    def test_unlisted_test_module_fails_closed_and_a_not_test_twin_is_held(self):
        app = APP.replace("seeded();", "seeded();\n        crate::core::tests::rig();\n        crate::core::registry();")
        fx = Fixture(self.tmp.name, core=CORE + TESTS_MOD + TWIN, app=app)
        rc, out = fx.codemod()
        self.assertEqual(rc, 3, out)
        self.assertIn("opens a test module, not excluded: src/core.rs :: mod tests", out)
        self.assertEqual(fx.core(), CORE + TESTS_MOD + TWIN)
        (fx.repo / verify.EXCLUDE).write_text(json.dumps(exclusions("fn kept", "mod tests")))
        rc, out = fx.codemod()
        self.assertEqual(rc, 0, out)
        self.assertIn("held=1", out)
        self.assertIn("fn registry — namesake with an opposite gate", out)
        self.assertIn("#[cfg(not(test))]\npub(crate) fn registry() {}\n\n#[cfg(test)]\npub(crate) fn registry", fx.core())
        # Listing both sides with their gates is the other way; a wrong count is stale.
        (fx.repo / verify.EXCLUDE).write_text(json.dumps(exclusions("fn kept", "mod tests",
                                                                    ("fn registry", ["not(test)", "test"]))))
        self.assertIn("held=0", fx.codemod()[1])
        (fx.repo / verify.EXCLUDE).write_text(json.dumps(exclusions("fn kept", "mod tests", "fn registry")))
        rc, out = fx.codemod()
        self.assertEqual(rc, 2, out)
        self.assertIn("exclusion matches 2 items with cfg ['not(test)', 'test'], expected ['test']", out)

    def test_every_target_of_one_reexport_opens_or_holds_together(self):
        # Sol krxts-1 P1: `use gate::{a, b, c}` kept one path per line and opened only the last target.
        core = CORE + REEXPORT
        app = APP.replace("seeded();", "seeded();\n        crate::core::first();")
        fx = Fixture(self.tmp.name, core=core, app=app)
        self.assertEqual(fx.codemod()[0], 0)
        for want in (f"    #[cfg({TS})]\n    pub(crate) fn first()", f"    #[cfg(all({TS}, unix))]\n    pub(crate) fn second()",
                     f"    #[cfg({TS})]\n    pub(crate) fn third()", f"#[cfg({TS})]\npub(crate) use gate::{{"):
            self.assertIn(want, fx.core())
        # One excluded target holds the re-export and the other two with it.
        # Sol krxts-2 P1: a direct `gate::first()` next to the re-export must not open `first` on its own.
        direct = app.replace("crate::core::first();", "crate::core::first();\n        crate::core::gate::first();")
        fx = Fixture(Path(self.tmp.name) / "held", core=core, app=direct,
                     exclude=("fn kept", ("mod gate > fn second", ["all(test, unix)"])))
        rc, out = fx.codemod()
        self.assertEqual(rc, 0, out)
        for kept in ("    #[cfg(test)]\n    pub(crate) fn first()", "    #[cfg(all(test, unix))]\n    pub(crate) fn second()",
                     "    #[cfg(test)]\n    pub(crate) fn third()"):
            self.assertIn(kept, fx.core())
        self.assertIn("#[cfg(test)]\npub(crate) use gate::{", fx.core())

    def test_a_target_in_a_macro_body_holds_the_reexport_even_though_its_module_exists(self):
        # krxts-3 P1-1 (readiness/source.rs:119 `clear::AFTER_PUBLICATION`): the module resolves, the static does not.
        app = APP.replace("seeded();", "seeded();\n        crate::core::AFTER.with(|after| after.get());")
        fx = Fixture(self.tmp.name, core=CORE + TLS_REEXPORT, app=app)
        rc, out = fx.codemod()
        self.assertEqual(rc, 0, out)
        self.assertIn("use declaration: in a macro body for `core::clear::AFTER`", out)
        self.assertTrue(fx.core().endswith(TLS_REEXPORT), fx.core()[-400:])

    def test_a_direct_reference_alone_still_holds_a_group_with_an_excluded_sibling(self):
        # krxts-3 P1-2: no upper test names the re-export; `second` is excluded and was never a hit.
        app = APP.replace("seeded();", "seeded();\n        crate::core::gate::first();")
        fx = Fixture(self.tmp.name, core=CORE + REEXPORT, app=app,
                     exclude=("fn kept", ("mod gate > fn second", ["all(test, unix)"])))
        rc, out = fx.codemod()
        self.assertEqual(rc, 0, out)
        self.assertTrue(fx.core().endswith(REEXPORT), fx.core()[-400:])

    def test_a_comment_semicolon_inside_the_use_does_not_end_it(self):
        # Sol krxts-2 P1: `first, // note;` ended the declaration at the comment, so two targets stayed shut.
        core = CORE + REEXPORT.replace("    first, second, third,", "    first, // note; see below\n    second, third,")
        fx = Fixture(self.tmp.name, core=core, app=APP.replace("seeded();", "seeded();\n        crate::core::first();"))
        self.assertEqual(fx.codemod()[0], 0)
        for want in (f"    #[cfg({TS})]\n    pub(crate) fn first()", f"    #[cfg(all({TS}, unix))]\n    pub(crate) fn second()",
                     f"    #[cfg({TS})]\n    pub(crate) fn third()"):
            self.assertIn(want, fx.core())

    def test_a_glob_or_macro_target_holds_the_whole_declaration(self):
        app = APP.replace("seeded();", "seeded();\n        crate::core::first();")
        fx = Fixture(self.tmp.name, core=CORE + REEXPORT.split("#[cfg(test)]\npub(crate) use")[0] + GLOB_USE, app=app)
        rc, out = fx.codemod()
        self.assertEqual(rc, 0, out)
        self.assertIn("use declaration: glob target `self::extra::*`", out)
        self.assertIn("    #[cfg(test)]\n    pub(crate) fn first()", fx.core())
        self.assertTrue(fx.core().endswith(GLOB_USE))
        # A path a macro builds is never followed either.
        (fx.repo / "src/core.rs").write_text(CORE + MACRO_USE)
        line = (CORE + MACRO_USE).split("\n").index("        pub(crate) use $crate::core::gate::$name;") + 1
        self.assertEqual(codemod.Tree(fx.repo).use_text("src/core.rs", line)[1], "path built by a macro (`$` / `!`)")

    def test_a_drifted_bundle_member_holds_the_whole_bundle(self):
        # Sol krxts-2 P2: RECORDS under `any(test, windows)` opened the reader and the record without the store.
        core = CORE + BUNDLE.replace("#[cfg(test)]\nstatic RECORDS", "#[cfg(any(test, windows))]\nstatic RECORDS")
        members = ("fn judged", ("static RECORDS", "test"), "fn record > stmt RECORDS.lock().unwrap().push(value);")
        fx = Fixture(self.tmp.name, core=core, app=APP.replace("seeded();", "seeded();\n        crate::core::judged();"),
                     bundles=[("records", members)])
        rc, out = fx.codemod()
        self.assertEqual(rc, 0, out)
        self.assertIn("static RECORDS has cfg `any(test, windows)`, recorded `test`", out)
        self.assertTrue(fx.core().endswith(BUNDLE.replace("#[cfg(test)]\nstatic RECORDS",
                                                          "#[cfg(any(test, windows))]\nstatic RECORDS")))

    def test_an_automatically_held_drop_holds_its_type(self):
        # Sol krxts-2 P2: the type opened while `impl Drop` under `all(test, test)` stayed shut.
        app = APP.replace("seeded();", "seeded();\n        let _r = crate::core::Restore;")
        fx = Fixture(self.tmp.name, core=CORE + HELD_DROP, app=app)
        rc, out = fx.codemod()
        self.assertEqual(rc, 0, out)
        self.assertIn("its impl Drop is held", out)
        self.assertTrue(fx.core().endswith(HELD_DROP))

    def test_an_opposite_gate_on_another_types_method_is_not_a_twin(self):
        # Sol krxts-2 P2: `B::probe` under `not(all(test, unix))` held the `A::probe` upper tests use.
        app = APP.replace("seeded();", "seeded();\n        crate::core::Alpha::probe();")
        fx = Fixture(self.tmp.name, core=CORE + TWO_TYPES, app=app)
        rc, out = fx.codemod()
        self.assertEqual(rc, 0, out)
        self.assertIn("held=0", out)
        self.assertIn(f"impl Alpha {{\n    #[cfg({TS})]\n    pub(crate) fn probe()", fx.core())
        self.assertIn("impl Beta {\n    #[cfg(not(all(test, unix)))]\n    pub(crate) fn probe()", fx.core())

    def test_a_bundle_opens_its_reader_store_and_recording_statement_together(self):
        core, app = CORE + BUNDLE, APP.replace("seeded();", "seeded();\n        crate::core::judged();")
        members = ("fn judged", "static RECORDS", "fn record > stmt RECORDS.lock().unwrap().push(value);")
        fx = Fixture(self.tmp.name, core=core, app=app, bundles=[("records", members)])
        rc, out = fx.codemod()
        self.assertEqual(rc, 0, out)
        for want in (f"#[cfg({TS})]\npub(crate) fn judged", f"#[cfg({TS})]\nstatic RECORDS",
                     f"    #[cfg({TS})]\n    RECORDS"):
            self.assertIn(want, fx.core())
        # Without the bundle only the reader opens: the store and the record would stay test-only.
        fx = Fixture(Path(self.tmp.name) / "loose", core=core, app=app)
        self.assertEqual(fx.codemod()[0], 0)
        self.assertIn("#[cfg(test)]\nstatic RECORDS", fx.core())
        fx = Fixture(Path(self.tmp.name) / "stale", core=core, app=app, bundles=[("records", ("fn gone",))])
        rc, out = fx.codemod()
        self.assertEqual((rc, "bundle records: src/core.rs :: fn gone matches 0 items, not 1" in out), (2, True), out)

    def test_an_excluded_type_holds_its_drop(self):
        fx = Fixture(self.tmp.name, exclude=("fn kept", "struct Guard"))
        rc, out = fx.codemod()
        self.assertEqual(rc, 0, out)
        self.assertIn("#[cfg(test)]\npub(crate) struct Guard;\n\n#[cfg(test)]\nimpl Drop for Guard", fx.core())

    def test_attribute_text_in_a_macro_or_raw_string_is_never_a_gate(self):
        fx = Fixture(self.tmp.name, core=CORE + FAKE)
        self.assertEqual(fx.codemod()[0], 0)
        core = fx.core()
        self.assertIn(f"#[cfg({TS})]\npub(crate) fn helper()", core)
        self.assertTrue(core.endswith(FAKE), core[-300:])

    def test_compound_predicates_and_opposite_gates_are_held(self):
        app = APP.replace("seeded();", "seeded();\n        crate::core::twice();\n        crate::core::plat();\n"
                                       "        crate::core::attr_twin();")
        fx = Fixture(self.tmp.name, core=CORE + COMPOUND, app=app)
        rc, out = fx.codemod()
        self.assertEqual(rc, 0, out)
        self.assertIn("held=3", out)
        for why in ("fn twice — no rewrite rule: #[cfg(all(test, test))]",
                    "fn plat — namesake with an opposite gate at line", "fn attr_twin — namesake with an opposite gate"):
            self.assertIn(why, out)
        self.assertTrue(fx.core().endswith(COMPOUND))

    def test_exclusion_rematch_survives_moves_but_not_a_namesake_of_another_gate(self):
        fx = Fixture(self.tmp.name)
        (fx.repo / "src/core.rs").write_text("// moved\n\n" + CORE)
        self.assertEqual(fx.codemod("--count")[0], 0)
        (fx.repo / "src/core.rs").write_text(CORE.replace("#[cfg(test)]\npub(crate) fn kept", "#[cfg(unix)]\npub(crate) fn kept"))
        rc, out = fx.codemod("--count")
        self.assertEqual(rc, 2, out)
        self.assertIn("exclusion matches 1 items with cfg ['unix'], expected ['test']: src/core.rs :: fn kept", out)

    def test_stale_exclusion_fails(self):
        fx = Fixture(self.tmp.name, exclude=("fn kept", "fn gone"))
        rc, out = fx.codemod()
        self.assertEqual(rc, 2, out)
        self.assertIn("exclusion matches nothing: src/core.rs :: fn gone", out)

    def test_labels_name_the_item_inside_its_containers(self):
        fx = Fixture(self.tmp.name)
        tree = codemod.Tree(fx.repo)
        labels = {v[0]: k for k, v in codemod.cfg_items(tree, "src/core.rs").items()}
        self.assertEqual(labels[CORE.split("\n").index("    pub(crate) fn seeded() {}")],
                         "mod fixtures > fn seeded")
        self.assertIn("impl Drop for Guard", labels.values())
        self.assertIn("fn after_macro", labels.values())  # `format!(` opens no block
        self.assertIn("impl Point", labels.values())


class Verify(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.fx = Fixture(self.tmp.name)
        self.base = self.fx.git("rev-parse", "HEAD")

    def converted(self, edit=None):
        self.assertEqual(self.fx.codemod()[0], 0)
        if edit:
            path = self.fx.repo / "src/core.rs"
            path.write_text(edit(path.read_text()))
        return self.fx.commit("convert")

    def test_codemod_output_is_only_a_structural_pass_without_step_e(self):
        # krxts-3 P2-1: a-d alone never print the full PASS nor exit 0.
        rc, out = self.fx.verify(self.base, self.converted())
        self.assertEqual(rc, 3, out)
        self.assertTrue(out.startswith("TS_VERIFY STRUCTURAL_PASS subs=11 scanned=12 excluded=1 idem=0"), out)
        self.assertNotIn("TS_VERIFY PASS", out)

    def test_one_extra_character_fails_a(self):
        rc, out = self.fx.verify(self.base, self.converted(lambda t: t.replace("fn helper() {}", "fn helper() { }")))
        self.assertEqual((rc, out.split("\n")[0]), (1, "TS_VERIFY FAIL a"), out)
        self.assertIn("not an allowed substitution: 'pub(crate) fn helper() {}'", out)

    def test_whitespace_change_on_a_gate_fails_a(self):
        rc, out = self.fx.verify(self.base, self.converted(lambda t: t.replace(f"    #[cfg(all({TS}, unix))]",
                                                                               f"\t#[cfg(all({TS}, unix))]")))
        self.assertEqual(rc, 1)
        self.assertIn("not an allowed substitution", out)

    def test_another_cfg_atom_fails_a(self):
        rc, out = self.fx.verify(self.base, self.converted(
            lambda t: t.replace("#[cfg(unix)]", '#[cfg(any(unix, feature = "test-support"))]')))
        self.assertEqual(rc, 1)
        self.assertIn("not an allowed substitution: '#[cfg(unix)]'", out)

    def test_changing_an_excluded_item_fails_b_and_c(self):
        rc, out = self.fx.verify(self.base, self.converted(
            lambda t: t.replace("#[cfg(test)]\npub(crate) fn kept", f"#[cfg({TS})]\npub(crate) fn kept")))
        # c and b catch it; d does not (an entry still matches its item once opened, for the manual commit).
        self.assertEqual((rc, out.split("\n")[0]), (1, "TS_VERIFY FAIL c,b"), out)
        self.assertIn("excluded item changed or missing: src/core.rs :: fn kept", out)
        gate = CORE.split("\n").index("pub(crate) fn kept() {}")  # 0-based item line = 1-based gate line
        self.assertIn(f"diff line the codemod does not make: src/core.rs:{gate}", out)

    def test_a_missed_gate_fails_b_and_d(self):
        rc, out = self.fx.verify(self.base, self.converted(
            lambda t: t.replace(f"#[cfg({TS})]\npub(crate) fn helper", "#[cfg(test)]\npub(crate) fn helper")))
        self.assertEqual((rc, out.split("\n")[0]), (1, "TS_VERIFY FAIL b,d"), out)
        self.assertIn("subs 10 != scanned 12 - excluded 1", out)
        self.assertIn("edit left at head: src/core.rs:3", out)

    def test_an_exclusion_list_edit_fails(self):
        self.fx.codemod()
        (self.fx.repo / verify.EXCLUDE).write_text(json.dumps(exclusions("fn kept", "fn unused")))
        rc, out = self.fx.verify(self.base, self.fx.commit("convert"))
        self.assertEqual(rc, 1)
        self.assertIn("only modified .rs files may change", out)
        self.assertIn("differs between the commits", out)


FAKE_CARGO = """#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
core = Path("src/core.rs")
json.dump({"argv": sys.argv[1:], "cwd": os.getcwd(), "target": os.environ.get("CARGO_TARGET_DIR"),
           "color": os.environ.get("CARGO_TERM_COLOR"),
           "core": core.read_text() if core.is_file() else None}, open(os.environ["FAKE_LOG"], "w"))
sys.stderr.write(os.environ.get("FAKE_STDERR", ""))
sys.exit(int(os.environ.get("FAKE_RC", "0")))
"""


class Compile(unittest.TestCase):
    """Step e with a fake cargo: the command, where it runs, and how its rc and diagnostics are read."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.fx = Fixture(self.tmp.name)
        self.base = self.fx.git("rev-parse", "HEAD")
        self.assertEqual(self.fx.codemod()[0], 0)
        self.head = self.fx.commit("convert")
        cargo = Path(self.tmp.name) / "fake-cargo"
        cargo.write_text(FAKE_CARGO)
        cargo.chmod(0o755)
        self.log = Path(self.tmp.name) / "cargo.json"
        self.env = {"TS_VERIFY_CARGO": str(cargo), "FAKE_LOG": str(self.log),
                    "TS_VERIFY_TARGET_DIR": str(Path(self.tmp.name) / "tgt")}

    def run_e(self, **env):
        with mock.patch.dict(os.environ, {**self.env, **env}):
            return self.fx.verify(self.base, self.head, "fx")

    def test_it_builds_the_package_lib_with_test_support_on_a_checkout_of_head(self):
        rc, out = self.run_e()
        self.assertEqual(rc, 0, out)
        self.assertTrue(out.startswith("TS_VERIFY PASS subs=11 scanned=12 excluded=1 idem=0 compile=0"), out)
        self.assertIn("| e compile | PASS | rc=0 errors=0 cmd=`check -p fx --lib --features test-support "
                      "--message-format short --color never`", out)
        call = json.loads(self.log.read_text())
        # krxts-3 P2-2: the real Cargo feature (optional dependencies on), no colour codes in the diagnostics.
        self.assertEqual(call["argv"], ["check", "-p", "fx", "--lib", "--features", "test-support",
                                        "--message-format", "short", "--color", "never"])
        self.assertEqual(call["color"], "never")
        self.assertEqual(call["target"], self.env["TS_VERIFY_TARGET_DIR"])
        # The head commit's tree, not the working tree: a later uncommitted edit is not what gets built.
        self.assertNotEqual(Path(call["cwd"]).resolve(), self.fx.repo.resolve())
        self.assertIn(f"#[cfg({TS})]\npub(crate) fn helper()", call["core"])

    def test_a_failed_build_or_an_error_line_fails_e(self):
        stderr = ("src/core.rs:9:5: error[E0432]: unresolved import `gate::second`\n"
                  "error: could not compile `fx` (lib) due to 1 previous error\n")
        for rc_text, want in (("101", "rc=101 errors=1"), ("0", "rc=0 errors=1")):
            rc, out = self.run_e(FAKE_RC=rc_text, FAKE_STDERR=stderr)
            self.assertEqual((rc, out.split("\n")[0]), (1, "TS_VERIFY FAIL e"), out)
            self.assertIn(f"| e compile | FAIL | {want} ", out)
            self.assertIn("e  src/core.rs:9:5: error[E0432]: unresolved import `gate::second`", out)
        rc, out = self.run_e(FAKE_RC="1", FAKE_STDERR="warning: only a warning\n")
        self.assertIn("| e compile | FAIL | rc=1 errors=0 ", out)

    def test_a_cargo_that_does_not_start_fails_e_and_no_flag_runs_no_cargo(self):
        rc, out = self.run_e(TS_VERIFY_CARGO=str(Path(self.tmp.name) / "missing-cargo"))
        self.assertEqual((rc, out.split("\n")[0]), (1, "TS_VERIFY FAIL e"), out)
        self.assertIn("cargo did not start", out)
        with mock.patch.dict(os.environ, self.env):
            rc, out = self.fx.verify(self.base, self.head)
        self.assertEqual(rc, 3, out)
        self.assertNotIn("e compile", out)
        self.assertFalse(self.log.exists())


class Equivalence(unittest.TestCase):
    def classify(self, before, after, cfg_equiv=True):
        modules = {("-", "src/a.rs"): ("fx", "a"), ("+", "src/a.rs"): ("fx", "a")}
        return equiv_class.classify({"src/a.rs": before}, {"src/a.rs": after}, modules, {"fx"},
                                    {"src/a.rs"} if cfg_equiv else frozenset())

    def test_test_support_atom_is_the_cfg_class(self):
        before = "#[cfg(test)]\nfn a() {}\nimpl X {\n    #[cfg(all(test, unix))]\n    fn b() {}\n}\n#[cfg(test)]\nuse x::Y;\n"
        after = before.replace("cfg(test)", f"cfg({TS})").replace("all(test,", f"all({TS},")
        v = self.classify(before, after)
        self.assertEqual((v.klass, v.cfg, v.vis, v.reasons), ("cfg", 3, 0, []))
        swapped = before.replace("cfg(test)", 'cfg(any(feature = "test-support", test))')
        self.assertEqual(self.classify(before, swapped).klass, "cfg")
        # With visibility too, the class is vis; in a package that declares the feature it is C.
        self.assertEqual(self.classify(before, after.replace("fn a", "pub(crate) fn a")).klass, "vis")
        self.assertEqual(self.classify(before, after, cfg_equiv=False).klass, "C")

    def test_two_blocks_with_one_key_pair_by_their_tokens(self):
        before = "impl X {\n    #[cfg(test)]\n    fn a() {}\n}\nimpl X {\n    #[cfg(test)]\n    fn b() {}\n}\n"
        v = self.classify(before, before.replace("cfg(test)", f"cfg({TS})"))
        self.assertEqual((v.klass, v.cfg, v.reasons), ("cfg", 2, []))
        self.assertEqual(self.classify(before, before.replace("fn b() {}", "fn b() { 1; }")).klass, "C")

    def test_other_cfg_changes_stay_c(self):
        before = "#[cfg(test)]\nfn a() {}\n"
        for after in ('#[cfg(any(test, feature = "other"))]\nfn a() {}\n', "#[cfg(any(test, unix))]\nfn a() {}\n",
                      '#[cfg(not(any(test, feature = "test-support")))]\nfn a() {}\n',
                      f'fn a() {{ let _ = "{TS}"; }}\n'):
            self.assertEqual(self.classify(before, after).klass, "C", after)
        quoted = "fn a() -> &'static str {\n    stringify! {\n        #[cfg(test)]\n        fn h() {}\n    }\n}\n"
        self.assertEqual(self.classify(quoted, quoted.replace("cfg(test)", f"cfg({TS})")).klass, "C")
        body = 'fn a() { m!(any(test, feature = "test-support")); }\n'
        self.assertEqual(self.classify(body.replace(f"{TS}", "test"), body).klass, "C")

    def test_declaration_key_reads_the_atom_as_test(self):
        tokens = lambda text: tuple(t.value for t in rts.rust_tokens(text)[3:-1])
        self.assertEqual(rts.canonical_cfg(tokens(f"#[cfg({TS})]")), tokens("#[cfg(test)]"))
        self.assertEqual(rts.canonical_cfg(tokens(f"#[cfg(all({TS}, unix))]")), tokens("#[cfg(all(test, unix))]"))
        self.assertEqual(rts.canonical_cfg(tokens('#[cfg(any(test, feature = "other"))]')),
                         tokens('#[cfg(any(test, feature = "other"))]'))


class SourceLosses(unittest.TestCase):
    def test_a_test_moving_binaries_matches_across_the_spellings(self):
        # The same test file leaves the root lib for crate k, whose `mod` gate reads the test-support spelling.
        with tempfile.TemporaryDirectory() as tmp:
            roots = {}
            manifest = '[package]\nname = "{}"\nversion = "0.1.0"\n'
            for side, gate in (("base", "test"), ("head", TS), ("plain", "all(test, unix)")):
                root = Path(tmp) / side
                files = {"crates/k/src/t.rs": "#[test]\nfn one() {}\n",
                         "crates/k/Cargo.toml": manifest.format("k")}
                if side == "base":
                    files.update({"Cargo.toml": manifest.format("fx"), "crates/k/src/lib.rs": "\n",
                                  "src/lib.rs": '#[cfg(test)]\n#[path = "../crates/k/src/t.rs"]\nmod t;\n'})
                else:
                    files.update({"Cargo.toml": manifest.format("fx") + '[workspace]\nmembers = ["crates/k"]\n',
                                  "src/lib.rs": "\n", "crates/k/src/lib.rs": f"#[cfg({gate})]\nmod t;\n"})
                for rel, text in files.items():
                    (root / rel).parent.mkdir(parents=True, exist_ok=True)
                    (root / rel).write_text(text)
                roots[side] = root
            base, _, base_libs = rts.platform_tests(roots["base"])
            for side, moved in (("head", 1), ("plain", 0)):
                head, _, head_libs = rts.platform_tests(roots[side])
                losses = rts._source_losses(roots["base"], roots[side], base, head, base_libs, head_libs)
                self.assertEqual(len(losses["relocated"]["linux"]), moved, (side, losses))
                self.assertEqual(losses["removed"]["linux"], [] if moved else ["t::one"], side)


if __name__ == "__main__":
    unittest.main()
