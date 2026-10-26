"""Unit tests for scripts/check_contract_symbol_refs.py (#4268).

The checker's job is ONLY the doc<->code set comparison; symbol EXISTENCE is
proven by the compiler (`cargo check --all-targets` on the
`relay_state_contract_refs` reference blocks). Round 3 moved the anchor SET from
`// sym:` *comments* to the compiler-checked reference expressions themselves, so
these tests target: doc-anchor extraction, the code-derived Rust-anchor parser
(use / field / assoc-fn forms + `super::`/`crate::` resolution), the exact-cfg
gate, the set comparison, and the distinct-anchor floor. Each round-2/round-3
false-pass has a dedicated reproduction.
"""

from __future__ import annotations

import importlib.util
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest
from unittest.mock import patch
from pathlib import Path

import yaml
from tests.test_high_risk_recovery_path_filter import pattern_to_regex, select

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / "scripts" / "check_contract_symbol_refs.py"

_SPEC = importlib.util.spec_from_file_location("check_contract_symbol_refs", SCRIPT_PATH)
CHECKER = importlib.util.module_from_spec(_SPEC)
assert _SPEC.loader is not None
sys.modules[_SPEC.name] = CHECKER
_SPEC.loader.exec_module(CHECKER)

INFLIGHT_BASE = ("inflight", "store")


def _block(body: str, cfg: str = "#[cfg(test)]") -> str:
    """Wrap reference statements in a `relay_state_contract_refs` module."""
    return textwrap.dedent(
        f"""
        {cfg}
        mod relay_state_contract_refs {{
            #[test]
            fn contract_symbols_exist() {{
        {textwrap.indent(textwrap.dedent(body), " " * 8)}
            }}
        }}
        """
    )


class ExtractDocAnchorsTest(unittest.TestCase):
    def test_inline_anchors_extracted_distinct(self) -> None:
        text = textwrap.dedent(
            """
            - Definition `sym:inflight::model::InflightTurnState::response_sent_offset`.
            - Prose backtick ignored: `save_inflight_state`.
            - Repeat `sym:inflight::model::InflightTurnState::response_sent_offset` again.
            - Other `sym:turn_bridge::spawn_turn_bridge`.
            """
        )
        self.assertEqual(
            CHECKER.extract_doc_anchors(text),
            {
                "inflight::model::InflightTurnState::response_sent_offset",
                "turn_bridge::spawn_turn_bridge",
            },
        )

    def test_defect2_fenced_block_anchors_excluded(self) -> None:
        text = textwrap.dedent(
            """
            Real `sym:a::b::Kept`.

            ```
            `sym:a::b::FencedExample`
            ```

            ~~~
            `sym:a::b::TildeFenced`
            ~~~
            """
        )
        self.assertEqual(CHECKER.extract_doc_anchors(text), {"a::b::Kept"})

    def test_defect2_indented_block_anchors_excluded(self) -> None:
        text = (
            "Real `sym:a::b::Kept`.\n"
            "\n"
            "    `sym:a::b::SpaceIndented`\n"
            "\n"
            "\t`sym:a::b::TabIndented`\n"
        )
        self.assertEqual(CHECKER.extract_doc_anchors(text), {"a::b::Kept"})

    def test_list_continuation_anchors_are_not_treated_as_code(self) -> None:
        text = (
            "- Definition: `InflightTurnState::x`\n"
            "    (`sym:inflight::model::InflightTurnState::x`).\n"
        )
        self.assertEqual(
            CHECKER.extract_doc_anchors(text),
            {"inflight::model::InflightTurnState::x"},
        )

    def test_angle_bracket_placeholder_ignored(self) -> None:
        self.assertEqual(CHECKER.extract_doc_anchors("`sym:<module>::<Symbol>`"), set())


