"""Byte preservation and fail-closed contracts for the statechart index."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from scripts.ci import script_check_inputs, statechart_index as index


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
        self.readme.write_bytes(PREFIX + index.BEGIN + b"stale\n" + index.END + SUFFIX)
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
        for args in ((), ("--check",)):
            done = self.cli(*args)
            self.assertEqual(done.returncode, status, done.stdout + done.stderr)
            self.assertIn(name, done.stderr)
            self.assertEqual(self.readme.read_bytes(), before)
        return done.stderr

    def test_generation_is_deterministic_and_preserves_outside_bytes(self) -> None:
        self.assertEqual(self.cli().returncode, 0)
        first = self.readme.read_bytes()
        self.assertEqual(first, PREFIX + index.BEGIN + index.HEADER + ROW + index.END + SUFFIX)
        self.assertEqual(self.cli().returncode, 0)
        self.assertEqual(self.readme.read_bytes(), first)

    def test_migration_inserts_only_markers_around_original_table(self) -> None:
        original = PREFIX + index.HEADER + ROW + SUFFIX
        self.readme.write_bytes(PREFIX + index.BEGIN + index.HEADER + ROW + index.END + SUFFIX)
        self.assertEqual(self.cli().returncode, 0)
        self.assertEqual(self.readme.read_bytes().replace(index.BEGIN, b"").replace(index.END, b""),
                         original)

    def test_check_is_read_only_and_detects_drift_then_clean(self) -> None:
        before = self.readme.read_bytes()
        self.assertEqual(self.cli("--check").returncode, 1)
        self.assertEqual(self.readme.read_bytes(), before)
        self.assertEqual(self.cli().returncode, 0)
        self.assertEqual(self.cli("--check").returncode, 0)
        self.fragment("alpha", ROW.replace(b"None", b"Known gap"))
        done = self.cli("--check")
        self.assertEqual(done.returncode, 1, done.stderr)
        self.assertIn("drift", done.stderr)

    def test_manual_edits_are_preserved_and_do_not_require_a_fixed_digest(self) -> None:
        self.assertEqual(self.cli().returncode, 0)
        edited = self.readme.read_bytes().replace(b"Manual introduction.", b"Updated manual text.")
        self.readme.write_bytes(edited)
        self.assertEqual(self.cli("--check").returncode, 0)
        self.assertEqual(self.cli().returncode, 0)
        self.assertEqual(self.readme.read_bytes(), edited)

    def test_distinct_keys_compose_in_key_order(self) -> None:
        self.fragment("zulu", ROW.replace(b"alpha", b"zulu"))
        self.fragment("beta", ROW.replace(b"alpha", b"beta"))
        self.assertEqual(self.cli().returncode, 0)
        data = self.readme.read_bytes()
        self.assertLess(data.index(b"`alpha`"), data.index(b"`beta`"))
        self.assertLess(data.index(b"`beta`"), data.index(b"`zulu`"))
        self.assertEqual(self.cli("--check").returncode, 0)

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
        self.assertIn(row, self.readme.read_bytes())
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

    def test_marker_damage_or_duplication_has_exit_two(self) -> None:
        valid = self.readme.read_bytes()
        for data in (valid.replace(index.BEGIN, b""), valid.replace(index.END, b""),
                     valid + index.BEGIN, valid + index.END,
                     PREFIX + index.END + b"stale\n" + index.BEGIN + SUFFIX,
                     valid.replace(index.BEGIN, b"<!-- STATECHART INDEX:BEGIN-->\n"),
                     b"prefix " + index.BEGIN + index.END,
                     index.END + index.BEGIN,
                     valid + b"<!-- STATECHART INDEX:OTHER -->\n"):
            with self.subTest(data=data):
                self.readme.write_bytes(data)
                self.assert_rejected("README.md", status=2)

    def test_unresolved_merge_conflicts_inside_or_outside_markers_are_rejected(self) -> None:
        valid = self.readme.read_bytes()
        for conflict in (b"<<<<<<< ours\n", b"=======\n", b">>>>>>> theirs\n", b"||||||| base\n",
                         b"=======\r\n"):
            for data in (conflict + valid, valid + conflict,
                         valid.replace(b"stale\n", conflict)):
                with self.subTest(data=data):
                    self.readme.write_bytes(data)
                    self.assert_rejected("README.md")


class StatechartIndexCIContract(unittest.TestCase):
    def test_committed_index_is_clean(self) -> None:
        done = subprocess.run([sys.executable, str(SCRIPT), "--check"], cwd=ROOT,
                              capture_output=True, text=True, timeout=30)
        self.assertEqual(done.returncode, 0, done.stderr)

    def test_required_script_only_shard_runs_drift_and_selected_tests(self) -> None:
        runner = (ROOT / "scripts/ci-script-checks.sh").read_text()
        self.assertIn('if run_check contracts "Statechart index drift"; then\n'
                      '"$PYTHON" scripts/ci/statechart_index.py --check\nfi', runner)
        self.assertIn('if run_check contracts "Statechart index generator tests"; then\n'
                      '"$PYTHON" -m unittest tests.test_statechart_index\nfi', runner)
        self.assertNotIn("Statechart index drift", script_check_inputs.SELF_TEST_INPUTS)
        for path in ("scripts/ci/statechart_index.py", "tests/test_statechart_index.py",
                     "docs/statecharts/index.d/new-key.md", "docs/statecharts/README.md",
                     "docs/statecharts/area/deleted-report.md"):
            env = {**os.environ, "GITHUB_EVENT_NAME": "pull_request",
                   "SCRIPT_CHECK_CHANGES_RESULT": "success",
                   "SCRIPT_CHECK_CHANGED_FILES": json.dumps([path])}
            with self.subTest(path=path):
                self.assertNotIn("Statechart index generator tests", script_check_inputs.skipped_checks(env))


if __name__ == "__main__":
    unittest.main()
