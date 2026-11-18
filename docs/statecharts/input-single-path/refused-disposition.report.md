# An input refused for good after its readiness windows ended (#8217)

Initial model: repair of review head `f5be815fdd`. Continuation-copy repair baseline:
`9ebc980b41220de5be352e1be9377dfe1a074a5b` (fix-2).
Fixed settlement implementation code SHA: `b39d22d89f84fe390da4d8dadb254d732484f102`.
Model: `RefusedDisposition.tla`, 6 configs. View: [refused-disposition.view.mjs](refused-disposition.view.mjs).
Charts: [fixed](refused-disposition.fixed.chart.mmd), [before](refused-disposition.before.chart.mmd),
[continuation-copy regression](refused-disposition.retain-continuation.chart.mmd).
Design: [8217-unproven-stamp-settlement.design.md](../../design/8217-unproven-stamp-settlement.design.md) §4.6.

`ReadinessRequiet.tla` ends at "retained, notice posted". This model starts there and follows the settled input
through a daemon restart, the notice drain and a retry of its ticket.

## What the model stands for

| Element | Meaning | Code it stands for |
|---|---|---|
| `head` | the reserved head whose wait, windows included, ended without evidence and without a key | `input_owner/effect.rs` `held()`, `Completion::Refused` |
| `Settle`, `Policy = "Before"` | the retained proven-unsent input: its body stays and a restart may requeue it | `queued_head/v2/retained.rs` `retain_not_sent`, `requeueable` |
| `Settle`, `Policy = "Fixed"` | the refusal for good: members leave the queue, the record keeps identities, fingerprints, the reason and the notice intent | `queued_head/v2/refused.rs` `refuse_not_sent`, `v2_types.rs` `RefusedInput` |
| `continuationBody` | the same input's original retained under `effort_restart.continuation`, independent of the queue copy | `v2/effort_continue.rs:66-94`, `v2_types/effort_restart.rs:46-49` |
| `Post`, `outbox` | the drain posts the owed notice; the persistent dedupe key keeps a repeat to one row | `input_owner/notice.rs` `owed`, `tell`; `queue_io/retained_notice.rs` `enqueue` |
| `Mark`, `told` | the owner records that its notice went out | `retained.rs` `notice_enqueued`, `refused.rs` `refusal_told` |
| `Restart` | recovery requeues before it drains; `posting` is lost with the process | `input_owner/recovery.rs` `recover` |
| `Retry` | the caller retries its ticket: the receipt answers Duplicate and the kept identity fences the sources | `source_admission.rs` `advance_ticket`, `sources.rs` `fenced` |
| `Drive` | a queued input is submitted without the person resending it | the owner drive |

Only the refused input's continuation copy is represented. Other input originals and other open continuations
are outside the state space; matching-scope Rust tests cover that the repair preserves them. The cancelled
continuation copy is not requeued by this model: `pending()` already excludes cancelled intents, so the
regression is persisted body retention, not an assumed requeue bug. `NoBodyOnceSettled` includes both copies.

Not modelled: the 24-hour removal of a told record, a queue clear, and the other proven-unsent retention paths, which
keep their retained-and-requeue contract.

## Results

| Config | Expected | Result |
|---|---|---|
| `RefusedDispositionFixed` | every rule holds, including queue and continuation originals absent | GREEN |
| `RefusedDispositionRetainContinuation` (fix-2 baseline / `retain_continuation` mutant) | `NoBodyOnceSettled` breaks | RED after one settle: queue body absent, original still in restart intent; all other rules hold |
| `RefusedDispositionBefore` | `RefusedStaysOut`, `NoBodyOnceSettled`, `NoKeyAfterEnd`, `NoticeExactlyOnce` break | RED as listed: a restart before the notice puts the input back and the drive submits it |
| `RefusedDispositionRetryAdmits` (mutant: the retried ticket admits its copy) | `RefusedStaysOut`, `NoKeyAfterEnd`, `NoticeExactlyOnce` break | RED as listed |
| `RefusedDispositionNoDedupe` (mutant: the outbox key does not dedupe) | `NoticeAtMostOnce`, `NoticeExactlyOnce` break | RED as listed: post, restart before the mark, post again |
| `RefusedDispositionDrainSkipsRefused` (mutant: the drain reads only retained inputs) | `NoticeExactlyOnce` breaks | RED as listed: the notice is never posted |

