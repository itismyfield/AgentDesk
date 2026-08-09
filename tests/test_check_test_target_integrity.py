"""Tests for the test-target integrity gate (#5003 S1).

The gate exists because cargo exits 0 when a libtest filter matches nothing,
so a curated lane with the wrong target flag runs 0 tests while its required
check stays green. These tests are the gate's own mutation proof: a known-bad
command fixture must FAIL validation and its corrected form must PASS.
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "check_test_target_integrity.py"
_spec = importlib.util.spec_from_file_location("check_test_target_integrity", SCRIPT)
assert _spec and _spec.loader
integrity = importlib.util.module_from_spec(_spec)
sys.modules[_spec.name] = integrity
_spec.loader.exec_module(integrity)

BAD_COMMAND = "cargo test --bin agentdesk high_risk_recovery:: -- --test-threads=1"
GOOD_COMMAND = "cargo test --lib high_risk_recovery:: -- --test-threads=1"


def build_fixture_repo(root: Path, command: str, *,
                       just_text: str | None = None,
                       integration_test: bool = False,
                       bin_test: bool = False) -> Path:
    """Materialize a minimal crate + workflow mirroring the real layout."""
    (root / "src").mkdir(parents=True)
    (root / "src" / "server").mkdir()
    (root / "Cargo.toml").write_text(
        '[package]\nname = "agentdesk"\n\n[lib]\npath = "src/lib.rs"\n\n'
        '[[bin]]\nname = "agentdesk"\npath = "src/main.rs"\n',
        encoding="utf-8",
    )
    (root / "src" / "lib.rs").write_text(
        "mod server;\nmod high_risk_recovery;\nmod route;\n", encoding="utf-8"
    )
    (root / "src" / "high_risk_recovery.rs").write_text(
        "#[cfg(test)]\nmod tests {}\n", encoding="utf-8"
    )
    (root / "src" / "server" / "mod.rs").write_text(
        "pub(crate) mod multinode_regression;\n", encoding="utf-8"
    )
    (root / "src" / "server" / "multinode_regression.rs").write_text(
        "#[cfg(test)]\nmod tests {}\n", encoding="utf-8"
    )
    # #[path] redirection mirroring src/services/auto_queue/route.rs style:
    # the redirected file lives next to the declaring file, not under a
    # directory named after the declaring module.
    (root / "src" / "route.rs").write_text(
        '#[cfg(test)]\n#[path = "redirected_impl.rs"]\nmod redirected_impl;\n',
        encoding="utf-8",
    )
    (root / "src" / "redirected_impl.rs").write_text(
        "#[cfg(test)]\nmod tests {}\n", encoding="utf-8"
    )
    (root / "src" / "main.rs").write_text(
        "mod bin_owned;\nfn main() {}\n" if bin_test else "fn main() {}\n",
        encoding="utf-8",
    )
    if bin_test:
        (root / "src" / "bin_owned.rs").write_text(
            "#[cfg(test)]\nmod tests { #[test] fn owned_case() {} }\n",
            encoding="utf-8",
        )
    workflows = root / ".github" / "workflows"
    workflows.mkdir(parents=True)
    workflow = workflows / "ci-fixture.yml"
    workflow.write_text(
        f'jobs:\n  lane:\n    steps:\n      - run: "{command}"\n',
        encoding="utf-8",
    )
    manifest = root / integrity.LIB_INVENTORY_MANIFEST_REL
    manifest.parent.mkdir(parents=True, exist_ok=True)
    manifest.write_text(integrity.render_lib_inventory_manifest({
        "high_risk_recovery::tests::recovery_case",
        "server::multinode_regression::tests::nested_case",
        "route::redirected_impl::tests::redirected_case",
    }), encoding="utf-8")
    if integration_test:
        (root / "tests").mkdir()
        (root / "tests" / "integration_only.rs").write_text(
            "#[test]\nfn integration_only_case() {}\n", encoding="utf-8"
        )
    (root / "justfile").write_text(
        just_text if just_text is not None else f"fixture:\n    {GOOD_COMMAND}\n",
        encoding="utf-8",
    )
    (root / integrity.SOURCE_FLOOR_REL).write_text(
        "workflows=1\njustfile=1\n", encoding="utf-8"
    )
    return workflow


def run_fixture(command: str, allowlist: str = "", *,
                just_text: str | None = None,
                integration_test: bool = False,
                bin_test: bool = False) -> list:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        workflow = build_fixture_repo(
            root, command, just_text=just_text,
            integration_test=integration_test, bin_test=bin_test,
        )
        allow = root / "allowlist.txt"
        allow.write_text(allowlist, encoding="utf-8")
        return integrity.check_workflows(
            root, [workflow], integrity.load_allowlist(allow),
            with_list_check=False,
        )


class MutationProof(unittest.TestCase):
    """Known-bad command must fail; the corrected command must pass."""

    def test_bad_bin_command_is_flagged(self) -> None:
        violations = run_fixture(BAD_COMMAND)
        self.assertEqual(len(violations), 1, violations)
        violation = violations[0]
        self.assertEqual(violation.kind, "target-mismatch")
        self.assertIn("high_risk_recovery", violation.detail)
        self.assertIn("src/lib.rs", violation.detail)
        self.assertIn("bin:agentdesk", violation.detail)

    def test_fixed_lib_command_passes(self) -> None:
        self.assertEqual(run_fixture(GOOD_COMMAND), [])

    def test_justfile_bad_target_and_full_path_typo_are_blocked(self) -> None:
        for command, kind in (
            (BAD_COMMAND, "target-mismatch"),
            ("cargo test --lib high_risk_recovery::renamed_case", "zero-match"),
        ):
            with self.subTest(command=command):
                violations = run_fixture(
                    GOOD_COMMAND,
                    just_text=f"fixture:\n    {command}\n",
                )
                self.assertEqual([item.kind for item in violations], [kind])

    def test_all_targets_keeps_typed_selection_and_known_surfaces(self) -> None:
        spec = integrity.parse_command(
            "cargo test --all-targets bin_owned::tests::owned_case".split()
        )
        self.assertIs(spec.selection, integrity.TargetSelection.ALL_TARGETS)
        self.assertFalse(
            spec.skipped,
            "mutation self-assert: --all-targets must stay statically observed",
        )
        self.assertEqual(
            spec.targets, (),
            "mutation self-assert: --all-targets must not collapse to lib",
        )
        self.assertEqual(run_fixture(
            "cargo test --all-targets bin_owned::tests::owned_case",
            bin_test=True,
        ), [])

    def test_all_targets_integration_owned_filter_is_inconclusive(self) -> None:
        self.assertEqual(run_fixture(
            "cargo test --all-targets integration_only_case",
            integration_test=True,
        ), [])

    def test_explicit_lib_typo_is_zero_match(self) -> None:
        violations = run_fixture(
            "cargo test --lib high_risk_recovery::renamed_case"
        )
        self.assertEqual([item.kind for item in violations], ["zero-match"])

    def test_literal_filter_survives_each_dynamic_token_form(self) -> None:
        for dynamic in ("${EXTRA_ARGS}", "$EXTRA_ARGS", "{{EXTRA_ARGS}}"):
            with self.subTest(dynamic=dynamic):
                command = (
                    "cargo test --lib high_risk_recovery::renamed_case "
                    + dynamic
                )
                spec = integrity.parse_command(command.split())
                self.assertEqual(
                    spec.filters, ("high_risk_recovery::renamed_case",),
                    "mutation self-assert: dynamic handling must be token-local",
                )
                self.assertEqual(
                    [item.kind for item in run_fixture(command)], ["zero-match"]
                )

    def test_dynamic_only_filters_are_not_false_zero_matches(self) -> None:
        self.assertEqual(run_fixture(
            "cargo test --lib ${TEST_FILTER} $EXTRA_ARGS {{JUST_ARGS}}"
        ), [])

    def test_dynamic_target_values_are_token_local(self) -> None:
        owned = "bin_owned::tests::owned_case"
        cases = (
            (f"cargo test --bin $BIN {owned}", (), True, True),
            (f"cargo test --bin ${{BIN}} {owned}", (), True, True),
            (f"cargo test --bin {{{{BIN}}}} {owned}", (), True, True),
            (f"cargo test --test $TEST {owned}", (), True, True),
            (f"cargo test --test ${{TEST}} {owned}", (), True, True),
            (f"cargo test --test {{{{TEST}}}} {owned}", (), True, True),
            (f"cargo test --bin missing-bin $FILTER",
             ("unknown-target",), False, False),
            (f"cargo test --bin missing-bin --test $TEST {owned}",
             ("unknown-target",), False, True),
            ("cargo test --lib --bin $BIN missing_case", (), False, True),
        )
        for command, kinds, bin_test, target_inconclusive in cases:
            with self.subTest(command=command):
                spec = integrity.parse_command(command.split())
                self.assertEqual(spec.target_inconclusive, target_inconclusive)
                self.assertEqual(
                    tuple(item.kind for item in run_fixture(
                        command, bin_test=bin_test,
                    )),
                    kinds,
                    "mutation self-assert: dynamic target values stay unjudged",
                )

    def test_positive_filters_form_one_or_union(self) -> None:
        hit = "high_risk_recovery::tests::recovery_case"
        for suffix in (
            "missing_case",
            "--exact missing_case",
            f"{hit} missing_case",
        ):
            with self.subTest(suffix=suffix):
                self.assertEqual(run_fixture(
                    f"cargo test --lib {hit} -- {suffix}"
                ), [])
        self.assertEqual(
            [item.kind for item in run_fixture(
                "cargo test --lib missing_one -- missing_two"
            )],
            ["zero-match"],
        )

    def test_exact_aware_skip_applies_to_final_union(self) -> None:
        full = "high_risk_recovery::tests::recovery_case"
        cases = (
            (f"cargo test --lib {full} -- --exact --skip {full}", True),
            (f"cargo test --lib {full} -- --skip {full} --exact", True),
            (f"cargo test --lib {full} -- --exact --skip recovery_case", False),
            (f"cargo test --lib recovery_case -- --skip recovery_case", True),
            (f"cargo test --lib {full} -- --exact --skip other --skip nope", False),
            ("cargo test --lib -- --skip tests", True),
            ("cargo test --lib -- --skip high_risk_recovery "
             "--skip multinode_regression --skip redirected_case", True),
            ("cargo test --lib -- --skip recovery_case", False),
            ("cargo test --lib -- --skip $SKIP", False),
        )
        for command, blocked in cases:
            with self.subTest(command=command):
                violations = run_fixture(command)
                self.assertEqual(bool(violations), blocked, violations)

    def test_libtest_value_options_never_become_filters(self) -> None:
        hit = "high_risk_recovery::tests::recovery_case"
        values = {
            "--test-threads": "1", "--format": "pretty",
            "--color": "always", "--logfile": "fixture.log",
            "-Z": "unstable-options",
        }
        self.assertEqual(set(values), set(integrity.LIBTEST_VALUE_OPTIONS))
        for option, value in values.items():
            for spelling in (f"{option} {value}", f"{option}={value}"):
                with self.subTest(spelling=spelling):
                    command = f"cargo test --lib {hit} -- {spelling}"
                    self.assertEqual(run_fixture(command), [])
                    self.assertEqual(
                        integrity.parse_command(command.split()).filters, (hit,)
                    )
        spec = integrity.parse_command(
            f"cargo test --lib {hit} -- --nocapture literal_filter".split()
        )
        self.assertEqual(spec.filters, (hit, "literal_filter"))

    def test_nested_lib_module_under_bin_is_flagged(self) -> None:
        violations = run_fixture(
            "cargo test --bin agentdesk multinode_regression:: -- --test-threads=1"
        )
        self.assertEqual(len(violations), 1, violations)
        self.assertIn("src/server/mod.rs", violations[0].detail)

    def test_nested_lib_module_under_lib_passes(self) -> None:
        violations = run_fixture(
            "cargo test --lib multinode_regression:: -- --test-threads=1"
        )
        self.assertEqual(violations, [])


class PathRedirection(unittest.TestCase):
    """#[path = "..."] mod declarations must resolve (review blocker #2)."""

    def test_redirected_module_is_not_a_false_positive(self) -> None:
        self.assertEqual(
            run_fixture("cargo test --lib redirected_impl::tests"), [])

    def test_real_repo_inventories_path_redirected_modules(self) -> None:
        # These real modules are only reachable through #[path] redirections
        # (e.g. src/services/auto_queue/route.rs) and produced false
        # unknown-module hits before the fix.
        targets = integrity.discover_targets(REPO_ROOT)
        modules = integrity.collect_modules(targets["lib"], REPO_ROOT)
        for name in ("completion_gate", "liveness", "output_policy",
                     "activate_command"):
            with self.subTest(module=name):
                self.assertIn(name, modules)


