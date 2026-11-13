"""statecharts_tlc counts only a clean TLC finish or one named rule breaking."""

import importlib.util
import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import threading
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
    def test_symmetry_is_refused_only_for_properties(self) -> None:
        for rules in ("PROPERTY Live\nSYMMETRY Sym\n", "SYMMETRY Sym\nPROPERTIES Live Later\n"):
            with self.subTest(rules), self.assertRaisesRegex(tlc.CheckError, "SYMMETRY"):
                tlc.parse_config("SPECIFICATION Spec\n" + rules)
        config = tlc.parse_config("SPECIFICATION Spec\nSYMMETRY Sym\nINVARIANT Safe\n")
        self.assertEqual(config.rules(), ["Safe", "NoDeadlock"])
        self.assertIn("SYMMETRY Sym", config.render(["Safe"], True))

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
              config: str = "SPECIFICATION Spec\nINVARIANT Known\nPROPERTY Live\n",
              preflights: list | None = None) -> tuple:
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

            def preflight(java, jar, workdir, module, name, deadline):
                if preflights is not None:
                    preflights.append((tlc.parse_config((workdir / name).read_text(encoding="utf-8")), deadline))

            with mock.patch.object(tlc, "check_fairness", side_effect=preflight, create=True), \
                    mock.patch.object(tlc, "run_tlc", side_effect=run) as runner:
                result = tlc.check_cfg(root, "M.cfg", "M", expected, "java", root / "tla2tools.jar", 60)
        return result, rendered, deadlines, runner.call_count

    def test_fairness_preflight_keeps_all_properties_and_shares_deadline(self) -> None:
        preflights = []
        result, _, deadlines, count = self.check(
            [(0, SUCCESS), (13, TEMPORAL_BROKEN), (0, TEMPORAL_SUCCESS)], {"Live"},
            "SPECIFICATION Spec\nINVARIANT Known\nPROPERTIES Live Later\n", preflights)
        self.assertEqual(result.problems(), [])
        self.assertEqual(count, 3)
        self.assertEqual(len(preflights), 1)
        config, deadline = preflights[0]
        self.assertEqual(config.rules(), ["Live", "Later"])
        self.assertEqual(set(deadlines), {deadline})

    def test_symmetry_liveness_never_invokes_tlc_or_fairness(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "M.tla").write_text("---- MODULE M ----\n====\n", encoding="utf-8")
            (root / "M.cfg").write_text("SPECIFICATION Spec\nPROPERTY Live\nSYMMETRY Sym\n", encoding="utf-8")
            with mock.patch.object(tlc, "check_fairness", create=True) as fairness, \
                    mock.patch.object(tlc, "run_tlc", return_value=(0, SUCCESS)) as run:
                result = tlc.check_cfg(root, "M.cfg", "M", set(), "java", root / "tla2tools.jar", 60)
            self.assertIn("SYMMETRY", result.error or "")
            self.assertEqual(result.runs, 0)
            fairness.assert_not_called()
            run.assert_not_called()

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


class FairnessCheckTest(unittest.TestCase):
    def check(self, output: tuple[int, str] | Exception) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with mock.patch.object(tlc, "run_java", create=True) as run:
                if isinstance(output, Exception):
                    run.side_effect = output
                else:
                    run.return_value = output
                tlc.check_fairness("java", root / "tla2tools.jar", root, "M", "fairness.cfg", 123.0)
            self.assertEqual(run.call_count, 1)
            self.assertEqual(run.call_args.args[-1], 123.0)

    def test_preflight_requires_one_success_with_applied_assumptions_and_properties(self) -> None:
        good = "STATECHART_FAIRNESS_OK assumptions=5 properties=1\n"
        self.check((0, "SANY finished.\n" + good))
        for output in ((1, good), (0, "SANY finished.\n"),
                       (0, "STATECHART_FAIRNESS_OK assumptions=0 properties=1\n"),
                       (0, "STATECHART_FAIRNESS_OK assumptions=5 properties=0\n"), (0, good + good)):
            with self.subTest(output), self.assertRaises(tlc.CheckError):
                self.check(output)

    def test_preflight_timeout_is_an_error(self) -> None:
        with self.assertRaisesRegex(tlc.CheckError, "timed out"):
            self.check(tlc.CheckError("timed out"))


