"""Unit tests for the H2 tmux-boundary measurer (fixtures only, no cargo)."""

from __future__ import annotations

import io
import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT / "scripts/ci"))
import h2_measure as h2  # noqa: E402

TMUX = "agentdesk::services::platform::tmux::has_session"
CMD = "std::process::Command::new"
WRAP = "agentdesk::services::probe::alive"
TYPE = "agentdesk::services::relay::TmuxBackend"
CLIPPY_TOML = f"""
disallowed-methods = [
  {{ path = "{TMUX}", reason = "H2 EXEC both" }},
  {{ path = "{WRAP}", reason = "H2 W linux" }},
  {{ path = "{CMD}", reason = "H2 SUBPROC both" }},
  {{ path = "some::other::thing", reason = "unrelated" }},
]
disallowed-types = [
  {{ path = "{TYPE}", reason = "H2 TYPES both" }},
]
"""
SOURCES = {
    "src/lib.rs": "pub mod services;\n",
    "src/services/mod.rs": "pub mod platform;\npub mod probe;\n#[path = \"relay_impl.rs\"]\nmod relay;\npub mod launch;\n",
    "src/services/platform/mod.rs": "pub mod tmux;\n",
    "src/services/platform/tmux.rs": "pub fn has_session(_: &str) -> bool { helper() }\n",
    "src/services/probe.rs": textwrap.dedent("""\
        pub fn alive(name: &str) -> bool {
            crate::services::platform::tmux::has_session(name)
        }
        mod inner {
            pub(super) fn twice(name: &str) -> bool {
                fn nested(n: &str) -> bool { crate::services::platform::tmux::has_session(n) }
                nested(name) && super::alive(name)
            }
        }
        static LABEL: &str = { "x" };
        pub struct Probe;
        impl Probe {
            pub fn check(&self) -> bool { let f = |n: &str| super::probe::alive(n); f("a") }
        }
        """),
    "src/services/relay_impl.rs": textwrap.dedent("""\
        pub trait Backend { fn send(&self); }
        pub struct TmuxBackend;
        impl Backend for TmuxBackend {
            fn send(&self) { let _ = crate::services::platform::tmux::has_session("s"); }
        }
        pub fn build() -> Box<dyn Backend> { Box::new(TmuxBackend) }
        """),
    "src/services/launch.rs": textwrap.dedent("""\
        use std::process::Command;
        pub fn dynamic(bin: &str) { Command::new(bin); }
        pub fn shell(script: &str) { Command::new("/bin/bash").args(["-c", script]); }
        pub fn literal_shell() { Command::new("bash").args(["-c", "./x.sh"]).arg(3); }
        pub fn plain(repo: &str) { Command::new("git").arg(repo); }
        pub fn ssh(host: &str) {
            let mut c = Command::new("ssh");
            c.arg("-o").arg(format!("Host={host}"));
        }
        pub fn pointer() -> Vec<Command> { vec!["a"].into_iter().map(Command::new).collect() }
        """),
}

def locate(text: str, needle: str, nth: int = 0) -> tuple[int, int]:
    index = -1
    for _ in range(nth + 1):
        index = text.index(needle, index + 1)
    line = text.count("\n", 0, index) + 1
    return line, index - (text.rfind("\n", 0, index) + 1) + 1

def diag(file, line, col, callee, *, lint="clippy::disallowed_methods", kind="lib", expansion=None) -> str:
    span = {"file_name": file, "line_start": line, "column_start": col, "is_primary": True,
            **({"expansion": {"span": expansion}} if expansion else {})}
    return json.dumps({"reason": "compiler-message", "target": {"kind": [kind]}, "message": {
        "code": {"code": lint}, "message": f"use of a disallowed method `{callee}`", "spans": [span]}})

