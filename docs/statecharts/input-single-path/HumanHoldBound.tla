---- MODULE HumanHoldBound ----
\* #8430 projection: one human owner front held behind another turn's leftover lease/row.
\* Refines HerdrBusyInput's "held" phase (RetryHeld fires only when Eligible) for the case
\* where the blocking evidence is a mailbox cancel_token / inflight row whose turn has ended.
\* Code: HEAD a53f299560. Value ranges come from the entry-point table in
\* docs/design/8430-human-hold-bound.design.md (rows cited as E#/C#/D#). Design w2.
\*
\* Hold    : owner_input.rs:249-290 owner_drive has_active_turn (or a non-Managed row,
\*           turn_deliver.rs:128) -> hand_busy :407 -> owned_busy.rs:33-96 running: Busy
\*           injects; anything else holds (busy_source_unavailable). Idle -> HoldKind Observe
\*           (no deadline), Held -> Readiness (10 s due). held_watch.rs:18 LOOK 2 s re-drives.
\* Cleanup : the Current paths that never release this shape are abstracted away. Target:
\*   lease shape  OperatorRelease CAS (turn_lease.rs:148-336) -> mute settlement
\*                (turn_finalizer.rs:288) -> completion admission QueueEligible
\*                (completion_admission.rs:85) -> drain wakes the owner.
\*   row shape    captured-row clear -> narrow mute ledger settlement -> transition lease
\*                released -> owner deadline tick wake (drive.rs:139-171); no MailboxReleased
\*                (episode.rs:32 returns without a token).
\* Scenario:
\*   "incident"   11:48: old turn ended, terminal never submitted, token+row remain, Idle.
\*   "running"    old turn really runs past the bound; the screen looks idle between tools.
\*   "draft" / "modal" / "imageDraft"  old turn ended; a text draft / a modal / an image-only
\*                draft (Codex Screen.attachments with Composer::Empty) is on the screen.
\*   "flicker"    incident, observations fail intermittently, then settle on Idle.
\*   "late"       incident, the lost old terminal arrives after the bound released it.
\*   "restart"    incident, one dcserver restart while held (clock/streak are memory).
\*   "clearDrop"  #8343: !clear, unpinned source while turn 1 runs; a 2-member unit waits.
\*   "idleDue"    R2: no token; an idle-path readiness hold (person draft) on a 2-member unit.
\*   "rowOnly"    #8372: headless routine row (Managed, no token, no pane); run terminal and
\*                its producer exit received.
\*   "runLive"    headless row whose run is terminal but whose producer is still alive.
\*   "rowPane"    watcher self-heal ExternalInput row (liveness.rs:462-473), no token, pane Idle.
EXTENDS Naturals
CONSTANTS Policy,    \* "current" (HEAD a53f299560) | "target" (design w2)
          Scenario,
          Mutant     \* "None" | "NoIdle" | "NoTranscript" | "NoScreenGate" | "NoAttachments"
                     \* | "NoCleanup" | "NoDedupe" | "NoLedgerFence" | "NoR1" | "FirstOnly"
                     \* | "RunOnly" | "NoWake" | "GateTimeout" | "ManagedOnly"
Bound == 2           \* abstract 3 minutes: held re-drives before the bound may fire
Streak == 2          \* consecutive positive witnesses required
VARIABLE s
vars == <<s>>

OldRuns == Scenario \in {"running", "clearDrop"}
Headless == Scenario \in {"rowOnly", "runLive"}
Init == s = [front |-> "queued",       \* queued | held | stalled | sent | dropped | done
             attempted |-> FALSE,
             unit |-> IF Scenario \in {"clearDrop", "idleDue"} THEN 2 ELSE 1,
             token |-> IF Headless \/ Scenario \in {"rowPane", "idleDue"} THEN "none" ELSE "old",
             row |-> IF Scenario = "idleDue" THEN "none" ELSE "old",
             rowKind |-> IF Scenario = "rowPane" THEN "external" ELSE "managed",
             old |-> IF OldRuns THEN "running" ELSE "ended",
             producer |-> IF OldRuns \/ Scenario = "runLive" THEN "running" ELSE "ended",
             run |-> IF Headless THEN "terminal" ELSE "none",
             terminal |-> IF OldRuns THEN "pending" ELSE "lost",
             ledger |-> "pending",
             settle |-> "none",        \* bound settlement: none | mute | reacted
             screen |-> CASE Scenario \in {"draft", "idleDue"} -> "draft"
                          [] Scenario = "modal" -> "modal"
                          [] Scenario = "imageDraft" -> "imageDraft"
                          [] Headless -> "none"
                          [] OldRuns -> "working"
                          [] OTHER -> "idle",
             transcript |-> CASE OldRuns -> "open"
                              [] Headless -> "none"
                              [] OTHER -> "ended",
             source |-> ~OldRuns,
             due |-> "none",           \* Readiness due: none | armed | expired
             clock |-> 0, streak |-> 0,
             kick |-> FALSE,           \* QueueEligible completion kick (memory)
             tickOwed |-> FALSE,       \* owner deadline tick armed by a row-shape release
             sends |-> 0,
             released |-> FALSE,
             boundBad |-> FALSE,
             notice |-> "none",        \* none | bound | generic | first | identified
             newCleared |-> FALSE,
             flickers |-> 0,
             up |-> TRUE, restarts |-> 0]

\* ---- observation (target witness; leaf readers in the design's Idle table) ----
\* Pane hosts: capture read succeeded and the common screen gate holds (Codex: !modal,
\* !queued, !attachments, Composer::Empty from busy_inject::screen::read), and the
\* transcript/execution evidence ended (judge_in / read_native_end / completion scan).
ScreenOK == \/ Mutant = "NoScreenGate"
            \/ s.screen = "idle"
            \/ (Mutant = "NoAttachments" /\ s.screen = "imageDraft")
TranscriptEnded == s.transcript = "ended" \/ Mutant = "NoTranscript"
\* Headless routine row: captured row bound to its run/attempt, explicit terminal run status
\* AND the producer exit (or its receipt). Lookup failure or mismatch is Unknown.
HeadlessEnded == s.token = "none" /\ s.run = "terminal"
                 /\ (s.producer = "ended" \/ Mutant = "RunOnly")
Witness == IF s.screen = "none" THEN HeadlessEnded ELSE ScreenOK /\ TranscriptEnded
NextStreak == IF Witness THEN IF s.streak < Streak THEN s.streak + 1 ELSE s.streak ELSE 0
\* Ground truth the bound must never act against.
SessionBusy == \/ s.old = "running"
               \/ s.screen \in {"working", "draft", "modal", "imageDraft", "unknown"}
               \/ (s.screen = "none" /\ s.producer # "ended")

\* ---- owner drive ----
DropNotice == IF Policy = "target"
              THEN IF s.unit > 1 /\ Mutant = "FirstOnly" THEN "first" ELSE "identified"
              ELSE "generic"
DeliverIdle == s' = [s EXCEPT !.front = "sent", !.attempted = TRUE, !.sends = @ + 1,
                              !.token = "new", !.row = "new", !.due = "none",
                              !.screen = "working", !.transcript = "open"]
\* R1: a hold taken while a lease is held arms no Readiness due (source_hold(state, active)).
BusyDue == IF Scenario = "clearDrop" /\ s.due = "none"
              /\ (Policy = "current" \/ Mutant = "NoR1")
           THEN "armed" ELSE s.due
Drive == /\ s.up /\ s.front = "queued"
         /\ IF s.token = "none" /\ s.row = "none"
            THEN IF s.due = "expired"
                 \* first-key gate (readiness_hold.rs:183-201)
                 THEN s' = [s EXCEPT !.front = "dropped", !.notice = DropNotice]
                 ELSE IF Scenario = "idleDue" /\ s.screen = "draft"
                 \* R2: an idle-path readiness hold keeps its 10 s due
                 THEN s' = [s EXCEPT !.front = "held",
                                     !.due = IF @ = "none" THEN "armed" ELSE @]
                 ELSE DeliverIdle
            ELSE IF s.token = "none" /\ s.row = "old"
            THEN IF s.rowKind = "managed" /\ Policy = "current"
                 \* foreign_row.rs:75-99 -> owner_input.rs:1121 NotSent, unheld (#8372)
                 THEN s' = [s EXCEPT !.front = "stalled"]
                 \* non-Managed row: external_turn_holds_channel -> hand_busy hold;
                 \* target R4: a Managed foreign-row refusal is recorded as a hold too
                 ELSE s' = [s EXCEPT !.front = "held"]
            ELSE IF s.screen = "working" /\ s.source /\ s.old = "running"
            THEN s' = [s EXCEPT !.front = "sent", !.attempted = TRUE, !.sends = @ + 1]
            ELSE s' = [s EXCEPT !.front = "held", !.due = BusyDue]

Rewake == /\ s.up /\ s.front = "held" /\ s.clock < Bound
          /\ s' = [s EXCEPT !.front = "queued", !.clock = @ + 1, !.streak = NextStreak]
RewakeSaturated == /\ Policy = "current" /\ s.up /\ s.front = "held" /\ s.clock = Bound
                   /\ s' = [s EXCEPT !.front = "queued"]
Observe == /\ Policy = "target" /\ s.up /\ s.front = "held" /\ s.clock = Bound
           /\ s.streak # NextStreak
           /\ s' = [s EXCEPT !.streak = NextStreak]
\* QueueEligible completion kick re-drives a held front once the lease/row is gone.
TurnEndWake == /\ s.up /\ s.kick
               /\ s' = [s EXCEPT !.kick = FALSE,
                                 !.front = IF @ = "held" /\ s.token = "none" /\ s.row = "none"
                                           THEN "queued" ELSE @]
\* Existing owner deadline tick -> wake(drive) (drive.rs:159-161).
DeadlineTick == /\ s.up /\ s.tickOwed
                /\ s' = [s EXCEPT !.tickOwed = FALSE,
                                  !.front = IF @ = "held" THEN "queued" ELSE @]

