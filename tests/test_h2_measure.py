"""Unit tests for the H2 tmux-boundary measurer (fixtures only, no cargo)."""

from __future__ import annotations

import bisect
import hashlib
import io
import json
import os
import re
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
import h2_measure as h2  # noqa: E402
import h2_env  # noqa: E402
import h2_items  # noqa: E402

WRAPPERS = ("RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER",
            "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER")
CLEARED = ("CARGO", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS", "CARGO_BUILD_TARGET",
           "RUSTC", "CARGO_BUILD_RUSTC", "RUSTC_BOOTSTRAP", "CLIPPY_ARGS",
           "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS", "CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER",
           "CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER", "CARGO_PROFILE_DEV_OPT_LEVEL", "CARGO_PROFILE_RELEASE_DEBUG",
           "CARGO_UNSTABLE_BUILD_STD", "CARGO_FEATURE_LIVE", "CARGO_CFG_TARGET_OS", "__CARGO_DEFAULT_LIB_METADATA")
POISON = dict.fromkeys((*WRAPPERS, *CLEARED), "inherited-value")

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

ITEM_RE = re.compile(r"\b(fn|const|static|struct|impl|trait)\b\s*(\w*)")
HEADER = {"struct": "Struct", "impl": "Impl { of_trait: true }", "trait": "Trait"}

def item_rows(root: Path, rel: str, text: str, paths: dict, defs) -> list[dict]:
    """Compiler-shaped records for fixture text; a fn's path (or `!reason`) is named in `paths`, never derived."""
    rows = []
    for match in ITEM_RE.finditer(text):
        keyword, name = match.groups()
        end, depth = match.end(), 0
        while end < len(text) and not (depth == 0 and text[end] == ";") and not (depth == 1 and text[end] == "}"):
            depth += (text[end] == "{") - (text[end] == "}")
            end += 1
        if keyword in ("const", "static") and text[end] == "}":
            end = text.index(";", end)
        path = paths.get((rel, name), "!unlisted") if keyword == "fn" else "!" + (
            "const-item" if keyword in ("const", "static") else "module-level")
        kind = "fn" if keyword == "fn" else "const" if keyword in ("const", "static") else "header"
        rows.append(dict(file=rel, lo=match.start(), hi=end + 1, kind=kind, display=name or keyword,
                         path=None if path.startswith("!") else path, unregistrable=path[1:] if path.startswith("!") else None,
                         line=text.count("\n", 0, match.start()) + 1, def_=next(defs), parent=0,
                         def_kind=HEADER.get(keyword, "Fn" if kind == "fn" else "Const"), macro=False))
        rows[-1]["def"] = rows[-1].pop("def_")
    return rows

def fill_bytes(span, sources: dict) -> None:
    while isinstance(span, dict):
        if span.get("file_name") in sources and "byte_start" not in span:
            text = sources[span["file_name"]]
            start = sum(len(line) + 1 for line in text.split("\n")[:span["line_start"] - 1]) + span["column_start"] - 1
            span.update(byte_start=start, byte_end=start + 1)
        span = (span.get("expansion") or {}).get("span")

def session_items(root: Path, sources: dict, lines, paths: dict, *, conf: Path, run: Path, lane: str) -> h2_items.Items:
    """The Items a sealed session of `lines` would load: lib messages, fixture records, run/lane/config binding."""
    events, own = [], []
    for line in lines:
        event = json.loads(line) if line.startswith("{") else None
        for span in ((event or {}).get("message") or {}).get("spans", []):
            fill_bytes(span, sources)
        if event is not None and event.get("reason") == "compiler-message" and "lib" in event["target"]["kind"]:
            own.append(event["message"])
        events.append(json.dumps(event) if event is not None else line)
    defs = iter(range(1, 10_000))
    rows = {str(root / rel): item_rows(root, rel, text, paths, defs) for rel, text in sources.items()}
    raw = {str(root / rel): text.encode() for rel, text in sources.items()}
    conf_text = (conf / "clippy.toml").read_bytes() if (conf / "clippy.toml").exists() else b""
    manifest = dict(run_dir=str(run), lane=lane, request=dict(repo=str(root), lane=lane, conf_dir=str(conf),
                                                               source=dict(config=hashlib.sha256(conf_text).hexdigest())))
    breaks = {name: [m.start() for m in re.finditer(b"\n", body)] for name, body in raw.items()}
    return h2_items.Items(root, str(root), rows, raw, breaks, own, 0, manifest, list(events))

