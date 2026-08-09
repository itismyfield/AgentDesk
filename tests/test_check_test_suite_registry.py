"""Mutation fixtures for the #5003 S3a-1 test-suite registry core."""

from __future__ import annotations

import importlib.util
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "check_test_suite_registry.py"
SPEC = importlib.util.spec_from_file_location("check_test_suite_registry", SCRIPT)
assert SPEC and SPEC.loader
registry = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = registry
SPEC.loader.exec_module(registry)

SUITE = """import unittest
class Alpha(unittest.TestCase):
    def test_one(self): pass
    def test_two(self): pass
"""


class FixtureRepo:
    def __init__(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        subprocess.run(["git", "init", "-q"], cwd=self.root, check=True)
        self.write("scripts/ci-script-checks.sh", "#!/usr/bin/env bash\n")

    def close(self) -> None:
        self.temporary.cleanup()

    def write(self, path: str, text: str, *, tracked: bool = True) -> None:
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")
        if tracked:
            subprocess.run(["git", "add", path], cwd=self.root, check=True)

    def scan(self, **changes: object) -> registry.RegistryResult:
        config = registry.RegistryConfig(
            surface_sources=("scripts/ci-script-checks.sh",), **changes
        )
        return registry.scan_registry(self.root, config)


class RegistryMutationSuite(unittest.TestCase):
    def setUp(self) -> None:
        self.repo = FixtureRepo()
        self.addCleanup(self.repo.close)

    def candidate(self, result: registry.RegistryResult, path: str) -> registry.Candidate:
        return next(candidate for candidate in result.candidates if candidate.path == path)

    def test_m1_tracked_unwired_is_none_and_m2_literal_wiring_makes_full(self) -> None:
        self.repo.write("tests/test_zz_mutant.py", SUITE)
        self.repo.write("tests/test_untracked_fixture.py", SUITE, tracked=False)
        result = self.repo.scan()
        self.assertEqual([c.path for c in result.candidates], ["tests/test_zz_mutant.py"])
        self.assertEqual(self.candidate(result, "tests/test_zz_mutant.py").execution, registry.ExecStatus.NONE)
        self.repo.write("scripts/ci-script-checks.sh", '"$PYTHON" -m unittest tests.test_zz_mutant\n')
        self.assertEqual(self.candidate(self.repo.scan(), "tests/test_zz_mutant.py").execution, registry.ExecStatus.FULL)

    def test_m3_deleted_call_becomes_none_and_m5_wired_identity_remains_exact(self) -> None:
        self.repo.write("tests/test_wired.py", SUITE)
        self.repo.write("scripts/ci-script-checks.sh", '"$PYTHON" -m unittest tests.test_wired\n')
        wired = self.candidate(self.repo.scan(), "tests/test_wired.py")
        self.assertEqual((wired.path, wired.execution), ("tests/test_wired.py", registry.ExecStatus.FULL))
        self.repo.write("scripts/ci-script-checks.sh", "#!/usr/bin/env bash\n")
        self.assertEqual(self.candidate(self.repo.scan(), wired.path).execution, registry.ExecStatus.NONE)

    def test_m4_each_family_glob_loss_trips_its_shrink_only_floor(self) -> None:
        fixtures = {
            "tests/test_a.py": SUITE, "scripts/e2e/test_b.py": SUITE,
            "scripts/test_c.py": SUITE, "tests/test_d.sh": "#!/bin/sh\ntrue\n",
            "scripts/check_e.py": "print('gate')\n",
        }
        for path, text in fixtures.items():
            self.repo.write(path, text)
        baseline = self.repo.scan().family_counts
        self.assertEqual({family: baseline[family] for family in registry.Family}, {family: 1 for family in registry.Family})
        for family in registry.Family:
            with self.subTest(family=family.value):
                patterns = dict(registry.DEFAULT_PATTERNS)
                patterns[family] = ()
                mutated = self.repo.scan(patterns=patterns)
                self.assertEqual(mutated.family_violations(baseline)[0].family, family)

    def test_m6_unittest_and_python_path_zero_test_harness_is_not_wiring(self) -> None:
        bare = "def test_bare(): pass\n"
        self.repo.write("tests/test_bare.py", bare)
        for command in (
            '"$PYTHON" -m unittest tests.test_bare\n',
            "python3 tests/test_bare.py\n",
        ):
            self.repo.write("scripts/ci-script-checks.sh", command)
            result = self.repo.scan()
            self.assertEqual(self.candidate(result, "tests/test_bare.py").execution, registry.ExecStatus.NONE)
            self.assertIn(registry.DiagnosticKind.HARNESS_MISMATCH, {d.kind for d in result.diagnostics})
        self.repo.write("scripts/ci-script-checks.sh", "pytest tests/test_bare.py\n")
        self.assertEqual(self.candidate(self.repo.scan(), "tests/test_bare.py").execution, registry.ExecStatus.FULL)
        self.repo.write("tests/test_runner.py", SUITE + "\nif __name__ == '__main__': unittest.main()\n")
        self.repo.write("scripts/ci-script-checks.sh", "python3 tests/test_runner.py\n")
        self.assertEqual(self.candidate(self.repo.scan(), "tests/test_runner.py").execution, registry.ExecStatus.FULL)

    def test_m6b_four_backslash_continuations_are_all_full(self) -> None:
        paths = [f"tests/test_continuation_{index}.py" for index in range(4)]
        for path in paths:
            self.repo.write(path, SUITE)
        modules = [path[:-3].replace("/", ".") for path in paths]
        command = '"$PYTHON" -m unittest \\\n  ' + " \\\n  ".join(modules) + "\n"
        self.repo.write("scripts/ci-script-checks.sh", command)
        self.assertEqual([self.candidate(self.repo.scan(), path).execution for path in paths], [registry.ExecStatus.FULL] * 4)

    def test_m7_report_gate_and_own_suite_are_independent_cross_seal_records(self) -> None:
        self.repo.write("scripts/audit_report.py", "print('report only')\n")
        self.repo.write("tests/test_audit_report.py", SUITE)
        self.repo.write("scripts/ci-script-checks.sh", '"$PYTHON" -m unittest tests.test_audit_report\n')
        result = self.repo.scan()
        self.assertEqual(self.candidate(result, "scripts/audit_report.py").execution, registry.ExecStatus.NONE)
        self.assertEqual(self.candidate(result, "tests/test_audit_report.py").execution, registry.ExecStatus.FULL)
        self.repo.write("scripts/ci-script-checks.sh", "#!/usr/bin/env bash\n")
        self.assertEqual(self.candidate(self.repo.scan(), "tests/test_audit_report.py").execution, registry.ExecStatus.NONE)

    def test_missing_unreadable_and_malformed_sources_are_explicit_fatal_results(self) -> None:
        missing = registry.scan_registry(
            self.repo.root, registry.RegistryConfig(surface_sources=("missing",))
        )
        self.assertFalse(missing.input_valid)
        self.assertIn(registry.DiagnosticKind.SOURCE_MISSING, {d.kind for d in missing.diagnostics})
        self.repo.write("package.json", "{")
        malformed = registry.scan_registry(
            self.repo.root, registry.RegistryConfig(surface_sources=("package.json",))
        )
        self.assertIn(registry.DiagnosticKind.SOURCE_MALFORMED, {d.kind for d in malformed.diagnostics if d.fatal})
        (self.repo.root / "unreadable").mkdir()
        unreadable = registry.scan_registry(
            self.repo.root, registry.RegistryConfig(surface_sources=("unreadable",))
        )
        self.assertIn(registry.DiagnosticKind.SOURCE_UNREADABLE, {d.kind for d in unreadable.diagnostics if d.fatal})

    def test_comments_assertions_arrays_and_strings_do_not_execute_candidates(self) -> None:
        self.repo.write("tests/test_mentions.py", SUITE)
        self.repo.write("tests/test_mentions.sh", "#!/bin/sh\ntrue\n")
        self.repo.write("scripts/ci-script-checks.sh", """
# "$PYTHON" -m unittest tests.test_mentions
mention=("$PYTHON" -m unittest tests.test_mentions)
self.assertIn("python -m unittest tests.test_mentions", lines)
required_shell_suites=(tests/test_mentions.sh)
""")
        result = self.repo.scan()
        self.assertEqual(self.candidate(result, "tests/test_mentions.py").execution, registry.ExecStatus.NONE)
        shell = self.candidate(result, "tests/test_mentions.sh")
        self.assertEqual((shell.execution, shell.pin), (registry.ExecStatus.NONE, registry.PinStatus.REQUIRED_ARRAY))

    def test_exact_method_selection_is_partial_and_preserves_unexecuted_ids(self) -> None:
        self.repo.write("tests/test_partial.py", SUITE + "\nclass Beta(unittest.TestCase):\n    def test_three(self): pass\n")
        self.repo.write("scripts/ci-script-checks.sh", '"$PYTHON" -m unittest tests.test_partial.Alpha.test_one\n')
        result = self.repo.scan()
        candidate = self.candidate(result, "tests/test_partial.py")
        self.assertEqual(candidate.execution, registry.ExecStatus.PARTIAL)
        self.assertEqual(candidate.executed_tests, ("Alpha.test_one",))
        self.assertEqual(candidate.unexecuted_tests, ("Alpha.test_two", "Beta.test_three"))
        target = "tests.test_partial.Alpha.test_one"
        self.assertEqual(result.partial_violations({(candidate.path, target)}), ())
        self.assertEqual(len(result.partial_violations()), 1)

    def test_shell_full_required_array_and_glob_unpinned_axes_are_distinct(self) -> None:
        for name in "abc":
            self.repo.write(f"tests/test_{name}.sh", "#!/bin/sh\ntrue\n")
        self.repo.write("scripts/ci-script-checks.sh", """
bash tests/test_a.sh
required_shell_suites=(tests/test_b.sh)
for shell_test in tests/*.sh; do
  bash "$shell_test"
done
""")
        result = self.repo.scan()
        observed = {name: (self.candidate(result, f"tests/test_{name}.sh").execution, self.candidate(result, f"tests/test_{name}.sh").pin) for name in "abc"}
        self.assertEqual(observed["a"], (registry.ExecStatus.FULL, registry.PinStatus.PINNED))
        self.assertEqual(observed["b"], (registry.ExecStatus.GLOB, registry.PinStatus.REQUIRED_ARRAY))
        self.assertEqual(observed["c"], (registry.ExecStatus.GLOB, registry.PinStatus.GLOB_UNPINNED))

    def test_literal_shell_call_chain_is_recursive_and_dynamic_shape_is_fatal(self) -> None:
        self.repo.write("tests/test_chain.py", SUITE)
        self.repo.write("scripts/driver.sh", '#!/bin/sh\n"$PYTHON" -m unittest tests.test_chain\n')
        self.repo.write("scripts/ci-script-checks.sh", "bash scripts/driver.sh\n")
        result = self.repo.scan()
        self.assertEqual(self.candidate(result, "tests/test_chain.py").execution, registry.ExecStatus.FULL)
        self.assertIn("scripts/driver.sh", result.reachable_sources)
        self.repo.write("scripts/ci-script-checks.sh", "find tests -name 'test_*.py' -exec python3 {} \\;\n")
        unsupported = self.repo.scan()
        self.assertIn(registry.DiagnosticKind.UNSUPPORTED_CALL, {d.kind for d in unsupported.diagnostics if d.fatal})


if __name__ == "__main__":
    unittest.main()
