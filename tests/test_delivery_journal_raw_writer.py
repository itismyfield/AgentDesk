from __future__ import annotations

import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path
ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/check_delivery_journal_raw_writer.py"
SPEC = importlib.util.spec_from_file_location("journal_writer_guard", SCRIPT)
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)
def write(root: Path, rel: str, body: str) -> None:
    path = root / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")
class RawWriterAllowlistTests(unittest.TestCase):
    def fixture(self, extra: str = "") -> Path:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        subprocess.run(["git", "init", "-q"], cwd=root, check=True)
        write(root, "src/services/discord/session_relay_sink/journal/pg_store.rs", "fn append_delivery_journal_batch() {}\n")
        write(root, "src/services/discord/session_relay_sink/journal.rs", "fn actor() { append_delivery_journal_batch(); }\n")
        for index, (_, rel, symbol) in enumerate(guard.FAMILY_REGISTRY):
            call = " self.journal.begin_fresh();" if index == 0 else ""
            write(root, rel, f"fn {symbol}() {{{call}}}\n")
        if extra:
            write(root, "src/services/discord/rogue.rs", extra)
        subprocess.run(["git", "add", "-A"], cwd=root, check=True)
        return root
    def test_exact_allowlist_passes(self):
        ok, message = guard.check(self.fixture())
        self.assertTrue(ok, message)

    def test_raw_store_external_call_fails_its_own_assert(self):
        ok, message = guard.check(self.fixture("fn rogue() { append_delivery_journal_batch(); }\n"))
        self.assertFalse(ok)
        self.assertIn("exceeds monotonic baseline", message)
    def test_top_level_src_rust_rogue_call_fails_its_own_assert(self):
        root = self.fixture()
        write(root, "src/config.rs", "fn rogue() { append_delivery_journal_batch(); }\n")
        subprocess.run(["git", "add", "-A"], cwd=root, check=True)
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("src/config.rs", message)
    def test_line_comments_are_excluded_but_block_comments_and_strings_count(self):
        """Declare the exact lexical boundary: // is excluded; /* */ and strings count."""
        ok, message = guard.check(self.fixture("// append_delivery_journal_batch(x);\n"))
        self.assertTrue(ok, message)
        for marker in (
            "/* append_delivery_journal_batch(x); */\n",
            'const S: &str = "append_delivery_journal_batch(x);";\n',
        ):
            ok, message = guard.check(self.fixture(marker))
            self.assertFalse(ok, marker)
            self.assertIn("raw writer call count 2 exceeds monotonic baseline 1", message)

    def test_test_area_and_string_facade_markers_count_as_declared(self):
        """Evidence: whole-file lexical scanning counts test and string text."""
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[4][1]
        path.write_text(path.read_text(encoding="utf-8") +
                        'const STRING_MARKER: &str = "self.journal.finish_fresh(";\n#[cfg(all(test, unix))]\nmod tests {\n    fn dishonest() { self.journal.begin_fresh(); }\n    const TEST_MARKER: &str = "self.journal.begin_fresh(";\n}\n',
                        encoding="utf-8")
        self.assertTrue(guard.family_status(root)[0][4][1], "test/string markers are declared lexical matches")

    def test_cfg_test_fn_facade_marker_counts_as_known_limit(self):
        """Evidence: a top-level cfg(test) function is counted, not parsed away."""
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[4][1]
        path.write_text(path.read_text(encoding="utf-8") + '#[cfg(test)] fn journal_probe() { self.journal.begin_fresh(); }\n', encoding="utf-8")
        self.assertTrue(guard.family_status(root)[0][4][1], "cfg(test) fn marker is a declared lexical match")
        ok, message = guard.check(root); self.assertFalse(ok); self.assertIn("uninstrumented families: 4/6", message)

    def test_line_doc_comment_markers_are_known_limit_and_not_counted(self):
        """Known limitation and declared behavior: line doc comments are stripped after // on each line."""
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[4][1]
        path.write_text(path.read_text(encoding="utf-8") +
                        "//! self.journal.begin_fresh();\n/// self.journal.begin_fresh();\n",
                        encoding="utf-8")
        status, error = guard.family_status(root)
        self.assertEqual(error, "")
        self.assertFalse(status[4][1], "line doc comment markers are excluded by the declared lexical cut")
        ok, message = guard.check(root)
        self.assertTrue(ok, message)
        self.assertIn("uninstrumented families: 5/6", message)

    def test_block_marker_strings_do_not_hide_real_facade_calls(self):
        """Evidence: block-marker strings no longer delete calls across lines."""
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[0][1]
        path.write_text(path.read_text(encoding="utf-8") + 'const BLOCK_OPEN: &str = "/*";\nself.journal.begin_fresh();\nconst BLOCK_CLOSE: &str = "*/";\n', encoding="utf-8")
        ok, message = guard.check(root); self.assertTrue(ok, message); self.assertIn("uninstrumented families: 5/6", message)

    def test_raw_string_marker_is_known_lexical_false_positive(self):
        """Known limit: raw strings are not parsed and may count as calls."""
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[4][1]
        path.write_text(path.read_text(encoding="utf-8") +
                        'const RAW: &str = r#"x" self.journal.begin_fresh("#;\n', encoding="utf-8")
        status, error = guard.family_status(root)
        self.assertEqual(error, "")
        self.assertTrue(status[4][1], "raw-string marker intentionally pierces lexical scan")

    def test_family_baseline_is_measured_and_named(self):
        ok, message = guard.check(self.fixture())
        self.assertTrue(ok, message)
        self.assertIn("uninstrumented families: 5/6", message)
        self.assertIn("sink direct family", message)

    def test_instrumentation_rule_is_mechanical(self):
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[0][1]
        path.write_text(path.read_text(encoding="utf-8").replace("self.journal.begin_fresh();", ""), encoding="utf-8")
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("exceeds baseline", message)

    def test_missing_anchor_symbol_fails_closed(self):
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[0][1]
        path.write_text(path.read_text(encoding="utf-8").replace(guard.FAMILY_REGISTRY[0][2], "anchor_removed"), encoding="utf-8")
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("family anchor symbol missing", message)

    def test_missing_anchor_file_fails_closed(self):
        root = self.fixture()
        path = root / guard.FAMILY_REGISTRY[0][1]
        path.unlink()
        subprocess.run(["git", "rm", "-q", "--cached", "--", guard.FAMILY_REGISTRY[0][1]], cwd=root, check=True)
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("family anchor missing", message)

    def test_baseline_increase_names_families(self):
        root = self.fixture()
        old = guard.UNINSTRUMENTED_FAMILY_BASELINE
        guard.UNINSTRUMENTED_FAMILY_BASELINE = 4
        self.addCleanup(setattr, guard, "UNINSTRUMENTED_FAMILY_BASELINE", old)
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("sink direct family", message)

    def test_baseline_decrease_requires_repin_command(self):
        root = self.fixture()
        old = guard.UNINSTRUMENTED_FAMILY_BASELINE
        guard.UNINSTRUMENTED_FAMILY_BASELINE = 6
        self.addCleanup(setattr, guard, "UNINSTRUMENTED_FAMILY_BASELINE", old)
        ok, message = guard.check(root)
        self.assertFalse(ok)
        self.assertIn("re-pin with: python3", message)
    def test_live_repository_matches_exact_allowlist(self):
        result = subprocess.run(["python3", str(SCRIPT)], cwd=ROOT, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertRegex(result.stdout, r"scanned Rust files: [1-9][0-9]*")
        self.assertIn("uninstrumented families: 5/6", result.stdout)
if __name__ == "__main__":
    unittest.main()
