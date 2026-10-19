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
    "lib_test_prepare",
    "lint",
    "relay_authority_targets",
    "relay_authority_mutations",
    "test_fast",
    "high-risk-recovery",
    "check_fast_cross_os",
    "check_fast_cross_os_targets",
    "library_sweep",
)
KEPT_WHEN_COMMENT_ONLY = ("scripts", "scripts_guards", "scripts_contracts")
KEPT_FOR_INCLUDED_RUST = {"lib_test_prepare", "lint", "relay_authority_targets", "relay_authority_mutations", "library_sweep"}
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
    lists = {**dict.fromkeys(("dashboard", "high_risk_recovery", "pg_db", "rust_or_policy",
                            "rust_compile", "relay_contract", "cross_os_rust", "win32_build_token"), []), **lists}
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

    def test_docs_only_changes_pass_including_filtered_markdown(self) -> None:
        for status in ("A", "M", "D", "R", "C"):
            with self.subTest(status=status):
                self.assertVerdict(True, 0, [(status, "README.md", "README.md")], outputs(rust_compile=["README.md"]))

    def test_non_markdown_file_refuses_even_when_no_filter_selects_it(self) -> None:
        entries = [("M", RS, RS), ("A", "docs/diagram.svg", "docs/diagram.svg")]
        self.assertVerdict(False, 0, entries, outputs(**RS_FILTERS), "non-Rust, non-Markdown")

    def test_workflow_file_refuses(self) -> None:
        wf = ".github/workflows/ci-pr.yml"
        filters = {**RS_FILTERS, "high_risk_recovery": [wf], "rust_or_policy": [RS, wf]}
        self.assertVerdict(False, 0, [("M", RS, RS), ("M", wf, wf)], outputs(**filters), "protected")

    def test_markdown_that_selects_a_filter_refuses(self) -> None:
        doc = "docs/relay-state-contract.md"
        for name in ("relay_contract", "dashboard", "win32_build_token"):
            filters = {**RS_FILTERS, name: [doc]}
            self.assertVerdict(False, 0, [("M", RS, RS), ("M", doc, doc)], outputs(**filters), doc)

    def test_protected_paths_refuse_even_when_the_filter_misses_them(self) -> None:
        for path in (".github/README.md", "scripts/README.md", "policies/a.yaml", "migrations/notes.md", "Cargo.md", "tools/build.rs"):
            with self.subTest(path=path):
                self.assertVerdict(False, 0, [("M", path, path)], outputs(), "protected")

    def test_yaml_requires_an_in_place_modification(self) -> None:
        for status in ("A", "D", "R", "C", "T"):
            with self.subTest(status=status):
                self.assertVerdict(False, 0, [(status, "tests/a.yaml", "tests/a.yaml")], outputs(), "in-place")

    def test_empty_diff_and_missing_protected_filter_refuse(self) -> None:
        self.assertVerdict(False, 0, [], outputs())
        missing = outputs(**RS_FILTERS)
        del missing["relay_contract"]
        self.assertVerdict(False, 0, [("M", RS, RS)], missing)

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


