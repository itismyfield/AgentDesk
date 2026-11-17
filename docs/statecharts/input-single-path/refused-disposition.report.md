# An input refused for good after its readiness windows ended (#8217)

Written against the repair of review head f5be815fdd (branch `design/i8217-unproven-stamp`).
Model: `RefusedDisposition.tla`, 5 configs. View: [refused-disposition.view.mjs](refused-disposition.view.mjs).
Charts: [fixed](refused-disposition.fixed.chart.mmd), [before](refused-disposition.before.chart.mmd).
Design: [8217-unproven-stamp-settlement.design.md](../../design/8217-unproven-stamp-settlement.design.md) §4.6.

`ReadinessRequiet.tla` ends at "retained, notice posted". This model starts there and follows the settled input
through a daemon restart, the notice drain and a retry of its ticket.

## What the model stands for

| Element | Meaning | Code it stands for |
|---|---|---|
| `head` | the reserved head whose wait, windows included, ended without evidence and without a key | `input_owner/effect.rs` `held()`, `Completion::Refused` |
| `Settle`, `Policy = "Before"` | the retained proven-unsent input: its body stays and a restart may requeue it | `queued_head/v2/retained.rs` `retain_not_sent`, `requeueable` |
| `Settle`, `Policy = "Fixed"` | the refusal for good: members leave the queue, the record keeps identities, fingerprints, the reason and the notice intent | `queued_head/v2/refused.rs` `refuse_not_sent`, `v2_types.rs` `RefusedInput` |
| `Post`, `outbox` | the drain posts the owed notice; the persistent dedupe key keeps a repeat to one row | `input_owner/notice.rs` `owed`, `tell`; `queue_io/retained_notice.rs` `enqueue` |
| `Mark`, `told` | the owner records that its notice went out | `retained.rs` `notice_enqueued`, `refused.rs` `refusal_told` |
| `Restart` | recovery requeues before it drains; `posting` is lost with the process | `input_owner/recovery.rs` `recover` |
| `Retry` | the caller retries its ticket: the receipt answers Duplicate and the kept identity fences the sources | `source_admission.rs` `advance_ticket`, `sources.rs` `fenced` |
| `Drive` | a queued input is submitted without the person resending it | the owner drive |

Not modelled: the 24-hour removal of a told record, a queue clear, and the other proven-unsent retention paths, which
keep their retained-and-requeue contract.

## Results

| Config | Expected | Result |
|---|---|---|
| `RefusedDispositionFixed` | every rule holds | GREEN |
| `RefusedDispositionBefore` | `RefusedStaysOut`, `NoBodyOnceSettled`, `NoKeyAfterEnd`, `NoticeExactlyOnce` break | RED as listed: a restart before the notice puts the input back and the drive submits it |
| `RefusedDispositionRetryAdmits` (mutant: the retried ticket admits its copy) | `RefusedStaysOut`, `NoKeyAfterEnd`, `NoticeExactlyOnce` break | RED as listed |
| `RefusedDispositionNoDedupe` (mutant: the outbox key does not dedupe) | `NoticeAtMostOnce`, `NoticeExactlyOnce` break | RED as listed: post, restart before the mark, post again |
| `RefusedDispositionDrainSkipsRefused` (mutant: the drain reads only retained inputs) | `NoticeExactlyOnce` breaks | RED as listed: the notice is never posted |

`RefusedDispositionBefore` is also the mutant that turns the refusal back into a requeueable proven-unsent input.
The shortest `RefusedStaysOut` counterexample is: settle (retained with its body), restart (requeued). It matches
`recovery.rs` running `requeue_known_not_sent` before the drain on a `retain_not_sent` entry whose notice is not out.

Run: `python3 scripts/ci/statecharts_tlc.py --base f5be815fdd` selected these 5 configs; all matched `expected.tsv` (59.4 s).
