# D1b: refused bodies are reasoned and discarded, and an admitted takeover keeps proven debt (#7651)

<!-- doc-path-refs: proposed src/services/discord/recovery_engine/refused_discard.rs src/services/tui_o/writer/late_adopt/codex/prior.rs -->

Design on `6467b53cab3715e4cc096d2693762f3fc4f146ca` (D1a, xh CLEAN). No production code
changes here. The landing unit is the current stack plus D1b.

## User decision being implemented

The 2026-11-17 decision (reconfirmed 2026-11-18) supersedes decision A (2026-11-16):

- An `AdoptionRefused` body is not retained and not sent through Legacy.
- First the same execution is normalized (the first-admission refusal cause is cleared and the
  output judged again). When that fails, the channel gets one reason and the body is discarded;
  the reason may say the answer is still in the terminal session.
- Retention while adoption waits (`AwaitingAdoption`) and delivery after adoption (C2a-C2c) stay.
  When the wait ends in Refused, the same discard-with-reason path runs.

## Entry points

`now` is the current stack head; `D1b` is the designed behavior. Which channels reach these paths
depends on the operator configuration (`tui_o.writer` channels and late adoption); this design
does not read it.

| # | Entry point | Now | D1b |
|---|---|---|---|
| E1 | `src/services/tui_o/cutover/channel_gate.rs:463-464` adoption to identity error | Pending/Deferred give AwaitingAdoption; Refused/Released give AdoptionRefused | unchanged classification; AdoptionRefused becomes a discard trigger, not a hold |
| E2 | `channel_gate.rs:543-565` body claim, late child `body_owner() == Some(false)` | late child Deferred sends through Legacy; undecided child is ChildHeld | unchanged (Deferred is not refused); a Refused child takes E1's discard |
| E3 | `src/services/tui_o/channel_policy/children.rs:37-44` `body_owner` | Deferred late child gives Legacy the body | unchanged |
| E4 | `src/services/discord/tmux_watcher/commit_decisions.rs:8-15` watcher offset | Awaiting and Refused both hold the offset (body stays in the transcript, never posted) | Awaiting holds; Refused posts the reason once and commits past the episode |
| E5 | `src/services/discord/recovery_engine/completion_delivery.rs:88-89` | Awaiting and Refused keep the inflight row without spending retry budget | Awaiting keeps; Refused posts the reason once and clears the row |
| E6 | `src/services/discord/recovery_paths/shared.rs:183-185`, `controller_cutover.rs:162` | both outcomes give `RowDisposition::Preserve` | Awaiting preserves; Refused gives a new discard disposition after the reason |
| E7 | `src/services/discord/recovery_engine/herdr_row_end.rs:239-276` restarted herdr row | `held_body` for Awaiting **and Refused**: the body is archived as retained debt, then the row is released | Awaiting archives as now; Refused posts the reason once and releases the row with no archive |
| E8 | `src/services/discord/inflight/clear_store/herdr_row_end.rs:89-96` snapshot of the exact row | `Ok(true) \| Err(AwaitingAdoption \| AdoptionRefused)` snapshots | `Ok(true) \| Err(AwaitingAdoption)` only |
| E9 | `src/services/discord/inflight/herdr_body_debt.rs:58-85` retained snapshot (`disposition: "retained"`) | written for E7's Awaiting and Refused rows | written for Awaiting only; a discard API removes a snapshot by episode |
| E10 | `src/services/discord/inflight/herdr_body_debt/reconcile.rs`, `src/services/tui_o/writer/herdr_body_debt.rs:253-337` read/wake | retained snapshots are restored and woken until delivered | a snapshot whose channel is now Refused is discarded with one reason instead of woken |
| E11 | `src/services/cluster/intake_router_hook/owner_record/output/debt_recovery/authority.rs:42-56` debt effect rows (0195) | inserted `ready` under a debt session | a `ready` effect of a discarded episode is deleted with its grants in one custody-locked transaction; `reserved`/`confirmed` effects are never discarded (an effect may already be posted) |
| E12 | `src/services/discord/runtime_bootstrap/o_writer_host/adoption_wake.rs:23` boot debt wake | wakes retained debt channels | also settles Refused snapshots through E10 |
| E13 | `src/services/discord/turn_bridge/terminal_outcome_delivery/foreign_terminal_handoff.rs:271` `preserve_or_publish` | preserves a foreign terminal snapshot for later publish | stops preserving a Refused episode, posts the reason, scrubs the stored body fields (the D2 paragraph in `herdr-body-hold.design.md`) |
| E14 | `src/services/tui_o/channel_policy/adoption.rs:15-17` `#[cfg(test)] mod output_disposition` | fresh disposition, normalization and reason receipts compiled for tests only | installed: first fresh output normalizes, then adopts or posts one reason and discards |
| E15 | `src/services/tui_o/writer/late_adopt/codex/normalization.rs`, `startup_dialog/normalize.rs`, `codex_tui/host_input/trust.rs`, `session_host/tmux_host.rs:66-74`, `platform/tmux.rs:641-656` | test-only (skip-pin `scripts/test_only_module_skip_pin.d/mac-mini-7651-d1a.txt`) | production; the skip-pin rows are removed by the census sync |
| E16 | `src/services/discord/outbound/tui_rejection_notice.rs:98-150` `deliver`/`reconcile` | test-only notice | the single reason sender for E4-E14, keyed by `src/db/tui_output_disposition.rs` |
| E17 | continuation (the D1b carry patch: a `normalization::continuation` child and `Output::adopt_resumed`) | not on the branch | resumed output under the same normalization and reason contract |
| E18 | `src/services/tui_o/writer/late_adopt/codex.rs:302-316` admitted takeover | commits `[B@0]` only, as main | owes proven-undelivered earlier rollouts; never blocked by a delivered or unprovable one |
| E19 | `src/services/discord/outbound/o_writer_legacy.rs:117-147` `codex_records` | returns only the current generation's frontier and receipts | also returns earlier generations' receipts (they are already kept in the delivery record) |
| E20 | `src/services/tui_o/writer/late_adopt/codex/delivered.rs:70-115` `generation`/`owner`/`floor` | a receipt proves a floor only when the current generation has one rollout | an earlier generation's receipts prove the floor of that generation's sole rollout |

