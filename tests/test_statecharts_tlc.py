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


def msg(code: int, body: str, level: int = 1) -> str:
    return f"@!@!@STARTMSG {code}:{level} @!@!@\n{body}\n@!@!@ENDMSG {code} @!@!@\n"


SUCCESS = msg(2193, "Model checking completed. No error has been found.", 0)
DEADLOCK = msg(2114, "Deadlock reached.")
# TLC 1.7.4 ExecutionAuthority main/fix1 output with -tool.
TEMPORAL_START = msg(2192, "Checking temporal properties for the complete state space with "
                     "6 total distinct states at (2026-11-04 17:40:44)", 0)
TEMPORAL_FINISH = msg(2267, "Finished checking temporal properties in 00s at 2026-11-04 17:40:44", 0)
TEMPORAL_BROKEN = TEMPORAL_START + msg(2116, "Temporal properties were violated.\n") + TEMPORAL_FINISH
TEMPORAL_SUCCESS = TEMPORAL_START + TEMPORAL_FINISH + SUCCESS


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

    def test_temporal_violation_is_attributed_to_the_only_checked_property(self) -> None:
        self.assertEqual(tlc.classify(13, TEMPORAL_BROKEN, [], False, "Live"), "Live")
        self.assertIsNone(tlc.classify(0, TEMPORAL_SUCCESS, [], False, "Live"))

    def test_temporal_endings_require_matching_exit_and_check_evidence(self) -> None:
        cases = {
            "property ignored": (0, SUCCESS),
            "check never completed": (0, TEMPORAL_START + SUCCESS),
            "violation without temporal checking": (13, msg(2116, "Temporal properties were violated.")),
            "wrong violation exit": (12, TEMPORAL_BROKEN),
            "missing violation message": (13, TEMPORAL_START + TEMPORAL_FINISH),
            "success and violation": (13, TEMPORAL_BROKEN + SUCCESS),
            "success exit and violation": (0, TEMPORAL_BROKEN + SUCCESS),
            "two violations": (13, TEMPORAL_BROKEN + msg(2116, "Temporal properties were violated.")),
            "safety during a temporal run": (12, violated("A")),
            "temporal and safety": (13, TEMPORAL_BROKEN + violated("A")),
            "temporal and deadlock": (13, TEMPORAL_BROKEN + DEADLOCK),
        }
        for label, (status, output) in cases.items():
            with self.subTest(label), self.assertRaises(tlc.CheckError):
                tlc.classify(status, output, [], False, "Live")
        with self.assertRaises(tlc.CheckError):
            tlc.classify(13, TEMPORAL_BROKEN, [], False)
        with self.assertRaises(tlc.CheckError):
            tlc.classify(0, TEMPORAL_BROKEN + SUCCESS, ["A"], True)


class ConfigTest(unittest.TestCase):
    def test_property_keywords_and_render_keep_only_selected_rules(self) -> None:
        config = tlc.parse_config('SPECIFICATION Spec\nINVARIANT Safe\nPROPERTY Live\n'
                                  'PROPERTIES\n  Later\n  Last\nCHECK_DEADLOCK TRUE\n'
                                  '\\* PROPERTY Hidden\n')
        self.assertEqual(config.properties, ["Live", "Later", "Last"])
        self.assertEqual(config.rules(), ["Safe", "Live", "Later", "Last", "NoDeadlock"])
        safety = tlc.parse_config(config.render(["Safe"], True))
        self.assertEqual(safety.properties, [])
        self.assertEqual(safety.rules(), ["Safe", "NoDeadlock"])
        temporal = tlc.parse_config(config.render([], False, ["Later"]))
        self.assertEqual(temporal.rules(), ["Later"])
        self.assertIn("SPECIFICATION Spec", temporal.text)

    def test_property_names_and_specification_must_be_unambiguous(self) -> None:
        cases = ("PROPERTY\n", "PROPERTY Live,Later\n", "PROPERTY Live\nPROPERTY Live\n",
                 "INVARIANT Live\nPROPERTY Live\n", "PROPERTY NoDeadlock\n")
        for tail in cases:
            with self.subTest(tail), self.assertRaises(tlc.CheckError):
                tlc.parse_config("SPECIFICATION Spec\n" + tail)
        for spec in ("", "SPECIFICATION\n", "SPECIFICATION Spec Other\n",
                     "SPECIFICATION Spec\nSPECIFICATION Other\n",
                     "SPECIFICATION Spec\nINIT Init\nNEXT Next\n"):
            with self.subTest(spec), self.assertRaisesRegex(tlc.CheckError, "SPECIFICATION"):
                tlc.parse_config(spec + "PROPERTY Live\n")

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
        for char in ("\u00a0", "\v", "\f", "\u2028"):
            for text in (f"INIT Init\nNEXT Next\nINVARIANT Bad\n{char}", f"INVARIANT Bad\n\\* note\n{char}"):
                with self.subTest(text), self.assertRaisesRegex(tlc.CheckError, "outside a comment"):
                    tlc.parse_config(text)
            with self.subTest(f"{char!r} in comment bodies"):
                text = f"INIT Init\n(* a {char} *)\nINVARIANT A\n\\* b {char}"
                self.assertEqual(tlc.parse_config(text).rules(), ["A", "NoDeadlock"])

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