class ResolveSymbolTest(unittest.TestCase):
    def test_single_super_keeps_host_module(self) -> None:
        self.assertEqual(
            CHECKER._resolve_symbol("super::validate_inflight_state_for_save", INFLIGHT_BASE),
            "inflight::store::validate_inflight_state_for_save",
        )

    def test_double_super_drops_one_component(self) -> None:
        self.assertEqual(
            CHECKER._resolve_symbol("super::super::save_store::save_inflight_state", INFLIGHT_BASE),
            "inflight::save_store::save_inflight_state",
        )

    def test_discord_crate_prefix_stripped(self) -> None:
        self.assertEqual(
            CHECKER._resolve_symbol(
                "crate::services::discord::tmux::advance_watcher_confirmed_end", INFLIGHT_BASE
            ),
            "tmux::advance_watcher_confirmed_end",
        )

    def test_service_sibling_prefix_retained(self) -> None:
        self.assertEqual(
            CHECKER._resolve_symbol(
                "crate::services::provider::CancelToken::turn_nonce", INFLIGHT_BASE
            ),
            "provider::CancelToken::turn_nonce",
        )

    def test_non_service_crate_path_raises(self) -> None:
        with self.assertRaises(ValueError):
            CHECKER._resolve_symbol("crate::config::Config", INFLIGHT_BASE)

    def test_too_many_supers_raises(self) -> None:
        with self.assertRaises(ValueError):
            CHECKER._resolve_symbol("super::super::super::super::x", INFLIGHT_BASE)


class CfgGateTest(unittest.TestCase):
    """Byte-exact block-cfg whitelist + item-attribute whitelist (r5). No parser:
    the checker enumerates the exact allowed cfg spellings and the exact allowed
    in-block attribute (`#[test]`)."""

    _REF = "use super::validate_inflight_state_for_save as _;\n"

    def _errors(self, *, cfg: str = "#[cfg(test)]", body: str | None = None):
        _, errors = CHECKER.extract_rust_anchors(
            _block(body if body is not None else self._REF, cfg=cfg), INFLIGHT_BASE, "store.rs"
        )
        return errors

    def test_accepted_block_cfgs(self) -> None:
        # The only two whitelisted gates. `unix` is allowed only because the
        # required PR compile (the `lint` clippy) is ubuntu-latest.
        for cfg in ("#[cfg(test)]", "#[cfg(all(test, unix))]"):
            self.assertEqual(self._errors(cfg=cfg), [], cfg)

    def test_rejected_block_cfgs(self) -> None:
        for cfg in (
            "#[cfg(all(test, windows))]",              # no required job compiles it
            "#[cfg(all(test, not(unix)))]",            # false on required ubuntu
            '#[cfg(all(test, target_os = "freebsd"))]',
            '#[cfg(all(test, target_feature = "avx2"))]',
            "#[cfg(all(test, target_os = linux))]",    # malformed (unquoted)
            '#[cfg(all(test, target_os = "linux))]',   # malformed (unterminated)
            '#[cfg(all(test, feature = "never"))]',    # feature can be off everywhere
            "#[cfg(not(test))]",                       # production-only
            "#[cfg(any(test, unix))]",                 # compiles without test
            '#[cfg(all(test, target_os = "linux"))]',  # off-whitelist spelling (intended)
            "#[cfg(unix)]",                            # no test
        ):
            self.assertTrue(self._errors(cfg=cfg), cfg)

    def test_item_level_attribute_on_reference_is_rejected(self) -> None:
        # The r5 hole: an item-level cfg keeps the block alive (block-cfg check
        # passes) but drops that ONE reference from the required compile.
        for attr in (
            '#[cfg(feature = "never")]',
            "#[cfg(unix)]",
            "#[cfg(not(test))]",
        ):
            body = f"{attr}\nuse super::validate_inflight_state_for_save as _;\n"
            errors = self._errors(cfg="#[cfg(test)]", body=body)
            self.assertTrue(any("item-level cfg" in e for e in errors), (attr, errors))


class StripperTest(unittest.TestCase):
    def test_strips_preserve_length_and_newlines(self) -> None:
        src = 'let a = 1; // note\nlet s = "x{y}\\"z";\n/* multi\nline */ let b = 2;\n'
        out = CHECKER._strip_comments_and_strings(src)
        self.assertEqual(len(out), len(src))
        self.assertEqual(out.count("\n"), src.count("\n"))
        self.assertNotIn("note", out)
        self.assertNotIn("x{y}", out)
        self.assertNotIn("multi", out)
        self.assertIn("let a = 1;", out)
        self.assertIn("let b = 2;", out)

    def test_nested_block_comments(self) -> None:
        src = "/* outer /* inner */ still comment */ let x = 1;"
        out = CHECKER._strip_comments_and_strings(src)
        self.assertNotIn("still comment", out)
        self.assertIn("let x = 1;", out)

    def test_raw_strings_with_hashes(self) -> None:
        src = 'let s = r#"has "quote" and #[cfg(test)] mod fake {}"#; let y = 2;'
        out = CHECKER._strip_comments_and_strings(src)
        self.assertNotIn("mod fake", out)
        self.assertNotIn("#[cfg(test)]", out)
        self.assertIn("let y = 2;", out)

    def test_raw_identifier_not_treated_as_string(self) -> None:
        src = "use super::r#fn as _;"
        self.assertEqual(CHECKER._strip_comments_and_strings(src), src)


