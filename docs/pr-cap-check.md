# PR size check

Run `scripts/pr_cap_check.sh [commit-ish]` from the repository whose PR you are
measuring. The target defaults to `HEAD`. An absolute path to the script also
measures the caller's repository, so it works from another worktree or a
subdirectory. Only committed changes at the target count; staged, unstaged,
and untracked changes are not included.

Every measurement fetches `origin`'s `main` into `refs/remotes/origin/main`,
including when the configured fetch mapping omits that ref. It pins both
commits, finds their merge-base, and measures the target from that base with
Git numstat. Fetch, ref, merge-base, or diff failures return nonzero without a
PASS result. There is no local `main` fallback or fetch-skip/cap-override flag.

The cap is **20 changed files / +800 added lines**. Deletions are reported but
give no credit. A verified result within both caps prints `CAP: PASS`, the
remaining file/addition budget, and returns 0. Exceeding either cap prints
`CAP: FAIL` and returns 1. Other errors also return nonzero.

Git accepts one commit-ish (a branch, tag pointing to a commit, commit ID, or
revision such as `HEAD~1`). Empty, option-like, missing, ambiguous, non-commit,
and multi-revision targets fail. This checks size, not whether the target is
the head of a published PR.

NUL-delimited numstat preserves spaces, tabs, and newlines in filenames.
Rename detection uses Git's 50% similarity threshold: a detected rename counts
as one changed file and only its edited lines count; an undetected rename is
a deletion plus an addition. Binary entries count as changed files, but Git
provides no line counts for them, so the helper returns 1 instead of certifying
their addition budget. Text totals do not describe binary size. External diff
and text conversion helpers are disabled; normal Git attributes still determine
whether a file is text or binary.

The merge-base calculation follows the existing primitive in
`scripts/ratchet_admission.py` (`_merge_base`), whose configurable candidates
and local fallback serve a different checker. Use this entrypoint for PR size
checks; it keeps the fresh-main and fixed-cap policy in one place.

Run the local bare-repository regression fixtures with:

```bash
python3 -m unittest scripts.test_pr_cap_check
```

They use temporary repositories and local remotes, with no network or live
runtime state changes. The CI script-check aggregate runs the same fixtures.

## Operator skill rollout

The operator's `99_Skills/agentdesk-issue-pipeline/SKILL.md` is outside this
repository. Its narrow cap-instruction update is preserved in
[`operator-patches/5758-pr-cap-helper.patch`](operator-patches/5758-pr-cap-helper.patch).
Apply it to that canonical source only after the helper is available on main;
do not edit provider mirrors. From `99_Skills`, run `git apply --check` with
the patch's absolute path before `git apply`. These commands also work when
the skill directory is not a Git repository. If the source has changed,
reconcile that cap instruction instead of overwriting unrelated skill edits.
Review-scope diff examples and historical evidence are outside this patch.
