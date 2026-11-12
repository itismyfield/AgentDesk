"""Exercise list policy through real union merges and the required PR job."""

from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[1]
GUARD = ROOT / "scripts/ci/list_merge_guard.py"
EXPECTED = "docs/statecharts/expected.tsv"
REASONS = "scripts/ci/test-removal-justifications.txt"
FILES = (EXPECTED, REASONS)
SEED = {
    EXPECTED: "# configs\na/A.cfg\tA\t-\t-\nz/Z.cfg\tZ\t-\t-\n",
    REASONS: "# reasons\nagentdesk a::test | removed | original\nagentdesk z::test | removed | retained\n",
}


class MergeRepo:
    def __init__(self, root: Path):
        self.root = root
        self.git("init", "-q", "-b", "ours")
        self.git("config", "user.email", "fixture@example.invalid")
        self.git("config", "user.name", "List merge fixture")
        self.git("config", "commit.gpgsign", "false")
        self.write(".gitattributes", "".join(f"{p} merge=union\n" for p in FILES))
        for path, text in SEED.items():
            self.write(path, text)
        self.base = self.commit("base")

    def git(self, *args, check=True):
        result = subprocess.run(["git", "-C", str(self.root), *args], text=True,
                                capture_output=True)
        if check and result.returncode:
            raise AssertionError(f"git {args}: {result.stdout}{result.stderr}")
        return result

    def write(self, path, text):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")

    def commit(self, message):
        self.git("add", ".")
        self.git("commit", "-qm", message, "--allow-empty")
        return self.git("rev-parse", "HEAD").stdout.strip()

    def merge(self, ours=None, theirs=None):
        for path, text in (ours or {}).items():
            self.write(path, text)
        self.ours = self.commit("ours")
        self.git("checkout", "-qb", "theirs", self.base)
        for path, text in (theirs or {}).items():
            self.write(path, text)
        self.theirs = self.commit("theirs")
        self.git("checkout", "-q", "ours")
        self.merged = self.git("merge", "--no-ff", "--no-edit", "theirs", check=False)

    def guard(self, *args):
        return subprocess.run([sys.executable, str(GUARD), self.base, self.ours, self.theirs,
                               "--repo", str(self.root), *args], capture_output=True, text=True)


class UnionMergeGuardTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="list-merge-")
        self.addCleanup(self.tmp.cleanup)
        self.repo = MergeRepo(Path(self.tmp.name))

    def assert_rejected(self, message):
        result = self.repo.guard()
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(message, result.stdout + result.stderr)

    def test_delete_modify_resurrection(self):
        self.repo.merge({REASONS: SEED[REASONS].replace("agentdesk a::test | removed | original\n", "")},
                        {REASONS: SEED[REASONS].replace("original", "changed")})
        self.assertEqual(self.repo.merged.returncode, 0)
        self.assertIn("changed", (self.repo.root / REASONS).read_text())
        self.assert_rejected("delete/modify")

    def test_modify_modify(self):
        self.repo.merge({REASONS: SEED[REASONS].replace("original", "ours")},
                        {REASONS: SEED[REASONS].replace("original", "theirs")})
        self.assertEqual(self.repo.merged.returncode, 0)
        self.assert_rejected("listed twice")

    def test_missing_row(self):
        self.repo.merge(theirs={EXPECTED: SEED[EXPECTED] + "zz/New.cfg\tNew\t-\t-\n"})
        self.assertEqual(self.repo.merged.returncode, 0)
        self.repo.write(EXPECTED, SEED[EXPECTED])
        self.assert_rejected("missing keys")

    def test_duplicate_cfg(self):
        self.repo.merge({EXPECTED: SEED[EXPECTED] + "zz/New.cfg\tNew\tFirst\t-\n"},
                        {EXPECTED: SEED[EXPECTED] + "zz/New.cfg\tNew\tSecond\t-\n"})
        self.assertEqual(self.repo.merged.returncode, 0)
        self.assert_rejected("listed twice")

    def test_overlapping_scope(self):
        row = "agentdesk zz::test | removed | "
        self.repo.merge({REASONS: SEED[REASONS] + row + "all platforms\n"},
                        {REASONS: SEED[REASONS] + row + "[platform=windows] one platform\n"})
        self.assertEqual(self.repo.merged.returncode, 0)
        self.assert_rejected("listed twice")

    def test_resolved_modify_modify_still_rejected(self):
        self.repo.merge({REASONS: SEED[REASONS].replace("original", "ours")},
                        {REASONS: SEED[REASONS].replace("original", "theirs")})
        self.repo.write(REASONS, SEED[REASONS].replace("original", "ours"))
        self.assert_rejected("different edits")

    def test_concurrent_different_additions_still_rejected_after_resolution(self):
        prefix = SEED[REASONS] + "agentdesk zz::test | removed | "
        self.repo.merge({REASONS: prefix + "ours\n"}, {REASONS: prefix + "theirs\n"})
        self.repo.write(REASONS, prefix + "ours\n")
        self.assert_rejected("different edits")

    def test_unilateral_delete_and_modify_are_preserved(self):
        self.repo.merge({REASONS: SEED[REASONS].replace("agentdesk a::test | removed | original\n", "")},
                        {EXPECTED: SEED[EXPECTED].replace("A\t-\t-", "A\tBroken\t-")})
        result = self.repo.guard()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.repo.write(REASONS, SEED[REASONS])
        self.assert_rejected("unexpected keys")

    def test_deleted_key_cannot_be_restored_in_result(self):
        self.repo.merge({REASONS: SEED[REASONS].replace("agentdesk a::test | removed | original\n", "")})
        self.assertEqual(self.repo.merged.returncode, 0)
        self.repo.write(REASONS, SEED[REASONS])
        self.assert_rejected("unexpected keys")

    def test_identical_edits_merge_once(self):
        change = {REASONS: SEED[REASONS].replace("original", "same")}
        self.repo.merge(change, change)
        result = self.repo.guard()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_different_explicit_scopes_are_allowed(self):
        prefix = SEED[REASONS] + "agentdesk zz::test | removed | "
        self.repo.merge({REASONS: prefix + "[platform=linux] linux\n"},
                        {REASONS: prefix + "[platform=windows] windows\n"})
        result = self.repo.guard("--normalize")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_empty_and_malformed_result_fail(self):
        self.repo.merge()
        for content, message in (("# header only\n", "empty"), ("not a row\n", "tab-separated")):
            with self.subTest(content=content):
                self.repo.write(EXPECTED, content)
                self.assert_rejected(message)

    def test_unsorted_historical_inputs_normalize_without_changing_rows(self):
        old = {path: "\n".join(text.splitlines()[::-1]) + "\n" for path, text in SEED.items()}
        self.repo.merge(old)
        self.assert_rejected("not sorted")
        result = self.repo.guard("--normalize")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        for path in FILES:
            self.assertEqual((self.repo.root / path).read_text(), SEED[path])

    def test_normalization_does_not_hide_duplicates_or_drop_comments(self):
        self.repo.merge()
        before = SEED[EXPECTED] + "a/A.cfg\tA\t-\t-\n"
        self.repo.write(EXPECTED, before)
        result = self.repo.guard("--normalize")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("listed twice", result.stderr)
        self.assertEqual((self.repo.root / EXPECTED).read_text(), before)

    def test_unknown_ref_and_wrong_merge_base_fail(self):
        self.repo.merge(theirs={REASONS: SEED[REASONS].replace("original", "changed")})
        self.repo.base = "missing-ref"
        self.assert_rejected("git")
        self.repo.base = self.repo.theirs
        self.assert_rejected("merge-base")

    def test_squashed_branch_uses_actual_common_ancestor(self):
        self.repo.merge(theirs={EXPECTED: SEED[EXPECTED] + "zz/New.cfg\tNew\t-\t-\n"})
        self.repo.git("checkout", "-qB", "squashed", self.repo.base)
        self.repo.git("merge", "--squash", "theirs")
        self.repo.ours = self.repo.commit("squash landing")
        result = self.repo.guard()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_independent_additions_normalize_identically_in_both_directions(self):
        changes = []
        for letter in ("b", "c"):
            changes.append({EXPECTED: SEED[EXPECTED] + f"{letter}/New.cfg\tNew\t-\t-\n",
                            REASONS: SEED[REASONS] + f"agentdesk {letter}::test | removed | {letter}\n"})
        self.repo.merge(*changes)
        self.assertEqual(self.repo.merged.returncode, 0)
        normalized = self.repo.guard("--normalize")
        self.assertEqual(normalized.returncode, 0, normalized.stdout + normalized.stderr)
        forward = {path: (self.repo.root / path).read_bytes() for path in FILES}
        self.repo.git("checkout", "-q", "--", *FILES)
        self.repo.git("checkout", "-q", "theirs")
        reverse = self.repo.git("merge", "--no-ff", "--no-edit", self.repo.ours)
        self.assertEqual(reverse.returncode, 0)
        self.repo.ours, self.repo.theirs = self.repo.theirs, self.repo.ours
        normalized = self.repo.guard("--normalize")
        self.assertEqual(normalized.returncode, 0, normalized.stdout + normalized.stderr)
        self.assertEqual(forward, {path: (self.repo.root / path).read_bytes() for path in FILES})