class R6CommentEvasionTest(unittest.TestCase):
    """The three r6 holes: raw-text matching let comments/strings defeat the
    attribute walk, block discovery, and reference counting."""

    _GOOD_BODY = (
        "    #[test]\n"
        "    fn contract_symbols_exist() {\n"
        "        use super::validate_inflight_state_for_save as _;\n"
        "    }\n"
    )

    def _mod(self, attr_stack: str, cfg: str = "#[cfg(test)]") -> str:
        return f"{attr_stack}{cfg}\nmod relay_state_contract_refs {{\n{self._GOOD_BODY}}}\n"

    def test_r6_attr_hidden_above_line_comment_is_collected(self) -> None:
        # rustc attaches BOTH attrs to the mod (comments are transparent); the
        # checker must too, and fail the len==1 whitelist.
        text = self._mod('#[cfg(feature = "never")]\n// innocuous comment\n')
        _, errors = CHECKER.extract_rust_anchors(text, INFLIGHT_BASE, "x.rs")
        self.assertTrue(any("must be gated" in e for e in errors), errors)

    def test_r6_attr_hidden_above_block_comment_is_collected(self) -> None:
        text = self._mod('#[cfg(feature = "never")]\n/* spacer */\n')
        _, errors = CHECKER.extract_rust_anchors(text, INFLIGHT_BASE, "x.rs")
        self.assertTrue(any("must be gated" in e for e in errors), errors)

    def test_r6_cfg_attr_above_blank_line_is_collected(self) -> None:
        text = self._mod('#[cfg_attr(test, cfg(feature = "never"))]\n\n')
        _, errors = CHECKER.extract_rust_anchors(text, INFLIGHT_BASE, "x.rs")
        self.assertTrue(any("must be gated" in e for e in errors), errors)

    def test_r6_fake_block_in_raw_string_does_not_shadow_real_block(self) -> None:
        fake = (
            'const FAKE: &str = r#"\n'
            "#[cfg(test)]\n"
            "mod relay_state_contract_refs {\n"
            "    #[test]\n"
            "    fn contract_symbols_exist() {\n"
            "        use super::validate_inflight_state_for_save as _;\n"
            "    }\n"
            "}\n"
            '"#;\n'
        )
        text = fake + self._mod("", cfg="#[cfg(all(test, unix, windows))]")
        _, errors = CHECKER.extract_rust_anchors(text, INFLIGHT_BASE, "x.rs")
        self.assertTrue(any("must be gated" in e for e in errors), errors)

    def test_r6_fake_block_in_block_comment_does_not_shadow_real_block(self) -> None:
        fake = (
            "/*\n"
            "#[cfg(test)]\n"
            "mod relay_state_contract_refs {\n"
            "    fn contract_symbols_exist() {}\n"
            "}\n"
            "*/\n"
        )
        text = fake + self._mod("", cfg='#[cfg(all(test, feature = "never"))]')
        _, errors = CHECKER.extract_rust_anchors(text, INFLIGHT_BASE, "x.rs")
        self.assertTrue(any("must be gated" in e for e in errors), errors)

    def test_r6_block_commented_use_is_not_an_anchor(self) -> None:
        body = "/*\nuse super::validate_inflight_state_for_save as _;\n*/\n"
        anchors, errors = CHECKER.extract_rust_anchors(
            _block(body), INFLIGHT_BASE, "x.rs"
        )
        self.assertEqual(errors, [])
        self.assertEqual(anchors, set())


