"""Executable contracts for advisory Rust condition clustering."""
from __future__ import annotations

import importlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))


class DuplicateConditionsTests(unittest.TestCase):
    def setUp(self):
        self.check = importlib.import_module("audit_maintainability.checks.duplicate_conditions")
        self.temp = TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.write("src/lib.rs", "mod consumer;")

    def write(self, path, source):
        target = self.repo / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(source)

    def scan(self, source):
        self.write("src/consumer.rs", source)
        return self.check.scan(self.repo, mode="normalized")

    def repeated(self, expression, count=3):
        return "\n".join(f"fn f{i}(x: bool, y: bool) {{ if {expression} {{}} }}" for i in range(count))

    def test_two_sites_silent_three_sites_warn(self):
        self.assertEqual(self.scan(self.repeated("x && y", 2)), [])
        rows = self.scan(self.repeated("x && y"))
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["count"], 3)
        self.assertEqual(len(rows[0]["sites"]), 3)
        self.assertEqual(len(rows[0]["normalized_hash"]), 64)

    def test_default_warning_is_exact_and_normalized_is_opt_in(self):
        self.scan("""
            fn first(x: bool, y: bool) { if x && y {} }
            fn second(a: bool, b: bool) { if a && b {} }
            fn third(c: bool, d: bool) { if c && d {} }
        """)
        with mock.patch.object(self.check.common, "REPO_ROOT", self.repo):
            self.assertEqual(list(self.check._run(set())), [])
        self.assertEqual(self.check.scan(self.repo), [])
        self.assertEqual([r["count"] for r in self.check.scan(self.repo, mode="normalized")], [3])

    def test_cli_default_is_exact_and_normalized_is_opt_in(self):
        self.scan("""
            fn first(x: bool, y: bool) { if x && y {} }
            fn second(a: bool, b: bool) { if a && b {} }
            fn third(c: bool, d: bool) { if c && d {} }
        """)
        def run(*args):
            result = subprocess.run(
                [sys.executable, "-m", "audit_maintainability.checks.duplicate_conditions",
                 "--repo", str(self.repo), *args], cwd=ROOT, capture_output=True, text=True,
                env={**os.environ, "PYTHONPATH": str(ROOT / "scripts"),
                     "PYTHONPYCACHEPREFIX": str(self.repo / "cli-cache"),
                     "PYTHONDONTWRITEBYTECODE": "1"}, check=True)
            return json.loads(result.stdout)
        self.assertEqual(run(), [])
        self.assertEqual([r["count"] for r in run("--mode", "normalized")], [3])

    def test_generic_bound_parameters_are_alpha_locals(self):
        functions = [
            "fn first<F: Fn(bool) -> bool>(x: bool, y: bool, f: F) { if x && y {} }",
            "fn second<F: FnMut(bool) -> bool>(a: bool, b: bool, f: F) { if a && b {} }",
            "fn third<F: FnOnce(bool) -> bool>(c: bool, d: bool, f: F) { if c && d {} }",
        ]
        self.assertEqual(self.scan("\n".join(functions[:2])), [])
        self.assertEqual([r["count"] for r in self.scan("\n".join(functions))], [3])
        self.assertEqual(self.check.scan(self.repo, mode="exact"), [])

    def test_registered_predicate_is_extracted_across_different_guards(self):
        functions = [
            'fn a(channel: u64, ready: bool) { if ready && herdr_endpoint(select(channel, "(,)" )).is_none() {} }',
            'fn b(channel: u64, active: bool) { if active || herdr_endpoint(select(channel, "(,)" )).is_none() {} }',
            'fn c(channel: u64) { if provider == Provider::Claude && herdr_endpoint(select(channel, "(,)" )).is_none() {} }',
        ]
        self.assertEqual(self.scan("\n".join(functions[:2])), [])
        self.scan("\n".join(functions))
        for mode in ("exact", "normalized"):
            with self.subTest(mode=mode):
                rows = self.check.scan(self.repo, mode=mode)
                known = [r for r in rows if all(s["kind"] == "known_accessor" for s in r["sites"])]
                self.assertEqual([r["count"] for r in known], [3])
                self.assertTrue(all(s["expression"].startswith("herdr_endpoint(")
                                    and s["expression"].endswith(".is_none()") for s in known[0]["sites"]))

    def test_registered_predicate_keeps_polarity_arguments_and_fields(self):
        def guarded(expressions):
            guards = ("ready &&", "active ||", "provider == Provider::Claude &&")
            return "\n".join(f"fn f{i}(channel: Channel, ready: bool, active: bool) {{ if {guard} {expr} {{}} }}"
                             for i, (guard, expr) in enumerate(zip(guards, expressions)))
        base = "herdr_endpoint(channel.left).is_none()"
        for other in ("herdr_endpoint(channel.left).is_some()", "herdr_endpoint(channel.right).is_none()",
                      "herdr_endpoint(other(channel.left)).is_none()"):
            with self.subTest(other=other):
                self.assertEqual(self.scan(guarded([base, base, other])), [])
        self.assertEqual(self.scan(guarded(["other_endpoint(channel).is_none()"] * 3)), [])
        rows = self.scan(guarded([base] * 3) + guarded([base.replace("is_none", "is_some")] * 3))
        self.assertEqual(sorted(r["count"] for r in rows), [3, 3])

    def test_alpha_locals_and_accessor_argument_receiver_get(self):
        rows = self.scan("""
            use crate::config::session_hosts::herdr_endpoint as endpoint;
            fn a(channel: ChannelId, ready: bool) {
                if ready && endpoint(channel.get()).is_none() {}
            }
            fn b(target: u64, active: bool) {
                if active && crate::config::session_hosts::herdr_endpoint(target).is_none() {}
            }
            fn c(other: ChannelId, enabled: bool) {
                let local = other;
                if enabled && herdr_endpoint(local.get()).is_none() {}
            }
        """)
        self.assertEqual([r["count"] for r in rows], [3, 3])
        self.assertEqual({s["kind"] for r in rows for s in r["sites"]}, {"compound", "known_accessor"})
        self.assertTrue(all(not r["ambiguous"] for r in rows))
        self.assertEqual(self.check.scan(self.repo, mode="exact"), [])

    def test_registered_method_receiver_is_normalized(self):
        rows = self.scan("""
            fn a(c: u64, config: Config) { if config.herdr_endpoint(c).is_none() {} }
            fn b(d: u64) { if herdr_endpoint(d).is_none() {} }
            fn c(e: ChannelId) { if herdr_endpoint(e.get()).is_none() {} }
        """)
        self.assertEqual([r["count"] for r in rows], [3])

    def test_unknown_get_receiver_is_an_explicit_ambiguous_candidate(self):
        rows = self.scan(self.repeated("herdr_endpoint(x.get()).is_none()"))
        self.assertEqual([r["count"] for r in rows], [3])
        self.assertTrue(rows[0]["ambiguous"])
        self.assertIn("unverified ChannelId receiver", rows[0]["sites"][0]["normalization_notes"])
        self.assertEqual(self.scan(self.repeated("herdr_endpoint(factory().get()).is_none()", 2)
                                   + self.repeated("herdr_endpoint(factory()).is_none()", 1)), [])
        self.assertEqual(self.scan(self.repeated("herdr_endpoint(transform(x.get())).is_none()", 2)
                                   + self.repeated("herdr_endpoint(transform(x)).is_none()", 1)), [])

    def test_redundant_atom_parentheses_and_numeric_separators(self):
        source = self.repeated("(x) == 1_000 && y", 2) + self.repeated("x == 1000 && (y)", 1)
        self.assertEqual([r["count"] for r in self.scan(source)], [3])

    def test_match_guard_and_else_if_conditions(self):
        rows = self.scan("""
            fn a(x: bool, y: bool) { match value { Some(_) if x && y => {}, _ => {} } }
            fn b(a: bool, b: bool) { if false {} else if a && b {} }
            fn c(c: bool, d: bool) { let enabled = c && d; }
        """)
        self.assertEqual([r["count"] for r in rows], [3])
        rows = self.scan("""
            fn a(value: Option<u64>) { match value { Some(channel) if herdr_endpoint(channel).is_some() => {}, _ => {} } }
            fn b(value: Option<u64>) { match value { Some(id) if herdr_endpoint(id).is_some() => {}, _ => {} } }
            fn c(value: Option<u64>) { match value { Some(target) if herdr_endpoint(target).is_some() => {}, _ => {} } }
        """)
        self.assertEqual([r["count"] for r in rows], [3])

    def test_known_host_seed_remains_visible_without_boolean_connective(self):
        rows = self.scan("""
            fn a(channel_id: ChannelId) { if crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_some() {} }
            fn b(channel: u64) { if crate::config::session_hosts::herdr_endpoint(channel).is_some() {} }
            fn c(thread_channel_id: ChannelId) { if crate::config::session_hosts::herdr_endpoint(thread_channel_id.get()).is_some() {} }
        """)
        self.assertEqual([r["count"] for r in rows], [3])
        self.assertEqual({s["kind"] for s in rows[0]["sites"]}, {"known_accessor"})

    def test_literals_order_fields_and_variable_identity_are_preserved(self):
        source = self.repeated('x.left == "a" && y.right == "b"', 2)
        for different in ('x.left == "z" && y.right == "b"',
                          'y.right == "b" && x.left == "a"',
                          'x.other == "a" && y.right == "b"',
                          'x.left == "a" && x.right == "b"'):
            with self.subTest(different=different):
                self.assertEqual(self.scan(source + self.repeated(different, 1)), [])

    def test_order_of_calls_is_preserved(self):
        for forward, reverse in (("first(x) && second(y)", "second(y) && first(x)"),
                                 ("first() && second()", "second() && first()")):
            with self.subTest(forward=forward):
                source = self.repeated(forward, 2)
                self.assertEqual(self.scan(source + self.repeated(reverse, 1)), [])

    def test_get_and_fields_outside_registered_accessor_stay_distinct(self):
        source = self.repeated("x.get() == 1 && y.ready", 2)
        self.assertEqual(self.scan(source + self.repeated("x == 1 && y.ready", 1)), [])
        source = self.repeated("herdr_endpoint(x.left).is_none() && y", 2)
        self.assertEqual(self.scan(source + self.repeated("herdr_endpoint(x.right).is_none() && y", 1)), [])
        source = self.repeated("x.left.herdr_endpoint(y).is_none()", 2)
        self.assertEqual(self.scan(source + self.repeated("x.right.herdr_endpoint(y).is_none()", 1)), [])
        source = self.repeated("factory().herdr_endpoint(y).is_none()", 2)
        self.assertEqual(self.scan(source + self.repeated("other_factory().herdr_endpoint(y).is_none()", 1)), [])

    def test_plain_helper_reuse_is_not_copied_logic(self):
        self.assertEqual(self.scan(self.repeated("shared_policy(x, y)", 5)), [])
        self.assertEqual(self.scan(self.repeated("x.is_some()", 5)), [])
        self.assertEqual(self.scan(self.repeated("x.unwrap_or_else(|| y)", 5)), [])
        self.assertEqual(self.scan(self.repeated("move || async move { run(); }", 5)), [])

    def test_string_code_alone_cannot_supply_the_third_site(self):
        source = self.repeated("x && y", 2) + '''
            const TEXT: &str = r###"fn fake(x:bool,y:bool) { if x && y {} }"###;
        '''
        self.assertEqual(self.scan(source), [])

    def test_incomplete_let_chain_prefix_is_not_a_condition(self):
        self.assertEqual(self.scan(self.repeated("x && let Some(y) = optional()", 3)), [])

    def test_strings_comments_and_inline_test_code_do_not_add_sites(self):
        source = self.repeated("x && y", 2) + '''
            const TEXT: &str = r###"fn fake(x:bool,y:bool) { if x && y {} }"###;
            /* if x && y {} /* nested */ */
            #[cfg(test)] fn test_only(x: bool, y: bool) { if x && y {} }
            #[cfg(test)] mod tests { fn nested(x: bool, y: bool) { if x && y {} } }
        '''
        self.assertEqual(self.scan(source), [])

    def test_conditions_in_let_tail_parentheses_and_while(self):
        rows = self.scan("""
            fn a(x: bool, y: bool) { let live = (x && y).then(|| 1); }
            fn b(a: bool, b: bool) -> bool { a && b }
            fn c(c: bool, d: bool) { while c && d {} }
        """)
        self.assertEqual([r["count"] for r in rows], [3])

    def test_parentheses_and_short_circuit_grouping_are_preserved(self):
        self.assertEqual(self.scan(self.repeated("x && (y || x)", 2)
                                   + self.repeated("(x && y) || x", 1)), [])

    def test_closure_bound_variables_are_alpha_renamed(self):
        rows = self.scan("""
            fn a(x: Option<u64>) { if x.is_some_and(|id| id > 0 && id < 10) {} }
            fn b(y: Option<u64>) { if y.is_some_and(|value| value > 0 && value < 10) {} }
            fn c(z: Option<u64>) { if z.is_some_and(|n| n > 0 && n < 10) {} }
        """)
        self.assertEqual([r["count"] for r in rows], [3])
        rows = self.scan("""
            fn a(x: bool, y: Option<u64>) { if x && y.is_some_and(|id| herdr_endpoint(id.get()).is_none()) {} }
            fn b(a: bool, b: Option<u64>) { if a && b.is_some_and(|n| herdr_endpoint(n).is_none()) {} }
            fn c(c: bool, d: Option<u64>) { if c && d.is_some_and(|value| herdr_endpoint(value.get()).is_none()) {} }
        """)
        self.assertTrue(any(r["count"] == 3 and "is_some_and" in r["sites"][0]["expression"] for r in rows))

    def test_cross_file_module_graph_and_aliased_accessor(self):
        self.write("src/lib.rs", "mod consumer; mod other; mod last; mod test_support;")
        self.write("src/other.rs", "pub use crate::config::session_hosts::herdr_endpoint as endpoint;"
                   "fn b(c: u64) { if endpoint(c).is_none() {} }")
        self.write("src/last.rs", "use crate::other::endpoint as policy;"
                   "fn c(id: u64) { if policy(id).is_none() {} }")
        self.write("src/test_support.rs", "#![cfg(test)]\nfn t(id:u64) { if herdr_endpoint(id).is_none() {} }")
        rows = self.scan("fn a(c: u64) { if herdr_endpoint(c).is_none() {} }")
        self.assertEqual([r["count"] for r in rows], [3])
        self.assertEqual({s["path"] for s in rows[0]["sites"]},
                         {"src/consumer.rs", "src/other.rs", "src/last.rs"})

    def test_unbound_symbols_are_not_alpha_locals(self):
        source = self.repeated("GLOBAL_ONE && x", 2)
        self.assertEqual(self.scan(source + self.repeated("GLOBAL_TWO && x", 1)), [])
        self.assertEqual(self.scan(self.repeated("global_one && x", 2)
                                   + self.repeated("global_two && x", 1)), [])
        source = """
            fn a(x: bool) { match flag { true if x && true => {}, _ => {} } }
            fn b(y: bool) { match flag { true if y && true => {}, _ => {} } }
            fn c(z: bool) { match flag { false if z && false => {}, _ => {} } }
        """
        self.assertEqual(self.scan(source), [])

    def test_findings_and_scan_failure_remain_visible_warnings(self):
        self.scan(self.repeated("x && y"))
        with mock.patch.object(self.check.common, "REPO_ROOT", self.repo):
            findings = list(self.check._run(set()))
        self.assertEqual([f.severity for f in findings], ["warn"])
        self.assertFalse(self.check.CHECK.hard_gate)
        self.assertEqual(findings[0].extra["count"], 3)
        with mock.patch.object(self.check, "scan", side_effect=ValueError("broken source graph")):
            findings = list(self.check._run(set()))
        self.assertEqual([f.severity for f in findings], ["warn"])
        self.assertIn("scan failed", findings[0].message)

    def test_audit_and_ci_prepush_wiring_keeps_findings_advisory(self):
        spec = importlib.util.spec_from_file_location("condition_audit", ROOT / "scripts/audit_maintainability.py")
        harness = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(harness)
        self.assertIn(self.check.CHECK, harness.load_check_specs())
        self.scan(self.repeated("x && y"))
        with mock.patch.object(self.check.common, "REPO_ROOT", self.repo), \
                mock.patch.object(harness, "load_check_specs", return_value=[self.check.CHECK]), \
                mock.patch("sys.stdout", new_callable=io.StringIO), \
                mock.patch("sys.stderr", new_callable=io.StringIO) as stderr:
            self.assertEqual(harness.main(["--check"]), 0)
        self.assertIn("warning `duplicate_conditions`", stderr.getvalue())
        aggregate = (ROOT / "scripts/ci-script-checks.sh").read_text()
        self.assertIn('if run_check guards "Repeated condition advisory tests"; then\n'
                      '"$PYTHON" -m unittest tests.test_duplicate_conditions\nfi', aggregate)
        self.assertIn('"$PYTHON" scripts/audit_maintainability.py --check', aggregate)

    def test_closure_shadowing_does_not_merge_outer_and_inner_bindings(self):
        source = self.repeated("x && y.is_some_and(|x| x && y)", 2)
        rows = self.scan(source + self.repeated("x && y.is_some_and(|z| x && y)", 1))
        self.assertFalse(any("is_some_and" in s["expression"] for r in rows for s in r["sites"]))


if __name__ == "__main__":
    unittest.main()
