import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SPECIFIC_USER_HOME = re.compile(
    r"/Users/(?!REPLACE_ME\b|user\b|me\b|example\b)[A-Za-z0-9._-]+"
)


class PortableDocsExamplesTests(unittest.TestCase):
    def read(self, relative_path: str) -> str:
        return (ROOT / relative_path).read_text(encoding="utf-8")

    def test_starter_example_config_uses_portable_placeholders(self):
        text = self.read("agentdesk.example.yaml")

        self.assertNotIn("mac-mini-release", text)
        self.assertNotIn("mac-book-release", text)
        self.assertNotIn("itismyfield", text)
        self.assertNotIn("1469870512812462284", text)
        self.assertIn("example-main-node", text)
        self.assertIn("example-worker-node", text)
        self.assertIn("YOUR_GUILD_ID", text)
        self.assertIn("YOUR_DEV_CATEGORY_ID", text)
        self.assertIn("YOUR_OPERATIONS_CATEGORY_ID", text)
        self.assertEqual(text.count("shared_prompt:"), 1)
        self.assertNotIn("/IDENTITY.md", text)

    def test_readme_cluster_snippet_uses_starter_node_names(self):
        text = self.read("README.md")

        self.assertNotIn("instance_id: mac-mini-release", text)
        self.assertNotIn("mac-book-release", text)
        self.assertIn("instance_id: example-main-node", text)
        self.assertIn("default_preferred_labels: [example-worker]", text)
        self.assertIn("operator-init-portable.py --root ~/.adk/release", text)

    def test_portable_docs_do_not_embed_operator_user_home(self):
        for relative_path in (
            "docs/source-of-truth.md",
            "docs/claude-md-load-order.md",
        ):
            with self.subTest(path=relative_path):
                text = self.read(relative_path)
                self.assertIsNone(SPECIFIC_USER_HOME.search(text))


if __name__ == "__main__":
    unittest.main()
