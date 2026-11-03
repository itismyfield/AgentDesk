"""statecharts_tlc counts only a clean TLC finish or one named rule breaking."""

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

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
        text = ('CONSTANTS\n  Mut = "INVARIANT (* x"\n\\* INVARIANT Hidden (* "a\\b" ; ? é\nINIT Init\nNEXT Next\n'
                '(* INVARIANT Gone "a\\b" ` é\n  \\* still a block comment *)\n'
                "INVARIANTS\n  A\n  B\nCHECK_DEADLOCK TRUE\nINVARIANT C\n")
        config = tlc.parse_config(text)
        self.assertEqual(config.rules(), ["A", "B", "C", "NoDeadlock"])
        again = tlc.parse_config(config.render(["B"], False))
        self.assertEqual(again.rules(), ["B"])
        self.assertIn('Mut = "INVARIANT (* x"', config.render(["B"], False))

    def test_unsupported_or_ambiguous_configs_fail(self) -> None:
        for text in ("INIT Init\nPROPERTY Live\n", "INVARIANT\nINIT Init\n", "INVARIANT NoDeadlock\n",
                     "CHECK_DEADLOCK maybe\n"):
            with self.subTest(text), self.assertRaises(tlc.CheckError):
                tlc.parse_config(text)

    def test_text_tlc_reads_past_fails(self) -> None:
        cases = {
            "nested comment": ("INIT Init\n(* outer\n  (* inner *)\n  INVARIANT Ghost\n  \\* outer ends *)\n"
                               "INVARIANT Bad\n"),
            "unclosed comment": "INIT Init\n(* open\nINVARIANT Bad\n",
            "comment closed inside quotes": 'INIT Init\n(* a "*)" *)\nINVARIANT Bad\n',
            "unclosed string": 'INIT Init\n"abc\nINVARIANT Bad\n',
        }
        for label, text in cases.items():
            with self.subTest(label), self.assertRaisesRegex(tlc.CheckError, "not closed|nested"):
                tlc.parse_config(text)

    def test_backslash_in_a_string_fails(self) -> None:
        cases = {
            "unknown escape before the rules": ('INIT Init\nNEXT Next\n"bad \\* escape"\n'
                                                "INVARIANT Bad\nCHECK_DEADLOCK FALSE\n"),
            "escape in a constant value": 'CONSTANTS\n  Mut = "a\\"b"\nINVARIANT Bad\n',
            "line comment mark in a string": 'CONSTANTS\n  Mut = "INVARIANT (* \\*"\nINVARIANT Bad\n',
        }
        for label, text in cases.items():
            with self.subTest(label), self.assertRaisesRegex(tlc.CheckError, "escaped"):
                tlc.parse_config(text)

    def test_characters_tlc_cannot_read_fail(self) -> None:
        for line in (";", "?", '"x`x"', "é", "\fx"):
            with self.subTest(line), self.assertRaisesRegex(tlc.CheckError, "outside a comment"):
                tlc.parse_config(f"INIT Init\n{line}\nINVARIANT Bad\n")

    def test_check_deadlock_is_declared_at_most_once(self) -> None:
        for first in ("TRUE", "FALSE"):
            for second in ("TRUE", "FALSE"):
                with self.subTest(f"{first}/{second}"), self.assertRaisesRegex(tlc.CheckError, "twice"):
                    tlc.parse_config(f"INVARIANT A\nCHECK_DEADLOCK {first}\nCHECK_DEADLOCK {second}\n")
        for text, on in (("INVARIANT A\n", True), ("CHECK_DEADLOCK TRUE\nINVARIANT A\n", True),
                         ("INVARIANT A\nCHECK_DEADLOCK FALSE\n", False)):
            with self.subTest(text):
                self.assertEqual(tlc.parse_config(text).check_deadlock, on)


class CheckCfgTest(unittest.TestCase):
    def check(self, outputs: list[tuple[int, str]]) -> "tlc.Result":
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "M.tla").write_text("---- MODULE M ----\n====\n", encoding="utf-8")
            (root / "M.cfg").write_text("INIT Init\nNEXT Next\nINVARIANT Known\nCHECK_DEADLOCK FALSE\n",
                                        encoding="utf-8")
            with mock.patch.object(tlc, "run_tlc", side_effect=outputs) as run:
                result = tlc.check_cfg(root, "M.cfg", "M", {"Known"}, "java", root / "tla2tools.jar", 60)
            self.assertEqual(run.call_count, len(outputs))
            return result

    def test_run_after_the_last_rule_breaks_must_finish_clean(self) -> None:
        known = (12, msg(2107, "Invariant Known is violated by the initial state:"))
        self.assertEqual(self.check([known, (0, SUCCESS)]).problems(), [])
        failed = self.check([known, (255, msg(2179, "Attempted to apply the operator \\div to 1 and 0"))])
        self.assertEqual(failed.broken, {"Known"})
        self.assertIn("exit 255", failed.error or "")


class ProblemsTest(unittest.TestCase):
    def test_new_and_fixed_rules_are_named_per_config(self) -> None:
        result = tlc.Result("a/M.cfg", {"Old"}, broken={"New"}, rules=["Old", "New"])
        new, fixed = result.problems()
        self.assertTrue(new.startswith("a/M.cfg: New:") and "newly broken" in new)
        self.assertTrue(fixed.startswith("a/M.cfg: Old:") and "now holds" in fixed)


if __name__ == "__main__":
    unittest.main()
