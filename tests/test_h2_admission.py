"""Unit tests for the H2 admission gate (fixture trees and a throwaway git repo, no cargo)."""

from __future__ import annotations

import io
import json
import os
import shutil
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
import h2_items  # noqa: E402
import h2_measure as h2  # noqa: E402
from tests.test_h2_measure import diag, locate, session_items  # noqa: E402
from tests.test_h2_modmap import write_modmap  # noqa: E402

TMUX = "agentdesk::services::platform::tmux::has_session"
CMD, TOKIO = "std::process::Command::new", "tokio::process::Command::new"
TYPE = "agentdesk::services::relay::TmuxBackend"
W_PATHS = ("agentdesk::services::probe::alive", "agentdesk::services::probe::user", "agentdesk::services::relay::build",
           "agentdesk::services::relay::Backend::send")
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
# compiler item paths of the fixture fns (the trait impl registers its trait method)
PATHS = {(PROBE, "alive"): W_PATHS[0], (PROBE, "user"): W_PATHS[1], (PROBE, "fresh"): "agentdesk::services::probe::fresh",
         (RELAY, "build"): W_PATHS[2], (RELAY, "send"): W_PATHS[3]}
# (file, needle, callee, lint) for every diagnostic the fixture sources produce
NEEDLES = [(OWNER, "pub fn has_session", TMUX, None), (PROBE, "crate::services::platform::tmux::has_session(name)", TMUX, None),
           (PROBE, 'alive("x")', "agentdesk::services::probe::alive", None), (PROBE, 'Command::new("git")', CMD, None),
           (PROBE, 'Command::new("gh")', CMD, None), (PROBE, 'has_session("y")', TMUX, None),
           (PROBE, 'has_session("z")', TMUX, None), (RELAY, 'has_session("s")', TMUX, None),
           (RELAY, "TmuxBackend)", TYPE, "clippy::disallowed_types")]

def diag_lines(sources: dict[str, str]) -> list[str]:
    return [diag(file, *locate(sources[file], needle), callee, lint=lint or "clippy::disallowed_methods")
            for file, needle, callee, lint in NEEDLES if needle in sources[file]]

def artifact(root: Path, digest: str, *, name: str = "agentdesk", src: str = "src/lib.rs", test: bool = False) -> str:
    """A cargo `compiler-artifact` line for a lib whose dep-info is `target/debug/deps/<name>-<digest>.d`."""
    return json.dumps({"reason": "compiler-artifact", "target": {"kind": ["lib"], "name": name, "src_path": str(root / src)},
                       "profile": {"test": test}, "filenames": [str(root / f"target/debug/deps/lib{name}-{digest}.rmeta")]})

def write_depinfo(root: Path, digest: str, deps) -> Path:
    """A rustc-shaped dep-info: `.d` and `.rmeta` rules, per-file empty rules, env-dep comments."""
    path = root / f"target/debug/deps/agentdesk-{digest}.d"
    path.parent.mkdir(parents=True, exist_ok=True)
    escaped = [str(d).replace(" ", "\\ ") for d in deps]
    rmeta = path.with_name(f"libagentdesk-{digest}.rmeta")
    path.write_text(f"{path}: {' '.join(escaped)}\n\n{rmeta}: {' '.join(escaped)}\n\n" + "".join(f"{d}:\n" for d in escaped)
                    + f"\n# env-dep:CARGO_MANIFEST_DIR={root}\n# env-dep:CLIPPY_CONF_DIR\n", encoding="utf-8")
    return path

def mod_row(file: str, modpath: str, parent: str = "src/lib.rs", attrs: str = "-") -> str:
    """A driver map row for a hand-written `mod x;` in `parent`."""
    return f"{file}\t{modpath}\t#0\t#0\tmodule\t{parent}\t{parent}:1:1: 1:9 (#0)\t{attrs}\tfile"

TREE_MAP = [mod_row("src/services/mod.rs", "crate::services"),
            mod_row("src/services/platform/mod.rs", "crate::services::platform", "src/services/mod.rs"),
            mod_row(OWNER, "crate::services::platform::tmux", "src/services/platform/mod.rs"),
            mod_row(PROBE, "crate::services::probe", "src/services/mod.rs"),
            mod_row(RELAY, "crate::services::relay", "src/services/mod.rs", 'path#0["relay_impl.rs"]')]

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
        self.root = Path(tmp.name).resolve()
        for name, value in PATCHES.items():
            patcher = mock.patch.object(adm, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.sources, self.extra = dict(SOURCES), []
        self.write(self.sources)
        (self.root / "scripts/ci").mkdir(parents=True)
        (self.root / "clippy.toml").write_text(CLIPPY_TOML, encoding="utf-8")
        self.regen_baseline()
        write_depinfo(self.root, "00aa", [*SOURCES, "Cargo.toml"])
        self.modmap = write_modmap(self.root / "target/modmap.tsv", TREE_MAP)
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
        return h2.measure(self.root, self.items(lines=diag_lines(self.sources)), h2.load_config(self.root / "clippy.toml"))

    def items(self, lane: str = "linux", lines=None, conf: Path | None = None, run: Path | None = None):
        conf = conf or self.root.parent / f"{self.root.name}-conf"
        return session_items(self.root, self.sources, self.lines() if lines is None else lines, PATHS,
                             conf=conf, run=run or conf.parent / f"{self.root.name}-run", lane=lane)

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
        return adm.evaluate(self.root, lane, self.base, self.items(lane), self.modmap)

    def run_main(self, *args: str) -> tuple[int, str, str]:
        """admission --session over a sealed run of this lane (its load is the fixture's Items)."""
        lane = args[args.index("--lane") + 1]
        conf = self.root.parent / f"{self.root.name}-{lane}-conf"
        conf.mkdir(exist_ok=True)
        self.addCleanup(shutil.rmtree, conf, ignore_errors=True)
        h2.write_conf(conf, h2.load_config(self.root / "clippy.toml"), lane)
        run = conf.parent / f"{self.root.name}-{lane}-run"
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err), \
                mock.patch.object(h2_items, "load", return_value=self.items(lane, conf=conf, run=run)), \
                mock.patch.object(h2, "session_runner", side_effect=AssertionError("--session runs no session")):
            code = adm.main(["--repo", str(self.root), "--session", str(run), "--modmap", str(self.modmap), *args])
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
        with redirect_stdout(out := io.StringIO()), mock.patch.object(h2, "session_runner", return_value=lambda conf, run:
                self.items(lines=diag_lines(self.sources), conf=conf, run=run)):
            h2.main(["--repo", str(self.root), "--lane", "linux"])
        rows = json.loads(out.getvalue())["rows"]
        self.assertEqual([r.get("lines") for r in rows["subproc"]], [[4, 5]])
        self.assertNotIn("lines", rows["exec"][0])

