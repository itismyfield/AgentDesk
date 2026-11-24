"""test_support_codemod.py, test_support_verify.py and the `test-support` cfg equivalence (#7353 KRX-TS)."""

import io
import json
import subprocess
import sys
import tempfile
import textwrap
import unittest
from contextlib import redirect_stdout
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


def exclusions(*items):
    return {"entries": [{"file": "src/core.rs", "item": i, "reason": "fixture"} for i in items]}


class Fixture:
    """A git repository with one KRX module (core) and one DL module (app) whose test uses core's gates."""

    def __init__(self, root, core=CORE, app=APP, exclude=("fn kept",)):
        self.repo = Path(root) / "repo"
        files = {"Cargo.toml": '[package]\nname = "fx"\nversion = "0.1.0"\nedition = "2021"\n',
                 "src/lib.rs": "mod app;\nmod core;\nmod decoy;\n", "src/core.rs": core, "src/app.rs": app,
                 "src/decoy.rs": DECOY,
                 "scripts/crate_split/partition.json": json.dumps(PARTITION),
                 verify.EXCLUDE: json.dumps(exclusions(*exclude))}
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
            rc = codemod.main(["--tree", str(self.repo), "--exclude", str(self.repo / verify.EXCLUDE), *args])
        return rc, out.getvalue()

    def verify(self, base, head):
        out = io.StringIO()
        with redirect_stdout(out):
            rc = verify.verify(self.repo, base, head)
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

    def test_unlisted_test_module_or_not_test_twin_fails_closed(self):
        app = APP.replace("seeded();", "seeded();\n        crate::core::tests::rig();\n        crate::core::registry();")
        fx = Fixture(self.tmp.name, core=CORE + TESTS_MOD + TWIN, app=app)
        rc, out = fx.codemod()
        self.assertEqual(rc, 3, out)
        self.assertIn("opens a test module, not excluded: src/core.rs :: mod tests", out)
        self.assertIn("has a cfg(not(test)) twin, not excluded: src/core.rs :: fn registry", out)
        self.assertEqual(fx.core(), CORE + TESTS_MOD + TWIN)
        (fx.repo / verify.EXCLUDE).write_text(json.dumps(exclusions("fn kept", "mod tests", "fn registry")))
        rc, out = fx.codemod()
        self.assertEqual(rc, 0, out)
        self.assertIn("listed=3", out)
        self.assertIn("#[cfg(not(test))]\npub(crate) fn registry", fx.core())

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

    def test_codemod_output_passes(self):
        rc, out = self.fx.verify(self.base, self.converted())
        self.assertEqual(rc, 0, out)
        self.assertTrue(out.startswith("TS_VERIFY PASS subs=11 scanned=12 excluded=1 idem=0"), out)

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
