# Idle observer attachment — slice F6 (#7676)

Base: `90fe565acb`. The existing ownership columns and action names remain unchanged.
The added `observer` record and `ObserverInit`/`ObserverNext` isolate one queued original on
an idle Claude pane during boot or a later binding publication. `attached` means a readable
transcript source is ready for its observer; a runtime tuple alone or a relay watcher task is
insufficient. The source may attach before the owner starts, after it captures no observer, or after `idle_unobserved` returns. A second registration of the
same already-ready source adds no wake. A durable Pending source becoming readable and
Resolved is its first readiness event even when `(runtime_kind, session_id, output_path)` stays
unchanged.

## Code mapping

| Action | Code |
|---|---|
| `ObserverDriveGrants` | `queue_io/input_owner/drive.rs:98-124,168-180`; `turn_orchestrator/queued_head/v2.rs:248-315` |
| `ObserverIdleUnobserved` | `router/message_handler/provider_dispatch/owner_input.rs:686-687`: no observer refuses before reserve or keys |
| `ObserverSubmits` | `owner_input.rs:706-710`: the observed owner handoff runs once |
| `ObserverTurnReleases` | `turn_orchestrator/episode_identity.rs:303-320,428-437` releases the active token then marks the existing signal done; `turn_finished_signal.rs:26-44` latches completion; attachment captures/checks its latch at `rehydration.rs:388-405` |
| `ObserverAttaches` | `tui_prompt_dedupe/runtime_binding.rs:59-112` publishes a changed, ready source; `tui_prompt_relay/rehydration.rs:316-364` resolves its channel/runtime after publication, and `wake_observed_source` at `rehydration.rs:367-431` schedules the existing kickoff for a waiting front |

The wake is attached to the common `RuntimeBindingPublication` boundary, rather than only the
rehydration pass. Its `Drop` compares `(runtime_kind, session_id, output_path)` and publishes the
new binding before notifying `discord::observer_binding_published`. A missing file and a Claude
binding with a durable `Pending` are not ready at this boundary; its staged binding and explicit
channel mapping are published together (`runtime_binding.rs:299-338`) so readiness is judged
against the right channel. A caller with no explicit channel keeps the existing mapping and TTL.
This also covers a deferred adoption before rehydration and a hook publication outside that pass. Offset-only refreshes and
re-registering the same already-ready source add no wake.

A second boundary has no tuple change: a durable `Pending` names the exact same file and session
before the file exists, then that file becomes readable and its record becomes `Resolved`.
The held-binding restore branch in `tui_prompt_dedupe/pending.rs::restore_under_source_authority`
(`pending.rs:605-628`) captures `pending_seq: Some`, persists the source as `Persisted::Logged`,
and emits the observer-ready callback only after `pending_seq` becomes `None`. The hook's
`runtime_binding/claude_source.rs::Candidate::judge` captures whether this exact source has a
durable Pending before `record_verified`. Its callback requires that prior Pending, a successful
`Committed::Appended`, no Pending afterwards, and proposal session/path equal to the bound source.
An unchanged verified record emits no additional readiness event. A first verification pin with
no prior Pending also emits no wake: the same source was already observable, so writing its first
pin is bookkeeping rather than observer readiness. Both readiness edges enter the same publication
callback and release-latch/pending-front kickoff path, rather than relying on a runtime-key change.

`ObserverAttaches` abstracts the source becoming observable, including this same-key Pending →
Resolved boundary. While only an unresolved/missing binding exists, `observer.attached` remains
false; the eventual readable resolution is the one `ObserverAttaches` action. A first pin of an
already-observable source leaves `observer.attached` true and is not another `ObserverAttaches`
action; it adds no attachment wake. No additional model state is needed for a tuple which cannot
yet build an observer or for an audit pin which leaves observer availability unchanged.
Two actual-owner tests drive
that path through the real restore resolver and the live hook respectively:

- `a_pending_observer_becoming_readable_wakes_after_the_refused_token_finishes`
- `a_hook_verifying_a_pending_observer_wakes_after_the_refused_token_finishes`

Both prove the tuple stays unchanged and the durable `Resolved` count is exactly one, then require
the readiness event to wait for the held turn token and deliver once after release. Before these
two callbacks were added, both tests passed their same-key/one-Resolved assertions and failed with
`ready source stranded; wake_count=0 keys=(0, 0)` (lane evidence `pending-before-f6.log`,
0 passed/2 failed).
Repeating an already-resolved publication adds no wake.

The callback waits for source authority, rechecks that the current source still equals the
published one, and reads its channel mapping under that authority (`rehydration.rs:342-352`).
The existing global `HealthRegistry` supplies that provider/channel's `SharedData` at line 357.
The shared `wake_observed_source` checks for a session ID and an existing transcript which does not
report busy. It captures the existing `turn_finished` latch, checks for a pending owner front,
and only then checks whether the mailbox holds a turn token. If held, it waits on that latch
and rechecks the front before scheduling the existing kickoff. The rehydration wrapper calls its
local fallback only when no global registry is installed (`rehydration.rs:278-293`),
so the common publication and wrapper do not both wake the same source edge. These paths add
no persistent state, timer or retry loop.

