"""Tests for the H2 module map wrapper against stub cargo/rustup: only a complete map this run wrote is accepted."""

from __future__ import annotations

import io
import copy
import os
import json
import re
import signal
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT / "scripts/ci"))
import h2_depinfo  # noqa: E402
import h2_measure as h2  # noqa: E402
import h2_modmap as modmap  # noqa: E402
import h2_session  # noqa: E402

SUITE_ARGS = tuple(x for module in modmap.SESSION_SUITES for x in ("--suite", module))

# The canary rows the real driver writes: `shared`, `spliced` and `wrapped::passed` are clean, the rest is not.
CANARY_ROWS = ["src/shared.rs\tcrate::shared\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:13:1: 13:12 (#0)\t-\tfile",
               "src/lib.rs\tcrate::wrapped\t#4\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:9:49: 9:68 (#4)\t-\tinline",
               "src/lib.rs\tcrate::wrapped\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:15:17: 15:32 (#0)\t-\twrapped",
               "src/wrapped/passed.rs\tcrate::wrapped::passed\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:15:17: 15:32 (#0)\t-\tfile",
               "src/lib.rs\tcrate::named\t#0\t#5\tmodule\tsrc/lib.rs\tsrc/lib.rs:16:7: 16:31 (#0)\t-\tinline",
               "src/lib.rs\tcrate::named\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:16:13: 16:29 (#0)\t-\twrapped",
               "src/lib.rs\tcrate::keyword\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:17:5: 17:37 (#0)\t-\tinline",
               "src/lib.rs\tcrate::keyword\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:17:19: 17:35 (#0)\t-\twrapped",
               "src/lib.rs\tcrate::spliced\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:19:1: 21:2 (#0)\t-\tinline",
               "src/shared.rs\tcrate::spliced\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/shared.rs:1:1: 1:19 (#0)\t-\tinclude",
               'src/shared.rs\tcrate::{fn probe}::injected\t#8\t#8\tfn:probe\tsrc/lib.rs\tsrc/lib.rs:6:9: 6:22 (#8)'
               '\tpath#8["shared.rs"]\tfile']
