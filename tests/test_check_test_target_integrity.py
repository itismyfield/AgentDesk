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
import locale
import os
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


def build_layout_repo(root: Path, route_decl: str, *,
                      commands: tuple[str, ...] = (),
                      main_rs: str | None = None,
                      manifest: set[str] | None = None,
                      extra_files: dict[str, str] | None = None) -> Path:
    """Fixture crate whose `src/route.rs` carries one `#[path]` layout.

    Every child module name exists in exactly one file, so the inventory says
    which file the walker really opened: `from_redirect` only lives in the
    redirect target and `from_sibling` only in the sibling's own file.
    """
    workflow = build_fixture_repo(root, GOOD_COMMAND)
    (root / "src" / "route.rs").write_text(
        route_decl + "mod route_sibling;\n", encoding="utf-8")
    (root / "src" / "redirected_impl.rs").write_text(
        "mod from_redirect {}\n#[cfg(test)]\n"
        "mod tests { #[test] fn redirected_case() {} }\n", encoding="utf-8")
    (root / "src" / "route").mkdir(exist_ok=True)
    (root / "src" / "route" / "route_sibling.rs").write_text(
        "mod from_sibling {}\n", encoding="utf-8")
    if main_rs is not None:
        (root / "src" / "main.rs").write_text(main_rs, encoding="utf-8")
    for rel, text in (extra_files or {}).items():
        (root / rel).write_text(text, encoding="utf-8")
    if manifest is not None:
        (root / integrity.LIB_INVENTORY_MANIFEST_REL).write_text(
            integrity.render_lib_inventory_manifest(manifest),
            encoding="utf-8")
    if commands:
        workflow.write_text(
            "jobs:\n  lane:\n    steps:\n"
            + "".join(f'      - run: "{command}"\n' for command in commands),
            encoding="utf-8")
    return workflow



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
        # #5008 item 18 / #5081: prove descent, not just the mod declaration
        # or a hand-written manifest claiming that the child contains a test.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            command = "cargo test --lib redirected_impl::redirect_only"
            build_fixture_repo(root, command)
            (root / integrity.LIB_INVENTORY_MANIFEST_REL).unlink()
            child = root / "src" / "redirected_impl.rs"
            child.write_text(
                "#[cfg(test)]\nmod redirect_only {\n"
                "    #[test]\n    fn child_case() {}\n}\n",
                encoding="utf-8",
            )
            targets = integrity.discover_targets(root)
            modules = integrity.collect_modules(targets["lib"], root)
            self.assertEqual(
                modules.get("redirect_only"), "src/redirected_impl.rs:2",
                "module walker must descend into the redirected child",
            )
            inventory = integrity.collect_static_tests(targets["lib"], root)
            test_id = "route::redirected_impl::redirect_only::child_case"
            self.assertEqual(
                inventory.tests, {test_id: "src/redirected_impl.rs:4"},
                "test scanner must collect the real redirected child test",
            )
            self.assertEqual(inventory.module_errors, {})
            self.assertEqual(inventory.duplicate_tests, ())
            spec = integrity.parse_command(command.split())
            self.assertIsNotNone(spec)
            self.assertEqual(integrity.validate_command(
                spec, {"lib": modules}, root, frozenset(inventory.tests),
            ), [])
            # Removing the real test must make the same filter fail even
            # though both module declarations are still present.
            child.write_text("#[cfg(test)]\nmod redirect_only {}\n",
                             encoding="utf-8")
            empty = integrity.collect_static_tests(targets["lib"], root)
            self.assertEqual(empty.tests, {})
            findings = integrity.validate_command(
                spec, {"lib": modules}, root, frozenset(empty.tests),
            )
            self.assertEqual([kind for kind, _ in findings], ["zero-match"])

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