class YamlTokenTests(unittest.TestCase):
    def test_comments_and_spaces_preserve_all_token_attributes(self) -> None:
        for before, after in (
            ("id: E-1\n", "# note\nid: E-1  # more\n"),
            ("a: &name [1, 2]\nb: *name\n", "a: &name [1,  2] # note\nb: *name\n"),
            ("---\nx: 1\n---\nx: 2\n", "---\nx: 1 # note\n---\nx: 2\n"),
            ("x: 1\nx: 2\n", "x: 1\nx: 2 # duplicate unchanged\n"),
        ):
            with self.subTest(before=before):
                self.assertEqual(gate.yaml_tokens(before.encode()), gate.yaml_tokens(after.encode()))

    def test_values_scalar_styles_tags_anchors_and_all_documents_are_significant(self) -> None:
        for before, after in (
            ("x: true\n", "x: 1\n"),
            ("x: 1\n", "x: 1.0\n"),
            ("x: 1\n", 'x: "1"\n'),
            ('x: "value"\n', "x: 'value'\n"),
            ("x: |\n  # content\n", "x: |\n  # changed content\n"),
            ("x: |\n  words\n", "x: >\n  words\n"),
            ("x: !!str 1\n", "x: 1\n"),
            ("x: &one 1\ny: *one\n", "x: &two 1\ny: *two\n"),
            ("---\nx: 1\n---\nx: 2\n", "---\nx: 1\n---\nx: 3\n"),
            ("x: 1\nx: 2\n", "x: 2\nx: 1\n"),
        ):
            with self.subTest(before=before, after=after):
                self.assertNotEqual(gate.yaml_tokens(before.encode()), gate.yaml_tokens(after.encode()))

    def test_invalid_syntax_aliases_and_encoding_refuse(self) -> None:
        for raw in (b"x: [1\n", b"x: *undefined\n", b"x: \xff\n", b"x: [1}\n"):
            with self.subTest(raw=raw), self.assertRaises(Exception):
                gate.yaml_tokens(raw)


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
            "scripts/ci/rust_runtime_reads.py",
        ):
            (self.root / rel).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(REPO_ROOT / rel, self.root / rel)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "gate@example.invalid")
        self.git("config", "user.name", "gate")
        (self.root / ".git/info/exclude").write_text("gh-output\ngh-summary\n")
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
        self.assertIn("yaml_available=true version=", summary)

    def test_docs_and_36_yaml_comments_without_rust_skip(self) -> None:
        paths = [f"tests/e2e/tui_relay/scenarios/E-{i}.yaml" for i in range(1, 37)]
        for path in paths:
            self.write(path, "coverage_class: live\n")
        self.base = self.commit("scenario seed")
        for path in paths:
            self.write(path, "# manual procedure\ncoverage_class: live\n")
        docs = ["docs/e2e/manual.md", "docs/runbook.md", "tests/e2e/tui_relay/scenarios/README.md"]
        for path in docs:
            self.write(path, "# Manual procedure\n")
        filters = json.dumps(outputs(rust_or_policy=paths + docs[-1:], rust_compile=paths + docs[-1:]))
        result, output = self.run_gate(self.commit("docs and YAML comments"), filters=filters)
        self.assertEqual(output, "comment_only=true\nrust_tests_skip=true\n", result.stdout + result.stderr)

    def test_yaml_value_block_scalar_and_invalid_parse_keep_full_run(self) -> None:
        path = "tests/scenario.yaml"
        for before, after in (("x: true\n", "x: 1\n"),
                              ("x: |\n  # text\n", "x: |\n  # other\n"),
                              ("x: [1\n", "# comment\nx: [1\n")):
            with self.subTest(before=before):
                self.write(path, before)
                base = self.commit("YAML seed")
                self.write(path, after)
                result, output = self.run_gate(self.commit("YAML edit"), base=base,
                                               filters=json.dumps(outputs(rust_compile=[path])))
                self.assertEqual(output, "comment_only=false\nrust_tests_skip=false\n", result.stdout)

    def test_yaml_rename_and_delete_cannot_use_parse_only_exception(self) -> None:
        old, new = "tests/old.yaml", "tests/new.yaml"
        self.write(old, "id: unchanged\n")
        self.base = self.commit("YAML seed")
        self.git("mv", old, new)
        result, output = self.run_gate(self.commit("rename YAML"), filters=json.dumps(outputs(rust_compile=[old, new])))
        self.assertEqual(output, "comment_only=false\nrust_tests_skip=false\n", result.stdout)
        base = self.git("rev-parse", "HEAD")
        self.git("rm", new)
        result, output = self.run_gate(self.commit("delete YAML"), base=base, filters=json.dumps(outputs(rust_compile=[new])))
        self.assertEqual(output, "comment_only=false\nrust_tests_skip=false\n", result.stdout)

    def test_unlisted_runtime_yaml_reader_keeps_full_run(self) -> None:
        path = "tests/scenario.yaml"
        self.write(path, "id: unchanged\n")
        self.write("src/raw_reader.rs", f'fn reader() {{ std::fs::read_to_string("{path}"); }}\n')
        self.base = self.commit("raw YAML reader")
        self.write(path, "# comment\nid: unchanged\n")
        result, output = self.run_gate(self.commit("YAML comment"), filters=json.dumps(outputs(rust_compile=[path])))
        self.assertEqual(output, "comment_only=false\nrust_tests_skip=false\n", result.stdout)
        self.assertIn("runtime reader", result.stdout)

    def test_parse_only_reader_allows_yaml_but_source_pin_change_refuses(self) -> None:
        import rust_runtime_reads as runtime

        path = "tests/e2e/tui_relay/scenarios/a.yaml"
        self.write(path, "id: unchanged\n")
        reader = runtime.SCENARIO_READER
        self.write(reader, (REPO_ROOT / reader).read_text())
        self.base = self.commit("parse-only YAML reader")
        self.write(path, "# comment\nid: unchanged\n")
        head = self.commit("YAML comment")
        filters = json.dumps(outputs(rust_compile=[path]))
        result, output = self.run_gate(head, filters=filters)
        self.assertEqual(output, "comment_only=true\nrust_tests_skip=true\n", result.stdout)
        self.write(reader, (REPO_ROOT / reader).read_text() + "\n// reader source changed\n")
        result, output = self.run_gate(self.commit("reader pin drift"), filters=filters)
        self.assertEqual(output, "comment_only=false\nrust_tests_skip=false\n", result.stdout)
        self.assertIn("source pin mismatch", result.stdout)

    def test_included_markdown_keeps_full_run(self) -> None:
        path = "docs/data.md"
        self.write(path, "# Before\n")
        self.write("src/raw_reader.rs", f'const DOC: &str = include_str!("../{path}");\n')
        self.base = self.commit("included Markdown")
        self.write(path, "# After\n")
        result, output = self.run_gate(self.commit("Markdown edit"), filters=json.dumps(outputs()))
        self.assertEqual(output, "comment_only=false\nrust_tests_skip=false\n", result.stdout)
        self.assertIn("include_str!", result.stdout)

    def test_missing_yaml_module_and_runtime_scan_failure_keep_full_run(self) -> None:
        self.write("docs/guide.md", "# guide\n")
        head = self.commit("documentation")
        # A module placed next to the gate simulates an interpreter without PyYAML.
        (self.root / "scripts/ci/yaml.py").write_text("raise ImportError('unavailable')\n")
        result, output = self.run_gate(head, filters=json.dumps(outputs()))
        self.assertEqual(output, "comment_only=false\nrust_tests_skip=false\n", result.stdout)
        self.assertIn("yaml_available=false version=unavailable", result.stdout)
        (self.root / "scripts/ci/yaml.py").unlink()
        (self.root / "scripts/ci/rust_runtime_reads.py").write_text("raise RuntimeError('runtime scan exploded')\n")
        result, output = self.run_gate(head, filters=json.dumps(outputs()))
        self.assertEqual(output, "comment_only=false\nrust_tests_skip=false\n", result.stdout)
        self.assertIn("runtime reader scan error", result.stdout)

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
FORCE_FULL = "needs.pg_shards.outputs.force_full == 'true'"


