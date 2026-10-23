from __future__ import annotations

import importlib.util
import re
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
        if extra:
            write(root, "src/services/discord/rogue.rs", extra)
        subprocess.run(["git", "add", "-A"], cwd=root, check=True)
        write(root, "src/services/tui_o/exact_pg.rs", "fn evidence() { discord::append_exact_metadata(); }\n")
        return root

    def test_exact_metadata_facade_registration_is_required(self):
        root = self.fixture()
        previous = guard.EXACT_METADATA_FACADES
        try:
            guard.EXACT_METADATA_FACADES = {}
            self.assertIn("registry drift", guard.exact_facade_error(root))
        finally:
            guard.EXACT_METADATA_FACADES = previous

    def test_exact_allowlist_passes(self):
        root = self.fixture()
        write(root, "src/services/tui_o/exact_pg.rs", "fn evidence() { discord::append_exact_metadata(); }\n")
        ok, message = guard.check(root)
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


if __name__ == "__main__":
    unittest.main()