class PathAttributeLayout(unittest.TestCase):
    """`#[path]` must survive same-line and comment layouts (#5008 items 6/7).

    Both defects were silent inventory losses, not crashes: the same-line form
    dropped the module name and leaked its redirect onto the next `mod`, and a
    comment between the attribute and its `mod` detached the redirect. A lost
    module makes `unknown-module` collapse into nothing whenever the lib is
    also selected, so the gate exits 0 with no output at all.
    """

    # Each layout declares the same `mod redirected_impl;`; only trivia and
    # line breaks differ. `mod` is always the last line of the snippet.
    LAYOUTS = {
        "adjacent_control":
            '#[path = "redirected_impl.rs"]\nmod redirected_impl;\n',
        "cfg_between_control":
            '#[path = "redirected_impl.rs"]\n#[cfg(test)]\n'
            'mod redirected_impl;\n',
        "same_line": '#[path = "redirected_impl.rs"] mod redirected_impl;\n',
        "same_line_pub":
            '#[path = "redirected_impl.rs"] pub mod redirected_impl;\n',
        "same_line_then_cfg":
            '#[path = "redirected_impl.rs"] #[cfg(test)] '
            'mod redirected_impl;\n',
        "cfg_then_same_line":
            '#[cfg(test)] #[path = "redirected_impl.rs"] '
            'mod redirected_impl;\n',
        "same_line_block_comment":
            '#[path = "redirected_impl.rs"] /* moved */ '
            'mod redirected_impl;\n',
        "trailing_comment":
            '#[path = "redirected_impl.rs"] // moved\n'
            'mod redirected_impl;\n',
        "line_comment_between":
            '#[path = "redirected_impl.rs"]\n// why this file moved\n'
            'mod redirected_impl;\n',
        "block_comment_between":
            '#[path = "redirected_impl.rs"]\n/* why this file\n'
            '   moved */\nmod redirected_impl;\n',
        "doc_comment_between":
            '#[path = "redirected_impl.rs"]\n/// the moved module\n'
            'mod redirected_impl;\n',
        "comment_then_cfg":
            '#[path = "redirected_impl.rs"]\n// moved\n#[cfg(test)]\n'
            'mod redirected_impl;\n',
    }

    def test_every_layout_keeps_identity_descent_and_sibling(self) -> None:
        for layout, decl in self.LAYOUTS.items():
            with self.subTest(layout=layout), \
                    tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                build_layout_repo(root, decl)
                modules = integrity.collect_modules(
                    integrity.discover_targets(root)["lib"], root)
                decl_line = decl.count("\n")
                self.assertEqual(
                    modules.get("redirected_impl"),
                    f"src/route.rs:{decl_line}",
                    "the redirected module keeps its own name and site")
                self.assertEqual(
                    modules.get("from_redirect"), "src/redirected_impl.rs:1",
                    "the walker must descend into the redirect target")
                self.assertEqual(
                    modules.get("route_sibling"),
                    f"src/route.rs:{decl_line + 1}",
                    "the sibling `mod` keeps its own name and site")
                self.assertEqual(
                    modules.get("from_sibling"),
                    "src/route/route_sibling.rs:1",
                    "the redirect must not be reused for the next `mod`")

    def test_every_layout_reports_the_real_declaration_site(self) -> None:
        # rc alone proves nothing here: a lost module can turn one finding
        # into a differently-worded one, so assert the kind, the filter and
        # the file each module was actually declared in.
        commands = ("cargo test --bin agentdesk "
                    "redirected_impl::tests::redirected_case",
                    "cargo test --bin agentdesk from_redirect::case",
                    "cargo test --bin agentdesk from_sibling::case")
        for layout, decl in self.LAYOUTS.items():
            with self.subTest(layout=layout), \
                    tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                workflow = build_layout_repo(root, decl, commands=commands)
                violations = integrity.check_workflows(
                    root, [workflow], set(), with_list_check=False)
                self.assertEqual([v.kind for v in violations],
                                 ["target-mismatch"] * 3,
                                 [v.render() for v in violations])
                sites = (f"src/route.rs:{decl.count(chr(10))}",
                         "src/redirected_impl.rs:1",
                         "src/route/route_sibling.rs:1")
                for violation, command, site in zip(violations, commands,
                                                    sites):
                    filt = command.rsplit(" ", 1)[1]
                    self.assertIn(f"filter `{filt}` names module",
                                  violation.detail)
                    self.assertIn(f"lib ({site})", violation.detail)

    def test_bin_side_layout_loss_would_silence_the_gate(self) -> None:
        # The worst shape of #5008 item 6: the bin module vanishes, so the
        # lib-selecting command gets no finding at all and --enforce exits 0.
        commands = ("cargo test --lib bin_owned::tests::owned_case",
                    "cargo test --lib deep_child::case")
        bin_layouts = {
            "adjacent_control":
                '#[path = "renamed_bin.rs"]\nmod bin_owned;\n',
            "same_line": '#[path = "renamed_bin.rs"] mod bin_owned;\n',
            "line_comment_between":
                '#[path = "renamed_bin.rs"]\n// moved\nmod bin_owned;\n',
            "block_comment_between":
                '#[path = "renamed_bin.rs"]\n/* moved */\nmod bin_owned;\n',
        }
        for layout, decl in bin_layouts.items():
            with self.subTest(layout=layout), \
                    tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                workflow = build_layout_repo(
                    root, self.LAYOUTS["adjacent_control"], commands=commands,
                    main_rs=decl + "fn main() {}\n",
                    manifest={"bin_owned::tests::owned_case",
                              "deep_child::case",
                              "high_risk_recovery::tests::recovery_case"},
                    extra_files={"src/renamed_bin.rs":
                                 "mod deep_child {}\n#[cfg(test)]\n"
                                 "mod tests { #[test] fn owned_case() {} }\n"})
                allow = root / "allowlist.txt"
                allow.write_text("", encoding="utf-8")
                stdout = io.StringIO()
                with contextlib.redirect_stdout(stdout):
                    rc = integrity.main([
                        "--repo-root", str(root), "--workflow", str(workflow),
                        "--allowlist", str(allow), "--enforce",
                    ])
                report = stdout.getvalue()
                self.assertEqual(rc, 1, report)
                self.assertIn(
                    "[target-mismatch] filter `bin_owned::tests::owned_case` "
                    "names module `bin_owned` declared in bin:agentdesk "
                    f"(src/main.rs:{decl.count(chr(10))})", report)
                self.assertIn(
                    "[target-mismatch] filter `deep_child::case` names module "
                    "`deep_child` declared in bin:agentdesk "
                    "(src/renamed_bin.rs:1)", report)

    def test_comment_stripping_leaves_literals_and_paths_intact(self) -> None:
        # Naive comment stripping would cut `#[path = "nested//child.rs"]` in
        # half and desynchronise on quotes that merely look like comments.
        noise = ('const LINE: &str = "// not a comment";\n'
                 'const BLOCK: &str = "/* still code */";\n'
                 'const QUOTE: char = \'"\';\n'
                 'const RAW: &str = r#"// "quoted" /* nested */"#;\n'
                 'const TICK: &str = "it\'s fine";\n')
        decl = (noise + '#[path = "nested//redirected_impl.rs"]\n'
                '// the file moved\nmod redirected_impl;\n')
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            build_layout_repo(
                root, decl,
                extra_files={"src/nested_redirect.rs": "mod unused {}\n"})
            nested = root / "src" / "nested"
            nested.mkdir()
            (nested / "redirected_impl.rs").write_text(
                "mod from_redirect {}\n", encoding="utf-8")
            modules = integrity.collect_modules(
                integrity.discover_targets(root)["lib"], root)
        self.assertEqual(modules.get("redirected_impl"),
                         f"src/route.rs:{decl.count(chr(10))}")
        self.assertEqual(modules.get("from_redirect"),
                         "src/nested/redirected_impl.rs:1",
                         "the `//` inside the path string is not a comment")
        self.assertEqual(modules.get("from_sibling"),
                         "src/route/route_sibling.rs:1")

    def test_commented_out_and_interposed_items_still_detach(self) -> None:
        # Treating comments as trivia must not inventory a commented-out
        # `mod`, and real code between #[path] and `mod` must keep detaching
        # the attribute the way rustc does.
        ghosts = ('// mod line_ghost;\n/* mod block_ghost;\n'
                  '   mod second_ghost; */\n')
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            build_layout_repo(
                root, ghosts + self.LAYOUTS["adjacent_control"])
            commented = integrity.collect_modules(
                integrity.discover_targets(root)["lib"], root)
        for ghost in ("line_ghost", "block_ghost", "second_ghost"):
            self.assertNotIn(ghost, commented)
        self.assertEqual(commented.get("from_redirect"),
                         "src/redirected_impl.rs:1")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            build_layout_repo(
                root, '#[path = "redirected_impl.rs"]\n'
                'fn interposed() {}\nmod redirected_impl;\n')
            detached = integrity.collect_modules(
                integrity.discover_targets(root)["lib"], root)
        self.assertEqual(detached.get("redirected_impl"), "src/route.rs:3")
        self.assertNotIn("from_redirect", detached,
                         "an item between #[path] and `mod` detaches it")



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
        return integrity.InventoryComparison(
            frozenset(), frozenset(), frozenset(static_ids), frozenset()
        )

    def _write_manifest(self, root: Path, test_ids: set[str], raw: str | None = None) -> Path:
        path = root / integrity.LIB_INVENTORY_MANIFEST_REL
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(
            (raw if raw is not None else
             integrity.render_lib_inventory_manifest(test_ids)).encode("utf-8")
        )
        return path

    def _inventory_cli_streams(self, root: Path, comparison) \
            -> tuple[int, str, str]:
        stdout, stderr = io.StringIO(), io.StringIO()
        with mock.patch.object(
            integrity, "compare_lib_inventory", return_value=comparison,
        ) as compare, mock.patch.object(
            integrity, "expected_lib_static_only", return_value=frozenset(),
        ), mock.patch.object(
            integrity, "LIB_INVENTORY_KNOWN_CARGO_ONLY", frozenset(),
        ), contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            rc = integrity.main([
                "--repo-root", str(root), "--verify-lib-inventory",
            ])
        if comparison is None:
            compare.assert_not_called()
        return rc, stdout.getvalue(), stderr.getvalue()

    def _inventory_cli(self, root: Path, comparison) -> tuple[int, str]:
        rc, stdout, stderr = self._inventory_cli_streams(root, comparison)
        return rc, stdout + stderr

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

    def test_same_count_rename_reports_both_sides_in_order(self) -> None:
        manifest = {"module::tests::kept", "module::tests::old_id"}
        actual = {"module::tests::kept", "module::tests::new_id"}
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._write_manifest(root, manifest)
            rc, stdout, stderr = self._inventory_cli_streams(
                root, self._comparison(actual)
            )
        self.assertEqual(rc, 1)
        self.assertIn("manifest-only=1 actual-only=1", stdout)
        self.assertIn("manifest-count=2 actual-count=2 delta=+0", stdout)
        self.assertEqual(
            stderr,
            "lib inventory manifest-only (deleted from source): "
            "module::tests::old_id\n"
            "lib inventory actual-only (added in source): "
            "module::tests::new_id\n",
        )

    def test_utf8_bom_is_rejected_before_comparison(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = self._write_manifest(root, {"module::tests::kept"})
            path.write_bytes(b"\xef\xbb\xbf" + path.read_bytes())
            rc, stdout, stderr = self._inventory_cli_streams(root, None)
        self.assertEqual(rc, 2)
        self.assertEqual(stdout, "")
        self.assertTrue(stderr.endswith(
            ": invalid generated-manifest header\n"
            "lib inventory re-pin: run `python3 "
            "scripts/check_test_target_integrity.py "
            "--write-lib-inventory-manifest`, review the manifest diff, "
            "then rerun --verify-lib-inventory\n"
        ), stderr)

    def test_non_ascii_ids_use_utf8_byte_order_and_round_trip(self) -> None:
        ids = {
            "module::tests::z_case",
            "module::tests::ß_case",
            "module::tests::é_case",
        }
        rendered = integrity.render_lib_inventory_manifest(ids)
        rows = rendered.split("[tests]\n", 1)[1].splitlines()
        self.assertEqual(
            rows, sorted(ids, key=lambda value: value.encode("utf-8"))
        )
        self.assertEqual(
            integrity.parse_lib_inventory_manifest(rendered), frozenset(ids)
        )
        self.assertNotIn("\r", rendered)
        self.assertTrue(rendered.endswith("\n"))
        self.assertFalse(rendered.endswith("\n\n"))

    def test_checked_in_manifest_is_canonical_byte_for_byte(self) -> None:
        path = REPO_ROOT / integrity.LIB_INVENTORY_MANIFEST_REL
        checked_in = path.read_bytes()
        with mock.patch.object(integrity.subprocess, "run") as cargo_run:
            parsed = integrity.load_lib_inventory_manifest(path)
            rerendered = integrity.render_lib_inventory_manifest(parsed).encode(
                "utf-8"
            )
        cargo_run.assert_not_called()
        self.assertTrue(parsed)
        self.assertEqual(
            rerendered,
            checked_in,
            "the checked-in manifest must be its own canonical byte rendering",
        )

    def test_manifest_contract_is_locale_independent(self) -> None:
        original = locale.setlocale(locale.LC_ALL)
        proc = subprocess.run(
            ["locale", "-a"], capture_output=True, text=True, check=True
        )
        aliases = ["C", "POSIX", *(
            name for name in proc.stdout.splitlines()
            if "utf8" in name.lower().replace("-", "")
        )]
        ids = {
            "module::tests::z_case",
            "module::tests::ß_case",
            "module::tests::é_case",
        }
        expected_rows = sorted(ids, key=lambda value: value.encode("utf-8"))
        observed = []
        try:
            configured = set()
            for name in aliases:
                canonical = locale.setlocale(locale.LC_ALL, name)
                if canonical in configured:
                    continue
                configured.add(canonical)
                with tempfile.TemporaryDirectory() as tmp:
                    root = Path(tmp)
                    manifest = integrity.render_lib_inventory_manifest(ids)
                    parsed = integrity.parse_lib_inventory_manifest(manifest)
                    self._write_manifest(root, ids, raw=manifest)
                    rc, stdout, stderr = self._inventory_cli_streams(
                        root, self._comparison(ids)
                    )
                observed.append((
                    manifest,
                    sorted(parsed, key=lambda value: value.encode("utf-8")),
                    rc, stdout, stderr,
                ))
        finally:
            locale.setlocale(locale.LC_ALL, original)
        self.assertTrue(observed)
        self.assertTrue(all(item == observed[0] for item in observed[1:]))
        manifest, parsed, rc, stdout, stderr = observed[0]
        self.assertEqual(
            manifest.split("[tests]\n", 1)[1].splitlines(), expected_rows
        )
        self.assertEqual(parsed, expected_rows)
        self.assertEqual((rc, stderr), (0, ""))
        self.assertIn("manifest=match", stdout)

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

    def test_scanner_shrink_regeneration_is_rejected_by_cargo_backstop(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "src").mkdir()
            (root / "Cargo.toml").write_text(
                '[package]\nname = "fixture"\n\n[lib]\npath = "src/lib.rs"\n',
                encoding="utf-8",
            )
            lib_root = root / "src/lib.rs"
            lib_root.write_text(
                "#[cfg(test)]\nmod tests {\n"
                "    #[test]\n    fn kept_by_scanner() {}\n"
                "    #[test]\n    fn dropped_by_regressed_scanner() {}\n"
                "}\n",
                encoding="utf-8",
            )
            complete = integrity.collect_static_tests(lib_root, root)
            omitted = "tests::dropped_by_regressed_scanner"
            self.assertIn(omitted, complete.tests)

            shrunken_tests = dict(complete.tests)
            del shrunken_tests[omitted]
            shrunken = integrity.StaticTestInventory(
                shrunken_tests,
                dict(complete.module_errors),
                complete.duplicate_tests,
            )
            cargo_stdout = "".join(
                f"{test_id}: test\n"
                for test_id in sorted(complete.tests)
            )
            cargo_result = subprocess.CompletedProcess(
                [], 0, stdout=cargo_stdout, stderr=""
            )
            output = io.StringIO()
            with mock.patch.object(
                integrity, "collect_static_tests", return_value=shrunken,
            ):
                with contextlib.redirect_stdout(output):
                    write_rc = integrity.main([
                        "--repo-root", str(root),
                        "--write-lib-inventory-manifest",
                    ])
                regenerated = integrity.load_lib_inventory_manifest(
                    root / integrity.LIB_INVENTORY_MANIFEST_REL
                )
                with mock.patch.object(
                    integrity.subprocess, "run", return_value=cargo_result,
                ) as cargo_run, mock.patch.object(
                    integrity, "expected_lib_static_only",
                    return_value=frozenset(),
                ), mock.patch.object(
                    integrity, "LIB_INVENTORY_KNOWN_CARGO_ONLY", frozenset(),
                ), contextlib.redirect_stdout(output), \
                        contextlib.redirect_stderr(output):
                    verify_rc = integrity.main([
                        "--repo-root", str(root),
                        "--verify-lib-inventory",
                    ])

        self.assertEqual(write_rc, 0)
        self.assertEqual(regenerated, frozenset(shrunken_tests))
        self.assertNotIn(
            omitted,
            regenerated,
            "mutation self-assert: regeneration absorbed scanner undercollection",
        )
        self.assertEqual(verify_rc, 1, output.getvalue())
        self.assertIn("manifest=match", output.getvalue())
        self.assertIn(
            "manifest-only=0 actual-only=0 static-only=0 cargo-only=1",
            output.getvalue(),
        )
        self.assertIn(f"lib inventory cargo-only: {omitted}", output.getvalue())
        cargo_run.assert_called_once()
        cargo_argv = cargo_run.call_args.args[0]
        self.assertEqual(cargo_argv, [
            "cargo", "test", "--manifest-path",
            str(root.resolve() / "Cargo.toml"),
            "--lib", "--", "--list",
        ])

    def test_sequential_git_merges_preserve_manifest_union(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            fixture_root = Path(tmp)
            root = fixture_root / "repo"
            root.mkdir()
            missing_gpg = fixture_root / "missing-gpg"
            hostile_global = fixture_root / "hostile.gitconfig"
            hostile_global.write_text(
                "[core]\n\tautocrlf = true\n"
                "[commit]\n\tgpgSign = true\n",
                encoding="utf-8",
            )
            inherited_git_env = dict(os.environ)
            inherited_git_env.update({
                "GIT_CONFIG_GLOBAL": str(hostile_global),
                "GIT_CONFIG_SYSTEM": str(hostile_global),
                "GIT_CONFIG_COUNT": "3",
                "GIT_CONFIG_KEY_0": "core.autocrlf",
                "GIT_CONFIG_VALUE_0": "true",
                "GIT_CONFIG_KEY_1": "commit.gpgSign",
                "GIT_CONFIG_VALUE_1": "true",
                "GIT_CONFIG_KEY_2": "gpg.program",
                "GIT_CONFIG_VALUE_2": str(missing_gpg),
            })
            git_env = {
                key: value for key, value in inherited_git_env.items()
                if key != "GIT_CONFIG" and not key.startswith("GIT_CONFIG_")
            }
            git_env.update({
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_CONFIG_GLOBAL": os.devnull,
            })
            self.assertEqual(
                {
                    key: value for key, value in git_env.items()
                    if key == "GIT_CONFIG" or key.startswith("GIT_CONFIG_")
                },
                {
                    "GIT_CONFIG_NOSYSTEM": "1",
                    "GIT_CONFIG_GLOBAL": os.devnull,
                },
            )
            (root / "src").mkdir()
            (root / ".gitattributes").write_text(
                "scripts/** text eol=lf\n", encoding="utf-8"
            )
            (root / "Cargo.toml").write_text(
                '[package]\nname = "fixture"\n\n[lib]\npath = "src/lib.rs"\n',
                encoding="utf-8",
            )
            (root / "src/lib.rs").write_text(
                "mod alpha;\nmod middle;\nmod omega;\n", encoding="utf-8"
            )
            for module in ("alpha", "middle", "omega"):
                cases = "\n".join(
                    f"    #[test]\n    fn base_{index}() {{}}"
                    for index in range(6)
                )
                (root / f"src/{module}.rs").write_text(
                    f"#[cfg(test)]\nmod tests {{\n{cases}\n}}\n",
                    encoding="utf-8",
                )

            def git(*args: str) -> subprocess.CompletedProcess[str]:
                return subprocess.run(
                    ["git", *args], cwd=root, check=True,
                    capture_output=True, text=True, env=git_env,
                )

            def regenerate() -> None:
                with contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(integrity.main([
                        "--repo-root", str(root),
                        "--write-lib-inventory-manifest",
                    ]), 0)

            for key in ("core.autocrlf", "commit.gpgSign"):
                hostile = subprocess.run(
                    ["git", "config", "--get", key], cwd=root,
                    check=True, capture_output=True, text=True,
                    env=inherited_git_env,
                )
                self.assertEqual(hostile.stdout.strip(), "true")

            git("init", "-q", "-b", "main")
            git("config", "user.name", "Inventory Contract")
            git("config", "user.email", "inventory@example.invalid")
            git("config", "commit.gpgSign", "false")
            git("config", "core.autocrlf", "false")
            git("config", "core.eol", "lf")
            self.assertEqual(
                git("config", "--local", "--get", "commit.gpgSign")
                .stdout.strip(),
                "false",
            )
            self.assertEqual(
                git("config", "--local", "--get", "core.autocrlf")
                .stdout.strip(),
                "false",
            )
            regenerate()
            git("add", ".")
            git("commit", "-q", "-m", "base inventory")
            self.assertEqual(
                git("show", "HEAD:.gitattributes").stdout,
                "scripts/** text eol=lf\n",
            )

            git("switch", "-q", "-c", "branch-a")
            alpha = root / "src/alpha.rs"
            alpha.write_text(
                alpha.read_text("utf-8")
                + "#[cfg(test)]\nmod branch_a { #[test] fn added() {} }\n",
                encoding="utf-8",
            )
            regenerate()
            branch_a_ids = integrity.load_lib_inventory_manifest(
                root / integrity.LIB_INVENTORY_MANIFEST_REL
            )
            self.assertIn("alpha::branch_a::added", branch_a_ids)
            self.assertNotIn("omega::branch_b::added", branch_a_ids)
            git("add", ".")
            git("commit", "-q", "-m", "add alpha test")

            git("switch", "-q", "main")
            git("switch", "-q", "-c", "branch-b")
            omega = root / "src/omega.rs"
            omega.write_text(
                omega.read_text("utf-8")
                + "#[cfg(test)]\nmod branch_b { #[test] fn added() {} }\n",
                encoding="utf-8",
            )
            regenerate()
            branch_b_ids = integrity.load_lib_inventory_manifest(
                root / integrity.LIB_INVENTORY_MANIFEST_REL
            )
            self.assertIn("omega::branch_b::added", branch_b_ids)
            self.assertNotIn("alpha::branch_a::added", branch_b_ids)
            git("add", ".")
            git("commit", "-q", "-m", "add omega test")

            git("switch", "-q", "main")
            git("merge", "-q", "--no-ff", "--no-edit", "branch-a")
            git("merge", "-q", "--no-ff", "--no-edit", "branch-b")

            inventory = integrity.collect_static_tests(root / "src/lib.rs", root)
            static_ids = frozenset(inventory.tests)
            manifest_ids = integrity.load_lib_inventory_manifest(
                root / integrity.LIB_INVENTORY_MANIFEST_REL
            )
            manifest_raw = (
                root / integrity.LIB_INVENTORY_MANIFEST_REL
            ).read_bytes()
            manifest_rel = integrity.LIB_INVENTORY_MANIFEST_REL.as_posix()
            self.assertEqual(
                git("check-attr", "text", "eol", "--", manifest_rel)
                .stdout.splitlines(),
                [
                    f"{manifest_rel}: text: set",
                    f"{manifest_rel}: eol: lf",
                ],
            )
            self.assertNotIn(b"\r", manifest_raw)
            cargo_result = subprocess.CompletedProcess(
                [], 0,
                stdout="".join(
                    f"{test_id}: test\n" for test_id in sorted(static_ids)
                ),
                stderr="",
            )
            output = io.StringIO()
            with mock.patch.object(
                integrity.subprocess, "run", return_value=cargo_result,
            ), mock.patch.object(
                integrity, "expected_lib_static_only", return_value=frozenset(),
            ), mock.patch.object(
                integrity, "LIB_INVENTORY_KNOWN_CARGO_ONLY", frozenset(),
            ), contextlib.redirect_stdout(output), \
                    contextlib.redirect_stderr(output):
                verify_rc = integrity.main([
                    "--repo-root", str(root), "--verify-lib-inventory",
                ])

            self.assertEqual(git("status", "--porcelain").stdout, "")

        self.assertEqual(manifest_ids, static_ids)
        self.assertIn("alpha::branch_a::added", manifest_ids)
        self.assertIn("omega::branch_b::added", manifest_ids)
        self.assertEqual(
            integrity.render_lib_inventory_manifest(manifest_ids).encode("utf-8"),
            manifest_raw,
        )
        self.assertEqual(verify_rc, 0, output.getvalue())
        self.assertIn("manifest=match", output.getvalue())

    def test_ci_script_runs_one_timed_verifier_after_unittests(self) -> None:
        # This is deliberately source-only: asserting CI wiring must not start
        # the Cargo-backed verifier from the cargo-free unittest lane.
        lines = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(
            "utf-8"
        ).splitlines()
        unittest_line = (
            '"$PYTHON" -m unittest tests.test_check_test_target_integrity'
        )
        verifier_line = (
            'AGENTDESK_CI_TIMEOUT_REPORT=1 "$PYTHON" scripts/ci-timeout.py 900 '
            '"$PYTHON" scripts/check_test_target_integrity.py '
            '--verify-lib-inventory'
        )
        verifier_lines = [
            line for line in lines
            if "--verify-lib-inventory" in line
        ]
        self.assertEqual(
            verifier_lines,
            [verifier_line],
            "the Cargo verifier must have one exact, timed CI invocation",
        )
        self.assertEqual(lines.count(unittest_line), 1)
        self.assertLess(lines.index(unittest_line), lines.index(verifier_line))


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


def write_files(root: Path, files: dict[str, str]) -> None:
    for rel, text in files.items():
        target = root / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")


def build_frame_repo(root: Path, files: dict[str, str], *,
                     lib_path: str = "src/lib.rs") -> Path:
    """Crate whose only content is the module layout under test.

    Every layout built here is valid Rust, and the asserted leaf location
    is the one rustc itself demands. Each layout was compiled standalone
    with `rustc --edition 2021 --crate-type lib --emit=metadata`
    (rustc 1.94.1 and 1.94.0; no cargo/linking/execution) as written and
    again with each asserted leaf removed: only the layout as written
    compiles, so the decoy locations are not accepted substitutes.
    Unreferenced `decoy_*` files are never read by rustc.
    """
    (root / "Cargo.toml").write_text(
        '[package]\nname = "fixture"\nversion = "0.1.0"\n'
        f'edition = "2021"\n\n[lib]\npath = "{lib_path}"\n\n'
        '[[bin]]\nname = "fixture"\npath = "src/main.rs"\n',
        encoding="utf-8")
    write_files(root, {"src/main.rs": "fn main() {}\n", **files})
    return root / lib_path


@contextlib.contextmanager
def record_reads():
    """Record every file path the walker actually opens."""
    opened: list[str] = []
    real = Path.read_text

    def traced(self, *args, **kwargs):
        opened.append(str(self))
        return real(self, *args, **kwargs)

    with mock.patch.object(Path, "read_text", traced):
        yield opened


class InlineDirectoryContext(unittest.TestCase):
    """Compiler-backed directory ownership (#5008 item 8).

    Unique no-test leaves identify actual reads versus decoys; even empty
    modules distinguish target-mismatch from unknown-module.
    """

    # layout -> (files, {module: first declaration site}, forbidden modules)
    LAYOUTS: dict[str, tuple[dict[str, str], dict[str, str],
                             tuple[str, ...]]] = {
        # `foo.rs` owns `foo/`; an inline scope appends its own name.
        "non_mod_rs_inline": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner.rs": "mod tests {\n    mod child;\n}\n",
            "src/owner/tests/child.rs": "mod leaf_non_mod_rs {}\n",
            "src/child.rs": "mod decoy_declaring_dir {}\n",
            "src/tests/child.rs": "mod decoy_relative_dropped {}\n",
        }, {
            "owner": "src/lib.rs:1",
            "tests": "src/owner.rs:1",
            "child": "src/owner.rs:2",
            "leaf_non_mod_rs": "src/owner/tests/child.rs:1",
        }, ("decoy_declaring_dir", "decoy_relative_dropped")),
        # `owner/mod.rs` owns `owner/` with nothing pending.
        "mod_rs_inline": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner/mod.rs": "mod tests {\n    mod child;\n}\n",
            "src/owner/tests/child.rs": "mod leaf_mod_rs {}\n",
            "src/owner/child.rs": "mod decoy_mod_rs_flat {}\n",
        }, {
            "child": "src/owner/mod.rs:2",
            "leaf_mod_rs": "src/owner/tests/child.rs:1",
        }, ("decoy_mod_rs_flat",)),
        # voice_barge_in: outlined #[path] resolves in its enclosing inline scope.
        "outlined_path_inside_inline_scope": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner.rs":
                'mod tests {\n    #[path = "pcm.rs"]\n    mod pcm;\n}\n',
            "src/owner/tests/pcm.rs": "mod leaf_in_scope {}\n",
            "src/pcm.rs": "mod decoy_file_parent {}\n",
        }, {
            "pcm": "src/owner.rs:3",
            "leaf_in_scope": "src/owner/tests/pcm.rs:1",
        }, ("decoy_file_parent",)),
        # Inline #[path] renames the directory without consuming relative.
        "inline_path_directory_override": ({
            "src/lib.rs": "mod layout;\n",
            "src/layout.rs":
                '#[path = "moved_dir"]\nmod scope {\n    mod child;\n}\n',
            "src/moved_dir/child.rs": "mod leaf_inline_path {}\n",
            "src/layout/scope/child.rs": "mod decoy_override_ignored {}\n",
            "src/layout/moved_dir/child.rs": "mod decoy_relative_used {}\n",
        }, {
            "child": "src/layout.rs:3",
            "leaf_inline_path": "src/moved_dir/child.rs:1",
        }, ("decoy_override_ignored", "decoy_relative_used")),
        # A `#[path]` file is mod.rs-like: its children are its siblings.
        "redirect_child_is_a_sibling": ({
            "src/lib.rs": '#[path = "renamed.rs"]\nmod alpha;\n',
            "src/renamed.rs": "mod nested;\n",
            "src/nested.rs": "mod leaf_sibling {}\n",
            "src/renamed/nested.rs": "mod decoy_stem_reattached {}\n",
        }, {
            "nested": "src/renamed.rs:1",
            "leaf_sibling": "src/nested.rs:1",
        }, ("decoy_stem_reattached",)),
        # Literal braces do not move scope; a closed inline scope cannot leak.
        "nested_scopes_and_literal_braces": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner.rs":
                "mod outer {\n"
                '    const BRACES: &str = "} mod fake { {";\n'
                "    mod empty { }\n"
                "    mod inner {\n"
                "        mod deep;\n"
                "    }\n"
                "}\n"
                "mod after_scopes;\n",
            "src/owner/outer/inner/deep.rs": "mod leaf_deep {}\n",
            "src/owner/after_scopes.rs": "mod leaf_after {}\n",
            "src/owner/deep.rs": "mod decoy_scope_lost {}\n",
            "src/owner/outer/after_scopes.rs": "mod decoy_scope_leaked {}\n",
        }, {
            "deep": "src/owner.rs:5",
            "after_scopes": "src/owner.rs:8",
            "leaf_deep": "src/owner/outer/inner/deep.rs:1",
            "leaf_after": "src/owner/after_scopes.rs:1",
        }, ("decoy_scope_lost", "decoy_scope_leaked", "fake")),
        # rustc warns on the non-module attribute, but it cannot rename a sibling.
        "path_on_a_closed_item_never_renames_a_later_scope": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner.rs":
                '#[path = "moved_dir"]\nstruct Marker { held: u8 }\n'
                "mod scope {\n    mod child;\n}\n",
            "src/owner/scope/child.rs": "mod leaf_after_closed_item {}\n",
            "src/moved_dir/child.rs": "mod decoy_attr_leaked {}\n",
        }, {
            "child": "src/owner.rs:4",
            "leaf_after_closed_item": "src/owner/scope/child.rs:1",
        }, ("decoy_attr_leaked",)),
        # Semicolon items consume their attributes just as braced items do.
        "path_on_a_semicolon_item_never_renames_a_later_scope": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner.rs":
                '#[path = "moved_type"]\ntype Alias = u8;\n'
                "mod scope {\n    mod child;\n}\n",
            "src/owner/scope/child.rs": "mod leaf_after_alias {}\n",
            "src/moved_type/child.rs": "mod decoy_alias_leaked {}\n",
        }, {
            "child": "src/owner.rs:4",
            "leaf_after_alias": "src/owner/scope/child.rs:1",
        }, ("decoy_alias_leaked",)),
        # String brackets cannot extend an attribute over later item bodies.
        "attribute_string_brackets_never_extend_the_attribute": ({
            "src/lib.rs":
                'mod outer {\n    #[doc = "["]\n    pub fn helper() {}\n}\n'
                '#[doc = "]"]\nmod sibling;\n',
            "src/sibling.rs": "mod leaf_real_sibling {}\n",
            "src/outer/sibling.rs": "mod decoy_attr_swallowed_scope {}\n",
        }, {
            "sibling": "src/lib.rs:6",
            "leaf_real_sibling": "src/sibling.rs:1",
        }, ("decoy_attr_swallowed_scope",)),
        # The string `]` names a directory; it does not close the attribute.
        "inline_path_directory_named_with_a_bracket": ({
            "src/lib.rs": '#[path = "]"]\nmod outer {\n    mod child;\n}\n',
            "src/]/child.rs": "mod leaf_bracket_directory {}\n",
            "src/outer/child.rs": "mod decoy_bracket_attr_lost {}\n",
        }, {
            "child": "src/lib.rs:3",
            "leaf_bracket_directory": "src/]/child.rs:1",
        }, ("decoy_bracket_attr_lost",)),
        # Function blocks drop the file's pending relative component.
        "block_drops_the_files_pending_relative": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner.rs":
                "fn helper() {\n    mod scope {\n"
                '        #[path = "moved.rs"]\n        mod child;\n    }\n}\n',
            "src/scope/moved.rs": "mod leaf_block_scope {}\n",
            "src/owner/scope/moved.rs": "mod decoy_relative_kept {}\n",
        }, {
            "child": "src/owner.rs:4",
            "leaf_block_scope": "src/scope/moved.rs:1",
        }, ("decoy_relative_kept",)),
        # Dropping relative is distinct from an inline #[path] directory rename.
        "block_then_redirected_inline_scope": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner.rs":
                'fn helper() {\n    #[path = "renamed"]\n    mod scope {\n'
                '        #[path = "moved.rs"]\n        mod child;\n    }\n}\n',
            "src/renamed/moved.rs": "mod leaf_block_renamed {}\n",
            "src/owner/renamed/moved.rs": "mod decoy_block_relative_kept {}\n",
        }, {
            "child": "src/owner.rs:5",
            "leaf_block_renamed": "src/renamed/moved.rs:1",
        }, ("decoy_block_relative_kept",)),
        # Block-local #[path] resolves in the enclosing inline directory.
        "block_level_path_resolves_against_its_inline_scope": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner.rs":
                "mod scope {\n    fn helper() {\n"
                '        #[path = "moved.rs"]\n        mod inner;\n    }\n}\n',
            "src/owner/scope/moved.rs": "mod leaf_block_path {}\n",
            "src/owner/moved.rs": "mod decoy_block_at_file_dir {}\n",
        }, {
            "inner": "src/owner.rs:4",
            "leaf_block_path": "src/owner/scope/moved.rs:1",
        }, ("decoy_block_at_file_dir",)),
        # A string `"#"` before an index expression is not attribute punctuation.
        "string_hash_before_an_index_is_not_an_attribute": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner.rs":
                'fn helper() {\n    let _ = &"#"[{\n        mod scope {\n'
                '            #[path = "moved.rs"]\n            mod child;\n'
                "        }\n        0\n    }..];\n}\n",
            "src/scope/moved.rs": "mod leaf_string_hash {}\n",
            "src/moved.rs": "mod decoy_string_hash_opened_attr {}\n",
        }, {
            "child": "src/owner.rs:5",
            "leaf_string_hash": "src/scope/moved.rs:1",
        }, ("decoy_string_hash_opened_attr",)),
        # A byte string is the same token with a `b` in front of it.
        "byte_string_hash_before_an_index_is_not_an_attribute": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner.rs":
                'fn helper() {\n    let _ = b"#"[{\n        mod scope {\n'
                '            #[path = "moved.rs"]\n            mod child;\n'
                "        }\n        0\n    }];\n}\n",
            "src/scope/moved.rs": "mod leaf_byte_hash {}\n",
            "src/moved.rs": "mod decoy_byte_hash_opened_attr {}\n",
        }, {
            "child": "src/owner.rs:5",
            "leaf_byte_hash": "src/scope/moved.rs:1",
        }, ("decoy_byte_hash_opened_attr",)),
        # Macro token-tree braces preserve the file's relative component.
        "macro_delimiter_braces_are_not_a_block": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner.rs":
                "macro_rules! passthrough { ($($t:tt)*) => { $($t)* }; }\n"
                "passthrough! {\n    mod scope {\n"
                '        #[path = "moved.rs"]\n        mod child;\n    }\n}\n',
            "src/owner/scope/moved.rs": "mod leaf_macro_wrapper {}\n",
            "src/scope/moved.rs": "mod decoy_macro_read_as_block {}\n",
        }, {
            "child": "src/owner.rs:5",
            "leaf_macro_wrapper": "src/owner/scope/moved.rs:1",
        }, ("decoy_macro_read_as_block",)),
        # Parenthesized wrappers retain the same directory ownership.
        "parenthesized_macro_wrapper_keeps_the_component": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner.rs":
                "macro_rules! passthrough { ($($t:tt)*) => { $($t)* }; }\n"
                "passthrough!(\n    mod scope {\n"
                '        #[path = "moved.rs"]\n        mod child;\n    }\n);\n',
            "src/owner/scope/moved.rs": "mod leaf_paren_wrapper {}\n",
            "src/scope/moved.rs": "mod decoy_paren_read_as_block {}\n",
        }, {
            "child": "src/owner.rs:5",
            "leaf_paren_wrapper": "src/owner/scope/moved.rs:1",
        }, ("decoy_paren_read_as_block",)),
        # R3: source gaps distinguish dropped &&; closing the block restores relative.
        "and_unary_not_block_is_not_a_macro_header": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner.rs":
                "const FLAG: bool = true;\n"
                "const VALUE: bool = FLAG && !{\n"
                "    mod scope {\n"
                '        #[path = "moved.rs"]\n        mod child;\n    }\n'
                "    false\n};\n"
                'mod sibling {\n    #[path = "tail.rs"]\n    mod tail;\n}\n',
            "src/scope/moved.rs": "mod leaf_and_unary_not {}\n",
            "src/owner/sibling/tail.rs": "mod leaf_and_tail {}\n",
            "src/owner/scope/moved.rs": "mod decoy_and_read_as_macro {}\n",
        }, {
            "child": "src/owner.rs:5",
            "tail": "src/owner.rs:11",
            "leaf_and_unary_not": "src/scope/moved.rs:1",
            "leaf_and_tail": "src/owner/sibling/tail.rs:1",
        }, ("decoy_and_read_as_macro",)),
        # R3: `if` lexes as an ident; its keyword status must reject the header.
        "if_unary_not_block_is_not_a_macro_header": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner.rs":
                "const VALUE: bool = if !{\n    mod scope {\n"
                '        #[path = "moved.rs"]\n        mod child;\n    }\n'
                "    false\n} { true } else { false };\n"
                'mod sibling {\n    #[path = "tail.rs"]\n    mod tail;\n}\n',
            "src/scope/moved.rs": "mod leaf_if_unary_not {}\n",
            "src/owner/sibling/tail.rs": "mod leaf_if_tail {}\n",
            "src/owner/scope/moved.rs": "mod decoy_if_read_as_macro {}\n",
        }, {
            "child": "src/owner.rs:4",
            "tail": "src/owner.rs:10",
            "leaf_if_unary_not": "src/scope/moved.rs:1",
            "leaf_if_tail": "src/owner/sibling/tail.rs:1",
        }, ("decoy_if_read_as_macro",)),
        # MB: skip the whole inner attribute, not just a nested #[path] payload.
        "inner_attribute_payload_never_renames_a_later_scope": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner.rs":
                '#![cfg_attr(any(), opaque(#[path = "fake"]))]\n'
                'mod scope {\n    #[path = "moved.rs"]\n    mod child;\n}\n',
            "src/owner/scope/moved.rs": "mod leaf_inner_attr {}\n",
            "src/fake/moved.rs": "mod decoy_inner_path_leaked {}\n",
        }, {
            "child": "src/owner.rs:4",
            "leaf_inner_attr": "src/owner/scope/moved.rs:1",
        }, ("decoy_inner_path_leaked",)),
        # Preserve parenthesized transcribers and nested namespaced calls.
        "namespaced_and_nested_wrappers_keep_the_component": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner.rs":
                "macro_rules! define_tree {\n"
                "    () => ( mod defined {\n        mod one;\n    } );\n}\n"
                "define_tree!();\n#[macro_export]\n"
                "macro_rules! passthrough { ($($t:tt)*) => ( $($t)* ); }\n"
                "crate::passthrough! {\n    crate::passthrough! {\n"
                "        mod scope {\n"
                '            #[path = "moved.rs"]\n            mod child;\n'
                "        }\n    }\n}\n",
            "src/owner/defined/one.rs": "mod leaf_transcriber {}\n",
            "src/owner/scope/moved.rs": "mod leaf_nested_wrapper {}\n",
            "src/defined/one.rs": "mod decoy_transcriber_block {}\n",
            "src/scope/moved.rs": "mod decoy_nested_read_as_block {}\n",
        }, {
            "one": "src/owner.rs:3",
            "child": "src/owner.rs:13",
            "leaf_transcriber": "src/owner/defined/one.rs:1",
            "leaf_nested_wrapper": "src/owner/scope/moved.rs:1",
        }, ("decoy_transcriber_block", "decoy_nested_read_as_block")),
        # R5: raw keyword and non-keyword definitions both have three-token names.
        "raw_definition_names_keep_the_component": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner.rs":
                "macro_rules! r#match {\n    () => ( mod keyword_defined {\n"
                "        mod one;\n    } );\n}\n"
                "macro_rules! r#define_tree {\n"
                "    () => ( mod ordinary_defined {\n"
                "        mod two;\n    } );\n}\n"
                "r#match!();\nr#define_tree!();\n"
                'mod sibling {\n    #[path = "tail.rs"]\n    mod tail;\n}\n',
            "src/owner/keyword_defined/one.rs": "mod leaf_raw_keyword {}\n",
            "src/owner/ordinary_defined/two.rs": "mod leaf_raw_ordinary {}\n",
            "src/owner/sibling/tail.rs": "mod leaf_raw_tail {}\n",
            "src/keyword_defined/one.rs": "mod decoy_raw_keyword_block {}\n",
            "src/ordinary_defined/two.rs": "mod decoy_raw_ordinary_block {}\n",
        }, {
            "one": "src/owner.rs:3",
            "two": "src/owner.rs:8",
            "tail": "src/owner.rs:15",
            "leaf_raw_keyword": "src/owner/keyword_defined/one.rs:1",
            "leaf_raw_ordinary": "src/owner/ordinary_defined/two.rs:1",
            "leaf_raw_tail": "src/owner/sibling/tail.rs:1",
        }, ("decoy_raw_keyword_block", "decoy_raw_ordinary_block")),
        # R6: the function owns this inner path, not the next inline module.
        "inner_path_on_a_function_never_renames_a_later_scope": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner.rs":
                'fn helper() {\n    #![path = "fake"]\n    mod scope {\n'
                '        #[path = "moved.rs"]\n        mod child;\n    }\n}\n'
                'mod sibling {\n    #[path = "tail.rs"]\n    mod tail;\n}\n',
            "src/scope/moved.rs": "mod leaf_inner_owner {}\n",
            "src/owner/sibling/tail.rs": "mod leaf_inner_tail {}\n",
            "src/fake/moved.rs": "mod decoy_inner_path_to_next_item {}\n",
        }, {
            "child": "src/owner.rs:5",
            "tail": "src/owner.rs:10",
            "leaf_inner_owner": "src/scope/moved.rs:1",
            "leaf_inner_tail": "src/owner/sibling/tail.rs:1",
        }, ("decoy_inner_path_to_next_item",)),
        # R7: a present `#[path]` joins the current directory whatever it
        # spells, so `""` is presence, not absence.
        "inline_empty_path_override_is_not_absence": ({
            "src/lib.rs": "mod owner;\n",
            "src/owner.rs": '#[path = ""]\nmod scope {\n    mod child;\n}\n'
                            'mod sibling {\n    #[path = "tail.rs"]\n'
                            "    mod tail;\n}\n",
            "src/child.rs": "mod leaf_empty_override {}\n",
            "src/owner/sibling/tail.rs": "mod leaf_empty_tail {}\n",
            "src/owner/scope/child.rs": "mod decoy_empty_read_as_absent {}\n",
        }, {
            "child": "src/owner.rs:3",
            "tail": "src/owner.rs:7",
            "leaf_empty_override": "src/child.rs:1",
            "leaf_empty_tail": "src/owner/sibling/tail.rs:1",
        }, ("decoy_empty_read_as_absent",)),
    }

    def test_every_frame_resolves_the_way_rustc_does(self) -> None:
        for layout, (files, expected, forbidden) in self.LAYOUTS.items():
            with self.subTest(layout=layout), \
                    tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                lib = build_frame_repo(root, files)
                with record_reads() as opened:
                    modules = integrity.collect_modules(lib, root)
                for name, site in expected.items():
                    self.assertEqual(modules.get(name), site,
                                     f"{layout}: {name}")
                for name in forbidden:
                    self.assertNotIn(name, modules,
                                     f"{layout}: decoy must not be recorded")
                decoys = {str(root / rel) for rel, text in files.items()
                          if "decoy" in text}
                self.assertEqual(sorted(decoys & set(opened)), [],
                                 f"{layout}: decoy file must never be read")
                # Empty modules still participate in filter classification.
                self.assertEqual(
                    integrity.collect_static_tests(lib, root).tests, {},
                    f"{layout}: fixture is a no-test marker layout")

    ROOTS = {
        # A crate root owns its own directory whatever it is called.
        "custom_lib_root": ("src/custom_root.rs", {
            "src/custom_root.rs": "mod child;\n",
            "src/child.rs": "mod leaf_custom_root {}\n",
            "src/custom_root/child.rs": "mod decoy_root_as_module {}\n",
        }, "leaf_custom_root", "src/child.rs:1", "decoy_root_as_module"),
        "integration_root": ("tests/smoke.rs", {
            "tests/smoke.rs": "mod helper;\n",
            "tests/helper.rs": "mod leaf_integration {}\n",
            "tests/smoke/helper.rs": "mod decoy_test_as_module {}\n",
        }, "leaf_integration", "tests/helper.rs:1", "decoy_test_as_module"),
    }

    # layout -> (filter, actual file, decoy); rc=1 alone cannot distinguish them.
    ENTRYPOINTS = {
        "attribute_string_brackets_never_extend_the_attribute":
            ("leaf_real_sibling::case", "src/sibling.rs",
             "src/outer/sibling.rs"),
        "block_drops_the_files_pending_relative":
            ("leaf_block_scope::case", "src/scope/moved.rs",
             "src/owner/scope/moved.rs"),
        "string_hash_before_an_index_is_not_an_attribute":
            ("leaf_string_hash::case", "src/scope/moved.rs",
             "src/moved.rs"),
        "macro_delimiter_braces_are_not_a_block":
            ("leaf_macro_wrapper::case", "src/owner/scope/moved.rs",
             "src/scope/moved.rs"),
        "and_unary_not_block_is_not_a_macro_header":
            ("leaf_and_unary_not::case", "src/scope/moved.rs",
             "src/owner/scope/moved.rs"),
        "if_unary_not_block_is_not_a_macro_header":
            ("leaf_if_unary_not::case", "src/scope/moved.rs",
             "src/owner/scope/moved.rs"),
        "inner_attribute_payload_never_renames_a_later_scope":
            ("leaf_inner_attr::case", "src/owner/scope/moved.rs",
             "src/fake/moved.rs"),
        "namespaced_and_nested_wrappers_keep_the_component":
            ("leaf_nested_wrapper::case", "src/owner/scope/moved.rs",
             "src/scope/moved.rs"),
        "raw_definition_names_keep_the_component":
            ("leaf_raw_keyword::case", "src/owner/keyword_defined/one.rs",
             "src/keyword_defined/one.rs"),
        "inner_path_on_a_function_never_renames_a_later_scope":
            ("leaf_inner_owner::case", "src/scope/moved.rs",
             "src/fake/moved.rs"),
        "inline_empty_path_override_is_not_absence":
            ("leaf_empty_override::case", "src/child.rs",
             "src/owner/scope/child.rs"),
    }

    def test_boundary_layouts_reach_main_with_the_real_file(self) -> None:
        # Assert classification, actual site and decoy non-read, not just rc=1.
        for layout, (filt, site, decoy) in self.ENTRYPOINTS.items():
            with self.subTest(layout=layout), \
                    tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                build_frame_repo(root, self.LAYOUTS[layout][0])
                decoys = self.LAYOUTS[layout][2]
                workflow = root / ".github/workflows/ci-fixture.yml"
                write_files(root, {
                    str(integrity.LIB_INVENTORY_MANIFEST_REL):
                        integrity.render_lib_inventory_manifest(set()),
                    str(integrity.SOURCE_FLOOR_REL): "workflows=1\njustfile=1\n",
                    "justfile": f"fixture:\n    cargo test --lib {filt}\n",
                    "allowlist.txt": "",
                    ".github/workflows/ci-fixture.yml":
                        "jobs:\n  lane:\n    steps:\n" + "".join(
                            f'      - run: "cargo test --bin fixture {one}"\n'
                            for one in (filt, *(f"{d}::case" for d in decoys))),
                })
                stdout, stderr = io.StringIO(), io.StringIO()
                with record_reads() as opened, \
                        contextlib.redirect_stdout(stdout), \
                        contextlib.redirect_stderr(stderr):
                    rc = integrity.main([
                        "--repo-root", str(root), "--workflow", str(workflow),
                        "--allowlist", str(root / "allowlist.txt"), "--enforce",
                    ])
                report = stdout.getvalue()
                self.assertEqual(rc, 1, report)
                self.assertIn(
                    f"[target-mismatch] filter `{filt}` names module "
                    f"`{filt.split('::')[0]}` declared in lib ({site}:1)",
                    report)
                self.assertEqual(stderr.getvalue(), "")
                self.assertNotIn(str(root / decoy), opened,
                                 f"{layout}: main must not read the decoy")
                # Check the opposite direction too: decoys must be unknown.
                for name in decoys:
                    self.assertIn("[unknown-module] module-path filter "
                                  f"`{name}::case`", report)

    # spelling -> (actual leaf site, decoy site). Only an absent attribute
    # consumes the file's relative component and appends the module's name.
    PATH_SPELLINGS = {
        '#[path = ""]\n': ("src/child.rs:1", None),
        '#[path = "."]\n': ("src/child.rs:1", None),
        '#[path = "moved"]\n': (None, None),
        "": (None, "src/owner/scope/child.rs:1"),
    }

    def test_a_present_inline_path_is_not_an_absent_one(self) -> None:
        files = self.LAYOUTS["inline_empty_path_override_is_not_absence"][0]
        for spelling, (leaf, decoy) in self.PATH_SPELLINGS.items():
            with self.subTest(path=spelling or "absent"), \
                    tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                owner = files["src/owner.rs"].replace('#[path = ""]\n',
                                                      spelling)
                lib = build_frame_repo(root, {**files, "src/owner.rs": owner})
                modules = integrity.collect_modules(lib, root)
                self.assertEqual(modules.get("leaf_empty_override"), leaf)
                self.assertEqual(modules.get("decoy_empty_read_as_absent"),
                                 decoy)

    def test_custom_and_integration_roots_own_their_directory(self) -> None:
        for label, (root_rel, files, leaf, site, decoy) in self.ROOTS.items():
            with self.subTest(root=label), \
                    tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                build_frame_repo(root, files)
                modules = integrity.collect_modules(root / root_rel, root)
                self.assertEqual(modules.get(leaf), site, label)
                self.assertNotIn(decoy, modules, label)

    def test_integration_target_is_collected_once_through_validation(self) \
            -> None:
        # The lazy `--test` inventory used `setdefault`, whose argument
        # is evaluated even when the target is already cached, so the
        # frame walk re-read the whole integration tree per command.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            build_frame_repo(root, {
                "src/lib.rs": "",
                "tests/smoke.rs": "mod helper;\n",
                "tests/helper.rs": "mod leaf_integration {}\n",
            })
            inventories: dict[str, dict[str, str]] = {}
            walked: list[Path] = []
            real = integrity.collect_modules

            def counted(source: Path, repo: Path) -> dict[str, str]:
                walked.append(source)
                return real(source, repo)

            with mock.patch.object(integrity, "collect_modules", counted):
                for filt in ("leaf_integration::case", "other::case"):
                    spec = integrity.parse_command(
                        f"cargo test --test smoke {filt}".split())
                    integrity.validate_command(spec, inventories, root)
            self.assertEqual(
                inventories["test:smoke"].get("leaf_integration"),
                "tests/helper.rs:1")
            self.assertEqual(walked, [root / "tests/smoke.rs"])

    def test_same_file_under_two_owners_is_walked_for_both(self) -> None:
        # `src/owner/shared.rs` is both a plain child of `owner` and a
        # `#[path]` alias at the crate root. rustc compiles this only when
        # BOTH child files exist, so both ownerships must be walked while
        # the reported site stays the real declaration line.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            lib = build_frame_repo(root, {
                "src/lib.rs": 'mod owner;\n#[path = "owner/shared.rs"]\n'
                              "mod aliased;\n",
                "src/owner.rs": "mod shared;\n",
                "src/owner/shared.rs": "mod leaf_shared;\n",
                "src/owner/shared/leaf_shared.rs": "mod leaf_owner_route {}\n",
                "src/owner/leaf_shared.rs": "mod leaf_alias_route {}\n",
            })
            modules = integrity.collect_modules(lib, root)
            self.assertEqual(modules.get("leaf_owner_route"),
                             "src/owner/shared/leaf_shared.rs:1")
            self.assertEqual(modules.get("leaf_alias_route"),
                             "src/owner/leaf_shared.rs:1")
            self.assertEqual(modules.get("leaf_shared"),
                             "src/owner/shared.rs:1")

    def test_read_errors_propagate_and_never_become_missing(self) -> None:
        # An unreadable file that exists is not an absent module: the OS
        # error has to reach the caller, which fails the lane closed.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            lib = build_frame_repo(root, {
                "src/lib.rs": "mod owner;\n",
                "src/owner.rs": "mod child;\n",
                "src/owner/child.rs": "mod leaf {}\n",
            })
            with mock.patch.object(Path, "read_text",
                                   side_effect=OSError("unreadable source")):
                with self.assertRaises(OSError):
                    integrity.collect_modules(lib, root)
            (root / "src/owner/child.rs").write_bytes(b"mod leaf {\xff}\n")
            with self.assertRaises(UnicodeError):
                integrity.collect_modules(lib, root)


