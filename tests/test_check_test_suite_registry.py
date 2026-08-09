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
        sources = changes.pop("surface_sources", ("scripts/ci-script-checks.sh",))
        config = registry.RegistryConfig(surface_sources=sources, **changes)
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
    def test_recursive_family_matcher_covers_zero_one_and_deep_directories(self) -> None:
        fixtures = (
            (registry.Family.TESTS_PY, "tests/a/b/test_deep.py", SUITE),
            (registry.Family.E2E_PY, "scripts/e2e/a/b/test_deep.py", SUITE),
            (registry.Family.SCRIPTS_PY, "scripts/a/b/test_deep.py", SUITE),
            (registry.Family.SHELL, "tests/a/b/test_deep.sh", "#!/bin/sh\ntrue\n"),
        )
        for family, path, text in fixtures:
            with self.subTest(family=family.value):
                repo = FixtureRepo()
                self.addCleanup(repo.close)
                repo.write(path, text)
                result = repo.scan()
                self.assertEqual(result.family_counts[family], 1)
                self.assertEqual([item.path for item in result.candidates], [path])
        self.assertTrue(registry._matches("tests/test_root.py", registry.DEFAULT_PATTERNS[registry.Family.TESTS_PY]))
        self.assertTrue(registry._matches("tests/a/test_one.py", registry.DEFAULT_PATTERNS[registry.Family.TESTS_PY]))
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
        dead = SUITE + "\ndef never_called():\n    unittest.main()\n"
        self.repo.write("tests/test_dead_main.py", dead)
        self.repo.write("scripts/ci-script-checks.sh", "python3 tests/test_dead_main.py\n")
        result = self.repo.scan()
        self.assertEqual(self.candidate(result, "tests/test_dead_main.py").execution, registry.ExecStatus.NONE)
        self.assertIn(registry.DiagnosticKind.HARNESS_MISMATCH, {d.kind for d in result.diagnostics})
        for body in ("unittest.main()", "if __name__ == '__main__':\n    raise SystemExit(unittest.main())"):
            self.repo.write("tests/test_live_main.py", SUITE + "\n" + body + "\n")
            self.repo.write("scripts/ci-script-checks.sh", "python3 tests/test_live_main.py\n")
            self.assertEqual(self.candidate(self.repo.scan(), "tests/test_live_main.py").execution, registry.ExecStatus.FULL)
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
    def test_tracked_candidate_validation_is_shared_and_fail_closed(self) -> None:
        cases = (
            ("tests/test_gone.sh", "#!/bin/sh\ntrue\n", None, "for x in tests/*.sh; do bash \"$x\"; done\n", registry.DiagnosticKind.SOURCE_MISSING),
            ("scripts/check_gone.py", "print('ok')\n", None, "python3 scripts/check_gone.py\n", registry.DiagnosticKind.SOURCE_MISSING),
            ("tests/test_dir.sh", "true\n", "directory", "bash tests/test_dir.sh\n", registry.DiagnosticKind.SOURCE_UNREADABLE),
            ("tests/test_nul.sh", "\0", "keep", "bash tests/test_nul.sh\n", registry.DiagnosticKind.SOURCE_MALFORMED),
            ("tests/test_cont.sh", "echo still \\", "keep", "bash tests/test_cont.sh\n", registry.DiagnosticKind.SOURCE_MALFORMED),
            ("scripts/check_bad.py", "if :\n", "keep", "python3 scripts/check_bad.py\n", registry.DiagnosticKind.PYTHON_MALFORMED),
        )
        for path, text, shape, command, diagnostic in cases:
            with self.subTest(path=path):
                repo = FixtureRepo()
                self.addCleanup(repo.close)
                repo.write(path, text)
                if shape is None:
                    (repo.root / path).unlink()
                elif shape == "directory":
                    (repo.root / path).unlink()
                    (repo.root / path).mkdir()
                repo.write("scripts/ci-script-checks.sh", command)
                result = repo.scan()
                candidate = next(item for item in result.candidates if item.path == path)
                self.assertFalse(result.input_valid)
                self.assertEqual(candidate.execution, registry.ExecStatus.NONE)
                self.assertIn(diagnostic, {item.kind for item in result.diagnostics if item.fatal})
    def test_comments_assertions_arrays_and_strings_do_not_execute_candidates(self) -> None:
        self.repo.write("tests/test_mentions.py", SUITE)
        self.repo.write("tests/test_mentions.sh", "#!/bin/sh\ntrue\n")
        self.repo.write("scripts/ci-script-checks.sh", """
# "$PYTHON" -m unittest tests.test_mentions
mention=("$PYTHON" -m unittest tests.test_mentions)
self.assertIn("python -m unittest tests.test_mentions", lines)
echo 'python -m unittest tests.test_mentions'
echo python -m unittest tests.test_mentions
printf '%s' pytest tests/test_mentions.py
required_shell_suites=(tests/test_mentions.sh)
""")
        result = self.repo.scan()
        self.assertEqual(self.candidate(result, "tests/test_mentions.py").execution, registry.ExecStatus.NONE)
        shell = self.candidate(result, "tests/test_mentions.sh")
        self.assertEqual((shell.execution, shell.pin), (registry.ExecStatus.NONE, registry.PinStatus.REQUIRED_ARRAY))
    def test_recipe_text_and_workflow_metadata_are_not_commands(self) -> None:
        self.repo.write("tests/test_meta.py", SUITE)
        self.repo.write("justfile", "check:\n  echo python -m unittest tests.test_meta\n")
        self.repo.write("Makefile", "check:\n\tprintf pytest tests/test_meta.py\n")
        result = self.repo.scan(surface_sources=("justfile", "Makefile"))
        self.assertEqual(self.candidate(result, "tests/test_meta.py").execution, registry.ExecStatus.NONE)
        workflow = "name: pytest tests/test_meta.py\ndescription: |\n  run: pytest tests/test_meta.py\n# run: pytest tests/test_meta.py\nuses: pytest tests/test_meta.py\njobs:\n  x:\n    name: python -m unittest tests.test_meta\n"
        self.repo.write(".github/workflows/ci.yml", workflow)
        result = self.repo.scan(surface_sources=(".github/workflows/ci.yml",))
        self.assertEqual(self.candidate(result, "tests/test_meta.py").execution, registry.ExecStatus.NONE)
        runs = ("      - run: python -m unittest tests.test_meta\n", "      - run: 'python -m unittest tests.test_meta'\n", '      - run: "python -m unittest tests.test_meta"\n')
        runs += tuple(f"      - run: {style}\n          python -m unittest{suffix}\n" for style, suffix in (("|", " \\\n          tests.test_meta"), ("|-", " tests.test_meta"), ("|+", " tests.test_meta"), (">", "\n          tests.test_meta"), (">-", "\n          tests.test_meta"), (">+", "\n          tests.test_meta")))
        runs += ("      - description: |\n          run: pytest tests/test_meta.py\n        run: python -m unittest tests.test_meta\n",)
        for run in runs:
            self.repo.write(".github/workflows/ci.yml", workflow + "    steps:\n" + run); result = self.repo.scan(surface_sources=(".github/workflows/ci.yml",))
            self.assertEqual(self.candidate(result, "tests/test_meta.py").execution, registry.ExecStatus.FULL)
    def test_execution_globs_are_path_segment_anchored(self) -> None:
        fixtures = {"tests/test_root.py": SUITE, "tests/a/test_nested.py": SUITE, "tests/test_root.sh": "true\n", "tests/a/test_nested.sh": "true\n"}
        for path, text in fixtures.items(): self.repo.write(path, text)
        for patterns, expected in ((("tests/*.py", "tests/*.sh"), [registry.ExecStatus.GLOB, registry.ExecStatus.NONE] * 2), (("tests/**/*.py", "tests/**/*.sh"), [registry.ExecStatus.GLOB] * 4)):
            self.repo.write("scripts/ci-script-checks.sh", f"pytest {patterns[0]}\nfor item in {patterns[1]}; do bash \"$item\"; done\n")
            self.assertEqual([self.candidate(self.repo.scan(), path).execution for path in fixtures], expected)
    def test_comment_backslashes_are_not_false_continuations(self) -> None:
        cases = (("# harmless documentation \\", True), ("true # harmless documentation \\", True), ("printf '%s' '#' \\", False), ("printf '%s' \\# \\", False))
        for text, valid in cases:
            with self.subTest(text=text):
                repo = FixtureRepo(); self.addCleanup(repo.close)
                repo.write("tests/test_comment.sh", "#!/bin/sh\n" + text + "\n"); repo.write("scripts/ci-script-checks.sh", "bash tests/test_comment.sh\n")
                result = repo.scan()
                self.assertEqual(result.input_valid, valid)
                self.assertEqual(next(c for c in result.candidates if c.path == "tests/test_comment.sh").execution, registry.ExecStatus.FULL if valid else registry.ExecStatus.NONE)
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
        direct, required, glob = (self.candidate(result, f"tests/test_{name}.sh") for name in "abc")
        direct_pin = next(item for item in direct.pin_evidence if item.status is registry.PinStatus.PINNED)
        required_pin = next(item for item in required.pin_evidence if item.status is registry.PinStatus.REQUIRED_ARRAY)
        self.assertEqual((direct_pin.source, direct_pin.command), ("scripts/ci-script-checks.sh", "bash tests/test_a.sh"))
        self.assertEqual((required_pin.source, required_pin.command), ("scripts/ci-script-checks.sh", "required_shell_suites=(tests/test_b.sh)"))
        self.assertEqual((glob.invocations[0].source, glob.invocations[0].command), ("scripts/ci-script-checks.sh", 'bash "$shell_test"'))
        self.assertEqual((glob.pin_evidence[0].source, glob.pin_evidence[0].target), ("scripts/ci-script-checks.sh", "tests/*.sh"))
        self.repo.write("justfile", "x:\n  bash tests/test_a.sh\n  bash tests/test_a.sh\n")
        sources = ("scripts/ci-script-checks.sh", "justfile")
        evidence = self.candidate(self.repo.scan(surface_sources=sources), "tests/test_a.sh").pin_evidence
        self.assertEqual([item.source for item in evidence if item.status is registry.PinStatus.PINNED], sorted(sources))
    def test_literal_shell_call_chain_is_recursive_and_dynamic_shape_is_fatal(self) -> None:
        self.repo.write("tests/test_chain.py", SUITE)
        self.repo.write("scripts/leaf.sh", '#!/bin/sh\n"$PYTHON" -m unittest tests.test_chain\nbash "$SCRIPT_DIR/sub/driver.sh"\n')
        self.repo.write("scripts/sub/driver.sh", '#!/bin/sh\nbash "$SCRIPT_DIR/../leaf.sh"\n')
        self.repo.write("scripts/driver.sh", '#!/bin/sh\nbash "$SCRIPT_DIR/sub/driver.sh"\n')
        self.repo.write("scripts/ci-script-checks.sh", 'bash "scripts/driver.sh"\n')
        result = self.repo.scan()
        self.assertEqual(self.candidate(result, "tests/test_chain.py").execution, registry.ExecStatus.FULL)
        self.assertIn("scripts/driver.sh", result.reachable_sources)
        self.assertEqual(len(result.reachable_sources), len(set(result.reachable_sources)))
        self.repo.write("tests/test_dynamic.sh", "#!/bin/sh\ntrue\n")
        for target in ('$target', '${target}', '{{ target }}', '$SCRIPT_DIR/../../../escape.sh'):
            self.repo.write("scripts/ci-script-checks.sh", f'target=tests/test_dynamic.sh; bash "{target}"\n')
            dynamic = self.repo.scan()
            self.assertEqual(self.candidate(dynamic, "tests/test_dynamic.sh").execution, registry.ExecStatus.NONE)
            self.assertIn(registry.DiagnosticKind.UNSUPPORTED_CALL, {d.kind for d in dynamic.diagnostics if d.fatal})
        self.repo.write("scripts/ci-script-checks.sh", "echo 'bash $target'\n")
        self.assertNotIn(registry.DiagnosticKind.UNSUPPORTED_CALL, {d.kind for d in self.repo.scan().diagnostics})
        self.repo.write("scripts/ci-script-checks.sh", "find tests -name 'test_*.py' -exec python3 {} \\;\n")
        unsupported = self.repo.scan()
        self.assertIn(registry.DiagnosticKind.UNSUPPORTED_CALL, {d.kind for d in unsupported.diagnostics if d.fatal})
if __name__ == "__main__":
    unittest.main()
