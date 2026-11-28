# Catch-up cursor over a V2 queue channel

Code SHA `1f61b76313ebc6219a0c4cb6117e25582aa57a1d` is the before snapshot. Code SHA `e2381c0c157411c8cf12e32d3e783fa33b607fd7` implements the fixed `v2_held_sources` partition.

## Model and rules

Three ordered Discord source IDs in one persisted V2 owner queue drive a contiguous phase-1 frontier. Each retained record has an original, a notice flag and a reason/disposition. The model uses the equivalence classes in the entry-point table. It models eligible unknown messages within the 300-second recovery window. The age gate is downstream of known-ID classification.

| Rule | What TLC checks | Fixed result |
|---|---|---|
| `ClosedRetentionDoesNotSeal` | A retained ID with no remaining owner obligation has the passing `V2Delivered` arm. | Pass |
| `NoClosedRetentionReinjection` | Scans add no retained ID with a closed obligation to the reinjection set. | Pass |
| `OwedRetentionStaysOpen` | All owed retained IDs have the `Queued` arm, including cleared unsent records with a true notice flag. | Pass |
| `NoUndeliveredBehindCursor` | The cursor passes only finished inputs. A previously passed told-unsent record can later be explicitly cleared. | Pass |
| `DeliveredInputsAdvanceCursor` | If an ordered prefix stays finished, the cursor eventually reaches it under weak fairness of Scan. | Pass |

## Entry points at the code SHA

The inventory uses `git grep` on `retained_inputs`, `RetainedInput`, `notice.enqueued`, call sites of the writers and readers, and queue control routes. Paths in the table are relative to `src/services/`.

