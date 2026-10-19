"""Admission-level regressions for cfg(test) tokens carried by production Rust."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from tests import test_h2_admission as fixtures

adm = fixtures.adm
OWNER = fixtures.OWNER
OWNER_MODULE = "agentdesk::services::platform::tmux"


class CfgCarrierRejections(fixtures.Tree):
    def add_owner_source(self, source: str) -> None:
        self.edit(OWNER, "pub(crate) fn read", source + "\npub(crate) fn read")

    def assert_ext_macro_rejected(self, source: str) -> None:
        self.add_owner_source(source)
        self.assertIn(
            f"R-E: {OWNER} uses item-level macro `ext!`; owner files must be plain items",
            self.evaluate(),
        )

    def test_ce2a_doc_rhs_then_macro_invocation_is_rejected(self) -> None:
        self.assert_ext_macro_rejected('#[doc = stringify!(#[cfg(test)])]\next!{}')

    def test_ce2b_doc_rhs_then_inline_module_macro_is_rejected(self) -> None:
        self.assert_ext_macro_rejected(
            '#[doc = stringify!(#[cfg(test)])] pub mod m {\n    ext!{}\n}'
        )

    def test_ce2c_cfg_attr_doc_rhs_then_macro_is_rejected(self) -> None:
        self.assert_ext_macro_rejected(
            '#[cfg_attr(all(), doc = concat!(stringify!(#[cfg(test)])))]\next!{}'
        )

    def test_ce2d_doc_rhs_then_plain_owner_api_is_rejected(self) -> None:
        self.add_owner_source('#[doc = stringify!(#[cfg(test)])]\npub fn hidden_exec() {}')
        self.assertIn(
            f"R-E: owner pub fn inventory mismatch: {OWNER_MODULE}::hidden_exec (unclassified)",
            self.evaluate(),
        )

    def test_function_body_cfg_token_then_macro_is_rejected(self) -> None:
        self.assert_ext_macro_rejected(
            'pub fn f() { let _ = stringify!(#[cfg(test)]); }\next!{}'
        )

    def test_multiline_ce2a_doc_rhs_then_macro_is_rejected(self) -> None:
        self.assert_ext_macro_rejected('#[doc = stringify!(\n    #[cfg(test)]\n)] ext!{}')

    def test_multiline_ce2b_doc_rhs_then_inline_module_is_rejected(self) -> None:
        self.assert_ext_macro_rejected(
            '#[doc = stringify!(\n    #[cfg(test)]\n)] pub mod m { ext!{} }'
        )

    def test_multiline_ce2c_cfg_attr_doc_rhs_then_macro_is_rejected(self) -> None:
        self.assert_ext_macro_rejected(
            '#[cfg_attr(all(), doc = concat!(stringify!(\n    #[cfg(test)]\n)))] ext!{}'
        )

    def test_multiline_ce2d_doc_rhs_then_plain_api_is_rejected(self) -> None:
        self.add_owner_source('#[doc = stringify!(\n    #[cfg(test)]\n)] pub fn hidden_exec() {}')
        self.assertIn(
            f"R-E: owner pub fn inventory mismatch: {OWNER_MODULE}::hidden_exec (unclassified)",
            self.evaluate(),
        )

    def test_multiline_function_body_token_then_macro_is_rejected(self) -> None:
        self.assert_ext_macro_rejected(
            'pub fn f() {\n    let _ = stringify!(#[cfg(test)]\n); } ext!{}'
        )

    def assert_mixed_line_rejected(self, source: str) -> None:
        self.add_owner_source(source)
        self.assertIn(
            f"R-E: owner pub fn inventory mismatch: {OWNER_MODULE}::production_api (unclassified)",
            self.evaluate(),
        )

    def test_test_only_item_before_same_line_production_api_is_rejected(self) -> None:
        self.assert_mixed_line_rejected(
            '#[cfg(test)] fn helper() {} pub fn production_api() {}'
        )

    def test_production_api_before_same_line_test_only_item_is_rejected(self) -> None:
        self.assert_mixed_line_rejected(
            'pub fn production_api() {} #[cfg(test)] fn helper() {}'
        )

    def test_real_cfg_test_module_still_masks_owner_api_and_macro(self) -> None:
        self.edit(OWNER, 'pub fn not_an_owner_api() {}', 'pub fn hidden_exec() {}\next!{}')
        self.assertEqual(self.evaluate(), [])
        self.assertNotIn(f"{OWNER_MODULE}::tests::hidden_exec", adm.owner_pub_fns(self.root, OWNER, OWNER_MODULE))


class NestedCfgTokenViews(unittest.TestCase):
    def test_doc_and_macro_cfg_tokens_do_not_hide_owner_api(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            path = root / OWNER
            path.parent.mkdir(parents=True)
            path.write_text(
                "#[doc = stringify!(\n"
                "    #[cfg(test)]\n"
                ")]\n"
                "pub fn after_doc_rhs() {}\n"
                "pub fn carries_macro_token() {\n"
                "    stringify!(\n"
                "        #[cfg(test)]\n"
                "    );\n"
                "}\n"
                "#[cfg(test)]\n"
                "pub fn test_only_api() {}\n"
                "#[cfg(test)] pub fn inline_test_only_api() {}\n"
                "pub fn after_test_items() {}\n",
                encoding="utf-8",
            )
            owner_api = adm.owner_pub_fns(root, OWNER, "agentdesk::services::platform::tmux")
            path.write_text(
                "#![allow(dead_code)]\n#![cfg(test)]\npub fn crate_test_only_api() {}\n",
                encoding="utf-8",
            )
            inner_cfg_api = adm.owner_pub_fns(root, OWNER, "agentdesk::services::platform::tmux")
        self.assertIn("agentdesk::services::platform::tmux::after_doc_rhs", owner_api)
        self.assertIn("agentdesk::services::platform::tmux::carries_macro_token", owner_api)
        self.assertIn("agentdesk::services::platform::tmux::after_test_items", owner_api)
        self.assertNotIn("agentdesk::services::platform::tmux::test_only_api", owner_api)
        self.assertNotIn("agentdesk::services::platform::tmux::inline_test_only_api", owner_api)
        self.assertEqual(inner_cfg_api, set())


if __name__ == "__main__":
    unittest.main()
