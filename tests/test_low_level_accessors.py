"""Rust fixtures for accessor scope and per-site no-growth contracts."""
from __future__ import annotations

from copy import deepcopy
import importlib
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT / "scripts") not in sys.path:
    sys.path.insert(0, str(ROOT / "scripts"))


class LowLevelAccessorsTest(unittest.TestCase):
    def setUp(self):
        try:
            self.check = importlib.import_module("audit_maintainability.checks.low_level_accessors")
        except ModuleNotFoundError as exc:
            if exc.name != "audit_maintainability.checks.low_level_accessors":
                raise
            self.fail("accessor scanner is not implemented")
        self.temp = TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.write("src/lib.rs", "mod consumer;\n")

    def write(self, name, text):
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    def scan(self, source, *, path="src/consumer.rs"):
        self.write(path, source)
        return self.check.scan(self.repo)

    def count(self, rows, rule=None):
        return sum(row["count"] for row in rows if rule is None or row["rule"] == rule)

    def row(self, **changes):
        row = {
            "rule": "herdr_endpoint", "accessor": "herdr_endpoint",
            "owned_paths": ["src/config/session_hosts.rs", "src/config/session_hosts/"],
            "path": "src/consumer.rs", "enclosing_fn": "old",
            "normalized_hash": "a" * 64, "count": 1, "line": 4,
        }
        row.update(changes)
        return row

    def test_audit_runner_returns_error_finding_for_new_site(self):
        generator = importlib.import_module("audit_maintainability.checks.low_level_accessor_baseline")
        accepted = self.scan("fn original() { herdr_endpoint(channel); }")
        generator.write_baselines(self.repo, accepted)
        with mock.patch.object(self.check.common, "REPO_ROOT", self.repo):
            initial = list(self.check._run(set()))
            self.assertFalse([finding for finding in initial if finding.severity == "error"])
            self.write("src/consumer.rs", """
                fn original() { herdr_endpoint(channel); }
                fn added() { herdr_endpoint(other); }
            """)
            findings = list(self.check._run(set()))
        self.assertTrue(any(finding.rule == "low_level_accessors" and finding.severity == "error"
                            for finding in findings), findings)
        self.assertTrue(any(finding.severity == "info" and finding.extra.get("enclosing_fn") == "added"
                            for finding in findings), findings)

    def test_audit_runner_returns_error_finding_when_baseline_is_missing(self):
        self.scan("fn original() { herdr_endpoint(channel); }")
        with mock.patch.object(self.check.common, "REPO_ROOT", self.repo):
            findings = list(self.check._run(set()))
        self.assertTrue(any(finding.rule == "low_level_accessors" and finding.severity == "error"
                            for finding in findings), findings)
        self.assertFalse((self.repo / "scripts/audit_maintainability/baselines/generated/low_level_accessors").exists())

    def test_new_site_and_same_total_replacement_are_errors(self):
        old = self.scan("fn old() { crate::config::session_hosts::herdr_endpoint(channel); }")
        self.assertTrue(self.check.compare([], old))
        new = self.scan("fn new() { crate::config::session_hosts::herdr_endpoint(channel); }")
        self.assertEqual(self.count(old), self.count(new))
        self.assertTrue(self.check.compare(old, new))

    def test_same_function_same_count_changed_argument_is_error(self):
        old = self.scan("fn caller() { herdr_endpoint(11); }")
        new = self.scan("fn caller() { herdr_endpoint(12); }")
        self.assertEqual(self.count(old), self.count(new))
        self.assertNotEqual(old[0]["normalized_hash"], new[0]["normalized_hash"])
        self.assertTrue(self.check.compare(old, new))

    def test_inline_module_same_named_function_replacement_is_new_site(self):
        old = self.scan("""
            mod a { fn run() { herdr_endpoint(1); } }
            mod b { fn run() {} }
        """)
        new = self.scan("""
            mod a { fn run() {} }
            mod b { fn run() { herdr_endpoint(1); } }
        """)
        self.assertEqual(self.count(old), 1)
        self.assertEqual(self.count(new), 1)
        self.assertTrue(self.check.compare(old, new))

    def test_impl_same_named_function_replacement_is_new_site(self):
        old = self.scan("""
            struct A; struct B;
            impl A { fn run() { herdr_endpoint(1); } }
            impl B { fn run() {} }
        """)
        new = self.scan("""
            struct A; struct B;
            impl A { fn run() {} }
            impl B { fn run() { herdr_endpoint(1); } }
        """)
        self.assertEqual(self.count(old), 1)
        self.assertEqual(self.count(new), 1)
        self.assertTrue(self.check.compare(old, new))

    def test_nested_same_named_function_replacement_is_new_site(self):
        old = self.scan("""
            fn a() { fn inner() { herdr_endpoint(id); } }
            fn b() { fn inner() {} }
        """)
        new = self.scan("""
            fn a() { fn inner() {} }
            fn b() { fn inner() { herdr_endpoint(id); } }
        """)
        self.assertEqual(self.count(old), 1)
        self.assertEqual(self.count(new), 1)
        self.assertTrue(self.check.compare(old, new))

    def test_cross_file_super_import_alias_is_detected(self):
        self.write("src/lib.rs", """
            use crate::config::session_hosts::herdr_endpoint as raw;
            mod child;
        """)
        self.write("src/child.rs", "use super::raw as alias; fn caller() { alias(1); }")
        rows = self.check.scan(self.repo)
        self.assertEqual(self.count(rows, "herdr_endpoint"), 1)
        self.assertEqual(rows[0]["path"], "src/child.rs")

    def test_duplicate_call_count_increase_is_error(self):
        old = self.scan("fn caller() { crate::config::session_hosts::herdr_endpoint(channel); }")
        new = self.scan("""fn caller() {
            crate::config::session_hosts::herdr_endpoint(channel);
            crate::config::session_hosts::herdr_endpoint(channel);
        }""")
        self.assertEqual(self.count(new), 2)
        self.assertTrue(self.check.compare(old, new))

    def test_qualified_multiline_call_is_not_a_zero_pass(self):
        rows = self.scan("""fn caller() {
            crate :: config :: session_hosts ::
                herdr_endpoint
                (
                    channel.get()
                ).is_some();
        }""")
        self.assertEqual(self.count(rows, "herdr_endpoint"), 1)
        self.assertTrue(self.check.compare([], rows))

    def test_function_import_alias_is_detected(self):
        rows = self.scan("""
            use crate::config::session_hosts::herdr_endpoint as lookup;
            fn caller() { lookup(channel); }
        """)
        self.assertEqual(self.count(rows, "herdr_endpoint"), 1)
        self.assertTrue(self.check.compare([], rows))

    def test_grouped_function_and_module_aliases_are_detected(self):
        rows = self.scan("""
            use crate::config::{session_hosts as hosts};
            use crate::config::session_hosts::{with_boot as inspect, configured_channel_ids as ids};
            fn caller() { hosts::herdr_endpoint(channel); inspect(|boot| boot.is_some()); ids(); }
        """)
        self.assertEqual(self.count(rows, "herdr_endpoint"), 1)
        self.assertEqual(self.count(rows, "with_boot"), 1)
        self.assertEqual(self.count(rows, "configured_channel_ids"), 1)

    def test_method_and_function_pointer_references_are_detected(self):
        rows = self.scan("""fn caller(boot: &BootSessionHosts) {
            let lookup = crate::config::session_hosts::herdr_endpoint;
            let _ = boot.herdr_endpoint(channel);
            let _ = boot.channels();
        }""")
        self.assertEqual(self.count(rows, "herdr_endpoint"), 2)
        self.assertEqual(self.count(rows, "boot_channels"), 1)

    def test_typed_host_inventory_receiver_is_detected_after_rename(self):
        rows = self.scan("""fn caller(hosts: &crate::config::session_hosts::BootSessionHosts) {
            hosts.channels();
        }""")
        self.assertEqual(self.count(rows, "boot_channels"), 1)

    def test_imported_host_inventory_type_alias_is_detected(self):
        rows = self.scan("""
            use crate::config::session_hosts::BootSessionHosts as Hosts;
            fn caller(hosts: &Hosts) { hosts.channels(); }
        """)
        self.assertEqual(self.count(rows, "boot_channels"), 1)

    def test_with_boot_closure_receiver_chain_is_detected(self):
        rows = self.scan("""fn caller() {
            crate::config::session_hosts::with_boot(|hosts| hosts.unwrap().channels());
        }""")
        self.assertEqual(self.count(rows, "with_boot"), 1)
        self.assertEqual(self.count(rows, "boot_channels"), 1)

    def test_typed_output_inventory_named_boot_is_not_a_host_accessor(self):
        rows = self.scan("""fn caller(boot: &crate::services::tui_o::channel_policy::BootChannels) {
            boot.channels();
        }""")
        self.assertEqual(self.count(rows, "boot_channels"), 0)
        self.assertEqual(rows, [])

    def test_turbofish_qualified_and_imported_accessors_are_detected(self):
        rows = self.scan("""
            use crate::config::session_hosts::with_boot;
            fn qualified() { crate::config::session_hosts::with_boot::<bool>(|boot| boot.is_some()); }
            fn imported() { with_boot::<bool>(|boot| boot.is_some()); }
        """)
        self.assertEqual(self.count(rows, "with_boot"), 2)

    def test_nested_super_import_alias_is_detected(self):
        rows = self.scan("""
            use crate::config::session_hosts::herdr_endpoint as raw;
            mod child {
                use super::raw as alias;
                fn caller() { alias(1); }
            }
        """)
        self.assertEqual(self.count(rows, "herdr_endpoint"), 1)

    def test_associated_boot_channels_call_is_detected(self):
        rows = self.scan("""
            use crate::config::session_hosts::BootSessionHosts;
            fn caller(boot: &BootSessionHosts) { BootSessionHosts::channels(boot); }
        """)
        self.assertEqual(self.count(rows, "boot_channels"), 1)

    def test_operator_and_character_changes_preserve_hash_distinctions(self):
        for first, second in (("channel + 1", "channel - 1"), ("'a'", "'b'")):
            with self.subTest(first=first, second=second):
                old = self.scan("fn caller() { herdr_endpoint(" + first + "); }")
                new = self.scan("fn caller() { herdr_endpoint(" + second + "); }")
                self.assertEqual(self.count(old), 1)
                self.assertEqual(self.count(new), 1)
                self.assertNotEqual(old[0]["normalized_hash"], new[0]["normalized_hash"])
                self.assertTrue(self.check.compare(old, new))

    def test_boot_receiver_type_bindings_do_not_leak_between_functions(self):
        host = "fn host(boot: &crate::config::session_hosts::BootSessionHosts) { boot.channels(); }"
        output = "fn output(boot: &crate::services::tui_o::channel_policy::BootChannels) { boot.channels(); }"
        for functions in ((host, output), (output, host)):
            with self.subTest(first=functions[0]):
                rows = self.scan("\n".join(functions))
                self.assertEqual(self.count(rows, "boot_channels"), 1)
                self.assertEqual({row["enclosing_fn"] for row in rows if row["rule"] == "boot_channels"},
                                 {"host"})

    def test_typed_closure_boot_binding_does_not_hide_outer_host_receiver(self):
        for body in ("boot.channels()", "{ boot.channels() }"):
            with self.subTest(body=body):
                rows = self.scan(
                    "fn host(boot: &crate::config::session_hosts::BootSessionHosts) {\n"
                    "    let output = |boot: &crate::services::tui_o::channel_policy::BootChannels| "
                    + body + ";\n    boot.channels();\n}"
                )
                host_rows = [row for row in rows if row["rule"] == "boot_channels"]
                self.assertEqual(self.count(host_rows), 1)
                self.assertEqual(host_rows[0]["enclosing_fn"], "host")
                self.assertEqual(host_rows[0]["line"], 3)

    def test_registered_launch_and_inventory_accessors_are_detected(self):
        rows = self.scan("""fn caller() {
            crate::services::herdr_launch::herdr_configured_for_tui_launch(Some(channel));
            crate::config::session_hosts::configured_channel_ids();
            crate::config::session_hosts::with_boot(|boot| boot.is_some());
        }""")
        self.assertEqual(self.count(rows), 3)
        self.assertEqual({row["rule"] for row in rows},
                         {"herdr_configured_for_tui_launch", "configured_channel_ids", "with_boot"})

    def test_owner_module_and_nested_owned_file_are_excluded(self):
        self.write("src/lib.rs", "mod config { pub mod session_hosts; }\nmod consumer;\n")
        self.write("src/config/session_hosts.rs", """
            mod internal;
            fn inside() { herdr_endpoint(channel); with_boot(|boot| boot.channels()); }
        """)
        self.write("src/config/session_hosts/internal.rs", "fn inside() { herdr_endpoint(channel); }")
        self.assertEqual(self.scan("fn caller() { common_policy(channel); }"), [])

    def test_owner_prefix_does_not_exempt_a_similarly_named_module(self):
        self.write("src/lib.rs", "mod config { mod session_hosts_copy; }\nmod consumer;\n")
        self.write("src/config/session_hosts_copy.rs", "fn caller() { herdr_endpoint(channel); }")
        self.assertEqual(self.count(self.scan("")), 1)

    def test_launch_consumer_is_not_a_raw_endpoint_owner(self):
        self.write("src/lib.rs", "mod services { mod herdr_launch; }\nmod consumer;\n")
        self.write("src/services/herdr_launch.rs", """fn caller() {
            herdr_configured_for_tui_launch(Some(channel));
            crate::config::session_hosts::herdr_endpoint(channel);
        }""")
        rows = self.scan("")
        self.assertEqual(self.count(rows, "herdr_configured_for_tui_launch"), 0)
        self.assertEqual(self.count(rows, "herdr_endpoint"), 1)

    def test_inline_cfg_test_and_test_only_module_are_excluded(self):
        rows = self.scan("""
            #[cfg(test)]
            mod tests { fn ignored() { herdr_endpoint(channel); } }
            #[cfg(all(test, feature = "extra"))]
            fn ignored_too() { configured_channel_ids(); }
            fn caller() { common_policy(channel); }
        """)
        self.assertEqual(rows, [])
        self.write("src/lib.rs", "#[cfg(test)] mod consumer;\n")
        self.assertEqual(self.scan("fn ignored() { herdr_endpoint(channel); }"), [])

    def test_mixed_line_inline_test_does_not_hide_following_production(self):
        rows = self.scan("""#[cfg(test)] fn ignored() { herdr_endpoint(test_channel); }
            fn real() { herdr_endpoint(channel); }
        """)
        self.assertEqual(self.count(rows), 1)
        self.assertEqual(rows[0]["enclosing_fn"], "real")

    def test_same_line_inline_test_exclusion_preserves_production_suffix(self):
        rows = self.scan("#[cfg(test)] fn ignored() { herdr_endpoint(test_channel); } "
                         "fn real() { herdr_endpoint(channel); }")
        self.assertEqual(self.count(rows), 1)
        self.assertEqual(rows[0]["enclosing_fn"], "real")

    def test_test_word_in_a_production_filename_does_not_hide_calls(self):
        self.write("src/lib.rs", "mod policy_tests;\n")
        rows = self.scan("fn real() { herdr_endpoint(channel); }", path="src/policy_tests.rs")
        self.assertEqual(self.count(rows, "herdr_endpoint"), 1)

    def test_definitions_imports_strings_and_nested_comments_are_excluded(self):
        rows = self.scan('''
            use crate::config::session_hosts::herdr_endpoint as lookup;
            fn herdr_endpoint(channel: u64) {}
            fn caller() {
                let normal = "herdr_endpoint(channel)";
                let raw = r###"{ with_boot(boot) configured_channel_ids()"###;
                let character = '}';
                /* outer herdr_endpoint(channel); /* nested with_boot(boot) */ */
                // lookup(channel);
            }
        ''')
        self.assertEqual(rows, [])

    def test_common_helper_reuse_and_o_output_membership_are_out_of_scope(self):
        rows = self.scan("""fn caller(snapshot: &OutputSnapshot) {
            common_policy(channel); common_policy(other); common_policy(third);
            snapshot.channels();
            crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel(channel, None);
        }""")
        self.assertEqual(rows, [])

    def test_whitespace_comment_and_line_changes_preserve_site_identity(self):
        old = self.scan("fn caller() { herdr_endpoint(channel.get()); }")
        new = self.scan("""// shifted line
            fn caller() { herdr_endpoint /* comment */ (
                channel . get ( )
            ); }
        """)
        self.assertEqual(self.count(old), 1)
        self.assertEqual(old[0]["normalized_hash"], new[0]["normalized_hash"])
        self.assertFalse(self.check.compare(old, new))

    def test_nested_function_enclosing_scope_restores_outer_name(self):
        rows = self.scan("""fn outer() {
            fn inner() { herdr_endpoint(inside); }
            herdr_endpoint(outside);
        }""")
        self.assertEqual({row["enclosing_fn"] for row in rows}, {"outer", "outer::inner"})

    def test_relabel_allows_only_identical_hash_multiset(self):
        old = [self.row()]
        moved = [self.row(path="src/moved.rs", enclosing_fn="renamed", line=40)]
        self.assertTrue(self.check.compare(old, moved))
        self.assertFalse(self.check.compare(old, moved, relabel=True))
        changed = [dict(moved[0], normalized_hash="b" * 64)]
        self.assertTrue(self.check.compare(old, changed, relabel=True))

    def test_relabel_does_not_accept_accessor_or_owner_scope_changes(self):
        old = [self.row()]
        for changed in (self.row(accessor="different_accessor"),
                        self.row(owned_paths=["src/"])):
            with self.subTest(changed=changed):
                self.assertTrue(self.check.compare(old, [changed], relabel=True))

    def test_relabel_rejects_copied_call_count_increase(self):
        old = [self.row()]
        copied = [self.row(path="src/moved.rs", count=2)]
        self.assertTrue(self.check.compare(old, copied, relabel=True))

    def test_relabel_rejects_equal_total_with_hash_multiplicity_exchange(self):
        old = [self.row(count=2), self.row(normalized_hash="b" * 64)]
        new = [self.row(count=1), self.row(normalized_hash="b" * 64, count=2)]
        self.assertEqual(self.count(old), self.count(new))
        self.assertTrue(self.check.compare(old, new, relabel=True))

    def test_reduce_only_deletes_or_decreases_existing_site_counts(self):
        old = [self.row(count=3), self.row(enclosing_fn="other")]
        original = deepcopy(old)
        self.assertFalse(self.check.compare(old, []))
        self.assertFalse(self.check.compare(old, [self.row(count=2)]))
        self.assertTrue(self.check.compare(old, [self.row(count=4)]))
        self.assertTrue(self.check.compare(old, [self.row(enclosing_fn="new")]))
        self.assertEqual(old, original)


if __name__ == "__main__":
    unittest.main()