## Contracts

- (a) Prior evidence: **A fresh admission proves its own rollout. Whether an earlier rollout A
  was delivered or is owed is judged from A's own generation, so moving the generation at the
  admitted launch never erases it: a delivered A is not owed again and never holds the admitted
  rollout back; a proven-undelivered A is owed from its frontier ahead of the admitted rollout;
  an A that can be proven neither way is left out as main does, with an alarm.**
- (b) Fresh and resumed output: **The first output of an admitted execution is normalized once
  within one total budget. It is adopted, or it gets exactly one reason and its body is discarded.
  Nothing of a rejected body is sent through Legacy or kept for replay.**
- (c) Refused bodies: **No body is archived or kept for a channel whose adoption is Refused or
  Released. A body kept while adoption waited is delivered after adoption, or discarded with
  exactly one reason when the adoption is refused. The reason is keyed durably by the body's
  identity, so restarts and retries never post it twice.**

## Lifetimes

- Waiting retention (E7-E12) ends in one of: O adoption and delivery (C2 receipt), or refusal and
  discard with one reason. Released counts as refused.
- A reason intent is persisted before the post and settled only by its exact receipt or proven no
  effect (D1a receipt rules). An uncertain post is reconciled, never posted again.
- The admitted prior judgement lives with the actor run; after a restart it is rebuilt from the
  delivery record (E19/E20), not from memory.

## Migration

- None required. `tui_output_dispositions` (0196) stores `identity` as a JSON object and `reason`
  as unconstrained text, so herdr row identities and an adoption-refused reason fit with a kind
  tag in the identity. Debt effects of a discarded episode are deleted, not given a new state.
