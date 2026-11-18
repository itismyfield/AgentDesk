# D1b: refused bodies are reasoned and discarded, and an admitted takeover keeps proven debt (#7651)

<!-- doc-path-refs: proposed src/services/discord/recovery_engine/refused_discard.rs src/services/tui_o/writer/late_adopt/codex/prior.rs src/services/tui_o/channel_policy/adoption/episode_key.rs -->

Design on `6467b53cab3715e4cc096d2693762f3fc4f146ca` (D1a, xh CLEAN). No production code
changes here. The landing unit is the current stack plus D1b. Revision 2 answers the design review
(P1-1 and P2-1..P2-5); the four slices and deletion without a migration stay (the next free number,
if one is ever needed, is 0199).

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
| E11 | `src/services/cluster/intake_router_hook/owner_record/output/debt_recovery/authority.rs:42-56` debt effect rows (0195); `debt_recovery.rs:208` `no_request` returns a reserved effect to `ready` but keeps its `attempt_id`, the `no_effect` attempt and the revoked grant | inserted `ready` under a debt session | a discarded episode's effect is deleted in one custody-locked transaction, in this order: check the effect is `ready` and every attempt of it is `no_effect`; clear `effects.attempt_id`; delete its `no_effect` attempts; delete its grants; delete the effect. `reserved`/`confirmed` effects and any unsettled attempt are never deleted. The 0196 reason row stays |
| E12 | `src/services/discord/runtime_bootstrap/o_writer_host/adoption_wake.rs:23` boot debt wake | wakes retained debt channels | also settles Refused snapshots through E10, and resumes 0196 reason intents that no longer have a row (first post when no attempt exists, reconcile otherwise) |
| E13 | `src/services/discord/turn_bridge/terminal_outcome_delivery/foreign_terminal_handoff.rs:271` `preserve_or_publish` | preserves a foreign terminal snapshot for later publish | stops preserving a Refused episode, posts the reason, scrubs the stored body fields (the D2 paragraph in `herdr-body-hold.design.md`) |
| E14 | `src/services/tui_o/channel_policy/adoption.rs:15-17` `#[cfg(test)] mod output_disposition` | fresh disposition, normalization and reason receipts compiled for tests only | installed: first fresh output normalizes, then adopts or posts one reason and discards |
| E15 | `src/services/tui_o/writer/late_adopt/codex/normalization.rs`, `startup_dialog/normalize.rs`, `codex_tui/host_input/trust.rs`, `session_host/tmux_host.rs:66-74`, `platform/tmux.rs:641-656` | test-only (skip-pin `scripts/test_only_module_skip_pin.d/mac-mini-7651-d1a.txt`) | production; the skip-pin rows are removed by the census sync |
| E16 | `src/services/discord/outbound/tui_rejection_notice.rs:98-150` `deliver`/`reconcile` | test-only notice | the single reason sender for E4-E14, keyed by `src/db/tui_output_disposition.rs` |
| E17 | continuation (the D1b carry patch: a `normalization::continuation` child and `Output::adopt_resumed`) | not on the branch | resumed output under the same normalization and reason contract |
| E18 | `src/services/tui_o/writer/late_adopt/codex.rs:302-316` admitted takeover, and the installed normalizer `src/services/tui_o/channel_policy/adoption/output_disposition.rs:129-200` `finish_first` | commits `[B@0]` only, as main; `finish_first` adopts only a single-source init (`:169`) | owes proven-undelivered earlier rollouts; never blocked by a delivered or unprovable one; `finish_first` adopts exactly the takeover's own proved owed list followed by B at byte 0 |
| E19 | `src/services/discord/outbound/o_writer_legacy.rs:117-147` `codex_records` | returns only the current generation's frontier and receipts | also returns earlier generations' receipts (they are already kept in the delivery record) |
| E20 | `src/services/tui_o/writer/late_adopt/codex/delivered.rs:70-115` `generation`/`owner`/`floor` | a receipt proves a floor only when the current generation has one rollout | an earlier generation's receipts prove the floor of that generation's sole rollout |

## Contracts

