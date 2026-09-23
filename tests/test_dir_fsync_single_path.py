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
            '        let directory =\n            std::fs::File::open(parent).map_err(|err| format!("{err}"))?;\n'
            '        sync_atomic_file(&directory, label, "parent")\n',
            "    let d = std::fs::File::open(\n        parent,\n    )?;\n    d.sync_all()?;\n",
            "    let d = File::open(&root)?;\n    d.sync_data()?;\n",
            "    OpenOptions::new().read(true).open(parent)?.sync_all()?;\n",
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
            "    let bytes = File::open(&redirect_log)?;\n"
            "    let entry = File::open(parent.join(\"x.json\"))?;\n"
            "    entry.sync_all()?;\n"
            "    let mut temp = OpenOptions::new().write(true).create_new(true).open(&temp)?;\n"
            "    temp.write_all(bytes).and_then(|_| temp.sync_all())?;\n"
            "    // File::open(parent)?.sync_all()?;\n"
        )
        self.assertEqual(audit(tree({"src/server/ok.rs": text})), [])

    def test_builder_set_to_write_in_an_earlier_statement_passes(self):
        # #6157 shapes: a write-mode builder opened later, and a read followed by
        # a function whose name contains `sync`.
        text = (
            "fn publish(dir: &Path) -> io::Result<()> {\n"
            "    let mut options = OpenOptions::new();\n"
            "    options.read(true).write(true).create(true);\n"
            "    let mut file = options.open(&temporary)?;\n"
            "    file.write_all(b\"x\").and_then(|()| file.sync_all())?;\n"
            "    Ok(())\n"
            "}\n"
            "fn read_receipt(path: &Path) -> io::Result<String> {\n"
            "    options.open(path)?.take(65).read_to_string(&mut result)?;\n"
            "    Ok(result)\n"
            "}\n"
            "fn sync_receipt_directory(dir: &Path) {}\n"
        )
        self.assertEqual(audit(tree({"src/server/ok.rs": text})), [])

    def test_write_builder_does_not_exempt_a_later_function(self):
        for text in (
            "fn a() {\n    options.write(true);\n}\n"
            "fn b(dir: &Path) {\n    let d = options.open(dir)?;\n    d.sync_all()?;\n}\n",
            "fn a() {\n    options.write(true);\n}\n"
            "fn b(dir: &Path) -> Result<(), String> {\n"
            "    for path in std::iter::once(dir).chain(dir.parent()) {\n"
            "        fs::File::open(path)\n"
            "            .and_then(|file| file.sync_all())\n"
            "            .map_err(|e| e.to_string())?;\n"
            "    }\n    Ok(())\n}\n",
        ):
            with self.subTest(text=text):
                self.assertEqual(len(audit(tree({"src/server/drift.rs": text}))), 1)


if __name__ == "__main__":
    unittest.main()
