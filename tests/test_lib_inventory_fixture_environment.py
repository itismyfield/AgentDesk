"""Cargo inventory fixtures must be isolated from the runner's real artifact."""

from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from tests import test_check_test_target_integrity as fixture


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