class LivenessCfgTest(unittest.TestCase):
    def check(self, outputs: list, expected: set[str],
              config: str = "SPECIFICATION Spec\nINVARIANT Known\nPROPERTY Live\n") -> tuple:
        rendered = []
        deadlines = []
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "M.tla").write_text("---- MODULE M ----\n====\n", encoding="utf-8")
            (root / "M.cfg").write_text(config, encoding="utf-8")

            def run(java, jar, workdir, module, name, deadline):
                rendered.append(tlc.parse_config((workdir / name).read_text(encoding="utf-8")))
                deadlines.append(deadline)
                output = outputs[len(rendered) - 1]
                if isinstance(output, Exception):
                    raise output
                return output

            with mock.patch.object(tlc, "run_tlc", side_effect=run) as runner:
                result = tlc.check_cfg(root, "M.cfg", "M", expected, "java", root / "tla2tools.jar", 60)
        return result, rendered, deadlines, runner.call_count

    def test_expected_liveness_violation_passes(self) -> None:
        result, rendered, _, count = self.check([(0, SUCCESS), (13, TEMPORAL_BROKEN)], {"Live"})
        self.assertEqual(result.problems(), [])
        self.assertEqual(result.broken, {"Live"})
        self.assertEqual(count, 2)
        self.assertEqual(rendered[1].rules(), ["Live"])

    def test_unexpected_liveness_violation_fails(self) -> None:
        result, _, _, _ = self.check([(0, SUCCESS), (13, TEMPORAL_BROKEN)], set())
        self.assertEqual(len(result.problems()), 1)
        self.assertIn("Live: newly broken", result.problems()[0])

    def test_expected_liveness_violation_now_holds_fails(self) -> None:
        result, _, _, _ = self.check([(0, SUCCESS), (0, TEMPORAL_SUCCESS)], {"Live"})
        self.assertEqual(len(result.problems()), 1)
        self.assertIn("Live: now holds", result.problems()[0])

    def test_properties_run_separately_after_safety_finishes(self) -> None:
        config = "SPECIFICATION Spec\nINVARIANT Known\nPROPERTIES Live Later\n"
        outputs = [(12, violated("Known")), (0, SUCCESS), (13, TEMPORAL_BROKEN), (0, TEMPORAL_SUCCESS)]
        result, rendered, deadlines, count = self.check(outputs, {"Known", "Live"}, config)
        self.assertEqual(result.problems(), [])
        self.assertEqual(count, 4)
        self.assertEqual([c.rules() for c in rendered], [["Known", "NoDeadlock"], ["NoDeadlock"], ["Live"], ["Later"]])
        self.assertEqual(len(set(deadlines)), 1)

    def test_liveness_without_specification_never_invokes_tlc(self) -> None:
        result, _, _, count = self.check([], set(), "INIT Init\nNEXT Next\nPROPERTY Live\n")
        self.assertEqual(count, 0)
        self.assertIn("SPECIFICATION", result.error or "")

    def test_ignored_property_and_timeout_are_errors(self) -> None:
        ignored, _, _, _ = self.check([(0, SUCCESS), (0, SUCCESS)], set())
        self.assertTrue(ignored.problems())
        self.assertIsNotNone(ignored.error)
        timeout, _, _, _ = self.check([(0, SUCCESS), tlc.CheckError("timed out")], set())
        self.assertIn("timed out", timeout.error or "")
        self.assertIn("limit 60 s per config", timeout.error or "")


class ExpectedTest(unittest.TestCase):
    def read(self, text: str) -> dict:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "expected.tsv"
            path.write_text(text, encoding="utf-8")
            return tlc.read_expected(path)

    def test_three_column_rows_and_optional_liveness_column(self) -> None:
        rows = self.read("old.cfg\tM\tKnown,NoDeadlock\nclean.cfg\tM\t-\n"
                         "live.cfg\tM\tKnown\tLive,Later\nfixed.cfg\tM\t-\t-\n")
        self.assertEqual(rows, {"old.cfg": ("M", {"Known", "NoDeadlock"}), "clean.cfg": ("M", set()),
                                "live.cfg": ("M", {"Known", "Live", "Later"}), "fixed.cfg": ("M", set())})

    def test_malformed_liveness_columns_fail(self) -> None:
        for text in ("a.cfg\tM\t-\tLive,\n", "a.cfg\tM\t-\tLive\textra\n"):
            with self.subTest(text), self.assertRaises(tlc.CheckError):
                self.read(text)


class ProblemsTest(unittest.TestCase):
    def test_new_and_fixed_rules_are_named_per_config(self) -> None:
        result = tlc.Result("a/M.cfg", {"Old"}, broken={"New"}, rules=["Old", "New"])
        new, fixed = result.problems()
        self.assertTrue(new.startswith("a/M.cfg: New:") and "newly broken" in new)
        self.assertTrue(fixed.startswith("a/M.cfg: Old:") and "now holds" in fixed)


if __name__ == "__main__":
    unittest.main()