@unittest.skipUnless(os.environ.get("TLA2TOOLS"), "set TLA2TOOLS to run real TLC regressions")
class FairnessIntegrationTest(unittest.TestCase):
    def setUp(self) -> None:
        self.jar = Path(os.environ["TLA2TOOLS"]).expanduser().resolve()
        self.java = os.environ.get("JAVA", "java")

    def test_unused_fairness_definition_does_not_satisfy_selected_specification(self) -> None:
        source = SCRIPT_PATH.parents[2] / "docs/statecharts/execution-authority"
        original = (source / "ExecutionAuthority.tla").read_text(encoding="utf-8")
        spec = "Spec == Init /\\ [][Next]_vars /\\ Fairness"
        self.assertEqual(original.count(spec), 1)
        without_fairness = original.replace(spec, "Spec == Init /\\ [][Next]_vars")
        self.assertIn("Fairness ==", without_fairness)
        self.assertIn("WF_vars(TickObserves)", without_fairness)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "ExecutionAuthority.tla").write_text(without_fairness, encoding="utf-8")
            cfg = "ExecutionAuthorityMain.cfg"
            (root / cfg).write_bytes((source / cfg).read_bytes())
            (root / "expected.tsv").write_text(
                f"{cfg}\tExecutionAuthority\t-\tAdmissionResumes\n", encoding="utf-8")
            module, expected = tlc.read_expected(root / "expected.tsv")[cfg]
            with mock.patch.object(tlc, "run_tlc", wraps=tlc.run_tlc) as run:
                result = tlc.check_cfg(root, cfg, module, expected, self.java, self.jar, 60)
            self.assertIn("fairness", (result.error or "").lower())
            self.assertTrue(result.problems())
            self.assertEqual(result.broken, set())
            self.assertEqual(result.runs, 0)
            run.assert_not_called()

    def test_review_model_without_symmetry_detects_live_violation(self) -> None:
        model = r"""---- MODULE M ----
EXTENDS TLC
CONSTANTS a, b
VARIABLE x
Init == x \in {a, b}
Next == x' \in {a, b}
Spec == Init /\ [][Next]_x /\ WF_x(Next)
Live == \E p \in {a, b}: <>[] (x = p)
Sym == Permutations({a, b})
====
"""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "M.tla").write_text(model, encoding="utf-8")
            (root / "M.cfg").write_text("CONSTANTS a = a b = b\nSPECIFICATION Spec\nPROPERTY Live\n",
                                        encoding="utf-8")
            result = tlc.check_cfg(root, "M.cfg", "M", set(), self.java, self.jar, 60)
            self.assertIsNone(result.error)
            self.assertEqual(result.broken, {"Live"})
            self.assertIn("Live: newly broken", result.problems()[0])


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


class SelectionTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.expected = {
            "a/First.cfg": ("First", set()),
            "a/Variant.cfg": ("First", set()),
            "b/Second.cfg": ("Second", set()),
            "c/Other.cfg": ("Other", set()),
        }
        for cfg in self.expected:
            self.write(cfg, "INIT Init\nNEXT Next\n")
        self.write("expected.tsv", "".join(f"{cfg}\t{module}\t-\n" for cfg, (module, _) in self.expected.items()))
        self.write("a/First.tla", "---- MODULE First ----\nEXTENDS Bridge\n====\n")
        self.write("a/Bridge.tla", "---- MODULE Bridge ----\nS == INSTANCE Shared\n====\n")
        self.write("b/Second.tla", "---- MODULE Second ----\nLOCAL S ==\n INSTANCE Shared WITH x <- y\n====\n")
        self.write("shared/Shared.tla", "---- MODULE Shared ----\nEXTENDS Naturals\n====\n")
        self.write("c/Other.tla", '---- MODULE Other ----\n\\* EXTENDS Shared\n'
                   '(* outer (* INSTANCE Shared *) ignored *)\nText == "INSTANCE Shared"\n====\n')

    def write(self, path: str, text: str) -> None:
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")

    def select(self, *paths: str, previous: dict | None = None) -> list[str]:
        rows = None if previous is None else {
            cfg: f"{cfg}\t{module}\t{','.join(sorted(rules)) or '-'}"
            for cfg, (module, rules) in previous.items()
        }
        return tlc.select_configs(self.root, self.expected, list(paths), rows)

    def test_changed_cfg_selects_only_that_variant(self) -> None:
        self.assertEqual(self.select("docs/statecharts/a/Variant.cfg"), ["a/Variant.cfg"])

    def test_shared_module_selects_transitive_extends_and_instance_consumers_across_folders(self) -> None:
        self.assertEqual(self.select("docs/statecharts/shared/Shared.tla"),
                         ["a/First.cfg", "a/Variant.cfg", "b/Second.cfg"])

    def test_extends_lists_can_span_lines_and_dependency_cycles_terminate(self) -> None:
        self.write("a/First.tla", "---- MODULE First ----\nEXTENDS Naturals,\n Bridge, Shared\n====\n")
        self.write("a/Bridge.tla", "---- MODULE Bridge ----\nEXTENDS First\n====\n")
        self.assertEqual(self.select("docs/statecharts/a/Bridge.tla"), ["a/First.cfg", "a/Variant.cfg"])

    def test_module_names_can_start_with_digits(self) -> None:
        self.write("shared/1Shared.tla", "---- MODULE 1Shared ----\n====\n")
        self.write("a/First.tla", "---- MODULE First ----\nEXTENDS 1Shared\n====\n")
        self.write("b/Second.tla", "---- MODULE Second ----\nS == INSTANCE 1Shared\n====\n")
        self.assertEqual(self.select("docs/statecharts/shared/1Shared.tla"),
                         ["a/First.cfg", "a/Variant.cfg", "b/Second.cfg"])

    def test_unclosed_strings_cannot_silently_hide_dependencies(self) -> None:
        with self.assertRaisesRegex(tlc.CheckError, "string not closed"):
            tlc.module_dependencies('---- MODULE M ----\nText == "')

    def test_local_module_takes_precedence_over_same_name_in_another_folder(self) -> None:
        self.write("c/Shared.tla", "---- MODULE Shared ----\n====\n")
        self.write("c/Other.tla", "---- MODULE Other ----\nEXTENDS Shared\n====\n")
        self.assertEqual(self.select("docs/statecharts/shared/Shared.tla"),
                         ["a/First.cfg", "a/Variant.cfg", "b/Second.cfg"])

    def test_only_changed_expected_rows_select_configs(self) -> None:
        previous = dict(self.expected)
        previous["b/Second.cfg"] = ("Second", {"OldRule"})
        self.assertEqual(self.select("docs/statecharts/expected.tsv", previous=previous), ["b/Second.cfg"])
        self.assertEqual(self.select("docs/statecharts/expected.tsv", previous=self.expected), [])

    def test_expected_row_edits_select_even_when_the_rule_set_is_unchanged(self) -> None:
        previous = tlc.expected_rows((self.root / "expected.tsv").read_text(encoding="utf-8"))
        self.write("expected.tsv", "".join(f"{cfg}\t{module}\t-\t-\n" for cfg, (module, _) in self.expected.items()))
        self.assertEqual(tlc.select_configs(self.root, self.expected, ["docs/statecharts/expected.tsv"], previous),
                         sorted(self.expected))

    def test_missing_expected_history_falls_back_to_all(self) -> None:
        self.assertEqual(self.select("docs/statecharts/expected.tsv"), sorted(self.expected))

    def test_checker_changes_and_full_mode_select_all(self) -> None:
        for path in ("scripts/ci/statecharts_tlc.py", "scripts/ci/StatechartFairness.java",
                     ".github/workflows/statecharts-tlc.yml"):
            with self.subTest(path):
                self.assertEqual(self.select(path), sorted(self.expected))
        self.assertEqual(tlc.select_configs(self.root, self.expected, None), sorted(self.expected))

    def test_unrelated_files_select_nothing(self) -> None:
        self.assertEqual(self.select("README.md", "docs/statecharts/a/report.md"), [])
        self.assertEqual(self.select(), [])

    def test_full_selection_keeps_the_cfg_file_boundary(self) -> None:
        self.write("notes.txt", "not a config\n")
        expected = dict(self.expected, **{"notes.txt": ("Other", set())})
        self.assertEqual(tlc.select_configs(self.root, expected, None), sorted(self.expected))

    def test_deleted_shared_module_still_selects_its_consumers(self) -> None:
        (self.root / "shared/Shared.tla").unlink()
        self.assertEqual(self.select("docs/statecharts/shared/Shared.tla"),
                         ["a/First.cfg", "a/Variant.cfg", "b/Second.cfg"])

    def test_renamed_and_deleted_configs_only_select_existing_targets(self) -> None:
        (self.root / "a/Variant.cfg").rename(self.root / "a/Renamed.cfg")
        self.expected["a/Renamed.cfg"] = self.expected.pop("a/Variant.cfg")
        self.assertEqual(self.select("docs/statecharts/a/Variant.cfg", "docs/statecharts/a/Renamed.cfg"),
                         ["a/Renamed.cfg"])
        (self.root / "a/First.cfg").unlink()
        self.expected.pop("a/First.cfg")
        self.assertEqual(self.select("docs/statecharts/a/First.cfg"), [])


class SelectionCliTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.repo = Path(self.tmp.name)
        self.root = self.repo / "docs/statecharts"
        self.root.mkdir(parents=True)
        self.git("init", "-q")
        self.git("config", "user.name", "TLC tests")
        self.git("config", "user.email", "tlc@example.invalid")
        for cfg in ("M.cfg", "N.cfg"):
            (self.root / cfg).write_text("INIT Init\nNEXT Next\n", encoding="utf-8")
        self.expected = self.root / "expected.tsv"
        self.expected.write_text("M.cfg\tM\t-\nN.cfg\tN\t-\n", encoding="utf-8")
        self.commit()
        self.base = self.git("rev-parse", "HEAD").strip()

    def git(self, *args: str) -> str:
        return subprocess.run(["git", "-C", str(self.repo), *args], check=True,
                              capture_output=True, text=True).stdout

    def commit(self) -> None:
        self.git("add", ".")
        self.git("commit", "-qm", "fixture")

    def run_main(self, *args: str, barrier: threading.Barrier | None = None) -> tuple[int, str, str, mock.Mock]:
        out, err = io.StringIO(), io.StringIO()
        jar = self.repo / "tla2tools.jar"
        jar.touch()

        def check(root, cfg, module, rules, java, jar, timeout):
            if barrier is not None:
                barrier.wait(timeout=5)
            return tlc.Result(cfg, rules, broken=rules, rules=list(rules), runs=1)

        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err), \
                mock.patch.object(tlc, "REPO_ROOT", self.repo), \
                mock.patch.object(tlc, "check_cfg", side_effect=check) as checked:
            rc = tlc.main(["--root", str(self.root), "--jar", str(jar), "--base", self.base, *args])
        return rc, out.getvalue(), err.getvalue(), checked

    def diverge_main(self) -> None:
        checker = self.repo / "scripts/ci/statecharts_tlc.py"
        checker.parent.mkdir(parents=True)
        checker.write_text("baseline checker\n", encoding="utf-8")
        (self.root / "M.tla").write_text("---- MODULE M ----\n====\n", encoding="utf-8")
        self.commit()
        ancestor = self.git("rev-parse", "HEAD").strip()
        self.git("checkout", "-qb", "main-advanced")
        checker.write_text("main-only checker change\n", encoding="utf-8")
        (self.root / "M.tla").write_text("---- MODULE M ----\n\\* main-only model change\n====\n",
                                         encoding="utf-8")
        self.expected.write_text("M.cfg\tM\t-\nN.cfg\tN\tMainOnly\n", encoding="utf-8")
        self.commit()
        self.base = self.git("rev-parse", "HEAD").strip()
        self.git("checkout", "-qb", "pr-head", ancestor)

    def test_shared_module_change_reaches_main_and_preamble_quotes_fail_closed(self) -> None:
        for preamble in ("", 'Quotation mark: "\n'):
            with self.subTest(preamble=preamble):
                (self.root / "M.tla").write_text(
                    preamble + '---- MODULE M ----\nEXTENDS Shared\nVARIABLE x\n'
                    'Init == x = "ok"\nNext == x\' = x\nInv == Valid(x)\n====\n', encoding="utf-8")
                (self.root / "M.cfg").write_text(
                    "INIT Init\nNEXT Next\nINVARIANT Inv\nCHECK_DEADLOCK FALSE\n", encoding="utf-8")
                shared = self.root / "Shared.tla"
                shared.write_text('---- MODULE Shared ----\nValid(v) == v = "ok"\n====\n', encoding="utf-8")
                self.commit()
                self.base = self.git("rev-parse", "HEAD").strip()
                shared.write_text('---- MODULE Shared ----\nValid(v) == v = "bad"\n====\n', encoding="utf-8")
                self.commit()
                self.assertEqual(self.git("diff", "--name-only", f"{self.base}..HEAD").splitlines(),
                                 ["docs/statecharts/Shared.tla"])
                rc, out, err, checked = self.run_main()
                if preamble:
                    self.assertEqual(rc, 1)
                    self.assertIn("M.tla", err)
                    self.assertIn("MODULE header", err)
                    self.assertIn("preamble", err)
                    self.assertNotIn("NO_TARGETS", out)
                    checked.assert_not_called()
                else:
                    self.assertEqual((rc, err), (0, ""))
                    self.assertIn("Selected 1 configs", out)
                    self.assertEqual([call.args[1] for call in checked.call_args_list], ["M.cfg"])

    def test_document_only_pr_excludes_main_only_checker_and_model_changes(self) -> None:
        self.diverge_main()
        (self.root / "README.md").write_text("PR documentation only\n", encoding="utf-8")
        self.commit()
        rc, out, err, checked = self.run_main()
        self.assertEqual((rc, err), (1, ""))
        self.assertIn("Selected 0 configs", out)
        self.assertIn("NO_TARGETS", out)
        checked.assert_not_called()

    def test_pr_cfg_change_excludes_main_only_checker_and_model_changes(self) -> None:
        self.diverge_main()
        (self.root / "M.cfg").write_text("INIT Init\nNEXT Next\n\\* PR change\n", encoding="utf-8")
        self.commit()
        rc, out, err, checked = self.run_main()
        self.assertEqual((rc, err), (0, ""))
        self.assertIn("Selected 1 configs", out)
        self.assertEqual([call.args[1] for call in checked.call_args_list], ["M.cfg"])

    def test_expected_rows_use_merge_base_and_exclude_main_only_row_changes(self) -> None:
        self.diverge_main()
        self.expected.write_text("M.cfg\tM\tPrOnly\nN.cfg\tN\t-\n", encoding="utf-8")
        self.commit()
        rc, out, err, checked = self.run_main()
        self.assertEqual((rc, err), (0, ""))
        self.assertIn("Selected 1 configs", out)
        self.assertEqual([call.args[1] for call in checked.call_args_list], ["M.cfg"])
        self.assertIn("| M.cfg | PrOnly | PrOnly |", out)

    def test_shallow_history_without_merge_base_checks_all_instead_of_no_targets(self) -> None:
        (self.root / "README.md").write_text("PR documentation only\n", encoding="utf-8")
        self.commit()
        (self.repo / ".git/shallow").write_text(self.git("rev-parse", "HEAD"), encoding="utf-8")
        rc, out, err, checked = self.run_main()
        self.assertEqual(rc, 0)
        self.assertIn("FULL_FALLBACK", err)
        self.assertIn("merge-base", err)
        self.assertIn("Selected 2 configs", out)
        self.assertNotIn("NO_TARGETS", out)
        self.assertCountEqual([call.args[1] for call in checked.call_args_list], ["M.cfg", "N.cfg"])

    def test_cli_reads_base_expected_rows_and_preserves_broken_output(self) -> None:
        self.expected.write_text("M.cfg\tM\tKnown\nN.cfg\tN\t-\n", encoding="utf-8")
        self.commit()
        rc, out, err, checked = self.run_main("--jobs", "2")
        self.assertEqual((rc, err), (0, ""))
        self.assertEqual([call.args[1] for call in checked.call_args_list], ["M.cfg"])
        self.assertTrue(out.startswith("Selected 1 configs (changed):\n  M.cfg\n"))
        self.assertIn("broken=Known", out)
        self.assertIn("| M.cfg | Known | Known |", out)
        self.assertIn("1 configs", out)

    def test_parallel_full_fallback_checks_each_config_once_and_keeps_sorted_output(self) -> None:
        checker = self.repo / "scripts/ci/statecharts_tlc.py"
        checker.parent.mkdir(parents=True)
        checker.write_text("checker changed\n", encoding="utf-8")
        self.expected.write_text("M.cfg\tM\tKnown\nN.cfg\tN\tOther\n", encoding="utf-8")
        self.commit()
        rc, out, err, checked = self.run_main("--jobs", "2", barrier=threading.Barrier(2))
        self.assertEqual((rc, err), (0, ""))
        self.assertCountEqual([call.args[1] for call in checked.call_args_list], ["M.cfg", "N.cfg"])
        self.assertLess(out.index("ok   M.cfg"), out.index("ok   N.cfg"))
        self.assertIn("| M.cfg | Known | Known |", out)
        self.assertIn("| N.cfg | Other | Other |", out)

    def test_no_targets_is_explicit_and_never_claims_a_check_passed(self) -> None:
        (self.repo / "README.md").write_text("docs only\n", encoding="utf-8")
        self.commit()
        rc, out, err, checked = self.run_main()
        self.assertEqual((rc, err), (1, ""))
        checked.assert_not_called()
        self.assertIn("Selected 0 configs (changed):", out)
        self.assertIn("NO_TARGETS", out)
        self.assertNotIn("broken=", out)
        self.assertNotIn("| cfg |", out)

    def test_deleted_config_with_stale_expected_row_fails_even_without_targets(self) -> None:
        (self.root / "M.cfg").unlink()
        self.commit()
        rc, out, err, checked = self.run_main()
        self.assertEqual(rc, 1)
        self.assertIn("M.cfg: listed in expected.tsv but the file is gone", err)
        self.assertIn("NO_TARGETS", out)
        checked.assert_not_called()

    def test_git_rename_checks_new_config_and_clean_deletion_has_no_targets(self) -> None:
        (self.root / "M.cfg").rename(self.root / "Renamed.cfg")
        self.expected.write_text("Renamed.cfg\tM\t-\nN.cfg\tN\t-\n", encoding="utf-8")
        self.commit()
        rc, out, err, checked = self.run_main()
        self.assertEqual((rc, err), (0, ""))
        self.assertEqual([call.args[1] for call in checked.call_args_list], ["Renamed.cfg"])
        self.base = self.git("rev-parse", "HEAD").strip()
        (self.root / "Renamed.cfg").unlink()
        self.expected.write_text("N.cfg\tN\t-\n", encoding="utf-8")
        self.commit()
        rc, out, err, checked = self.run_main()
        self.assertEqual((rc, err), (1, ""))
        self.assertIn("NO_TARGETS", out)
        checked.assert_not_called()

    def test_unavailable_base_checks_all_with_an_explicit_fallback(self) -> None:
        self.base = "missing-base"
        rc, out, err, checked = self.run_main()
        self.assertEqual(rc, 0)
        self.assertIn("FULL_FALLBACK", err)
        self.assertIn("merge-base", err)
        self.assertNotIn("NO_TARGETS", out)
        self.assertCountEqual([call.args[1] for call in checked.call_args_list], ["M.cfg", "N.cfg"])


