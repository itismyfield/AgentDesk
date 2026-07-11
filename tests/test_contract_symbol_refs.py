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
import sys
import textwrap
import unittest
from pathlib import Path

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

    def test_crate_prefix_stripped(self) -> None:
        self.assertEqual(
            CHECKER._resolve_symbol(
                "crate::services::discord::tmux::advance_watcher_confirmed_end", INFLIGHT_BASE
            ),
            "tmux::advance_watcher_confirmed_end",
        )

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
        # required PR compile (check_fast) is ubuntu-latest.
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
        macos_workflow = (
            REPO_ROOT / ".github" / "workflows" / "ci-macos-trusted.yml"
        ).read_text(encoding="utf-8")
        for name, source in (("ci-pr", workflow), ("ci-macos-trusted", macos_workflow)):
            self.assertFalse(
                "ci_relax_safe" in source,
                f"{name} retains the retired CI relaxation output/condition",
            )
            self.assertFalse(
                "tui-relay-stabilization" in source,
                f"{name} still grants a branch-name CI escape hatch",
            )

        # A relay_contract path filter and output exist.
        self.assertIn("relay_contract:", workflow)
        self.assertIn(
            "relay_contract: ${{ steps.filter.outputs.relay_contract }}", workflow
        )
        # check_fast also runs for a doc-only relay-contract binding change.
        self.assertIn(
            "|| needs.changes.outputs.relay_contract == 'true'", workflow
        )
        # The anchor host files are covered by the filter.
        for host in (
            "src/services/discord/inflight/store.rs",
            "src/services/discord/turn_bridge/terminal_delivery.rs",
            "src/services/discord/tmux_watcher/liveness.rs",
            "src/services/discord/router/message_handler/watchdog.rs",
            "docs/relay-state-contract.md",
        ):
            self.assertIn(host, workflow)
        # The required-context mirror gates the forced run.
        self.assertIn("Relay-contract fast check mirror (always, #4268)", workflow)


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


if __name__ == "__main__":
    unittest.main()