DueExpire == /\ s.due = "armed" /\ s.front \in {"queued", "held"}
             /\ s' = [s EXCEPT !.due = "expired"]
\* refuse_readiness_front (readiness_hold.rs:111-149) via settle_at_deadline: unit leaves.
DueDrop == /\ s.up /\ s.due = "expired" /\ s.front = "held"
           /\ s' = [s EXCEPT !.front = "dropped", !.notice = DropNotice]

\* ---- the bound (target) ----
BoundReady == /\ s.clock = Bound
              /\ ((s.streak = Streak /\ Witness) \/ Mutant = "NoIdle")
              /\ ~s.attempted
LeaseShape == s.token = "old"
RowShape == s.token = "none" /\ s.row = "old"
                  /\ (Mutant # "ManagedOnly" \/ s.rowKind = "managed")
BoundFire == /\ Policy = "target" /\ s.up /\ s.front = "held" /\ BoundReady
             /\ s.row # "new" /\ (LeaseShape \/ RowShape)
             /\ IF Mutant = "NoCleanup"
                THEN s' = [s EXCEPT !.front = "queued", !.clock = 0, !.streak = 0,
                                    !.boundBad = @ \/ SessionBusy]
                ELSE IF LeaseShape
                \* OperatorRelease: mute, QueueEligible kick after the CAS
                THEN s' = [s EXCEPT !.token = "none", !.row = IF @ = "old" THEN "none" ELSE @,
                                    !.ledger = "finalized", !.settle = "mute",
                                    !.released = TRUE, !.notice = "bound", !.kick = TRUE,
                                    !.sends = IF Mutant = "NoDedupe" THEN @ + 1 ELSE @,
                                    !.boundBad = @ \/ SessionBusy]
                \* row shape: no token -> no MailboxReleased; settle mute, arm the tick
                ELSE s' = [s EXCEPT !.row = "none", !.ledger = "finalized",
                                    !.settle = IF Mutant = "GateTimeout" THEN "reacted"
                                               ELSE "mute",
                                    !.released = TRUE, !.notice = "bound",
                                    !.tickOwed = (Mutant # "NoWake"),
                                    !.boundBad = @ \/ SessionBusy]