def cancelled_path_gate(expr: str, lookup) -> bool:
    """Only the status-first cancellation guard and one path output, which the selector's
    kill switch may open in parentheses, are shared gates."""
    def unparen(value: str) -> str:
        value = value.strip()
        while value.startswith("(") and value.endswith(")"):
            depth = 0
            for index, char in enumerate(value):
                depth += (char == "(") - (char == ")")
                if depth == 0:
                    break
            if depth or index != len(value) - 1:
                break
            value = value[1:-1].strip()
        return value

    terms = unparen(expr).split("&&")
    status = unparen(terms[0])
    assert len(terms) == 2 and status.startswith("!"), expr
    assert re.fullmatch(r"cancelled\s*\(\s*\)", unparen(status[1:])), expr
    path, *opened = unparen(terms[1]).split("||")
    assert not opened or terms[1].strip().startswith("(") and [o.strip() for o in opened] == [FORCE_FULL], expr
    output = re.fullmatch(r"needs\.changes\.outputs\.([A-Za-z0-9_-]+)\s*(==|!=)\s*'true'", unparen(path))
    assert output, expr
    forced = bool(opened) and str(lookup("needs.pg_shards.outputs.force_full")).lower() == "true"
    selected = str(lookup("needs.changes.outputs." + output.group(1))).lower() == "true"
    if output.group(2) == "!=":
        selected = not selected
    return not truthy(lookup("cancelled()")) and (selected or forced)