class R7StructuralSealTest(unittest.TestCase):
    """r7 seals: char-literal lexing, block uniqueness, top-level declaration."""

    _GOOD = (
        "#[cfg(test)]\n"
        "mod relay_state_contract_refs {\n"
        "    #[test]\n"
        "    fn contract_symbols_exist() {\n"
        "        use super::validate_inflight_state_for_save as _;\n"
        "    }\n"
        "}\n"
    )

    def test_r7_char_literal_blanked_lifetime_kept(self) -> None:
        src = "let q = '\"'; let e = '\\''; fn f<'a>(x: &'a str) {}"
        out = CHECKER._strip_comments_and_strings(src)
        self.assertNotIn("'\"'", out)
        self.assertNotIn("'\\''", out)
        self.assertIn("<'a>", out)
        self.assertIn("&'a str", out)
        self.assertEqual(len(out), len(src))

    def test_r7_char_trap_cannot_hide_item_attr(self) -> None:
        # codex R7-1: `'"'` desyncs a naive string state so the attr line lands
        # "inside a string" and is blanked. With char literals lexed, the attr
        # survives stripping and the item-attr whitelist rejects it.
        body = (
            "let _q = '\"';\n"
            '#[cfg(feature = "never")] // "\n'
            "use super::validate_inflight_state_for_save as _;\n"
        )
        _, errors = CHECKER.extract_rust_anchors(_block(body), INFLIGHT_BASE, "x.rs")
        self.assertTrue(any("disallowed attribute" in e for e in errors), errors)

    def test_r8_byte_and_c_literal_prefixes_fully_blanked(self) -> None:
        # codex R8-1: r7 lexed only the bare `'x'` of `b'x'`, leaving a stray
        # `b` that _ASSOC_RE collected as a phantom `b` anchor (false-positive
        # rc=1 on an honest byte-literal edit). The prefix letter is part of
        # the Rust token and must be blanked with the literal.
        for src in (
            "let _ = b'x';",
            'let _ = b"hi";',
            'let _ = br#"hi"#;',
            'let _ = c"hi";',
            'let _ = cr#"hi"#;',
        ):
            out = CHECKER._strip_comments_and_strings(src)
            self.assertEqual(out, "let _ =" + " " * (len(src) - 8) + ";", src)
        # Token-start guard: an identifier ending in `b` is not a prefix, and a
        # lone `b` in expression position stays code.
        for keep in ("blob_b = 1", "x < b && b < y", "fn r#fn() {}"):
            self.assertEqual(CHECKER._strip_comments_and_strings(keep), keep)

    def test_r8_byte_char_in_anchor_block_makes_no_phantom_anchor(self) -> None:
        # End-to-end shape of codex R8-1: an honest `let _ = b'x';` line inside
        # the anchor block must not add a `b` symbol to the extracted anchors.
        body = (
            "let _ = b'x';\n"
            "use super::validate_inflight_state_for_save as _;\n"
        )
        anchors, errors = CHECKER.extract_rust_anchors(
            _block(body), INFLIGHT_BASE, "x.rs"
        )
        base = "::".join(INFLIGHT_BASE)
        self.assertEqual(errors, [], errors)
        # Exact set: under the r7 lexer the stray prefix produced a bare `b`
        # anchor here (and rc=1 in the full doc<->code comparison).
        self.assertEqual(anchors, {f"{base}::validate_inflight_state_for_save"})

    def test_r7_macro_decoy_duplicate_declaration_fails(self) -> None:
        # codex R7-2: an unexpanded macro_rules! transcriber holds a decoy block;
        # uniqueness (exactly one declaration per file) rejects it regardless of
        # which one a first-match scan would have adopted.
        decoy = (
            "macro_rules! decoy {\n"
            "    () => {\n"
            "mod relay_state_contract_refs {\n"
            "    fn contract_symbols_exist() {}\n"
            "}\n"
            "    };\n"
            "}\n"
        )
        _, errors = CHECKER.extract_rust_anchors(decoy + self._GOOD, INFLIGHT_BASE, "x.rs")
        self.assertTrue(any("exactly once" in e for e in errors), errors)

    def test_r7_nested_under_cfg_parent_fails_top_level(self) -> None:
        # codex R7-3: a disabled ancestor (`#[cfg(feature = "never")] mod parent`)
        # compiles the whole subtree out; the immediate cfg looked fine. Any
        # nesting (indentation) is rejected.
        text = (
            '#[cfg(feature = "never")]\n'
            "mod parent {\n"
            "    #[cfg(test)]\n"
            "    mod relay_state_contract_refs {\n"
            "        #[test]\n"
            "        fn contract_symbols_exist() {\n"
            "            use super::super::validate_inflight_state_for_save as _;\n"
            "        }\n"
            "    }\n"
            "}\n"
        )
        _, errors = CHECKER.extract_rust_anchors(text, INFLIGHT_BASE, "x.rs")
        self.assertTrue(any("top level" in e for e in errors), errors)

    def test_r7_plain_nesting_without_cfg_also_fails(self) -> None:
        # The honest-mistake variant: a refactor tucks the anchor module inside
        # another module with no cfg at all — still rejected (top-level rule).
        text = (
            "mod parent {\n"
            "    #[cfg(test)]\n"
            "    mod relay_state_contract_refs {\n"
            "        #[test]\n"
            "        fn contract_symbols_exist() {\n"
            "            use super::super::validate_inflight_state_for_save as _;\n"
            "        }\n"
            "    }\n"
            "}\n"
        )
        _, errors = CHECKER.extract_rust_anchors(text, INFLIGHT_BASE, "x.rs")
        self.assertTrue(any("top level" in e for e in errors), errors)


