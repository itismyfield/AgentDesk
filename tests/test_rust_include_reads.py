"""Include macros must fail closed without mistaking ordinary names for readers."""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(ROOT / "scripts"), str(ROOT / "scripts" / "ci")]
import rust_include_reads as includes  # noqa: E402


class IncludeMacroTests(unittest.TestCase):
    def scan(self, body: str) -> list[includes.Site]:
        return includes.scan("src/reader.rs", body, {""}, set())

    def test_loop_binding_and_local_names_are_not_readers(self) -> None:
        self.assertEqual(self.scan("for include in [false, true] { check(include); }"), [])
        self.assertEqual(self.scan("let include_str = value; let include_bytes = include_str;"), [])

    def test_struct_fields_and_access_are_not_readers(self) -> None:
        self.assertEqual(self.scan("struct Options { include: bool }\nlet o = Options { include: true }; o.include;"), [])

    def test_macro_definitions_and_module_paths_are_not_readers(self) -> None:
        self.assertEqual(self.scan("macro_rules! include { () => {} }\nuse crate::include::Options;"), [])

    def test_real_calls_with_each_delimiter_and_qualified_path(self) -> None:
        sites = self.scan('include!("a.inc"); std::include_str!["b.md"]; core::include_bytes!{"c.yaml"};')
        self.assertEqual([site.target for site in sites], ["src/a.inc", "src/b.md", "src/c.yaml"])

    def test_aliased_import_remains_unresolved_without_a_local_call(self) -> None:
        sites = self.scan("pub use ::std::include_str as grab;")
        self.assertEqual(len(sites), 1)
        self.assertEqual(sites[0].macro, "include_str")
        self.assertIsNone(sites[0].target)
        self.assertTrue(sites[0].reads("docs/guide.md"))

    def test_nested_brace_imports_remain_unresolved(self) -> None:
        sites = self.scan("use std::{fmt, {include_str as grab, include_bytes}, include};")
        self.assertEqual([site.macro for site in sites], ["include_str", "include_bytes", "include"])
        self.assertTrue(all(site.target is None and site.reads("src/body.rs") for site in sites))

    def test_unresolved_and_malformed_calls_fail_closed(self) -> None:
        for body in ("include_str!(PATH);", "include_str!", "include_str!PATH;", 'include_str!("x.md";', 'include_str!("x.md"}):'):
            with self.subTest(body=body):
                sites = self.scan(body)
                self.assertEqual(len(sites), 1)
                self.assertIsNone(sites[0].target)
                self.assertTrue(sites[0].reads("tests/case.yaml"))

    def test_comment_and_string_mentions_are_not_readers(self) -> None:
        self.assertEqual(self.scan('// include_str!("a");\nlet x = "include!(b)";'), [])

    def test_spliced_file_and_cross_file_alias_remain_readers(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            files = {
                "Cargo.toml": "[package]\nname = 'scan'\nversion = '0.1.0'\n",
                "src/lib.rs": 'include!("calls.inc");\npub use std::include_bytes as grab;\n',
                "src/calls.inc": 'const DATA: &str = include_str!("../docs/guide.md");\n',
                "src/other.rs": 'const DATA: &[u8] = crate::grab!("../tests/case.yaml");\n',
                "docs/guide.md": "guide\n",
                "tests/case.yaml": "case: true\n",
            }
            for name, body in files.items():
                (root / name).parent.mkdir(parents=True, exist_ok=True)
                (root / name).write_text(body, encoding="utf-8")
            for args in (("init", "-q"), ("add", "."), ("-c", "user.email=scan@example.invalid", "-c", "user.name=scan", "commit", "-qm", "seed")):
                subprocess.run(["git", "-C", directory, *args], check=True, capture_output=True)
            sites = includes.tree_sites(directory, "HEAD")
            self.assertTrue(any(site.target == "docs/guide.md" and site.path == "src/calls.inc" for site in sites))
            self.assertTrue(any(site.macro == "include_bytes" and site.target is None and site.reads("tests/case.yaml") for site in sites))


if __name__ == "__main__":
    unittest.main()
