"""The new-ID gate accepts only the cancellation guard's two-term extension."""

from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/lane_new_id_registration.py"
FILTERS = {"pg_paths": ("src/**", "scripts/ci/**"), "rust_paths": ("Cargo.toml",)}
OUTPUTS = {"pg_db": "pg_paths", "rust_tests": "rust_paths"}
EXPECTED = FILTERS["pg_paths"]
OUTPUT = "needs.changes.outputs.pg_db == 'true'"
POSITIVE = "${{ !cancelled() && " + OUTPUT + " }}"


def load(path: Path, name: str):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


registration = load(SCRIPT, "lane_activation_registration")


class LaneActivation(unittest.TestCase):
    def activate(self, condition, needs=None, filters=None, outputs=None):
        return registration._activation(condition, {"changes", "lib_test_prepare"} if needs is None else needs,
                                        FILTERS if filters is None else filters,
                                        OUTPUTS if outputs is None else outputs)

    def test_cancellation_guard_and_one_output_preserve_exact_patterns(self):
        for expression in (
            POSITIVE,
            "!cancelled() && " + OUTPUT,
            "${{ (!cancelled()) && (" + OUTPUT + ") }}",
            "${{ ((!cancelled()) && (" + OUTPUT + ")) }}",
            "${{ ( ! ( cancelled ( ) ) ) && (( " + OUTPUT + " )) }}",
            "${{\n ( ! cancelled ( ) ) \n && (needs.changes.outputs.pg_db=='true')\n }}",
            "${{ ((( ! (((cancelled())))) && ((" + OUTPUT + ")))) }}",
        ):
            with self.subTest(expression=expression):
                self.assertEqual(self.activate(expression), EXPECTED)

    def test_other_status_functions_and_unproven_needs_fail_closed(self):
        for expression in (
            "always() && " + OUTPUT,
            "!always() && " + OUTPUT,
            "!success() && " + OUTPUT,
            "!cancelled(false) && " + OUTPUT,
            "cancelled() && " + OUTPUT,
            "!!cancelled() && " + OUTPUT,
            "!cancelled() && needs.other.outputs.pg_db == 'true'",
            "!cancelled() && needs.changes.result == 'success'",
            OUTPUT + " && !cancelled()",
        ):
            with self.subTest(expression=expression):
                self.assertIsInstance(self.activate(expression), str)
        self.assertIsInstance(self.activate(POSITIVE, needs={"lib_test_prepare"}), str)

    def test_mixed_or_extra_terms_and_malformed_parentheses_fail_closed(self):
        for expression in (
            "!cancelled() && " + OUTPUT + " || needs.changes.outputs.rust_tests == 'true'",
            "(!cancelled() || always()) && " + OUTPUT,
            "!cancelled() && (" + OUTPUT + " || needs.changes.outputs.rust_tests == 'true')",
            "!cancelled() && " + OUTPUT + " && always()",
            "!cancelled() && " + OUTPUT + " && " + OUTPUT,
            "!cancelled() && " + OUTPUT + " &&",
            "!cancelled() &&",
            "(!cancelled() && " + OUTPUT,
            "!cancelled()) && (" + OUTPUT + ")",
            "(!cancelled()) && (" + OUTPUT + "))",
            "!cancelled() && needs.changes.outputs.pg_db == 't rue'",
        ):
            with self.subTest(expression=expression):
                self.assertIsInstance(self.activate(expression), str)

    def test_plain_output_and_existing_or_behavior_are_preserved(self):
        self.assertEqual(self.activate(OUTPUT), EXPECTED)
        self.assertEqual(self.activate(OUTPUT + " || needs.changes.outputs.rust_tests == 'true'"),
                         EXPECTED + FILTERS["rust_paths"])
        self.assertIsInstance(self.activate("always() || " + OUTPUT), str)

    def test_guard_extension_does_not_hide_missing_output_or_filter_proofs(self):
        self.assertIsInstance(self.activate(POSITIVE, outputs={}), str)
        self.assertIsInstance(self.activate(POSITIVE, filters={"pg_paths": None}), str)
        self.assertIsInstance(self.activate(POSITIVE.replace("'true'", "'false'")), str)

    def test_removing_new_recognition_makes_the_positive_contract_red(self):
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertEqual(source.count('if "&&" in expr:'), 1)
        with tempfile.TemporaryDirectory() as temporary:
            mutant_path = Path(temporary) / "lane_activation_mutant.py"
            mutant_path.write_text(source.replace('if "&&" in expr:', 'if False and "&&" in expr:'), encoding="utf-8")
            mutant = load(mutant_path, "lane_activation_mutant")
            class PositiveContract(unittest.TestCase):
                def runTest(self):
                    self.assertEqual(mutant._activation(POSITIVE, {"changes"}, FILTERS, OUTPUTS), EXPECTED)
            result = unittest.TestResult()
            PositiveContract().run(result)
        self.assertFalse(result.wasSuccessful())
        self.assertEqual(len(result.failures), 1)
        self.assertEqual(len(result.errors), 0)