class RustConsistentCrateProof(unittest.TestCase):
    """Compiler-backed main() oracle: real site, positive --enforce, no decoy.

    Bin filtering stays invalid; the cited file, not rc=1, proves ownership.
    """

    LIB = 'mod outer {\n    #[path = "renamed.rs"]\n    mod alpha;\n}\n'
    CHILD = "mod deep_child {\n    #[test]\n    fn case() {}\n}\n"
    BAD_BIN = "cargo test --bin fixture deep_child::case"
    GOOD_LIB = "cargo test --lib outer::alpha::deep_child::case"

    def build(self, root: Path, commands: tuple[str, ...], *,
              decoy: bool = False) -> Path:
        files = {
            "src/lib.rs": self.LIB,
            "src/outer/renamed.rs": self.CHILD,
            "src/main.rs": "#[test]\nfn bin_smoke() {}\nfn main() {}\n",
        }
        if decoy:
            # Same file name one directory up: what the old walker read.
            files["src/renamed.rs"] = "mod decoy_only {}\n"
        build_frame_repo(root, files)
        write_files(root, {
            str(integrity.LIB_INVENTORY_MANIFEST_REL):
                integrity.render_lib_inventory_manifest(
                    {"outer::alpha::deep_child::case"}),
            str(integrity.SOURCE_FLOOR_REL): "workflows=1\njustfile=1\n",
            "justfile": f"fixture:\n    {self.GOOD_LIB}\n",
            "allowlist.txt": "",
            ".github/workflows/ci-fixture.yml":
                "jobs:\n  lane:\n    steps:\n" + "".join(
                    f'      - run: "{command}"\n' for command in commands),
        })
        return root / ".github/workflows/ci-fixture.yml"

    def run_main(self, root: Path, workflow: Path, *args: str) \
            -> tuple[int, str, str]:
        stdout, stderr = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(stdout), \
                contextlib.redirect_stderr(stderr):
            rc = integrity.main([
                "--repo-root", str(root), "--workflow", str(workflow),
                "--allowlist", str(root / "allowlist.txt"), *args,
            ])
        return rc, stdout.getvalue(), stderr.getvalue()

    def test_deep_child_is_named_with_its_real_declaration_site(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            workflow = self.build(root, (self.BAD_BIN,))
            rc, report, errs = self.run_main(root, workflow, "--enforce")
            self.assertEqual(rc, 1, report)
            self.assertIn(
                "[target-mismatch] filter `deep_child::case` names module "
                "`deep_child` declared in lib (src/outer/renamed.rs:1)",
                report)
            self.assertNotIn("unknown-module", report)
            self.assertNotIn("module-path-", report)
            self.assertEqual(errs, "")

    def test_corrected_lib_command_stays_clean(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            workflow = self.build(root, (self.GOOD_LIB,))
            rc, report, errs = self.run_main(root, workflow, "--enforce")
            self.assertEqual(rc, 0, report)
            self.assertIn("test-target integrity check passed", report)
            self.assertNotIn("module-path-", report)
            self.assertEqual(errs, "")

    def test_same_named_parent_decoy_is_never_read(self) -> None:
        decoy_command = "cargo test --bin fixture decoy_only::case"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            workflow = self.build(root, (self.BAD_BIN, decoy_command),
                                  decoy=True)
            with record_reads() as opened:
                rc, report, _ = self.run_main(root, workflow, "--enforce")
            self.assertEqual(rc, 1, report)
            self.assertIn("[unknown-module] module-path filter "
                          "`decoy_only::case`", report)
            self.assertIn("declared in lib (src/outer/renamed.rs:1)", report)
            self.assertNotIn(str(root / "src/renamed.rs"), opened,
                             "the walker must not read the same-named file "
                             "next to the declaring source")
            modules = integrity.collect_modules(root / "src/lib.rs", root)
            self.assertNotIn("decoy_only", modules)
            self.assertEqual(modules.get("deep_child"),
                             "src/outer/renamed.rs:1")


class MacroHeaderPredicate(unittest.TestCase):
    """Header predicate controls for spans, source gaps and keyword names."""

    # source -> is its last `{` a macro's token-tree delimiter?
    HEADERS = {
        "passthrough! { }": True,
        "path::to::passthrough! { }": True,
        "passthrough !\n{ }": True,
        "passthrough /* moved */ ! // why\n{ }": True,
        "r#match! { }": True,  # a raw identifier escapes the keyword rule
        "macro_rules! passthrough { }": True,
        "macro_rules /* c */ ! passthrough { }": True,
        "FLAG && !{ }": False,  # the operators the lexer drops
        "FLAG || !{ }": False,
        "MASK & !{ }": False,
        "if !{ }": False,  # keywords are idents to this lexer
        "while !{ }": False,
        "match !{ }": False,
        "_ !{ }": False,
        "!{ }": False,
        "{ }": False,
        '"passthrough" ! { }': False,  # a string is not a name
        "macro_rules! if { }": False,
        "macro_rules! r#match { }": True,  # the name is three tokens wide
        "macro_rules! r#define_tree { }": True,
        "macro_rules /* c */ ! /* c */ r#match /* c */ { }": True,
        "macro_rules! r #match { }": False,  # `r#NAME` must be contiguous
        "macro_rules! r# match { }": False,
        "macro_rules! r #plain { }": False,
        "macro_rules! r# plain { }": False,
    }

    def last_brace(self, tokens: list) -> int:
        return max(offset for offset, token in enumerate(tokens)
                   if token.kind == "punct" and token.value == "{")

    def test_only_a_real_lexical_header_delimits_a_macro(self) -> None:
        for source, expected in self.HEADERS.items():
            with self.subTest(source=source):
                tokens = integrity._rust_tokens(source)
                self.assertIs(
                    integrity._macro_delimiter(
                        tokens, self.last_brace(tokens), source),
                    expected)

    def test_an_unproven_gap_is_not_a_macro_header(self) -> None:
        # Spanless tokens and empty source cannot prove a header.
        source = "passthrough! { }"
        tokens = integrity._rust_tokens(source)
        index = self.last_brace(tokens)
        self.assertTrue(integrity._macro_delimiter(tokens, index, source))
        spanless = [integrity.RustToken(token.value, token.line, token.kind)
                    for token in tokens]
        self.assertEqual(spanless, tokens, "spans must not change identity")
        self.assertFalse(integrity._macro_delimiter(spanless, index, source))
        self.assertFalse(integrity._macro_delimiter(tokens, index, ""))


class StaticAttributeBoundaries(unittest.TestCase):
    def test_attribute_payloads_do_not_change_item_boundaries(self) -> None:
        sources = {
            "inner_test_payload": ('#![cfg_attr(any(), opaque(#[test]))]\n'
                                   'fn not_a_test() {}\n', {}),
            "string_brackets": ('#[doc = "["]\n#[test]\nfn real_test() {}\n'
                                '#[doc = "]"]\nfn not_a_test() {}\n',
                                {"real_test": "src/lib.rs:3"}),
            "string_hash_index": ('fn helper() { let _ = &"#"[{\n'
                                  '#[test] fn nested() {}\n0 }..]; }\n',
                                  {"nested": "src/lib.rs:2"}),
            "inner_path_payload": ('#![cfg_attr(any(), opaque(#[path = "fake.rs"]))]\n'
                                   'mod child;\n',
                                   {"child::real_test": "src/child.rs:1"}),
            # An inner attribute belongs to the form it is written in,
            # so a function's own `#![path]` is not the next mod's.
            "inner_path_on_a_function": ('fn helper() {\n'
                                         '    #![path = "fake.rs"]\n}\n'
                                         'mod child;\n',
                                         {"child::real_test": "src/child.rs:1"}),
        }
        for label, (source, expected) in sources.items():
            with self.subTest(label=label), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                lib = build_frame_repo(root, {
                    "src/lib.rs": source,
                    "src/child.rs": "#[test] fn real_test() {}\n",
                    "src/fake.rs": "#[test] fn fake_test() {}\n",
                })
                inventory = integrity.collect_static_tests(lib, root)
                self.assertEqual(inventory.tests, expected)
                self.assertEqual(inventory.module_errors, {})
                self.assertEqual(inventory.duplicate_tests, ())


if __name__ == "__main__":
    unittest.main()
