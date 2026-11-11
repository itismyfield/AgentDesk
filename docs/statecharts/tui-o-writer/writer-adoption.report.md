# Writer adoption and the TUI Legacy body (#7651 stage 1)

Code baseline: `13f9acdd9a0f5e3aa500d20cd1b9a31a91e7ced4`; line numbers below are at this change's head, which moves `channel_policy.rs`, `channel_policy/adoption.rs` and `cutover/channel_gate.rs`. The stage-1 behavior is plan r2 with its r2.1 corrections (lane `report-p1-2.md`); `Fix = "gateOnly"` is that plan without the warm takeover from the Legacy delivery frontier.

One TUI-bound channel carries one turn; one non-TUI channel in the same config carries a headless turn. The model checks who posts the TUI turn's body, when its row and the watcher dispatch may be settled, and whether a held turn is eventually answered.

## Values and where they come from

| Variable | Values | Code |
|---|---|---|
| `sel` | default, explicitOff, listExcl, lateOff, defaultOff | `shadow/tap.rs:61-67`, `channel_policy.rs:73-134` (implicit default off when no cluster ids or a binding is rejected) |
| `site` | home, foreign | `channel_policy/adoption.rs:384-390`; off the home `seeded` makes no candidate (`channel_policy.rs:316-324`) |
| `root` | runtime root present or not | `channel_policy/adoption.rs:419-421` reads every channel as `Held` without a root |
| `exec` | none, permit, unverifiedHeld | `channel_policy.rs:446-513`, `ownership/execution.rs:72-78`, gate `cutover/channel_gate.rs:461-471` |
| `bound` | none, fresh, withOutput | `writer/late_adopt/codex.rs:205-207` (no new bind, Retry), `late_adopt/codex/prefix.rs:205-213` with `codex.rs:229-230` (bound rollout already has output, Skip) |
| `store` | none, init, damaged | `channel_policy/adoption.rs:414-439` |
| `adopt` | None, Pending, Deferred, Committed, Held, Released, Refused | `channel_policy/adoption.rs:16-28`; `Refused` is the stage-1 refusal (plan r2 G5) |
| custody | another turn's row, pending start or open intake; the held turn's own row | `runtime_bootstrap/o_writer_host.rs:306-339`, `writer/deferred.rs:89-92`; the kinds block alike, so the model keeps whose custody it is |
| `attempts` | 0..3, tmux absent | `discord/inflight.rs:341-344`, `recovery_paths/shared.rs:156-171` |

Actions map to the bridge tick and terminal (`turn_bridge/stream_tick.rs:389-397`, `terminal_outcome_delivery.rs:440-453`), restart recovery (`recovery_engine/completion_delivery.rs:117-151`), the watcher dispatch finalizer (`tmux_watcher.rs:2351-2392`), the takeovers (`late_adopt.rs:182-186`, `late_adopt/codex.rs:203-310`), the stale end-of-file handoff (`writer/deferred.rs:26-29`), refusal (`writer/host.rs:659-678`, `host/first_init.rs:79-80`, `writer/activation.rs:223-225`) and headless delivery (`turn_bridge/headless_delivery.rs:588-608`).

A restart keeps the turn: a running turn's bridge is restored and judges again (`restore_inflight.rs:2378`); a held row waits for the once-per-boot recovery.

## Rules

- `NoLegacyBodyOnTui`: Legacy posts no body on the TUI channel.
- `AnswerAtMostOnce`: O and Legacy together post the turn at most once.
- `UnadoptedNotOwned`: without an O store the gate never answers that O owns the body.
- `RowClearedHasEvidence`, `NoEarlyDispatch`: the row and the watcher dispatch settle only after a post, or while the O store owns the turn's range unposted.
- `HeldBodyNotSkipped`: a commit while the turn is held starts at the Legacy delivery frontier, not at the end of file.
- `OtherCustodyBlocks`: no commit while another turn's custody is open.
- `HeadlessKept`: the non-TUI turn is never held and posts its body.
- `HeldTurnAnswered` (PROPERTY): a held turn on an adoptable channel (home, default selection) is posted by O unless a refusal happened.
- `AdoptionSettles` (PROPERTY): a Deferred adoption with a turn on an adoptable channel commits unless a refusal happened.
- `RowEventuallySettled` (PROPERTY): a turn O posted gets its row settled.