- (a) Prior evidence: **A fresh admission proves its own rollout. Whether an earlier rollout A
  was delivered or is owed is judged from A's own generation, so moving the generation at the
  admitted launch never erases it: a delivered A is not owed again and never holds the admitted
  rollout back; a proven-undelivered A is owed from its frontier ahead of the admitted rollout;
  an A that can be proven neither way is left out as main does, with an alarm.**
- (b) Fresh and resumed output: **The first output of an admitted execution is normalized once
  within one total budget. It is adopted, or its body is discarded with one durable reason intent
  that is posted at most once. Nothing of a rejected body is sent through Legacy or kept for replay.
  An admitted takeover that owes earlier rollouts is adopted when its init is exactly the owed list
  it proved, in order and from the proved frontiers, followed by B at byte 0 with an empty prefix
  hash; origin, policy parent, first proof and identity checks stay, under the candidate lock.**
- (c) Refused bodies: **No body is archived or kept for a channel whose adoption is Refused or
  Released. A body kept while adoption waited is delivered after adoption, or discarded with
  one reason when the adoption is refused. The final archive eligibility check and the durable copy
  publish run under one candidate guard (lock order: the inflight row lock, then the candidate
  guard), so no Refused/Released transition of that candidate lands between them: an archive that
  checked first makes the refusal wait until the copy is published (the refusal then discards it),
  and a refusal that completed first leaves no copy.**
- Episode key and first reason: **Every consumer keys a rejected body by one canonical episode key,
  the provider, the channel and the digest of the admitted input's turn nonce, which does not
  change with the consumer, the owner or the generation. A key that already holds a reason keeps
  its first reason (as `Output::restore` does); a later consumer never writes a second reason or
  re-adopts or re-archives that body. An existing reason intent or receipt is terminal for that
  body episode: an observing or archiving consumer discards it reusing that reason and never copies
  or posts that body; new admissions on the channel are not gated by it. A view with no turn nonce keeps a kind-tagged key of its own
  and is the one place a second reason remains possible.**
- Reason outcome: **Discarding first persists the body-free reason intent; only then is the body,
  offset, row or debt released. A verified created message settles the intent; a proven no-effect
  frees the attempt for another post; an uncertain or timed-out post, or a restart between reserve
  and post, keeps the attempt and is only reconciled, never posted again. If the intent cannot be
  persisted, nothing is released and the consumer retries. An uncertain reason is never a body
  delivery or input success. The reason is posted exactly once when its post is verified and at
  most once otherwise. The durable intent is resumed on its own: once the body, offset, row and
  debt are gone, the 0196 row alone drives the post, so an intent with no attempt is posted on the
  next start (E12 sweep through E16) and one with an attempt is only reconciled.**

## Lifetimes

- Waiting retention (E7-E12) ends in one of: O adoption and delivery (C2 receipt), or refusal and
  discard with one reason. Released counts as refused.
- A reason intent is persisted before the post and settled only by its exact receipt or proven no
  effect (D1a receipt rules). An uncertain post is reconciled, never posted again.
- Delivered evidence for an earlier rollout comes back from its generation's receipts (E19/E20).
  Owed evidence (source, its generation, the proved frontier and prefix hash, and the baseline order
  and send count it was judged under) is written durably by `prior.rs` when the actor judges it and
  is removed only when the init that includes it is durable or a later receipt proves delivery. A
  generation move or a restart never drops it, a missing receipt is never read as unsent, and an
  earlier rollout with neither kind of evidence is unprovable: left out as main does, with an alarm.

## Migration

- None required. `tui_output_dispositions` (0196) stores `identity` as a JSON object and `reason`
  as unconstrained text, so herdr row identities and an adoption-refused reason fit with a kind
  tag in the identity. Debt effects of a discarded episode are deleted, not given a new state.
- No production database has 0195/0196 or C1 snapshots yet (the stack is unlanded), so there are
  no existing Refused rows to clean up. If review asks for a durable `discarded` effect state
  instead of deletion, the number assigned for this stack is 0199 (main has 0194 and 0197; this
  branch 0195 and 0196).

## State charts

