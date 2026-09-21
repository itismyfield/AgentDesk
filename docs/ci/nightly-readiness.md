# Nightly failure notification and readiness (#6006)

`Main CI Triage` receives completed `CI Main` and `CI Nightly` runs. Both the
workflow and helper require the trusted repository and `main`. Main keeps its
push/streak/recovery policy. Nightly accepts schedule or workflow_dispatch and
records the first completed workflow failure, including runner shutdowns, without
fetching historical runs or logs. This happens after the whole workflow completes,
not immediately when its first job fails. Success/cancellation makes no writes.

Nightly owns `[ci-red] CI Nightly 실패(main)` with the exact body marker
`<!-- agentdesk:ci-nightly:main -->`. It does not use main's identifier/recovery
path. The issue body and every comment page are checked for an exact-line marker
containing repository, upstream run ID and upstream run_attempt. Triage's own
attempt is irrelevant. A first create includes its marker in the body; subsequent
failures include theirs in a comment. Keep these markers when editing the issue.

Closed issues are included: an old marker means no writes; a new failure reopens
before commenting. Ambiguous candidates, malformed responses and GitHub API errors
fail the triage job. If a write was applied but its response failed, rerunning
reads persisted state first. Lookup/write is not a transaction or exactly-once
promise. Marker deletion and manual concurrent edits require operator inspection.

Only eligible nightly failure jobs enter a fixed concurrency group with
`cancel-in-progress: false` and `queue: max`. GitHub currently documents up to 100
pending runs; full queues can cancel additional jobs. Queue entry order is not
upstream event order. See [GitHub concurrency documentation](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/control-workflow-concurrency).
Cancelled/error triage runs or missing upstream run/attempt markers require manual
comparison and rerunning the corresponding triage run. No automatic reconciliation
is provided. Local actionlint 1.7.12 rejects `queue`; this schema lag is not a
successful lint result. The setting must also be verified on GitHub after merge.

GitHub recording is mandatory. AgentDesk immediate sync uses the existing route
as best-effort, with a 5-second connect and 15-second total timeout. A replay does
not repeat labels, issue writes or sync POSTs, even after sync failure. Check actual
AgentDesk receipt separately; use the normal repository sync operation to recover
a missed immediate sync. A green mock or GitHub issue is not receipt evidence.

On 2026-09-20, run 35533824414 (63b27abc1e5025783d629f20d393aad27e07564e)
failed its PostgreSQL cargo step at 20:00:01 UTC after starting at 19:55:06 UTC,
with exit 143 and a runner shutdown signal during compilation. The 30-minute
step timeout was not reached. These facts do not establish OOM or shutdown cause.
The inspected 2026-09-13–20 window contains eight failed runs, not 100 days.
macOS, Windows and Playwright failures are separate unresolved observations.

Keep #6006 open (`Refs #6006`) until actual schedule/dispatch delivery is recorded
separately, upstream attempt replay is checked, shutdown cause is established,
and PostgreSQL readiness, compilation, selected tests/results/skips and the
required complete nightly lanes are observed. This notification change does not
change PostgreSQL filters, profiles, timeouts or repair those other failing lanes.

## N1 mitigation: profiles and macOS tmux (Refs #6006)

Only `full_macos`, `full_windows` and `postgres_full` set both
`CARGO_PROFILE_DEV_DEBUG` and `CARGO_PROFILE_TEST_DEBUG` to `"0"` at job level.
This aligns their debug-information settings with Main/PR, reducing build/link
work at the cost of native backtrace detail. Panic/assertion text remains.
The focused Windows Discord step retains its explicit debug env and `BASH_ENV`.
macOS installs tmux and prints `tmux -V`; three historical failures directly
reported missing tmux, two were inferred. Inspect all five, not a promised 55→50.

Selectors, direct foreground Cargo statements, false-positive replay, runners,
timeouts, registry/git-only cache, baselines and required contexts are unchanged.
Both Cargo extractors must retain their full inventories: a generic wrapper can
vanish from target-integrity extraction while membership still sees a substring.
The focused regression module is in the PR/Main script-check aggregate, not the
Nightly inline scripts job. No new required context or guard pin is introduced.

The captured PG 143 occurred about five minutes into a 30-minute compile step,
before tests; its cause is UNKNOWN. Debug-off is mitigation, not proof of OOM or
causal identity with #4245. Workflow-red counts are not days or PG-failure counts;
the Windows timings are not an isolated 3.7× debuginfo experiment. Repeated
warnings do not establish link units or compiler phases. A build banner or a
static inventory count does not establish PG test execution/readiness.

Notification #6080/#6082/card/replay is already complete. Root will observe one
post-merge Nightly at its exact SHA/run/attempt using the event-only watch.
Require actual PG startup/readiness, compilation and selected per-target test
results; legitimate zero-selected auxiliary targets are not themselves failure.
Report PG separately from overall Nightly. B/C/E are outside N1, not dependent
on one another; the old Windows residual is not current after #6081. #6006 stays OPEN.
