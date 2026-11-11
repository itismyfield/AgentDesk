# Hosted cleanup ownership boundary

Code baseline: `1d5f2657f043f4b451a688512d66a2c817dd9707` (the merged S4-11 base and generated-file synchronization). The model checks the existing deletion policy before and after moving marker observations from DB orchestration to services. The boundary name is the only difference between Before and After; this is a bounded policy model, not execution of Rust or PostgreSQL.

There was no equivalent `sessions` bulk/explicit DELETE model in `session-lifecycle`, so this area has its own folder.

## Policy

`LegacyObservedMarkerSafe` protects SQL NULL / Legacy rows using the marker observation on the candidate's locators: absent or tmux permits cleanup; non-tmux, unknown and read failure preserve the row. The service does not read markers for owned Known Retired records. Pending, Bound, unknown and unowned records remain protected. This is the approved existing policy, including its existing race gaps.

The explicit route observes before the first row-lock wait, releases that wait lock, compares the row and observes Legacy markers again, then conditionally deletes under the actual mutation lock. Bulk observes candidates once and uses the existing conditional SQL DELETE. No extra activity, heartbeat, alias or filesystem fence is assumed.

## Results

| Config | Statechart exit | States | Expected result |
|---|---:|---:|---|
| `HostedCleanupBefore.cfg` | 0 | 27,514 | every core invariant and liveness property holds |
| `HostedCleanupAfter.cfg` | 0 | 27,514 | same result after the ownership move |
| `HostedCleanupNoRecheck.cfg` | 1 | 22,588 | removing the existing recheck breaks `ChangedRowRechecked`, `BulkDisconnectedRechecked`, `ExplicitAliasRechecked` |
| `HostedCleanupBeforeGaps.cfg` | 1 | 27,514 | four stronger characterization rules expose the existing gaps |
| `HostedCleanupAfterGaps.cfg` | 1 | 27,514 | exactly the same four gaps remain |

The core invariants are `LegacyObservedMarkerSafe`, `KnownRetiredSkipsMarker`, `UnknownAndUnownedKept`, `ExplicitLockWaitMarkerNotMissed`, `ChangedRowRechecked`, `ExplicitAliasRechecked`, `BulkDisconnectedRechecked` and `RollbackKeepsEarlier`. `NoDeadlock` and `EligibleEventuallyDeleted` also hold in both core configs. The latter requires repeated cleanup calls and weak fairness once the row is stably eligible; it does not assume progress through permanent failure or endless new turns.

The repository checker `scripts/ci/statecharts_tlc.py` selected these five exact configs from a scratch copy with the same model/config bytes and matching manifest rows: **5 selected, 5 matched expectations, 0 errors**, 10.7 seconds total. Per-config times were After 2.6 s, AfterGaps 5.6 s, Before 2.5 s, BeforeGaps 5.6 s, NoRecheck 4.6 s. No existing expected-rule row was changed.

All configs exceed the tool's 500-state chart budget. Core outputs therefore have reports and no chart; the mutant chart contains 10 states/10 arrows, and each gap chart contains 13 states/15 arrows. See [before report](before.report.md), [after report](after.report.md), [mutant report](no-recheck.report.md), [before gaps](before-gaps.report.md), [after gaps](after-gaps.report.md), [before gap chart](before-gaps.chart.mmd) and [after gap chart](after-gaps.chart.mmd).

## Actions and code correspondence

Coordinates below compare the frozen baseline with the service ownership implementation. The entrypoint and neighboring-route census remains in the S4-11 lane report; this table maps the modeled boundary itself.

