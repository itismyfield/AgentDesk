"""Unit tests for the H2 admission gate (fixture trees and a throwaway git repo, no cargo)."""

from __future__ import annotations

import io
import json
import os
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT / "scripts/ci"))
import h2_admission as adm  # noqa: E402
import h2_depinfo  # noqa: E402
import h2_measure as h2  # noqa: E402
from tests.test_h2_measure import diag, locate  # noqa: E402  (shared clippy-JSON fixture helpers)

TMUX = "agentdesk::services::platform::tmux::has_session"
CMD, TOKIO = "std::process::Command::new", "tokio::process::Command::new"
TYPE = "agentdesk::services::relay::TmuxBackend"
W_PATHS = ("agentdesk::services::probe::alive", "agentdesk::services::probe::user", "agentdesk::services::relay::build")
PROBE, RELAY, OWNER = "src/services/probe.rs", "src/services/relay_impl.rs", "src/services/platform/tmux.rs"
SOURCES = {
    "src/lib.rs": "pub mod services;\n",
    "src/services/mod.rs": 'pub mod platform;\npub mod probe;\n#[path = "relay_impl.rs"]\nmod relay;\n',
    "src/services/platform/mod.rs": "pub mod tmux;\n",
    OWNER: textwrap.dedent("""\
        pub fn has_session(_: &str) -> bool { true }
        pub(crate) fn read_process_args() {}
        #[cfg(test)]
        mod tests {
        pub fn not_an_owner_api() {}
        }
        """),
    PROBE: textwrap.dedent("""\
        use std::process::Command;
        pub fn alive(name: &str) -> bool { crate::services::platform::tmux::has_session(name) }
        pub fn user() -> bool { alive("x") }
        const _: () = { Command::new("git"); };
        const _: () = { Command::new("gh"); };
        """),
    RELAY: textwrap.dedent("""\
        pub trait Backend { fn send(&self); }
        pub struct TmuxBackend;
        impl Backend for TmuxBackend {
            fn send(&self) { let _ = crate::services::platform::tmux::has_session("s"); }
        }
        pub fn build() -> Box<dyn Backend> { Box::new(TmuxBackend) }
        """),
}
CLIPPY_TOML = "\n".join([
    "disallowed-methods = [",
    f'  {{ path = "{TMUX}", reason = "H2 EXEC both" }},',
    *(f'  {{ path = "{p}", reason = "H2 W both" }},' for p in W_PATHS),
    f'  {{ path = "{CMD}", reason = "H2 SUBPROC both" }},',
    f'  {{ path = "{TOKIO}", reason = "H2 SUBPROC both" }},',
    "]", "disallowed-types = [", f'  {{ path = "{TYPE}", reason = "H2 TYPES both" }},', "]", ""])
# (file, needle, callee, lint) for every diagnostic the fixture sources produce
NEEDLES = [(OWNER, "pub fn has_session", TMUX, None), (PROBE, "crate::services::platform::tmux::has_session(name)", TMUX, None),
           (PROBE, 'alive("x")', "agentdesk::services::probe::alive", None), (PROBE, 'Command::new("git")', CMD, None),
           (PROBE, 'Command::new("gh")', CMD, None), (PROBE, 'has_session("y")', TMUX, None),
           (PROBE, 'has_session("z")', TMUX, None), (RELAY, 'has_session("s")', TMUX, None),
           (RELAY, "TmuxBackend)", TYPE, "clippy::disallowed_types")]

def diag_lines(sources: dict[str, str]) -> list[str]:
    return [diag(file, *locate(sources[file], needle), callee, lint=lint or "clippy::disallowed_methods")
            for file, needle, callee, lint in NEEDLES if needle in sources[file]]

def artifact(root: Path, digest: str, *, name: str = "agentdesk", src: str = "src/lib.rs", test: bool = False,
             features: tuple[str, ...] = ()) -> str:
    """A cargo `compiler-artifact` line for a lib whose dep-info is `target/debug/deps/<name>-<digest>.d`."""
    return json.dumps({"reason": "compiler-artifact", "target": {"kind": ["lib"], "name": name, "src_path": str(root / src)},
                       "profile": {"test": test}, "features": list(features),
                       "filenames": [str(root / f"target/debug/deps/lib{name}-{digest}.rmeta")]})

# `rustc --print cfg` of a Linux host; fixtures stand it in for the toolchain call.
HOST_CFG = 'debug_assertions\npanic="unwind"\ntarget_family="unix"\ntarget_os="linux"\nunix\n'

def host_cfg(case: unittest.TestCase) -> None:
    patcher = mock.patch.object(h2_depinfo, "rustc_cfg", return_value=HOST_CFG, create=True)
    patcher.start()
    case.addCleanup(patcher.stop)

def write_depinfo(root: Path, digest: str, deps) -> Path:
    """A rustc-shaped dep-info: `.d` and `.rmeta` rules, per-file empty rules, env-dep comments."""
    path = root / f"target/debug/deps/agentdesk-{digest}.d"
    path.parent.mkdir(parents=True, exist_ok=True)
    escaped = [str(d).replace(" ", "\\ ") for d in deps]
    rmeta = path.with_name(f"libagentdesk-{digest}.rmeta")
    path.write_text(f"{path}: {' '.join(escaped)}\n\n{rmeta}: {' '.join(escaped)}\n\n" + "".join(f"{d}:\n" for d in escaped)
                    + f"\n# env-dep:CARGO_MANIFEST_DIR={root}\n# env-dep:CLIPPY_CONF_DIR\n", encoding="utf-8")
    return path

def dup_mod(file: str, line: int) -> str:
    return json.dumps({"reason": "compiler-message", "target": {"kind": ["lib"]}, "message": {
        "code": {"code": "clippy::duplicate_mod"}, "message": "file is loaded as a module multiple times: `src/a.rs`",
        "spans": [{"file_name": file, "line_start": line, "column_start": 1, "is_primary": True}]}})

