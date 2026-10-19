"""Keep cfg-like carrier tokens visible to the durable writer classifier."""

import unittest

from tests import test_durable_frontier_writer_call_sites as fixture


class CfgCarrierTests(unittest.TestCase):
    classify = fixture.CfgTestNestingTests.classify

    def test_cfg_tokens_inside_doc_rhs_and_macro_inputs_do_not_hide_owner_items(self):
        lines = self.classify(
            "#[doc = stringify!(\n"
            "    #[cfg(test)]\n"
            ")]\n"
            "pub fn after_doc_rhs() {}\n"
            'pub fn carries_literal() { let _ = "#[cfg(test)]"; }\n'
            "pub fn carries_macro_token() {\n"
            "    stringify!(\n"
            "        #[cfg(test)]\n"
            "    );\n"
            "}\n"
            "macro_rules! make_test_item { () => { #[cfg(test)] fn generated() {} } }\n"
            "pub fn after_macro_definition() {}\n"
            "#[cfg(test)]\n"
            "mod tests { fn hidden() {} }\n"
            "pub fn after_real_cfg_item() {}\n"
        )
        self.assertTrue(lines["pub fn after_doc_rhs() {}"])
        self.assertTrue(lines["pub fn carries_literal() { let _ =               ; }"])
        self.assertTrue(lines["pub fn carries_macro_token() {"])
        self.assertTrue(lines["stringify!("])
        self.assertTrue(
            lines["macro_rules! make_test_item { () => { #[cfg(test)] fn generated() {} } }"]
        )
        self.assertTrue(lines["pub fn after_macro_definition() {}"])
        self.assertTrue(lines["pub fn after_real_cfg_item() {}"])
        self.assertFalse(lines["mod tests { fn hidden() {} }"])

    def assert_macro_carrier_is_production(self, opener: str, closer: str, name: str):
        terminator = ";" if closer != "}" else ""
        lines = self.classify(
            f"macro_rules! {name} {opener}\n"
            "    () => {\n"
            "        #[cfg(test)]\n"
            "        pub fn generated() {}\n"
            "    }\n"
            f"{closer}{terminator}\n"
            "pub fn after_macro_definition() {}\n"
            "#[cfg(test)]\n"
            "mod tests { fn hidden() {} }\n"
        )
        self.assertTrue(lines["pub fn generated() {}"])
        self.assertTrue(lines["pub fn after_macro_definition() {}"])
        self.assertFalse(lines["mod tests { fn hidden() {} }"])

    def test_named_macro_parenthesized_body_tokens_remain_production(self):
        self.assert_macro_carrier_is_production("(", ")", "carrier")

    def test_named_macro_bracketed_body_tokens_remain_production(self):
        self.assert_macro_carrier_is_production("[", "]", "carrier")

    def test_raw_named_macro_body_tokens_remain_production(self):
        for opener, closer in (("(", ")"), ("[", "]"), ("{", "}")):
            with self.subTest(delimiter=opener):
                self.assert_macro_carrier_is_production(opener, closer, "r#carrier")

    def test_raw_macro_invocation_tokens_remain_production(self):
        for opener, closer in (("(", ")"), ("[", "]"), ("{", "}")):
            with self.subTest(delimiter=opener):
                terminator = ";" if closer != "}" else ""
                lines = self.classify(
                    f"r#carrier!{opener}\n"
                    "    #[cfg(test)]\n"
                    "    pub fn generated() {}\n"
                    f"{closer}{terminator}\n"
                    "pub fn after_macro_invocation() {}\n"
                )
                self.assertTrue(lines["pub fn generated() {}"])
                self.assertTrue(lines["pub fn after_macro_invocation() {}"])


if __name__ == "__main__":
    unittest.main()
