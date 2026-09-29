"""Comment-only CI skip: scripts/ci/comment_only_gate.py and its ci-pr.yml wiring.

A false ``comment_only=true`` skips the PostgreSQL, high-risk, Windows and
library-sweep lanes for real code, so the gate cases below pin the refusals.
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

import comment_only_gate as gate  # noqa: E402

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


class GateProcessTests(unittest.TestCase):
    """Runs the gate the way the workflow step does, against a real git history."""

    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        for rel in ("scripts/check_comment_only_change.py", "scripts/rust_lex.py", "scripts/ci/comment_only_gate.py"):
            (self.root / rel).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(REPO_ROOT / rel, self.root / rel)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "gate@example.invalid")
        self.git("config", "user.name", "gate")
        self.write("src/lib.rs", "// chatty\npub fn one() -> u32 {\n    1\n}\n")
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
        self.assertEqual(output, "comment_only=true\n")
        summary = (self.root / "gh-summary").read_text(encoding="utf-8")
        self.assertIn("comment_only=true", summary)
        self.assertIn("OK: every changed Rust file is byte-identical", summary)

    def test_code_change_writes_false_with_the_judge_finding(self) -> None:
        self.write("src/lib.rs", "// chatty\npub fn one() -> u32 {\n    2\n}\n")
        result, output = self.run_gate(self.commit("code"))
        self.assertEqual(output, "comment_only=false\n")
        self.assertIn("code differs after comment removal", result.stdout)

    def test_unknown_base_writes_false(self) -> None:
        self.write("src/lib.rs", "pub fn one() -> u32 {\n    1\n}\n")
        head = self.commit("comment only")
        for base in ("0" * 40, "not-a-sha"):
            with self.subTest(base=base):
                _result, output = self.run_gate(head, base=base)
                self.assertEqual(output, "comment_only=false\n")

    def test_broken_judge_or_filter_input_writes_false(self) -> None:
        self.write("src/lib.rs", "pub fn one() -> u32 {\n    1\n}\n")
        head = self.commit("comment only")
        _result, output = self.run_gate(head, filters="{not json")
        self.assertEqual(output, "comment_only=false\n")
        (self.root / "scripts/check_comment_only_change.py").write_text(
            "raise RuntimeError('judge exploded')\n", encoding="utf-8"
        )
        result, output = self.run_gate(head)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output, "comment_only=false\n")
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
            assert value == "always" and take() == ("op", "(") and take() == ("op", ")"), expr
            return True
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


class WorkflowSimulation:
    """Evaluates one ci-pr.yml run from the filter and gate step outputs."""

    def __init__(self, filters: dict[str, str], comment_only: str | None, forced: dict[str, str] | None = None):
        self.jobs = yaml.safe_load(PR_WORKFLOW.read_text(encoding="utf-8"))["jobs"]
        steps = {"filter": filters, "comment_only": {} if comment_only is None else {"comment_only": comment_only}}

        def step_lookup(name: str) -> object:
            _steps, step, _outputs, key = name.split(".")
            return steps[step].get(key, "")

        self.changes = {key: render(value, step_lookup) for key, value in self.jobs["changes"]["outputs"].items()}
        self.results = {"changes": "success"}
        for job_id, job in self.jobs.items():
            if job_id != "changes" and "always()" not in str(job.get("if", "")):
                runs = "if" not in job or truthy(evaluate(job["if"], self.lookup))
                self.results[job_id] = "success" if runs else "skipped"
        self.results.update(forced or {})

    def lookup(self, name: str) -> object:
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
            mirror_steps = [s for s in job.get("steps", []) if s.get("run") == "./scripts/required-check-mirror.sh"]
            if not mirror_steps:
                continue
            runs = published.setdefault(job["name"], [])
            for step in mirror_steps:
                if "if" in step and not truthy(evaluate(render(step["if"], self.lookup), self.lookup)):
                    continue
                env = {key: render(value, self.lookup) for key, value in step["env"].items()}
                runs.append(subprocess.run(
                    ["bash", str(MIRROR)], env={"PATH": os.environ["PATH"], **env},
                    capture_output=True, text=True, check=False,
                ))
        return published


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
            run = WorkflowSimulation(raw, "true")
            with self.subTest(relay_contract=bool(relay_contract)):
                for job_id in SKIPPED_WHEN_COMMENT_ONLY:
                    self.assertEqual(run.results[job_id], "skipped", job_id)
                for job_id in KEPT_WHEN_COMMENT_ONLY:
                    self.assertEqual(run.results[job_id], "success", job_id)
                self.assertEqual(run.changes["comment_only"], "true")
                self.assertPublishedGreen(run)

    def test_gate_false_or_unset_keeps_the_raw_filters(self) -> None:
        for gate_output in ("false", None, ""):
            run = WorkflowSimulation(HEAVY_RAW, gate_output)
            with self.subTest(gate_output=gate_output):
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


if __name__ == "__main__":
    unittest.main()