# The canary as a driver that lost every expansion context would write it.
DRIFTED_ROWS = [re.sub(r"#[1-9]\d*", "#0", row) for row in CANARY_ROWS]
# Stub cargo: `build` leaves a driver binary; `check` does what STUB_CANARY / STUB_REPO says with MODMAP_OUT.
STUB_CARGO = r"""#!/usr/bin/env python3
import json, os, pathlib, platform, shutil, sys
args = sys.argv[1:]
if os.environ.get("STUB_CALLS"):
    with open(os.environ["STUB_CALLS"], "a") as calls: calls.write(json.dumps(args) + "\n")
if os.environ.get("STUB_ENV_LOG"):
    keys = ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET", "RUSTC_BOOTSTRAP", "CARGO_INCREMENTAL",
            "RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER")
    with open(os.environ["STUB_ENV_LOG"], "a") as log:
        log.write(json.dumps({key: os.environ.get(key) for key in keys}) + "\n")
if args[0] == "build":
    driver = pathlib.Path(args[args.index("--target-dir") + 1], "release/modmap-driver")
    driver.parent.mkdir(parents=True, exist_ok=True)
    driver.touch()
    sys.exit(0)
manifest = args[args.index("--manifest-path") + 1]
action, _, source = os.environ["STUB_CANARY" if "/canary/" in manifest else "STUB_REPO"].partition(":")
out = pathlib.Path(os.environ["MODMAP_OUT"])
if action == "fail":
    print(json.dumps({"reason": "compiler-message", "message": {"rendered": "error: driver rejected the map", "level": "error"}}))
    print("cargo: check failed", file=sys.stderr)
    sys.exit(101)
if action in ("copy", "old"):
    shutil.copy(source, out)
if action == "old":
    os.utime(out, ns=(0, 0))
if os.environ.get("MODMAP_CFG_OUT"):
    cfg = pathlib.Path(os.environ["MODMAP_CFG_OUT"])
    proof = pathlib.Path(str(cfg) + ".invocation.json")
    mode = os.environ.get("STUB_CFG_" + os.environ.get("MODMAP_KIND", "").upper(), os.environ.get("STUB_CFG", "ok"))
    probes = [["feature", "h2_cfg_probe"], ["h2_probe_pair"], ["h2_probe_pair", ""],
              ["h2_probe_multi", "first"], ["h2_probe_multi", "second"],
              ["h2_probe_escape", 'quote=" slash=\\ newline=\n한글']]
    session = [["target_os", "macos" if sys.platform == "darwin" else "linux"],
               ["target_arch", "aarch64" if platform.machine() in ("arm64", "aarch64") else "x86_64"],
               ["target_family", "unix"], ["unix"], ["panic", "unwind"], ["debug_assertions"]]
    atoms = sorted(probes + session)
    crate = pathlib.Path(manifest).parent
    argv = [os.environ["RUSTC_WORKSPACE_WRAPPER"], "/rustc", "src/lib.rs"]
    for atom in probes:
        argv += ["--cfg", atom[0] + ("=" + json.dumps(atom[1], ensure_ascii=False) if len(atom) == 2 else "")]
    invocation = dict(nonce=os.environ["MODMAP_CFG_NONCE"], argv=argv, root=str(crate))
    if mode == "nonce": invocation["nonce"] = "earlier-run"
    if mode == "argv": invocation["argv"] += ["--test"]
    if mode == "feature": atoms.remove(["feature", "h2_cfg_probe"])
    if mode == "unexpected": atoms.append(["h2_probe_extra"])
    if mode == "target": atoms.remove(session[0])
    if mode == "none": sys.exit(0)
    cfg.write_text("{}" if mode == "schema" else json.dumps(sorted(atoms)))
    if os.environ.get("MODMAP_RUN_ID"):
        bound = dict(schema=1, run_id=os.environ["MODMAP_RUN_ID"], nonce=os.environ["MODMAP_CFG_NONCE"], atoms=sorted(atoms))
        invocation.update(schema=1, run_id=bound["run_id"], kind=os.environ["MODMAP_KIND"], out=str(out),
                          tsv=out.read_text() if out.exists() else "", env={"MODMAP_RUN_ID": bound["run_id"]})
        if mode == "repost": bound["nonce"] = "0" * 32
        if mode != "schema": cfg.write_text(json.dumps(bound))
    if mode != "no-proof": proof.write_text(json.dumps(invocation))
    if mode == "old": os.utime(cfg, ns=(0, 0))
    if mode == "old-proof": os.utime(proof, ns=(0, 0))
    if mode == "fail-after": sys.exit(101)
"""

def write_modmap(path: Path, rows) -> Path:
    path.write_text("".join(f"{line}\n" for line in (h2_depinfo.MODMAP_HEADER, h2_depinfo.MODMAP_ROOT, *rows)), encoding="utf-8")
    return path