- No production database has 0195/0196 or C1 snapshots yet (the stack is unlanded), so there are
  no existing Refused rows to clean up. If review asks for a durable `discarded` effect state
  instead of deletion, the next free number is 0198 (main has 0194 and 0197; this branch 0195 and
  0196).

## State charts

- `WriterAdoptionHerdrRowEnd` gains the D1b discard (`CDiscard`, `CDiscardLate`) with rules
  `NoRefusedArchive`, `ReasonAtMostOnce`, `DiscardVisible` and `RefusedCopyDiscarded`; the current
  code (`C`, `CLate`) breaks the no-archive rule and both settlement properties.
- `CodexAdmittedPrior` checks the admitted prior rules (`ProvenOwedKept`, `DeliveredNotReposted`,
  `UnprovableAlarmed`, `AdmittedEventuallyCommitted`): the landed head and the r10 attempt break
  them, the design keeps them.
- Results are recorded in `docs/statecharts/expected.tsv` and checked with
  `scripts/ci/statecharts_tlc.py`.

## Test plan

- (c) host fixtures for E7/E8/E9: a Refused restarted row posts one reason, archives nothing and
  releases the row; an Awaiting row archives and a later refusal discards with one reason; a
  restart after the reason posts nothing. PG: a `ready` effect and its grants are deleted, a
  `reserved` one is kept. Mutants: archive on Refused, second reason, silent discard.
- (b) the D1a fixtures run with the policy installed (no `cfg(test)` boundary): adopted output
  posts once, rejected output posts one reason and no body on every consumer E4-E6 and E13.
  Mutants: skip normalization, body on rejection, reason twice.
- Continuation: the carry patch's continuation tests with the installed policy.
- (a) Records unit tests for earlier-generation attribution (sole rollout proves, two rollouts do
  not); host fixtures for delivered A (B commits, A not reposted), undelivered A (`[A, B]`),
  unprovable A (B alone, alarm), and the late-child path where Legacy delivers A after the actor's
  last look (the r10 review scenario). Mutants: current-generation-only records, owe every
  earlier rollout, block on an unprovable rollout.

## Slices and size

Estimates of production files / net code lines, measured later with `pr_cap_prod.py`:

| Slice | Content | Files | Net | Production change |
|---|---|---|---|---|
| D1b-1 | (c) and the reason infrastructure: E7-E12, E16, identity kind in `tui_output_disposition`, new child `recovery_engine/refused_discard.rs` | 11 | +360 | Refused herdr bodies are reasoned and discarded instead of retained |
| D1b-2 | (b) fresh activation and consumers: E1, E4-E6, E13-E15, census sync | 13 | +330 | first fresh output normalizes; Refused bodies on watcher/recovery/foreign handoff are reasoned and discarded |
| D1b-3 | (b) continuation: E17 from the carry patch | 3 | +310 | resumed output under the same contract |
| D1b-4 | (a) prior evidence: E18-E20, new child `late_adopt/codex/prior.rs` | 5 | +220 | admitted takeovers owe proven-undelivered earlier rollouts |
| Total | | about 32 | about +1220 (range +1050 to +1450) | |

The total crosses the +1200 stop line, so D1b is split as above. Each slice stays under the cap,
lands behind the previous one, and keeps every giant file to block moves and call swaps: the new
bodies go into the two new child files and existing sub-1000-line modules.

## Review focus

- No path archives or keeps a Refused/Released body: E7, E8, E9 and E13, including a refusal that
  lands between the ownership read and the archive (re-check under the row lock).
- One reason per body identity across restarts, uncertain posts and the herdr and fresh paths.
- Waiting retention still ends: delivery after adoption, or discard after refusal (no permanent
  hold left in E4-E6).
- `reserved`/`confirmed` debt effects are never deleted.
- Earlier-generation attribution only proves a rollout that was the sole bound source of that
  generation; edited-message receipts and receipts written after the next bind keep proving
  nothing (`delivered.rs` owner rule).
- An unprovable earlier rollout never blocks an admitted takeover (no regression from main).
