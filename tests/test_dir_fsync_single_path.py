import tempfile
import unittest
from pathlib import Path

from scripts.check_dir_fsync_single_path import audit

CANONICAL = "src/services/discord/runtime_store.rs"
HELPER = '    fs::File::open(parent.unwrap_or_else(|| Path::new(".")))?.sync_all()\n'


def tree(files: dict[str, str]) -> Path:
    root = Path(tempfile.mkdtemp())
    for relative, text in {CANONICAL: HELPER, **files}.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
    return root


class DirFsyncSinglePathTests(unittest.TestCase):
    def test_repository_has_exactly_the_canonical_helper(self):
        self.assertEqual(audit(Path(__file__).resolve().parents[1]), [])

    def test_canonical_helper_alone_passes(self):
        self.assertEqual(audit(tree({})), [])

    def test_inline_copies_are_rejected(self):
        for text in (
            "        File::open(parent)?.sync_all()?;\n",
            '        let directory =\n            std::fs::File::open(parent).map_err(|err| format!("{err}"))?;\n',
            "    let d = File::open(&state_dir)?;\n",
            "    File::open(root.join(\"x\"))?.sync_data()?;\n",
        ):
            with self.subTest(text=text):
                findings = audit(tree({"src/server/drift.rs": text}))
                self.assertEqual(len(findings), 1)
                self.assertIn("src/server/drift.rs", findings[0])

    def test_second_copy_in_canonical_file_is_rejected(self):
        self.assertEqual(len(audit(tree({CANONICAL: HELPER * 2}))), 1)

    def test_missing_canonical_helper_is_rejected(self):
        self.assertEqual(len(audit(tree({CANONICAL: "fn other() {}\n"}))), 1)

    def test_plain_file_opens_and_line_comments_pass(self):
        text = (
            "    let bytes = File::open(&path)?;\n"
            "    // File::open(parent)?.sync_all()?;\n"
        )
        self.assertEqual(audit(tree({"src/server/ok.rs": text})), [])


if __name__ == "__main__":
    unittest.main()
