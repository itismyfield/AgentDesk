# Known flaky retry

`scripts/ci/known-flaky-tests.json` is a temporary retry allowlist. Each entry
names an exact failed test, a CI run demonstrating the failure, and its cause.
It does not skip tests or change their expected result.

The trusted default-branch `CI PR infrastructure retry` workflow requests
`gh run rerun --failed` only on attempt 1, when all failed jobs have complete
test summaries and every failing test is listed. Unknown failures, incomplete
logs, mixed failures, cancelled jobs, and stale attempts are not eligible.
Required-context mirror failures are eligible only when the exact trusted
job and failing step report the expected upstream-failure diagnostic, their
upstream job already passed the named-test classification, and Changed paths
succeeded. Independent mirror failures, failed pins, and mismatched upstreams
block retry. A workflow-contract test keeps these explicit mappings in sync.
Both classifiers check the current open PR head against the failed run's SHA
immediately before requesting a retry. Missing PR evidence or an advanced head
blocks the request. The PostgreSQL classifier repeats this check for each shard.
The existing PostgreSQL infrastructure classifier remains the fallback even if
the flaky step fails with an exception; cancellation or a handled flaky retry
suppresses that fallback.
Attempt 2 never requests another automatic retry, even if it fails differently.

After a root-cause fix passes ten consecutive runs under load, remove its
entry in the same change. Keep the original failure, cause, and repair evidence
in the issue/lane report. Do not add broad module prefixes or wildcard entries.
Missing evidence is a real failure requiring investigation, not permission to
extend the allowlist.

Coordinator integration: `wait-merge-infra.sh` should recognize
`rerun-requested:known-flaky` for the same run ID and head SHA, wait for attempt
2, and report `CI_FLAKY_RERUN_PASS` only when that attempt succeeds. It must not
request another retry while this workflow owns the first retry, or label a
manual or infrastructure rerun as a known-flaky pass.
The retry workflow's display title includes the source run ID, head SHA, and
attempt, so the coordinator can locate its classification log without confusing
it with another pull request's retry.

## Retired candidates

The active list is empty after the repairs in this change. Original evidence is retained here; a repair must pass the required load proof before this change is ready for review.

- `services::claude_tui::busy_inject_tests::an_owned_draft_is_entered_once_and_confirmed_from_the_transcript` — [failure run](https://github.com/itismyfield/AgentDesk/actions/runs/39602502973). Owned draft confirmation races the transcript fixture under load.
- `services::discord::herdr_terminate::http_force::tests::force_duplicate_abort_restart_pg` — [failure run](https://github.com/itismyfield/AgentDesk/actions/runs/39703194502). Two cancel requests are not guaranteed to overlap before postlude removes the session.
- `services::platform::tmux::session_inventory::real_tests::u_inventory_missing_socket_real` — [failure run](https://github.com/itismyfield/AgentDesk/actions/runs/39746557961). The fixture's noncanonical temporary path disagrees with tmux's canonical socket diagnostic; ambient namespace environment also requires isolation.
- `services::platform::tmux::session_inventory::tests::u_inventory_complete_stdout` — [failure run](https://github.com/itismyfield/AgentDesk/actions/runs/39423859570). Ambient namespace environment races parallel tests.