\* ---- old turn ----
OldEnds == /\ s.old = "running"
           /\ s' = [s EXCEPT !.old = "ended", !.producer = "ended", !.terminal = "submitted",
                             !.screen = IF s.token = "new" THEN @ ELSE "idle",
                             !.transcript = IF s.token = "new" THEN @ ELSE "ended"]
OldTerminal == /\ s.terminal = "submitted"
               /\ IF s.ledger = "finalized"
                  THEN IF Mutant = "NoLedgerFence" /\ s.token = "new"
                       THEN s' = [s EXCEPT !.terminal = "done", !.newCleared = TRUE,
                                           !.token = "none"]
                       ELSE s' = [s EXCEPT !.terminal = "done"]
                  ELSE s' = [s EXCEPT !.terminal = "done", !.ledger = "finalized",
                                      !.token = IF s.token = "old" THEN "none" ELSE @,
                                      !.row = IF s.row = "old" THEN "none" ELSE @,
                                      !.kick = TRUE]
LateTerminal == /\ Scenario = "late" /\ s.terminal = "lost" /\ s.released
                /\ s' = [s EXCEPT !.terminal = "submitted"]
NewEnds == /\ s.front = "sent" /\ s.token = "new"
           /\ s' = [s EXCEPT !.front = "done", !.token = "none", !.row = "none",
                             !.screen = "idle", !.transcript = "ended"]