class ExtractRustAnchorsTest(unittest.TestCase):
    def test_use_field_and_assoc_forms_parsed_from_code(self) -> None:
        body = """
        let _ = |s: &super::super::model::InflightTurnState| {
            let _ = &s.response_sent_offset;
        };
        let _ = super::super::model::InflightTurnState::effective_relay_owner_kind;
        use super::super::save_store::save_inflight_state as _;
        use super::validate_inflight_state_for_save as _;
        """
        anchors, errors = CHECKER.extract_rust_anchors(_block(body), INFLIGHT_BASE, "store.rs")
        self.assertEqual(errors, [])
        self.assertEqual(
            anchors,
            {
                "inflight::model::InflightTurnState::response_sent_offset",
                "inflight::model::InflightTurnState::effective_relay_owner_kind",
                "inflight::save_store::save_inflight_state",
                "inflight::store::validate_inflight_state_for_save",
            },
        )

    def test_defect1_commented_out_reference_drops_its_anchor(self) -> None:
        # r3 core defect: a `//`-commented reference must NOT be counted (the
        # compiler no longer checks it). The anchor vanishes with the code.
        body = """
        use super::super::save_store::save_inflight_state as _;
        // use super::validate_inflight_state_for_save as _;
        """
        anchors, errors = CHECKER.extract_rust_anchors(_block(body), INFLIGHT_BASE, "store.rs")
        self.assertEqual(errors, [])
        self.assertEqual(anchors, {"inflight::save_store::save_inflight_state"})

    def test_defect1_glob_use_is_not_an_anchor(self) -> None:
        # Replacing a real reference with `use super::*;` (which compiles) must
        # drop the anchor so the set comparison fails.
        body = "use super::*;\n"
        anchors, errors = CHECKER.extract_rust_anchors(_block(body), INFLIGHT_BASE, "store.rs")
        self.assertEqual(errors, [])
        self.assertEqual(anchors, set())

    def test_defect1_missing_cfg_is_rejected(self) -> None:
        body = "use super::validate_inflight_state_for_save as _;\n"
        # Build a block with no cfg attribute at all.
        text = (
            "mod relay_state_contract_refs {\n"
            "    #[test]\n"
            "    fn contract_symbols_exist() {\n"
            f"        {body}"
            "    }\n"
            "}\n"
        )
        anchors, errors = CHECKER.extract_rust_anchors(text, INFLIGHT_BASE, "store.rs")
        self.assertTrue(any("cfg(test)" in e for e in errors), errors)

    def test_missing_block_is_an_error(self) -> None:
        anchors, errors = CHECKER.extract_rust_anchors("fn unrelated() {}\n", INFLIGHT_BASE, "x.rs")
        self.assertTrue(any("no `mod relay_state_contract_refs`" in e for e in errors), errors)