class Wrapper(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.maps, self.root, stubs = Path(tmp.name), Path(tmp.name) / "repo", Path(tmp.name) / "bin"
        (self.root / "scripts/ci").mkdir(parents=True)
        (self.root / h2.BASELINE_FILES[0]).write_text("", encoding="utf-8")
        stubs.mkdir()
        for name, text in (("cargo", STUB_CARGO), ("rustup", "#!/bin/sh\necho rustc-dev-x86_64-unknown-linux-gnu\n")):
            (stubs / name).write_text(text, encoding="utf-8")
            (stubs / name).chmod(0o755)
        clean = [f"src/m{n}.rs\tcrate::m{n}\t#0\t#0\tmodule\tsrc/lib.rs\tsrc/lib.rs:{n}:1: {n}:9 (#0)\t-\tfile" for n in range(1000)]
        # only file rows count: the short map pads 3 file modules with 997 inline ones
        padded = clean[:3] + [row.replace("\tfile", "\tinline") for row in clean[3:]]
        self.full, self.short = write_modmap(self.maps / "full.tsv", clean), write_modmap(self.maps / "short.tsv", padded)
        self.env = dict(os.environ, PATH=f"{stubs}{os.pathsep}{os.environ['PATH']}", STUB_REPO=f"copy:{self.full}",
                        STUB_CANARY=f"copy:{write_modmap(self.maps / 'canary.tsv', CANARY_ROWS)}")

    def run_wrapper(self, *args: str, **env: str) -> tuple[int, str]:
        args = tuple(x for arg in args for x in ((arg, *SUITE_ARGS) if arg == "--canary" else (arg,)))
        entry = (f"import sys; sys.path.insert(0, {str(REPO_ROOT / 'scripts/ci')!r}); "
                 "import h2_modmap; from unittest.mock import patch; "
                 "p = patch.object(h2_modmap, 'check_session_canary'); p.start(); "
                 "sys.exit(h2_modmap.main(sys.argv[1:]))")
        proc = subprocess.run([sys.executable, "-c", entry, "--repo", str(self.root), *args],
                              env={**self.env, **env}, capture_output=True, text=True)
        return proc.returncode, proc.stdout + proc.stderr

    def test_only_a_complete_map_written_by_this_run_passes(self) -> None:
        code, output = self.run_wrapper()
        self.assertEqual((code, "h2-modmap: 1000 file modules -> " in output), (0, True), output)
        # no write at rc 0 (a pass-through compile), a stale mtime, a short map, a failed check; a map left by an
        # earlier run sits at the output path each time
        for action, needle in (("none", "was not written"), (f"old:{self.full}", "predates this run"),
                               (f"copy:{self.short}", "lists 3 file modules (< 1000)"), ("fail", "failed (101)")):
            with self.subTest(action=action.split(":")[0]):
                shutil.copy(self.full, self.root / "target/h2/modmap.tsv")
                code, output = self.run_wrapper(STUB_REPO=action)
                self.assertEqual((code, needle in output), (1, True), output)

    def test_inert_maps_the_repo_once_a_baseline_exists(self) -> None:
        self.assertEqual(self.run_wrapper("--inert")[0], 0)
        code, output = self.run_wrapper("--inert", STUB_REPO="fail")
        self.assertEqual((code, "failed (101)" in output), (1, True), output)

    def test_the_canary_must_keep_its_verdict_even_while_inert(self) -> None:
        drifted = f"copy:{write_modmap(self.maps / 'drifted.tsv', DRIFTED_ROWS)}"
        self.assertEqual(self.run_wrapper(STUB_CANARY=drifted)[0], 1)
        (self.root / h2.BASELINE_FILES[0]).unlink()
        # inert without a baseline: nothing runs, or only the canary, which still fails hard
        self.assertEqual(self.run_wrapper("--inert", STUB_CANARY="fail", STUB_REPO="fail"),
                         (0, "h2-modmap: no baseline committed; inert no-op; root=skipped\n"))
        code, output = self.run_wrapper("--inert", "--canary", STUB_REPO="fail")
        self.assertEqual((code, "repo map skipped" in output), (0, True), output)
        code, output = self.run_wrapper("--inert", "--canary", STUB_CANARY=drifted)
        self.assertEqual((code, "driver canary drifted" in output), (1, True), output)

    def test_cfg_requires_fresh_output_and_this_driver_invocation(self) -> None:
        (self.root / h2.BASELINE_FILES[0]).unlink()
        cases = (("none", "was not written"), ("old", "predates this run"),
                 ("no-proof", "was not written"), ("old-proof", "predates this run"),
                 ("nonce", "nonce mismatch"), ("argv", "argv/root mismatch"),
                 ("schema", "expected a nonempty cfg array"), ("feature", "probe atoms differ"),
                 ("unexpected", "probe atoms differ"), ("target", "session target/codegen atoms differ"),
                 ("fail-after", "failed (101)"))
        for mode, needle in cases:
            with self.subTest(mode=mode):
                code, output = self.run_wrapper("--inert", "--canary", STUB_REPO="fail")
                self.assertEqual((code, "cfg self-test holds" in output), (0, True), output)
                code, output = self.run_wrapper("--inert", "--canary", STUB_CFG=mode, STUB_REPO="fail")
                self.assertEqual((code, needle in output), (1, True), output)

    def test_json_diagnostics_and_stderr_survive_canary_and_root_failure(self) -> None:
        for which in ("STUB_CANARY", "STUB_REPO"):
            with self.subTest(which=which):
                code, output = self.run_wrapper(**{which: "fail"})
                self.assertEqual(code, 1, output)
                self.assertIn("error: driver rejected the map", output)
                self.assertIn("cargo: check failed", output)
                name = "canary" if which == "STUB_CANARY" else "modmap"
                log = self.root / f"target/h2/{name}.cargo.jsonl"
                self.assertEqual(json.loads(log.read_text())["reason"], "compiler-message")
                self.assertIn("cargo: check failed", log.with_suffix(".stderr").read_text())

    def test_shared_env_is_clean_and_bootstrap_is_driver_only(self) -> None:
        log = self.maps / "env.jsonl"
        code, output = self.run_wrapper(STUB_ENV_LOG=str(log), RUSTFLAGS="bad", CARGO_ENCODED_RUSTFLAGS="bad",
                                        CARGO_BUILD_TARGET="foreign", RUSTC_BOOTSTRAP="bad", RUSTC_WRAPPER="cache",
                                        CARGO_BUILD_RUSTC_WRAPPER="cache", CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER="cache")
        self.assertEqual(code, 0, output)
        build, canary, root = [json.loads(line) for line in log.read_text().splitlines()]
        for env in (build, canary, root):
            for key in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET"):
                self.assertIsNone(env[key], key)
            self.assertEqual(env["CARGO_INCREMENTAL"], "0")
            for key in ("RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"):
                self.assertEqual(env[key], "", key)
        self.assertEqual([env["RUSTC_BOOTSTRAP"] for env in (build, canary, root)], ["1", None, None])
        self.assertEqual(build["RUSTC_WORKSPACE_WRAPPER"], "")
        self.assertEqual(canary["RUSTC_WORKSPACE_WRAPPER"], root["RUSTC_WORKSPACE_WRAPPER"])
        self.assertTrue(root["RUSTC_WORKSPACE_WRAPPER"].endswith("/release/modmap-driver"))

class SessionCanary(unittest.TestCase):
    def test_non_canary_reports_suites_not_run(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(modmap, "build_driver"), \
                patch.object(modmap, "collection_context"), patch.object(modmap, "map_run", return_value=[]), \
                patch.object(modmap.h2_depinfo, "modmap_problems", return_value=modmap.CANARY_PROBLEMS), \
                patch.object(h2_session, "session", return_value={"proof": {"cfg": ["clippy", "h2_items_bs_clippy"]}}), \
                patch.object(modmap, "check_canary_items"), patch.object(modmap, "run_suite") as suite, \
                patch("sys.stdout", new=io.StringIO()) as output:
            self.assertEqual(modmap.main(["--repo", tmp, "--lane", "linux"]), 0)
            self.assertIn("cold Clippy session and items anchors hold; suites not run", output.getvalue())
            self.assertNotIn("driver controls and workspace e2e hold", output.getvalue())
            suite.assert_not_called()

    def test_slow_suite_times_out_and_kills_its_process_group(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(modmap.os, "killpg", wraps=os.killpg) as killpg:
            root = Path(tmp).resolve()
            (root / "tests").mkdir()
            (root / "tests/__init__.py").touch()
            (root / "tests/test_h2_session_driver.py").write_text(
                "import os, subprocess, sys, time\n"
                "from pathlib import Path\n"
                "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])\n"
                "Path('group').write_text(str(os.getpgrp()) + ':' + str(os.getpgid(child.pid)))\n"
                "sys.stderr.write('Ran 8 tests in 0.0s\\n\\nOK\\n'); sys.stderr.flush()\n"
                "time.sleep(60)\n")
            with self.assertRaisesRegex(modmap.ModmapError, "TIMEOUT.*tests.test_h2_session_driver"):
                modmap.run_suite(root, root / "driver", "tests.test_h2_session_driver", timeout=1)
            parent_group, child_group = map(int, (root / "group").read_text().split(":"))
            self.assertEqual(parent_group, child_group)
            killpg.assert_called_once_with(parent_group, signal.SIGKILL)
            with patch.object(modmap.subprocess, "Popen") as ended, \
                    patch.object(modmap.os, "killpg", side_effect=ProcessLookupError):
                process = ended.return_value.__enter__.return_value
                process.returncode = 0
                process.communicate.side_effect = [subprocess.TimeoutExpired("suite", 1), (None, "Ran 8 tests in 1s\nOK\n")]
                with self.assertRaisesRegex(modmap.ModmapError, "TIMEOUT"):
                    modmap.run_suite(root, root / "driver", "tests.test_h2_session_driver", timeout=1)

    def test_cold_session_cfg_and_required_suites(self):
        suites = list(modmap.SESSION_SUITES)
        with tempfile.TemporaryDirectory() as tmp, patch.object(h2_session, "session") as session, \
                patch.object(modmap.subprocess, "Popen") as suite, patch("sys.stdout", new=io.StringIO()) as output, \
                patch.object(modmap, "check_canary_items") as items:
            root = Path(tmp).resolve()
            driver, crate = root / "driver", root / modmap.DRIVER / "canary"
            process = suite.return_value.__enter__.return_value
            process.returncode = 0
            process.communicate.return_value = (None, f"......\nRan {max(modmap.SESSION_SUITES.values())} tests in 9.1s\n\nOK\n")
            session.return_value = {"proof": {"cfg": ["clippy", "h2_items_bs_clippy"]}}
            for n in range(2):
                run = root / "target/h2/runs" / str(n)
                modmap.check_session_canary(root, driver, crate, run, "linux", suites)
                args, kwargs = session.call_args
                self.assertEqual(args, (root, crate, run / "session", run / "session-config", "linux"))
                self.assertEqual(kwargs, dict(driver=driver, extra=("--locked", "--target-dir",
                                 str(run / "session-target"), "--features", "h2_cfg_probe")))
                self.assertFalse((run / "session-target").exists())
                self.assertEqual((run / "session-config/clippy.toml").read_text(), "")
                items.assert_called_with(crate, run / "session/items.jsonl")
            self.assertEqual([c.args[0] for c in suite.call_args_list],
                             [[sys.executable, "-m", "unittest", module] for module in suites] * 2)
            self.assertTrue(all(c.kwargs["env"]["H2_SESSION_DRIVER"] == str(driver) for c in suite.call_args_list))
            self.assertTrue(all(c.kwargs["start_new_session"] for c in suite.call_args_list))
            self.assertTrue(all(c.kwargs["timeout"] == 1200 for c in process.communicate.call_args_list))
            self.assertEqual(output.getvalue().count("driver controls and workspace e2e hold"), 2)
            for cfg in (["clippy"], ["h2_items_bs_clippy"], []):
                session.return_value = {"proof": {"cfg": cfg}}
                with self.assertRaisesRegex(modmap.ModmapError, "build-script cfg"):
                    modmap.check_session_canary(root, driver, crate, root / str(cfg), "linux", suites)
            self.assertEqual(suite.call_count, 4)
            with patch.object(modmap, "check_canary_items", side_effect=modmap.ModmapError("items failed")):
                session.return_value = {"proof": {"cfg": ["clippy", "h2_items_bs_clippy"]}}
                with self.assertRaisesRegex(modmap.ModmapError, "items failed"):
                    modmap.check_session_canary(root, driver, crate, root / "items-failed", "linux", suites)
            self.assertEqual(suite.call_count, 4)
            session.return_value = {"proof": {"cfg": ["clippy", "h2_items_bs_clippy"]}}
            for module, n in modmap.SESSION_SUITES.items():
                process.returncode = 1
                with self.assertRaisesRegex(modmap.ModmapError, f"{module} failed"):
                    modmap.run_suite(root, driver, module)
                process.returncode = 0
                for i, stderr in enumerate(("", "\nRan 0 tests in 0.0s\n\nOK\n", f"Ran {n - 1} tests in 1s\n\nOK\n",
                                            f"Ran {n} tests in 1s\n\nOK (skipped={n})\n", f"Ran {n} tests in 1s\n")):
                    process.communicate.return_value = (None, stderr)
                    with self.assertRaisesRegex(modmap.ModmapError, f"{module} ran incompletely or skipped"):
                        modmap.run_suite(root, driver, module)
            # the first failing suite stops the canary before the next one runs
            suite.reset_mock()
            with self.assertRaisesRegex(modmap.ModmapError, suites[0]):
                modmap.check_session_canary(root, driver, crate, root / "stop", "linux", suites)
            self.assertEqual(suite.call_count, 1)
            occupied = root / "occupied"
            (occupied / "session-target").mkdir(parents=True)
            with self.assertRaisesRegex(modmap.ModmapError, "cold target"):
                modmap.check_session_canary(root, driver, crate, occupied, "linux", suites)
            self.assertEqual(output.getvalue().count("driver controls and workspace e2e hold"), 2)

    def test_canary_must_name_every_suite_once(self):
        first, second = modmap.SESSION_SUITES
        for argv in (["--canary"], ["--canary", "--suite", first], ["--canary", "--suite", second],
                     ["--canary", "--suite", first, "--suite", first, "--suite", second],
                     ["--suite", first, "--suite", second], ["--canary", "--suite", "tests.test_h2_session"]):
            with self.subTest(argv=argv), patch.object(modmap, "build_driver") as build, \
                    patch("sys.stderr", new=io.StringIO()), self.assertRaises(SystemExit) as exit_:
                modmap.main(["--inert", *argv])
            self.assertEqual(exit_.exception.code, 2)
            build.assert_not_called()

    def test_inert_canary_runs_session_and_propagates_failure(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(modmap, "build_driver") as build, \
                patch.object(modmap, "collection_context"), patch.object(modmap, "map_run"), \
                patch.object(modmap, "map_modules"), \
                patch.object(modmap.h2_depinfo, "modmap_problems", return_value=modmap.CANARY_PROBLEMS), \
                patch.object(modmap, "check_session_canary", side_effect=h2.MeasureError("session failed")) as session:
            for flags, lane in ((["--lane", "linux"], "linux"), ([], "macos" if sys.platform == "darwin" else "linux")):
                self.assertEqual(modmap.main(["--repo", tmp, "--inert", "--canary", *SUITE_ARGS, *flags]), 1)
                self.assertEqual(session.call_args.args[-2:], (lane, list(modmap.SESSION_SUITES)))
            self.assertEqual(session.call_count, 2)
            session.reset_mock()
            build.reset_mock()
            self.assertEqual(modmap.main(["--repo", tmp, "--inert"]), 0)
            session.assert_not_called()
            build.assert_not_called()

class ItemsCanary(unittest.TestCase):
    def test_source_oracle_rejects_spans_paths_owners_and_empty_output(self):
        crate = REPO_ROOT / modmap.DRIVER / "canary"
        rows = []
        for i, (kind, value, name, anchor) in enumerate(modmap.CANARY_ITEMS):
            raw = (crate / "src" / name).read_bytes()
            matches = list(re.finditer(anchor, raw, re.S))
            self.assertEqual(len(matches), 1, anchor)
            lo, hi = matches[0].span()
            path = "modmap_canary::" + value if value.startswith("items_") else None
            rows.append(dict(kind=kind, path=path, unregistrable=None if path else value,
                             file="src/" + name, lo=lo, hi=hi, line=raw.count(b"\n", 0, lo) + 1,
                             macro=b"!" in raw[lo:hi], def_kind="Fn", parent=0, **{"def": i + 1}))
            if value == "anon-const":
                rows[-1]["def_kind"] = "AnonConst" if b"mixed!" in raw[lo:hi] else "InlineConst"
            if kind == "nested_fn":
                owner = next(j for j, row in enumerate(rows) if row["kind"] == "fn" and
                             row["lo"] < lo < hi < row["hi"] and row["file"] == "src/" + name)
                rows[-1].update(fold=owner, parent=rows[owner]["def"], escape=value == "fold-escape")
        method = next(r for r in rows if r["macro"] and r["kind"] == "trait_impl_method")
        header = dict(method, kind="header", def_kind="Impl { of_trait: true }", **{"def": 100})
        method["parent"] = 100
        rows.append(header)
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "items.jsonl"
            def check(records):
                arrays = [[r.get(key) for key in modmap.ITEM_FIELDS[:14 if r["kind"] == "nested_fn" else 12]] for r in records]
                output.write_text("{}\n" + "".join(json.dumps(r) + "\n" for r in arrays))
                modmap.check_canary_items(crate, output)
            check(rows)
            for key, value in (("lo", 0), ("hi", 0), ("line", 999), ("path", "wrong")):
                bad = copy.deepcopy(rows)
                bad[0][key] = value
                with self.subTest(key=key), self.assertRaises(modmap.ModmapError):
                    check(bad)
            for bad in ([], rows[1:], rows + [rows[0]], [r for r in rows if r is not header]):
                with self.assertRaises(modmap.ModmapError):
                    check(bad)
            for mutation in ("normalized", "definition"):
                bad = copy.deepcopy(rows)
                row = next(r for r in bad if r["file"].endswith("items_crlf.rs")) if mutation == "normalized" else next(
                    r for r in bad if r["path"] == "modmap_canary::items_probe::made_a")
                raw = (crate / row["file"]).read_bytes()
                if mutation == "normalized":
                    for bound in ("lo", "hi"):
                        row[bound] -= 3 + raw[:row[bound]].count(b"\r\n")
                else:
                    row["lo"] = raw.index(b"pub fn made_a() {}")
                    row["hi"] = row["lo"] + len(b"pub fn made_a() {}")
                with self.subTest(mutation=mutation), self.assertRaisesRegex(modmap.ModmapError, "anchor"):
                    check(bad)
            fixture = Path(tmp) / "canary"
            shutil.copytree(crate / "src", fixture / "src")
            crlf = fixture / "src/items_crlf.rs"
            crlf.write_bytes(crlf.read_bytes().replace(b"\r\n", b"\n"))
            with self.assertRaisesRegex(modmap.ModmapError, "BOM/CRLF"):
                modmap.check_canary_items(fixture, output)


class CiWiring(unittest.TestCase):
    def test_linux_script_checks_self_test_the_driver_without_a_wrapper(self) -> None:
        import yaml  # installed on the script-check runners
        steps = yaml.safe_load((REPO_ROOT / ".github/workflows/ci-pr.yml").read_text(encoding="utf-8"))["jobs"]["scripts"]["steps"]
        toolchain = next(step for step in steps if step.get("uses", "").startswith("dtolnay/rust-toolchain"))
        step = next(step for step in steps if step.get("name") == "H2 module map (linux, inert)")
        components = [part.strip() for part in toolchain["with"]["components"].split(",")]
        self.assertIn("rustc-dev", components)
        self.assertIn("llvm-tools", components)
        self.assertEqual(step.get("timeout-minutes"), 30)
        self.assertEqual((step.get("if"), step.get("env"), step["run"]),
                         (None, {"RUSTC_WRAPPER": ""}, "python3 scripts/ci/h2_modmap.py --lane linux --inert --canary "
                          + " ".join(SUITE_ARGS)))
        self.assertLess(steps.index(toolchain), steps.index(step))

if __name__ == "__main__":
    unittest.main()
