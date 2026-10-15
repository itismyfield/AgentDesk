"""Cargo inventory fixtures must be isolated from the runner's real artifact."""

from __future__ import annotations

import os
import shlex
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from tests import test_check_relay_authority_contract as relay_fixture
from tests import test_check_test_target_integrity as fixture
from tests import test_non_pg_test_filter as non_pg_fixture

# These cases stub cargo with a shell function or fixture binary that artifact input would bypass.
NON_PG_SHELL_CARGO_CASES = (
    "test_failed_list_cannot_run_an_unsharded_selection",
    "test_shard_recipe_stops_before_targets_on_source_or_primary_failure",
    "test_source_exports_filter_and_replays_the_generated_selection",
    "test_pg_shards_partition_the_unsharded_selection",
    "test_static_pg_restoration_is_what_bash_sources",
    "test_main_pg_shards_run_each_test_postgres_command_once",
)
RELAY_FIXTURE_CASES = (
    ("SelectionContract", "test_run_lane_appends_list_and_removes_agentdesk_root"),
    ("FloorGateEndToEnd", "test_listing_below_the_declared_floor_makes_the_gate_red"),
    ("FloorGateEndToEnd", "test_listing_that_meets_the_declared_floor_stays_green"),
    ("FloorGateEndToEnd", "test_an_empty_listing_is_red_even_though_cargo_exits_zero"),
)


class LibInventoryFixtureEnvironment(unittest.TestCase):
    def test_cargo_backstop_is_hermetic_under_inherited_artifact_input(self):
        cases = (
            "test_scanner_shrink_regeneration_is_rejected_by_cargo_backstop",
            "test_sequential_git_merges_preserve_manifest_union",
            "test_lib_inventory_build_strips_debuginfo_and_keeps_environment",
        )
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            # Metadata presence would otherwise trigger rustc through the Cargo subprocess mock.
            (directory / "metadata.json").write_text("{}\n", encoding="utf-8")
            for name in cases:
                with self.subTest(case=name), mock.patch.dict(os.environ, ADK_LIB_TEST_ARTIFACT=str(directory)), \
                     mock.patch.object(fixture.integrity, "_membership", side_effect=AssertionError("Cargo fixture must not probe the runner artifact")) as artifact_loader:
                    result = unittest.TestResult()
                    fixture.LibInventoryManifestContract(name).run(result)
                    self.assertEqual(result.testsRun, 1)
                    self.assertEqual(result.skipped, [])
                    self.assertTrue(result.wasSuccessful(), result.errors + result.failures)
                    artifact_loader.assert_not_called()
                    self.assertEqual(os.environ["ADK_LIB_TEST_ARTIFACT"], str(directory))

    def test_removing_fixture_isolation_reproduces_the_extra_rustc_call(self):
        source = Path(fixture.__file__).read_text(encoding="utf-8")
        start = source.index("    def test_scanner_shrink_regeneration_is_rejected_by_cargo_backstop(")
        end = source.index("    def test_sequential_git_merges_preserve_manifest_union(", start)
        block = source[start:end]
        isolation = 'with tempfile.TemporaryDirectory() as tmp, mock.patch.dict(os.environ, ADK_LIB_TEST_ARTIFACT=""):'
        self.assertEqual(block.count(isolation), 1)
        namespace = {"__name__": "lib_inventory_fixture_mutant", "__file__": fixture.__file__}
        exec(compile(source[:start] + block.replace(isolation, "with tempfile.TemporaryDirectory() as tmp:") + source[end:], fixture.__file__, "exec"), namespace)
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            (directory / "metadata.json").write_text("{}\n", encoding="utf-8")
            env = {"PATH": os.environ["PATH"], "ADK_LIB_TEST_ARTIFACT": str(directory),
                   "CARGO_PROFILE_DEV_DEBUG": "0", "CARGO_PROFILE_TEST_DEBUG": "0",
                   "CARGO_PROFILE_DEV_STRIP": "none", "CARGO_PROFILE_TEST_STRIP": "none"}
            with mock.patch.dict(os.environ, env, clear=True):
                result = unittest.TestResult()
                namespace["LibInventoryManifestContract"]("test_scanner_shrink_regeneration_is_rejected_by_cargo_backstop").run(result)
        self.assertEqual(result.testsRun, 1)
        self.assertEqual(result.skipped, [])
        self.assertEqual(len(result.failures), 1)
        self.assertEqual(result.errors, [])
        detail = result.failures[0][1]
        self.assertIn("Called 2 times", detail)
        self.assertIn("['rustc', '-vV']", detail)
        self.assertIn("['cargo', 'test', '--manifest-path'", detail)


class ShellCargoFixtureEnvironment(unittest.TestCase):
    def run_cases(self, cases):
        # A recording PATH cargo sees any fixture that falls through lib-test-artifact.py to Cargo.
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            calls, cargo = directory / "cargo.calls", directory / "bin" / "cargo"
            cargo.parent.mkdir()
            cargo.write_text(f'#!/bin/sh\nprintf "%s\\n" "$*" >> {shlex.quote(str(calls))}\n', encoding="utf-8")
            cargo.chmod(0o755)
            artifact = str(directory / "artifact")
            env = {"ADK_LIB_TEST_ARTIFACT": artifact, "PATH": f"{cargo.parent}{os.pathsep}{os.environ['PATH']}"}
            results = []
            with mock.patch.dict(os.environ, env), \
                 mock.patch.object(relay_fixture.contract, "artifact_contract", side_effect=AssertionError("relay fixture must not probe the runner artifact")):
                for case in cases:
                    result = unittest.TestResult()
                    case.run(result)
                    results.append(result)
                self.assertEqual(os.environ["ADK_LIB_TEST_ARTIFACT"], artifact)
            return results, calls.read_text(encoding="utf-8") if calls.exists() else ""

    def test_shell_cargo_stubs_are_hermetic_under_inherited_artifact_input(self):
        cases = [non_pg_fixture.NonPgTestFilter(name) for name in NON_PG_SHELL_CARGO_CASES]
        cases += [getattr(relay_fixture, owner)(name) for owner, name in RELAY_FIXTURE_CASES]
        results, calls = self.run_cases(cases)
        for case, result in zip(cases, results):
            with self.subTest(case=case.id()):
                self.assertEqual(result.testsRun, 1)
                self.assertEqual(result.skipped, [])
                self.assertTrue(result.wasSuccessful(), result.errors + result.failures)
        self.assertEqual(calls, "", "a fixture reached Cargo through lib-test-artifact.py")

    def test_removing_the_isolation_lets_cargo_bypass_the_shell_stub(self):
        source = Path(non_pg_fixture.__file__).read_text(encoding="utf-8")
        isolation = '        os.environ.pop("ADK_LIB_TEST_ARTIFACT", None)\n'
        self.assertEqual(source.count(isolation), 1)
        namespace = {"__name__": "non_pg_filter_mutant", "__file__": non_pg_fixture.__file__}
        exec(compile(source.replace(isolation, ""), non_pg_fixture.__file__, "exec"), namespace)
        case = namespace["NonPgTestFilter"]("test_source_exports_filter_and_replays_the_generated_selection")
        [result], calls = self.run_cases([case])
        self.assertEqual(result.testsRun, 1)
        self.assertFalse(result.wasSuccessful())
        self.assertIn("test --lib -- --exact ", calls)