class ReportTest(unittest.TestCase):
    def _report(self, doc: set[str], code: set[str], min_anchors: int = 3):
        return CHECKER.build_report(
            doc_anchors=doc, rust_anchors=code, min_anchors=min_anchors
        )

    def test_clean_when_sets_equal_and_above_floor(self) -> None:
        anchors = {"a::A", "b::B", "c::C"}
        report = self._report(anchors, set(anchors))
        self.assertTrue(report.is_clean(), CHECKER.format_report(report))

    def test_doc_anchor_without_code_reference_is_flagged(self) -> None:
        report = self._report({"a::A", "b::B", "c::C", "d::Extra"}, {"a::A", "b::B", "c::C"})
        self.assertFalse(report.is_clean())
        self.assertIn("d::Extra", report.missing_in_code)

    def test_code_reference_without_doc_anchor_is_flagged(self) -> None:
        report = self._report({"a::A", "b::B", "c::C"}, {"a::A", "b::B", "c::C", "z::Extra"})
        self.assertFalse(report.is_clean())
        self.assertIn("z::Extra", report.missing_in_doc)

    def test_synchronized_gutting_below_floor_is_flagged(self) -> None:
        report = self._report({"a::A", "b::B"}, {"a::A", "b::B"}, min_anchors=20)
        self.assertFalse(report.is_clean())
        self.assertTrue(report.below_floor)

    def test_missing_doc_file_is_flagged(self) -> None:
        report = CHECKER.build_report(
            doc=REPO_ROOT / "does-not-exist.md",
            rust_anchors={"a::A"},
            min_anchors=1,
        )
        self.assertFalse(report.is_clean())

    def test_missing_rust_source_is_flagged(self) -> None:
        report = CHECKER.build_report(
            doc_anchors={"a::A"},
            rust_sources={REPO_ROOT / "nope.rs": ("x",)},
            min_anchors=1,
        )
        self.assertFalse(report.is_clean())