PATHS = {("src/services/probe.rs", "alive"): "agentdesk::services::probe::alive",
         ("src/services/probe.rs", "twice"): "agentdesk::services::probe::inner::twice",
         ("src/services/probe.rs", "nested"): "agentdesk::services::probe::inner::twice",
         ("src/services/probe.rs", "check"): "agentdesk::services::probe::Probe::check",
         ("src/services/relay_impl.rs", "send"): "agentdesk::services::relay::Backend::send",
         ("src/services/relay_impl.rs", "build"): "agentdesk::services::relay::build",
         ("src/services/platform/tmux.rs", "has_session"): TMUX,
         **{("src/services/launch.rs", n): f"agentdesk::services::launch::{n}"
            for n in ("dynamic", "shell", "literal_shell", "plain", "ssh", "pointer")},
         ("src/lib.rs", "a"): "agentdesk::a", ("src/lib.rs", "b"): "agentdesk::b"}

class Fixture(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.sources, self.paths = dict(SOURCES), dict(PATHS)
        for rel, text in SOURCES.items():
            (self.root / rel).parent.mkdir(parents=True, exist_ok=True)
            (self.root / rel).write_text(text, encoding="utf-8")
        (self.root / "clippy.toml").write_text(CLIPPY_TOML, encoding="utf-8")
        (self.root / "scripts/ci").mkdir(parents=True)
        self.config = h2.load_config(self.root / "clippy.toml")
        h2._MODULE_TABLES.clear()

    def at(self, rel: str, needle: str, callee: str, nth: int = 0, **kw) -> str:
        return diag(rel, *locate(SOURCES[rel], needle, nth), callee, **kw)

    def items(self, lines, conf: Path | None = None, run: Path | None = None, lane: str = "linux") -> h2_items.Items:
        conf, run = conf or self.root / "conf", run or self.root / "run"
        return session_items(self.root, self.sources, lines, self.paths, conf=conf, run=run, lane=lane)

    def session(self, lane: str, lines=None) -> str:
        """A sealed run directory whose load yields this lane's Items; `lines` default to the fixture's."""
        run = self.root / "sealed" / lane
        conf = run.parent / f"{lane}-conf"
        conf.mkdir(parents=True, exist_ok=True)
        h2.write_conf(conf, self.config, lane)
        items = self.items(self.lines() if lines is None else lines, conf, run, lane)
        loader = mock.patch.object(h2_items, "load", side_effect=lambda path, crate: (
            self.assertEqual((path, crate), (run / "manifest.json", self.root)) or items))
        loader.start()
        self.addCleanup(loader.stop)
        return str(run)

    def runner(self, produce, lane: str = "linux"):
        """A session runner whose Clippy output for the written lane configuration is `produce(conf)`."""
        return lambda conf, run: self.items(produce(conf), conf, run, lane)

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
            return src.enclosing(src.offset(*locate(text, needle, nth)))[0]
        self.assertEqual(item("crate::services"), "alive")
        self.assertEqual(item("has_session(n)"), "inner::twice::nested")
        self.assertEqual(item("super::alive"), "inner::twice")
        self.assertEqual(item("super::probe::alive"), "Probe::check")
        self.assertEqual(item('"x"'), "static LABEL")
        self.assertEqual(item("pub struct"), "<module>")
        relay = h2.SourceFile(SOURCES["src/services/relay_impl.rs"])
        pos = relay.offset(*locate(SOURCES["src/services/relay_impl.rs"], "let _"))
        self.assertEqual(relay.enclosing(pos)[0], "<TmuxBackend as Backend>::send")

    def test_module_paths_and_dispatchers(self) -> None:
        table = h2._module_table(self.root)
        self.assertEqual(table["src/services/relay_impl.rs"], "agentdesk::services::relay")
        self.assertEqual(table["src/services/platform/tmux.rs"], "agentdesk::services::platform::tmux")
        # The design's R-D list names 45 programs (its summary says 44); all are kept.
        self.assertEqual((len(h2.DISPATCHERS), len(set(h2.DISPATCHERS))), (45, 45))
        self.assertLessEqual({"bash", "cmd.exe", "env", "ssh", "launchctl", "perl"}, set(h2.DISPATCHERS))

class Measurement(Fixture):
    def test_rows_sets_and_derivation(self) -> None:
        result = h2.measure(self.root, self.items(self.lines()), self.config)
        rows = result["rows"]
        probe = "src/services/probe.rs"
        self.assertEqual(rows["exec"][(probe, "alive", TMUX)], 1)
        self.assertEqual(rows["exec"][(probe, "inner::twice::nested", TMUX)], 1)
        self.assertEqual(rows["w"][(probe, "Probe::check", WRAP)], 1)
        self.assertEqual(sum(rows["types"].values()), 2)
        self.assertFalse(any(k[0].endswith("platform/tmux.rs") for s in rows.values() for k in s))
        self.assertEqual(result["total"], 14)  # owner site counted, unrelated/test/std excluded
        # compiler item paths: a nested fn folds, a trait impl registers its trait method, a type site in a fn its fn
        self.assertEqual(result["derived"]["W"], {
            "agentdesk::services::probe::alive", "agentdesk::services::probe::inner::twice",
            "agentdesk::services::probe::Probe::check", "agentdesk::services::relay::build",
            "agentdesk::services::relay::Backend::send"})
        # R-D and the non-literal-program rule; literal-only dispatchers and plain programs stay out
        self.assertEqual(result["derived"]["SUBPROC_W"], {
            f"agentdesk::services::launch::{name}" for name in ("dynamic", "shell", "ssh", "pointer")})
        self.assertEqual(result["derived"]["unregistrable"], set())
        relay = "src/services/relay_impl.rs"
        self.assertEqual(result["reg"][(relay, "<module>", TYPE)], [h2.MODULE_LEVEL])  # the struct itself
        self.assertEqual(result["reg"][(relay, "build", TYPE)], ["agentdesk::services::relay::build"])
        self.assertEqual(result["reg"][(probe, "inner::twice::nested", TMUX)], ["agentdesk::services::probe::inner::twice"])

    def test_registered_self_type_no_longer_exempts_a_trait_impl(self) -> None:
        self.paths[("src/services/relay_impl.rs", "send")] = "!external:core"
        for config in (self.config, {k: v for k, v in self.config.items() if v[0] != "TYPES"}):
            result = h2.measure(self.root, self.items(self.lines()), config)
            self.assertEqual(result["derived"]["unregistrable"],
                             {"src/services/relay_impl.rs::<TmuxBackend as Backend>::send (external:core)"})

    def test_module_level_method_call_and_mapping_failures_are_not_skipped(self) -> None:
        text = "pub struct S;\nimpl S { }\nuse x;\n"
        self.sources["src/lib.rs"] = text
        (self.root / "src/lib.rs").write_text(text)
        call = diag("src/lib.rs", 2, 6, TMUX)
        result = h2.measure(self.root, self.items([call]), self.config)
        self.assertEqual(result["derived"]["unregistrable"], {"src/lib.rs::<module> (module-call)"})
        self.assertEqual(result["reg"][("src/lib.rs", "<module>", TMUX)], [None])
        with self.assertRaises(h2_items.MappingError) as caught:  # a site outside every item record
            h2.measure(self.root, self.items([diag("src/lib.rs", 3, 1, TMUX)]), self.config)
        self.assertEqual(caught.exception.reason, "no-item")
        items = self.items([call])
        del items.raw[str(self.root / "src/lib.rs")], items.rows[str(self.root / "src/lib.rs")]
        with self.assertRaisesRegex(h2_items.MappingError, "no-item"):  # a file the session did not read
            h2.measure(self.root, items, self.config)

    def test_only_the_proved_lib_messages_are_measured(self) -> None:
        items = self.items(self.lines())
        other = self.items([self.at("src/services/probe.rs", "super::alive", WRAP)])
        items.messages = other.messages  # the lines stay, the attributed lib messages decide
        result = h2.measure(self.root, items, self.config)
        self.assertEqual((result["total"], set(result["rows"]["w"])), (1, {("src/services/probe.rs", "inner::twice", WRAP)}))

class Baseline(Fixture):
    def run_main(self, *args: str) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = h2.main(["--repo", str(self.root), *args])
        return code, out.getvalue(), err.getvalue()

    def seed_baseline(self) -> None:
        config = {p: e for p, e in self.config.items() if "macos" in e[1]}
        rows = h2.measure(self.root, self.items(self.lines()), config)["rows"]
        h2.write_baseline(self.root, {s: {k: {"linux": 7, "macos": v} for k, v in r.items()} for s, r in rows.items()})

    def test_missing_baseline_is_inert_or_red(self) -> None:
        with mock.patch.object(h2, "session_runner", side_effect=AssertionError("session before the baseline gate")):
            code, out, _ = self.run_main("--lane", "linux", "--check", "--inert")
            self.assertEqual((code, out), (0, "h2: no baseline committed; inert no-op\n"))
            self.assertEqual(self.run_main("--lane", "linux", "--check")[0], 2)
        self.assertFalse((self.root / "target").exists())

    def test_check_compares_only_its_lane(self) -> None:
        self.seed_baseline()
        self.assertEqual(self.run_main("--lane", "macos", "--check", "--session", self.session("macos"))[0], 0)
        code, _, err = self.run_main("--lane", "linux", "--check", "--session", self.session("linux"))
        self.assertEqual(code, 1)
        self.assertIn("measured 1, baseline 7", err)
        code, _, err = self.run_main("--lane", "linux", "--check", "--inert", "--session", self.session("linux"))
        self.assertEqual(code, 0)
        self.assertIn("::warning::h2:", err)
        path = self.root / h2.BASELINE_FILES[0]
        row = next(line for line in path.read_text().splitlines() if line.startswith("  {"))
        path.write_text(path.read_text().replace(row, row + "\n" + row, 1))
        with self.assertRaises(h2.MeasureError):  # a duplicated row is rejected
            h2.load_baseline(self.root)

    def test_external_session_must_be_sealed_bound_and_of_this_lane(self) -> None:
        h2.write_baseline(self.root, {s: {} for s in h2.SECTIONS})
        empty = self.root / "unsealed"
        empty.mkdir()
        for args in (("--json", str(empty)), ("--regen", "--session", str(empty))):
            with self.subTest(args=args), redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                h2.main(["--repo", str(self.root), "--lane", "linux", *args])
        code, _, err = self.run_main("--lane", "linux", "--check", "--session", str(empty))
        self.assertEqual(code, 1)
        self.assertIn("unsealed", err)
        linux = self.session("linux", lines=[])
        code, _, err = self.run_main("--lane", "macos", "--check", "--session", linux)
        self.assertEqual(code, 1)
        self.assertIn("is not the macos run", err)

    def test_regen_reaches_fixpoint_and_keeps_other_lane(self) -> None:
        self.seed_baseline()
        passes: list[Path] = []
        h2.regen(self.root, "macos", runner=self.runner(lambda conf: passes.append(conf) or self.lines(), "macos"))
        config = h2.load_config(self.root / "clippy.toml")
        self.assertEqual(config[WRAP], ("W", frozenset(h2.LANES)))  # linux kept, macos derived
        self.assertEqual(config["agentdesk::services::probe::Probe::check"], ("W", frozenset({"macos"})))
        self.assertEqual(config["agentdesk::services::launch::ssh"], ("SUBPROC_W", frozenset({"macos"})))
        self.assertEqual(len(passes), 3)  # seed, callers, then convergence
        exec_rows = h2.load_baseline(self.root)["exec"]
        self.assertEqual(exec_rows[("src/services/probe.rs", "alive", TMUX)], {"linux": 7, "macos": 1})

class LaneGuards(Fixture):
    def test_lane_projection_and_seed_preserve_hand_kept_and_other_lane(self) -> None:
        config = {f"agentdesk::{s}_{tag}": (s, frozenset(h2.LANES if tag == "both" else [tag]))
                  for s in h2.SET_SECTION for tag in (*h2.LANES, "both")}
        for lane in h2.LANES:
            with self.subTest(lane=lane):
                self.assertEqual(h2.lane_config(config, lane), {p: e for p, e in config.items() if lane in e[1]})
                expected = {p: (s, ls - {lane} if s in ("W", "SUBPROC_W") else ls) for p, (s, ls) in config.items()}
                self.assertEqual(h2.seed_config(config, lane), {p: e for p, e in expected.items() if e[1]})
        self.assertEqual(len(config), 15)

    def test_lane_session_binds_its_run_and_lane_configuration(self) -> None:
        stored = (self.root / "clippy.toml").read_bytes()
        calls = []
        def runner(conf, run):
            calls.append((conf, run))
            return self.items(self.lines(), conf, run, "macos")
        items, conf = h2.lane_session(self.root, self.config, "macos", runner=runner)
        self.assertEqual(calls, [(conf.parent, conf.parent.parent / "check")])
        self.assertEqual(conf.parent.parent.parent, self.root / h2.SESSIONS)
        self.assertEqual(h2.load_config(conf), {p: e for p, e in self.config.items() if "macos" in e[1]})
        self.assertEqual(items.manifest["run_dir"], str(calls[0][1]))
        self.assertEqual((self.root / "clippy.toml").read_bytes(), stored)
        wrong = {"lane": lambda c, r: self.items([], c, r, "linux"), "run": lambda c, r: self.items([], c, c, "macos"),
                 "config": lambda c, r: self.items([], c.parent, r, "macos")}
        for name, other in wrong.items():
            with self.subTest(name=name), self.assertRaisesRegex(h2.MeasureError, "is not the macos run"):
                h2.lane_session(self.root, self.config, "macos", runner=other)

    def test_session_runner_builds_once_and_loads_what_the_session_sealed(self) -> None:
        import h2_modmap, h2_session
        driver = self.root / "driver"
        manifests = iter(({"manifest": str(self.root / f"r{n}/manifest.json")} for n in range(2)))
        with mock.patch.object(h2_modmap, "build_driver", return_value=driver) as build, \
                mock.patch.object(h2_session, "session", side_effect=lambda *a, **k: next(manifests)) as run, \
                mock.patch.object(h2_items, "load", side_effect=lambda path, crate: (path, crate)) as load:
            runner = h2.session_runner(self.root, "linux")
            self.assertEqual(runner(self.root / "c", self.root / "r0"), (self.root / "r0/manifest.json", self.root))
            runner(self.root / "c", self.root / "r1")
        build.assert_called_once_with(self.root)
        self.assertEqual(run.call_args_list[0], mock.call(
            self.root, self.root, self.root / "r0", self.root / "c", "linux", driver=driver,
            extra=("--locked", "--target-dir", str(self.root / "target/h2/target"))))
        self.assertEqual(load.call_count, 2)
        with mock.patch.object(h2_modmap, "build_driver", side_effect=h2_modmap.ModmapError("no rustc-dev")):
            with self.assertRaisesRegex(h2.MeasureError, "modmap-driver build: no rustc-dev"):
                h2.session_runner(self.root, "linux")

    def test_codeless_config_messages_fail_before_lint_and_target_filters(self) -> None:
        for file in ("clippy.toml", "./clippy.toml", "/tmp/conf/clippy.toml"):
            for message in ("does not refer to a reachable function", "expected a function, found a module",
                            "expected a type, found a method", "future diagnostic wording"):
                event = json.loads(diag("src/lib.rs", 1, 1, TMUX, kind="bin"))
                event["message"].update(code=None, message=message)
                event["message"]["spans"].append({"file_name": file, "is_primary": False})
                with self.subTest(file=file, message=message), self.assertRaisesRegex(h2.MeasureError, "clippy.*H2 path"):
                    h2.diagnostics([json.dumps(event)])
        event["message"]["spans"] = [{"file_name": "src/lib.rs"}]
        self.assertEqual(h2.diagnostics([json.dumps(event)]), [])
        self.assertEqual(h2.diagnostics([diag("src/lib.rs", 1, 1, TMUX)]),
                         [("src/lib.rs", 1, 1, "clippy::disallowed_methods", TMUX)])

    def test_coded_config_spans_fail_but_source_diagnostics_pass(self) -> None:
        for code in (*h2.LINTS, "unused_imports"):
            for primary in (False, True):
                event = json.loads(diag("clippy.toml" if primary else "src/lib.rs", 1, 1, TMUX, lint=code))
                if not primary:
                    event["message"]["spans"].append({"file_name": "/tmp/conf/clippy.toml", "is_primary": False})
                    event["target"]["kind"] = ["bin"]
                with self.subTest(code=code, primary=primary), self.assertRaisesRegex(h2.MeasureError, "clippy.*H2 path"):
                    h2.diagnostics([json.dumps(event)])
            expected = [("src/lib.rs", 1, 1, code, TMUX)] if code in h2.LINTS else []
            self.assertEqual(h2.diagnostics([diag("src/lib.rs", 1, 1, TMUX, lint=code)]), expected)

    def test_config_path_format_rejects_silently_ignored_spellings(self) -> None:
        for path in ("<A as B>::m", "crate::f", "unknown::f", "services::f", "agentdesk", "agentdesk::",
                     "agentdesk::r#match::f", "agentdesk::Type<T>::f", "agentdesk::foo bar", "::agentdesk::f",
                     "agentdesk::9f", "agentdesk::f\n"):
            (self.root / "clippy.toml").write_text(h2.render_clippy_toml({path: ("EXEC", frozenset(h2.LANES))}))
            with self.subTest(path=path), self.assertRaisesRegex(h2.MeasureError, "H2 path"):
                h2.load_config(self.root / "clippy.toml")
        valid = {f"{crate}::module::_f2": ("EXEC", frozenset(h2.LANES))
                 for crate in ("agentdesk", "std", "core", "alloc", "tokio")}
        (self.root / "clippy.toml").write_text(h2.render_clippy_toml(valid))
        self.assertEqual(h2.load_config(self.root / "clippy.toml"), valid)

    def test_regen_drops_stale_current_lane_and_preserves_other_lane(self) -> None:
        config = {TMUX: ("EXEC", frozenset(h2.LANES)), WRAP: ("W", frozenset(h2.LANES)),
                  "agentdesk::gone": ("W", frozenset({"macos"})),
                  "agentdesk::old_subproc": ("SUBPROC_W", frozenset(h2.LANES))}
        (self.root / "clippy.toml").write_text(h2.render_clippy_toml(config))
        passes = []
        def runner(conf):
            active = h2.load_config(conf / "clippy.toml")
            passes.append(active)
            self.assertNotIn("agentdesk::gone", active)
            self.assertNotIn("agentdesk::old_subproc", active)
            lines = [self.at("src/services/probe.rs", "crate::services::platform::tmux::has_session(name)", TMUX)]
            if WRAP in active:
                lines.append(self.at("src/services/probe.rs", "super::probe::alive", WRAP))
            return lines
        h2.regen(self.root, "macos", runner=self.runner(runner, "macos"))
        self.assertEqual(passes[0], {TMUX: config[TMUX]})
        self.assertEqual(len(passes), 3)
        stored = h2.load_config(self.root / "clippy.toml")
        self.assertNotIn("agentdesk::gone", stored)
        self.assertEqual(stored["agentdesk::old_subproc"], ("SUBPROC_W", frozenset({"linux"})))
        self.assertEqual(stored[WRAP], config[WRAP])
        self.assertIn("agentdesk::services::probe::Probe::check", stored)

    def test_regen_maps_each_pass_only_with_its_own_session(self) -> None:
        (self.root / "clippy.toml").write_text(h2.render_clippy_toml({TMUX: self.config[TMUX]}))
        runs = []
        def runner(conf, run):
            runs.append((conf, run))
            run.mkdir(parents=True)
            (run / "items.jsonl").write_text("{}")
            return self.items(self.lines(), conf, run)
        h2.regen(self.root, "linux", runner=runner)
        base = runs[0][1].parent
        self.assertEqual([run for _, run in runs], [base / f"pass-{n}" for n in range(1, len(runs) + 1)])
        self.assertEqual((len(runs) > 2, {conf for conf, _ in runs}), (True, {base / "conf"}))
        self.assertFalse(any((run / "items.jsonl").exists() for _, run in runs))
        first = []
        def reuse(conf, run):  # the first pass's items served again once the configuration grew
            first.append(first[0] if first else self.items(self.lines(), conf, run))
            return first[-1]
        (self.root / "clippy.toml").write_text(h2.render_clippy_toml({TMUX: self.config[TMUX]}))
        with self.assertRaisesRegex(h2.MeasureError, r"is not the linux run .*pass-2 "):
            h2.regen(self.root, "linux", runner=reuse)

    def test_regen_unregistrable_message_requires_restructuring(self) -> None:
        (self.root / "clippy.toml").write_text(h2.render_clippy_toml({TMUX: self.config[TMUX]}))
        self.paths[("src/services/relay_impl.rs", "send")] = "!external:core"
        with self.assertRaisesRegex(h2.MeasureError, r"cannot register \(reason\): .*::send \(external:core\); "
                                    "restructure the call site"):
            h2.regen(self.root, "linux", runner=self.runner(lambda conf: self.lines()))

    def test_seed_unreachable_wrapper_cycle_is_removed(self) -> None:
        source = self.sources["src/lib.rs"] = "fn a() { b(); }\nfn b() { a(); }\n"
        (self.root / "src/lib.rs").write_text(source)
        config = {TMUX: self.config[TMUX], **{f"agentdesk::{name}": ("W", frozenset({"linux"})) for name in "ab"}}
        (self.root / "clippy.toml").write_text(h2.render_clippy_toml(config))
        def runner(conf):
            active = h2.load_config(conf / "clippy.toml")
            return [diag("src/lib.rs", *locate(source, f"{callee}();"), f"agentdesk::{callee}")
                    for callee in "ab" if f"agentdesk::{callee}" in active]
        h2.regen(self.root, "linux", runner=self.runner(runner))
        self.assertEqual(h2.load_config(self.root / "clippy.toml"), {TMUX: self.config[TMUX]})
        self.assertEqual(h2.load_baseline(self.root)["w"], {})

    def test_main_uses_lane_config_for_its_session_and_an_external_one(self) -> None:
        for lane in h2.LANES:
            def runner(conf):
                self.assertEqual(h2.load_config(conf / "clippy.toml"), {p: e for p, e in self.config.items() if lane in e[1]})
                return self.lines()
            for external in (False, True):
                args = ["--session", self.session(lane)] if external else []
                with mock.patch.object(h2, "session_runner", return_value=self.runner(runner, lane)) as cargo, \
                        redirect_stdout(out := io.StringIO()):
                    rc = h2.main(["--repo", str(self.root), "--lane", lane, *args])
                self.assertEqual(rc, 0)
                self.assertEqual(cargo.call_count, 0 if external else 1)
                self.assertEqual(len(json.loads(out.getvalue())["rows"]["w"]), 2 if lane == "linux" else 0)

    def test_check_rejects_unresolved_current_lane_but_never_sends_other_lane(self) -> None:
        missing = "agentdesk::linux_only"
        h2.write_baseline(self.root, {s: {} for s in h2.SECTIONS})
        for tag, expected in (("linux", 0), ("macos", 1), ("both", 1)):
            config = {TMUX: ("EXEC", frozenset(h2.LANES)),
                      missing: ("W", frozenset(h2.LANES if tag == "both" else [tag]))}
            (self.root / "clippy.toml").write_text(h2.render_clippy_toml(config))
            def runner(conf):
                active = h2.load_config(conf / "clippy.toml")
                warning = json.loads(diag(str(conf / "clippy.toml"), 2, 1, missing))
                warning["message"]["code"] = None
                return [json.dumps(warning)] if missing in active else []
            with mock.patch.object(h2, "session_runner", return_value=self.runner(runner, "macos")), \
                    redirect_stdout(io.StringIO()), \
                    redirect_stderr(err := io.StringIO()), self.subTest(tag=tag):
                self.assertEqual(h2.main(["--repo", str(self.root), "--lane", "macos", "--check"]), expected)
                if expected:
                    self.assertIn("run --regen", err.getvalue())

    def test_regen_config_warning_fails_without_rewriting_stored_data(self) -> None:
        stored = (self.root / "clippy.toml").read_bytes()
        event = json.loads(diag("clippy.toml", 2, 1, TMUX))
        event["message"]["code"] = None
        with self.assertRaisesRegex(h2.MeasureError, "clippy could not use an H2 path"):
            h2.regen(self.root, "linux", runner=self.runner(lambda conf: [json.dumps(event)]))
        self.assertEqual((self.root / "clippy.toml").read_bytes(), stored)
        self.assertIsNone(h2.load_baseline(self.root))

    def test_external_json_config_warning_fails_check(self) -> None:
        h2.write_baseline(self.root, {s: {} for s in h2.SECTIONS})
        event = json.loads(diag("clippy.toml", 2, 1, TMUX))
        event["message"]["code"] = None
        path = self.session("linux", [json.dumps(event)])
        with mock.patch.object(h2, "session_runner") as cargo, redirect_stderr(err := io.StringIO()):
            rc = h2.main(["--repo", str(self.root), "--lane", "linux", "--check", "--session", path])
        self.assertEqual(rc, 1)
        self.assertIn("clippy could not use an H2 path", err.getvalue())
        cargo.assert_not_called()

class CleanEnvironment(unittest.TestCase):
    def test_python_modes_clear_each_key_and_preserve_execution_inputs(self) -> None:
        kept = {"PATH": "/bin", "CARGO_HOME": "/tmp/cargo", "CARGO_TARGET_DIR": "/tmp/target",
                "RUSTUP_TOOLCHAIN": "1.94.1", "CLIPPY_CONF_DIR": "/tmp/conf", "CARGO_TARGET_KEEP": "keep",
                "OTHER_CARGO_PROFILE_DEV_DEBUG": "keep"}
        with mock.patch.dict(os.environ, {**POISON, **kept}, clear=True):
            for mode in ("measure", "map", "driver"):
                env = h2_env.environment(mode)
                for key in CLEARED:
                    with self.subTest(mode=mode, key=key):
                        if key == "RUSTC_BOOTSTRAP" and mode == "driver":
                            self.assertEqual(env[key], "1")
                        else:
                            self.assertNotIn(key, env)
                self.assertEqual({key: env.get(key) for key in WRAPPERS}, dict.fromkeys(WRAPPERS, ""))
                self.assertEqual({key: env[key] for key in kept}, kept)
                self.assertEqual(env["CARGO_INCREMENTAL"], "0")
            self.assertEqual(dict(os.environ), {**POISON, **kept})

    def test_map_driver_injection_survives_clean_environment(self) -> None:
        import h2_modmap
        with mock.patch.dict(os.environ, POISON), mock.patch.object(h2_modmap.subprocess, "run",
                return_value=subprocess.CompletedProcess([], 0)) as run:
            h2_modmap.cargo(Path("/tmp"), "check", RUSTC_WORKSPACE_WRAPPER="/tmp/driver")
        env = run.call_args.kwargs["env"]
        self.assertEqual(env["RUSTC_WORKSPACE_WRAPPER"], "/tmp/driver")
        self.assertEqual(env["RUSTC_WRAPPER"], "")
        self.assertNotIn("CLIPPY_ARGS", env)

class ShellEntrypoint(unittest.TestCase):
    def run_sh(self, *args: str, host: str, probe: bool = False, forbid_work: bool = False) -> subprocess.CompletedProcess:
        with tempfile.TemporaryDirectory() as tmp:
            tree = Path(tmp)
            (tree / "scripts/ci").mkdir(parents=True)
            (tree / "scripts/ci/h2_measure.sh").write_text((REPO_ROOT / "scripts/ci/h2_measure.sh").read_text())
            (tree / "scripts/ci/h2_env.py").write_text((REPO_ROOT / "scripts/ci/h2_env.py").read_text())
            (bin_dir := tree / "bin").mkdir()
            (bin_dir / "python3").symlink_to(sys.executable)
            for tool, body in (("rustc", f"echo 'host: {host}'"), ("rustup", "echo clippy-aarch64"),
                               ("launcher", 'echo "py $*"')):
                (bin_dir / tool).write_text(f"#!/bin/sh\n{body}\n")
                (bin_dir / tool).chmod(0o755)
            if "--with-baseline" in args:
                (tree / h2.BASELINE_FILES[1]).write_text("")
                args = tuple(a for a in args if a != "--with-baseline")
            if probe:
                (bin_dir / "launcher").write_text(f"#!{sys.executable}\n" + textwrap.dedent("""\
                    import os, runpy, sys
                    assert sys.argv[1] == 'scripts/ci/h2_measure.py', sys.argv
                    assert os.getpid() == int(os.environ['H2_SHELL_PID']), 'launcher lost shell PID'
                    runpy.run_path(sys.argv[1], run_name='__main__')
                """))
                (tree / "scripts/ci/h2_measure.py").write_text(textwrap.dedent("""\
                    import json, os, sys
                    print(json.dumps({'argv': sys.argv[1:], 'incremental': os.environ['CARGO_INCREMENTAL'],
                                      'flags': [key for key in json.loads(os.environ['H2_CLEAR_KEYS']) if key in os.environ],
                                      'wrappers': {key: os.environ.get(key) for key in json.loads(os.environ['H2_WRAPPER_KEYS'])}}))
                """))
            if forbid_work:
                for tool in ("python3", "rustc", "rustup", "launcher"):
                    (bin_dir / tool).unlink()
                    (bin_dir / tool).write_text('#!/bin/sh\necho "unexpected collection" >&2\nexit 97\n')
                    (bin_dir / tool).chmod(0o755)
            env = dict(os.environ, **POISON, PATH=f"{bin_dir}:{os.environ['PATH']}", PYTHON=str(bin_dir / "launcher"),
                       H2_CLEAR_KEYS=json.dumps(CLEARED), H2_WRAPPER_KEYS=json.dumps(WRAPPERS), CARGO_INCREMENTAL="1")
            return subprocess.run(["bash", "-c", 'export H2_SHELL_PID=$$; exec bash "$@"', "h2-test",
                                   str(tree / "scripts/ci/h2_measure.sh"), *args],
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

    def test_python_launcher_keeps_process_and_clean_environment(self) -> None:
        result = self.run_sh("--lane", "macos", "--regen", "--with-baseline", host="aarch64-apple-darwin", probe=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), {"argv": ["scripts/ci/h2_measure.py", "--lane", "macos", "--regen"],
                                                   "incremental": "0", "flags": [], "wrappers": dict.fromkeys(WRAPPERS, "")})

    def test_inert_exits_before_any_environment_or_measurement_collection(self) -> None:
        result = self.run_sh("--lane", "linux", "--inert", host="wrong", forbid_work=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("inert no-op", result.stdout)
        self.assertEqual(result.stderr, "")

if __name__ == "__main__":
    unittest.main()
