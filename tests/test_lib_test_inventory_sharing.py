"""Shared compiled-oracle use preserves inventory and relay floor failures."""

from __future__ import annotations

import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]


def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts" / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


integrity = load("check_test_target_integrity")
relay = load("check_relay_authority_contract")


class SharedCompiledOracle(unittest.TestCase):
    def check_shared_inventory(self, listing, static_only, cargo_only):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "src").mkdir()
            (root / "Cargo.toml").write_text('[package]\nname = "fixture"\n[lib]\npath = "src/lib.rs"\n')
            (root / "src/lib.rs").write_text("#[cfg(test)] mod tests { #[test] fn plain() {} }\n")
            helper = SimpleNamespace(validated_inventory=mock.Mock(return_value=listing))
            runner = mock.Mock(side_effect=AssertionError("shared oracle must avoid cargo"))
            with mock.patch.dict(os.environ, {"ADK_LIB_TEST_ARTIFACT": "/artifact"}), \
                 mock.patch.object(integrity, "_membership", return_value=helper):
                result = integrity.compare_lib_inventory(root, runner=runner)
            runner.assert_not_called()
            assert result.static_only == static_only
            assert result.cargo_only == cargo_only
            assert result.execution_error is None

    def test_invalid_shared_inventory_uses_original_cargo_build_environment(self):
        inventory = SimpleNamespace(tests={}, module_errors={}, duplicate_tests=())
        helper = SimpleNamespace(validated_inventory=mock.Mock(return_value=None))
        runner = mock.Mock(return_value=subprocess.CompletedProcess([], 0, "", ""))
        with mock.patch.dict(os.environ, {"ADK_LIB_TEST_ARTIFACT": "/bad"}), \
             mock.patch.object(integrity, "_membership", return_value=helper), \
             mock.patch.object(integrity, "discover_targets", return_value={"lib": ROOT / "src/lib.rs"}), \
             mock.patch.object(integrity, "collect_static_tests", return_value=inventory):
            integrity.compare_lib_inventory(ROOT, runner=runner)
        assert runner.call_count == 1
        assert runner.call_args.args[0] == ["cargo", "test", "--manifest-path", str(ROOT / "Cargo.toml"), "--lib", "--", "--list"]
        assert runner.call_args.kwargs["env"]["CARGO_PROFILE_TEST_DEBUG"] == "0"

    def check_shared_relay(self, listing, selected, failed):
        lane = relay.Lane("lane", "T1", "fixture", ("cargo", "test", "--lib", "fixture::"), 2)
        helper = SimpleNamespace(validated_inventory=mock.Mock(return_value=listing))
        runner = mock.Mock(side_effect=AssertionError("shared relay oracle must avoid cargo"))
        with mock.patch.dict(os.environ, {"ADK_LIB_TEST_ARTIFACT": "/artifact"}), \
             mock.patch.object(relay, "artifact_contract", return_value=helper):
            result = relay.run_lane(lane, ROOT, runner=runner)
        runner.assert_not_called()
        assert result.selected == selected
        assert bool(relay.failures_for(result)) is failed

    def test_invalid_relay_listing_falls_back_and_keeps_failed_cargo_red(self):
        lane = relay.Lane("lane", "T1", "fixture", ("cargo", "test", "--lib", "fixture::"), 1)
        helper = SimpleNamespace(validated_inventory=mock.Mock(return_value=None))
        runner = mock.Mock(return_value=subprocess.CompletedProcess([], 9, "", "failed build"))
        with mock.patch.dict(os.environ, {"ADK_LIB_TEST_ARTIFACT": "/bad", "AGENTDESK_ROOT_DIR": "/wrong"}), \
             mock.patch.object(relay, "artifact_contract", return_value=helper):
            result = relay.run_lane(lane, ROOT, runner=runner)
        assert runner.call_count == 1
        assert runner.call_args.args[0] == lane.command + ("--", "--list")
        assert "AGENTDESK_ROOT_DIR" not in runner.call_args.kwargs["env"]
        assert result.returncode == 9
        assert relay.failures_for(result)

    def test_exact_shared_run_wrapper_remains_visible_to_static_target_gate(self):
        words = ["env", "-u", "AGENTDESK_ROOT_DIR", "python3", "scripts/ci/lib-test-artifact.py", "run", "--", "cargo", "test", "--lib", "fixture::"]
        assert integrity._direct_cargo_test(words, words) == words[7:]
        for index, value in ((4, "scripts/unknown.py"), (5, "list"), (6, "--directory")):
            changed = words.copy()
            changed[index] = value
            assert integrity._direct_cargo_test(changed, changed) is None
    def test_shared_inventory_still_compares_actual_static_ids(self):
        for listing, static_only, cargo_only in (
            ("tests::plain: test\n", set(), set()),
            ("tests::other: test\n", {"tests::plain"}, {"tests::other"}),
            ("", {"tests::plain"}, set()),
        ):
            with self.subTest(listing=listing):
                self.check_shared_inventory(listing, static_only, cargo_only)
    def test_shared_relay_listing_preserves_filter_and_declared_floor(self):
        for listing, selected, failed in (
            ("fixture::one: test\nfixture::two: test\nother::three: test\n", 2, False),
            ("fixture::one: test\nother::three: test\n", 1, True),
            ("other::three: test\n", 0, True),
        ):
            with self.subTest(listing=listing):
                self.check_shared_relay(listing, selected, failed)
    def test_shell_dispatch_keeps_cargo_visible(self):
        words = ["run_prepared_lib_test", "cargo", "test", "--lib", "fixture::"]
        assert integrity._direct_cargo_test(words, words) == words[1:]
        assert integrity._parse_command_line("source scripts/ci/non-pg-test-filter.sh && " + " ".join(words)) == words[1:]
