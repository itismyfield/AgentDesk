import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


class GitHookPythonResolutionTests(unittest.TestCase):
    def test_pre_push_uses_portable_python_runner_helper(self):
        text = (ROOT / ".githooks" / "pre-push").read_text(encoding="utf-8")

        self.assertIn('source "$PROJECT_ROOT/scripts/resolve-python-runner.sh"', text)
        self.assertIn("agentdesk_run_python scripts/generate_inventory_docs.py", text)
        self.assertNotIn("python3 scripts/generate_inventory_docs.py", text)

    def test_python_runner_resolution_order_is_documented_in_helper(self):
        text = (ROOT / "scripts" / "resolve-python-runner.sh").read_text(encoding="utf-8")

        order = [
            text.index("${PYTHON:-}"),
            text.index("command -v python3"),
            text.index("command -v uv"),
            text.index("command -v py "),
        ]
        self.assertEqual(order, sorted(order))
        self.assertIn("set PYTHON=/path/to/python", text)


if __name__ == "__main__":
    unittest.main()
