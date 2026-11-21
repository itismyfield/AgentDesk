"""Byte preservation and fail-closed contracts for the statechart index."""

from __future__ import annotations

from contextlib import redirect_stderr
import hashlib
from io import StringIO
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from scripts.ci import script_check_inputs, statechart_index as index, statechart_index_report
from scripts.ci.generated_artifact_report import ReportError, resolve_comparison


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/ci/statechart_index.py"
ROW = b"| `alpha` | `abc123` | None | [report](area/report.md) |\n"
PREFIX = b"# State charts\r\n\nManual introduction.\n\n"
SUFFIX = b"\nManual explanation.\r\n"


class StatechartIndexTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="statechart-index-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.fragments = self.root / "index.d"
        self.fragments.mkdir()
        (self.root / "area").mkdir()
        (self.root / "area/report.md").write_bytes(b"# Report\n")
        self.readme = self.root / "README.md"
        self.readme.write_bytes(PREFIX + SUFFIX)
        self.output = self.root / "index.generated.md"
        self.fragment("alpha", ROW)

    def fragment(self, key: str, row: bytes = ROW, *, filename: str | None = None) -> Path:
        path = self.fragments / f"{filename or key}.md"
        path.write_bytes(f"key: {key}\n".encode() + row)
        return path

    def cli(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run([sys.executable, str(SCRIPT), "--root", str(self.root), *args],
                              capture_output=True, text=True, timeout=30)

    def assert_rejected(self, name: str, *, status: int = 1) -> str:
        before = self.readme.read_bytes()
        for args in (("--output", str(self.output)), ("--check",)):
            done = self.cli(*args)
            self.assertEqual(done.returncode, status, done.stdout + done.stderr)
            self.assertIn(name, done.stderr)
            self.assertIn(index.RECOVERY, done.stderr)
            self.assertFalse(self.output.exists())
            self.assertEqual(self.readme.read_bytes(), before)
        return done.stderr

    def test_standalone_output_rejects_missing_and_stale_published_bytes(self) -> None:
        self.readme.write_bytes(PREFIX + SUFFIX)
        output = self.root / "index.generated.md"
        for expected in (None, b"stale\n"):
            if expected is not None:
                output.write_bytes(expected)
            done = self.cli("--check-output", str(output))
            self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
            self.assertIn("drift", done.stderr)
            self.assertIn("--output docs/statecharts/index.generated.md", done.stderr)
        self.assertEqual(self.cli("--output", str(output)).returncode, 0)
        self.assertEqual(self.cli("--check-output", str(output)).returncode, 0)
        self.assertEqual(self.readme.read_bytes(), PREFIX + SUFFIX)

    def test_plain_readme_accepts_independent_fragment_change(self) -> None:
        self.readme.write_bytes(PREFIX + SUFFIX)
        self.fragment("zulu")
        done = self.cli("--check")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(self.readme.read_bytes(), PREFIX + SUFFIX)

    def test_generation_is_deterministic_and_keeps_readme_bytes(self) -> None:
        self.fragment("zulu")
        self.fragment("aardvark", ROW.replace(b"alpha", b"aardvark"))
        self.assertEqual(self.cli("--output", str(self.output)).returncode, 0)
        first = self.output.read_bytes()
        self.assertEqual(first, index.HEADER + ROW.replace(b"alpha", b"aardvark") + ROW + ROW)
        self.assertEqual(self.cli("--output", str(self.output)).returncode, 0)
        self.assertEqual(self.output.read_bytes(), first)
        self.assertEqual(self.readme.read_bytes(), PREFIX + SUFFIX)

    def test_output_cannot_replace_readme(self) -> None:
        done = self.cli("--output", str(self.readme))
        self.assertEqual(done.returncode, 1, done.stderr)
        self.assertEqual(self.readme.read_bytes(), PREFIX + SUFFIX)

    def test_same_key_different_fragments_are_rejected(self) -> None:
        self.fragment("alpha", ROW.replace(b"None", b"Contradiction"), filename="beta")
        error = self.assert_rejected("beta.md")
        self.assertIn("duplicate key", error)
        self.assertIn("alpha.md", error)

    def test_filename_and_body_key_must_match(self) -> None:
        (self.fragments / "alpha.md").unlink()
        self.fragment("different", filename="alpha")
        self.assertIn("filename", self.assert_rejected("alpha.md"))

    def test_keys_must_be_readable_slugs(self) -> None:
        for key in ("Alpha", "alpha_beta", "alpha--beta", "", "../alpha"):
            with self.subTest(key=key):
                (self.fragments / "alpha.md").write_bytes(f"key: {key}\n".encode() + ROW)
                self.assert_rejected("alpha.md")

    def test_empty_rows_and_extra_lines_are_rejected(self) -> None:
        for data in (b"", b"key: alpha\n", b"key: alpha\n\n", b"key: alpha\n \n",
                     b"key: alpha\n" + ROW + b"\n", b"key: alpha\n" + ROW + ROW,
                     b"key: alpha\r\n" + ROW, b"key: alpha\n" + ROW.rstrip(b"\n")):
            with self.subTest(data=data):
                (self.fragments / "alpha.md").write_bytes(data)
                self.assert_rejected("alpha.md")

    def test_invalid_utf8_names_the_fragment(self) -> None:
        self.fragment("alpha", ROW.replace(b"None", b"\xff"))
        self.assertIn("UTF-8", self.assert_rejected("alpha.md"))

    def test_wrong_column_count_and_empty_cells_are_rejected(self) -> None:
        for row in (b"| a | b | c |\n", b"| a | b | c | d | e |\n", b"a | b | c | d |\n",
                    b"| a | b | c | d\n", b"| a | b | | d |\n"):
            with self.subTest(row=row):
                self.fragment("alpha", row)
                self.assert_rejected("alpha.md")

    def test_escaped_pipes_are_preserved_and_unescaped_code_pipes_are_columns(self) -> None:
        row = ROW.replace(b"None", rb"`a\|b` and a\|b")
        self.fragment("alpha", row)
        self.assertEqual(self.cli().returncode, 0)
        self.assertEqual(self.cli("--output", str(self.output)).returncode, 0)
        self.assertIn(row, self.output.read_bytes())
        self.output.unlink()
        self.fragment("alpha", ROW.replace(b"None", b"`a|b`"))
        self.assert_rejected("alpha.md")

    def test_broken_link_uses_readme_directory_instead_of_fragment_directory(self) -> None:
        (self.root / "area/report.md").unlink()
        (self.fragments / "area").mkdir()
        (self.fragments / "area/report.md").write_text("Wrong base\n")
        self.assertIn("area/report.md", self.assert_rejected("alpha.md"))

    def test_link_path_checks_decode_urls_and_ignore_anchor_and_query(self) -> None:
        (self.root / "area/space (report).md").write_text("Report\n")
        for target in (b"area/space%20%28report%29.md#details", b"<area/space (report).md>",
                       b"area/report.md?raw=1#details", b"#index", b"../" + self.root.name.encode() + b"/area/report.md"):
            with self.subTest(target=target):
                self.fragment("alpha", ROW.replace(b"area/report.md", target))
                self.assertEqual(self.cli().returncode, 0)
        self.fragment("alpha", ROW.replace(b"area/report.md", b"area/missing%20report.md#details"))
        self.assert_rejected("alpha.md")

    def test_every_inline_link_and_image_is_checked(self) -> None:
        for extra in (b" [missing](gone.md)", b" ![missing](gone.md)"):
            self.fragment("alpha", ROW.replace(b"None", b"None" + extra))
            self.assert_rejected("alpha.md")

    def test_unsupported_or_malformed_links_fail_instead_of_silently_passing(self) -> None:
        for link in (b"[report][ref]", b"[report](missing", b"[report]", b"[report](file:///tmp/x)",
                     b"[report](/absolute.md)", b"[report](area/bad%00.md)"):
            with self.subTest(link=link):
                self.fragment("alpha", ROW.replace(b"[report](area/report.md)", link))
                self.assert_rejected("alpha.md")

    def test_external_links_and_code_literals_are_not_local_paths(self) -> None:
        self.fragment("alpha", ROW.replace(b"None", b"`[literal](absent.md)`")
                      .replace(b"area/report.md", b"https://example.invalid/report"))
        self.assertEqual(self.cli().returncode, 0)

    def test_empty_fragment_directory_is_rejected(self) -> None:
        (self.fragments / "alpha.md").unlink()
        self.assert_rejected("index.d")

    def test_retired_markers_are_rejected(self) -> None:
        for retired in (index.BEGIN, index.END, index.BEGIN + index.HEADER + ROW + index.END):
            with self.subTest(retired=retired):
                self.readme.write_bytes(PREFIX + retired + SUFFIX)
                self.assertIn("retired", self.assert_rejected("README.md", status=2))

    def test_legacy_marker_bounds_and_rows_are_validated(self) -> None:
        valid = PREFIX + index.BEGIN + index.HEADER + ROW + index.END + SUFFIX
        self.assertEqual(index.legacy_table(valid, self.readme, lambda _: True), index.HEADER + ROW)
        for data in (valid.replace(index.BEGIN, b""), valid.replace(index.END, b""),
                     valid + index.BEGIN, valid + index.END,
                     index.END + index.BEGIN, b"prefix " + valid[len(PREFIX):]):
            with self.subTest(data=data), self.assertRaises(index.MarkerError):
                index.legacy_table(data, self.readme, lambda _: True)

    def test_unresolved_manual_merge_conflicts_are_rejected(self) -> None:
        for conflict in (b"<<<<<<< ours\n", b"=======\n", b">>>>>>> theirs\n", b"||||||| base\n",
                         b"=======\r\n"):
            with self.subTest(conflict=conflict):
                self.readme.write_bytes(PREFIX + conflict + SUFFIX)
                self.assertIn("conflict", self.assert_rejected("README.md"))


class StatechartIndexReportTest(unittest.TestCase):
    def setUp(self) -> None:
        temp = tempfile.TemporaryDirectory(prefix="statechart-report-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.charts = self.root / "docs/statecharts"
        self.fragments = self.charts / "index.d"
        self.fragments.mkdir(parents=True)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        (self.charts / "README.md").write_bytes(PREFIX + SUFFIX)
        report = self.charts / "area/report.md"
        report.parent.mkdir()
        report.write_text("# Report\n")
        self.fragment("alpha")
        self.base = self.commit("start")

    def git(self, *args, check=True):
        return subprocess.run(["git", "-C", str(self.root), *args], text=True,
                              capture_output=True, check=check, timeout=30)

    def fragment(self, key, row=ROW):
        (self.fragments / f"{key}.md").write_bytes(f"key: {key}\n".encode() + row)

    def commit(self, message):
        self.git("add", "docs")
        self.git("commit", "-qm", message)
        return self.git("rev-parse", "HEAD").stdout.strip()

    def test_pr_merge_of_independent_rows_uses_merge_base_and_keeps_manual_edits(self):
        self.git("branch", "other")
        self.fragment("beta")
        self.first = self.commit("first row")
        self.git("switch", "-q", "other")
        self.fragment("gamma")
        self.fragment("alpha", ROW.replace(b"None", b"changed contract"))
        introduction = PREFIX.replace(b"Manual introduction.", b"Revised introduction.")
        (self.charts / "README.md").write_bytes(introduction + SUFFIX)
        second = self.commit("second row")
        self.git("switch", "-q", "main")
        self.git("merge", "-q", "--no-edit", "other")
        head = self.git("rev-parse", "HEAD").stdout.strip()
        env = {"GITHUB_EVENT_NAME": "pull_request", "GFP_HEAD_SHA": second, "GITHUB_SHA": head,
               "TEST_LANE_BASELINE_REF": "HEAD^1", "GITHUB_REPOSITORY": "owner/repo"}
        output = self.root / "result"
        statechart_index_report.report(self.root, output, env)
        manifest = json.loads((output / "manifest.json").read_text())
        self.assertEqual((manifest["base"], manifest["head"]), (self.base, head))
        self.assertNotEqual(manifest["base"], self.first)
        self.assertEqual(manifest["sha256"], {
            name: hashlib.sha256((output / name).read_bytes()).hexdigest()
            for name in ("base.md", "head.md", "summary.md")
        })
        summary = (output / "summary.md").read_text()
        self.assertIn("Added (2)", summary)
        self.assertIn("Changed (1)", summary)
        self.assertIn(f"/blob/{head}/docs/statecharts/index.d/gamma.md", summary)
        self.assertIn(f"/blob/{head}/docs/statecharts/area/report.md", (output / "head.md").read_text())
        self.assertIn(f"/blob/{self.base}/docs/statecharts/area/report.md", (output / "base.md").read_text())
        self.assertEqual((self.charts / "README.md").read_bytes(), introduction + SUFFIX)
        self.assertEqual(self.git("status", "--porcelain", "--", "docs").stdout, "")

    def test_publish_rejects_changed_head_bytes(self):
        output = self.root / "modified"
        head = output / "head.md"
        original_write = Path.write_bytes

        def write(path, data):
            return original_write(path, b"stale table\n" if path == head else data)

        with mock.patch.object(Path, "write_bytes", write):
            with self.assertRaises(ReportError) as error:
                statechart_index_report.report(self.root, output, {})
        self.assertEqual(str(error.exception), f"{head}: artifact bytes differ from generated output")
        self.assertFalse((output / "manifest.json").exists())

    def test_report_cli_rejects_missing_published_head(self):
        output = self.root / "missing"
        head = output / "head.md"
        original_write = Path.write_bytes

        def write(path, data):
            return len(data) if path == head else original_write(path, data)

        stderr = StringIO()
        with (mock.patch.object(Path, "write_bytes", write),
              mock.patch.object(Path, "cwd", return_value=self.root),
              mock.patch.object(sys, "argv", ["statechart_index_report", str(output)]),
              mock.patch.dict(os.environ, {"GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_SHA": self.base}),
              redirect_stderr(stderr)):
            self.assertEqual(statechart_index_report.main(), 1)
        self.assertIn(str(head), stderr.getvalue())
        self.assertIn(index.RECOVERY, stderr.getvalue())
        self.assertFalse((output / "manifest.json").exists())

    def test_main_reports_changes_with_self_baseline_and_manual_is_explicit(self):
        (self.fragments / "alpha.md").unlink()
        self.fragment("delta")
        self.commit("replace row")
        event = self.root / "event.json"
        event.write_text(json.dumps({"before": self.base, "ref": "refs/heads/main"}))
        env = {"GITHUB_EVENT_NAME": "push", "GITHUB_EVENT_PATH": str(event),
               "TEST_LANE_BASELINE_REF": "HEAD", "GFP_BASE_SHA": "HEAD"}
        output = self.root / "push"
        statechart_index_report.report(self.root, output, env)
        summary = (output / "summary.md").read_text()
        self.assertIn("Added (1)", summary)
        self.assertIn("Removed (1)", summary)
        self.assertIn(f"/blob/{self.base}/docs/statecharts/index.d/alpha.md", summary)
        manual = self.root / "manual"
        statechart_index_report.report(self.root, manual, {"GITHUB_EVENT_NAME": "workflow_dispatch"})
        summary = (manual / "summary.md").read_text()
        self.assertIn("Self check", summary)
        self.assertNotIn("Added", summary)
        self.assertEqual((manual / "base.md").read_bytes(), (manual / "head.md").read_bytes())
        event.write_text(json.dumps({"before": "0" * 40, "ref": "refs/heads/main"}))
        with self.assertRaisesRegex(ReportError, "previous main SHA"):
            resolve_comparison(self.root, env)

    def test_manual_readme_conflict_is_not_resolved_by_index_report(self):
        self.git("branch", "other")
        readme = self.charts / "README.md"
        readme.write_bytes(b"first text\n")
        self.commit("first manual edit")
        self.git("switch", "-q", "other")
        readme.write_bytes(b"second text\n")
        self.commit("second manual edit")
        self.git("switch", "-q", "main")
        self.assertEqual(self.git("merge", "--no-edit", "other", check=False).returncode, 1)
        self.assertIn("UU docs/statecharts/README.md", self.git("status", "--porcelain").stdout)
        done = subprocess.run([sys.executable, str(SCRIPT), "--root", str(self.charts), "--check"],
                              capture_output=True, text=True, timeout=30)
        self.assertEqual(done.returncode, 1, done.stderr)
        self.assertIn("README.md: unresolved merge conflict", done.stderr)
        self.assertIn(index.RECOVERY, done.stderr)


class StatechartIndexCIContract(unittest.TestCase):
    def test_committed_index_sources_are_valid(self) -> None:
        done = subprocess.run([sys.executable, str(SCRIPT), "--check"], cwd=ROOT,
                              capture_output=True, text=True, timeout=30)
        self.assertEqual(done.returncode, 0, done.stderr)

    def test_required_shard_always_checks_sources_and_publishes_report(self) -> None:
        runner = (ROOT / "scripts/ci-script-checks.sh").read_text()
        self.assertIn('if run_check contracts "Statechart index source contract"; then\n'
                      '"$PYTHON" scripts/ci/statechart_index.py --check\n', runner)
        self.assertIn('-m scripts.ci.statechart_index_report', runner)
        for workflow in ("ci-pr.yml", "ci-main.yml"):
            contracts = (ROOT / ".github/workflows" / workflow).read_text().split("  scripts_contracts:", 1)[1]
            self.assertIn("GENERATED_ARTIFACT_REPORT_DIR: target/generated-contracts", contracts)
            self.assertIn("path: target/generated-contracts/statechart-index/", contracts)
        self.assertIn('if run_check contracts "Statechart index generator tests"; then\n'
                      '"$PYTHON" -m unittest tests.test_statechart_index\nfi', runner)
        self.assertNotIn("Statechart index source contract", script_check_inputs.SELF_TEST_INPUTS)
        for path in ("scripts/ci/statechart_index.py", "tests/test_statechart_index.py",
                     "scripts/ci/statechart_index_report.py", "scripts/ci/generated_artifact_report.py",
                     "docs/statecharts/index.d/new-key.md", "docs/statecharts/README.md",
                     "docs/statecharts/area/deleted-report.md"):
            env = {**os.environ, "GITHUB_EVENT_NAME": "pull_request",
                   "SCRIPT_CHECK_CHANGES_RESULT": "success",
                   "SCRIPT_CHECK_CHANGED_FILES": json.dumps([path])}
            with self.subTest(path=path):
                self.assertNotIn("Statechart index generator tests", script_check_inputs.skipped_checks(env))


if __name__ == "__main__":
    unittest.main()
