import importlib.util
import tempfile
import unittest
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "check_clippy_allow_ratchet", ROOT / "scripts" / "check_clippy_allow_ratchet.py"
)
assert SPEC and SPEC.loader
RATCHET = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RATCHET)


class ClippyAllowRatchetTest(unittest.TestCase):
    def test_checked_in_baseline_matches_current_occurrences(self) -> None:
        problems = RATCHET.validate_occurrences(
            RATCHET.collect_occurrences(), RATCHET.load_baseline()
        )
        self.assertEqual(problems, [])

    def test_new_allow_occurrence_fails(self) -> None:
        baseline = Counter({("src/example.rs", "too_many_arguments"): 1})
        actual = baseline.copy()
        actual[("src/example.rs", "too_many_arguments")] += 1
        problems = RATCHET.validate_occurrences(actual, baseline)
        self.assertEqual(len(problems), 1)
        self.assertIn("baseline 1", problems[0])

    def test_new_path_or_lint_occurrence_fails(self) -> None:
        actual = Counter({("src/new.rs", "type_complexity"): 1})
        problems = RATCHET.validate_occurrences(actual, Counter())
        self.assertEqual(len(problems), 1)
        self.assertIn("baseline 0", problems[0])

    def _collect_source(self, text: str) -> Counter[tuple[str, str]]:
        with tempfile.TemporaryDirectory(dir=ROOT) as temp_dir:
            root = Path(temp_dir)
            source = root / "sample.rs"
            source.write_text(text, encoding="utf-8")
            original_root = RATCHET.REPO_ROOT
            try:
                RATCHET.REPO_ROOT = root
                return RATCHET.collect_occurrences(root)
            finally:
                RATCHET.REPO_ROOT = original_root

    def test_only_attributes_are_counted(self) -> None:
        actual = self._collect_source(
            "// clippy::too_many_arguments\n"
            "#[allow(\n    clippy::too_many_arguments,\n    clippy::type_complexity\n)]\n"
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)
        self.assertEqual(actual[("sample.rs", "type_complexity")], 1)
        self.assertEqual(sum(actual.values()), 2)

    def test_clippy_all_group_allow_is_ratchet_visible(self) -> None:
        actual = self._collect_source("#![allow(clippy::all)]\nfn sample() {}\n")
        for lint in RATCHET.LINTS:
            self.assertEqual(actual[("sample.rs", lint)], 1)
        problems = RATCHET.validate_occurrences(actual, Counter())
        self.assertEqual(len(problems), len(RATCHET.LINTS))

    def test_cfg_attr_nested_allow_is_ratchet_visible(self) -> None:
        actual = self._collect_source(
            "#[cfg_attr(all(), allow(clippy::too_many_arguments))]\n"
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)
        problems = RATCHET.validate_occurrences(actual, Counter())
        self.assertEqual(len(problems), 1)
        self.assertIn("too_many_arguments", problems[0])

    def test_cfg_attr_nested_group_allow_is_ratchet_visible(self) -> None:
        actual = self._collect_source(
            "#[cfg_attr(all(), expect(clippy::complexity))]\n"
            "fn sample() {}\n"
        )
        for lint in RATCHET.LINTS:
            self.assertEqual(actual[("sample.rs", lint)], 1)
        self.assertEqual(len(RATCHET.validate_occurrences(actual, Counter())), 4)

    def test_reason_string_parentheses_do_not_hide_allow(self) -> None:
        actual = self._collect_source(
            '#[allow(clippy::too_many_arguments, reason = "legacy (tracked)")]\n'
            "fn sample() {}\n"
        )
        self.assertEqual(actual[("sample.rs", "too_many_arguments")], 1)
        problems = RATCHET.validate_occurrences(actual, Counter())
        self.assertEqual(len(problems), 1)
        self.assertIn("too_many_arguments", problems[0])


if __name__ == "__main__":
    unittest.main()
