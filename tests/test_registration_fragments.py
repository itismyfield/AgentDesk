"""The shared fragment reader: directory entries, `file:line` provenance and global keys."""

from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "registration_fragments_under_test", ROOT / "scripts/registration_fragments.py"
)
FRAGMENTS = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = FRAGMENTS
SPEC.loader.exec_module(FRAGMENTS)
LABEL = "scripts/list.d"


class FragmentDirectoryTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name) / "list.d"
        self.directory.mkdir()

    def read(self):
        return FRAGMENTS.read_lines(self.directory, LABEL)

    def test_missing_and_empty_directories_and_the_guide_read_as_no_rows(self) -> None:
        self.assertEqual(FRAGMENTS.read_lines(self.directory / "absent.d", LABEL), ([], []))
        self.assertEqual(self.read(), ([], []))
        (self.directory / "README.md").write_text("not | a row\n")
        self.assertEqual(self.read(), ([], []))

    def test_rows_carry_file_and_line_and_skip_blank_and_comment_lines(self) -> None:
        (self.directory / "mac-mini-6950-p1.txt").write_text("# why\n\nalpha\n  # indented\nbeta\n")
        (self.directory / "mac-book-12-s2_a.txt").write_text("gamma\n")
        lines, problems = self.read()
        self.assertEqual(problems, [])
        self.assertEqual([(line.location, line.text) for line in lines], [
            (f"{LABEL}/mac-book-12-s2_a.txt:1", "gamma"),
            (f"{LABEL}/mac-mini-6950-p1.txt:3", "alpha"),
            (f"{LABEL}/mac-mini-6950-p1.txt:5", "beta"),
        ])

    def test_every_other_entry_fails_so_no_row_hides_in_a_skipped_file(self) -> None:
        (self.directory / "mac-mini-6950-p1.text").write_text("hidden\n")
        (self.directory / "Mac-mini-6950-p1.txt").write_text("hidden\n")
        (self.directory / "mac-mini-p1.txt").write_text("hidden\n")
        (self.directory / "mac-mini-6950-p-1.txt").write_text("hidden\n")
        (self.directory / ".DS_Store").write_text("hidden\n")
        (self.directory / "nested").mkdir()
        (self.directory / "mac-mini-6950-p2.txt").symlink_to(self.directory / "nested")
        lines, problems = self.read()
        self.assertEqual(lines, [])
        self.assertEqual(sorted(location for location, _ in problems), sorted(
            f"{LABEL}/{name}" for name in (
                "mac-mini-6950-p1.text", "Mac-mini-6950-p1.txt", "mac-mini-p1.txt",
                "mac-mini-6950-p-1.txt", ".DS_Store", "nested", "mac-mini-6950-p2.txt",
            )
        ))

    def test_symlinked_or_non_directory_root_fails(self) -> None:
        link = self.directory.parent / "linked.d"
        link.symlink_to(self.directory)
        self.assertEqual(len(FRAGMENTS.read_lines(link, LABEL)[1]), 1)
        plain = self.directory.parent / "plain.d"
        plain.write_text("alpha\n")
        self.assertEqual(len(FRAGMENTS.read_lines(plain, LABEL)[1]), 1)

    def test_encoding_line_endings_and_final_newline_are_checked(self) -> None:
        (self.directory / "a-1-crlf.txt").write_bytes(b"alpha\r\n")
        (self.directory / "a-1-latin.txt").write_bytes(b"caf\xe9\n")
        (self.directory / "a-1-open.txt").write_text("alpha")
        lines, problems = self.read()
        self.assertEqual(dict(problems), {
            f"{LABEL}/a-1-crlf.txt": "a fragment must use LF line endings",
            f"{LABEL}/a-1-latin.txt": "a fragment must be UTF-8",
            f"{LABEL}/a-1-open.txt": "a fragment must end with a newline",
        })
        self.assertEqual([line.text for line in lines], ["alpha"])


class GlobalKeyTests(unittest.TestCase):
    def test_distinct_keys_index_in_first_seen_order(self) -> None:
        rows = [FRAGMENTS.Row("b", "x", "f:1"), FRAGMENTS.Row("a", "x", "g:1")]
        indexed, problems = FRAGMENTS.index(rows)
        self.assertEqual(problems, [])
        self.assertEqual(list(indexed), ["b", "a"])

    def test_identical_rows_are_a_duplicate_naming_both_locations(self) -> None:
        rows = [FRAGMENTS.Row("k", "x", "legacy:3"), FRAGMENTS.Row("k", "x", "a.d/n-1-s.txt:2")]
        indexed, problems = FRAGMENTS.index(rows)
        self.assertEqual(problems, [("a.d/n-1-s.txt:2", "duplicate of legacy:3 for `k`")])
        self.assertEqual(indexed["k"].location, "legacy:3")

    def test_same_key_with_other_payload_is_a_conflict_never_merged(self) -> None:
        rows = [FRAGMENTS.Row("k", "x", "f:1"), FRAGMENTS.Row("k", "y", "g:4"),
                FRAGMENTS.Row("k", "x", "h:9")]
        _, problems = FRAGMENTS.index(rows)
        self.assertEqual([location for location, _ in problems], ["g:4", "h:9"])
        self.assertIn("conflicts with f:1", problems[0][1])
        self.assertIn("'x' there, 'y' here", problems[0][1])
        self.assertIn("duplicate of f:1", problems[1][1])


if __name__ == "__main__":
    unittest.main()