class Fixture(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        for rel, text in SOURCES.items():
            (self.root / rel).parent.mkdir(parents=True, exist_ok=True)
            (self.root / rel).write_text(text, encoding="utf-8")
        (self.root / "clippy.toml").write_text(CLIPPY_TOML, encoding="utf-8")
        (self.root / "scripts/ci").mkdir(parents=True)
        self.config = h2.load_config(self.root / "clippy.toml")
        h2._MODULE_TABLES.clear()

    def at(self, rel: str, needle: str, callee: str, nth: int = 0, **kw) -> str:
        return diag(rel, *locate(SOURCES[rel], needle, nth), callee, **kw)

    def lines(self) -> list[str]:
        probe, relay, launch = "src/services/probe.rs", "src/services/relay_impl.rs", "src/services/launch.rs"
        return [
            "not json", '{"reason": "build-finished"}',
            self.at(probe, "crate::services::platform::tmux::has_session(name)", TMUX),
            self.at(probe, "crate::services::platform::tmux::has_session(n)", TMUX),
            self.at(probe, "super::alive", WRAP),
            self.at(probe, "super::probe::alive", WRAP),
            self.at(relay, "crate::services::platform::tmux", TMUX),
            self.at(relay, "TmuxBackend)", TYPE, lint="clippy::disallowed_types"),
            self.at(relay, "TmuxBackend;", TYPE, lint="clippy::disallowed_types"),
            diag("src/services/platform/tmux.rs", 1, 1, TMUX),  # owner: liveness only
            self.at(probe, "nested(name)", "some::other::thing"),
            self.at(probe, "super::alive", WRAP, kind="test"), diag("/rustc/std/src/macros.rs", 3, 1, WRAP),
            *(self.at(launch, "Command::new", CMD, nth=i) for i in range(6)),
            # A macro expansion is attributed to its outermost call site, once.
            diag("/rustc/x.rs", 9, 9, TMUX, expansion={"file_name": "src/m.rs", "expansion": {"span": {
                "file_name": "src/services/probe.rs", "line_start": 2, "column_start": 5}}}),
        ]

class DiagnosticsAndItems(Fixture):
    def test_config_and_diagnostics(self) -> None:
        self.assertEqual(self.config[WRAP], ("W", frozenset({"linux"})))
        self.assertEqual(self.config[TYPE], ("TYPES", frozenset(h2.LANES)))
        self.assertNotIn("some::other::thing", self.config)
        # r6 §2.5: a duplicated path is rejected, not silently overridden by the later entry
        (self.root / "clippy.toml").write_text(CLIPPY_TOML.replace("]\ndisallowed-types",
                                               f'  {{ path = "{TMUX}", reason = "H2 W linux" }},\n]\ndisallowed-types'))
        with self.assertRaisesRegex(h2.MeasureError, "duplicate H2 path"):
            h2.load_config(self.root / "clippy.toml")
        # a malformed H2 tag (here on a duplicate path) is an error, not a silently skipped entry
        (self.root / "clippy.toml").write_text(CLIPPY_TOML.replace("]\ndisallowed-types",
                                               f'  {{ path = "{TMUX}", reason = "H2 EXEC" }},\n]\ndisallowed-types'))
        with self.assertRaisesRegex(h2.MeasureError, "bad H2 entry"):
            h2.load_config(self.root / "clippy.toml")
        rows = h2.diagnostics(self.lines())
        self.assertEqual(len(rows), len(set(rows)))
        self.assertTrue(all(file.startswith("src/") for file, *_ in rows))
        # the macro call site collides with the direct call at probe.rs:2:5
        self.assertEqual(sum(1 for r in rows if r[:3] == ("src/services/probe.rs", 2, 5)), 1)
        with self.assertRaises(h2.MeasureError):
            h2.diagnostics([json.dumps({"reason": "compiler-message", "target": {"kind": ["lib"]},
                                        "message": {"code": {"code": h2.LINTS[0]}, "message": "?", "spans": []}})])

    def test_enclosing_items(self) -> None:
        src = h2.SourceFile(SOURCES["src/services/probe.rs"])
        def item(needle: str, nth: int = 0):
            text = SOURCES["src/services/probe.rs"]
            return src.enclosing(src.offset(*locate(text, needle, nth)))[:2]
        self.assertEqual(item("crate::services"), ("alive", ("alive",)))
        self.assertEqual(item("has_session(n)"), ("inner::twice::nested", ("inner", "twice")))
        self.assertEqual(item("super::alive"), ("inner::twice", ("inner", "twice")))
        self.assertEqual(item("super::probe::alive"), ("Probe::check", ("Probe", "check")))
        self.assertEqual(item('"x"')[0], "static LABEL")
        self.assertEqual(item("pub struct")[0], "<module>")
        relay = h2.SourceFile(SOURCES["src/services/relay_impl.rs"])
        pos = relay.offset(*locate(SOURCES["src/services/relay_impl.rs"], "let _"))
        self.assertEqual(relay.enclosing(pos)[:2], ("<TmuxBackend as Backend>::send", ()))

    def test_module_paths_and_dispatchers(self) -> None:
        table = h2._module_table(self.root)
        self.assertEqual(table["src/services/relay_impl.rs"], "agentdesk::services::relay")
        self.assertEqual(table["src/services/platform/tmux.rs"], "agentdesk::services::platform::tmux")
        # The design's R-D list names 45 programs (its summary says 44); all are kept.
        self.assertEqual((len(h2.DISPATCHERS), len(set(h2.DISPATCHERS))), (45, 45))
        self.assertLessEqual({"bash", "cmd.exe", "env", "ssh", "launchctl", "perl"}, set(h2.DISPATCHERS))

class Measurement(Fixture):
    def test_rows_sets_and_derivation(self) -> None:
        result = h2.measure(self.root, self.lines(), self.config)
        rows = result["rows"]
        probe = "src/services/probe.rs"
        self.assertEqual(rows["exec"][(probe, "alive", TMUX)], 1)
        self.assertEqual(rows["exec"][(probe, "inner::twice::nested", TMUX)], 1)
        self.assertEqual(rows["w"][(probe, "Probe::check", WRAP)], 1)
        self.assertEqual(sum(rows["types"].values()), 2)
        self.assertFalse(any(k[0].endswith("platform/tmux.rs") for s in rows.values() for k in s))
        self.assertEqual(result["total"], 14)  # owner site counted, unrelated/test/std excluded
        self.assertEqual(result["derived"]["W"], {
            "agentdesk::services::probe::alive", "agentdesk::services::probe::inner::twice",
            "agentdesk::services::probe::Probe::check", "agentdesk::services::relay::build"})
        # R-D and the non-literal-program rule; literal-only dispatchers and plain programs stay out
        self.assertEqual(result["derived"]["SUBPROC_W"], {
            f"agentdesk::services::launch::{name}" for name in ("dynamic", "shell", "ssh", "pointer")})
        self.assertEqual(result["derived"]["unregistrable"], set())

    def test_unregistrable_trait_impl_is_reported(self) -> None:
        config = {k: v for k, v in self.config.items() if v[0] != "TYPES"}
        result = h2.measure(self.root, self.lines(), config)
        self.assertEqual(result["derived"]["unregistrable"],
                         {"src/services/relay_impl.rs::<TmuxBackend as Backend>::send"})

class Baseline(Fixture):
    def run_main(self, *args: str) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = h2.main(["--repo", str(self.root), *args])
        return code, out.getvalue(), err.getvalue()

    def write_json(self) -> str:
        path = self.root / "clippy.json"
        path.write_text("\n".join(self.lines()), encoding="utf-8")
        return str(path)

    def seed_baseline(self) -> None:
        rows = h2.measure(self.root, self.lines(), self.config)["rows"]
        h2.write_baseline(self.root, {s: {k: {"linux": 7, "macos": v} for k, v in r.items()} for s, r in rows.items()})

    def test_missing_baseline_is_inert_or_red(self) -> None:
        code, out, _ = self.run_main("--lane", "linux", "--check", "--inert")
        self.assertEqual((code, out), (0, "h2: no baseline committed; inert no-op\n"))
        self.assertEqual(self.run_main("--lane", "linux", "--check")[0], 2)

    def test_check_compares_only_its_lane(self) -> None:
        self.seed_baseline()
        self.assertEqual(self.run_main("--lane", "macos", "--check", "--json", self.write_json())[0], 0)
        code, _, err = self.run_main("--lane", "linux", "--check", "--json", self.write_json())
        self.assertEqual(code, 1)
        self.assertIn("measured 1, baseline 7", err)
        code, _, err = self.run_main("--lane", "linux", "--check", "--inert", "--json", self.write_json())
        self.assertEqual(code, 0)
        self.assertIn("::warning::h2:", err)
        path = self.root / h2.BASELINE_FILES[0]
        row = next(line for line in path.read_text().splitlines() if line.startswith("  {"))
        path.write_text(path.read_text().replace(row, row + "\n" + row, 1))
        with self.assertRaises(h2.MeasureError):  # a duplicated row is rejected
            h2.load_baseline(self.root)

    def test_regen_reaches_fixpoint_and_keeps_other_lane(self) -> None:
        self.seed_baseline()
        passes: list[Path] = []
        h2.regen(self.root, "macos", runner=lambda root, conf: passes.append(conf) or self.lines())
        config = h2.load_config(self.root / "clippy.toml")
        self.assertEqual(config[WRAP], ("W", frozenset(h2.LANES)))  # linux kept, macos derived
        self.assertEqual(config["agentdesk::services::probe::Probe::check"], ("W", frozenset({"macos"})))
        self.assertEqual(config["agentdesk::services::launch::ssh"], ("SUBPROC_W", frozenset({"macos"})))
        self.assertEqual(len(passes), 2)  # second pass sees no change
        exec_rows = h2.load_baseline(self.root)["exec"]
        self.assertEqual(exec_rows[("src/services/probe.rs", "alive", TMUX)], {"linux": 7, "macos": 1})

class ShellEntrypoint(unittest.TestCase):
    def run_sh(self, *args: str, host: str) -> subprocess.CompletedProcess:
        with tempfile.TemporaryDirectory() as tmp:
            tree = Path(tmp)
            (tree / "scripts/ci").mkdir(parents=True)
            (tree / "scripts/ci/h2_measure.sh").write_text((REPO_ROOT / "scripts/ci/h2_measure.sh").read_text())
            (bin_dir := tree / "bin").mkdir()
            for tool, body in (("rustc", f"echo 'host: {host}'"), ("rustup", "echo clippy-aarch64"),
                               ("python3", 'echo "py $*"')):
                (bin_dir / tool).write_text(f"#!/bin/sh\n{body}\n")
                (bin_dir / tool).chmod(0o755)
            if "--with-baseline" in args:
                (tree / h2.BASELINE_FILES[1]).write_text("")
                args = tuple(a for a in args if a != "--with-baseline")
            env = dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}", RUSTFLAGS="-Cx", PYTHON="python3")
            return subprocess.run(["bash", str(tree / "scripts/ci/h2_measure.sh"), *args],
                                  env=env, capture_output=True, text=True)

    def test_inert_no_op_host_triple_and_hand_off(self) -> None:
        # Without a baseline the inert run exits before touching the toolchain.
        result = self.run_sh("--lane", "macos", "--inert", host="x86_64-unknown-linux-gnu")
        self.assertEqual((result.returncode, "inert no-op" in result.stdout), (0, True))
        result = self.run_sh("--lane", "macos", "--with-baseline", host="x86_64-apple-darwin")
        self.assertEqual(result.returncode, 3)
        self.assertIn("H2 measurement requires arm64 macOS host", result.stderr)
        self.assertEqual(self.run_sh("--lane", "linux", "--with-baseline", host="aarch64-apple-darwin").returncode, 3)
        self.assertEqual(self.run_sh("--lane", "bsd", host="aarch64-apple-darwin").returncode, 2)
        result = self.run_sh("--lane", "macos", "--inert", "--with-baseline", host="aarch64-apple-darwin")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "py scripts/ci/h2_measure.py --check --lane macos --inert")

if __name__ == "__main__":
    unittest.main()