- `WriterAdoptionHerdrRowEnd` D1b discard: the archive step re-reads ownership (`ArchiveRechecks`)
  and makes the copy in one atomic step, which the code meets by holding the candidate guard from
  that check through the publish, and `NoRefusedArchive` is observed when the copy is made; an
  episode that already holds a reason is closed (`EpisodeClosed`): Observe and Archive discard it
  reusing that reason and `OCreate` is disabled for it, and `BodyAfterAdoption` asks delivery only
  of a body that was not explicitly discarded; reason work does not need the row (`ReasonWorkable`); the reason is a durable intent
  (`reasonState`) that releases the body and is then posted (`ReasonCreated`) or left uncertain
  (`ReasonUncertain`); an episode that another consumer already gave a reason (`priorFresh`) keeps
  it. Rules: `NoRefusedArchive`, `ReasonAtMostOnce`, `DiscardVisible`, `FirstReasonKept`,
  `ReasonNotBodyReceipt`, `ReasonedBodyNotCopied`, `ReasonedBodyNotPosted`, `RefusedCopyDiscarded`,
  `ReasonEventuallySettles`. The current code breaks the no-archive rule and both settlement
  properties; the design keeps all of them; mutants for no recheck, retaining, reposting an
  uncertain reason, holding the body until the receipt, a consumer-local key, reading an uncertain
  reason as delivery, a silent discard, no terminal gate for an episode that holds a reason (copied
  and posted again) and reason work that needs the row (`ReasonEventuallySettles`) each break one.
- Scope of that GREEN: D1b cfgs run with `CapMode` false (the capability reservation, post, stop
  and transfer actions are checked by the existing Cap cfgs only), the reason post is one step after
  the intent, and Released, cancel, terminate, clear and stop are not composed with the discard.
  Those lifecycle ends reach the discard through E1 (Released), E5/E6 (inflight row ends) and E13
  (foreign custody), and their composition is left to the slice tests.
- `CodexAdmittedPrior` keeps A's truth apart from the evidence held across the generation move, one
  restart and a late Legacy receipt, then runs the installed normalizer on the commit. The stored
  init (`obsInit`) is kept apart from the proved owed list, and `SkewedCommit` reaches the takeover's
  own commit (identity and B proof right) with a stored list that differs from the proof, which the
  normalizer must leave ExistingEffectPending. Rules:
  `ProvenOwedKept`, `DeliveredNotReposted`, `NoAbsenceAsOwed`, `UnprovableAlarmed`,
  `AdoptedMatchesProof`, `AdmittedEventuallyCommitted`, `OwnCommitAdopted`. The landed head breaks
  `ProvenOwedKept`; the design keeps all; mutants for the singleton finish check (which also adopts
  a skewed list), dropping only the identity check, dropping only the list comparison, dropping evidence on the generation move or a restart, reading absence as owed, ignoring
  a late earlier-generation receipt and blocking on an unprovable rollout each break one.
- Results are recorded in `docs/statecharts/expected.tsv` and checked with
  `scripts/ci/statecharts_tlc.py`.

## Test plan

- (c) host fixtures for E7/E8/E9: a Refused restarted row posts one reason, archives nothing and
  releases the row; an Awaiting row archives and a later refusal discards with one reason. A hook
  after the last ownership check and before the publish races `release()` or a host guard drop: an
  archive that checked first holds the refusal until the copy is published, a refusal that
  completed first leaves zero copies, and releasing the candidate guard before the publish is a
  mutant. Reason resume: intent persisted and row released, restart before reserve posts once with
  no row; restart after reserve posts nothing and only reconciles. Restarts right after the
  intent, after reserve before the post, and after the post before the receipt: no body is kept
  again, no second post, a restored receipt adds none, only a proven no-effect posts again. PG: an
  effect that went reserve, `no_request`, `ready` is deleted with its `no_effect` attempt and grants
  while its 0196 reason row stays; a `reserved` effect is kept. Mutants: archive without the
  recheck, grant-first deletion (foreign-key RED), second reason, repost on uncertain, uncertain
  read as delivery, silent discard.