| File and line | Persisted state or consumer | Model |
|---|---|---|
| `turn_orchestrator/queued_head/v2.rs:181–208`, `turn_orchestrator/queued_head/v2/migration.rs:40` | Fresh owner and V1 migration start with empty retention. | Init has no retention |
| `turn_orchestrator/queued_head/v2_types.rs:191–215,254–257`, `turn_orchestrator/queued_head/compat.rs:52–59` | Deserializes the persisted notice and permits older entries with absent disposition. | Restores the corresponding state |
| `turn_orchestrator/queued_head/v2/retained.rs:65–91` | A timed-out or unknown head is retained as disposition-free unconfirmed, notice false. | Unconfirm |
| `discord/queue_io/input_owner/effect.rs:123–142`, `turn_orchestrator/queued_head/v2/retained.rs:206–245` | A proven no-send removes the attempt and retains originals with a retry-required notice owed. | RetainUnsent |
| `discord/router/message_handler/provider_dispatch/start_failure.rs:261`, `turn_orchestrator/queued_head/v2/claude_handoff.rs:185`, `turn_orchestrator/queued_head/v2/start_failure.rs:100–153` | Owner start and handoff failures retain before a key with an attemptless retry-required notice owed. | RetainUnsent |
| `turn_orchestrator/queued_head/v2/source_admission.rs:652–722` | An unpartitionable partially known batch is retained as `OpaqueUnsent`, retry-required notice false. | RetainUnsent |
| `turn_orchestrator/queued_head/v2/sources.rs:174–214` | Test-only legacy parked migration keeps an ambiguous original and creates an unconfirmed notice owed. | Unconfirm |
| `discord/router/message_handler/provider_dispatch/owner_input.rs:361`, `discord/queue_io/input_owner/notice.rs:40–63,82–112`, `discord/queue_io/retained_notice.rs:15–61` | Drain selects false flags, enqueues the deduplicated non-turn outbox message and acknowledges the exact attempt or attemptless identity. | Tell |
| `turn_orchestrator/queued_head/v2/retained.rs:274–300`, `turn_orchestrator/queued_head/v2/start_failure.rs:175–189` | Commits the notice flag. Retain may also owe a failure receipt for a bound origin. | Tell |
| `discord/queue_io/decision_drain.rs:111,142`, `turn_orchestrator/queued_head/v2/decision.rs:70–130` | Decision settlement removes the exact original. It can mark an entry naming the decision without queuing a notice. Production decision originals are synthetic and cannot merge (`discord/queue_io/input_owner/start.rs:21–39`, `discord/queue_io/input_owner/routine.rs:286–301`, `turn_orchestrator/intervention.rs:78–85`). | Removal outside the file projection, flag like Tell |
| `turn_orchestrator/queued_head/v2/control.rs:371–420`, `discord/queue_io.rs:139–175` | Cancel all requeues eligible unsent originals for removal, then sets `cleared` and enqueued on remaining records. | Requeue and Clear |
| `discord/queue_io.rs:99–117`, `turn_orchestrator/queued_head/v2/control.rs:102–111,187–241,301–313` | Cancel one selects queued or requeueable originals. Restart drain leaves the V2 file as custody. | Requeue for eligible unsent, restart preserves state |
| `turn_orchestrator/queued_head/v2/retained.rs:341–458`, `discord/queue_io/input_owner/recovery.rs:15–43` | Restart retries eligible untold proven-unsent entries and isolates queued copies of told originals. | Requeue, or preserve closed retention |
| `discord/runtime_bootstrap/queued_recovery.rs:31–59,76`, `turn_orchestrator/pending_queue_persistence/queued_recovery.rs:51–72,88–100`, `turn_orchestrator/queued_head/compat.rs:25–49` | Boot census uses existing `holds_input`. Unfinished Claude handoffs and source acknowledgements can trigger adoption independently. | Reload or preserve state |
| `discord/router/message_handler/provider_dispatch/owner_input/native_input.rs:187–212`, `turn_orchestrator/queued_head/v2/actor.rs:18–42`, `turn_orchestrator/queued_head/v2/sources.rs:75–112`, `turn_orchestrator/queued_head/v2.rs:210–246` | Adoption validates the snapshot. A fresh owner incarnation may adjust a restored head and keeps retained notices. | Reload; the owner can next Unconfirm, Requeue or Tell |
| `turn_orchestrator/queued_head/v2/commit.rs:17–73,76–116`, `turn_orchestrator/queued_head/v2/origin.rs:44–89,127–145` | Durable snapshot publication and exact read-back preserve the chosen flag. Origin-drop receipts are separate from terminal identities and are drained independently. | Scan reads a committed state |
| `turn_orchestrator/queued_head/v2/retained.rs:96–148`, `turn_orchestrator/queued_head/v2/routine_settlement.rs:169–198` | Exact late delivery writes terminal identities. Routine settlement may withdraw its authorized synthetic original. | Deliver, or synthetic withdrawal outside scope |
| `turn_orchestrator/queued_head/v2/retained.rs:248–270,304–327`, `turn_orchestrator/queued_head/v2/projection.rs:96–109`, `tui_input/human_frame/history/v2.rs:503` | Projection and boot bind closure consume retained evidence. History may still confirm the attempt. | Preserve retention, or Deliver |
| `turn_orchestrator/queued_head/v2/attempt.rs:164–206`, `turn_orchestrator/queued_head/v2/cold_source.rs:143–158`, `turn_orchestrator/queued_head/v2/sources.rs:218–305` | Attempt metadata, pre-key source acknowledgements and history sync use retained originals. | Preserve state |
| `turn_orchestrator/queued_head/v2.rs:575–590`, `turn_orchestrator/queued_head/v2/actor.rs:95–118`, `turn_orchestrator/queued_head/v2/source_admission.rs:338–420,509–528,600–621`, `turn_orchestrator/queued_head/v2/sources.rs:33–72` | Original/source lookup, ticket and bind status, no-send proof and Discord ID fencing read retained originals. | Preserve state |
| `turn_orchestrator/queued_head/v2/effort_continue.rs:253–277`, `turn_orchestrator/queued_head/v2/readiness_hold.rs:264–273`, `discord/queue_io/input_owner/routine.rs:107–134` | Effort continuation barrier, readiness hold cleanup, routine notice projection consume the record. | Preserve state |
| `turn_orchestrator/queued_head/v2/readiness_hold.rs:220–224`, `turn_orchestrator/queued_head/v2/refused.rs:56–77` | Refused attempt/front notices belong to separate persisted lists, already classified as settled evidence. | Settled evidence |
| `discord/commands/control.rs:597,739,794–806`, `discord/catch_up/retry_state.rs:533–576`, `turn_orchestrator/queued_head/v2/clear.rs:39–141,185–245` | Explicit session clear removes retained records, reports discarded sources and fences backlog outside the snapshot. | Clear models waiting control. Physical clear uses a separate fence |
| `discord/commands/stop.rs:84–92,180–213`, `turn_orchestrator/queued_head/v2/control.rs:31–68`, `turn_orchestrator/queued_head/v2/projection.rs:178–214`, `discord/herdr_terminate/retired_wake.rs:46–65` | Stop routes active-turn controls or wakes the owner later; checked terminate purge matches the snapshot and drops only untried waiting items. | Retention remains available |
| `turn_orchestrator/pending_queue_persistence/queued_recovery.rs:113–152`, `discord/recovery_known_ids.rs:149–173`, `discord/catch_up/phase1_sweep.rs:130–146,222–242`, `discord/catch_up/phase2_sweep.rs:134–135` | Both phases merge disk evidence into known IDs; phase 1 writes the contiguous frontier. | Arm, Scan |

