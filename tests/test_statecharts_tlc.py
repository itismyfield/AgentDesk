"""statecharts_tlc counts only a clean TLC finish or one named rule breaking."""

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT_PATH = Path(__file__).parents[1] / "scripts" / "ci" / "statecharts_tlc.py"
SPEC = importlib.util.spec_from_file_location("statecharts_tlc", SCRIPT_PATH)
assert SPEC and SPEC.loader
tlc = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = tlc
SPEC.loader.exec_module(tlc)


def msg(code: int, body: str) -> str:
    return f"@!@!@STARTMSG {code}:1 @!@!@\n{body}\n@!@!@ENDMSG {code} @!@!@\n"


SUCCESS = msg(2193, "Model checking completed. No error has been found.")
DEADLOCK = msg(2114, "Deadlock reached.")


def violated(name: str) -> str:
    return msg(2110, f"Invariant {name} is violated.")


class ClassifyTest(unittest.TestCase):
    def test_named_break_and_clean_finish(self) -> None:
        self.assertIsNone(tlc.classify(0, SUCCESS, ["A"], True))
        self.assertEqual(tlc.classify(12, violated("A"), ["A", "B"], True), "A")
        self.assertEqual(tlc.classify(11, DEADLOCK, ["A"], True), "NoDeadlock")

    def test_other_endings_never_pass(self) -> None:
        cases = {
            "parse error": (150, "Could not parse module TmuxSession"),
            "config error": (151, msg(5001, "unknown invariant")),
            "exit 0 without the success message": (0, "Finished in 91ms"),
            "violation exit naming no rule": (12, "Invariant violated"),
            "rule not checked in this run": (12, violated("C")),
            "two rules in one run": (12, violated("A") + violated("B")),
            "deadlock while deadlock is off": (11, DEADLOCK),
            "success message with a violation exit": (12, SUCCESS + violated("A")),
        }
        for label, (status, output) in cases.items():
            with self.subTest(label), self.assertRaises(tlc.CheckError):
                tlc.classify(status, output, ["A", "B"], label != "deadlock while deadlock is off")


class ConfigTest(unittest.TestCase):
    def test_render_keeps_only_the_given_rules(self) -> None:
        text = ('CONSTANTS\n  Mut = "INVARIANT"\n\\* INVARIANT Hidden\nINIT Init\nNEXT Next\n'
                "INVARIANTS\n  A\n  B\nCHECK_DEADLOCK TRUE\nINVARIANT C\n")
        config = tlc.parse_config(text)
        self.assertEqual(config.rules(), ["A", "B", "C", "NoDeadlock"])
        again = tlc.parse_config(config.render(["B"], False))
        self.assertEqual(again.rules(), ["B"])
        self.assertIn('Mut = "INVARIANT"', config.render(["B"], False))

    def test_unsupported_or_ambiguous_configs_fail(self) -> None:
        for text in ("INIT Init\nPROPERTY Live\n", "INVARIANT\nINIT Init\n", "INVARIANT NoDeadlock\n",
                     "CHECK_DEADLOCK maybe\n"):
            with self.subTest(text), self.assertRaises(tlc.CheckError):
                tlc.parse_config(text)


class ProblemsTest(unittest.TestCase):
    def test_new_and_fixed_rules_are_named_per_config(self) -> None:
        result = tlc.Result("a/M.cfg", {"Old"}, broken={"New"}, rules=["Old", "New"])
        new, fixed = result.problems()
        self.assertTrue(new.startswith("a/M.cfg: New:") and "newly broken" in new)
        self.assertTrue(fixed.startswith("a/M.cfg: Old:") and "now holds" in fixed)


if __name__ == "__main__":
    unittest.main()
