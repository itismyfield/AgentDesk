"""Exercise PR scope against committed Rust module trees without Cargo."""

from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts/ci"))
from nextest_scope import select_scope  # noqa: E402
from rust_module_scope import GitQueryError, git  # noqa: E402

CHANGED = ("agentdesk", "outer::changed::tests::unit")
CALLER = ("agentdesk", "caller::tests::unit")
NEW = ("agentdesk", "outer::changed::check_examples::fresh")
CORE = ("adk-store", "contract")
INVENTORY = {CHANGED, CALLER, NEW, CORE, ("agentdesk", "caller::unrelated::tests::unit"),
             ("agentdesk", "other::tests::unit")}


class ScopeTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        git(self.root, "init", "-q")
        self.base = self.commit("init", {
            "Cargo.toml": '[package]\nname = "agentdesk"\n[workspace]\nmembers = ["crates/store"]',
            "crates/store/Cargo.toml": '[package]\nname = "adk-store"\n[lib]\npath = "../../src/store.rs"',
            "src/lib.rs": "mod outer; mod caller; mod other;",
            "src/store.rs": "pub fn data() {}",
            "src/outer/mod.rs": "pub mod changed;",
            "src/outer/changed.rs": "pub fn works() {} mod tests { #[test] fn unit() {} }",
            "src/caller.rs": "use crate::outer::{changed}; mod tests { #[test] fn unit() {} }",
            "src/other.rs": "mod tests { #[test] fn unit() {} }",
        })

    def commit(self, name, files):
        for relative, data in files.items():
            path = self.root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(data, encoding="utf-8")
        git(self.root, "add", ".")
        git(self.root, "-c", "user.email=ci@example.invalid", "-c", "user.name=CI", "commit", "-qm", name)
        return git(self.root, "rev-parse", "HEAD").strip()

    def decision(self, files, ref="work/my-pr", base=None):
        head = self.commit("change", files)
        return select_scope(self.root, base or self.base, head, ref, INVENTORY, core_ids={CORE})

    def assert_fallback(self, decision, reason):
        self.assertEqual(decision.mode, "full")
        self.assertEqual(decision.ids, INVENTORY)
        self.assertEqual(decision.reason.split(":", 1)[0], reason)

    def test_module_change_and_grouped_direct_caller(self):
        decision = self.decision({"src/outer/changed.rs":
                                  'mod tests { #[test] fn unit() { assert_eq!(1, 1); } } '
                                  '#[path="odd_name.rs"] mod check_examples;',
                                  "src/outer/odd_name.rs": "#[test] fn fresh() {}"})
        self.assertEqual(decision.mode, "scope")
        self.assertEqual(decision.ids, {CHANGED, CALLER, NEW, CORE})

    def test_shared_input(self):
        self.assert_fallback(self.decision({"Cargo.toml": "[workspace]"}), "shared-input")
        for path in ("justfile", "package.json", "package-lock.json", ".github/workflows/ci-pr.yml"):
            with self.subTest(path=path):
                previous = git(self.root, "rev-parse", "HEAD").strip()
                files = {path: "changed", "src/caller.rs": f'const BUILD: &str = "{path}";'}
                self.assert_fallback(self.decision(files, base=previous), "shared-input")

    def test_workspace_library_outside_its_crate_directory(self):
        self.assert_fallback(self.decision({"src/store.rs": "pub fn data() -> u32 { 1 }"}), "workspace-dependency")

    def test_module_not_reached(self):
        self.assert_fallback(self.decision({"src/outer/unwired.rs": "pub fn orphan() {}"}), "unmapped-module")

    def test_landing_batch(self):
        self.assert_fallback(self.decision({"src/caller.rs": "fn change() {}"}, "land/batch-works"), "landing-batch")

    def test_git_failure_vs_no_grep_match(self):
        self.assertEqual(git(self.root, "grep", "-l", "-F", "does_not_appear", self.base, "--", "src"), "")
        self.assert_fallback(self.decision({"src/caller.rs": "fn change() {}"}, base="missing-tree"), "git-error")
        with self.assertRaises(GitQueryError):
            git(self.root, "grep", "-l", "-F", "no_files", "missing-tree", "--", "src")

    def test_uncommitted_source_cannot_influence_mapping(self):
        head = self.commit("change", {"src/caller.rs": "fn change() {}"})
        (self.root / "src" / "extra.rs").write_text("fn untracked() {}", encoding="utf-8")
        self.assert_fallback(select_scope(self.root, self.base, head, "work/my-pr", INVENTORY), "git-error")

    def test_macro_and_feature_boundaries(self):
        for source in ("macro_rules! generator { () => {} }", '#[cfg(feature = "optional")] fn change() {}',
                       "paste::paste! { fn foo() {} }", "mod nested { external::generate_tests!(); }"):
            with self.subTest(source=source):
                self.assert_fallback(self.decision({"src/caller.rs": source}), "macro-feature")