The refusal leaves a front claim outstanding. This does not block a later wake: `drive.rs:99`
starts that pass with `stalled = false`, `v2.rs:248` still returns its unattempted front,
and `grant_front`/`claim_turn` replace the old right. The `stalled && front_outstanding()` check
stops the refused pass from immediately handing the same original again. No drive change is needed.

## Results

| Config | Result |
|---|---|
| `InputOwnershipF6Before.cfg` | exit 1; 18 states, 42 steps; `ObserverResumes` breaks |
| `InputOwnershipF6.cfg` | exit 0; 24 states, 57 steps; all six invariants and `NoDeadlock` hold |
| `InputOwnershipF6NoWake.cfg` | exit 1; removing attachment's wake breaks `ObserverResumes` |
| `InputOwnershipF6DoubleWake.cfg` | exit 1; two attachment wake requests break `ObserverAttachmentWakesOnce` |
| `InputOwnershipF6InitiallyAttached.cfg` | holds; an existing observer adds no attachment wake |
| `InputOwnershipF6NoFront.cfg` | holds; attachment with no queued original adds no wake |

The fixed model preserves the original until one paste (`ObserverNoSilentLoss`), never pastes
it twice (`ObserverExactlyOnce`), sends no keys on the refused path (`ObserverRefusalHasNoKeys`),
and leaves no pending original with a readable observer, no turn token and neither a running handoff
nor a wake (`ObserverResumes`). `ObserverNoExtraWake` checks the initially attached and empty-queue cases.

`turnHeld` represents the existing mailbox token, separate from a locally settled handoff. A pending
attachment wake also represents a kickoff waiting on the existing completion latch. It is not a
new production state. `ObserverDriveGrants` requires no held token; `ObserverIdleUnobserved` only
settles the local handoff, and `ObserverTurnReleases` separately releases the token.

The shortest safety counterexample has four actions:

0. A front waits and boot has one owner wake.
1. The owner grants the front and captures no observer.
2. A readable source attaches without waking the owner.
3. The captured unobserved handoff refuses before any key. Its bridge still holds the turn token.
4. The bridge releases that token. No attachment wake remains, so the input is stranded.

The observed production ordering is also a path: grant → refuse → release → source attaches → stranded.
The fixed graph handles attachment before or after local refusal and token release: the pending
kickoff waits on the existing release latch, then the next serialized drive pass regrants and
delivers the front exactly once.
See `observer-before.chart.mmd` and `observer.chart.mmd`.

## Explicit eventual delivery

The invariant checker intentionally rejects `PROPERTY`, so the temporal rule is checked with
an additional scratch config. `ObserverSpec` gives weak fairness to grant, refusal, token release,
submit and attachment. `ObserverEventuallyDelivered` says a pending input with an attached source eventually
has exactly one paste. Plain TLC via the statechart command gives exit 1 before the fix and
for the no-wake mutant, and exit 0 after the fix. The counterexample stutters forever after
"Observer attached, input stranded"; it is a temporal failure, not only the safety proxy above.

Reproduce the safety graph with a new output folder:

```sh
export TLA2TOOLS="$HOME/.local/share/tla2tools/tla2tools-1.7.4.jar"
export JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.codex/skills/statechart/scripts/statechart \
  docs/statecharts/input-single-path/InputOwnership.tla \
  --config docs/statecharts/input-single-path/InputOwnershipF6Before.cfg \
  --view docs/statecharts/input-single-path/observer.view.mjs --out /tmp/f6-before-new
python3 scripts/ci/statecharts_tlc.py
```

For the temporal check, copy `InputOwnership.tla` outside the repository, place this config beside
it and run the same statechart command against those files. Set `Fix = "observerBefore"` for the
pre-fix column, or `Mut = "observerNoWake"` for the wake-deletion mutant.

```tla
CONSTANTS
  Fix = "observerWake"
  Mut = "none"
SPECIFICATION ObserverSpec
INVARIANT ObserverNoSilentLoss ObserverExactlyOnce ObserverRefusalHasNoKeys
INVARIANT ObserverAttachmentWakesOnce ObserverNoExtraWake
PROPERTY ObserverEventuallyDelivered
```

The view hides only `ObserverDone` stuttering. Repeated same-source registrations that do not
change the reader-visible state collapse into self steps. No failure state or delivery is hidden.
The model leaves out busy panes, actual transcript I/O, provider capacity refusals, new source
rotations after delivery, and more than one queued original. Their implementation behavior is
covered by the Rust owner and relay tests. There is no new modelled timer or periodic retry.

## F5/F6 model integration

The merged model preserves `observer` in every legacy action, including `DriveIdle` and
`DriveMeetsRow` when they explicitly update `row`. `InputOwnershipIdleMerge.cfg` executes
the ordinary `DriveIdle` for both an empty previous row and a marked row, so F6's isolated
`ObserverNext` configs cannot hide an unassigned legacy variable. Missing preservation is a
TLC assignment error, rather than an expected semantic counterexample.

Headless busy admission clears a previous marked row before provider input (`DriveStartBusy`);
the ordinary Discord owner busy shortcut has no admission/create step and preserves that row
(`DriveIdleBusyPane`). The full integration probes, mutants, safety/temporal matrix and F7
merge surfaces are recorded in [admission.report.md](admission.report.md).