`RefusedDispositionBefore` is also the mutant that turns the refusal back into a requeueable proven-unsent input.
The shortest `RefusedStaysOut` counterexample is: settle (retained with its body), restart (requeued). It matches
`recovery.rs` running `requeue_known_not_sent` before the drain on a `retain_not_sent` entry whose notice is not out.

Run: `python3 scripts/ci/statecharts_tlc.py --base f5be815fdd` selected these 5 configs; all matched `expected.tsv` (59.4 s).


## Continuation-copy regression (fix-2)

The shortest path for `NoBodyOnceSettled` is numbered 0 → 1 in the generated report:
0. A reserved effort continuation finishes waiting without evidence.
1. The owner refuses it, removes its queue members, and closes the pending continuation; the original copy stays.

At baseline `9ebc980b41220de5be352e1be9377dfe1a074a5b`, `refused.rs:25` calls
`close_dropped()`, whose `effort_continue.rs:267-272` branch only sets `cancelled = true`.
The original remains in `EffortRestart.continuation`, and serialization keeps it in the queue snapshot.
The fix clears that matching continuation in the same refusal commit. The runtime entrance replay is
owned by the fix-2 Rust regression tests and recorded in the lane report; this model alone is not runtime proof.

| Entrance / consumer (baseline coordinates) | Model action / value |
|---|---|
| `queued_head/v2/effort_continue.rs:66-107`: create and commit continuation original | `Init`, `continuationBody = TRUE` |
| `provider_dispatch/owner_input.rs:228,279`: effort continuation skips handoff and takes the idle start | `head`, the reserved continuation before `Settle` |
| `input_owner/effect.rs:111-124`: proven `Completion::Refused` uses owner refusal | `Settle`, fixed policy |
| `queued_head/v2/refused.rs:18-25`: remove members and close their continuation | `Settle`, both original copies leave together |
| `queued_head/v2/commit.rs:17,75`: commit/recover snapshot | durable variables survive `Restart` |
| `input_owner/recovery.rs:21-26`: requeue proven-unsent retained inputs only | `Restart`, a refusal stays out |
| `queued_head/v2/effort_continue.rs:40-42,149-150`: cancelled operation rejects reuse and owes no continuation | `Restart`, retained continuation body cannot admit by itself |
| `input_owner/notice.rs`: drain an owed refusal and mark its intent | `Post`, `Mark` |

The model represents one no-key final refusal, not the readiness-window timing that produces it;
`ReadinessRequiet.tla` and the readiness tests cover those windows. It also leaves byte-level JSON
serialization, continuation matching against unrelated inputs, and snapshot restoration to the entrypoint tests.
All visible transitions are shown. The tool hides only steps that remain in the same chart state (20 in Fixed
and RetainContinuation; 18 in Before), including repeated restarts that do not change the refusal disposition.

Validation on 2026-11-18: the official `scripts/ci/statecharts_tlc.py` checker ran a scratch root containing
these six configs and the exact source model. All 6/6 configs matched `expected.tsv` in 13.8 s;
it checked safety and the `NoticeExactlyOnce` liveness property separately, with fairness precheck enabled.
Fixed and RetainContinuation each reach 12 states / 27 steps; Fixed has no broken rule, and
RetainContinuation breaks only `NoBodyOnceSettled`. Before still has its four expected violations.
Raw checker log: `i8217/logs/fix-2-model-check.log`; JSON: `fix-2-model-check.json`.

Reproduce after copying this module, its cfgs and view into a scratch folder outside the repository:

```sh
TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar \
JAVA=/opt/homebrew/opt/openjdk/bin/java \
~/.codex/skills/statechart/scripts/statechart /tmp/refused/RefusedDisposition.tla \
  --config /tmp/refused/RefusedDispositionRetainContinuation.cfg \
  --view /tmp/refused/refused-disposition.view.mjs --out /tmp/refused-red-new
```

Expected exit is 1 for RetainContinuation; use `RefusedDispositionFixed.cfg` and a new `--out` folder
for exit 0. The full model and each liveness property remain enabled; no rule was weakened.
