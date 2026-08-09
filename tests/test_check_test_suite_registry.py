"""Acceptance and mutation verifier for the S3a-1 registry facts."""
from __future__ import annotations
import importlib.util, subprocess, sys, tempfile, unittest
from pathlib import Path
ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("check_test_suite_registry", ROOT / "scripts/check_test_suite_registry.py")
assert SPEC and SPEC.loader
registry = importlib.util.module_from_spec(SPEC); sys.modules[SPEC.name] = registry; SPEC.loader.exec_module(registry)
SUITE = """import unittest
class Parent(unittest.TestCase):
 def test_one(self): pass
 def test_two(self): pass
class Child(Parent):
 def test_three(self): pass
"""
SHELL = "#!/usr/bin/env bash\ntrue\n"
class Repo:
    def __init__(self):
        self.tmp = tempfile.TemporaryDirectory(); self.root = Path(self.tmp.name)
        subprocess.run(("git", "init", "-q"), cwd=self.root, check=True)
        self.write("scripts/ci-script-checks.sh", "#!/usr/bin/env bash\n")
    def __enter__(self): return self
    def __exit__(self, *_): self.tmp.cleanup()
    def write(self, name, data, tracked=True):
        path = self.root / name; path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data if isinstance(data, bytes) else data.encode())
        if tracked: subprocess.run(("git", "add", name), cwd=self.root, check=True)
        return path
    def scan(self, *sources):
        chosen = sources or ("scripts/ci-script-checks.sh",)
        return registry.scan_registry(self.root, registry.RegistryConfig(surface_sources=chosen))
    @staticmethod
    def candidate(result, path): return next(item for item in result.candidates if item.path == path)