class RequiredListGuardWiringTest(unittest.TestCase):
    def test_union_is_checked_by_required_job_on_all_paths(self):
        attributes = subprocess.check_output(["git", "check-attr", "merge", "--", *FILES],
                                             cwd=ROOT, text=True).splitlines()
        self.assertEqual(attributes, [f"{path}: merge: union" for path in FILES])
        jobs = yaml.safe_load((ROOT / ".github/workflows/ci-pr.yml").read_text())["jobs"]
        job = jobs["scripts_contracts"]
        self.assertIn("scripts_contracts", jobs["required_contexts"]["needs"])
        self.assertEqual(job["if"], "${{ !github.event.pull_request.draft && !cancelled() }}")
        checkout = next(step for step in job["steps"] if step.get("uses", "").startswith("actions/checkout@"))
        self.assertEqual(checkout["with"]["fetch-depth"], 0)
        step = next(step for step in job["steps"] if step.get("run") == "./scripts/ci-script-checks.sh")
        self.assertNotIn("if", step)
        self.assertFalse(step.get("continue-on-error", False))
        self.assertEqual(step["env"]["GFP_BASE_SHA"], "${{ github.event.pull_request.base.sha }}")
        self.assertEqual(step["env"]["GFP_HEAD_SHA"], "${{ github.event.pull_request.head.sha }}")
        self.assertEqual(step["env"]["GFP_EVENT_NAME"], "${{ github.event_name }}")
        self.assertEqual(step["env"]["SCRIPT_CHECK_SHARD"], "contracts")
        title = "Union list merge guard and required CI contract"
        script = (ROOT / "scripts/ci-script-checks.sh").read_text()
        registration = f'if run_check contracts "{title}"; then\n'
        body = script.split(registration, 1)[1].split('"$PYTHON" -m unittest tests.test_list_merge_guard\n', 1)[0]
        from scripts.ci.script_check_inputs import skipped_checks
        for paths in ('["README.md"]', '["docs/statecharts/expected.tsv"]', '[".gitattributes"]'):
            self.assertNotIn(title, skipped_checks({"GITHUB_EVENT_NAME": "pull_request",
                              "SCRIPT_CHECK_CHANGES_RESULT": "success", "SCRIPT_CHECK_CHANGED_FILES": paths}))
        with tempfile.TemporaryDirectory(prefix="list-wiring-") as directory:
            repo = MergeRepo(Path(directory))
            repo.merge(theirs={EXPECTED: SEED[EXPECTED] + "zz/New.cfg\tNew\t-\t-\n"})
            scripts = repo.root / "scripts/ci"
            scripts.mkdir(exist_ok=True, parents=True)
            for name in ("list_merge_guard.py", "statecharts_tlc.py", "nextest_ci.py", "nextest_required.py"):
                shutil.copyfile(ROOT / "scripts/ci" / name, scripts / name)
            env = {**os.environ, "GFP_EVENT_NAME": "pull_request", "GFP_BASE_SHA": repo.ours,
                   "GFP_HEAD_SHA": repo.theirs, "PYTHON": sys.executable}
            good = subprocess.run(["bash", "-euo", "pipefail", "-c", body],
                                  cwd=repo.root, env=env, capture_output=True, text=True)
            self.assertEqual(good.returncode, 0, good.stdout + good.stderr)
            repo.write(EXPECTED, SEED[EXPECTED])
            bad = subprocess.run(["bash", "-euo", "pipefail", "-c", body],
                                 cwd=repo.root, env=env, capture_output=True, text=True)
            self.assertNotEqual(bad.returncode, 0)
            self.assertIn("missing keys", bad.stderr)
            env.pop("GFP_HEAD_SHA")
            missing_ref = subprocess.run(["bash", "-euo", "pipefail", "-c", body],
                                         cwd=repo.root, env=env, capture_output=True, text=True)
            self.assertNotEqual(missing_ref.returncode, 0)
            self.assertIn("needs its head SHA", missing_ref.stderr)


if __name__ == "__main__":
    unittest.main()