## Counterexamples and check results

| Config | Result |
|---|---|
| `CatchUpFrontier.cfg` | Exit 0. All rules pass, 1,875 distinct states. |
| `CatchUpFrontierBefore.cfg` | Counterexample. `ClosedRetentionDoesNotSeal` and `DeliveredInputsAdvanceCursor` fail. |
| `CatchUpFrontierFilterOnly.cfg` | Counterexample. Both before rules fail and `NoClosedRetentionReinjection` also fails. |
| `CatchUpFrontierMain.cfg` | Historical all-V2-queued mutation still breaks liveness. |
| `CatchUpFrontierPendingPass.cfg` | The queued-pass mutation still breaks `NoUndeliveredBehindCursor` and deadlock freedom. |

1. **Before, retain.** A waiting original becomes `unsent`. The production start-failure path saves the original and a false retry-required notice (`start_failure.rs:100–153`).
2. **Before, acknowledge.** The outbox enqueue succeeds. The actor sets the flag (`notice.rs:90–110`, `start_failure.rs:175–189`), so `retained.rs:307–317` now says the owner owes no send. TLC reaches `toldUnsent` and fails the safety rule.
3. **Before, scan.** The old reader at `queued_recovery.rs:129–142` still emits `Queued` for the now-told ID. `classification.rs:126–127` selects Duplicate even beyond the age cutoff. `frontier_evidence.rs:23,56–58` makes it Open. `settled_frontier.rs:18–30,47–50` seals before it and ignores later settlements. The isolated liveness run reaches an equivalent closed-unconfirmed oldest record via Clear with two settled successors, then stutters at cursor 0.
4. **Filter only, scan.** The same told-unsent original has no terminal identity by `retained.rs:287–300` and `origin.rs:61–72`. Omitting it from both arms loses membership. `classification.rs:154–160` reaches Recover within the window. TLC records input 1 in the reinjection set on the first scan.
5. **Fixed, scan.** The told record is still a known ID and has `V2Delivered`. `frontier_evidence.rs:49–55` passes it. Owed source copies still win in the disk reader and pre-existing mailbox arms still win at merge.

The WSL host has Java and `tla2tools-1.7.4.jar`. The `~/.claude/skills/statechart/scripts/statechart` launcher is absent. Raw Java TLC returned 12 for invariant counterexamples and 13 for the isolated liveness counterexample. The before/fixed lane check maps a verified counterexample to status 1. The repository `scripts/ci/statecharts_tlc.py` checked all five configs with their expected verdicts in 11.8 seconds. The Mermaid chart shows the complete one-ID projection from TLC's DOT graph (15 distinct states), with labels from the view and stuttering edges omitted.

## Design boundary

The snapshot proof considers one effective V2 arm per source ID. A simultaneously owed copy, a live mailbox arm, or an earlier Open ID can still block a scan. Requeueability is abstracted as an eligible branch from unsent retention. `V2Delivered` includes resolved no-turn obligations, which is why KnownNotSent or opaque unsent originals can stay retained when the cursor passes. Time-dependent paging, terminal-history expiry, and physical clear/delete fences are handled by the caller paths in the table.
