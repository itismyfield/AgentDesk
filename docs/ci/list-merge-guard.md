# Union list merge guard API

The stable entry point is `scripts/ci/list_merge_guard.py`. It owns syntax,
duplicate/scope, three-way and sorted-order checks for
`docs/statecharts/expected.tsv` and `scripts/ci/test-removal-justifications.txt`.
Callers supply immutable inputs after merging; they do not implement these rules.

```bash
python3 "$merged/scripts/ci/list_merge_guard.py" --base "$merge_base" --ours "$ours" --theirs "$theirs" --merged "$merged" --repo "$merged"
```

| Input | Contract |
|---|---|
| `--base SHA` | The unique Git merge-base of `--ours` and `--theirs`, not an older event base. |
| `--ours SHA` | The actual first-parent commit used by the merge. |
| `--theirs SHA` | The actual second-parent commit used by the merge. |
| `--merged DIR_OR_TREE` | An existing directory containing the result lists, or a Git tree-ish (including a tree OID). A directory wins if both interpretations exist. |
| `--repo DIR` | The Git object store used for the three refs and an optional result tree. Defaults to the current directory. |
| `--normalize` | Optional: after every semantic check passes, sort a directory result and move comments into its header. Tree results are read-only and reject this option. |

| Exit | Meaning |
|---|---|
| 0 | Both lists pass parsing, duplicates/scopes, three-way content and sorted-order checks. |
| 1 | List contract violation: malformed/empty list, duplicate/overlapping key, conflicting edits, resurrection, missing/extra/changed row, or unsorted result. |
| 2 | Input error: invalid arguments/refs, wrong or ambiguous merge-base, invalid PR candidate provenance, missing/unreadable list, decoding error or Git failure. |

Default operation reads inputs without changing the worktree, index or refs.
Both 1 and 2 block landing; callers must preserve the distinction in logs.
The legacy positional `MERGE_BASE OURS THEIRS --repo DIR [--result REF]` form
remains available, with the same exit codes.

An old branch may lack the entry point. Callers can check
`[ -f "$merged/scripts/ci/list_merge_guard.py" ]` and skip only when it is absent.
A present entry point that returns nonzero must never be skipped. Enabling union
requires both the required CI caller and the local landing callers to be wired;
their rollout evidence belongs in the landing PR.
For a tree result, test the same fixed path with
`git -C "$repo" cat-file -e "$tree:scripts/ci/list_merge_guard.py"` instead of a
filesystem check.

The required contracts runner uses `--ci`. For PRs it reads `GFP_CANDIDATE_SHA`,
`GFP_BASE_SHA` and `GFP_HEAD_SHA`, pins the candidate to checked-out HEAD, and
uses the shared `scripts/ci/merge_candidate.py` provenance contract. Exactly two
ordered parents, the event head as second parent and event-base ancestry of the
first parent are required. The validated first parent supplies OURS. Non-PR
checks use HEAD for all three refs to validate the current lists.

The committed deletion/modification fixture is
`tests/fixtures/list_merge/delete-modify/`. Its production-shell and local-API
test creates real base/ours/theirs commits with union attributes and expects
Git merge to succeed while both guard callers return 1:

```bash
python3 -m unittest -v tests.test_list_merge_guard.CandidateProvenanceTest.test_delete_modify_fixture_is_rejected_by_production_shell
```
