"""Required-shard recipe membership for the native oracle regression tests."""
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


class NativeWiring(unittest.TestCase):
    def test_required_guards_recipe_executes_both_native_suites_and_mutants(self):
        expected = ('if run_check guards "N1 native oracle contracts"; then\n'
                    '"$PYTHON" -m unittest tests.test_n1_canary_native tests.test_n1_canary_native_cli tests.test_n1_canary_native_wiring\n'
                    '"$PYTHON" tests/run_n1_canary_native_mutations.py\nfi')
        self.assertTrue(expected in (ROOT / "scripts/ci-script-checks.sh").read_text(), "native suites must execute in the required guards shard")


if __name__ == "__main__":
    unittest.main()
