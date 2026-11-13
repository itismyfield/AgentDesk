# Deletion versus modification under Git union

Each of `base/`, `ours/` and `theirs/` contains the two list snapshots. Map the
basenames to `docs/statecharts/expected.tsv` and
`scripts/ci/test-removal-justifications.txt`, with `merge=union` for both paths.

OURS deletes `agentdesk a::test`; THEIRS changes its reason. A real Git union
merge succeeds and restores the changed row. Parsing alone accepts it, but the
three-way guard must return 1 with `delete/modify`.

`tests.test_list_merge_guard.CandidateProvenanceTest.test_delete_modify_fixture_is_rejected_by_production_shell`
reconstructs these commits and checks both the required production shell and
the stable local API against the same merged worktree.
