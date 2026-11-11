# Thread GC ownership model

Code baseline: `2ddece1ebb2de1f66c4263c4bc29239b36250ec3`. This report records the S4-12 design leg before the production ownership move. The existing `HostedCleanup.tla` is extended with `GCSpec` rather than replaced. Its prior bulk/explicit specification and five config expectation rows remain byte-identical.

`Boundary = "DB"` describes the current caller owning observation order; `Boundary = "service"` describes the proposed service caller. It changes no policy, predicate, lock, filesystem permission or process authority. The GC behavior has the same reachable-state count and rule results under both boundaries. This bounded abstraction does not execute Rust, PostgreSQL or tmux.

## Executed configs

| Config | Statechart exit | Reachable states | Wall time | Actual rule violations |
|---|---:|---:|---:|---|
| `ThreadGCBefore.cfg` | 0 | 53,944 | 1.606s | 없음 — 7 invariants + NoDeadlock |
| `ThreadGCAfter.cfg` | 0 | 53,944 | 1.593s | 없음 — 7 invariants + NoDeadlock |
| `ThreadGCNoRecheck.cfg` | 1 | 52,776 | 3.954s | GCNewTurnKept, GCConditionalRefusalsKept |
| `ThreadGCBeforeGaps.cfg` | 1 | 53,944 | 5.152s | GCCurrentLegacyMarkerSafe, GCCurrentOwnerLocal, GCAliasesRechecked |
| `ThreadGCAfterGaps.cfg` | 1 | 53,944 | 5.192s | GCCurrentLegacyMarkerSafe, GCCurrentOwnerLocal, GCAliasesRechecked |

The two core configs are GREEN; the no-recheck mutant is intentionally RED; both characterization configs are identically RED for three existing gaps. None of the eight required safety/deadlock rules is unchecked. The state counts above are the statechart tool's completed reachability count after it removes each broken rule and checks the others; `tlc.out` for RED configs is the first counterexample run and therefore has a smaller prefix count.