class ProblemsTest(unittest.TestCase):
    def test_new_and_fixed_rules_are_named_per_config(self) -> None:
        result = tlc.Result("a/M.cfg", {"Old"}, broken={"New"}, rules=["Old", "New"])
        new, fixed = result.problems()
        self.assertTrue(new.startswith("a/M.cfg: New:") and "newly broken" in new)
        self.assertTrue(fixed.startswith("a/M.cfg: Old:") and "now holds" in fixed)



class LeaseWrapperTest(unittest.TestCase):
    def setUp(self) -> None:
        source = SCRIPT_PATH.parents[2] / 'docs/statecharts/e2e-run-lease/check_models.py'
        spec = importlib.util.spec_from_file_location('lease_wrapper', source)
        self.wrapper = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.wrapper)
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.area = self.root / 'e2e-run-lease'
        self.area.mkdir()
        for name in self.wrapper.EXPECTED_CONFIGS:
            (self.area / name).write_text('SPECIFICATION Spec\n')
        (self.area / 'LeaseLifetime.tla').write_text('fixture spec')
        self.jar = self.root / 'tools.jar'
        self.jar.touch()
        self.rows = {f'e2e-run-lease/{name}': set() for name in self.wrapper.EXPECTED_CONFIGS}
        self.rows['e2e-run-lease/run-ttl_takeover.cfg'] = {'NoDualGateway', 'NoHolderTheft', 'NextRequestProgress'}
        self.write_expected()

    def write_expected(self) -> None:
        (self.root / 'expected.tsv').write_text(''.join(
            f'{name}\tLeaseLifetime\t{",".join(sorted(rules)) or "-"}\n'
            for name, rules in sorted(self.rows.items())))

    def test_shared_expected_values_preserve_full_rule_sets(self) -> None:
        loaded = self.wrapper.load_expectations(self.root)
        self.assertEqual(loaded['e2e-run-lease/run-ttl_takeover.cfg'][1],
                         {'NoDualGateway', 'NoHolderTheft', 'NextRequestProgress'})

    def test_missing_and_extra_registration_are_refused(self) -> None:
        self.rows.pop('e2e-run-lease/run-none.cfg')
        self.write_expected()
        with self.assertRaisesRegex(ValueError, 'incomplete canonical'):
            self.wrapper.load_expectations(self.root)
        self.rows['e2e-run-lease/unknown.cfg'] = set()
        self.write_expected()
        with self.assertRaisesRegex(ValueError, 'unexpected='):
            self.wrapper.load_expectations(self.root)

    def test_missing_config_is_refused(self) -> None:
        (self.area / 'run-none.cfg').unlink()
        with self.assertRaisesRegex(ValueError, 'incomplete TLC matrix'):
            self.wrapper.select_configs(self.area)

    def run_wrapper(self, mismatch=False):
        def checked(root, cfg, module, expected, java, jar, timeout):
            self.assertEqual(root, self.root)
            self.assertEqual(module, 'LeaseLifetime')
            self.assertEqual(expected, self.rows[cfg])
            broken = set(expected)
            if mismatch and cfg.endswith('/run-none.cfg'):
                broken.add('NoDualWriter')
            return self.wrapper.common.Result(cfg, set(expected), broken=broken,
                                              rules=sorted(set(expected) | broken), runs=2)
        out = self.root / 'results'
        args = ['checker', '--jar', str(self.jar), '--out', str(out)]
        with mock.patch.object(self.wrapper, 'BASE', self.area), \
             mock.patch.object(sys, 'argv', args), \
             mock.patch.object(self.wrapper.common, 'check_cfg', side_effect=checked) as check, \
             contextlib.redirect_stdout(io.StringIO()):
            status = self.wrapper.main()
        self.assertEqual(check.call_count, 23)
        return status, json.loads((out / 'results.json').read_text())

    def test_wrapper_delegates_every_config_to_common_checker(self) -> None:
        status, rows = self.run_wrapper()
        self.assertEqual(status, 0)
        self.assertEqual(len(rows), 23)
        self.assertTrue(all(row['matches_expectation'] for row in rows))
        row = next(row for row in rows if row['config'] == 'run-ttl_takeover.cfg')
        self.assertEqual(set(row['expected_rules']), self.rows['e2e-run-lease/run-ttl_takeover.cfg'])

    def test_common_mismatch_cannot_pass(self) -> None:
        status, rows = self.run_wrapper(mismatch=True)
        self.assertEqual(status, 1)
        self.assertFalse(next(row for row in rows if row['config'] == 'run-none.cfg')['matches_expectation'])

if __name__ == "__main__":
    unittest.main()