class EmptyTargetRule(unittest.TestCase):
    """A filtered command on a module-less target always runs 0 tests."""

    def test_typo_filter_on_empty_bin_is_flagged(self) -> None:
        violations = run_fixture(
            "cargo test --bin agentdesk high_risk_recovry -- --test-threads=1"
        )
        self.assertEqual([v.kind for v in violations], ["empty-target"])

    def test_unfiltered_empty_bin_is_not_flagged(self) -> None:
        self.assertEqual(run_fixture("cargo test --bin agentdesk"), [])

    def test_mismatch_takes_precedence_over_empty_target(self) -> None:
        violations = run_fixture(BAD_COMMAND)
        self.assertEqual([v.kind for v in violations], ["target-mismatch"])


class ExitCodeContract(unittest.TestCase):
    """Warn-only rollout must exit 0; --enforce must exit 1 on violations."""

    def _main_rc(self, command: str, extra: list[str], *,
                 bin_test: bool = False) -> int:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            workflow = build_fixture_repo(root, command, bin_test=bin_test)
            allow = root / "allowlist.txt"
            allow.write_text("", encoding="utf-8")
            argv = ["--repo-root", str(root), "--workflow", str(workflow),
                    "--allowlist", str(allow)] + extra
            with contextlib.redirect_stdout(io.StringIO()):
                return integrity.main(argv)

    def test_warn_only_default_exits_zero_on_violation(self) -> None:
        self.assertEqual(self._main_rc(BAD_COMMAND, []), 0)

    def test_enforce_exits_nonzero_on_violation(self) -> None:
        self.assertEqual(self._main_rc(BAD_COMMAND, ["--enforce"]), 1)

    def test_enforce_exits_zero_when_clean(self) -> None:
        self.assertEqual(self._main_rc(GOOD_COMMAND, ["--enforce"]), 0)

    def test_dynamic_target_value_enforce_exit_codes(self) -> None:
        owned = "bin_owned::tests::owned_case"
        for value in ("$BIN", "${BIN}", "{{BIN}}"):
            with self.subTest(value=value, dynamic=True):
                self.assertEqual(self._main_rc(
                    f"cargo test --bin {value} {owned}", ["--enforce"],
                    bin_test=True,
                ), 0)
            with self.subTest(value=value, dynamic=False):
                self.assertEqual(self._main_rc(
                    f"cargo test --bin missing-bin {owned}", ["--enforce"],
                    bin_test=True,
                ), 1)

    def test_source_floor_shrink_fails_and_addition_stays_green(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            workflow = build_fixture_repo(root, GOOD_COMMAND)
            inventories = integrity.load_lib_inventory_manifest(
                root / integrity.LIB_INVENTORY_MANIFEST_REL
            )
            shrunken = integrity.check_workflows(
                root, [workflow], set(), False, inventories,
                {"workflows": 2, "justfile": 2},
            )
            self.assertEqual(
                [item.kind for item in shrunken],
                ["extraction-floor", "extraction-floor"],
            )
            workflow.write_text(
                workflow.read_text("utf-8")
                + f"      - run: '{GOOD_COMMAND}'\n",
                encoding="utf-8",
            )
            (root / "justfile").write_text(
                f"one:\n    {GOOD_COMMAND}\ntwo:\n    {GOOD_COMMAND}\n",
                encoding="utf-8",
            )
            self.assertEqual(integrity.check_workflows(
                root, [workflow], set(), False, inventories,
                {"workflows": 1, "justfile": 1},
            ), [])

    def test_source_floor_file_is_fail_closed(self) -> None:
        invalid = (
            "", "workflows=1\n", "workflows=1\njustfile=0\n",
            "workflows=-1\njustfile=1\n", "workflows=x\njustfile=1\n",
            "unknown=1\nworkflows=1\njustfile=1\n",
            "workflows=1\nworkflows=2\njustfile=1\n",
            "workflows=1\njustfile=1\nmalformed\n",
        )
        for text in invalid:
            with self.subTest(text=text), tempfile.TemporaryDirectory() as tmp:
                path = Path(tmp) / "floors.txt"
                path.write_text(text, encoding="utf-8")
                with self.assertRaises((ValueError, OSError)):
                    integrity.load_source_floors(path)

    def test_missing_source_floor_file_returns_rc2(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            workflow = build_fixture_repo(root, GOOD_COMMAND)
            (root / integrity.SOURCE_FLOOR_REL).unlink()
            with contextlib.redirect_stderr(io.StringIO()):
                rc = integrity.main([
                    "--repo-root", str(root), "--workflow", str(workflow),
                    "--enforce",
                ])
        self.assertEqual(rc, 2)

    def test_ci_wiring_has_exact_standalone_enforce_call(self) -> None:
        script = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text("utf-8")
        exact = '"$PYTHON" scripts/check_test_target_integrity.py --enforce'
        self.assertEqual(
            [line for line in script.splitlines() if line == exact], [exact],
            "mutation self-assert: removing --enforce must fail this test",
        )


class AllowlistContract(unittest.TestCase):
    def test_allowlist_cannot_excuse_target_mismatch(self) -> None:
        # A target-mismatch means the command itself is wrong; the allowlist
        # (meant for legitimately-empty platform-cfg lanes) must not hide it.
        allow = "# attempted excuse\n" + BAD_COMMAND + "\n"
        violations = run_fixture(BAD_COMMAND, allowlist=allow)
        self.assertEqual([v.kind for v in violations], ["target-mismatch"])

    def test_allowlist_excuses_non_mismatch_kinds(self) -> None:
        command = "cargo test --lib bogus_module::tests"
        self.assertEqual(
            [v.kind for v in run_fixture(command)], ["zero-match"])
        allow = "# legitimately-empty on this platform\n" + command + "\n"
        self.assertEqual(run_fixture(command, allowlist=allow), [])

    def test_comments_and_blanks_do_not_allowlist(self) -> None:
        allow = "# comment only\n\n"
        self.assertEqual(len(run_fixture(BAD_COMMAND, allowlist=allow)), 1)


class ParserContract(unittest.TestCase):
    def test_all_targets_and_unfiltered_commands_are_skipped(self) -> None:
        for command in (
            "cargo test --all-targets -- --skip _pg_ --skip postgres_",
            "cargo test postgres_ -- --nocapture --test-threads=1",
        ):
            with self.subTest(command=command):
                self.assertEqual(run_fixture(command), [])

    def test_wrapped_command_is_still_parsed(self) -> None:
        prefixes = (
            "", "MODE=ci ", "env -u ROOT MODE=ci ",
            "env --unset ROOT --ignore-environment MODE=ci -- ",
            "env -P /usr/bin ", "nice ", "nice -n 10 ",
            "nice --adjustment 10 ", "nice --adjustment=10 ", "nice -10 ",
            "python scripts/ci-timeout.py 900 ",
            "python3 scripts/ci-timeout.py 900 ",
            "python scripts/run_test_lane.py --lane fixture -- ",
            "python3 scripts/run_test_lane.py --lane fixture -- ",
        )
        for prefix in prefixes:
            with self.subTest(prefix=prefix, command="bad"):
                violations = run_fixture(prefix + BAD_COMMAND)
                self.assertEqual(
                    [item.kind for item in violations], ["target-mismatch"]
                )
            with self.subTest(prefix=prefix, command="good"):
                self.assertEqual(run_fixture(prefix + GOOD_COMMAND), [])

    def test_raw_quoted_just_token_is_not_an_invocation(self) -> None:
        just_text = (
            f"real:\n    {GOOD_COMMAND}\n"
            'fixture:\n    "cargo test --bin agentdesk '
            'high_risk_recovery::"\n'
        )
        self.assertEqual(run_fixture(GOOD_COMMAND, just_text=just_text), [])
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "justfile"
            path.write_text(just_text, encoding="utf-8")
            self.assertEqual(len(integrity.extract_justfile_commands(path)), 1)

    def test_metadata_comments_echo_printf_and_assignment_are_not_commands(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            workflow = root / "fixture.yml"
            workflow.write_text(
                "name: cargo test --bin agentdesk bad::\n"
                "# run: cargo test --bin agentdesk bad::\n"
                "jobs:\n  lane:\n    steps:\n"
                "      - uses: cargo test --bin agentdesk bad::\n"
                f"      - run: '{GOOD_COMMAND}'\n",
                encoding="utf-8",
            )
            self.assertEqual(len(integrity.extract_commands(workflow)), 1)
            justfile = root / "justfile"
            justfile.write_text(
                'EXAMPLE := "cargo test --bin agentdesk bad::"\n'
                "fixture:\n"
                "    # cargo test --bin agentdesk bad::\n"
                "    echo cargo test --bin agentdesk bad::\n"
                "    printf cargo test --bin agentdesk bad::\n"
                f"    {GOOD_COMMAND}\n",
                encoding="utf-8",
            )
            self.assertEqual(len(integrity.extract_justfile_commands(justfile)), 1)

    def test_recognized_unsupported_env_string_split_is_inconclusive(self) -> None:
        diagnostics: list[str] = []
        self.assertIsNone(integrity._parse_command_line(
            'env -S "MODE=ci cargo test --lib typo"', diagnostics=diagnostics
        ))
        self.assertEqual(len(diagnostics), 1)
        self.assertIn("inconclusive-wrapper", diagnostics[0])

    def test_substring_filter_without_module_match_is_skipped(self) -> None:
        self.assertEqual(
            [item.kind for item in run_fixture(
                "cargo test --lib some_test_name_fragment"
            )],
            ["zero-match"],
        )


class RunListCheckContract(unittest.TestCase):
    """`--run-list-check` parsing, proven without compiling (mocked cargo)."""

    def _proc(self, stdout: str, rc: int = 0):
        return subprocess.CompletedProcess([], rc, stdout=stdout, stderr="")

    def test_zero_match_is_flagged(self) -> None:
        with mock.patch.object(
            integrity.subprocess, "run",
            return_value=self._proc("0 tests, 0 benchmarks\n"),
        ):
            detail = integrity.run_list_check(
                ["cargo", "test", "--bin", "agentdesk", "high_risk_recovery::"],
                REPO_ROOT,
            )
        self.assertIsNotNone(detail)
        self.assertIn("0 tests", detail)

    def test_nonzero_match_passes(self) -> None:
        with mock.patch.object(
            integrity.subprocess, "run",
            return_value=self._proc("6 tests, 0 benchmarks\n"),
        ):
            self.assertIsNone(integrity.run_list_check(
                ["cargo", "test", "--lib", "high_risk_recovery::"], REPO_ROOT))

    def test_failed_list_run_is_flagged(self) -> None:
        with mock.patch.object(
            integrity.subprocess, "run", return_value=self._proc("", rc=101),
        ):
            detail = integrity.run_list_check(["cargo", "test"], REPO_ROOT)
        self.assertIn("rc=101", detail)


class LibInventoryManifestContract(unittest.TestCase):
    def _fixture(self, root: Path) -> None:
        (root / "src" / "nested").mkdir(parents=True)
        (root / "Cargo.toml").write_text(
            '[package]\nname = "fixture"\n\n[lib]\npath = "src/lib.rs"\n',
            encoding="utf-8",
        )
        (root / "src" / "lib.rs").write_text(
            'mod nested;\nconst FAKE: &str = "#[test] fn string_case() {}";\n',
            encoding="utf-8",
        )
        (root / "src" / "nested" / "mod.rs").write_text(
            """// #[test] fn comment_case() {}
#[cfg(test)]
mod tests {
    const OPEN_BRACE: &str = "{";

    #[test]
    fn plain_case() {}

    #[tokio::test]
    async fn async_case() {}
}

#[cfg(test)]
mod after_string_brace {
    #[test]
    fn keeps_root_scope() {}
}
""",
            encoding="utf-8",
        )

    def test_static_inventory_collects_full_ids_not_comment_or_string(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._fixture(root)
            inventory = integrity.collect_static_tests(root / "src/lib.rs", root)
        self.assertEqual(set(inventory.tests), {
            "nested::tests::plain_case",
            "nested::tests::async_case",
            "nested::after_string_brace::keeps_root_scope",
        })
        self.assertEqual(inventory.module_errors, {})

    def _comparison(self, static_ids: set[str]):
        expected = integrity.expected_lib_static_only(sys.platform)
        assert expected is not None
        return integrity.InventoryComparison(
            expected,
            integrity.LIB_INVENTORY_KNOWN_CARGO_ONLY,
            frozenset(static_ids),
            frozenset(),
        )

    def _write_manifest(self, root: Path, test_ids: set[str], raw: str | None = None) -> Path:
        path = root / integrity.LIB_INVENTORY_MANIFEST_REL
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(
            (raw if raw is not None else
             integrity.render_lib_inventory_manifest(test_ids)).encode("utf-8")
        )
        return path

    def _inventory_cli(self, root: Path, comparison) -> tuple[int, str]:
        output = io.StringIO()
        with mock.patch.object(
            integrity, "compare_lib_inventory", return_value=comparison,
        ), contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            rc = integrity.main([
                "--repo-root", str(root), "--verify-lib-inventory",
            ])
        return rc, output.getvalue()

    def test_manifest_matches_actual_set(self) -> None:
        baseline = {"module::tests::kept", "module::tests::guard"}
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._write_manifest(root, baseline)
            rc, rendered = self._inventory_cli(root, self._comparison(baseline))
        self.assertEqual(rc, 0, rendered)
        self.assertIn("manifest=match", rendered)

    def test_added_id_fails_and_names_the_id(self) -> None:
        baseline = {"module::tests::kept", "module::tests::guard"}
        added = baseline | {"module::tests::new_case"}
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._write_manifest(root, baseline)
            rc, rendered = self._inventory_cli(root, self._comparison(added))
        self.assertNotEqual(rc, 0)
        self.assertIn(
            "lib inventory actual-only (added in source): module::tests::new_case",
            rendered,
        )
        self.assertIn("--write-lib-inventory-manifest", rendered)

    def test_deleted_id_fails_and_names_the_id(self) -> None:
        baseline = {"module::tests::kept", "module::tests::deleted"}
        actual = {"module::tests::kept"}
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._write_manifest(root, baseline)
            rc, rendered = self._inventory_cli(root, self._comparison(actual))
        self.assertNotEqual(rc, 0)
        self.assertIn(
            "lib inventory manifest-only (deleted from source): module::tests::deleted",
            rendered,
        )

    def test_unsorted_manifest_fails_closed(self) -> None:
        raw = "\n".join([
            integrity.LIB_INVENTORY_MANIFEST_HEADER,
            *integrity.LIB_INVENTORY_MANIFEST_RULES,
            "[tests]",
            "module::tests::z_case",
            "module::tests::a_case",
            "",
        ])
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._write_manifest(root, set(), raw=raw)
            output = io.StringIO()
            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
                rc = integrity.main([
                    "--repo-root", str(root), "--verify-lib-inventory",
                ])
        self.assertNotEqual(rc, 0)
        self.assertIn("must be sorted by bytewise UTF-8 order", output.getvalue())

    def test_missing_manifest_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            output = io.StringIO()
            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
                rc = integrity.main([
                    "--repo-root", str(root), "--verify-lib-inventory",
                ])
        self.assertNotEqual(rc, 0)
        rendered = output.getvalue()
        self.assertIn("cannot read", rendered)
        self.assertIn("--write-lib-inventory-manifest", rendered)

    def test_manifest_parser_rejects_duplicates_and_noncanonical_newlines(self) -> None:
        duplicate = "\n".join([
            integrity.LIB_INVENTORY_MANIFEST_HEADER,
            *integrity.LIB_INVENTORY_MANIFEST_RULES,
            "[tests]", "module::tests::same", "module::tests::same", "",
        ])
        with self.assertRaisesRegex(ValueError, "duplicate test IDs"):
            integrity.parse_lib_inventory_manifest(duplicate)
        with self.assertRaisesRegex(ValueError, "final LF"):
            integrity.parse_lib_inventory_manifest(duplicate[:-1])
        with self.assertRaisesRegex(ValueError, "LF line endings"):
            integrity.parse_lib_inventory_manifest(duplicate.replace("\n", "\r\n"))

    def test_write_flag_generates_canonical_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._fixture(root)
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                rc = integrity.main([
                    "--repo-root", str(root),
                    "--write-lib-inventory-manifest",
                ])
            path = root / integrity.LIB_INVENTORY_MANIFEST_REL
            rendered = path.read_bytes()
            manifest_ids = integrity.load_lib_inventory_manifest(path)
        self.assertEqual(rc, 0)
        self.assertEqual(
            manifest_ids,
            frozenset({
                "nested::tests::plain_case",
                "nested::tests::async_case",
                "nested::after_string_brace::keeps_root_scope",
            }),
        )
        self.assertTrue(rendered.endswith(b"\n"))
        self.assertNotIn(b"\r", rendered)
        self.assertIn("entries=3", output.getvalue())

    def test_ci_script_runs_inventory_verifier_as_a_standalone_command(self) -> None:
        lines = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(
            "utf-8"
        ).splitlines()
        self.assertIn(
            '"$PYTHON" scripts/check_test_target_integrity.py --verify-lib-inventory',
            lines,
        )


class ExecutionEvidenceSummaryContract(unittest.TestCase):
    def _summary_fields(self, rendered: str) -> dict[str, int]:
        lines = [line for line in rendered.splitlines()
                 if line.startswith("selection-evidence summary:")]
        self.assertEqual(
            len(lines), 1,
            "fixture self-assert: exactly one observer summary must be emitted",
        )
        fields = {}
        for word in lines[0].split():
            key, separator, value = word.partition("=")
            if separator and value.isdigit():
                fields[key] = int(value)
        return fields

    def _render(self, observations) -> str:
        output = io.StringIO()
        with mock.patch.object(
            integrity, "observe_curated", return_value=observations
        ), contextlib.redirect_stdout(output):
            rc = integrity.main([
                "--repo-root", str(REPO_ROOT), "--observe-selection",
                "--workflow", str(REPO_ROOT / ".github/workflows/ci-pr.yml"),
                "--job", "test_fast",
            ])
        self.assertEqual(rc, 0)
        return output.getvalue()

    def test_summary_matches_observer_state_and_verifier(self) -> None:
        observations = [
            (["cargo", "test", "--lib", "good"], 3, None),
            (["cargo", "test", "--lib", "failed"], 0,
             "list execution failed (plain rc=101, ignored rc=101)"),
        ]
        rendered = self._render(observations)
        self.assertEqual(self._summary_fields(rendered), {
            "invocations": 2, "nonzero": 1, "findings": 1,
            "extraction_errors": 0, "execution_errors": 1,
        }, "fixture self-assert: summary counters must match observer state")
        self.assertEqual(integrity.evidence_verification_errors(rendered), [])

    def test_verifier_rejects_summary_observation_contradiction(self) -> None:
        rendered = self._render([
            (["cargo", "test", "--lib", "empty"], 0,
             "selection has 0 non-ignored test ids"),
        ])
        mutated = rendered.replace("nonzero=0 findings=1",
                                   "nonzero=1 findings=0")
        errors = integrity.evidence_verification_errors(mutated)
        self.assertTrue(errors, "fixture self-assert: contradictory summary must fail")
        self.assertTrue(any("do not match evidence" in error for error in errors))

    def test_verifier_rejects_duplicate_summary_counter(self) -> None:
        rendered = self._render([
            (["cargo", "test", "--lib", "good"], 1, None),
        ])
        mutated = rendered.replace("invocations=1", "invocations=999 invocations=1")
        self.assertIn(
            "duplicate summary counter: invocations",
            integrity.evidence_verification_errors(mutated),
        )

    def test_internal_error_summary_is_truthful(self) -> None:
        output = io.StringIO()
        with mock.patch.object(
            integrity, "observe_curated", side_effect=RuntimeError("boom")
        ), contextlib.redirect_stdout(output):
            rc = integrity.main([
                "--repo-root", str(REPO_ROOT), "--observe-selection",
                "--workflow", str(REPO_ROOT / ".github/workflows/ci-pr.yml"),
                "--job", "test_fast",
            ])
        self.assertEqual(rc, 0)
        rendered = output.getvalue()
        self.assertEqual(self._summary_fields(rendered), {
            "invocations": 0, "nonzero": 0, "findings": 1,
            "extraction_errors": 0, "execution_errors": 1,
        })
        self.assertEqual(integrity.evidence_verification_errors(rendered), [])


class KnownOffenderRegression(unittest.TestCase):
    """Upper-bound ratchet over the real-repo offenders (#5003).

    `mismatches <= KNOWN`: repair slices may shrink the set freely (fixing a
    lane stays green here), but any NEW target-mismatch lane fails this test.
    Once all four offenders are repaired this set can be emptied.
    """

    HRR = "cargo test --bin agentdesk high_risk_recovery:: -- --test-threads=1"
    KNOWN = {
        (".github/workflows/ci-main.yml", HRR),
        (".github/workflows/ci-nightly.yml", HRR),
        (".github/workflows/ci-nightly.yml",
         "cargo test --bin agentdesk multinode_regression:: "
         "-- --nocapture --test-threads=1"),
        (".github/workflows/ci-pr.yml", HRR),
    }

    def test_no_new_offenders_beyond_known_set(self) -> None:
        workflows = sorted((REPO_ROOT / ".github/workflows").glob("*.yml"))
        violations = integrity.check_workflows(
            REPO_ROOT, workflows,
            integrity.load_allowlist(
                REPO_ROOT / "scripts/test_target_integrity_allowlist.txt"),
            with_list_check=False,
        )
        mismatches = {
            (violation.workflow, violation.command)
            for violation in violations
            if violation.kind in ("target-mismatch", "empty-target")
        }
        self.assertTrue(
            mismatches <= self.KNOWN,
            f"NEW mismatch lanes beyond known set: {mismatches - self.KNOWN}",
        )
        unexpected = [
            v for v in violations
            if v.kind not in ("target-mismatch", "empty-target")
        ]
        self.assertEqual(unexpected, [], "gate must not false-positive")

    def test_real_repo_warn_only_run_exits_zero(self) -> None:
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(integrity.main([]), 0)


if __name__ == "__main__":
    unittest.main()
