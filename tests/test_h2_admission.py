"""Unit tests for the H2 admission gate (fixture trees and a throwaway git repo, no cargo)."""

from __future__ import annotations

import io
import json
import subprocess
import sys
import tempfile
import textwrap
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT / "scripts/ci"))
import h2_admission as adm  # noqa: E402
import h2_measure as h2  # noqa: E402

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
NEEDLES = [
    (OWNER, "pub fn has_session", TMUX, None),
    (PROBE, "crate::services::platform::tmux::has_session(name)", TMUX, None),
    (PROBE, 'alive("x")', "agentdesk::services::probe::alive", None),
    (PROBE, 'Command::new("git")', CMD, None),
    (PROBE, 'Command::new("gh")', CMD, None),
    (PROBE, 'has_session("y")', TMUX, None),
    (PROBE, 'has_session("z")', TMUX, None),
    (RELAY, 'has_session("s")', TMUX, None),
    (RELAY, "TmuxBackend)", TYPE, "clippy::disallowed_types"),
]

def diag_lines(sources: dict[str, str]) -> list[str]:
    out = []
    for file, needle, callee, lint in NEEDLES:
        text = sources[file]
        if needle not in text:
            continue
        index = text.index(needle)
        span = {"file_name": file, "line_start": text.count("\n", 0, index) + 1,
                "column_start": index - (text.rfind("\n", 0, index) + 1) + 1, "is_primary": True}
        out.append(json.dumps({"reason": "compiler-message", "target": {"kind": ["lib"]}, "message": {
            "code": {"code": lint or "clippy::disallowed_methods"},
            "message": f"use of a disallowed method `{callee}`", "spans": [span]}}))
    return out

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
        self.sources = dict(SOURCES)
        self.write(self.sources)
        (self.root / "scripts/ci").mkdir(parents=True)
        (self.root / "clippy.toml").write_text(CLIPPY_TOML, encoding="utf-8")
        self.regen_baseline()
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
        rows = self.measure()["rows"]
        h2.write_baseline(self.root, {s: {k: dict.fromkeys(h2.LANES, v) for k, v in r.items()} for s, r in rows.items()})

    def edit(self, rel: str, old: str, new: str) -> None:
        self.sources[rel] = self.sources[rel].replace(old, new, 1)
        self.write({rel: self.sources[rel]})

    def admit(self, *rows: str) -> None:
        (self.root / adm.ADMISSIONS_FILE).write_text("".join(textwrap.dedent(r) for r in rows), encoding="utf-8")

    def evaluate(self, lane: str = "linux") -> list[str]:
        return adm.evaluate(self.root, lane, self.base, diag_lines(self.sources))

    def run_main(self, *args: str) -> tuple[int, str, str]:
        (json_path := self.root.parent / f"{self.root.name}.json").write_text("\n".join(diag_lines(self.sources)))
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
        out = io.StringIO()
        (self.root / "d.json").write_text("\n".join(diag_lines(self.sources)))
        with redirect_stdout(out):
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
        self.assertTrue(any(p.startswith("R-C: src/services/probe.rs pairs `Command` with 1") for p in problems))
        self.edit(PROBE, '.arg("tmux")', '.arg("status")')
        self.admit(row.replace("issue", "lines = [4, 5, 6]\nissue"))
        self.assertEqual(self.evaluate(), [])
        self.admit(GROW.replace("issue", "lines = [3]\nissue"))
        self.assertTrue(any("is unambiguous; drop lines" in p for p in self.evaluate()))

    def test_inventory_and_dead_entries(self) -> None:
        for added, path in (("pub fn kill_server() {}", "kill_server"),
                            ("pub struct RawTmux;\nimpl RawTmux {\n    pub fn run(&self) {}\n    fn private(&self) {}\n}", "RawTmux::run"),
                            ("mod inner {\n    pub(super) fn deep() {}\n}", "inner::deep"),
                            ('pub extern "C" fn ext() {}', "ext")):
            with self.subTest(added=path):
                self.edit(OWNER, "pub(crate) fn read", added + "\npub(crate) fn read")
                self.assertEqual(self.evaluate(), ["R-E: owner pub fn inventory mismatch: "
                                                   f"agentdesk::services::platform::tmux::{path} (unclassified)"])
                self.edit(OWNER, added + "\n", "")
        toml = (self.root / "clippy.toml").read_text()
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
    OLD_TEXT = 'use std::process::Command;\nfn f() { Command::new("git"); log("tmux session died"); }\n'

    def rules(self, files: dict[str, str], roster=frozenset(), pins=None) -> list[str]:
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(adm, "OWNER_ROSTER", roster), \
                mock.patch.object(adm, "R_C_GRANDFATHERED", {self.OLD: 1} if pins is None else pins):
            for rel, text in {self.OLD: self.OLD_TEXT, **files}.items():
                (Path(tmp) / rel).parent.mkdir(parents=True, exist_ok=True)
                (Path(tmp) / rel).write_text(textwrap.dedent(text), encoding="utf-8")
            return sorted(p.split(":")[0] for p in adm.zero_rules(Path(tmp)))

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
        # the same swap inside an already-grandfathered file raises its pinned count
        grown = self.OLD_TEXT.replace('Command::new("git")', 'Command::new("tmux")')
        self.assertEqual(self.rules({self.OLD: grown}), ["R-C"])
        self.assertEqual(self.rules({}, pins={self.OLD: 2}), ["R-C"])  # stale pin must be lowered
        self.assertEqual(self.rules({}, pins={}), ["R-C"])  # an unpinned existing pair is red

    def test_each_other_rule_rejects(self) -> None:
        self.assertEqual(self.rules({
            "src/c2.rs": 'static BIN: &\'static str = "tmux";\n',
            "src/f.rs": "fn f() { unsafe { libc::execvp(p, a) }; }\n",
            "src/f_ext.rs": "fn f(mut c: Command) { let _ = c.exec(); }\n",
            "src/services/session_host/extra.rs": "fn x() {}\n",
            "src/runtime_layout/windows_links.rs": "// spawns TMUX\n",
            "Cargo.lock": 'name = "portable-pty"\nname = "tmux_interface"\n',
        }), ["R-C2", "R-F", "R-F", "R-O", "R-O", "R-O", "R-O"])

if __name__ == "__main__":
    unittest.main()