PATCHES = dict(OWNER_ROSTER=frozenset({OWNER}), R_C_GRANDFATHERED={}, NONEXEC=frozenset(), W_TYPES=frozenset({TYPE}),
               PS=frozenset({"agentdesk::services::platform::tmux::read_process_args"}),
               KNOWN_UNREFERENCED={lane: frozenset({TOKIO}) for lane in h2.LANES})

class Tree(unittest.TestCase):
    """A fixture crate committed as `base`, with a measured baseline in both lanes."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        for name, value in PATCHES.items():
            patcher = mock.patch.object(adm, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        host_cfg(self)
        self.sources, self.extra = dict(SOURCES), []
        self.write(self.sources)
        (self.root / "scripts/ci").mkdir(parents=True)
        (self.root / "clippy.toml").write_text(CLIPPY_TOML, encoding="utf-8")
        self.regen_baseline()
        write_depinfo(self.root, "00aa", [*SOURCES, "Cargo.toml"])
        self.git("init", "-q", "-b", "main")
        self.commit("base")
        self.base = self.git("rev-parse", "HEAD").strip()

    def git(self, *args: str) -> str:
        return subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@t", *args], cwd=self.root,
                              check=True, capture_output=True, text=True).stdout

    def commit(self, message: str) -> None:
        self.git("add", "-A")
        self.git("commit", "-q", "--allow-empty", "-m", message)

    def write(self, sources: dict[str, str]) -> None:
        h2._MODULE_TABLES.clear()
        for rel, text in sources.items():
            (self.root / rel).parent.mkdir(parents=True, exist_ok=True)
            (self.root / rel).write_text(text, encoding="utf-8")

    def measure(self) -> dict:
        h2._MODULE_TABLES.clear()
        return h2.measure(self.root, diag_lines(self.sources), h2.load_config(self.root / "clippy.toml"))

    def regen_baseline(self) -> None:
        h2.write_baseline(self.root, {s: {k: dict.fromkeys(h2.LANES, v) for k, v in r.items()}
                                      for s, r in self.measure()["rows"].items()})

    def edit(self, rel: str, old: str, new: str) -> None:
        self.sources[rel] = self.sources[rel].replace(old, new, 1)
        self.write({rel: self.sources[rel]})

    def admit(self, *rows: str) -> None:
        (self.root / adm.ADMISSIONS_FILE).write_text("".join(textwrap.dedent(r) for r in rows), encoding="utf-8")

    def lines(self) -> list[str]:
        return [*diag_lines(self.sources), artifact(self.root, "00aa"), *self.extra]

    def evaluate(self, lane: str = "linux") -> list[str]:
        return adm.evaluate(self.root, lane, self.base, self.lines())

    def run_main(self, *args: str) -> tuple[int, str, str]:
        (json_path := self.root.parent / f"{self.root.name}.json").write_text("\n".join(self.lines()))
        self.addCleanup(json_path.unlink, missing_ok=True)
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = adm.main(["--repo", str(self.root), "--json", str(json_path), *args])
        return code, out.getvalue(), err.getvalue()

GROW = """\
    [[admission]]
    file = "src/services/probe.rs"
    item = "user"
    callee = "agentdesk::services::platform::tmux::has_session"
    old = 0
    new = 1
    lane = "both"
    issue = 5340
    base_sha = "audit-only"
    """

class MeasurerSites(Tree):
    def test_h8_lines_only_for_folded_items(self) -> None:
        result = self.measure()
        self.assertEqual(result["rows"]["subproc"][(PROBE, "const _", CMD)], 2)
        # both anonymous consts fold into one key; their span lines stay as aux metadata
        self.assertEqual(result["sites"]["subproc"], {(PROBE, "const _", CMD): [4, 5]})
        self.assertEqual(result["sites"]["exec"], {})
        (self.root / "d.json").write_text("\n".join(diag_lines(self.sources)))
        with redirect_stdout(out := io.StringIO()):
            h2.main(["--repo", str(self.root), "--lane", "linux", "--json", str(self.root / "d.json")])
        rows = json.loads(out.getvalue())["rows"]
        self.assertEqual([r.get("lines") for r in rows["subproc"]], [[4, 5]])
        self.assertNotIn("lines", rows["exec"][0])

class EndToEnd(Tree):
    def test_clean_tree_passes_both_lanes(self) -> None:
        self.assertEqual(self.evaluate("linux"), [])
        self.assertEqual(self.evaluate("macos"), [])
        self.assertEqual(self.run_main("--lane", "macos", "--base", self.base)[0], 0)

    def test_growth_needs_exactly_one_suffix_admission(self) -> None:
        self.edit(PROBE, 'alive("x")', 'alive("x") && crate::services::platform::tmux::has_session("y")')
        self.regen_baseline()
        problems = self.evaluate()
        self.assertEqual(len(problems), 2, problems)  # one per lane
        self.assertIn("grew 0 -> 1 without an admission", problems[0])
        self.admit(GROW)
        self.assertEqual(self.evaluate(), [])
        code, _, err = self.run_main("--lane", "linux", "--base", self.base)
        self.assertEqual((code, err), (0, ""))
        self.commit("admit")  # replaying the same admission against the new base is unused
        self.base = self.git("rev-parse", "HEAD").strip()
        self.admit(GROW, GROW)
        self.assertTrue(any("is admitted but did not grow" in p for p in self.evaluate()))
        self.admit("")  # removing a landed admission is an edit of the base prefix
        self.assertIn("were edited or removed", self.evaluate()[0])

    def test_old_new_and_lane_must_match(self) -> None:
        self.edit(PROBE, 'alive("x")', 'alive("x") && crate::services::platform::tmux::has_session("y")')
        self.regen_baseline()
        self.admit(GROW.replace("old = 0", "old = 1").replace("new = 1", "new = 2"))
        self.assertTrue(any("says 1->2, base/head are 0->1" in p for p in self.evaluate()))
        self.admit(GROW.replace('"both"', '"linux"'))
        self.assertEqual([p[:40] for p in self.evaluate()], ["admission: macos src/services/probe.rs :"])
        self.admit(GROW.replace('"both"', '"linux"'), GROW.replace('"both"', '"macos"'))
        self.assertEqual(self.evaluate(), [])
        self.admit(GROW, GROW.replace('"both"', '"linux"'))
        self.assertIn("claimed 2 times", self.evaluate()[0])

    def test_rw_and_fixpoint_equality(self) -> None:
        self.edit(PROBE, "pub fn user", 'pub fn fresh() -> bool { crate::services::platform::tmux::has_session("z") }\npub fn user')
        self.regen_baseline()
        self.admit(GROW.replace('"user"', '"fresh"'))
        problems = self.evaluate()
        self.assertTrue(any(p.startswith("R-W: src/services/probe.rs :: fresh") for p in problems), problems)
        self.assertIn("R-E: agentdesk::services::probe::fresh must be registered as W (linux); "
                      "run h2_measure.py --regen", problems)
        toml = (self.root / "clippy.toml").read_text()
        (self.root / "clippy.toml").write_text(toml.replace(
            "  { path = \"std", '  { path = "agentdesk::services::probe::fresh", reason = "H2 W both" },\n  { path = "std'))
        self.assertEqual(self.evaluate(), [])
        # a registered W entry nothing derives any more is stale
        (self.root / "clippy.toml").write_text(toml.replace(W_PATHS[1], "agentdesk::services::probe::gone"))
        self.assertIn("R-E: stale W (linux) entry agentdesk::services::probe::gone; run h2_measure.py --regen",
                      self.evaluate())

    def test_rw_prefix_trait_impl_and_types(self) -> None:
        config = h2.load_config(self.root / "clippy.toml")
        self.assertIsNone(adm.rw_problem(self.root, config, (PROBE, "user::inner", TMUX)))  # nested fn
        self.assertIsNone(adm.rw_problem(self.root, config, (RELAY, "<TmuxBackend as Backend>::send", TMUX)))
        self.assertIsNone(adm.rw_problem(self.root, config, (PROBE, "user", CMD)))  # SUBPROC is not R-W
        self.assertIsNotNone(adm.rw_problem(self.root, config, (RELAY, "<Other as Backend>::send", TMUX)))
        self.assertIsNotNone(adm.rw_problem(self.root, config, (RELAY, "make", TYPE)))  # TYPES mention
        self.assertIsNotNone(adm.rw_problem(self.root, config, (PROBE, "const X", TMUX)))

    def test_h8_admission_names_folded_lines(self) -> None:
        self.edit(PROBE, 'Command::new("gh"); };', 'Command::new("gh"); };\nconst _: () = { Command::new("git").arg("tmux"); };')
        NEEDLES.append((PROBE, 'Command::new("git").arg', CMD, None))
        self.addCleanup(NEEDLES.pop)
        self.regen_baseline()
        row = GROW.replace('"user"', '"const _"').replace(TMUX, CMD).replace("old = 0", "old = 2").replace("new = 1", "new = 3")
        self.admit(row)
        problems = self.evaluate()
        self.assertIn("folds several items (H8); set lines = [4, 5, 6]", "".join(problems))
        self.assertTrue(any(p.startswith("R-C: src/services/probe.rs pairs `Command` with a new tmux literal") for p in problems))
        self.edit(PROBE, '.arg("tmux")', '.arg("status")')
        self.admit(row.replace("issue", "lines = [4, 5, 6]\nissue"))
        self.assertEqual(self.evaluate(), [])
        self.admit(GROW.replace("issue", "lines = [3]\nissue"))
        self.assertTrue(any("is unambiguous; drop lines" in p for p in self.evaluate()))

    def test_inventory_and_dead_entries(self) -> None:
        # every pub fn shape is inventoried; owner API the item walk cannot enumerate is refused (r2)
        for added, needle in (("pub fn kill_server() {}", "tmux::kill_server (unclassified)"),
                              ("pub struct RawTmux;\nimpl RawTmux {\n    pub fn run(&self) {}\n    fn private(&self) {}\n}",
                               "tmux::RawTmux::run (unclassified)"),
                              ("mod inner {\n    pub(super) fn deep() {}\n}", "tmux::inner::deep (unclassified)"),
                              ('pub extern "C" fn ext() {}', "tmux::ext (unclassified)"),
                              ("macro_rules! make { () => { pub fn stealth() {} } }", "`macro_rules!`"),
                              ("fn helper() { macro_rules! inner { () => {} } }", "`macro_rules!`"),
                              ("make! { stealth }", "`make!`"),
                              ("cfg_if::cfg_if! { if #[cfg(unix)] { fn hidden() {} } }", "`cfg_if!`"),
                              ("pub trait Probe {\n    fn required(&self);\n    fn stealth(&self) { }\n}", "default method")):
            with self.subTest(shape=needle):
                self.edit(OWNER, "pub(crate) fn read", added + "\npub(crate) fn read")
                problems = self.evaluate()
                self.edit(OWNER, added + "\n", "")
                self.assertTrue(len(problems) <= 2 and any(needle in p and p.startswith("R-E: ") for p in problems), problems)
        self.assertEqual(self.evaluate(), [])
        toml = (self.root / "clippy.toml").read_text()
        malformed = toml.replace('reason = "H2 SUBPROC both"', 'reason = "H2 SUBPROC"', 1)
        (self.root / "clippy.toml").write_text(malformed)
        with self.assertRaisesRegex(h2.MeasureError, "bad H2 entry"):  # same parser as load_config
            adm.untagged_entries(self.root / "clippy.toml")
        (self.root / "clippy.toml").write_text(
            toml.replace(TOKIO, "tokio::process::Command::spawn").replace(TYPE, TYPE + "X")
            .replace("]\ndisallowed-types", '  { path = "x::y", reason = "other" },\n]\ndisallowed-types'))
        problems = "\n".join(self.evaluate())
        for text in ("SUBPROC must be exactly", "TYPES must be exactly", "without an `H2 <SET> <lane>` reason",
                     "tokio::process::Command::spawn is registered but has no linux diagnostic",
                     "tokio::process::Command::new is pinned in KNOWN_UNREFERENCED[linux]"):
            self.assertIn(text, problems)

    def test_base_prerequisites_and_inert(self) -> None:
        empty_tree = self.git("hash-object", "-t", "tree", "-w", "/dev/null").strip()
        empty = self.git("commit-tree", "-m", "empty", empty_tree).strip()
        self.assertIn("has no scripts/ci/h2_baseline_*.toml", adm.evaluate(self.root, "linux", empty, [])[0])
        (self.root / "clippy.toml").write_text(CLIPPY_TOML.replace("H2 W both", "H2 SUBPROC_W both"))
        self.commit("no W")
        rev = self.git("rev-parse", "HEAD").strip()
        self.assertIn("clippy.toml has no H2 W entries", adm.evaluate(self.root, "linux", rev, [])[0])
        for rel in h2.BASELINE_FILES:
            (self.root / rel).unlink()
        self.assertEqual(self.run_main("--lane", "linux", "--inert")[:2], (0, "h2-admission: no baseline committed; inert no-op\n"))
        self.assertEqual(self.run_main("--lane", "linux")[0], 2)

    def test_inert_reports_without_failing(self) -> None:
        self.edit(PROBE, 'alive("x")', 'alive("x") && crate::services::platform::tmux::has_session("y")')
        self.regen_baseline()
        code, _, err = self.run_main("--lane", "linux", "--base", self.base, "--inert")
        self.assertEqual(code, 0)
        self.assertIn("::warning::h2-admission: admission: linux", err)
        self.assertEqual(self.run_main("--lane", "linux", "--base", self.base)[0], 1)

    def test_duplicate_mod_is_an_r_o_violation(self) -> None:
        self.extra = [dup_mod("src/services/mod.rs", 3)]
        self.assertEqual([p[:28] for p in self.evaluate()], ["R-O: clippy::duplicate_mod: "])
        code, _, err = self.run_main("--lane", "linux", "--base", self.base, "--inert")
        self.assertEqual((code, "::warning::h2-admission: R-O: clippy::duplicate_mod" in err), (0, True))
        self.assertEqual(self.run_main("--lane", "linux", "--base", self.base)[0], 1)

    def test_unreadable_dep_info_follows_inert(self) -> None:
        deps = self.root / "target/debug/deps"
        depinfo, text = deps / "agentdesk-00aa.d", (deps / "agentdesk-00aa.d").read_bytes()
        self.addCleanup(deps.chmod, 0o755)
        self.addCleanup(depinfo.chmod, 0o644)
        # undecodable bytes, a read error, and a stat error while the .d is being selected
        for case, locked in (("undecodable", None), ("read", depinfo), ("stat", deps)):
            with self.subTest(case=case):
                depinfo.write_bytes(b"\xff" if locked is None else text)
                if locked is not None:
                    locked.chmod(0)
                try:
                    code, _, err = self.run_main("--lane", "linux", "--base", self.base, "--inert")
                    self.assertEqual((code, "::warning::h2-admission: R-O: " in err and "root lib dep-info" in err), (0, True))
                    self.assertEqual(self.run_main("--lane", "linux", "--base", self.base)[0], 1)
                finally:
                    deps.chmod(0o755)
                    depinfo.chmod(0o644)

class DepInfo(unittest.TestCase):
    """R-O over the root lib dep-info of a small crate; BASE is what rustc reads for the base lib.rs."""

    BASE = ("src/lib.rs", "src/a.rs", "src/sp ace.rs", "src/한글.rs")

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        lib = 'mod a;\n#[path = "sp ace.rs"]\nmod s;\n#[path = "한글.rs"]\nmod k;\n'
        files = {"src/lib.rs": lib, "src/a.rs": "", "src/sp ace.rs": "", "src/한글.rs": "", "src/b.rs": "", "src/payload.inc": ""}
        for rel, text in files.items():
            (self.root / rel).parent.mkdir(parents=True, exist_ok=True)
            (self.root / rel).write_text(text, encoding="utf-8")
        h2._MODULE_TABLES.clear()
        self.addCleanup(h2._MODULE_TABLES.clear)
        host_cfg(self)

    def mount(self, decls: str, rel: str = "src/lib.rs") -> None:
        with (self.root / rel).open("a", encoding="utf-8") as file:
            file.write(decls)
        h2._MODULE_TABLES.clear()

    def problems(self, *extra: str, lines: list[str] | None = None) -> list[str]:
        write_depinfo(self.root, "c0ffee", [*self.BASE, *extra])
        return h2_depinfo.ro_problems(self.root, [artifact(self.root, "c0ffee")] if lines is None else lines)

    def test_rust_input_must_be_in_the_module_tree(self) -> None:
        self.assertEqual(self.problems(), [])
        out_dir = "target/debug/build/agentdesk-1/out/g.rs"
        for extra, rel in (("src/b.rs", "src/b.rs"), (str(self.root / out_dir), out_dir)):  # unmounted file, OUT_DIR code
            with self.subTest(extra=extra):
                self.assertEqual(self.problems(extra), [f"R-O: {rel} is compiled into the lib but is not in the module tree"])

    def test_allowlisted_data_inputs_pass(self) -> None:
        self.assertEqual(self.problems(str(self.root / "migrations/postgres/0001_init.sql"), "Cargo.toml", "clippy.toml",
                                       "src/../defaults.json", "src/server/../../assets/runner-entry.html"), [])

    def test_other_non_rust_inputs_fail(self) -> None:
        with tempfile.TemporaryDirectory() as elsewhere:
            (outside := Path(elsewhere) / "g.rs").write_text("")
            self.assertIn("outside the repo", self.problems(str(outside))[0])
        for extra in ("src/shared.inc", "migrations/postgres/sub/x.sql", "vendor/migrations/postgres/x.sql", "assets/a.html"):
            with self.subTest(extra=extra):
                self.assertEqual(self.problems(extra), [f"R-O: lib compile input {extra} is not in the data allowlist"])
        # mounted by `#[path]`, so in the module tree, yet still not data
        self.mount('#[path = "payload.inc"]\nmod payload;\n')
        self.assertEqual(self.problems("src/payload.inc"), ["R-O: lib compile input src/payload.inc is not in the data allowlist"])

    def test_paths_are_unescaped_and_normalized(self) -> None:
        # `\ ` escapes, UTF-8, canonical absolute spelling and `..` all name module-tree files
        self.assertEqual(self.problems("src/sp ace.rs", "src/한글.rs", os.path.realpath(self.root / "src/a.rs"),
                                       "src/x/../a.rs"), [])

    def test_symlinked_modules_compare_by_target(self) -> None:
        with tempfile.TemporaryDirectory() as elsewhere:
            (outside := Path(os.path.realpath(elsewhere)) / "g.rs").write_text("")
            (self.root / "src/real.rs").write_text("")
            (self.root / "src/sym.rs").symlink_to("real.rs")
            (self.root / "src/out.rs").symlink_to(outside)
            self.mount("mod sym;\nmod out;\n")
            self.assertEqual(self.problems("src/sym.rs", "src/x/../sym.rs"), [])
            # `..` right after a directory symlink resolves on disk, for the module and its children
            (self.root / "shared/nested").mkdir(parents=True)
            (self.root / "shared/payload.rs").write_text("mod inner;\n")
            (self.root / "shared/inner.rs").write_text("")
            (self.root / "src/jump").symlink_to("../shared/nested")
            self.mount('#[path = "jump/../payload.rs"]\nmod hop;\n')
            hop = ("src/sym.rs", "src/jump/../payload.rs", "src/jump/../inner.rs")
            self.assertEqual(self.problems(*hop), [])
            self.assertEqual(self.problems(*hop, "src/out.rs"), [f"R-O: lib compile input {outside.as_posix()} is outside the repo"])

    def test_module_identity_is_the_real_file(self) -> None:
        # two files whose `..`-collapsed spellings collide are both modules, whichever is declared first
        (self.root / "shared/nested").mkdir(parents=True)
        (self.root / "shared/foo.rs").write_text("")
        (self.root / "src/foo.rs").write_text("")
        (self.root / "src/jump").symlink_to("../shared/nested")
        lib = (self.root / "src/lib.rs").read_text(encoding="utf-8")
        for decls in ('mod foo;\n#[path = "jump/../foo.rs"]\nmod hop;\n', '#[path = "jump/../foo.rs"]\nmod hop;\nmod foo;\n'):
            with self.subTest(decls=decls):
                (self.root / "src/lib.rs").write_text(lib, encoding="utf-8")
                self.mount(decls)
                self.assertEqual(self.problems("src/foo.rs", "src/jump/../foo.rs"), [])
                table = h2._module_table(self.root)
                self.assertEqual((table["src/foo.rs"], table["shared/foo.rs"]), ("agentdesk::foo", "agentdesk::hop"))
        # a spelling that leaves the repo lexically but lands inside it on disk is a module, and so is its child
        (self.root / "deep/a/b/c").mkdir(parents=True)
        (self.root / "deep/far.rs").write_text("mod kid;\n")
        (self.root / "deep/kid.rs").write_text("")
        (self.root / "src/down").symlink_to("../deep/a/b/c")
        (self.root / "src/lib.rs").write_text(lib, encoding="utf-8")
        self.mount('#[path = "down/../../../far.rs"]\nmod far;\n')
        self.assertEqual(self.problems("src/down/../../../far.rs", "src/down/../../../kid.rs"), [])

    def test_aliases_keep_the_rule_of_each_spelling(self) -> None:
        (self.root / "src/payload.inc").unlink()
        (self.root / "src/payload.inc").symlink_to("real.rs")
        (self.root / "src/real.rs").write_text("")
        (self.root / "migrations/postgres").mkdir(parents=True)
        (self.root / "migrations/postgres/1.sql").write_text("")
        (self.root / "src/alias.rs").symlink_to("../migrations/postgres/1.sql")
        (self.root / "src/data.inc").write_text("")
        (self.root / "src/sym.rs").symlink_to("data.inc")
        lib = (self.root / "src/lib.rs").read_text(encoding="utf-8")
        data, code = "R-O: lib compile input {} is not in the data allowlist", "R-O: {} is compiled into the lib but is not in the module tree"
        payload = '#[path = "payload.inc"]\nmod payload;\n'
        for decls, extra, expected in (
                (payload, ("src/payload.inc",), data.format("src/payload.inc")),  # non-Rust spelling of a module file
                (payload, ("src/payload.inc", "src/real.rs"), data.format("src/payload.inc")),
                ("", ("src/alias.rs",), code.format("src/alias.rs")),  # Rust spelling of allowlisted data
                ("mod sym;\n", ("src/sym.rs",), data.format("src/sym.rs"))):  # mounted Rust spelling of non-allowlisted data
            with self.subTest(extra=extra):
                (self.root / "src/lib.rs").write_text(lib + decls, encoding="utf-8")
                h2._MODULE_TABLES.clear()
                self.assertEqual(self.problems(*extra), [expected])

    def test_inactive_declarations_do_not_explain_a_compiled_file(self) -> None:
        # rustc compiled src/shared.rs (say an owner mounted it); only an active declaration explains it
        (self.root / "src/shared.rs").write_text("")
        (self.root / "src/m").mkdir()
        (self.root / "src/gate.rs").write_text('#![cfg(any())]\n#[path = "shared.rs"]\nmod shared;\n')
        lib = (self.root / "src/lib.rs").read_text(encoding="utf-8")
        unexplained = ["R-O: src/shared.rs is compiled into the lib but is not in the module tree"]
        for decls, want in (
                ("#[cfg(any())]\nmod shared;\n", unexplained),
                ("#[cfg(\n    any()\n)]\nmod shared;\n", unexplained),
                ("#[cfg(windows)] mod shared;\n", unexplained),
                ("#[cfg_attr(all(), cfg(any()))]\nmod shared;\n", unexplained),
                ('#[cfg(test)]\nmod m {\n    #[path = "../shared.rs"]\n    mod shared;\n}\n', unexplained),
                ('#[cfg(test)]\nfn f() {\n    #[path = "shared.rs"]\n    mod shared;\n}\n', unexplained),
                ("/*\nmod shared;\n*/\n", unexplained),
                ('const S: &str = r#"\nmod shared;\n"#;\n', unexplained),
                ("macro_rules! decoy {\n    () => {\n        mod shared;\n    };\n}\n", unexplained),
                ("mod gate;\n", unexplained),  # the file is read, its `#![cfg]` drops its children
                ("#[cfg(unix)]\nmod shared;\n", []),
                ("#[cfg(all(unix, not(test), target_os = \"linux\"))]\nmod shared;\n", []),
                ('#[cfg_attr(unix, path = "shared.rs")]\nmod other;\n', [])):
            with self.subTest(decls=decls):
                (self.root / "src/lib.rs").write_text(lib + decls, encoding="utf-8")
                h2._MODULE_TABLES.clear()
                self.assertEqual(self.problems("src/shared.rs", *(["src/gate.rs"] if "gate" in decls else [])), want)
        # features come from the root lib artifact of the same clippy run
        (self.root / "src/lib.rs").write_text(lib + '#[cfg(feature = "tls")]\nmod shared;\n', encoding="utf-8")
        h2._MODULE_TABLES.clear()
        write_depinfo(self.root, "c0ffee", [*self.BASE, "src/shared.rs"])
        self.assertEqual(h2_depinfo.ro_problems(self.root, [artifact(self.root, "c0ffee", features=("tls",))]), [])
        self.assertEqual(h2_depinfo.ro_problems(self.root, [artifact(self.root, "c0ffee", features=("other",))]), unexplained)

    def test_unevaluable_cfg_is_a_problem(self) -> None:
        (self.root / "src/shared.rs").write_text("")
        lib = (self.root / "src/lib.rs").read_text(encoding="utf-8")
        for pred in ("tokio_unstable", 'version("1.80")', "unix = ", "not(unix, windows)", "all(unix"):
            with self.subTest(pred=pred):
                (self.root / "src/lib.rs").write_text(f"{lib}#[cfg({pred})]\nmod shared;\n", encoding="utf-8")
                h2._MODULE_TABLES.clear()
                self.assertEqual([p[:len("R-O: src/lib.rs: cannot evaluate cfg")] for p in self.problems()],
                                 ["R-O: src/lib.rs: cannot evaluate cfg"])

    def test_active_modules_rustc_did_not_read_are_a_problem(self) -> None:
        # an evaluator that calls a module active which rustc never read is wrong, so the gate says so
        (self.root / "src/t.rs").write_text("")
        self.mount("#[cfg(test)]\nmod t;\n")
        self.BASE = ("src/lib.rs", "src/sp ace.rs", "src/한글.rs")
        self.assertEqual(self.problems(), ["R-O: cfg evaluation puts src/a.rs in the lib build but rustc did not read it"])

    def test_invalid_dep_info_is_a_problem(self) -> None:
        depinfo = write_depinfo(self.root, "c0ffee", ["src/lib.rs", "src/a.rs"])
        cases = {"empty": "", "stray line": depinfo.read_text(encoding="utf-8") + "not a rule\n",
                 "no compile rule": "unrelated: Cargo.toml\n", "no root source": f"{depinfo}: src/a.rs\n"}
        for case, text in cases.items():
            with self.subTest(case=case):
                depinfo.write_text(text, encoding="utf-8")
                problems = h2_depinfo.ro_problems(self.root, [artifact(self.root, "c0ffee")])
                self.assertEqual([p[:len(f"R-O: root lib dep-info {depinfo}")] for p in problems],
                                 [f"R-O: root lib dep-info {depinfo}"])

    def test_selecting_the_dep_info_fails_closed_on_every_python(self) -> None:
        # Python 3.14's is_file() swallows OSError, so a chmod cannot reach this boundary there; raise at the call
        depinfo, is_file, hits = write_depinfo(self.root, "c0ffee", self.BASE), Path.is_file, []
        def failing(path: Path) -> bool:
            if path == depinfo:
                hits.append(path)
                raise PermissionError(13, "Permission denied", str(path))
            return is_file(path)
        with mock.patch.object(Path, "is_file", failing):
            problems = h2_depinfo.ro_problems(self.root, [artifact(self.root, "c0ffee")])
        self.assertEqual((hits, problems), ([depinfo], [f"R-O: cannot read root lib dep-info: [Errno 13] Permission denied: '{depinfo}'"]))

    def test_dep_info_is_matched_by_the_root_lib_hash(self) -> None:
        decoy = write_depinfo(self.root, "deadbeef", ["src/lib.rs", "src/b.rs"])
        os.utime(decoy, (time.time() + 60, time.time() + 60))  # newest on disk, but not this run's artifact
        others = [artifact(self.root, "5e5e", name="serde", src="vendor/serde/src/lib.rs"),
                  artifact(self.root, "7e57", test=True)]
        self.assertEqual(self.problems(lines=[*others, artifact(self.root, "c0ffee")]), [])
        for lines, needle in (([], "expected one root lib artifact dep-info, found []"),
                              ([artifact(self.root, "c0ffee"), artifact(self.root, "deadbeef")], "expected one root lib"),
                              ([artifact(self.root, "0bad")], "agentdesk-0bad.d does not exist")):
            with self.subTest(needle=needle):
                self.assertIn(needle, self.problems(lines=lines)[0])

class ParseAdmissions(unittest.TestCase):
    def test_schema(self) -> None:
        self.assertEqual(adm.parse_admissions(None), [])
        self.assertEqual(adm.parse_admissions(textwrap.dedent(GROW))[0]["lane"], "both")
        for bad in ('"both"', '"windows"'), ("issue = 5340", "issue = 0"), ("new = 1", "new = 0"), \
                   ("old = 0", 'old = "0"'), ("base_sha", "kind = 1\nbase_sha"), ("issue = 5340\n", ""), \
                   ("issue", 'lines = ["4"]\nissue'):
            with self.subTest(bad=bad), self.assertRaises(adm.AdmissionError):
                adm.parse_admissions(textwrap.dedent(GROW).replace(*bad))

class ZeroRules(unittest.TestCase):
    OLD = "src/old.rs"  # grandfathered: already pairs `Command` with one tmux message literal
    OLD_TEXT = 'use std::process::Command;\nfn f() {\n    Command::new("git");\n    log("tmux session died");\n}\n'
    PIN = {OLD: {("f", 'log("tmux session died");'): 1}}  # (item, whitespace-normalized line)

    def rules(self, files: dict[str, str], roster=frozenset(), pins=None) -> list[str]:
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(adm, "OWNER_ROSTER", roster), \
                mock.patch.object(adm, "R_C_GRANDFATHERED", self.PIN if pins is None else pins):
            for rel, text in {self.OLD: self.OLD_TEXT, **files}.items():
                (Path(tmp) / rel).parent.mkdir(parents=True, exist_ok=True)
                (Path(tmp) / rel).write_text(textwrap.dedent(text), encoding="utf-8")
            return sorted(p.split(":")[0] for p in adm.zero_rules(Path(tmp)) + adm.owner_shape_problems(Path(tmp)))

    def test_clean_shapes_pass(self) -> None:
        self.assertEqual(self.rules({
            "src/a.rs": """\
                use std::process::Command;
                // Command::new("tmux") in a comment
                fn f() { Command::new("git").arg("status"); }
                const LABEL: &str = "probe-tmux";
                #[cfg(test)]
                mod tests { fn t() { std::process::Command::new("tmux"); libc::execvp(); } }
                """,
            "src/msg.rs": 'fn f() -> String { "tmux session died".into() }\n',  # no `Command` token
            "src/a_tests.rs": 'fn t() { Command::new("tmux"); }\n',
            "src/services/platform/tmux.rs": 'fn own() { Command::new("tmux"); }\n',
            "src/runtime_layout/windows_links.rs": "fn junction() {}\n",
            "Cargo.lock": 'name = "empty-lock"\nname = "rustyline"\n',
        }, roster=frozenset({"src/services/platform/tmux.rs"})), [])

    def test_r_c_catches_g2_shapes_and_keeps_pins_tight(self) -> None:
        # G2: a grandfathered spawn site re-pointed at tmux without a new Command::new
        for name, text in {
            "let_binding": 'use std::process::Command;\nfn f() { let bin = "tmux"; Command::new(bin); }\n',
            "alias": 'use std::process::Command as Cmd;\nfn f() { Cmd::new("tmux"); }\n',
            "wrapper": 'use std::process::Command;\nfn spawn(p: &str) { Command::new(p); }\nfn f() { spawn("tmux"); }\n',
            "path_program": 'fn f() { std::process::Command::new("/opt/bin/tmux"); }\n',
            "shell_arg": 'use std::process::Command;\nfn f(c: &mut Command) { c.args(["-c", "tmux kill-server"]); }\n',
        }.items():
            with self.subTest(shape=name):
                self.assertEqual(self.rules({f"src/{name}.rs": text}), ["R-C"])
        # inside a pinned file: an added literal, and a count-neutral 1:1 swap (review r2 case)
        grown = self.OLD_TEXT.replace('Command::new("git")', 'Command::new("tmux")')
        self.assertEqual(self.rules({self.OLD: grown}), ["R-C"])
        swapped = grown.replace('"tmux session died"', '"fallback via tmux died"')
        self.assertEqual(self.rules({self.OLD: swapped}), ["R-C", "R-C"])  # one gone, one new
        # moving a pinned line (reindented, reordered, shifted) inside its item stays green
        moved = "// header\n\n" + self.OLD_TEXT.replace('    Command::new("git");\n    log', '\n  log').replace(
            '"tmux session died");\n', '"tmux session died");\n    Command::new("git");\n')
        self.assertEqual(self.rules({self.OLD: moved}), [])
        # same item, same literal, new role (review r3: allowlist entry dropped, spawn program set)
        two = 'use std::process::Command;\nfn a() {\n    let allowed = ["gh", "tmux"];\n    let bin = "git";\n    Command::new(bin);\n}\n'
        pins = {self.OLD: self.PIN[self.OLD], "src/two.rs": {("a", 'let allowed = ["gh", "tmux"];'): 1}}
        self.assertEqual(self.rules({"src/two.rs": two}, pins=pins), [])
        self.assertEqual(self.rules({"src/two.rs": two.replace('["gh", "tmux"]', '["gh"]')
                                     .replace('bin = "git"', 'bin = "tmux"')}, pins=pins), ["R-C", "R-C"])
        # ... and the same literal moved into another item
        self.assertEqual(self.rules({"src/two.rs": two.replace('["gh", "tmux"]', '["gh"]')
                                     + 'fn b() { let allowed = ["gh", "tmux"]; }\n'}, pins=pins), ["R-C", "R-C"])
        stale = {self.OLD: {**self.PIN[self.OLD], ("f", "tmux gone"): 1}}
        self.assertEqual(self.rules({}, pins=stale), ["R-C"])  # a pin with no literal must be dropped
        self.assertEqual(self.rules({}, pins={}), ["R-C"])  # an unpinned existing pair is red

    def test_each_other_rule_rejects(self) -> None:
        self.assertEqual(self.rules({
            "src/c2.rs": 'static BIN: &\'static str = "tmux";\n',
            "src/f.rs": "fn f() { unsafe { libc::execvp(p, a) }; }\n",
            "src/f_ext.rs": "fn f(mut c: Command) { let _ = c.exec(); }\n",
            "src/services/session_host/extra.rs": "fn x() {}\n",
            "src/runtime_layout/windows_links.rs": "// spawns TMUX\n",
            "Cargo.lock": 'name = "portable-pty"\nname = "tmux_interface"\n',
            **ESCAPE,
        }, roster=frozenset(ESCAPE)), ["R-C2", "R-F", "R-F", "R-O", "R-O", "R-O", "R-O", "R-O"])

    def test_owner_path_attribute_needs_an_explicit_allowance(self) -> None:
        # review r3: an owner file mounting a non-owner file as its child module
        self.assertEqual(self.rules({**ESCAPE, "src/services/platform/pty_escape.rs": "pub(crate) fn stealth() {}\n"},
                                    roster=frozenset(ESCAPE)), ["R-O"])
        for attr in ('#[cfg_attr(unix, path = "pty_escape.rs")]', '#[cfg_attr(all(unix, not(test)), path="pty_escape.rs")]',
                     '#[cfg_attr(\n    unix,\n    path\n        = "pty_escape.rs"\n)]'):  # review r4: cfg_attr forms
            escape = {k: v.replace('#[path = "pty_escape.rs"]', attr) for k, v in ESCAPE.items()}
            self.assertEqual(self.rules(escape, roster=frozenset(ESCAPE)), ["R-O"], attr)
        with mock.patch.object(adm, "PATH_ATTR_ALLOWED", frozenset(ESCAPE)):
            self.assertEqual(self.rules(ESCAPE, roster=frozenset(ESCAPE)), [])

    def test_owner_path_attribute_span_is_bracket_matched(self) -> None:
        # review r5: a `]` before `path =` must not end the attribute span; string literals are already blanked
        red = ('#[cfg_attr(unix, doc = [h2], path = "pty_escape.rs")]', '#[cfg_attr(any(unix, doc = [[1], [2]]), path = "pty_escape.rs")]',
               '#[cfg_attr(\n    all(unix, not(test)),\n    doc = [h2],\n    path\n        = "pty_escape.rs"\n)]',
               '#[cfg_attr(unix, doc = "]", path = "pty_escape.rs")]')
        green = ('#[cfg_attr(unix, xpath = "pty_escape.rs")]', '#[doc = "["]', '#[doc = [h2]]\nfn f() { let path = 1; }')
        for attr, want in [*((a, ["R-O"]) for a in red), *((a, []) for a in green)]:
            escape = {k: v.replace('#[path = "pty_escape.rs"]', attr) for k, v in ESCAPE.items()}
            self.assertEqual(self.rules(escape, roster=frozenset(ESCAPE)), want, attr)

    def test_owner_path_attribute_span_skips_literals_and_comments(self) -> None:
        # review r6 P2: `]`/`[` inside a raw string, char literal or comment must not end the span early
        for attr in ('#[cfg_attr(unix, doc = r#"]"#, path = "pty_escape.rs")]', "#[cfg_attr(unix, marker = ']', path = \"pty_escape.rs\")]",
                     '#[cfg_attr(unix, /* ] [ */ path = "pty_escape.rs")]', '#[cfg_attr(unix, doc = r#"\n] [\n"#, path = "pty_escape.rs")]'):
            escape = {k: v.replace('#[path = "pty_escape.rs"]', attr) for k, v in ESCAPE.items()}
            self.assertEqual(self.rules(escape, roster=frozenset(ESCAPE)), ["R-O"], attr)

    def test_owner_macros_are_refused_outside_the_api_files(self) -> None:
        # review r6 P1: a macro can synthesize `#[path]` (`#[$attr]`), so every owner file refuses item-level macros
        host = {"src/services/session_host.rs": 'macro_rules! mount { ($attr:meta) => { #[$attr] mod escape; }; }\n'
                                                'mount!(path = "../outside_owner.rs");\n'}
        self.assertEqual(self.rules(host, roster=frozenset(host)), ["R-O", "R-O"])


    def test_owner_macros_inside_attributes_are_not_code(self) -> None:
        # a doc attribute's value macro is attribute input, not an item-level macro of the owner file
        host = {"src/services/session_host.rs": '#![doc = include_str!("../../README.md")]\n'
                                                '#[doc = concat!("x", "y")]\npub fn f() {}\n'
                                                '#[doc = include_str!(\n    "../data.sql"\n)]\npub struct S;\n'}
        self.assertEqual(self.rules(host, roster=frozenset(host)), [])
        tail = {k: v + 'concat!("a", "b");\n' for k, v in host.items()}
        self.assertEqual(self.rules(tail, roster=frozenset(tail)), ["R-O"])

ESCAPE = {"src/services/platform/tmux.rs": '#[path = "pty_escape.rs"]\npub(crate) mod escape;\npub fn has_session() {}\n'}

if __name__ == "__main__":
    unittest.main()
