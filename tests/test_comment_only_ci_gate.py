"""Comment-only CI skip: scripts/ci/comment_only_gate.py and its ci-pr.yml wiring.

A false ``comment_only=true`` skips the PostgreSQL, high-risk, Windows and
library-sweep lanes for real code, so the gate cases below pin the refusals. A
false ``rust_tests_skip=true`` skips the library sweep whose tests read a changed
file through ``include_str!``, so the include cases pin what that scan resolves.
The workflow cases evaluate ci-pr.yml's own expressions and run the real
required-check-mirror.sh, so a mirror that reads a different output than the
job it mirrors turns a skipped lane into a red required context here.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

import yaml

REPO_ROOT = Path(__file__).resolve().parents[1]
PR_WORKFLOW = REPO_ROOT / ".github/workflows/ci-pr.yml"
MIRROR = REPO_ROOT / "scripts/required-check-mirror.sh"
sys.path.insert(0, str(REPO_ROOT / "scripts" / "ci"))
sys.path.insert(0, str(REPO_ROOT / "scripts"))

import comment_only_gate as gate  # noqa: E402
import rust_include_reads as includes  # noqa: E402

SKIPPED_WHEN_COMMENT_ONLY = (
    "test_fast",
    "high-risk-recovery",
    "check_fast_cross_os",
    "check_fast_cross_os_targets",
    "library_sweep",
)
KEPT_WHEN_COMMENT_ONLY = ("check_fast", "lint", "scripts", "scripts_guards", "scripts_contracts")
REQUIRED_CONTEXTS = (
    "Lint",
    "Script checks",
    "Fast check (ubuntu-latest)",
    "High-risk recovery",
    "Dashboard (Node 22)",
    "Fast targeted tests (ubuntu-latest)",
    "relay-authority-contract",
    "Library test sweep (ubuntu-latest)",
    "Fast check cross OS required context (ubuntu-latest)",
)


def outputs(**lists: list[str]) -> dict[str, str]:
    """paths-filter outputs as toJSON renders them with ``list-files: json``."""
    rendered: dict[str, str] = {"changes": json.dumps([n for n, f in lists.items() if f])}
    for name, files in lists.items():
        rendered[name] = "true" if files else "false"
        rendered[f"{name}_count"] = str(len(files))
        rendered[f"{name}_files"] = json.dumps(files)
    return rendered


RS = "src/server/mod.rs"
RS_FILTERS = dict(pg_db=[RS], rust_or_policy=[RS], rust_compile=[RS], cross_os_rust=[RS], dashboard=[], relay_contract=[])


class DecideTests(unittest.TestCase):
    def assertVerdict(self, expected: bool, rc: int, entries, filters, reason: str = "") -> None:
        verdict, reasons = gate.decide(rc, entries, filters)
        self.assertEqual(verdict, expected, reasons)
        if reason:
            self.assertTrue(any(reason in item for item in reasons), reasons)

    def test_comment_only_rust_edit_passes(self) -> None:
        self.assertVerdict(True, 0, [("M", RS, RS)], outputs(**RS_FILTERS))

    def test_rust_edit_with_an_unfiltered_markdown_passes(self) -> None:
        entries = [("M", RS, RS), ("M", "docs/guide.md", "docs/guide.md"), ("A", "notes.md", "notes.md")]
        self.assertVerdict(True, 0, entries, outputs(**RS_FILTERS))

    def test_judge_failure_or_usage_error_refuses(self) -> None:
        for rc in (1, 2, -9):
            with self.subTest(rc=rc):
                self.assertVerdict(False, rc, [("M", RS, RS)], outputs(**RS_FILTERS), "exited")

    def test_docs_only_change_refuses(self) -> None:
        self.assertVerdict(False, 0, [("M", "README.md", "README.md")], outputs(dashboard=[]), "no changed .rs")

    def test_non_markdown_file_refuses_even_when_no_filter_selects_it(self) -> None:
        entries = [("M", RS, RS), ("A", "docs/diagram.svg", "docs/diagram.svg")]
        self.assertVerdict(False, 0, entries, outputs(**RS_FILTERS), "non-Rust, non-Markdown")

    def test_workflow_file_refuses(self) -> None:
        wf = ".github/workflows/ci-pr.yml"
        filters = {**RS_FILTERS, "high_risk_recovery": [wf], "rust_or_policy": [RS, wf]}
        self.assertVerdict(False, 0, [("M", RS, RS), ("M", wf, wf)], outputs(**filters), "non-Rust, non-Markdown")

    def test_markdown_that_selects_a_filter_refuses(self) -> None:
        doc = "docs/relay-state-contract.md"
        filters = {**RS_FILTERS, "relay_contract": [doc]}
        self.assertVerdict(False, 0, [("M", RS, RS), ("M", doc, doc)], outputs(**filters), doc)

    def test_added_deleted_renamed_or_retyped_rust_refuses(self) -> None:
        for entry in (
            ("A", "src/new.rs", "src/new.rs"),
            ("D", RS, RS),
            ("R", "src/a.rs", "src/b.rs"),
            ("R", "src/a.rs", "src/a.md"),
            ("R", "src/a.md", "src/a.rs"),
            ("T", RS, RS),
        ):
            with self.subTest(entry=entry):
                self.assertVerdict(False, 0, [("M", RS, RS), entry], outputs(**RS_FILTERS), "not an in-place edit")

    def test_filter_file_git_did_not_report_refuses(self) -> None:
        filters = {**RS_FILTERS, "pg_db": [RS, "src/db/postgres.rs"]}
        self.assertVerdict(False, 0, [("M", RS, RS)], outputs(**filters), "src/db/postgres.rs")

    def test_missing_or_malformed_filter_outputs_refuse(self) -> None:
        broken = outputs(**RS_FILTERS)
        del broken["pg_db_files"]
        true_but_empty = {**outputs(**RS_FILTERS), "dashboard": "true"}
        for filters in (None, "[]", {}, broken, {**broken, "pg_db_files": "not json"}, true_but_empty):
            with self.subTest(filters=filters):
                self.assertVerdict(False, 0, [("M", RS, RS)], filters)


class IncludeScanTests(unittest.TestCase):
    """What rust_include_reads.scan says each include call reads."""

    def scan(self, body: str, path: str = "src/a/reader.rs", cargo=("",), symlinks=()) -> list:
        return includes.scan(path, textwrap.dedent(body), set(cargo), set(symlinks))

    def test_literal_and_manifest_paths_resolve_like_rustc(self) -> None:
        sites = self.scan('''
            // include_str!("comment.rs") and "include_str!(\\"string.rs\\")" are not calls.
            const A: &str = include_str!("same.rs");
            const B: &str = include_str!("../b/up.rs");
            const C: &[u8] = include_bytes!(r#"./raw.rs"#);
            const D: &str = std::include_str!(concat!("../", "joined.rs"));
            const E: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/rooted.rs"));
            const F: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/", file!()));
            include!(
                "multi\\
                 line.rs",
            );
            const G: &str = include_str!("../../../outside.rs");
        ''')
        self.assertEqual(
            [site.target for site in sites],
            ["src/a/same.rs", "src/b/up.rs", "src/a/raw.rs", "src/joined.rs",
             "src/rooted.rs", "src/a/reader.rs", "src/a/multiline.rs"],
        )
        self.assertEqual([site.line for site in sites][:2], [3, 4])
        nested = self.scan('include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/x.rs"));', "tools/t/src/lib.rs", ("", "tools/t"))
        self.assertEqual([site.target for site in nested], ["tools/t/x.rs"])

    def test_unresolvable_arguments_may_read_any_rust_file(self) -> None:
        for body in (
            'macro_rules! grab { ($p:expr) => { include_str!($p) }; }',
            'include_str!(concat!(env!("OUT_DIR"), "/gen.rs"));',
            'include_str!(PATH);',
            'include_str!("\\u{2e}\\u{2e}/x.rs");',
            'use std::include_str as grab;',
            'include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/", file!()));',  # file!() outside the root crate
            'include_str!("linked/data.json");',  # a symlinked directory may lead to any file
        ):
            with self.subTest(body=body):
                sites = self.scan(body, "tools/t/src/lib.rs", ("", "tools/t"), {"tools/t/src/linked"})
                self.assertEqual(len(sites), 1, sites)
                self.assertIsNone(sites[0].target)
                self.assertTrue(sites[0].reads("src/server/mod.rs"), sites[0])
        (json_site,) = self.scan('include_str!(concat!(env!("OUT_DIR"), "/data.json"));')
        self.assertFalse(json_site.reads("src/server/mod.rs"))
        self.assertFalse(json_site.reads("docs/guide.md"))


class GateProcessTests(unittest.TestCase):
    """Runs the gate the way the workflow step does, against a real git history."""

    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        for rel in (
            "scripts/check_comment_only_change.py",
            "scripts/rust_lex.py",
            "scripts/ci/comment_only_gate.py",
            "scripts/ci/rust_include_reads.py",
        ):
            (self.root / rel).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(REPO_ROOT / rel, self.root / rel)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "gate@example.invalid")
        self.git("config", "user.name", "gate")
        self.write("src/lib.rs", "// chatty\npub fn one() -> u32 {\n    1\n}\n")
        self.write("src/body.rs", "// body\npub fn two() -> u32 {\n    2\n}\n")
        self.write("src/reader.rs", 'const BODY: &str = include_str!("body.rs");\n')
        self.base = self.commit("seed")

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def git(self, *args: str) -> str:
        return subprocess.run(
            ["git", "-C", str(self.root), *args], check=True, capture_output=True, text=True
        ).stdout.strip()

    def write(self, rel: str, body: str) -> None:
        (self.root / rel).parent.mkdir(parents=True, exist_ok=True)
        (self.root / rel).write_text(body, encoding="utf-8")

    def commit(self, message: str) -> str:
        self.git("add", "-A")
        self.git("commit", "-q", "-m", message)
        return self.git("rev-parse", "HEAD")

    def run_gate(self, head: str, base: str | None = None, filters: str | None = None) -> tuple[subprocess.CompletedProcess[str], str]:
        out = self.root / "gh-output"
        out.write_text("", encoding="utf-8")
        env = {
            **os.environ,
            "PYTHONDONTWRITEBYTECODE": "1",
            "BASE_SHA": base or self.base,
            "HEAD_SHA": head,
            "FILTER_OUTPUTS": filters if filters is not None else json.dumps(outputs(rust_or_policy=["src/lib.rs"])),
            "GITHUB_OUTPUT": str(out),
            "GITHUB_STEP_SUMMARY": str(self.root / "gh-summary"),
        }
        result = subprocess.run(
            [sys.executable, str(self.root / "scripts/ci/comment_only_gate.py")],
            env=env, capture_output=True, text=True, check=False,
        )
        return result, out.read_text(encoding="utf-8")

    def test_comment_only_commit_writes_true_and_the_judge_verdict(self) -> None:
        self.write("src/lib.rs", "pub fn one() -> u32 {\n    1 // why\n}\n")
        self.write("docs/guide.md", "# guide\n")
        result, output = self.run_gate(self.commit("comment only"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output, "comment_only=true\nrust_tests_skip=true\n")
        summary = (self.root / "gh-summary").read_text(encoding="utf-8")
        self.assertIn("comment_only=true", summary)
        self.assertIn("scanned 1 include calls", summary)
        self.assertIn("OK: every changed Rust file is byte-identical", summary)

    def test_comment_edit_a_test_may_read_keeps_the_library_sweep(self) -> None:
        filters = json.dumps(outputs(rust_or_policy=["src/body.rs"]))
        self.write("src/body.rs", "// reworded\npub fn two() -> u32 {\n    2\n}\n")
        result, output = self.run_gate(self.commit("comment on an included file"), filters=filters)
        self.assertEqual(output, "comment_only=true\nrust_tests_skip=false\n")
        self.assertIn("src/body.rs is read by src/reader.rs:1 include_str! -> src/body.rs", result.stdout)

        # The call may exist only on the base side, when main added it after the PR branched.
        self.git("rm", "-q", "src/reader.rs")
        fork = self.commit("drop the reader")
        self.write("src/reader.rs", 'const BODY: &str = include_str!("body.rs");\n')
        base = self.commit("main adds the reader again")
        self.git("checkout", "-q", "-b", "pr", fork)
        self.write("src/body.rs", "// reworded twice\npub fn two() -> u32 {\n    2\n}\n")
        result, output = self.run_gate(self.commit("comment on body"), base=base, filters=filters)
        self.assertEqual(output, "comment_only=true\nrust_tests_skip=false\n", result.stdout)
        self.assertIn("src/body.rs is read by src/reader.rs:1", result.stdout)

    def test_call_inside_a_file_include_splices_is_seen(self) -> None:
        self.write("src/reader.rs", 'include!("calls.inc");\n')
        self.write("src/calls.inc", 'const BODY: &str = include_str!("body.rs");\n')
        self.base = self.commit("reader splices its calls")
        self.write("src/body.rs", "// reworded\npub fn two() -> u32 {\n    2\n}\n")
        result, output = self.run_gate(self.commit("comment on body"), filters=json.dumps(outputs(rust_or_policy=["src/body.rs"])))
        self.assertEqual(output, "comment_only=true\nrust_tests_skip=false\n")
        self.assertIn("src/body.rs is read by src/calls.inc:1 include_str!", result.stdout)

    def test_unresolved_include_keeps_the_library_sweep_and_a_scan_failure_writes_false(self) -> None:
        self.write("src/reader.rs", 'const BODY: &str = include_str!(concat!(env!("OUT_DIR"), "/gen.rs"));\n')
        self.base = self.commit("unresolvable include")
        self.write("src/lib.rs", "pub fn one() -> u32 {\n    1\n}\n")
        head = self.commit("comment only")
        result, output = self.run_gate(head)
        self.assertEqual(output, "comment_only=true\nrust_tests_skip=false\n")
        self.assertIn("unresolved, tail '/gen.rs'", result.stdout)

        (self.root / "scripts/ci/rust_include_reads.py").write_text("raise RuntimeError('scan exploded')\n", encoding="utf-8")
        result, output = self.run_gate(head)
        self.assertEqual(output, "comment_only=false\nrust_tests_skip=false\n")
        self.assertIn("include scan error: RuntimeError('scan exploded')", result.stdout)

    def test_line_break_the_judge_erases_inside_a_literal_writes_false(self) -> None:
        # The judge splits with str.splitlines() and rejoins with LF, so these edits compare equal.
        for old, new in (("\n", "\u2028"), ("\n", "\r"), ("\n", "\x85"), ("\u2028", "\n")):
            with self.subTest(old=old, new=new):
                self.write("src/lib.rs", f'const S: &str = "a{old}b";\n')
                base = self.commit(f"literal with {old!r}")
                self.write("src/lib.rs", f'const S: &str = "a{new}b";\n')
                result, output = self.run_gate(self.commit(f"literal with {new!r}"), base=base)
                self.assertEqual(output, "comment_only=false\nrust_tests_skip=false\n", result.stdout)
                self.assertIn("other than LF/CRLF", result.stdout)

    def test_crlf_comment_edit_still_writes_true(self) -> None:
        self.git("config", "core.autocrlf", "false")
        (self.root / "src/lib.rs").write_bytes(b'const S: &str = "a\r\nb";\r\npub fn one() -> u32 {\r\n    1\r\n}\r\n')
        base = self.commit("crlf source")
        (self.root / "src/lib.rs").write_bytes(b'const S: &str = "a\r\nb";\r\npub fn one() -> u32 {\r\n    1 // why\r\n}\r\n')
        result, output = self.run_gate(self.commit("crlf comment"), base=base)
        self.assertEqual(output, "comment_only=true\nrust_tests_skip=true\n", result.stdout)

    def test_code_change_writes_false_with_the_judge_finding(self) -> None:
        self.write("src/lib.rs", "// chatty\npub fn one() -> u32 {\n    2\n}\n")
        result, output = self.run_gate(self.commit("code"))
        self.assertEqual(output, "comment_only=false\nrust_tests_skip=false\n")
        self.assertIn("code differs after comment removal", result.stdout)

    def test_unknown_base_writes_false(self) -> None:
        self.write("src/lib.rs", "pub fn one() -> u32 {\n    1\n}\n")
        head = self.commit("comment only")
        for base in ("0" * 40, "not-a-sha"):
            with self.subTest(base=base):
                _result, output = self.run_gate(head, base=base)
                self.assertEqual(output, "comment_only=false\nrust_tests_skip=false\n")

    def test_broken_judge_or_filter_input_writes_false(self) -> None:
        self.write("src/lib.rs", "pub fn one() -> u32 {\n    1\n}\n")
        head = self.commit("comment only")
        _result, output = self.run_gate(head, filters="{not json")
        self.assertEqual(output, "comment_only=false\nrust_tests_skip=false\n")
        (self.root / "scripts/check_comment_only_change.py").write_text(
            "raise RuntimeError('judge exploded')\n", encoding="utf-8"
        )
        result, output = self.run_gate(head)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output, "comment_only=false\nrust_tests_skip=false\n")
        self.assertIn("judge exploded", result.stdout)


TOKEN = re.compile(r"\s*(?:(\|\||&&|==|!=|!|\(|\))|'((?:[^']|'')*)'|([A-Za-z_][\w.-]*))")


def evaluate(expr: str, lookup) -> object:
    """The subset of GitHub expression syntax ci-pr.yml uses in if/outputs/env."""
    tokens: list[tuple[str, str]] = []
    pos, expr = 0, expr.strip()
    while pos < len(expr):
        match = TOKEN.match(expr, pos)
        if not match:
            raise AssertionError(f"unsupported expression syntax at {expr[pos:]!r}")
        op, text, ident = match.groups()
        tokens.append(("op", op) if op else ("str", text.replace("''", "'")) if text is not None else ("id", ident))
        pos = match.end()
    tokens.append(("end", ""))
    index = 0

    def take() -> tuple[str, str]:
        nonlocal index
        index += 1
        return tokens[index - 1]

    def primary() -> object:
        kind, value = take()
        if (kind, value) == ("op", "!"):
            return not truthy(primary())
        if (kind, value) == ("op", "("):
            inner = disjunction()
            assert take() == ("op", ")"), expr
            return inner
        if kind == "str":
            return value
        if kind == "id" and value in ("true", "false"):
            return value == "true"
        if kind == "id" and tokens[index] == ("op", "("):
            assert value in ("always", "cancelled") and take() == ("op", "(") and take() == ("op", ")"), expr
            return value == "always" or truthy(lookup("cancelled()"))
        assert kind == "id", expr
        return lookup(value)

    def comparison() -> object:
        left = primary()
        while tokens[index] in (("op", "=="), ("op", "!=")):
            equal = take()[1] == "=="
            right = primary()
            same = str(left).lower() == str(right).lower()
            left = same if equal else not same
        return left

    def conjunction() -> object:
        left = comparison()
        while tokens[index] == ("op", "&&"):
            take()
            right = comparison()
            left = right if truthy(left) else left
        return left

    def disjunction() -> object:
        left = conjunction()
        while tokens[index] == ("op", "||"):
            take()
            right = conjunction()
            left = left if truthy(left) else right
        return left

    result = disjunction()
    assert tokens[index][0] == "end", expr
    return result


def truthy(value: object) -> bool:
    return value not in (None, "", False, 0)


def render(value: object, lookup) -> str:
    """An env/output value after `${{ }}` substitution, as a string."""
    if isinstance(value, bool):
        return "true" if value else "false"
    text = str(value)
    whole = re.fullmatch(r"\$\{\{(.*)\}\}", text.strip(), re.S)
    if whole:
        result = evaluate(whole.group(1), lookup)
        return ("true" if result else "false") if isinstance(result, bool) else ("" if result is None else str(result))
    assert "${{" not in text, text
    return text


def mirror_steps(job: dict) -> list[dict]:
    return [s for s in job.get("steps", []) if s.get("run") == "./scripts/required-check-mirror.sh"]


class WorkflowSimulation:
    """Evaluates one ci-pr.yml run from the filter and gate step outputs."""

    def __init__(
        self,
        filters: dict[str, str],
        comment_only: str | None,
        forced: dict[str, str] | None = None,
        rust_tests_skip: str | None = None,
        gate_outcome: str = "success",
        run_cancelled: bool = False,
    ):
        self.run_cancelled = run_cancelled
        self.jobs = yaml.safe_load(PR_WORKFLOW.read_text(encoding="utf-8"))["jobs"]
        gate_outputs = {"comment_only": comment_only, "rust_tests_skip": rust_tests_skip}
        steps = {"filter": filters, "comment_only": {k: v for k, v in gate_outputs.items() if v is not None}}
        outcomes = {"filter": "success", "comment_only": gate_outcome}

        def step_lookup(name: str) -> object:
            parts = name.split(".")
            if parts[2:] == ["outcome"]:
                return outcomes[parts[1]]
            _steps, step, _outputs, key = parts
            return steps[step].get(key, "")

        self.changes = {key: render(value, step_lookup) for key, value in self.jobs["changes"]["outputs"].items()}
        self.results = {"changes": "success"}
        for job_id, job in self.jobs.items():
            if job_id != "changes" and not mirror_steps(job):
                runs = "if" not in job or truthy(evaluate(job["if"], self.lookup))
                self.results[job_id] = "success" if runs else "skipped"
        self.results.update(forced or {})

    def lookup(self, name: str) -> object:
        if name == "cancelled()":
            return self.run_cancelled
        parts = name.split(".")
        assert parts[0] == "needs", name
        if parts[2] == "outputs":
            assert parts[1] == "changes", name
            return self.changes.get(parts[3], "")
        return self.results[parts[1]]

    def mirror_runs(self) -> dict[str, list[subprocess.CompletedProcess[str]]]:
        """Every job that publishes through required-check-mirror.sh, keyed by its context name."""
        published: dict[str, list[subprocess.CompletedProcess[str]]] = {}
        for job in self.jobs.values():
            if not mirror_steps(job) or not self.job_runs(job):
                continue
            runs = published.setdefault(job["name"], [])
            for step in mirror_steps(job):
                if "if" in step and not truthy(evaluate(render(step["if"], self.lookup), self.lookup)):
                    continue
                env = {key: render(value, self.lookup) for key, value in step["env"].items()}
                runs.append(subprocess.run(
                    ["bash", str(MIRROR)], env={"PATH": os.environ["PATH"], **env},
                    capture_output=True, text=True, check=False,
                ))
        return published

    def job_runs(self, job: dict) -> bool:
        """A job-level `if:`; one without a status function implies success(), as in Actions."""
        whole = re.fullmatch(r"\$\{\{(.*)\}\}", str(job.get("if", "")).strip(), re.S)
        condition = (whole.group(1) if whole else str(job.get("if", ""))).strip()
        if re.search(r"\b(?:always|cancelled)\(", condition):
            return truthy(evaluate(condition, self.lookup))
        needs = job.get("needs", [])
        needs = [needs] if isinstance(needs, str) else needs
        succeeded = not self.run_cancelled and all(self.results[name] == "success" for name in needs)
        return succeeded and (not condition or truthy(evaluate(condition, self.lookup)))


# Raw filters for a comment edit that selects every heavy lane.
HEAVY_RAW = outputs(
    dashboard=[], high_risk_recovery=[RS], pg_db=[RS], rust_or_policy=[RS], rust_compile=[RS],
    relay_contract=[], cross_os_rust=[RS], win32_build_token=[],
)
OVERRIDDEN = {"pg_db": "pg_db", "high_risk_recovery": "high_risk_recovery", "cross_os_rust": "cross_os_rust", "rust_tests": "rust_or_policy"}


class WorkflowWiringTests(unittest.TestCase):
    def assertPublishedGreen(self, run: WorkflowSimulation) -> None:
        published = run.mirror_runs()
        for context in REQUIRED_CONTEXTS:
            with self.subTest(context=context):
                self.assertTrue(published.get(context), f"{context} ran no mirror step")
                for result in published[context]:
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_comment_only_skips_heavy_lanes_and_every_required_context_is_green(self) -> None:
        for relay_contract in ([], ["src/services/discord/inflight/store.rs"]):
            raw = {**HEAVY_RAW, **outputs(relay_contract=relay_contract)}
            run = WorkflowSimulation(raw, "true", rust_tests_skip="true")
            with self.subTest(relay_contract=bool(relay_contract)):
                for job_id in SKIPPED_WHEN_COMMENT_ONLY:
                    self.assertEqual(run.results[job_id], "skipped", job_id)
                for job_id in KEPT_WHEN_COMMENT_ONLY:
                    self.assertEqual(run.results[job_id], "success", job_id)
                self.assertEqual(run.changes["comment_only"], "true")
                self.assertPublishedGreen(run)

    def test_include_reader_runs_only_the_library_sweep(self) -> None:
        for skip in ("false", None, ""):
            run = WorkflowSimulation(HEAVY_RAW, "true", rust_tests_skip=skip)
            with self.subTest(rust_tests_skip=skip):
                self.assertEqual(run.changes["rust_tests"], HEAVY_RAW["rust_or_policy"])
                self.assertEqual(run.results["library_sweep"], "success")
                for job_id in SKIPPED_WHEN_COMMENT_ONLY[:-1]:
                    self.assertEqual(run.results[job_id], "skipped", job_id)
                self.assertPublishedGreen(run)

    def test_gate_false_unset_or_failed_keeps_the_raw_filters(self) -> None:
        # A step that failed after writing both outputs still reads as the full run.
        for outcome, gate_output in (("success", "false"), ("success", None), ("success", ""), ("failure", "true")):
            run = WorkflowSimulation(HEAVY_RAW, gate_output, rust_tests_skip="true", gate_outcome=outcome)
            with self.subTest(outcome=outcome, gate_output=gate_output):
                self.assertEqual(run.changes["comment_only"], "false")
                for output, raw_filter in OVERRIDDEN.items():
                    self.assertEqual(run.changes[output], HEAVY_RAW[raw_filter], output)
                for job_id in SKIPPED_WHEN_COMMENT_ONLY:
                    self.assertEqual(run.results[job_id], "success", job_id)
                self.assertPublishedGreen(run)

    def test_heavy_mirrors_still_fail_closed_when_not_comment_only(self) -> None:
        contexts = {
            "test_fast": "Fast targeted tests (ubuntu-latest)",
            "high-risk-recovery": "High-risk recovery",
            "check_fast_cross_os": "Fast check cross OS required context (ubuntu-latest)",
            "check_fast_cross_os_targets": "Fast check cross OS required context (ubuntu-latest)",
            "library_sweep": "Library test sweep (ubuntu-latest)",
        }
        for job_id, context in contexts.items():
            with self.subTest(job=job_id):
                run = WorkflowSimulation(HEAVY_RAW, "false", forced={job_id: "skipped"})
                codes = [result.returncode for result in run.mirror_runs()[context]]
                self.assertIn(1, codes, f"{context} passed with {job_id} skipped while its filter is true")
        kept = WorkflowSimulation(HEAVY_RAW, "true", forced={"library_sweep": "skipped"}, rust_tests_skip="false")
        codes = [result.returncode for result in kept.mirror_runs()["Library test sweep (ubuntu-latest)"]]
        self.assertIn(1, codes, "library sweep kept for an include reader passed while skipped")

    def test_only_a_cancelled_run_stops_the_required_context_mirrors(self) -> None:
        # A condition-skipped required job counts as passing, and a job timeout reads as `cancelled`.
        jobs = yaml.safe_load(PR_WORKFLOW.read_text(encoding="utf-8"))["jobs"]
        needs = {job["name"]: [job["needs"]] if isinstance(job["needs"], str) else job["needs"]
                 for job in jobs.values() if mirror_steps(job)}
        self.assertLessEqual(set(REQUIRED_CONTEXTS), set(needs))
        for upstream in sorted({name for names in needs.values() for name in names}):
            for outcome in ("failure", "skipped", "cancelled"):
                with self.subTest(upstream=upstream, outcome=outcome):
                    live = WorkflowSimulation(HEAVY_RAW, "false", forced={upstream: outcome}).mirror_runs()
                    self.assertEqual(set(live), set(needs), "a live run skipped a required-context mirror")
                    for context, names in needs.items():
                        if upstream in names and outcome != "skipped":
                            self.assertIn(1, [run.returncode for run in live[context]], context)
            with self.subTest(upstream=upstream, run="cancelled"):
                cancelled = WorkflowSimulation(HEAVY_RAW, "false", forced={upstream: "cancelled"}, run_cancelled=True)
                self.assertEqual(cancelled.mirror_runs(), {}, "a cancelled run still published a mirror")


if __name__ == "__main__":
    unittest.main()