class EndToEnd(Tree):
    def test_clean_tree_passes_both_lanes(self) -> None:
        self.assertEqual(self.evaluate("linux"), [])
        self.assertEqual(self.evaluate("macos"), [])
        self.assertEqual(self.run_main("--lane", "macos", "--base", self.base)[0], 0)

    def test_own_session_uses_only_lane_paths_and_exempts_only_its_config(self) -> None:
        config = h2.load_config(self.root / "clippy.toml")
        config["agentdesk::linux_only"] = ("W", frozenset({"linux"}))
        (self.root / "clippy.toml").write_text(h2.render_clippy_toml(config))
        generated = []
        def runner(conf, run):
            generated.append(conf / "clippy.toml")
            self.assertEqual(run, conf.parent / "check")
            self.assertEqual(h2.load_config(conf / "clippy.toml"), {p: e for p, e in config.items() if "macos" in e[1]})
            write_depinfo(self.root, "00aa", [*SOURCES, "Cargo.toml", conf / "clippy.toml"])
            return self.items("macos", conf=conf, run=run)
        with mock.patch.object(h2, "session_runner", return_value=runner) as start, \
                redirect_stdout(io.StringIO()), redirect_stderr(err := io.StringIO()):
            self.assertEqual(adm.main(["--repo", str(self.root), "--lane", "macos", "--base", self.base,
                                      "--modmap", str(self.modmap)]), 0, err.getvalue())
        start.assert_called_once_with(self.root, "macos")
        self.assertTrue(generated[0].is_relative_to(self.root / h2.SESSIONS))
        rel = generated[0].relative_to(self.root)
        self.assertEqual(self.evaluate("macos"), [f"R-O: lib compile input {rel} is not in the data allowlist"])

    def test_session_config_does_not_exempt_other_inputs_beside_it(self) -> None:
        for extra, problem in (("other.json", "is not in the data allowlist"), ("nested/clippy.toml", "is not in the data allowlist"),
                               ("alias.rs", "is compiled into the lib but is not in the module tree")):
            def runner(conf, run):
                path = conf / extra
                path.parent.mkdir(parents=True, exist_ok=True)
                if extra == "alias.rs":
                    path.symlink_to(conf / "clippy.toml")
                else:
                    path.write_text("unrelated")
                write_depinfo(self.root, "00aa", [*SOURCES, "Cargo.toml", conf / "clippy.toml", path])
                return self.items("macos", conf=conf, run=run)
            with self.subTest(extra=extra), mock.patch.object(h2, "session_runner", return_value=runner), \
                    redirect_stderr(err := io.StringIO()):
                rc = adm.main(["--repo", str(self.root), "--lane", "macos", "--base", self.base,
                               "--modmap", str(self.modmap)])
                self.assertEqual(rc, 1)
                self.assertIn(f"{extra} {problem}", err.getvalue())

    def test_evaluate_filters_session_by_lane(self) -> None:
        config = h2.load_config(self.root / "clippy.toml")
        config["agentdesk::linux_only"] = ("W", frozenset({"linux"}))
        (self.root / "clippy.toml").write_text(h2.render_clippy_toml(config))
        lines = [*self.lines(), diag(PROBE, 1, 1, "agentdesk::linux_only")]
        self.assertEqual(adm.evaluate(self.root, "macos", self.base, self.items("macos", lines), self.modmap), [])

    def test_config_warning_rejects_a_session_with_or_without_code(self) -> None:
        for code in (None, "clippy::disallowed_methods", "unused_imports"):
            warning = json.loads(diag("clippy.toml", 2, 1, "agentdesk::missing"))
            warning["message"]["code"] = {"code": code} if code else None
            self.extra = [json.dumps(warning)]
            with self.subTest(code=code):
                rc, _, err = self.run_main("--lane", "macos", "--base", self.base)
                self.assertEqual(rc, 1)
                self.assertIn("clippy could not use an H2 path", err)

    def test_without_a_sealed_session_nothing_is_evaluated(self) -> None:
        empty = self.root.parent / f"{self.root.name}-unsealed"
        empty.mkdir()
        self.addCleanup(empty.rmdir)
        with redirect_stderr(err := io.StringIO()), redirect_stdout(io.StringIO()):
            rc = adm.main(["--repo", str(self.root), "--lane", "linux", "--base", self.base, "--modmap", str(self.modmap),
                           "--session", str(empty)])
        self.assertEqual((rc, "unsealed" in err.getvalue()), (1, True))
        with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):  # the unbound JSONL input is gone
            adm.main(["--repo", str(self.root), "--lane", "linux", "--base", self.base, "--modmap", str(self.modmap),
                      "--json", str(empty)])

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

    def test_rw_reads_each_measured_site_path(self) -> None:
        config, key = h2.load_config(self.root / "clippy.toml"), (RELAY, "<TmuxBackend as Backend>::send", TMUX)
        self.assertIsNone(adm.rw_problem(config, "linux", key, {key: [W_PATHS[3]]}))  # T22: the site's own path
        self.assertIsNone(adm.rw_problem(config, "linux", (PROBE, "user", CMD), {}))  # SUBPROC is not R-W
        self.assertIsNone(adm.rw_problem(config, "linux", (RELAY, "<module>", TYPE), {(RELAY, "<module>", TYPE): ["<module>"]}))
        for reg, why in (({}, "no measured site"), ({key: []}, "no measured site"),  # T23: never all([])
                         ({key: [W_PATHS[3], "agentdesk::services::relay::Other::send"]}, "site path agentdesk::services::relay::Other::send"),
                         ({key: [W_PATHS[3], None]}, "an unregistrable site")):  # T24, T25
            with self.subTest(why=why):
                self.assertEqual(adm.rw_problem(config, "linux", key, reg),
                                 f"R-W: {RELAY} :: {key[1]} gained {TMUX} but {why} is not a registered linux W* fn; "
                                 "run h2_measure.py --regen")
        # a registered Self type or an item-name prefix no longer exempts a site
        config[W_PATHS[3]] = ("W", frozenset({"macos"}))
        self.assertIsNotNone(adm.rw_problem(config, "linux", key, {key: [W_PATHS[3]]}))
        self.assertIsNotNone(adm.rw_problem(config, "linux", (PROBE, "user::inner", TMUX), {(PROBE, "user::inner", TMUX): [None]}))

    def test_rw_judges_only_growth_in_this_lane(self) -> None:
        self.edit(RELAY, 'has_session("s"); }', 'has_session("s"); let _ = crate::services::platform::tmux::has_session("t"); }')
        NEEDLES.append((RELAY, 'has_session("t")', TMUX, None))
        self.addCleanup(NEEDLES.pop)
        PATHS[(RELAY, "send")] = "!external:core"
        self.addCleanup(PATHS.__setitem__, (RELAY, "send"), W_PATHS[3])
        baseline = h2.load_baseline(self.root)
        baseline["exec"][(RELAY, "<TmuxBackend as Backend>::send", TMUX)] = {"linux": 1, "macos": 2}
        h2.write_baseline(self.root, baseline)
        self.admit(GROW.replace('"user"', '"<TmuxBackend as Backend>::send"').replace('"both"', '"macos"')
                   .replace("old = 0", "old = 1").replace("new = 1", "new = 2").replace(PROBE, RELAY))
        rw = [p for p in self.evaluate("linux") if p.startswith("R-W")]
        self.assertEqual(rw, [])  # grew only in macos: linux's session does not judge it
        self.assertEqual([p for p in self.evaluate("macos") if p.startswith("R-W")],
                         [f"R-W: {RELAY} :: <TmuxBackend as Backend>::send gained {TMUX} but an unregistrable site "
                          "is not a registered macos W* fn; run h2_measure.py --regen"])

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
        self.assertIn("has no scripts/ci/h2_baseline_*.toml", adm.evaluate(self.root, "linux", empty, [], self.modmap)[0])
        (self.root / "clippy.toml").write_text(CLIPPY_TOML.replace("H2 W both", "H2 SUBPROC_W both"))
        self.commit("no W")
        rev = self.git("rev-parse", "HEAD").strip()
        self.assertIn("clippy.toml has no H2 W entries", adm.evaluate(self.root, "linux", rev, [], self.modmap)[0])
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
    """R-O over the root lib dep-info against the driver's module map of a small crate."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.addCleanup(h2._MODULE_TABLES.clear)
        self.root = Path(tmp.name)
        for rel in ("src/lib.rs", "src/a.rs", "src/sp ace.rs", "src/한글.rs", "src/b.rs", "src/payload.inc"):
            (self.root / rel).parent.mkdir(parents=True, exist_ok=True)
            (self.root / rel).write_text("", encoding="utf-8")
        self.modmap = self.root / "modmap.tsv"
        self.mount(())

    def mount(self, modules: tuple[str, ...]) -> None:
        """lib.rs mounts src/a.rs and `modules` as crate::m0.., and the module map says so."""
        mods = ("src/a.rs", *modules)
        (self.root / "src/lib.rs").write_text("".join(f'#[path = "{os.path.relpath(rel, "src")}"]\nmod m{n};\n'
                                                      for n, rel in enumerate(mods)), encoding="utf-8")
        h2._MODULE_TABLES.clear()
        write_modmap(self.modmap, [mod_row(rel, f"crate::m{n}") for n, rel in enumerate(mods)])

    def problems(self, *extra: str, modules: tuple[str, ...] = (), lines: list[str] | None = None) -> list[str]:
        """src/lib.rs, src/a.rs and `extra` compiled; src/a.rs and `modules` mounted."""
        self.mount(modules)
        write_depinfo(self.root, "c0ffee", ["src/lib.rs", "src/a.rs", *extra])
        return h2_depinfo.ro_problems(self.root, [artifact(self.root, "c0ffee")] if lines is None else lines, self.modmap)

    def test_rust_input_must_be_in_the_module_tree(self) -> None:
        self.assertEqual(self.problems(), [])
        out_dir = "target/debug/build/agentdesk-1/out/g.rs"
        for extra, rel in (("src/b.rs", "src/b.rs"), (str(self.root / out_dir), out_dir)):  # unmounted file, OUT_DIR code
            with self.subTest(extra=extra):
                self.assertEqual(self.problems(extra), [f"R-O: {rel} is compiled into the lib but is not in the module tree"])

    def test_module_file_must_be_a_rust_input(self) -> None:
        # a map rustc's own .d does not back (another build, a stale map) cannot vouch for a module
        self.assertEqual(self.problems(modules=("src/b.rs",)),
                         ["R-O: module file src/b.rs is not among the lib's .rs compile inputs"])
        self.assertEqual(self.problems("src/b.rs", modules=("src/b.rs",)), [])

    def test_allowlisted_data_inputs_pass(self) -> None:
        self.assertEqual(self.problems(str(self.root / "migrations/postgres/0001_init.sql"), "Cargo.toml", "clippy.toml",
                                       "src/../defaults.json", "src/server/../../assets/runner-entry.html"), [])

    def test_other_non_rust_inputs_fail(self) -> None:
        with tempfile.TemporaryDirectory() as elsewhere:
            (outside := Path(elsewhere) / "g.rs").write_text("")
            self.assertIn("outside the repo", self.problems(str(outside))[0])
        data = "R-O: lib compile input {} is not in the data allowlist"
        for extra in ("src/shared.inc", "migrations/postgres/sub/x.sql", "vendor/migrations/postgres/x.sql", "assets/a.html"):
            with self.subTest(extra=extra):
                self.assertEqual(self.problems(extra), [data.format(extra)])
        # src/payload.inc mounted by `#[path]` is in the module map yet still not data
        self.assertEqual(self.problems("src/payload.inc", modules=("src/payload.inc",)),
                         ["R-O: file module crate::m1 (src/payload.inc) is not a .rs file inside the repo",
                          data.format("src/payload.inc"), "R-O: module file src/payload.inc is not among the lib's .rs compile inputs"])

    def test_paths_are_unescaped_and_normalized(self) -> None:
        # `\ ` escapes, UTF-8, canonical absolute spelling and `..` all name module-map files
        self.assertEqual(self.problems("src/sp ace.rs", "src/한글.rs", os.path.realpath(self.root / "src/a.rs"),
                                       "src/x/../a.rs", modules=("src/sp ace.rs", "src/한글.rs")), [])

    def test_symlinked_modules_compare_by_target(self) -> None:
        # the driver names a module by its realpath; every .d spelling of that file must resolve there
        with tempfile.TemporaryDirectory() as elsewhere:
            (outside := Path(os.path.realpath(elsewhere)) / "g.rs").write_text("")
            (self.root / "src/real.rs").write_text("")
            (self.root / "src/sym.rs").symlink_to("real.rs")
            (self.root / "src/out.rs").symlink_to(outside)
            self.assertEqual(self.problems("src/sym.rs", "src/x/../sym.rs", modules=("src/real.rs",)), [])
            # lib.rs mounts the link while the driver names its target: the walker's spelling resolves before comparing
            (self.root / "src/lib.rs").write_text('#[path = "a.rs"]\nmod m0;\n#[path = "sym.rs"]\nmod m1;\n', encoding="utf-8")
            h2._MODULE_TABLES.clear()
            self.assertEqual(h2_depinfo.ro_problems(self.root, [artifact(self.root, "c0ffee")], self.modmap), [])
            # `..` right after a directory symlink resolves on disk, for the file the walker opens and for its child
            (self.root / "shared/nested").mkdir(parents=True)
            (self.root / "shared/payload.rs").write_text("mod inner;\n")
            (self.root / "shared/inner.rs").write_text("")
            (self.root / "src/jump").symlink_to("../shared/nested")
            (self.root / "src/lib.rs").write_text('#[path = "a.rs"]\nmod m0;\n#[path = "jump/../payload.rs"]\nmod m1;\n',
                                                  encoding="utf-8")
            h2._MODULE_TABLES.clear()
            write_depinfo(self.root, "c0ffee", ["src/lib.rs", "src/a.rs", "src/jump/../payload.rs", "src/jump/../inner.rs"])
            write_modmap(self.modmap, [mod_row("src/a.rs", "crate::m0"), mod_row("shared/payload.rs", "crate::m1"),
                                       mod_row("shared/inner.rs", "crate::m1::inner", "shared/payload.rs")])
            self.assertEqual(h2_depinfo.ro_problems(self.root, [artifact(self.root, "c0ffee")], self.modmap), [])
            self.assertEqual(self.problems("src/out.rs"), [f"R-O: lib compile input {outside.as_posix()} is outside the repo"])

    def test_aliases_keep_the_rule_of_each_spelling(self) -> None:
        (self.root / "src/payload.inc").unlink()
        (self.root / "src/payload.inc").symlink_to("real.rs")
        (self.root / "src/real.rs").write_text("")
        (self.root / "migrations/postgres").mkdir(parents=True)
        (self.root / "migrations/postgres/1.sql").write_text("")
        (self.root / "src/alias.rs").symlink_to("../migrations/postgres/1.sql")
        (self.root / "src/data.inc").write_text("")
        (self.root / "src/sym.rs").symlink_to("data.inc")
        data, code = "R-O: lib compile input {} is not in the data allowlist", "R-O: {} is compiled into the lib but is not in the module tree"
        not_rs = "R-O: file module crate::m1 ({}) is not a .rs file inside the repo"
        for extra, modules, expected in (
                (("src/payload.inc",), ("src/real.rs",), [data.format("src/payload.inc")]),  # non-Rust spelling of a module
                (("src/payload.inc", "src/real.rs"), ("src/real.rs",), [data.format("src/payload.inc")]),
                (("src/alias.rs",), (), [code.format("src/alias.rs")]),  # Rust spelling of allowlisted data
                # mounted Rust spellings of non-allowlisted data, and of allowlisted data (only the module rule sees it)
                (("src/sym.rs",), ("src/data.inc",), [not_rs.format("src/data.inc"), data.format("src/sym.rs")]),
                (("src/alias.rs",), ("migrations/postgres/1.sql",), [not_rs.format("migrations/postgres/1.sql")])):
            with self.subTest(extra=extra, modules=modules):
                self.assertEqual(self.problems(*extra, modules=modules), expected)

    def test_invalid_dep_info_is_a_problem(self) -> None:
        depinfo = write_depinfo(self.root, "c0ffee", ["src/lib.rs", "src/a.rs"])
        cases = {"empty": "", "stray line": depinfo.read_text(encoding="utf-8") + "not a rule\n",
                 "no compile rule": "unrelated: Cargo.toml\n", "no root source": f"{depinfo}: src/a.rs\n"}
        for case, text in cases.items():
            with self.subTest(case=case):
                depinfo.write_text(text, encoding="utf-8")
                problems = h2_depinfo.ro_problems(self.root, [artifact(self.root, "c0ffee")], self.modmap)
                self.assertEqual([p[:len(f"R-O: root lib dep-info {depinfo}")] for p in problems],
                                 [f"R-O: root lib dep-info {depinfo}"])

    def test_unreadable_or_malformed_map_is_a_problem(self) -> None:
        self.assertEqual(self.problems(), [])
        good, row = self.modmap.read_text(encoding="utf-8"), mod_row("src/a.rs", "crate::m0")
        head = good.replace(row + "\n", "")
        cases = {"missing": None, "no final newline": good[:-1], "other root": good.replace("src/lib.rs\t", "src/main.rs\t", 1),
                 "short row": head + row.rsplit("\t", 1)[0] + "\n", "extra cell": head + row + "\t-\n",
                 "bad ctx": head + row.replace("#0", "#x", 1) + "\n", "dangling attr": head + row.replace("\t-\tfile", "\tdoc#0,\tfile\n"),
                 "unescaped quote": head + row.replace("\t-\tfile", '\tpath#0["a"b"]\tfile\n'),
                 "unknown kind": head + row.replace("\tfile", "\tmodule\n")}
        for case, text in cases.items():
            with self.subTest(case=case):
                self.modmap.unlink(missing_ok=True)
                if text is not None:
                    self.modmap.write_text(text, encoding="utf-8")
                problems = h2_depinfo.ro_problems(self.root, [artifact(self.root, "c0ffee")], self.modmap)
                self.assertEqual(len(problems), 1, problems)
                self.assertRegex(problems[0], r"^R-O: (cannot read module map|module map .* (lacks|is malformed))")
        # a broken .d does not hide a broken map, nor the other way round
        (self.root / "target/debug/deps/agentdesk-c0ffee.d").write_text("", encoding="utf-8")
        self.assertEqual(len(h2_depinfo.ro_problems(self.root, [artifact(self.root, "c0ffee")], self.modmap)), 2)

    def test_unreadable_walker_source_is_a_problem(self) -> None:
        # the text walker reads every module file too; a failure is one problem, never an empty table
        (self.root / "src/a.rs").write_bytes(b"\xff")
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertRegex(problems[0], r"^R-O: cannot read text walker module table: ")

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

# Driver rows of the H2 R-O counterexample crates (owner src/owner.rs, its clean row left out; `/abs/` the crate dir).
# Verdicts: m macro-made, n inside an item, a macro-made attribute, o owner `#[path]`, i an unmoduled .rs input, w walker.
PROBE_ROWS = """\
base\tsrc/shared.rs\tcrate::shared\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:2:1: 2:12 (#0)\t-\tfile
base\tsrc/plat.rs\tcrate::plat\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:3:14: 3:23 (#0)\t<cfg_trace>#0\tfile
main\tsrc/plat.rs\tcrate::plat\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:3:14: 3:23 (#0)\t<cfg_trace>#0\tfile
main\tsrc/shared.rs\tcrate::{fn probe}::injected\t#4\t#4\tfn:probe\tsrc/lib.rs\tsrc/lib.rs:1:53: 1:66 (#4)\tpath#4["shared.rs"]\tfile
v3\tsrc/shared.rs\tcrate::owner::{fn probe}::injected\t#4\t#4\tfn:probe\tsrc/owner.rs\tsrc/lib.rs:1:53: 1:66 (#4)\tpath#4["shared.rs"]\tfile
v3_ext\tsrc/shared.rs\tcrate::owner::{fn probe}::injected\t#5\t#5\tfn:probe\tsrc/owner.rs\t/abs/src/lib.rs:2:53: 2:66 (#5)\tpath#5["/abs/src/shared.rs"]\tfile
f1_rawcfg\tsrc/shared.rs\tcrate::owner::{fn probe}::injected\t#5\t#5\tfn:probe\tsrc/owner.rs\t/abs/src/lib.rs:2:53: 2:66 (#5)\tpath#5["/abs/src/shared.rs"]\tfile
f1_cfgattr_raw\tsrc/shared.rs\tcrate::owner::{fn probe}::injected\t#5\t#5\tfn:probe\tsrc/owner.rs\t/abs/src/lib.rs:2:53: 2:66 (#5)\tpath#5["/abs/src/shared.rs"]\tfile
f1_lrm\tsrc/shared.rs\tcrate::owner::{fn probe}::injected\t#5\t#5\tfn:probe\tsrc/owner.rs\t/abs/src/lib.rs:2:53: 2:66 (#5)\tpath#5["/abs/src/shared.rs"]\tfile
f2_tt\tsrc/shared.rs\tcrate::owner::{fn probe}::injected\t#5\t#5\tfn:probe\tsrc/owner.rs\t/abs/src/lib.rs:2:53: 2:66 (#5)\tpath#5["/abs/src/shared.rs"]\tfile
f3_constgen\tsrc/shared.rs\tcrate::owner::{fn probe}::injected\t#5\t#5\tfn:probe\tsrc/owner.rs\t/abs/src/lib.rs:2:53: 2:66 (#5)\tpath#5["/abs/src/shared.rs"]\tfile
p1_1\tsrc/shared.rs\tcrate::owner::{fn existing_owner_operation}::injected\t#5\t#5\tfn:existing_owner_operation\tsrc/owner.rs\t/abs/src/lib.rs:2:53: 2:66 (#5)\tpath#5["/abs/src/shared.rs"]\tfile
p1_2\tsrc/shared.rs\tcrate::owner::{fn existing_owner_operation}::injected\t#5\t#5\tfn:existing_owner_operation\tsrc/owner.rs\t/abs/src/lib.rs:2:53: 2:66 (#5)\tpath#5["/abs/src/shared.rs"]\tfile
x1_ident\tsrc/shared.rs\tcrate::owner::injected\t#4\t#0\tmodule\tsrc/owner.rs\tsrc/lib.rs:1:146: 1:153 (#4)\tpath#4["/abs/src/shared.rs"]\tfile
x1_pass_fn\tsrc/shared.rs\tcrate::owner::{fn probe}::injected\t#0\t#0\tfn:probe\tsrc/owner.rs\tsrc/owner.rs:1:148: 1:161 (#0)\tpath#0["/abs/src/shared.rs"]\tfile
x1_addpath\tsrc/shared.rs\tcrate::owner::injected\t#0\t#0\tmodule\tsrc/owner.rs\tsrc/owner.rs:1:12: 1:25 (#0)\tpath#4["/abs/src/shared.rs"]\tfile
x1_pass_mod\tsrc/shared.rs\tcrate::owner::injected\t#0\t#0\tmodule\tsrc/owner.rs\tsrc/owner.rs:1:132: 1:145 (#0)\tpath#0["/abs/src/shared.rs"]\tfile
x2_include\tsrc/shared.rs\tcrate::shared\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:2:1: 2:12 (#0)\t-\tfile
inline_in_fn\tsrc/inline/x.rs\tcrate::{fn probe}::inline::x\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:4:9: 4:15 (#0)\tpath#0["x.rs"]\tfile
"""
PROBE_VERDICTS = dict(base="w", main="mnaw", **dict.fromkeys(
    ("v3", "v3_ext", "f1_rawcfg", "f1_cfgattr_raw", "f1_lrm", "f2_tt", "f3_constgen", "p1_1", "p1_2"), "mnao"),
    x1_ident="maow", x1_pass_fn="no", x1_addpath="aow", x1_pass_mod="ow", x2_include="i", inline_in_fn="n")
PROBE_KINDS = {"m": "declared by a macro expansion", "n": "is declared inside", "a": "macro-made attribute",
               "o": "via #[path]", "i": "is compiled into the lib but is not in the module tree", "w": "text walker"}
# h2_measure._module_walk of each case beyond lib.rs and owner.rs; it reads neither `#[cfg(..)] mod x;` on one line
# nor a macro's `mod`, so base's plat.rs and the x1 mounts are missing.
PROBE_WALKER = dict(base={"src/shared.rs": "agentdesk::shared"}, f3_constgen={"src/shared.rs": "agentdesk::shared"},
                    p1_1={"src/shared.rs": "agentdesk::declared"}, p1_2={"src/shared.rs": "agentdesk::decoy::forged"},
                    x2_include={"src/shared.rs": "agentdesk::shared"}, inline_in_fn={"src/inline/x.rs": "agentdesk::inline::x"})

class ProbeFixtures(unittest.TestCase):
    def test_counterexample_crates_keep_their_verdicts(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(h2, "OWNER_FILES", frozenset({"src/owner.rs"})), \
                mock.patch.object(h2, "OWNER_PREFIXES", ()):
            root = Path(tmp)
            for case, kinds in PROBE_VERDICTS.items():
                table = {"src/lib.rs": h2.CRATE, "src/owner.rs": f"{h2.CRATE}::owner", **PROBE_WALKER.get(case, {})}
                walk = table, [root / rel for rel in table]
                with self.subTest(case=case), mock.patch.object(h2, "_module_walk", lambda _root, walk=walk: walk):
                    rows = [line.split("\t", 1)[1] for line in PROBE_ROWS.splitlines() if line.split("\t", 1)[0] == case]
                    extra = ["src/extra_body.rs"] if case == "x2_include" else []
                    write_depinfo(root, "c0ffee", ["src/lib.rs", *(row.split("\t")[0] for row in rows), *extra])
                    problems = h2_depinfo.ro_problems(root, [artifact(root, "c0ffee")], write_modmap(root / "map.tsv", rows))
                    found = [kind for problem in problems for kind, text in PROBE_KINDS.items() if text in problem]
                    self.assertEqual((sorted(found), len(problems)), (sorted(kinds), len(kinds)), problems)

# Crates R-O must judge: (sources, the rows the driver writes, the problems). R-W places sites by the text walker,
# which misreads `rel`, `pathattr` and anything inside a macro-made module; `incmod` and `splice` use include!.
RELOCATIONS = {
    "rel": ({"src/lib.rs": '#[path = "."]\nmod w {\n    pub mod z;\n}\n#[path = "q.rs"] // real file for crate::z\n'
                           "pub mod z;\npub fn g() { w::z::f() }\n", "src/q.rs": "pub fn f() {}\n", "src/z.rs": "pub fn f() {}\n"},
            ['src/lib.rs\tcrate::w\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:2:1: 4:2 (#0)\tpath#0["."]\tinline',
             "src/z.rs\tcrate::w::z\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:3:5: 3:15 (#0)\t-\tfile",
             'src/q.rs\tcrate::z\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:6:1: 6:11 (#0)\tpath#0["q.rs"]\tfile'],
            ["R-O: rustc compiles src/q.rs as agentdesk::z but the text walker reads it as no module",
             "R-O: rustc compiles src/z.rs as agentdesk::w::z but the text walker reads it as agentdesk::z"]),
    "macwrap": ({"src/lib.rs": "macro_rules! wrap {\n    ($i:item) => {\n        pub mod w {\n            $i\n        }\n    };\n}\n"
                               "wrap! {\n    pub mod z;\n}\npub fn g() { w::z::f() }\n", "src/w/z.rs": "pub fn f() {}\n"},
                ["src/lib.rs\tcrate::w\t#4\t#4\tmodule\tsrc/lib.rs\tsrc/lib.rs:3:9: 5:10 (#4)\t-\tinline",
                 "src/lib.rs\tcrate::w\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:9:5: 9:15 (#0)\t-\twrapped",
                 "src/w/z.rs\tcrate::w::z\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:9:5: 9:15 (#0)\t-\tfile"],
                ["R-O: inline module crate::w is declared by a macro expansion",
                 "R-O: macro-made module crate::w wraps hand-written items from src/lib.rs",
                 "R-O: rustc compiles src/w/z.rs as agentdesk::w::z but the text walker reads it as no module"]),
    "incmod": ({"src/lib.rs": 'pub mod a;\npub mod b {\n    include!("a.rs");\n}\n', "src/a.rs": "pub fn f() {}\n"},
               ["src/a.rs\tcrate::a\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:1:1: 1:11 (#0)\t-\tfile",
                "src/lib.rs\tcrate::b\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:2:1: 4:2 (#0)\t-\tinline",
                "src/a.rs\tcrate::b\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/a.rs:1:1: 1:14 (#0)\t-\tinclude"],
               ["R-O: include! splices src/a.rs into crate::b"]),
    "splice": ({"src/lib.rs": 'pub mod plain;\ninclude!("body.rs");\n', "src/body.rs": "pub fn from_body() {}\n",
                "src/plain.rs": "pub fn p() {}\n"},
               ["src/plain.rs\tcrate::plain\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:1:1: 1:15 (#0)\t-\tfile",
                "src/body.rs\tcrate\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/body.rs:1:1: 1:22 (#0)\t-\tinclude"],
               ["R-O: include! splices src/body.rs into crate", "R-O: src/body.rs is compiled into the lib but is not in the module tree"]),
    "itemwrap": ({"src/lib.rs": "pub fn tmux_exec() {}\nmacro_rules! wrap {\n    ($($i:item)*) => {\n        pub mod w {\n"
                                "            $($i)*\n        }\n    };\n}\nwrap! {\n    pub fn f() {\n        crate::tmux_exec()\n"
                                "    }\n}\npub fn g() {\n    w::f()\n}\npub mod plain;\n", "src/plain.rs": ""},
                 ["src/lib.rs\tcrate::w\t#4\t#4\tmodule\tsrc/lib.rs\tsrc/lib.rs:4:9: 6:10 (#4)\t-\tinline",
                  "src/lib.rs\tcrate::w\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:10:5: 12:6 (#0)\t-\twrapped",
                  "src/plain.rs\tcrate::plain\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:17:1: 17:15 (#0)\t-\tfile"],
                 ["R-O: macro-made module crate::w wraps hand-written items from src/lib.rs"]),
    # the tokio::select! shape: a macro-made module of macro-made items only, which the walker needs no path for
    "selectlike": ({"src/lib.rs": "// The shape tokio::select! leaves: a macro-made helper module inside a fn body, holding only "
                                  "macro-made items.\nmacro_rules! select_like {\n    ($e:expr) => {{\n        mod __select_util {\n"
                                  "            pub(super) enum Out<T> {\n                Val(T),\n                Disabled,\n"
                                  "            }\n        }\n        match $e {\n            v => __select_util::Out::Val(v),\n"
                                  "        }\n    }};\n}\npub fn f() -> u32 {\n    match select_like!(1u32) {\n        _ => 0,\n"
                                  "    }\n}\npub mod plain;\n", "src/plain.rs": "pub fn p() {}\n"},
                   ["src/lib.rs\tcrate::{fn f}::__select_util\t#4\t#4\tfn:f\tsrc/lib.rs\tsrc/lib.rs:4:9: 9:10 (#4)\t-\tinline",
                    "src/plain.rs\tcrate::plain\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:20:1: 20:15 (#0)\t-\tfile"],
                   []),
    "pathattr": ({"src/lib.rs": 'macro_rules! at_root {\n    ($i:item) => {\n        #[path = "."]\n        $i\n    };\n}\n'
                                "at_root! {\n    mod w {\n        pub mod z;\n    }\n}\npub fn g() {\n    w::z::f()\n}\n",
                  "src/z.rs": "pub fn f() {}\n"},
                 ['src/lib.rs\tcrate::w\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:8:5: 10:6 (#0)\tpath#4["."]\tinline',
                  "src/lib.rs\tcrate::w\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:9:9: 9:19 (#0)\t-\twrapped",
                  "src/z.rs\tcrate::w::z\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:9:9: 9:19 (#0)\t-\tfile"],
                 ["R-O: inline module crate::w carries a macro-made attribute",
                  "R-O: macro-made module crate::w wraps hand-written items from src/lib.rs",
                  "R-O: rustc compiles src/z.rs as agentdesk::w::z but the text walker reads it as no module"]),
    # a module any expansion defined is macro-made, even from call-site tokens only; the nearest module decides
    "ttbody": ({"src/lib.rs": "pub fn tmux_exec() {}\nmacro_rules! wrap {\n    ($body:tt) => {\n        pub mod w $body\n"
                              "    };\n}\nwrap!({\n    pub fn f() {\n        crate::tmux_exec()\n    }\n});\n"
                              "pub fn g() {\n    w::f()\n}\npub mod plain;\n", "src/plain.rs": ""},
               ["src/lib.rs\tcrate::w\t#4\t#4\tmodule\tsrc/lib.rs\tsrc/lib.rs:4:9: 4:24 (#4)\t-\tinline",
                "src/lib.rs\tcrate::w\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:8:5: 10:6 (#0)\t-\twrapped",
                "src/plain.rs\tcrate::plain\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:15:1: 15:15 (#0)\t-\tfile"],
               ["R-O: macro-made module crate::w wraps hand-written items from src/lib.rs"]),
    "identonly": ({"src/lib.rs": "pub fn tmux_exec() {}\nmacro_rules! wrap {\n    ($kw:tt $body:tt) => {\n"
                                 "        $kw w $body\n    };\n}\nwrap!(mod {\n    pub fn f() {\n"
                                 "        crate::tmux_exec()\n    }\n});\npub fn g() {\n    w::f()\n}\npub mod plain;\n", "src/plain.rs": ""},
                  ["src/lib.rs\tcrate::w\t#0\t#4\tmodule\tsrc/lib.rs\tsrc/lib.rs:7:7: 11:2 (#0)\t-\tinline",
                   "src/lib.rs\tcrate::w\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:8:5: 10:6 (#0)\t-\twrapped",
                   "src/plain.rs\tcrate::plain\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:15:1: 15:15 (#0)\t-\tfile"],
                  ["R-O: macro-made module crate::w wraps hand-written items from src/lib.rs"]),
    "itemonly": ({"src/lib.rs": "pub fn tmux_exec() {}\nmacro_rules! wrap {\n    ($name:ident $body:tt) => {\n"
                                "        pub mod $name $body\n    };\n}\nwrap!(w {\n    pub fn f() {\n"
                                "        crate::tmux_exec()\n    }\n});\npub fn g() {\n    w::f()\n}\npub mod plain;\n", "src/plain.rs": ""},
                 ["src/lib.rs\tcrate::w\t#4\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:4:9: 4:28 (#4)\t-\tinline",
                  "src/lib.rs\tcrate::w\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:8:5: 10:6 (#0)\t-\twrapped",
                  "src/plain.rs\tcrate::plain\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:15:1: 15:15 (#0)\t-\tfile"],
                 ["R-O: macro-made module crate::w wraps hand-written items from src/lib.rs"]),
    "nested2": ({"src/lib.rs": "pub fn tmux_exec() {}\nmacro_rules! inner {\n    ($body:tt) => {\n"
                               "        pub mod w $body\n    };\n}\nmacro_rules! outer {\n    ($body:tt) => {\n"
                               "        inner!($body);\n    };\n}\nouter!({\n    pub fn f() {\n"
                               "        crate::tmux_exec()\n    }\n});\npub fn g() {\n    w::f()\n}\npub mod plain;\n", "src/plain.rs": ""},
                ["src/lib.rs\tcrate::w\t#5\t#5\tmodule\tsrc/lib.rs\tsrc/lib.rs:4:9: 4:24 (#5)\t-\tinline",
                 "src/lib.rs\tcrate::w\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:13:5: 15:6 (#0)\t-\twrapped",
                 "src/plain.rs\tcrate::plain\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:20:1: 20:15 (#0)\t-\tfile"],
                ["R-O: macro-made module crate::w wraps hand-written items from src/lib.rs"]),
    "innerof": ({"src/lib.rs": "pub fn tmux_exec() {}\nmacro_rules! wrap {\n    ($body:tt) => {\n        pub mod o {\n"
                               "            pub mod w $body\n        }\n    };\n}\nwrap!({\n    pub fn f() {\n"
                               "        crate::tmux_exec()\n    }\n});\npub fn g() {\n    o::w::f()\n}\npub mod plain;\n", "src/plain.rs": ""},
                ["src/lib.rs\tcrate::o\t#4\t#4\tmodule\tsrc/lib.rs\tsrc/lib.rs:4:9: 6:10 (#4)\t-\tinline",
                 "src/lib.rs\tcrate::o::w\t#4\t#4\tmodule\tsrc/lib.rs\tsrc/lib.rs:5:13: 5:28 (#4)\t-\tinline",
                 "src/lib.rs\tcrate::o::w\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:10:5: 12:6 (#0)\t-\twrapped",
                 "src/plain.rs\tcrate::plain\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:17:1: 17:15 (#0)\t-\tfile"],
                ["R-O: macro-made module crate::o::w wraps hand-written items from src/lib.rs"]),
    "handmod": ({"src/lib.rs": "pub fn tmux_exec() {}\nmacro_rules! wrap {\n    ($body:tt) => {\n        pub mod w $body\n"
                               "    };\n}\nwrap!({\n    pub mod inner {\n        pub fn f() {\n"
                               "            crate::tmux_exec()\n        }\n    }\n});\npub fn g() {\n    w::inner::f()\n"
                               "}\npub mod plain;\n", "src/plain.rs": ""},
                ["src/lib.rs\tcrate::w\t#4\t#4\tmodule\tsrc/lib.rs\tsrc/lib.rs:4:9: 4:24 (#4)\t-\tinline",
                 "src/lib.rs\tcrate::w\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:8:5: 12:6 (#0)\t-\twrapped",
                 "src/lib.rs\tcrate::w::inner\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:8:5: 12:6 (#0)\t-\tinline",
                 "src/lib.rs\tcrate::w::inner\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:9:9: 11:10 (#0)\t-\twrapped",
                 "src/plain.rs\tcrate::plain\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:17:1: 17:15 (#0)\t-\tfile"],
                ["R-O: macro-made module crate::w wraps hand-written items from src/lib.rs",
                 "R-O: macro-made module crate::w::inner wraps hand-written items from src/lib.rs"]),
}

class Relocations(unittest.TestCase):
    def test_relocated_file_modules_are_caught(self) -> None:
        for case, (sources, rows, expected) in RELOCATIONS.items():
            with self.subTest(case=case), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                for rel, text in sources.items():
                    (root / rel).parent.mkdir(parents=True, exist_ok=True)
                    (root / rel).write_text(text, encoding="utf-8")
                h2._MODULE_TABLES.clear()
                write_depinfo(root, "c0ffee", sorted(sources))
                problems = h2_depinfo.ro_problems(root, [artifact(root, "c0ffee")], write_modmap(root / "map.tsv", rows))
                self.assertEqual(problems, expected)

    def test_an_owner_steers_no_file_module_by_path(self) -> None:
        # `#[path]` on an inline parent moves its file modules as surely as on the file module itself
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(h2, "OWNER_FILES", frozenset({"src/lib.rs"})):
            rows = h2_depinfo.load_modmap(write_modmap(Path(tmp) / "map.tsv", RELOCATIONS["rel"][1]))
            self.assertEqual(h2_depinfo.modmap_problems(rows), ["R-O: owner file src/lib.rs mounts inline module crate::w via #[path]",
                                                                "R-O: owner file src/lib.rs mounts src/q.rs via #[path]"])

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

class OwnerDocAttributes(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.owner = self.root / OWNER
        self.owner.parent.mkdir(parents=True)

    def test_doc_values_preserve_plain_owner_inventory(self) -> None:
        attrs = (
            '#[doc = "plain"]',
            '#[doc = concat!("a", "b")]',
            '#[doc = include_str!("note.md")]',
            '#[doc = concat!(r#"]/*"#,\n include_str!("note.md"))]',
            '#[cfg_attr(unix, doc = concat!("a", "b"))]',
            '#![doc = include_str!("note.md")]',
            r'#[doc = concat!("]\"// /*", r#"]"/* //"#, include_str!("note.md"))]',
            '#[doc /* ] */ = gen!([[(\']\')]], { /* [ /* ) */ ] */ })]',
            '#[cfg_attr(all(unix, not(test)), doc = gen!(), allow(unused), doc = other!())]',
            '#[cfg_attr(unix, cfg_attr(any(unix, windows), doc = gen!()),)]',
            '#[doc = some::r#gen!()]\n#[allow(unused)]',
            '#[' + 'cfg_attr(unix, ' * 24 + 'doc = gen!()' + ')' * 24 + ']',
        )
        for attr in attrs:
            for prefix, suffix, item in (("", "", "sample"), ("struct Api;\nimpl Api {\n", "}\n", "Api::sample")):
                with self.subTest(attr=attr, item=item):
                    self.owner.write_text(prefix + attr + '\npub fn sample() { body!(); }\n' + suffix, encoding="utf-8")
                    self.assertEqual(adm.owner_shape_problems(self.root), [])
                    self.assertEqual(adm.owner_pub_fns(self.root, OWNER, "crate::tmux"), {f"crate::tmux::{item}"})

    def test_only_supported_balanced_doc_values_exempt_macros(self) -> None:
        attrs = (
            '#[my_attr(doc = gen!())]',
            '#[my_attr(nested(doc = gen!()))]',
            '#[my_attr(#[doc = gen!()])]',
            '#[my_attr(\n#[doc = gen!()]',
            '#[cfg_attr(unix, my_attr(doc = gen!()))]',
            '#[cfg_attr(unix, my_attr(cfg_attr(unix, doc = gen!())))]',
            '#[cfg_attr(doc = gen!(), doc = accepted!())]',
            '#[cfg_attr(any(unix, doc = gen!()), doc = accepted!())]',
            '#[cfg_attr(unix, cfg_attr(doc = gen!(), doc = accepted!()))]',
            '#[cfg_attr(unix, other = gen!())]',
            '#[cfg_attr(unix, doc = gen!()) trailing]',
            '#[r#doc = gen!()]',
            '#[r#cfg_attr(unix, doc = gen!())]',
            '#[other::doc = gen!()]',
            '#[doc = "ok", gen!()]',
            '#[doc = gen!()',
            '#[doc = gen!())]',
            '#[doc = gen!([)]]',
            '#[cfg_attr(unix, doc = gen!()]',
        )
        for attr in attrs:
            with self.subTest(attr=attr):
                self.owner.write_text(attr + '\npub fn sample() {}\n', encoding="utf-8")
                self.assertEqual(adm.owner_shape_problems(self.root), [
                    f"R-E: {OWNER} uses item-level macro `gen!`; owner files must be plain items"])

    def test_doc_values_do_not_hide_item_macros_or_path(self) -> None:
        doc = '#[cfg_attr(x, doc = include_str!("note.md"))]\n'
        cases = (
            (doc + 'external!();', ('`external!`',), False),
            (doc + 'concat!();', ('`concat!`',), False),
            (doc + 'some::r#external!();', ('`external!`',), False),
            ('#[cfg_attr(x, doc = include_str!("note.md"), external!())]', ('`external!`',), False),
            ('#![doc = gen!(r#"]" // /*"#)]\nexternal!();', ('`external!`',), False),
            ('#[doc = gen!([[1], [2]])]\nexternal!();', ('`external!`',), False),
            ('#[doc = { macro_rules! hidden { () => { "x" } } hidden!() }]', ('`macro_rules!`',), False),
            (doc + 'macro_rules! hidden { () => {} }', ('`macro_rules!`',), False),
            ('#[cfg_attr(x, doc = include_str!("note.md"), path = "outside.rs")]', (), True),
            ('#[cfg_attr(x, doc = gen!(r#"]"#), cfg_attr(y, path = "outside.rs"))]', (), True),
            (doc + '#[path = "outside.rs"]\nmod child;', (), True),
            ('#[doc = gen!(path = "outside.rs")]', (), True),
            (doc + 'trait Api { fn default_method() {} }', ('trait default method Api::default_method',), False),
        )
        with mock.patch.object(adm, "OWNER_ROSTER", frozenset({OWNER})), mock.patch.object(adm, "R_C_GRANDFATHERED", {}):
            for source, needles, path in cases:
                with self.subTest(source=source):
                    self.owner.write_text(source + '\npub fn sample() {}\n', encoding="utf-8")
                    problems = adm.owner_shape_problems(self.root)
                    self.assertEqual(len(problems), len(needles), problems)
                    for problem, needle in zip(problems, needles):
                        self.assertIn(needle, problem)
                    self.assertEqual(adm.zero_rules(self.root),
                                     [f"R-O: {OWNER} uses #[path]; owner modules must live under the owner paths"] if path else [])

ESCAPE = {"src/services/platform/tmux.rs": '#[path = "pty_escape.rs"]\npub(crate) mod escape;\npub fn has_session() {}\n'}

if __name__ == "__main__":
    unittest.main()