`rejected` records any refusal: the `Reject` action, and a seed that is `Refused` (no runtime root, or an unverified execution view without a store). `Reject`, `Input`, `Restart`, the stale handoff and the watcher finalizer carry no fairness; writer, recovery and turn-end actions are weakly fair.

## Results (TLC 1.7.4, `scripts/ci/statecharts_tlc.py`)

| Config | Fix, Mut | Breaks |
|---|---|---|
| Main | main | `NoLegacyBodyOnTui`, `UnadoptedNotOwned`, `RowClearedHasEvidence`, `NoEarlyDispatch`; liveness `AdoptionSettles` |
| GateOnly | gateOnly | `HeldBodyNotSkipped`, `RowClearedHasEvidence`; liveness `HeldTurnAnswered`, `AdoptionSettles` |
| Stage1 | stage1 | none |
| OwnedMut | stage1, a held body read as owned | `UnadoptedNotOwned`, `RowClearedHasEvidence` |
| UnadoptedOwned | stage1, refusal kept as Held and the execution view answering owned | `UnadoptedNotOwned`, `RowClearedHasEvidence` |
| ExemptAll | stage1, every custody exempt | `OtherCustodyBlocks` |
| WatcherNoGuard | stage1, dispatch without evidence | `NoEarlyDispatch` |
| BudgetMut | stage1, waiting and refusal counted in the recovery budget | `RowClearedHasEvidence` |
| HeadlessMut | stage1, the non-TUI turn held too | `HeadlessKept` |

Main's violations are code paths on the baseline. Legacy posts the Deferred turn (the 11-08 trace: Deferred seed, a bound rollout with output skipped, warm input, Legacy W14/W10). Without a runtime root, or with an execution view and no store, the gate answers owned (`adoption.rs:419-421`, `channel_gate.rs:461-471`), so the bridge commits and the watcher settles with nothing posted. GateOnly holds the body but the warm channel never commits, or commits at end of file and skips the held turn.

## Left out, and how it relates

- Codex turn-mode confirmation (`cutover.rs:81-91`) waits for input after a commit; it does not change adoption.
- Herdr row-end and restart notices are not bodies; a held row is kept as on the restart path.
- Watcher streaming (W02) uses the same gate; only the terminal and dispatch are modelled.
- The output lock and delivery lease (`terminal_outcome_delivery.rs:735-757`) prevent duplicates; a held body takes no lease.
- Turn presence keeps a held turn Busy; that is `turn-presence`'s `EventuallyStableBusy`.
- Census instrumentation (denominators, end evidence) is an observation contract checked by Rust tests.
- tmux presence: with tmux alive the recovery budget never clears a row, so only the absent case is modelled.

## S18 purpose-specific port assumption check (design only)

Code SHA `92ef5b00b205679965c24c5c017f7237e8b5d57f`. Current code was checked at
`src/services/tui_o/channel_policy/adoption.rs:420,428` (Held without a root, stored commitment),
`src/services/tui_o/cutover/channel_gate.rs:460,466` (execution view remains owned/held), and
`src/services/tui_o/writer/host.rs:530` (managed writer start).
The proposed admission/lookup ports do not change adoption, O/Legacy body ownership, warm takeover,
or body-census/terminal settlement. Outbox/turn_bridge/relay_recovery callers are excluded from this slice.
No new WriterAdoption action or config is needed for an unchanged assumption.

The admission/authority projection is in the existing
[InputOwnershipAdmission model report](../input-single-path/InputOwnershipAdmissionPorts.report.md).
`SC_RUN=mac-needed`: no TLC execution in this WSL leg, and no new GREEN claim for the historical results above.