The official `scripts/ci/statecharts_tlc.py` checker was also run against a scratch root containing the exact model/config bytes and the ten hosted-cleanup expectation rows: **10 selected, 10 expected outcomes matched, 0 errors**, 19.3 seconds. Existing five configs retain their prior results. Raw log: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i7353/sc-s4-12/statecharts-ci.log`. Five new expected rows were added; no previous expected rule was removed or changed. The final handoff changes only the view label; the model/config/expectation bytes remain the same as the official checker run, so that checker was not repeated.

Core configs exceed the 500-state chart budget, so the tool correctly produces no core chart. The mutant chart has 9 states/8 arrows; each characterization chart has 15 states/14 arrows. Reports: [before](thread-gc-before.report.md), [after](thread-gc-after.report.md), [mutant](thread-gc-no-recheck.report.md), [before gaps](thread-gc-before-gaps.report.md), [after gaps](thread-gc-after-gaps.report.md). Mermaid sources: [mutant](thread-gc-no-recheck.chart.mmd), [before gaps](thread-gc-before-gaps.chart.mmd), [after gaps](thread-gc-after-gaps.chart.mmd). The view does not filter actions; the tool emits counterexample paths only above its graph budget.

## Rules and source correspondence

All coordinates below are historical evidence at the code baseline. A phrase such as “baseline line 138” identifies that historical blob, not the current file. Implementation coordinates are recorded in the lane artifact `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i7353/notes-s4-12-impl.md`, checked against implementation head `5703805f15a25ec70bacf283ee4e5e2bb14e9abe`.

| Model action/value or rule | Source evidence and meaning |
|---|---|
| `GCReadCandidate` | baseline line 90 of `src/db/dispatched_sessions/thread_gc.rs` legacy thread-id backfill; baseline line 91 candidate SELECT, baseline line 110 query/decode error becomes no candidates. `idle` abstracts the SQL eligible set idle/disconnected/aborted. |
| `GCObserveHost` / `GCUnknownLookupKept` | `services/tmux_turn_liveness.rs:186` composes CleanupRow gate then occupancy; `tmux_turn_liveness/cleanup_host.rs:18` checks payload and all Legacy locator markers; `db/dispatched_sessions/hosted_execution.rs:639` rejects unknown/unowned/non-retired. Query failure/missing/unreadable never becomes Legacy. |
| `GCObserveOccupancy` / `GCOccupancyLookupFailed` | `services/tmux_turn_liveness.rs:201` status/active dispatch/active children/open-child query; `:214` DB error or missing row is false. |
| `GCObserveOwner` | baseline line 68 of `src/db/dispatched_sessions/thread_gc.rs` runtime-root owner marker; baseline line 69 owner-file read, mismatch/unreadable becomes ProbeFailed. The model field `row.owner` denotes this external file, not a new DB column or a hosted-record owner. |
| `GCProbe` / `GCObservedHostBeforeProbe` / `GCObservedMissingAndLocal` | baseline line 74 of `src/db/dispatched_sessions/thread_gc.rs` presence probe only after local owner; baseline line 77 blocking-task failure is ProbeFailed; baseline line 130 only Missing reaches DELETE. `aliasMarker` stores this presence outcome in GCSpec. |
| `GCNewTurn` / `GCNewTurnKept` | baseline line 138 of `src/db/dispatched_sessions/thread_gc.rs` status, baseline line 139 dispatch, baseline line 140 active children, baseline line 141 deadline, baseline line 148 open-child rechecks; new Bound payload changes generation. `sawWaitMarker` is GCSpec's new-turn-started flag. A direct TUI turn with no SQL update is protected when the probe returns Present (`services/tmux_turn_liveness/tests_pg.rs:27`); no stronger post-Missing guarantee is assumed. |
| `GCRowProjectionChanges` / `GCConditionalRefusalsKept` | baseline line 137 of `src/db/dispatched_sessions/thread_gc.rs` thread id still present; baseline line 142 exact hosted raw, baseline line 143 provider/identity, baseline line 144 token, baseline line 145 channel/key, baseline line 146 SQL hosted gate. `generation` abstracts all these already-compared raw/identity/key fields; no new generation column is proposed. |
| `GCKillFailed` / `GCKillFailureKept` | `services/tmux_turn_liveness/tests_pg.rs:49` failed prior idle kill leaves Present; unavailable presence is ProbeFailed, and both preserve resume selectors. GC itself does not kill. |
| `GCConditionalDelete` / `GCAutocommit` / `GCStatementFailure` | baseline line 134 of `src/db/dispatched_sessions/thread_gc.rs` single conditional DELETE, baseline line 153 pool execution, baseline line 154 result check. GC has no explicit mutation lock or transaction. SQL's row lock and statement commit/rollback are abstracted as apply→autocommit; other writer interleavings stop after apply while that statement owns the row. |
| `GCRestart` | Discard uncommitted work and rerun candidate/host/occupancy/owner/probe once; no durable cleanup cache or new startup handler. |
| `NoDeadlock` | Every terminal preserved/deleted state has the existing `Done` self-loop. No GC liveness property is claimed: periodic scheduling, repeated calls and eventual disappearance of external errors are not changed by this ownership move. The original bulk/explicit liveness property remains checked by their old configs. |

`GCNewTurnKept`, `GCUnknownLookupKept`, `GCObservedHostBeforeProbe`, `GCObservedLegacyMarkerSafe`, `GCObservedMissingAndLocal`, `GCKillFailureKept` and `GCConditionalRefusalsKept` are the seven required invariants. `GCNewTurnKept` is about SQL-visible active/fresh/bound/occupied new turns; it does not claim that a JSONL-only turn starting after Missing is fenced by the current implementation.

## Counterexamples and code replay status

| Rule | Numbered shortest path | Code confirmation |
|---|---|---|
| `GCNewTurnKept` and `GCConditionalRefusalsKept` in NoRecheck | 0 candidate lookup → 1 snapshot → 2 safe host → 3 unoccupied → 4 local owner → 5 Missing → 6 DB status becomes active → 7 stale snapshot DELETE → 8 commit. | Source replay at the code baseline confirms that baseline line 138 of `src/db/dispatched_sessions/thread_gc.rs` refuses this active row. The mutant removes the conditional checks; a production Rust/PG mutant will be run in the implementation leg. This design leg did not execute a Rust mutant or PG reproduction. |
| `GCCurrentLegacyMarkerSafe` | Same steps 0–5 → 6 unsafe marker written → 7 conditional SQL → 8 commit. | `cleanup_host.rs:18` is observed once; baseline line 134 of `src/db/dispatched_sessions/thread_gc.rs` contains no fresh filesystem check. Source-confirmed existing window, no runtime repro in this design leg. |
| `GCCurrentOwnerLocal` | Same steps 0–5 → 6 owner file changes → 7 conditional SQL → 8 commit. | baseline line 69 of `src/db/dispatched_sessions/thread_gc.rs` owner is observed before presence, then baseline line 134 checks only DB predicates. Source-confirmed existing window, no runtime repro in this design leg. |
| `GCAliasesRechecked` | Same steps 0–5 → 6 alias added → 7 conditional SQL → 8 commit. | Candidate includes aliases (baseline line 94 of `src/db/dispatched_sessions/thread_gc.rs`); conditional SQL and `CleanupRow::bind_recheck` (`hosted_execution.rs:661`) do not compare aliases. Source-confirmed existing window. |

These existing gaps stay identical before/after. A model-authoring intermediate run mislabeled an ordinary projection change followed by restart as a new turn. Separating the GC new-turn flag from the generic row-changed flag corrected the abstraction; intermediate `*-final` outputs are retained as authoring history and are not final evidence. A final label-only correction names the conditional action `DELETE 조건 판정` in both ordinary and mutant charts. Only `*-handoff` and `runs-handoff.json` describe the final checked model/view bytes; earlier complete/verified/final runs are authoring history.

## Bounds and unchanged gaps

One session, at most one SQL-visible new turn/projection change, one marker change, one owner-file handover, one alias addition, one failed kill and one process restart are modeled. Status eligible values share the same branch; SQL raw/identity/key changes share a generation. Known Pending/Bound share `bound`; unreadable payload and lookup failures are distinct preservation cases. Initial filesystem marker states cover absent/tmux/non-tmux/read failure/unknown. The race writes one representative unsafe marker; the reader rejects all three unsafe variants through the same branch.

The filesystem and DB are not atomic. The current owner-file handover, post-observation marker change and alias gap are deliberately exposed by stronger characterization rules. Hosted-record owner mismatch is `foreign` and remains protected separately from owner-file handover. Known owned Retired records skip host-marker reads as approved in S4-11.

JSONL/selector-only new turns or tmux recreation after Missing with no SQL predicate change are outside the passing new-turn invariant and remain an existing gap. No runtime generation, lock, re-probe, alias check, owner-file fence, kill authority or filesystem permission is added by the design. The model does not simulate all adjacent Cancel/Terminate/clear/stop routes, PostgreSQL lock scheduling, FK cascades, provider JSONL parsing or real boot scheduling; their code census is in the lane plan, and this move changes neither their state writes nor guards.

## Reproduce

```sh
unset TMUX TMUX_PANE
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart \
 docs/statecharts/hosted-cleanup/HostedCleanup.tla \
 --config docs/statecharts/hosted-cleanup/ThreadGCBefore.cfg \
 --view docs/statecharts/hosted-cleanup/thread-gc.view.mjs \
 --out /Users/itismyfield/.adk/release/lane-runs/hl-20260925/i7353/sc-s4-12/reproduce-new
```

Use each of the other four config names with a new output directory. Raw logs, TLC outputs and exact commands/timings are under `sc-s4-12/*-handoff`, `runs-handoff.json` and `statecharts-ci.log`.
