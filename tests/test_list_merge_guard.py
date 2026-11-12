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
FIXTURE = ROOT / "tests/fixtures/list_merge/delete-modify"


def fixture_state(side):
    return {path: (FIXTURE / side / Path(path).name).read_text() for path in FILES}


SEED = fixture_state("base")


def production_body():
    script = (ROOT / "scripts/ci-script-checks.sh").read_text()
    registration = 'if run_check contracts "Union list merge guard and required CI contract"; then\n'
    return script.split(registration, 1)[1].split('"$PYTHON" -m unittest tests.test_list_merge_guard\n', 1)[0]


def install_guard(repo):
    scripts = repo.root / "scripts/ci"
    scripts.mkdir(exist_ok=True, parents=True)
    for name in ("list_merge_guard.py", "statecharts_tlc.py", "nextest_ci.py", "nextest_required.py", "merge_candidate.py"):
        source = ROOT / "scripts/ci" / name
        if source.exists():
            shutil.copyfile(source, scripts / name)


def run_production(repo, **overrides):
    env = {**os.environ, "GFP_EVENT_NAME": "pull_request", "GFP_BASE_SHA": repo.base,
           "GFP_HEAD_SHA": repo.theirs, "GFP_CANDIDATE_SHA": repo.git("rev-parse", "HEAD").stdout.strip(),
           "PYTHON": sys.executable}
    env.update(overrides)
    return subprocess.run(["bash", "-euo", "pipefail", "-c", production_body()],
                          cwd=repo.root, env=env, capture_output=True, text=True)


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
        self.repo.merge(fixture_state("ours"), fixture_state("theirs"))
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
        self.assertEqual(step["env"]["GFP_CANDIDATE_SHA"], "${{ github.sha }}")
        self.assertEqual(step["env"]["SCRIPT_CHECK_SHARD"], "contracts")
        title = "Union list merge guard and required CI contract"
        body = production_body()
        from scripts.ci.script_check_inputs import skipped_checks
        for paths in ('["README.md"]', '["docs/statecharts/expected.tsv"]', '[".gitattributes"]'):
            self.assertNotIn(title, skipped_checks({"GITHUB_EVENT_NAME": "pull_request",
                              "SCRIPT_CHECK_CHANGES_RESULT": "success", "SCRIPT_CHECK_CHANGED_FILES": paths}))
        with tempfile.TemporaryDirectory(prefix="list-wiring-") as directory:
            repo = MergeRepo(Path(directory))
            repo.merge(theirs={EXPECTED: SEED[EXPECTED] + "zz/New.cfg\tNew\t-\t-\n"})
            install_guard(repo)
            env = {**os.environ, "GFP_EVENT_NAME": "pull_request", "GFP_BASE_SHA": repo.ours,
                   "GFP_HEAD_SHA": repo.theirs, "GFP_CANDIDATE_SHA": repo.git("rev-parse", "HEAD").stdout.strip(),
                   "PYTHON": sys.executable}
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


class CandidateProvenanceTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="list-candidate-")
        self.addCleanup(self.tmp.cleanup)
        self.repo = MergeRepo(Path(self.tmp.name))
        install_guard(self.repo)

    def test_stale_event_base_preserves_main_only_rows(self):
        main = SEED[EXPECTED].replace("z/Z.cfg", "m/Main.cfg\tMain\t-\t-\nz/Z.cfg")
        self.repo.merge({EXPECTED: main}, {EXPECTED: SEED[EXPECTED] + "zz/Head.cfg\tHead\t-\t-\n"})
        self.assertEqual(self.repo.merged.returncode, 0)
        result = run_production(self.repo)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("ours=" + self.repo.ours, result.stdout)

    def test_candidate_must_equal_checked_out_head(self):
        self.repo.merge()
        result = run_production(self.repo, GFP_CANDIDATE_SHA=self.repo.theirs)
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("not checked-out HEAD", result.stderr)

    def test_candidate_must_have_exactly_two_parents(self):
        self.repo.merge()
        tree = self.repo.git("rev-parse", "HEAD^{tree}").stdout.strip()
        for parents in ((self.repo.ours,), (self.repo.ours, self.repo.theirs, self.repo.base)):
            with self.subTest(parents=len(parents)):
                args = [arg for parent in parents for arg in ("-p", parent)]
                candidate = self.repo.git("commit-tree", tree, *args, "-m", "invalid parent count").stdout.strip()
                self.repo.git("checkout", "-q", "--detach", candidate)
                result = run_production(self.repo)
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertIn("provenance mismatch", result.stderr)

    def test_second_parent_must_equal_event_head(self):
        self.repo.merge()
        result = run_production(self.repo, GFP_HEAD_SHA=self.repo.base)
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("provenance mismatch", result.stderr)

    def test_event_base_must_be_ancestor_of_first_parent(self):
        self.repo.merge()
        tree = self.repo.git("rev-parse", self.repo.base + "^{tree}").stdout.strip()
        unrelated = self.repo.git("commit-tree", tree, "-m", "unrelated root").stdout.strip()
        result = run_production(self.repo, GFP_BASE_SHA=unrelated)
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("provenance mismatch", result.stderr)

    def test_delete_modify_fixture_is_rejected_by_production_shell(self):
        self.repo.merge(fixture_state("ours"), fixture_state("theirs"))
        self.assertEqual(self.repo.merged.returncode, 0)
        result = run_production(self.repo)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("delete/modify", result.stderr)
        result = subprocess.run([sys.executable, str(GUARD), "--base", self.repo.base,
                                 "--ours", self.repo.ours, "--theirs", self.repo.theirs,
                                 "--merged", str(self.repo.root), "--repo", str(self.repo.root)],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("delete/modify", result.stderr)

    def test_local_api_worktree_tree_and_exit_codes(self):
        self.repo.merge()
        argv = [sys.executable, str(GUARD), "--base", self.repo.base, "--ours", self.repo.ours,
                "--theirs", self.repo.theirs, "--repo", str(self.repo.root)]
        for merged in (str(self.repo.root), self.repo.git("rev-parse", "HEAD^{tree}").stdout.strip()):
            with self.subTest(merged=merged):
                result = subprocess.run([*argv, "--merged", merged], capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.repo.write(EXPECTED, "# empty result\n")
        result = subprocess.run([*argv, "--merged", str(self.repo.root)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        invalid = argv.copy()
        invalid[invalid.index("--base") + 1] = "missing-ref"
        result = subprocess.run([*invalid, "--merged", str(self.repo.root)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("git", result.stderr)


if __name__ == "__main__":
    unittest.main()
