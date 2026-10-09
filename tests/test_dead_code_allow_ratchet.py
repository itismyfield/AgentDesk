import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
from scripts import check_dead_code_allow_ratchet as ratchet
from scripts.check_dead_code_allow_ratchet import BASELINE, collect, count, problems


class DeadCodeRatchetTest(unittest.TestCase):
    def test_clean_repository(self):
        self.assertEqual(problems(collect(), json.loads(BASELINE.read_text())), [])

    def test_new_and_relocated_suppressions_fail(self):
        self.assertTrue(problems({"src/new.rs": 1}, {}))
        self.assertTrue(problems({"src/a.rs": 2}, {"src/a.rs": 1}))
        self.assertTrue(problems({"src/b.rs": 1}, {"src/a.rs": 1}))
        self.assertTrue(problems({}, {"src/a.rs": 1}))

    def test_stale_allocation_must_be_lowered(self):
        self.assertTrue(problems({"src/a.rs": 1}, {"src/a.rs": 2}))
        self.assertEqual(problems({"src/a.rs": 1}, {"src/a.rs": 1}), [])

    def test_inner_scope_expansion_is_distinct_from_total(self):
        outer = "#[allow(dead_code)] fn f() {}"
        inner = "#![allow(dead_code)] fn f() {}"
        self.assertEqual(count(outer), count(inner))
        self.assertEqual(count(outer, inner_only=True), 0)
        self.assertEqual(count(inner, inner_only=True), 1)
        self.assertEqual(count('#![cfg_attr(test, expect(unused))]', inner_only=True), 1)
        self.assertEqual(count('// #![allow(dead_code)]', inner_only=True), 0)
        self.assertTrue(problems({"src/a.rs": 1}, {}))

    def test_inner_baseline_matches_current_source(self):
        self.assertEqual(problems(collect(inner_only=True), json.loads(ratchet.INNER_BASELINE.read_text())), [])

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


class DeadCodeAdmissionTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.git("init", "--quiet")
        self.git("config", "user.email", "test@example.com")
        self.git("config", "user.name", "Test")
        (self.root / "src").mkdir()
        (self.root / "scripts").mkdir()
        (self.root / "src/a.rs").write_text('#![allow(dead_code)]\n#[allow(dead_code)] fn f() {}\n')
        self.write_baselines({"src/a.rs": 2})
        self.commit()
        self.base = self.git("rev-parse", "HEAD").strip()

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.root), *args], text=True)

    def write_baselines(self, totals, inner=None):
        (self.root / "scripts/dead_code_allow_occurrences.json").write_text(json.dumps(totals))
        if inner is not None:
            (self.root / "scripts/dead_code_inner_allow_occurrences.json").write_text(json.dumps(inner))

    def commit(self):
        self.git("add", ".")
        self.git("commit", "--quiet", "-m", "fixture")

    def test_first_inner_baseline_is_admitted_from_base_source(self):
        self.assertEqual(ratchet.admission_problems(self.root, self.base, {"src/a.rs": 2}, {"src/a.rs": 1}), [])
        self.assertTrue(ratchet.admission_problems(self.root, self.base, {"src/a.rs": 2}, {"src/a.rs": 2}))
        self.assertTrue(ratchet.admission_problems(self.root, self.base, {"src/a.rs": 3}, {"src/a.rs": 1}))

    def test_existing_baselines_cannot_be_increased(self):
        self.write_baselines({"src/a.rs": 2}, {"src/a.rs": 1})
        self.commit()
        base = self.git("rev-parse", "HEAD").strip()
        self.assertTrue(ratchet.admission_problems(self.root, base, {"src/a.rs": 3}, {"src/a.rs": 1}))
        self.assertTrue(ratchet.admission_problems(self.root, base, {"src/a.rs": 2}, {"src/a.rs": 2}))
        self.assertEqual(ratchet.admission_problems(self.root, base, {"src/a.rs": 1}, {}), [])

    def test_scope_growth_stale_and_inflation_fail_through_main(self):
        self.write_baselines({"src/a.rs": 2}, {"src/a.rs": 1})
        source = self.root / "src/a.rs"
        source.write_text('#![allow(dead_code)]\n#![allow(dead_code)] fn f() {}\n')
        self.assertEqual(ratchet.main(["--base-ref", self.base], self.root), 1)
        self.write_baselines({"src/a.rs": 2}, {"src/a.rs": 2})
        self.assertEqual(ratchet.main(["--base-ref", self.base], self.root), 1)
        source.write_text('#[allow(dead_code)] fn f() {}\n')
        self.assertEqual(ratchet.main(["--base-ref", self.base], self.root), 1)
        self.write_baselines({"src/a.rs": 1}, {})
        self.assertEqual(ratchet.main(["--base-ref", self.base], self.root), 0)

    def test_missing_or_malformed_current_inner_baseline_fails(self):
        self.assertEqual(ratchet.main(["--base-ref", self.base], self.root), 1)
        self.write_baselines({"src/a.rs": 2}, {"src/a.rs": True})
        self.assertEqual(ratchet.main(["--base-ref", self.base], self.root), 1)

    def test_initial_source_transport_preserves_non_ascii_paths(self):
        path = self.root / "src/한글.rs"
        path.write_text('#![cfg_attr(test,\nallow(unused))]\nfn f() {}\n')
        self.commit()
        self.assertEqual(ratchet.collect_at(self.root, "HEAD"), {"src/a.rs": 1, "src/한글.rs": 1})

    def test_invalid_base_fails_closed_and_env_default_is_used(self):
        self.write_baselines({"src/a.rs": 2}, {"src/a.rs": 1})
        with patch.dict(os.environ, {"TEST_LANE_BASELINE_REF": self.base}):
            self.assertEqual(ratchet.main([], self.root), 0)
        with patch.dict(os.environ, {"TEST_LANE_BASELINE_REF": "missing-ref"}):
            self.assertEqual(ratchet.main([], self.root), 1)
            self.assertEqual(ratchet.main(["--base-ref", self.base], self.root), 0)

    def test_malformed_base_baseline_and_ambiguous_initial_source_fail(self):
        (self.root / "scripts/dead_code_allow_occurrences.json").write_text("[]")
        self.commit()
        with self.assertRaises(ValueError):
            ratchet.admission_problems(self.root, "HEAD", {"src/a.rs": 2}, {"src/a.rs": 1})
        self.write_baselines({"src/a.rs": 2})
        (self.root / "src/a.rs").write_text("#![allow(dead_code)")
        self.commit()
        with self.assertRaises(ValueError):
            ratchet.admission_problems(self.root, "HEAD", {"src/a.rs": 2}, {"src/a.rs": 1})