class WiringTest(unittest.TestCase):
    def test_checker_invoked_by_ci_script_checks(self) -> None:
        script = (REPO_ROOT / "scripts" / "ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn('"$PYTHON" scripts/check_contract_symbol_refs.py', script)
        self.assertIn('"$PYTHON" -m unittest tests.test_contract_symbol_refs', script)

    def test_sync_gate_unconditional_in_ci_pr(self) -> None:
        workflow = (REPO_ROOT / ".github" / "workflows" / "ci-pr.yml").read_text(
            encoding="utf-8"
        )
        marker = "- name: Contract symbol-ref sync gate (always, #4268)"
        self.assertIn(marker, workflow)
        start = workflow.index(marker)
        nxt = workflow.find("\n      - name:", start + len(marker))
        step = workflow[start : nxt if nxt != -1 else len(workflow)]
        self.assertIn("scripts/check_contract_symbol_refs.py", step)
        self.assertNotIn("ci_relax_safe", step)
        self.assertNotIn("if:", step)

    def test_compile_existence_gate_is_a_required_ci_job(self) -> None:
        workflow = (REPO_ROOT / ".github" / "workflows" / "ci-pr.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("cargo check --workspace --all-targets", workflow)

    def test_required_gates_have_no_branch_name_escape_hatch(self) -> None:
        # A branch name must never bypass compile/test/lint/script gates. The
        # old tui-relay-stabilization exception allowed test code to merge
        # without ever being compiled (#4246).
        workflow = (REPO_ROOT / ".github" / "workflows" / "ci-pr.yml").read_text(
            encoding="utf-8"
        )
        self.assertFalse(
            "ci_relax_safe" in workflow,
            "ci-pr retains the retired CI relaxation output/condition",
        )
        self.assertFalse(
            "tui-relay-stabilization" in workflow,
            "ci-pr still grants a branch-name CI escape hatch",
        )

        # The CI required judge reads the forced lint run for Lint and Fast check.
        judge = yaml.safe_load(workflow)["jobs"]["required_contexts"]
        self.assertIn("lint", judge["needs"])
        sys.path.insert(0, str(REPO_ROOT / "scripts" / "ci"))
        import nextest_required
        sys.path.pop(0)
        self.assertEqual(judge["name"], nextest_required.GATE)
        self.assertLessEqual({"Lint", "Fast check (ubuntu-latest)"}, set(nextest_required.CONTEXTS))


class RelayContractPathFilterTest(unittest.TestCase):
    """A doc-only PR reaches the `lint` compile only via this output, so the filter
    key it reads must select exactly the checker's binding sources."""

    def setUp(self) -> None:
        workflow = REPO_ROOT / ".github" / "workflows" / "ci-pr.yml"
        changes = yaml.safe_load(workflow.read_text(encoding="utf-8"))["jobs"]["changes"]
        expr = changes["outputs"]["relay_contract"]
        ref = re.fullmatch(r"\$\{\{\s*steps\.([\w-]+)\.outputs\.([\w-]+)\s*\}\}", expr)
        self.assertIsNotNone(ref, f"relay_contract output is not a bare filter read: {expr}")
        step_id, self.key = ref.groups()
        steps = [s for s in changes["steps"] if s.get("id") == step_id]
        self.assertEqual(len(steps), 1, f"changes job has no single step `{step_id}`")
        self.assertTrue(steps[0]["uses"].startswith("dorny/paths-filter"))
        filters = yaml.safe_load(steps[0]["with"]["filters"])
        self.assertIn(self.key, filters, f"step `{step_id}` defines no `{self.key}` filter")
        self.filters = {name: tuple(patterns) for name, patterns in filters.items()}
        self.sources = sorted(
            path.relative_to(REPO_ROOT).as_posix()
            for path in (
                CHECKER.DEFAULT_DOC, SCRIPT_PATH, Path(__file__).resolve(),
                *CHECKER.DEFAULT_REFERENCE_SOURCES,
            )
        )

    def test_filter_selects_every_binding_source(self) -> None:
        for path in self.sources:
            with self.subTest(path=path):
                self.assertIn(self.key, select(self.filters, [path]))

    def test_filter_has_no_pattern_outside_the_binding_sources(self) -> None:
        stale = [
            q for q in self.filters[self.key]
            if not any(pattern_to_regex(q).match(p) for p in self.sources)
        ]
        self.assertEqual(stale, [], "update REFERENCE_SOURCE_MODULES or drop the entry")


class CliSeverityTest(unittest.TestCase):
    def test_equality_floor_and_form_drift_warns(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            doc, source = Path(tmp, "contract.md"), Path(tmp, "source.rs")
            doc.write_text("`sym:module::old`", encoding="utf-8")
            source.write_text(_block("    use super::new as _;", cfg="#[cfg(windows)]"), encoding="utf-8")
            report = CHECKER.build_report(doc=doc, rust_sources={source: ("module",)})
        with patch.object(CHECKER, "build_report", return_value=report), patch("sys.stdout") as stdout:
            self.assertEqual(CHECKER.main([]), 0)
        self.assertIn("warning", str(stdout.write.call_args_list))

    def test_missing_reference_source_still_fails(self) -> None:
        report = CHECKER.build_report(doc_anchors=set(), rust_sources={Path("/missing/source.rs"): ("module",)})
        with patch.object(CHECKER, "build_report", return_value=report), patch("sys.stdout"):
            self.assertEqual(CHECKER.main([]), 1)

    def test_scanner_exception_cannot_pass(self) -> None:
        with patch.object(CHECKER, "build_report", side_effect=RuntimeError("scanner failed")):
            with self.assertRaisesRegex(RuntimeError, "scanner failed"):
                CHECKER.main([])


class IntegrationTest(unittest.TestCase):
    def test_repo_doc_and_code_anchors_in_sync(self) -> None:
        report = CHECKER.build_report()
        self.assertTrue(report.is_clean(), CHECKER.format_report(report))
        self.assertGreaterEqual(len(report.doc_anchors), CHECKER.MIN_CONTRACT_ANCHORS)
        self.assertEqual(report.doc_anchors, report.rust_anchors)

    def test_every_reference_source_declares_anchors(self) -> None:
        for rel, base in CHECKER.REFERENCE_SOURCE_MODULES.items():
            source = CHECKER.DISCORD_ROOT / rel
            anchors, errors = CHECKER.extract_rust_anchors(
                source.read_text(encoding="utf-8"), base, rel
            )
            self.assertEqual(errors, [], f"{rel}: {errors}")
            self.assertTrue(anchors, f"no anchors parsed from {rel}")


class ForwardPromiseTests(unittest.TestCase):
    def test_rejects_issue_and_same_change_update_promises(self) -> None:
        for text in (
            "#6012 updates this bullet in the same PR.",
            "A lane owns correcting this bullet in the same change.",
            "This section will be updated by #6012 in the same PR.",
            "This bullet must be corrected in the same PR.",
            "A lane will change this bullet in the same PR.",
            "This bullet will be amended in the same PR.",
            "The release may be delayed, so #6012 will update this bullet in the same PR.",
            "The project could slip, therefore #6012 will update this bullet in the same PR.",
            "The deadline may slip, so this bullet will be updated in the same PR.",
            "The review could overrun, which means this section will be revised in the same PR.",
            "#6012 edits this bullet in the same PR.",
            "This section is scheduled to be revised in the same PR.",
            "A lane is responsible for updating this bullet in the same change.",
            "The follow-up PR plans to update this bullet in the same PR.",
            "This section is planned to be revised in the same PR.",
            "The lane is committed to correcting this paragraph in the same change.",
            "#6012 will update the contract and correct this bullet in the same PR.",
            "#6012 will update the contract and review the test and correct this bullet in the same PR.",
            "#6012 will update the contract but also correct this bullet in the same PR.",
            "A lane will avoid editing another section and update this bullet in the same PR.",
            "Given no lane owns this section, #6012 will update this bullet in the same PR.",
            "With no lane assigned to this paragraph, #6012 will rewrite this paragraph in the same PR.",
        ):
            with self.subTest(text=text):
                self.assertTrue(CHECKER.forward_promise_errors(text))

    def test_follow_up_prediction_and_recheck_guidance_are_allowed(self) -> None:
        text = (
            "After #6012 lands, recheck this bullet before relying on it. "
            "A later change may update the contract. The prior PR #6012 updated "
            "this bullet in the preceding release. This bullet may be updated "
            "in the same PR. It is possible that this bullet will be updated in the same PR. "
            "This bullet will likely be updated in the same PR. "
            "The old paragraph said this section would be revised "
            "in the same change. No lane will update this bullet in the same PR. "
            "This section must not be corrected in the same change. The prior PR "
            "#6012 updated this bullet in the same PR. This paragraph is not "
            "scheduled to be changed in the same PR. This paragraph is not "
            "expected to be updated in the same PR. A lane is responsible for "
            "not changing this bullet in the same PR. A lane is responsible for "
            "ensuring this section is not changed in the same PR. A lane is "
            "responsible for ensuring the same PR does not change this bullet. "
            "A lane is responsible for ensuring this bullet won't be updated "
            "in the same PR. A lane is responsible for ensuring this bullet "
            "does not need updating in the same PR. A lane is responsible for "
            "ensuring it does not have to update this bullet in the same PR."
            " A lane will avoid updating this bullet in the same PR."
            " A lane will refrain from changing this bullet in the same PR."
            " No lane will update this paragraph and change this bullet in the same PR."
            " No lane will update the document and change this bullet in the same PR."
            " A lane must not edit anything and change this bullet in the same PR."
            " No lane is expected to update the document and change this bullet in the same PR."
            " No lane is scheduled to update the document and change this bullet in the same PR."
            " A lane is not expected to update the document and change this bullet in the same PR."
            " No lane is required to update the document and correct this bullet in the same PR."
            " No lane has to update the document and correct this bullet in the same PR."
            " No lane is planned to update the document and correct this bullet in the same PR."
        )
        self.assertEqual(CHECKER.forward_promise_errors(text), [])

    def test_unrelated_prohibition_does_not_hide_promise_in_next_clause(self) -> None:
        for text in (
            "A lane must not cite the old fact; #6012 will update this bullet in the same PR.",
            "No lane should cite this as fixed, or #6012 will update this bullet in the same PR.",
            "No lane should cite this as fixed, while #6012 will update this bullet in the same PR.",
        ):
            with self.subTest(text=text):
                self.assertTrue(CHECKER.forward_promise_errors(text))

    def test_repo_contract_has_no_forward_update_promise(self) -> None:
        self.assertEqual(CHECKER.check_forward_promises(), [])

    def test_checker_cli_fails_on_forward_promise_mutation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            doc = Path(tmp, "relay-state-contract.md")
            doc.write_text(
                CHECKER.DEFAULT_DOC.read_text(encoding="utf-8")
                + "\n\n#6012 updates this bullet in the same PR.\n",
                encoding="utf-8",
            )
            proc = subprocess.run(
                [sys.executable, str(SCRIPT_PATH), "--doc", str(doc)],
                capture_output=True,
                text=True,
            )
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("forward-promise check:", proc.stdout)


if __name__ == "__main__":
    unittest.main()