def evaluate(expr: str, lookup) -> object:
    """The subset of GitHub expression syntax ci-pr.yml uses in if/outputs/env."""
    expr = expr.strip()
    whole = re.fullmatch(r"\$\{\{(.*)\}\}", expr, re.S)
    if whole:
        expr = whole.group(1).strip()
    guarded_needs = re.search(r"\b[A-Za-z_][\w]*\s*\(", expr) and re.search(r"\bneeds\.", expr)
    if guarded_needs:
        return cancelled_path_gate(expr, lookup)
    tokens: list[tuple[str, str]] = []
    pos = 0
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
        force_full: str = "false",
    ):
        self.run_cancelled = run_cancelled
        self.force_full = force_full
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
        if parts[2] == "outputs" and parts[1] == "pg_shards":
            assert parts[3] == "force_full", name
            return self.force_full
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
        if re.search(r"\b(?:always|cancelled)\s*\(", condition):
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


class CancelledPathExpressionTests(unittest.TestCase):
    def test_cancelled_path_gate_tracks_only_run_cancellation_and_path_truth(self) -> None:
        output = "needs.changes.outputs.pg_db == 'true'"
        variants = (
            "${{ !cancelled() && " + output + " }}",
            "!cancelled() && " + output,
            "${{ ((!cancelled()) && (" + output + ")) }}",
            "${{ ( ! ( cancelled ( ) ) ) && ((" + output + ")) }}",
            "${{\n (! cancelled ( )) && (needs.changes.outputs.pg_db=='true')\n }}",
        )
        for cancelled in (False, True):
            for selected in ("true", "false", "", None):
                def lookup(name):
                    self.assertIn(name, ("cancelled()", "needs.changes.outputs.pg_db"))
                    return cancelled if name == "cancelled()" else selected
                for expr in variants:
                    with self.subTest(cancelled=cancelled, selected=selected, expression=expr):
                        self.assertEqual(evaluate(expr, lookup), not cancelled and selected == "true")

    def test_unsupported_status_path_expressions_remain_assertions(self) -> None:
        output = "needs.changes.outputs.pg_db == 'true'"
        for expr in (
            "always() && " + output,
            "!always() && " + output,
            "!success() && " + output,
            "!other() && " + output,
            "!cancelled() && needs.other.outputs.pg_db == 'true'",
            "!cancelled() && " + output + " && " + output,
            "!cancelled() && " + output + " || needs.changes.outputs.rust_tests == 'true'",
            "!cancelled() && " + output + " || " + FORCE_FULL,
            "!cancelled() && (" + output + " || needs.other.outputs.force_full == 'true')",
            "!cancelled() && (" + output + " || " + FORCE_FULL + " || " + output + ")",
            "(!cancelled() || always()) && " + output,
            output + " && !cancelled()",
            "!cancelled(false) && " + output,
            "(!cancelled() && " + output,
        ):
            with self.subTest(expression=expr), self.assertRaises(AssertionError):
                evaluate("${{ " + expr + " }}", lambda name: False if name == "cancelled()" else "true")

    def test_existing_output_boolean_parser_and_standalone_status_are_preserved(self) -> None:
        lookup = lambda name: False if name == "cancelled()" else "true"
        self.assertTrue(evaluate("needs.changes.outputs.rust_compile == 'true' && needs.changes.outputs.cross_os_rust == 'true'", lookup))
        self.assertEqual(evaluate("steps.comment_only.outcome == 'true' && 'false' || 'true'", lookup), "false")
        self.assertTrue(evaluate("${{ !cancelled() }}", lookup))
        self.assertTrue(evaluate("always()", lookup))
        self.assertTrue(evaluate("always() && runner.os != 'macOS'", lookup))

    def test_spaced_status_guard_bypasses_failed_prepare_but_respects_cancellation(self) -> None:
        run = WorkflowSimulation(HEAVY_RAW, "false", forced={"lib_test_prepare": "failure"})
        job = {"needs": ["changes", "lib_test_prepare"], "if": "${{ ! (cancelled ( )) && needs.changes.outputs.pg_db == 'true' }}"}
        self.assertTrue(run.job_runs(job))
        run.run_cancelled = True
        self.assertFalse(run.job_runs(job))

    def test_removing_wrapper_recognition_makes_named_workflow_tests_red(self) -> None:
        source = Path(__file__).read_text(encoding="utf-8")
        old = '    if whole:\n        expr = whole.group(1).strip()'
        self.assertEqual(source.count(old), 1)
        namespace = {"__name__": "comment_only_evaluator_mutant", "__file__": __file__}
        exec(compile(source.replace(old, '    if False and whole:\n        expr = whole.group(1).strip()'), __file__, "exec"), namespace)
        names = (
            "test_comment_only_skips_heavy_lanes_and_every_required_context_is_green",
            "test_gate_false_unset_or_failed_keeps_the_raw_filters",
        )
        suite = unittest.TestSuite(namespace["WorkflowWiringTests"](name) for name in names)
        result = unittest.TestResult()
        suite.run(result)
        self.assertFalse(result.wasSuccessful())
        self.assertEqual(result.errors, [])
        for name in names:
            self.assertTrue(any(name in test.id() for test, _ in result.failures), name)