| Action | Before | After |
|---|---|---|
| ReadCandidate | `db/dispatched_sessions/hosted_execution.rs:685,727,758` | `db/dispatched_sessions/hosted_execution.rs:677,726,748`; service orchestrates at `services/dispatched_sessions/hosted_cleanup.rs:11,29` |
| ObserveMarker | `hosted_execution.rs:643,674` | `services/tmux_turn_liveness/cleanup_host.rs:18`; `record_deletable` at `hosted_execution.rs:639` remains DB-only |
| WriteMarkerDuringWait | the marker-only interleaving in `canonical_identity_pg_tests.rs:2431` | same scenario; no new writer authority |
| NewTurnOrRespawn | bulk status condition `hosted_execution.rs:708`; raw/identity/key comparison `:709`; explicit CleanupRow comparison at frozen-baseline line 822 | identical SQL condition at `hosted_execution.rs:706`; explicit comparison `:791` |
| AddAlias | explicit CleanupRow equality includes ordered aliases; bulk SQL recheck does not | same row comparison and bulk SQL projection |
| ReleasePriorLock | `hosted_execution.rs:772,797` | service `hosted_cleanup.rs:37`; DB wait primitive `hosted_execution.rs:766` |
| ObserveAfterWait | `hosted_execution.rs:774,777` | `hosted_cleanup.rs:38–41` |
| ConditionalApply | frozen-baseline bulk DELETE at line 707 and locked explicit DELETE at line 814 | `hosted_execution.rs:697,783`; explicit DB transaction `dispatched_sessions.rs:2552` |
| Commit / Rollback | one bulk transaction; explicit wait transaction rolls back before actual mutation | same transaction boundaries and order |
| Restart / Retry | discard uncommitted work and read candidates on a later cleanup invocation | same behavior; at most one modeled restart and transient failure |

## Existing gaps retained

These are characterization failures, not failures of the approved core policy. None is repaired by the ownership move.

| Rule | Shortest path and code check |
|---|---|
| `LegacyCurrentMarkerSafe` | 5 steps: Legacy safe candidate observed → unsafe-marker alias added → bulk conditional DELETE → commit. Bulk compares the snapshot's raw/identity/primary key but does not recheck aliases or fresh markers. |
| `AllAliasesRechecked` | same 5-step path: a new alias is absent from bulk's recheck. Explicit's existing row equality rejects it. |
| `BulkLockWaitMarkerNotMissed` | 6 steps: Legacy absent marker observed → DELETE waits on row lock → marker-only unknown written → lock released → conditional DELETE → commit. Bulk has no post-wait marker observation. Explicit's Legacy post-wait observation is a core invariant and passes. |
| `ExplicitActivePreserved` | 6 steps: candidate → first observation → status-only active turn → explicit post-wait observation → conditional DELETE → commit. CleanupRow does not contain status, active dispatch, children or heartbeat; explicit deletion keeps this existing meaning. |

The NoRecheck mutant's 5-step row/6-step alias counterexamples show why the existing conditional checks remain necessary. They are model mutations, not a substitute for the lane's Rust mutant and throwaway-PG tests. The listed gaps were compared to the code paths; their PostgreSQL interleavings were not replayed as part of model authoring.

Normal `retire_pg` only CASes the hosted record; it does not remove `.host_kind`. An owned Retired record can therefore retain a Herdr marker. `KnownRetiredSkipsMarker` preserves the approved behavior for that path.

## Bounds and assumptions

One representative session and an earlier row in its bulk transaction are modeled. The abstract `bound` record stands for both non-retired Known states (Pending and Bound), which share the same cleanup refusal. Generation represents the hosted raw value, identity and primary key projection already covered by the SQL predicates. A separate alias generation distinguishes the existing explicit alias equality from bulk's alias gap. Marker states cover tmux, non-tmux, absent, read failure and unknown; the representative unsafe marker can be on the primary locator or a newly added alias.

A status-only new turn is separate from a Bound respawn. Only the former changes status without changing the existing compared generation. The model does not add a new generation column or any activity condition to explicit delete. Ownership mismatch is an initially unowned record; transition to a new Bound record changes the compared projection.

The filesystem observation and PostgreSQL lock are not atomic. The first explicit lock wait and the following observation are represented; an independent second marker-only write during acquisition of the final mutation lock is outside this small model. Actual tmux liveness probes, remote owner observations and other cleanup routes have their existing guards and are not silently added to hosted cleanup here.

The view does not filter actions. When state count exceeds 500 the tool emits only counterexample paths. `Boundary` names the DB/service observation owner but does not alter policy, locks, authority or I/O permissions.

## Reproduce

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart \
 docs/statecharts/hosted-cleanup/HostedCleanup.tla \
 --config docs/statecharts/hosted-cleanup/HostedCleanupBefore.cfg \
 --view docs/statecharts/hosted-cleanup/hosted-cleanup.view.mjs \
 --out /tmp/hosted-cleanup-before-new
```

Use the other config names and a new output directory for each run. `expected.tsv` registers the two passing core configs, three expected mutant rules, and four identical characterization rules in both gap configs. It is a manually maintained rule manifest, not a generated baseline; only the five new rows were added under the lane's explicit approval.
