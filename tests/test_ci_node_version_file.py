from pathlib import Path
import re
import unittest


REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = (
    REPO_ROOT / ".github/workflows/ci-main.yml",
    REPO_ROOT / ".github/workflows/ci-nightly.yml",
    REPO_ROOT / ".github/workflows/ci-pr.yml",
)


class CiNodeVersionFileTests(unittest.TestCase):
    def test_setup_node_uses_repository_version_file(self) -> None:
        self.assertRegex((REPO_ROOT / ".nvmrc").read_text(encoding="utf-8"), r"^\d+\.\d+\.\d+\s*$")

        for workflow in WORKFLOWS:
            text = workflow.read_text(encoding="utf-8")
            self.assertNotRegex(text, re.compile(r"^\s+node-version:", re.MULTILINE))
            setup_count = text.count("uses: actions/setup-node@")
            version_file_count = text.count('node-version-file: ".nvmrc"')
            self.assertGreater(setup_count, 0, workflow)
            self.assertEqual(version_file_count, setup_count, workflow)


if __name__ == "__main__":
    unittest.main()