class WorkflowWiringTests(unittest.TestCase):
    def assertPublishedGreen(self, run: WorkflowSimulation) -> None:
        published = run.mirror_runs()
        for context in REQUIRED_CONTEXTS:
            with self.subTest(context=context):
                self.assertTrue(published.get(context), f"{context} ran no mirror step")
                for result in published[context]:
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_comment_only_skips_heavy_lanes_and_every_required_context_is_green(self) -> None:
        for raw in (HEAVY_RAW, outputs(rust_or_policy=["tests/scenario.yaml", "tests/README.md"],
                                      rust_compile=["tests/scenario.yaml", "tests/README.md"])):
            run = WorkflowSimulation(raw, "true", rust_tests_skip="true")
            with self.subTest(filters=raw):
                for job_id in SKIPPED_WHEN_COMMENT_ONLY:
                    self.assertEqual(run.results[job_id], "skipped", job_id)
                for job_id in KEPT_WHEN_COMMENT_ONLY:
                    self.assertEqual(run.results[job_id], "success", job_id)
                self.assertEqual(run.changes["comment_only"], "true")
                self.assertEqual(run.changes["rust_compile"], "false")
                self.assertEqual(run.changes["rust_or_policy"], "false")
                self.assertPublishedGreen(run)

    def test_include_reader_runs_only_the_library_sweep(self) -> None:
        for skip in ("false", None, ""):
            run = WorkflowSimulation(HEAVY_RAW, "true", rust_tests_skip=skip)
            with self.subTest(rust_tests_skip=skip):
                self.assertEqual(run.changes["rust_tests"], HEAVY_RAW["rust_or_policy"])
                self.assertEqual(run.results["library_sweep"], "success")
                for job_id in SKIPPED_WHEN_COMMENT_ONLY:
                    self.assertEqual(run.results[job_id], "success" if job_id in KEPT_FOR_INCLUDED_RUST else "skipped", job_id)
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

    def test_the_kill_switch_runs_both_heavy_lanes_even_for_a_comment_only_change(self) -> None:
        run = WorkflowSimulation(HEAVY_RAW, "true", rust_tests_skip="true", force_full="true")
        self.assertEqual((run.results["test_fast"], run.results["library_sweep"]), ("success", "success"))
        self.assertPublishedGreen(run)
        failed = {"test_fast": "failure", "library_sweep": "failure"}
        published = WorkflowSimulation(HEAVY_RAW, "true", rust_tests_skip="true", force_full="true",
                                       forced=failed).mirror_runs()
        for context in ("Fast targeted tests (ubuntu-latest)", "Library test sweep (ubuntu-latest)"):
            self.assertIn(1, [result.returncode for result in published[context]], context)

    def test_heavy_mirrors_still_fail_closed_when_not_comment_only(self) -> None:
        contexts = {
            "lint": "Lint",
            "relay_authority_targets": "relay-authority-contract",
            "relay_authority_mutations": "relay-authority-contract",
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
