# Production PR size check

Run `bash scripts/pr_cap_check.sh [commit-ish]` in the repository being measured.
The target defaults to committed `HEAD`; an absolute helper path still measures
its caller's worktree. Uncommitted files never count. The local helper fetches
fresh `origin/main`, pins commits and measures from their merge-base. Fetch,
ambiguous/non-commit ref, merge-base and producer errors fail closed.

## Measurement and CI

The cap is **30 production files with a code change / net +800 code lines**.
Code deletions offset code additions. Blank/comment-only lines, matching tests,
fixtures, generated paths and documentation do not count. Rust inline
`#[cfg(test)] mod name { ... }` blocks are excluded using rustfmt indentation;
a test-module declaration without an inline body does not hide following code.

`scripts/pr_cap_prod.py` is an unchanged copy of the campaign skill's producer.
Its `EXCLUDE` tuple is the precise path policy: `tests/*`, `*/tests/*`,
`*_tests.rs`, `*_test.rs`, `*/test_*.py`, `test_*.py`, `*/fixtures/*`,
`*/testdata/*`, `*.snap`, `docs/*`, `*.md`,
`scripts/lib_test_inventory_manifest.txt`,
`scripts/sql_execution_surface_inventory.json`, and `*/generated/*`.
Other generated-looking paths are not implicitly exempt.

The required Script checks runner invokes the cap unconditionally after its
protected aggregate. `PR_CAP_CI=1` reads the actual head and declared base SHA
from `GITHUB_EVENT_PATH`; the synthetic merge SHA is not the measurement head.
Stacked PRs therefore count the child delta, not their parent's changes. Both
commits must already exist in the checkout (`fetch-depth: 0`); CI does not
fetch unrelated `origin/main`. Local mode still requires the fresh fetch.

`CAP: PASS` means both production limits passed. A measured violation prints
`CAP: FAIL` and exits nonzero unless explicitly advisory or exempt. Binary
entries fail instead of inventing line counts, including binaries on otherwise
excluded paths. Measurement errors cannot be converted to exemptions or warnings.

## Exception and kill-switch

A PR body may contain one nonblank, column-zero line:

```text
PR-CAP-EXEMPT: concrete reason this change cannot be split safely
```

CRLF and CR line endings are normalized. Fenced code, HTML comments and
quoted lines do not grant exemptions. Separate an exemption after a quotation
with a blank line so it is not a lazy quote continuation. Multiple reason lines
fail only when a measured violation is present. An empty reason grants nothing. This annotation
permits only a measured cap violation, and prints `CAP: EXEMPT`; it is not a
clean PASS. Reviewers must assess the reason and scope. Advisory, disabled and
exempt results emit a GitHub warning and step summary.

After editing the body or changing the base branch, push a new commit or
close/reopen the PR to obtain a fresh event and measurement. Re-running an old
run reuses its original payload. The current workflow does not run on `edited`.

Repository variable `PR_CAP_MODE` selects `enforce` (default), `report-only`
(measure and report violations without blocking), or `off` (explicit DISABLED,
no measurement). Unknown modes fail. The same environment variable works
locally. This is the operational kill-switch, not an alternate hidden threshold.

The rollout sample measured the latest 40 merged PRs by `mergedAt` (2026-10-06,
#6657 through #6613), using each landed merge's first parent: old gross
20-files/+800 failed 26/40 (65%); production 30-files/net+800 failed 0/40.
The production failure rate is below the 15% report-only trigger, so enforcement
is enabled immediately. This sample is not a future warning baseline.

## Verification and producer limits

```bash
python3 -m unittest scripts.test_pr_cap_check tests.test_pr_cap_ci_wiring
```

The fixtures run real local bare remotes, including clean exact boundaries,
planted violations, exclusions, net deletions, error handling and actual PR
head/stacked-base selection. The canonical-copy SHA256 is pinned in the suite;
recent landed PR outputs were also compared with the external skill producer.

The copied producer uses line-oriented Git numstat and `--no-renames`: a rename
can count both old and new production paths. Git-quoted paths, including non-ASCII and
tab/newline filenames, are not reliably counted. Lockfiles and the generated
`scripts/pg_test_lane_manifest.txt` and
`migrations/postgres/immutable-checksums.json` still consume the budget.
Comment/test classification is lexical, not a Rust parser, and the producer
treats `git show` failures as empty content. These
canonical limitations were preserved rather than silently changing campaign
measurement. The repository and skill copies must be reconciled by the
coordinator in a follow-up; changes need parity evidence and a reviewed policy.
The historical operator patch in `docs/operator-patches/5758-pr-cap-helper.patch`
is not the current production-cap policy.
