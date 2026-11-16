# Herdr handoff launch failure

Baseline: `33d1f4b5a9dce7023e2d0014e45a05ed6c530bba`. Companion to
[HerdrBusyHandoff](herdr-busy-handoff.report.md), focused on launch **before reserve**;
the old model's Launch action assumed success and missed this cycle.

Decision: a Known Retired Herdr row is not a tmux migration. Its first new input
uses the ordinary launch path (which may still resume the native session under
existing account/source checks). A restored ended journal remains authoritative
until reserve consumes it, explicit cancellation clears it, or bounded launch
failure retains its input. No pane cleanup authority is added.

## Entrypoint census and value bounds

`git grep -n -E 'begin_handoff|finish_handoff|handoff_start_failed|handoff_failed|handoff_ended|unfinished_handoff|claude_handoff::(prepare|recover)' -- src`
is the exhaustive journal reference census; coordinates below are baseline coordinates.
Tests are excluded from the production rows. The implementation report records the
updated coordinates and raw census. No production queue or server was changed.

| Entry / consumer | Code | Reachable journal state and effect |
|---|---|---|
| RootReplaced proof / retire | `discord/herdr_terminate/root_replaced.rs:396,536` proof + retirement; `discord/recovery_engine/herdr_server_replaced.rs:200` | Bound → Retired only after existing proof; no input send |
| Person input before busy/idle selection | `discord/router/message_handler/provider_dispatch/owner_input.rs:196` → `queue_io/input_owner/claude_handoff.rs:27` | Absent, Legacy, Pending, Bound, Retired, opaque rows; existing/new journal judged before dispatch |
| Begin/reuse | `queued_head/v2/claude_handoff.rs:10` | exact front + grant; UUID, logical/session key, original identity; ended=false, failures=0, notice_acked=false |
| Pin / old pane ended | same `:47,:74`; prepare `:132-164` | exact original/pane, ended=false→true; pin absent or verified tmux pin |
| Close failure / retry notice | same `:83,:93`; prepare `:165-188,:310` | durable saturating u32 failures; notice after two close failures, persistent dedupe + acknowledgment |
| Launch failure before reserve | `provider_dispatch/start_failure.rs:124`; `v2/claude_handoff.rs:116` | BEFORE: hold/revoke only, failures stays 0; AFTER: durable count + deadline, capped at five before retention |
| Retain and release head | `queued_head/v2/start_failure.rs:58` | existing KnownNotSent retained original + notice intent + bind relocation in one snapshot; new change closes matching journal in same snapshot |
| Successful reserve (including new execution before Bound) | `queued_head/v2.rs:250-257` | requires herdr target, matching logical, ended=true; consumes journal atomically with attempt; already satisfied on main |
| Bound prepare / input-free recovery finish | `claude_handoff.rs:73-85,:346-364`; `v2/claude_handoff.rs:101` | finish only after journal's original is no longer queued; id/logical matching |
| Local completed receipt | `queued_head/v2/control.rs:384-408` | consumes the original without reserve; an input-free ended journal still needs Bound recovery |
| Timed retry / extra wake | `claude_handoff.rs:270-309` | deduped `(channel,id)` task, rechecks live journal/front; persisted deadline prevents early launch on unrelated wake/reboot |
| Boot and restart census / adoption | `pending_queue_persistence/queued_recovery.rs:54,88-96`; `queued_head/compat.rs:11`; `owner_input.rs:656-685` | journal alone selects actor; retained notice drains before next wake; deadline defaults to 0 for old snapshots |
| Cancel one input | `queued_head/v2/control.rs:207-274` | clears journal only if removed original matches; old callbacks lose claim |
| Queue clear / clear waiting | same `:285-297,:412-422` | clears journal when unattempted waiting items taken; attempted custody preserved |
| Legacy migration / adoption | `queued_head/v2/migration.rs:52`; `queued_head/v2.rs:171`; `v2_types.rs:39` | no journal on V1 migration/new snapshot; optional journal on V2 restore |
| Legacy write and /clear guards | `queued_head/compat.rs:57-79` | V2 remains read-only to legacy save/clear (V2 clear goes through its owner); no new bypass |

`Cap=5` follows the implementation launch budget; backoff uses the existing
`1 << min(failures,5)` seconds (first four sleeps 2,4,8,16 when starting at zero). Existing close failures share
the journal count; restored counts are 0..Cap, with every larger u32 value
represented by Cap (the next launch failure exhausts the budget). The model uses two
originals to expose head release and exact original conservation. It abstracts
ended=true/no pin, no provider effect before reserve, one finite reboot, and
successful durable publication. Close-failure retries, unknown delivery after an
armed reserve, server admission and native transcript receipts stay covered by
the existing HerdrBusyHandoff and input-owner models; their guards are not asserted
by this model. The model's `wait` is a countdown abstraction of the persisted wall
clock deadline. Wall clock rollback can delay retry and is not a fairness proof. After wrapper
cleanup an elapsed deadline wakes once immediately. General prepare/refusal and
recovery timers instead fall back to the existing exponential delay when that
launch deadline has expired, so a read refusal cannot become a zero-delay loop.
Every valid timer retains Bound/input-free completion recovery.

Cancel/clear are explicit user-authorized terminal states, not loss. Boot preserves
the journal failure count and remaining deadline; the ordinary no-journal start
counter resets as main already does (`start_failure_tests.rs:adoption_resets_runtime_count_and_old_claim_cannot_settle_new_claim`). Fairness applies to actual RootReplaced,
Input, Prepare, Launch, Tick and Notice actions. Modeled snapshot publications succeed. Row/token teardown and outbox eventually
completing are explicit port assumptions. The Rust publication-fault test proves
original preservation, not automatic progress after a failed queue write. Their runtime
ordering, dedupe and deadline boundary require implementation tests.

## TLC result

Plain TLC 1.7.4 through `scripts/ci/statecharts_tlc.py`, Java on WSL; seven configs,
18 TLC runs; final measured duration is recorded in the implementation report. Every expected violation matched. Raw output:
`/home/adk/adk/lane-runs/i8146/logs/statechart.log`.

| Config | Safety | Front eventually delivered or visibly retained |
|---|---|---|
| Before (restored ended journal, launch always fails) | RED BoundedRetries | RED FrontProgress |
| Fixed (same restored journal) | GREEN | GREEN |
| Fresh (Retired, no journal, ordinary failure budget) | GREEN | GREEN |
| Success (Retired first input, no journal) | GREEN | GREEN |
| NoCount | RED BoundedRetries | RED FrontProgress |
| ImmediateWake | RED Backoff | GREEN |
| RetiredJournal | RED RetiredIsFresh | GREEN |

The baseline counterexample is replayable in code: Retired is accepted at
prepare:86-100; begin_handoff stores the journal; ended=true bypasses close;
Failure::commit intercepts start_failed; handoff_start_failed only holds/revokes;
completion immediately wakes; the exact same queued front repeats without any
failure count, retained record or terminal notice. The Rust RED test exercises
that actor path across snapshot re-adoption, rather than inferring a code bug
from TLC alone.