class RegistryContract(unittest.TestCase):
    def test_recursive_tracked_families_precedence_and_zero_counts(self):
        with Repo() as repo:
            families = ((registry.Family.TESTS_PY, "tests", ".py", SUITE),
                        (registry.Family.E2E_PY, "scripts/e2e", ".py", SUITE),
                        (registry.Family.SCRIPTS_PY, "scripts/other", ".py", SUITE),
                        (registry.Family.SHELL, "tests", ".sh", SHELL))
            expected = {}
            for family, base, suffix, body in families:
                expected[family] = {f"{base}/test_root{suffix}", f"{base}/a/test_one{suffix}", f"{base}/a/b/test_deep{suffix}"}
                for path in expected[family]: repo.write(path, body)
            gates = {"scripts/check_root.py", "scripts/audit_root.py", "scripts/check-root.py", "scripts/check-root.sh"}
            for path in gates: repo.write(path, SHELL if path.endswith(".sh") else "pass\n")
            for path in ("testers/test_no.py", "scripts/e2ex/test_no.py", "script/test_no.py",
                         "tests/test_no.pyi", "tests/test_no.bash", "tests/helper.py", "scripts/a/check_deep.py"):
                repo.write(path, SUITE if path.endswith((".py", ".pyi")) else SHELL)
            repo.write(".gitignore", "tests/test_ignored.py\n"); repo.write("tests/test_ignored.py", SUITE, tracked=False)
            result = repo.scan(); grouped = {family: {c.path for c in result.candidates if c.family is family} for family in registry.Family}
            for family, paths in expected.items(): self.assertEqual(grouped[family], paths, family.value)
            self.assertEqual(grouped[registry.Family.GATE], gates)
            self.assertEqual(result.family_counts, {**{key: 3 for key in expected}, registry.Family.GATE: 4})
            self.assertEqual(result.family_violations({registry.Family.TESTS_PY: 4})[0].actual, 3)
        with Repo() as empty: self.assertEqual(empty.scan().family_counts, {family: 0 for family in registry.Family})
    def test_tracked_inputs_fail_closed_without_masking_valid_candidate(self):
        cases = (("tests/test_gone.sh", SHELL, "missing", 'for x in tests/*.sh; do bash "$x"; done', registry.DiagnosticKind.SOURCE_MISSING),
                 ("scripts/check_gone.py", "pass\n", "missing", "python3 scripts/check_gone.py", registry.DiagnosticKind.SOURCE_MISSING),
                 ("tests/test_bad_utf.py", b"\xff", "keep", "python3 tests/test_bad_utf.py", registry.DiagnosticKind.SOURCE_UNREADABLE),
                 ("tests/test_nul.sh", b"true\0", "keep", "bash tests/test_nul.sh", registry.DiagnosticKind.SOURCE_MALFORMED),
                 ("tests/test_dir.sh", SHELL, "directory", "bash tests/test_dir.sh", registry.DiagnosticKind.SOURCE_UNREADABLE),
                 ("scripts/check_bad.py", "if :\n", "keep", "python3 scripts/check_bad.py", registry.DiagnosticKind.PYTHON_MALFORMED),
                 ("tests/test_cont.sh", "echo x " + "\\", "keep", "bash tests/test_cont.sh", registry.DiagnosticKind.SOURCE_MALFORMED),
                 ("tests/test_quote.sh", 'bash "unterminated\n', "keep", "bash tests/test_quote.sh", registry.DiagnosticKind.SOURCE_MALFORMED))
        for path, body, shape, command, kind in cases:
            with self.subTest(path=path), Repo() as repo:
                target = repo.write(path, body); repo.write("tests/test_good.py", SUITE)
                if shape == "missing": target.unlink()
                elif shape == "directory": target.unlink(); target.mkdir()
                repo.write("scripts/ci-script-checks.sh", command + "\npython3 -m unittest tests.test_good\n")
                result = repo.scan(); self.assertFalse(result.input_valid)
                self.assertEqual(repo.candidate(result, path).execution, registry.ExecStatus.NONE)
                self.assertEqual(repo.candidate(result, "tests/test_good.py").execution, registry.ExecStatus.FULL)
                self.assertIn(kind, {item.kind for item in result.diagnostics if item.fatal})
    def test_declared_root_input_errors_are_fatal(self):
        for source, payload in (("missing", None), ("package.json", "{"), (".github/workflows/x.yml", 'jobs:\n  x:\n    run: "unterminated\n')):
            with self.subTest(source=source), Repo() as repo:
                if payload is not None: repo.write(source, payload)
                result = repo.scan(source); self.assertFalse(result.input_valid)
                self.assertTrue(any(item.fatal for item in result.diagnostics))
    def test_nonexecution_text_and_workflow_scalar_state(self):
        with Repo() as repo:
            repo.write("tests/test_meta.py", SUITE); repo.write("tests/test_meta.sh", SHELL); repo.write("tests/test_prose.sh", SHELL)
            repo.write("scripts/ci-script-checks.sh", """# python3 -m unittest tests.test_meta
mention=(python3 -m unittest tests.test_meta)
"python3 -m unittest tests.test_meta"
self.assertIn("pytest tests/test_meta.py", text)
echo python3 -m unittest tests.test_meta
printf '%s' pytest tests/test_meta.py
echo 'required_shell_suites=(tests/test_prose.sh)'
required_shell_suites=(tests/test_meta.sh)
""")
            result = repo.scan(); self.assertEqual(repo.candidate(result, "tests/test_meta.py").execution, registry.ExecStatus.NONE)
            self.assertEqual((repo.candidate(result, "tests/test_meta.sh").execution, repo.candidate(result, "tests/test_meta.sh").pin),
                             (registry.ExecStatus.NONE, registry.PinStatus.REQUIRED_ARRAY))
            self.assertEqual(repo.candidate(result, "tests/test_prose.sh").pin, registry.PinStatus.NONE)
            workflow = "name: python3 -m unittest tests.test_meta\ndescription: |\n  run: pytest tests/test_meta.py\njobs:\n  x:\n    steps:\n"
            repo.write(".github/workflows/x.yml", workflow); self.assertEqual(repo.candidate(repo.scan(".github/workflows/x.yml"), "tests/test_meta.py").execution, registry.ExecStatus.NONE)
            runs = ("      - run: python3 -m unittest tests.test_meta\n", "      - run: 'python3 -m unittest tests.test_meta'\n",
                    '      - run: "python3 -m unittest tests.test_meta"\n')
            runs += tuple(f"      - run: {style}\n          python3 -m unittest \\\n          tests.test_meta\n" for style in ("|", "|-", "|+"))
            runs += tuple(f"      - run: {style}\n          python3 -m unittest\n          tests.test_meta\n" for style in (">", ">-", ">+"))
            runs += ("      - description: |\n          run: pytest tests/test_meta.py\n        run: python3 -m unittest tests.test_meta\n",)
            for run in runs:
                with self.subTest(run=run.splitlines()[0]):
                    repo.write(".github/workflows/x.yml", workflow + run)
                    self.assertEqual(repo.candidate(repo.scan(".github/workflows/x.yml"), "tests/test_meta.py").execution, registry.ExecStatus.FULL)
    def test_literal_reachability_normalization_cycles_and_source_locality(self):
        with Repo() as repo:
            repo.write("tests/test_a.py", SUITE); repo.write("tests/test_b.py", SUITE)
            repo.write("scripts/driver.sh", 'bash "scripts/sub/child.sh"\n')
            repo.write("scripts/sub/child.sh", 'bash "$SCRIPT_DIR/../leaf.sh"\n')
            repo.write("scripts/leaf.sh", 'python3 -m unittest tests.test_a\nbash "$SCRIPT_DIR/driver.sh"\n')
            repo.write("scripts/other/leaf.sh", "python3 -m unittest tests.test_b\n")
            repo.write("scripts/ci-script-checks.sh", 'bash "scripts/driver.sh"\n')
            result = repo.scan(); self.assertEqual(repo.candidate(result, "tests/test_a.py").execution, registry.ExecStatus.FULL)
            self.assertEqual(repo.candidate(result, "tests/test_b.py").execution, registry.ExecStatus.NONE)
            self.assertEqual(len(result.reachable_sources), len(set(result.reachable_sources)))
            self.assertIn("scripts/leaf.sh", result.reachable_sources); self.assertNotIn("scripts/other/leaf.sh", result.reachable_sources)
            repo.write("scripts/sub/child.sh", 'bash "$SCRIPT_DIR/../../../escape.sh"\n')
            escaped = repo.scan(); self.assertFalse(escaped.input_valid)
            self.assertIn(registry.DiagnosticKind.UNSUPPORTED_CALL, {item.kind for item in escaped.diagnostics if item.fatal})
    def test_dynamic_runner_targets_fail_closed_but_prose_is_inert(self):
        with Repo() as repo:
            repo.write("tests/test_dynamic.sh", SHELL)
            for command in ('target=tests/test_dynamic.sh; bash "$target"', 'bash "{{ target }}"', 'python3 "$target"', 'pytest "$target"', 'node --test "$target"'):
                repo.write("scripts/ci-script-checks.sh", command + "\n"); result = repo.scan()
                self.assertFalse(result.input_valid, command); self.assertEqual(repo.candidate(result, "tests/test_dynamic.sh").execution, registry.ExecStatus.NONE)
                self.assertIn(registry.DiagnosticKind.UNSUPPORTED_CALL, {item.kind for item in result.diagnostics if item.fatal})
            repo.write("scripts/ci-script-checks.sh", "echo 'bash $target'\n")
            self.assertTrue(repo.scan().input_valid)
    def test_static_ids_live_main_inverse_guard_and_harness_mismatch(self):
        cases = (("unittest.main()", registry.ExecStatus.FULL),
                 ("if __name__ == '__main__':\n unittest.main()", registry.ExecStatus.FULL),
                 ("if __name__ != '__main__':\n unittest.main()", registry.ExecStatus.NONE),
                 ("def hidden():\n unittest.main()", registry.ExecStatus.NONE),
                 ("class Hidden:\n unittest.main()", registry.ExecStatus.NONE),
                 ("hidden = lambda: unittest.main()", registry.ExecStatus.NONE),
                 ("if enabled:\n unittest.main()", registry.ExecStatus.NONE))
        with Repo() as repo:
            repo.write("scripts/ci-script-checks.sh", "python3 tests/test_live.py\n")
            for body, expected in cases:
                repo.write("tests/test_live.py", SUITE + body + "\n"); result = repo.scan()
                self.assertEqual(repo.candidate(result, "tests/test_live.py").execution, expected, body)
            repo.write("tests/test_bare.py", "def test_bare(): pass\n")
            for command in ("python3 -m unittest tests.test_bare", "python3 tests/test_bare.py"):
                repo.write("scripts/ci-script-checks.sh", command + "\n"); result = repo.scan()
                self.assertEqual(repo.candidate(result, "tests/test_bare.py").execution, registry.ExecStatus.NONE)
                self.assertIn(registry.DiagnosticKind.HARNESS_MISMATCH, {item.kind for item in result.diagnostics})
            repo.write("scripts/ci-script-checks.sh", "pytest tests/test_bare.py\n")
            self.assertEqual(repo.candidate(repo.scan(), "tests/test_bare.py").execution, registry.ExecStatus.FULL)
    def test_exact_method_is_partial_preserves_ids_and_deletion_is_none(self):
        with Repo() as repo:
            repo.write("tests/test_partial.py", SUITE); target = "tests.test_partial.Child.test_three"
            repo.write("scripts/ci-script-checks.sh", f"python3 -m unittest {target}\n"); result = repo.scan(); item = repo.candidate(result, "tests/test_partial.py")
            self.assertEqual((item.execution, item.executed_tests, item.unexecuted_tests),
                             (registry.ExecStatus.PARTIAL, ("Child.test_three",), ("Parent.test_one", "Parent.test_two")))
            self.assertEqual(item.pin_evidence[0].target, target)
            self.assertEqual(len(result.partial_violations()), 1); self.assertEqual(result.partial_violations({(item.path, target)}), ())
            repo.write("scripts/ci-script-checks.sh", "python3 -m unittest tests.test_partial\n")
            self.assertEqual(repo.candidate(repo.scan(), item.path).execution, registry.ExecStatus.FULL)
            repo.write("scripts/ci-script-checks.sh", "# deleted\n")
            self.assertEqual(repo.candidate(repo.scan(), item.path).execution, registry.ExecStatus.NONE)
    def test_orthogonal_axes_exact_provenance_dedupe_and_order(self):
        with Repo() as repo:
            for name in "abc": repo.write(f"tests/test_{name}.sh", SHELL)
            repo.write("scripts/ci-script-checks.sh", 'bash tests/test_a.sh\nrequired_shell_suites=(tests/test_b.sh)\nfor item in tests/*.sh; do\n bash "$item"\ndone\n')
            repo.write("justfile", "x:\n  bash tests/test_a.sh\n  bash tests/test_a.sh\n")
            result = repo.scan("scripts/ci-script-checks.sh", "justfile"); a, b, c = (repo.candidate(result, f"tests/test_{name}.sh") for name in "abc")
            self.assertEqual([(x.execution, x.pin) for x in (a, b, c)], [(registry.ExecStatus.FULL, registry.PinStatus.PINNED),
                             (registry.ExecStatus.GLOB, registry.PinStatus.REQUIRED_ARRAY), (registry.ExecStatus.GLOB, registry.PinStatus.GLOB_UNPINNED)])
            exact = [x for x in a.pin_evidence if x.status is registry.PinStatus.PINNED]
            self.assertEqual([(x.source, x.command, x.target) for x in exact], [("justfile", "bash tests/test_a.sh", "tests/test_a.sh"),
                             ("scripts/ci-script-checks.sh", "bash tests/test_a.sh", "tests/test_a.sh")])
            required = next(x for x in b.pin_evidence if x.status is registry.PinStatus.REQUIRED_ARRAY)
            self.assertEqual((required.source, required.command, required.target),
                             ("scripts/ci-script-checks.sh", "required_shell_suites=(tests/test_b.sh)", "tests/test_b.sh"))
            glob = next(x for x in c.invocations if x.glob); self.assertEqual((glob.source, glob.command, glob.target), ("scripts/ci-script-checks.sh", 'bash "$item"', "tests/*.sh"))
            self.assertEqual(result, repo.scan("scripts/ci-script-checks.sh", "justfile"))
    def test_path_segment_globs_root_and_explicit_recursive_cases(self):
        with Repo() as repo:
            fixtures = {"tests/test_root.py": SUITE, "tests/a/test_deep.py": SUITE, "tests/test_root.sh": SHELL, "tests/a/test_deep.sh": SHELL}
            for path, body in fixtures.items(): repo.write(path, body)
            for py, shell, expected in (("tests/*.py", "tests/*.sh", (registry.ExecStatus.GLOB, registry.ExecStatus.NONE, registry.ExecStatus.GLOB, registry.ExecStatus.NONE)),
                                        ("tests/**/*.py", "tests/**/*.sh", (registry.ExecStatus.GLOB,) * 4)):
                repo.write("scripts/ci-script-checks.sh", f'pytest {py}\nfor x in {shell}; do bash "$x"; done\n')
                result = repo.scan(); self.assertEqual(tuple(repo.candidate(result, path).execution for path in fixtures), expected)
    def test_comment_continuation_order_and_odd_even_parity(self):
        slash = "\\"
        cases = (("# docs " + slash + "\n", True), ("true # docs " + slash + "\n", True),
                 ("printf '%s' '#' " + slash + "\n", False), ("printf '%s' \\# " + slash + "\n", False),
                 ("printf '%s' ok " + slash * 2 + "\n", True), ("printf '%s' ok " + slash + "\n", False))
        for body, valid in cases:
            with self.subTest(body=body), Repo() as repo:
                repo.write("tests/test_cont.sh", "#!/bin/sh\n" + body); repo.write("scripts/ci-script-checks.sh", "bash tests/test_cont.sh\n")
                result = repo.scan(); self.assertEqual(result.input_valid, valid)
                self.assertEqual(repo.candidate(result, "tests/test_cont.sh").execution, registry.ExecStatus.FULL if valid else registry.ExecStatus.NONE)
    def test_reused_loop_variable_retains_every_pattern(self):
        with Repo() as repo:
            repo.write("tests/first/test_a.sh", SHELL); repo.write("tests/second/test_b.sh", SHELL)
            repo.write("scripts/ci-script-checks.sh", 'for item in tests/first/*.sh; do bash "$item"; done\nfor item in tests/second/*.sh; do bash "$item"; done\n')
            result = repo.scan(); first = repo.candidate(result, "tests/first/test_a.sh")
            self.assertEqual(first.execution, registry.ExecStatus.GLOB)
            self.assertEqual({item.target for item in first.invocations}, {"tests/first/*.sh"})
            self.assertEqual(repo.candidate(result, "tests/second/test_b.sh").execution, registry.ExecStatus.GLOB)
    def test_declared_roots_and_report_gate_are_independent_facts(self):
        with Repo() as repo:
            for name in ("report", "just", "make", "package"): repo.write(f"tests/test_{name}.py", SUITE)
            repo.write("scripts/audit_report.py", "print('report only')\n")
            repo.write("scripts/ci-script-checks.sh", "python3 -m unittest tests.test_report\n")
            repo.write("justfile", "x:\n  python3 -m unittest tests.test_just\n")
            repo.write("Makefile", "x:\n\tpython3 -m unittest tests.test_make\n")
            repo.write("package.json", '{"scripts":{"x":"python3 -m unittest tests.test_package"}}\n')
            result = repo.scan("scripts/ci-script-checks.sh", "justfile", "Makefile", "package.json")
            self.assertEqual(repo.candidate(result, "scripts/audit_report.py").execution, registry.ExecStatus.NONE)
            self.assertEqual([repo.candidate(result, f"tests/test_{name}.py").execution for name in ("report", "just", "make", "package")], [registry.ExecStatus.FULL] * 4)
if __name__ == "__main__": unittest.main()