\* runLive: the headless producer exits after its run closed (agent_executor.rs:444).
ProducerEnds == /\ Scenario = "runLive" /\ s.producer = "running"
                /\ s' = [s EXCEPT !.producer = "ended"]

\* ---- environment ----
Flicker == /\ Scenario = "flicker" /\ s.flickers < 2 /\ s.front \in {"queued", "held"}
           /\ s' = [s EXCEPT !.screen = IF @ = "idle" THEN "unknown" ELSE "idle",
                             !.flickers = @ + 1]
ToolGap == /\ Scenario = "running" /\ s.old = "running" /\ s.screen = "working"
           /\ s' = [s EXCEPT !.screen = "idle"]
ToolResume == /\ Scenario = "running" /\ s.old = "running" /\ s.screen = "idle"
              /\ s' = [s EXCEPT !.screen = "working"]
Crash == /\ Scenario = "restart" /\ s.up /\ s.restarts = 0 /\ s.front = "held"
         /\ s' = [s EXCEPT !.up = FALSE, !.restarts = 1, !.clock = 0, !.streak = 0,
                           !.kick = FALSE, !.tickOwed = FALSE, !.front = "queued"]
Boot == /\ ~s.up /\ s' = [s EXCEPT !.up = TRUE]

Next == \/ Drive \/ Rewake \/ RewakeSaturated \/ Observe \/ TurnEndWake \/ DeadlineTick
        \/ DueExpire \/ DueDrop \/ BoundFire
        \/ OldEnds \/ OldTerminal \/ LateTerminal \/ NewEnds \/ ProducerEnds
        \/ Flicker \/ ToolGap \/ ToolResume \/ Crash \/ Boot \/ UNCHANGED vars
Fairness == /\ WF_vars(Drive) /\ WF_vars(Rewake) /\ WF_vars(RewakeSaturated)
            /\ WF_vars(Observe) /\ WF_vars(TurnEndWake) /\ WF_vars(DeadlineTick)
            /\ WF_vars(DueExpire) /\ WF_vars(DueDrop) /\ WF_vars(BoundFire)
            /\ WF_vars(OldEnds) /\ WF_vars(OldTerminal) /\ WF_vars(LateTerminal)
            /\ WF_vars(NewEnds) /\ WF_vars(ProducerEnds) /\ WF_vars(Boot)
Spec == Init /\ [][Next]_vars /\ Fairness

TypeOK == /\ s.front \in {"queued", "held", "stalled", "sent", "dropped", "done"}
          /\ s.sends \in 0..2 /\ s.clock \in 0..Bound /\ s.streak \in 0..Streak
          /\ s.due \in {"none", "armed", "expired"} /\ s.settle \in {"none", "mute", "reacted"}
\* ---- INVARIANTS ----
NoDuplicateDelivery == s.sends <= 1
\* A discarded human unit always leaves one notice that names every member.
NoSilentLoss == s.front = "dropped" => s.notice = "identified"
\* The bound never acts on a working / draft / modal / image-draft / unobservable session.
NoBoundOnBusySession == ~s.boundBad
NoNewTurnClearedByOld == ~s.newCleared
\* The bound's settlement is mute: no completion reaction or success-delivery evidence.
BoundSettlesMute == s.settle # "reacted"
\* R1 guard (target cfgs): no Readiness due exists while another turn's lease is held.
R1NoDueWhileLeased == s.token = "old" => s.due = "none"
\* ---- PROPERTIES ----
Delivered == s.front \in {"sent", "done"}
\* If the session stays Idle, the held human input is eventually delivered.
IdleEventuallyDelivered == (<>[](s.screen = "idle")) => <>Delivered
\* Headless row whose producer ended does not block the reply forever.
OrphanEventuallyDelivered == <>Delivered
=============================================================================
