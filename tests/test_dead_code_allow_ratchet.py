import json
import unittest
from scripts.check_dead_code_allow_ratchet import BASELINE, collect, count, problems


class DeadCodeRatchetTest(unittest.TestCase):
    def test_clean_repository(self):
        self.assertEqual(problems(collect(), json.loads(BASELINE.read_text())), [])

    def test_new_and_relocated_suppressions_fail(self):
        self.assertTrue(problems({"src/new.rs": 1}, {}))
        self.assertTrue(problems({"src/a.rs": 2}, {"src/a.rs": 1}))
        self.assertTrue(problems({"src/b.rs": 1}, {"src/a.rs": 1}))
        self.assertEqual(problems({}, {"src/a.rs": 1}), [])

    def test_groups_expect_and_cfg_attr_count(self):
        for attribute in ("#[allow(dead_code)]", "#![allow(unused)]", "#[expect(warnings)]", "#[cfg_attr(test, allow(dead_code))]", "#[allow(dead_code, unused)]"):
            self.assertEqual(count(attribute + " fn f() {}"), 1)

    def test_comments_and_strings_do_not_count(self):
        self.assertEqual(count('// #[allow(dead_code)]\nconst S: &str = "#[allow(unused)]";'), 0)
        self.assertEqual(count('#[allow(clippy::dead_code)] fn f() {}'), 0)

    def test_ambiguous_and_bad_baselines_fail_closed(self):
        with self.assertRaises(ValueError):
            count("#[allow(dead_code)")
        for baseline in ([], {"a": True}, {"a": -1}):
            with self.assertRaises(ValueError):
                problems({}, baseline)