- Episode key: a fresh reason intent or receipt, then the same episode restored through herdr and
  foreign custody at once: same key, first reason kept, no second post, no re-archive, the old body
  posted zero times; a different
  input window gets a different key.
- (b) the D1a fixtures run with the policy installed (no `cfg(test)` boundary): adopted output
  posts once, rejected output posts one reason and no body on every consumer E4-E6 and E13.
  Mutants: skip normalization, body on rejection, reason twice.
- Continuation: the carry patch's continuation tests with the installed policy.
- (a) Records unit tests for earlier-generation attribution (sole rollout proves, two rollouts do
  not); host fixtures for delivered A (B commits, A not reposted), undelivered A (`[A, B]`),
  unprovable A (B alone, alarm), the late-child path where Legacy delivers A after the actor's last
  look (the r10 review scenario), A judged owed then a generation move and a restart before the
  init, and a late earlier-generation receipt in that window. Installed policy: the success fixture
  of `src/services/discord/tmux_watcher/terminal_relay_plan_tests.rs:1340-1439` (normalizer, then
  the body claim) with a proven-owed A: `[A@frontier, B@0]`, normalizer Adopted, `OwnedByO`, A and B
  once, no reason. Mutants: current-generation-only records, owe every earlier rollout, block on an
  unprovable rollout, drop evidence on the generation move, read absence as owed, ignore a late
  receipt, keep the singleton `finish_first`, drop only the identity check, drop only the list
  comparison (under the same B proof: an unproved A source inserted, an A frontier or prefix hash
  changed), a moved B start or a wrong source or parent.

## Slices and size

Estimates of production files / net code lines, measured later with `pr_cap_prod.py`:

| Slice | Content | Files | Net | Production change |
|---|---|---|---|---|
| D1b-1 | (c) and the reason infrastructure: E7-E12, E16, the canonical episode key (new child `channel_policy/adoption/episode_key.rs`), the reason outcome API and deletion order (new child `recovery_engine/refused_discard.rs`) | 13 | +440 | Refused herdr bodies are reasoned and discarded instead of retained; 0196 rows start being written |
| D1b-2 | (b) fresh activation and consumers: E1, E4-E6, E13-E15, census sync | 13 | +340 | first fresh output normalizes; Refused bodies on watcher/recovery/foreign handoff are reasoned and discarded |
| D1b-3 | (b) continuation: E17 from the carry patch | 3 | +310 | resumed output under the same contract |
| D1b-4 | (a) prior evidence and the multi-source finish check: E18-E20, new child `late_adopt/codex/prior.rs` with the durable owed evidence, `finish_first` | 8 | +340 | admitted takeovers owe proven-undelivered earlier rollouts and are adopted with them |
| Total | | about 37 | about +1430 (range +1250 to +1650) | |

The total is above +1200, so D1b lands as the four slices above (approved split). Each slice stays under the cap,
lands behind the previous one, and keeps every giant file to block moves and call swaps: the new
bodies go into the two new child files and existing sub-1000-line modules.

## Review focus

- No path archives or keeps a Refused/Released body: E7, E8, E9 and E13, including a refusal that
  lands between the ownership read and the archive (final check and copy publish under one candidate
  guard).
- One reason per body identity across restarts, uncertain posts and the herdr and fresh paths.
- Waiting retention still ends: delivery after adoption, or discard after refusal (no permanent
  hold left in E4-E6).
- `reserved`/`confirmed` debt effects are never deleted.
- Earlier-generation attribution only proves a rollout that was the sole bound source of that
  generation; edited-message receipts and receipts written after the next bind keep proving
  nothing (`delivered.rs` owner rule).
- An unprovable earlier rollout never blocks an admitted takeover (no regression from main).
- `finish_first` accepts only the takeover's own proved owed list followed by B; an unrelated init
  that merely contains B stays ExistingEffectPending.
- Body release waits for the durable reason intent, not for the receipt; an uncertain reason is
  never reposted and never counts as delivery.
- One canonical episode key across fresh, herdr and foreign views; the first reason is kept.
- Owed evidence survives the generation move and a restart; a missing receipt is never read as
  unsent.
