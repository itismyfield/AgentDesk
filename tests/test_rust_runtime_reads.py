"""Runtime Rust consumers remain blocking except for one immutable YAML reader."""

from __future__ import annotations

import hashlib
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(ROOT / "scripts"), str(ROOT / "scripts" / "ci")]
import rust_runtime_reads as readers  # noqa: E402


class ScanTests(unittest.TestCase):
    def test_unlisted_literal_and_directory_readers_block(self) -> None:
        for body, changed in [
            ('std::fs::read_to_string("docs/guide.md");', "docs/guide.md"),
            ('std::fs::read_to_string("../../docs/guide.md");', "docs/guide.md"),
            ('std::fs::read_to_string(format!("{root}/docs/guide.md"));', "docs/guide.md"),
            ('std::fs::read_to_string(concat!("docs/gu", "ide.md"));', "docs/guide.md"),
            ('std::fs::read_to_string(concat!["docs/gu", "ide.md"]);', "docs/guide.md"),
            ('std::fs::read_to_string(concat!{"docs/gu", "ide.md"});', "docs/guide.md"),
            ('std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/gu", "ide.md"));', "docs/guide.md"),
            ('std::fs::read(root.join("docs").join("guide.md"));', "docs/guide.md"),
            ('File::open(root.join("docs").join("guide.yaml"));', "docs/guide.yaml"),
            ('const DIR: &str = "tests/scenarios"; std::fs::read_dir(DIR);', "tests/scenarios/a.yaml"),
            ('std::fs::read_dir("docs");', "docs/nested/guide.md"),
            ('std::fs::read_dir(concat!("do", "cs"));', "docs/nested/guide.md"),
            ('const DIR: &str = "docs"; let root = DIR; std::fs::read_dir(root);', "docs/guide.md"),
            ('std::fs::read(root.join("tests/scenarios").join("a.yaml"));', "tests/scenarios/a.yaml"),
        ]:
            with self.subTest(body=body):
                self.assertTrue(readers.scan("src/reader.rs", body, {changed}))

    def test_comments_includes_unrelated_paths_and_writes_do_not_block(self) -> None:
        for body in [
            '// std::fs::read_to_string("docs/guide.md");',
            'const TEXT: &str = "std::fs::read_to_string(\\\"docs/guide.md\\\")";',
            'const TEXT: &str = include_str!("docs/guide.md");',
            'std::fs::read_to_string("docs/other.md");',
            'std::fs::write("docs/guide.md", "text");',
            'const UNUSED: &str = "docs"; std::fs::read_dir("src");',
        ]:
            with self.subTest(body=body):
                self.assertFalse(readers.scan("src/reader.rs", body, {"docs/guide.md"}))


def git(root: Path, *args: str) -> str:
    return subprocess.check_output(["git", "-C", str(root), *args], text=True).strip()


class RepositoryTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.repo = Path(self.tmp.name)
        git(self.repo, "init", "-q")
        git(self.repo, "config", "user.email", "ci@example.invalid")
        git(self.repo, "config", "user.name", "CI guard test")
        source = self.repo / readers.SCENARIO_READER
        source.parent.mkdir(parents=True)
        source.write_bytes((ROOT / readers.SCENARIO_READER).read_bytes())
        git(self.repo, "add", ".")
        git(self.repo, "commit", "-qm", "reader fixture")

    def test_allowtable_source_and_parse_only_shape_are_pinned(self) -> None:
        pin = readers.PARSE_ONLY_READERS[readers.SCENARIO_READER]
        source = (ROOT / readers.SCENARIO_READER).read_bytes()
        assert hashlib.sha256(source).hexdigest() == pin.sha256, (
            "scenario_census_e2e.rs changed: re-verify parse-only shape (lines 70-90) "
            "and update PARSE_ONLY_READERS sha256 in scripts/ci/rust_runtime_reads.py"
        )
        lines = source.decode().splitlines()
        assert "std::fs::read_dir(&dir)" in lines[69]
        assert 'extension == "yaml"' in lines[74]
        assert "std::fs::read_to_string(path)" in lines[pin.read_line - 1]
        assert "serde_yaml::from_str(&raw)" in lines[pin.parse_line - 1]
        assert lines[pin.read_line:pin.parse_line - 1] == [
            '                .unwrap_or_else(|error| panic!("read {file}: {error}"));',
            "            let doc: Value =",
        ]
        assert pin.reason

    def test_only_verified_inplace_yaml_gets_parse_only_exemption(self) -> None:
        repo = self.repo
        yaml_path = "tests/e2e/tui_relay/scenarios/E-1-single-prompt.yaml"
        readme = "tests/e2e/tui_relay/scenarios/README.md"
        assert not readers.runtime_readers(str(repo), "HEAD", {yaml_path, readme}, {yaml_path})
        assert readers.runtime_readers(str(repo), "HEAD", {yaml_path}, set())

    def test_pinned_yaml_reader_ignores_added_readme_path_change(self) -> None:
        yaml_path = "tests/e2e/tui_relay/scenarios/E-1-single-prompt.yaml"
        readme = "tests/e2e/tui_relay/scenarios/README.md"
        blockers = readers.runtime_readers(str(self.repo), "HEAD", {yaml_path, readme}, {yaml_path}, {readme})
        assert not blockers, blockers

    def test_source_pin_mutation_blocks_even_if_reader_path_literal_disappears(self) -> None:
        repo = self.repo
        source = repo / readers.SCENARIO_READER
        source.write_text(source.read_text().replace('"tests/e2e/tui_relay/scenarios"', '"indirect"'))
        git(repo, "add", ".")
        git(repo, "commit", "-qm", "mutate reader")
        changed = {"tests/e2e/tui_relay/scenarios/a.yaml"}
        blockers = readers.runtime_readers(str(repo), "HEAD", changed, changed)
        assert any("source pin mismatch" in item for item in blockers)

    def test_source_pin_cannot_be_hidden_by_removing_read_symbols(self) -> None:
        source = self.repo / readers.SCENARIO_READER
        source.write_text("// changed\n")
        git(self.repo, "add", ".")
        git(self.repo, "commit", "-qm", "replace reader")
        changed = {"tests/e2e/tui_relay/scenarios/a.yaml"}
        blockers = readers.runtime_readers(str(self.repo), "HEAD", changed, changed)
        assert any("source pin mismatch" in item for item in blockers)

    def test_unknown_reader_is_not_hidden_by_parse_only_exception(self) -> None:
        repo = self.repo
        source = repo / "src/other.rs"
        source.write_text('std::fs::read_to_string("tests/e2e/tui_relay/scenarios/a.yaml");')
        git(repo, "add", ".")
        git(repo, "commit", "-qm", "another reader")
        changed = {"tests/e2e/tui_relay/scenarios/a.yaml"}
        assert any("src/other.rs" in item for item in readers.runtime_readers(str(repo), "HEAD", changed, changed))

    def test_non_rust_extension_spliced_source_readers_block(self) -> None:
        (self.repo / "src/fragment.inc").write_text('std::fs::read_to_string("docs/guide.yaml");')
        (self.repo / "src/reader.rs").write_text('include!("fragment.inc");')
        git(self.repo, "add", ".")
        git(self.repo, "commit", "-qm", "spliced reader")
        changed = {"docs/guide.yaml"}
        blockers = readers.runtime_readers(str(self.repo), "HEAD", changed, changed)
        assert any("src/fragment.inc" in item for item in blockers)

    def test_unresolved_rust_splice_fails_closed_for_changed_data(self) -> None:
        (self.repo / "src/reader.rs").write_text('include!(concat!(env!("OUT_DIR"), "/generated.rs"));')
        git(self.repo, "add", ".")
        git(self.repo, "commit", "-qm", "unknown splice")
        changed = {"docs/guide.yaml"}
        blockers = readers.runtime_readers(str(self.repo), "HEAD", changed, changed)
        assert any("unresolved Rust splice" in item for item in blockers)

    def test_git_errors_propagate(self) -> None:
        repo = self.repo
        with self.assertRaises(RuntimeError):
            readers.runtime_readers(str(repo), "missing-ref", {"docs/guide.md"}, set())

    def test_literal_only_rename_is_scanned_but_inplace_edit_is_not_blocked(self) -> None:
        (self.repo / "src/mention.rs").write_text('const DOC: &str = "docs/guide.md";')
        git(self.repo, "add", ".")
        git(self.repo, "commit", "-qm", "path mention without filesystem calls")
        changed = {"docs/guide.md", "docs/renamed.md"}
        blockers = readers.runtime_readers(str(self.repo), "HEAD", changed, set(), changed)
        assert any("src/mention.rs" in item and "docs/guide.md" in item for item in blockers)
        assert not readers.runtime_readers(str(self.repo), "HEAD", changed, set())


class PathObservationTests(unittest.TestCase):
    def test_exists_format_on_deleted_path_blocks(self) -> None:
        changed = {"docs/x.md"}
        body = 'exists(&format!("{base}/docs/x.md"));'
        assert readers.scan("src/prompt.rs", body, changed, changed)

    def test_exists_on_inplace_edit_blocks_without_path_change_guard(self) -> None:
        assert readers.scan("src/prompt.rs", 'exists(&format!("{base}/docs/x.md"));', {"docs/x.md"})

    def test_presence_and_metadata_observers_block_inplace_edits(self) -> None:
        for name in ("try_exists", "is_file", "is_dir", "metadata", "symlink_metadata", "canonicalize", "copy", "File"):
            with self.subTest(name=name):
                assert readers.scan("src/prompt.rs", f'{name}("docs/x.md");', {"docs/x.md"})

    def test_literal_only_rename_blocks_old_and_new_paths(self) -> None:
        changed = {"docs/old.md", "docs/new.md"}
        body = 'const OLD: &str = "docs/old.md"; const NEW: &str = "docs/new.md";'
        blockers = readers.scan("src/mentions.rs", body, changed, changed)
        assert len(blockers) == 2
        assert not readers.scan("src/mentions.rs", body, changed)

    def test_comment_only_mentions_do_not_observe_path_changes(self) -> None:
        changed = {"docs/x.md"}
        assert not readers.scan("src/mentions.rs", '// "docs/x.md"', changed, changed)


if __name__ == "__main__":
    unittest.main()
