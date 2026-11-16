# Codex takeover turn-mode confirmation

Code base: `56fad666785bcdb8c28324d019067163db31a257`. Before is the actual base behavior; After describes the fix checked against the regression fixture. The model is not a proof of filesystem or queue implementation.

## Inputs and code mapping

- `original=present`: original path still exists; existing `ended` requires Idle plus turn evidence (`discord/tui_direct_pending_start/turn_retirement/codex.rs:137-150` at base). An archive sibling alongside a present original maps here.
- `original=archived`: original path is missing and a regular, non-symlink `.jsonl.zst` sibling exists. The sibling rule is shared with writer `rotation/readers.rs:571-580` through `codex_tui/rollout_index.rs`. Archive directories/symlinks do not qualify.
- `original=missing`: original absent without qualifying archive; missing alone supplies no completion proof.
- `binding=same/advanced`: no proven successor / an existing `binding_events::codex::source_ahead` proof in the one execution naming the init source. Corrupt history, ambiguous execution, pending-only and same-session history supply no proof; persistent read errors remain Retry and are outside the liveness premise of observable end evidence.
- `legacy`: unfinished synthetic start/inflight row. Start-lock exclusion and successful retirement are represented by a confirmation action enabled only after this blocker clears.
- `pending/executions`: one original in durable input custody; delivered exactly once after confirmation. Provider effects and persistence internals are abstracted and checked separately by the mailbox/pump fixture.

`End`, `Archive` and `Advance` represent source facts, not commands issued by this code. A qualifying archive or a proven successor establishes that the old takeover ended. Restart retains init/source facts and the pending original, while no confirmation has yet been observed. This bounded model has at most one pre-confirmation restart; it does not model perpetual daemon crashes.

## Rules and fairness

`TakeoverEventuallyConfirms`: once the previous takeover ended and no legacy blocker remains, the turn mode eventually confirms. `QueuedInputEventuallyRuns`: a confirmed mode eventually hands on the pending input. `NoEarlyConfirmation`, `NoDuplicateExecution`, `NoSilentLoss` and `NoInputBeforeConfirmation` are safety invariants.

Weak fairness is applied to confirmation, delivery and legacy completion in the actual Spec. End/archive/binding advancement are optional environmental actions. The model assumes source-end evidence remains readable and the existing retry/watch eventually executes; it does not turn arbitrary Retry into Idle.

## Actual TLC results

Command: `python3 scripts/ci/statecharts_tlc.py --root <scratch-model-root> --java /opt/homebrew/opt/openjdk/bin/java --jar ~/.local/share/tla2tools/tla2tools-1.7.4.jar`.

| cfg | Before/base | After/fault removed | Actual result |
|---|---|---|---|
| CodexTakeoverBefore | compressed or advanced original remains unreadable | no fix | RED: TakeoverEventuallyConfirms |
| CodexTakeoverAfter | same inputs | archive and binding proof accepted | GREEN: all four invariants and both properties |
| CodexTakeoverNoArchive | archive predicate removed | binding proof retained | RED: TakeoverEventuallyConfirms |
| CodexTakeoverNoBinding | binding progress ignored | archive proof retained | RED: TakeoverEventuallyConfirms |
| CodexTakeoverRetryAll | Retry unconditionally confirmed | unfinished/open source admitted | RED: NoEarlyConfirmation |

Five configs, 16 TLC runs, 11.5 seconds including fairness prechecks; exact violation sets matched. All old cfg rows are preserved. Chart wrapper additionally checked Before (expected RED) and After (GREEN, 48 reachable states / 140 steps). The 7 chart labels merge internal self-transitions; Rest and redundant restart/internal edges are hidden by the view. The chart does not cover indefinite bad I/O, every binding-log record format, proof validation inside Codex hooks, terminal UI readiness, or an arbitrarily crashing runtime.

Shortest Before counterexample: start with an archived original and no unfinished legacy turn → restart → stay waiting forever. The corresponding implementation replay is `archived_takeover_confirms_after_restart_and_releases_its_queued_input_once`: init records the original, boot reads it, the original closes and is compressed, and confirmation is retried while the same admitted input remains queued. Its base RED / fixed GREEN results are recorded in the implementation lane report.

## Reproduce charts

Copy the model/config/view into a scratch folder outside the repository, then run:

```sh
<statechart-skill>/scripts/statechart <scratch>/CodexTakeover.tla --config <scratch>/CodexTakeoverBefore.cfg --view <scratch>/takeover.view.mjs --out /tmp/i8195-before-new
<statechart-skill>/scripts/statechart <scratch>/CodexTakeover.tla --config <scratch>/CodexTakeoverAfter.cfg --view <scratch>/takeover.view.mjs --out /tmp/i8195-after-new
```

[Before chart](takeover-before.chart.mmd), [After chart](takeover-after.chart.mmd).

## Implementation replay

The base plus the new fixture produced two assertion failures with `Retry(ENOENT)` instead of `Confirmed`, including `TURN_MODE_AWAITED must clear after compression`. The fixed control binary passed all 67 selected tests, including the existing five Claude tests (with real throwaway PostgreSQL), writer archive/restart controls and input-hold controls. Both regression tests also passed with `TMPDIR=/tmp`; no failures or skips and both name-set comparisons matched. Runtime mutants removing archive recognition, ignoring proven binding progress, and confirming arbitrary Retry each failed the corresponding regression/control test (3/3 RED); the original production bytes were restored exactly. Native provider injection and operational observation remain deferred.

The runtime fixture seeds a boot snapshot from the persisted init before it compresses the old rollout. This exercises the restart-origin branch rather than an in-memory late-adoption candidate. Boot adoption reads only init/era metadata (`channel_policy/adoption.rs:456-481`, base), so compression before or after that snapshot construction yields the same selection; confirmation rereads the init on every attempt. The model also starts directly in the compressed-original state.

The archive marker is treated as end evidence by the requested contract. A still-present original, missing archive, archive symlink/directory or unfinished legacy turn remains held. Presence of an archive alone cannot independently prove that an external compressor closed every producer FD; this assumption is not an EOF proof. The writer's stronger output EOF/drain checks remain unchanged.
