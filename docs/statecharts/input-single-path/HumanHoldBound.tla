---- MODULE HumanHoldBound ----
\* #8430 projection: one human owner front held behind another turn's leftover lease/row.
\* Refines HerdrBusyInput's "held" phase (RetryHeld fires only when Eligible) for the case
\* where the blocking evidence is a mailbox cancel_token / inflight row whose turn has ended.
\* Code: HEAD a53f299560. Value ranges come from the entry-point table in
\* docs/design/8430-human-hold-bound.design.md (rows cited as E#/C#/D#).
\*
\* Hold    : owner_input.rs:249-290 owner_drive has_active_turn -> hand_busy :407 ->
\*           owned_busy.rs:33-96 running: Busy injects; anything else holds
\*           (busy_source_unavailable). Idle -> HoldKind Observe (no deadline), Held ->
\*           Readiness (10 s due, readiness_hold.rs:7). held_watch.rs:18 LOOK 2 s re-drives.
\* Cleanup : the Current paths that never release this shape are abstracted away (finalizer
\*           backstop silent exits, release API, !stop host_owned, stale-age "not confirmed
\*           dead"). Target: the bound captures the old episode and releases it through the
\*           existing CAS (OperatorRelease::claim pattern, turn_lease.rs:273-336); the ledger
\*           entry finalizes; the ordinary drain delivers the unattempted front once.
\* Scenario:
\*   "incident"  11:48: old turn ended, terminal never submitted, token+row remain, Idle.
\*   "running"   old turn really runs past the bound; busy source unavailable; the screen
\*               looks idle between tools while its transcript stays open.
\*   "draft"     old turn ended without terminal, a person draft sits in the composer.
\*   "modal"     same, a modal is open.
\*   "flicker"   incident, observations fail intermittently, then settle on Idle.
\*   "late"      incident, and the lost old terminal arrives after the bound released it.
\*   "restart"   incident, one dcserver restart while held: clock/streak are memory only and
\*               boot re-registers the lease from the row (runtime.rs:370).
\*   "clearDrop" #8343: after !clear the new session source is unpinned while turn 1 runs;
\*               the second input is held with a Readiness due that outlives the turn.
\*   "rowOnly"   #8372: a headless routine turn ended Unresolved and left a Managed row
\*               (no token, no pane); the first human reply's start hits foreign_row.
EXTENDS Naturals
CONSTANTS Policy,    \* "current" (HEAD a53f299560) | "target" (design w1)
          Scenario,
          Mutant     \* "None" | "NoIdle" | "NoTranscript" | "NoCleanup" | "NoDedupe"
                     \* | "NoLedgerFence" | "NoRebase"
Bound == 2           \* abstract 3 minutes: held re-drives before the bound may fire
Streak == 2          \* consecutive positive witnesses required
VARIABLE s
vars == <<s>>

OldRuns == Scenario \in {"running", "clearDrop"}
Init == s = [front |-> "queued",       \* queued | held | stalled | sent | dropped | done
             attempted |-> FALSE,      \* a key of this front may have reached the host
             token |-> IF Scenario = "rowOnly" THEN "none" ELSE "old",
             row |-> "old",            \* inflight row owner: none | old | new
             old |-> IF OldRuns THEN "running" ELSE "ended",
             producer |-> IF OldRuns THEN "running" ELSE "ended",
             terminal |-> IF OldRuns THEN "pending" ELSE "lost",
             ledger |-> "pending",     \* old episode ledger entry: pending | finalized
             screen |-> CASE Scenario = "draft" -> "draft"
                          [] Scenario = "modal" -> "modal"
                          [] Scenario = "rowOnly" -> "none"   \* headless: no pane by kind
                          [] OldRuns -> "working"
                          [] OTHER -> "idle",
             transcript |-> CASE OldRuns -> "open"
                              [] Scenario = "rowOnly" -> "none"
                              [] OTHER -> "ended",
             source |-> ~OldRuns,      \* busy source pinned (busy join possible)
             due |-> "none",           \* Readiness due: none | armed | expired
             clock |-> 0, streak |-> 0,
             sends |-> 0,              \* human input deliveries (paste + Enter)
             released |-> FALSE,       \* the bound released the old episode
             boundBad |-> FALSE,       \* the bound fired without a true Idle/ended session
             notice |-> "none",        \* none | bound | generic | identified
             newCleared |-> FALSE,     \* an old terminal released the new turn's lease
             flickers |-> 0,
             up |-> TRUE, restarts |-> 0]

\* ---- observation (target reads; leaf readers in the design's Idle table) ----
ScreenIdle == s.screen = "idle"
TranscriptEnded == s.transcript = "ended" \/ Mutant = "NoTranscript"
Witness == IF s.screen = "none"
           THEN s.token = "none" /\ s.producer = "ended"   \* headless row: producer ended
           ELSE ScreenIdle /\ TranscriptEnded               \* pane host: two evidences
NextStreak == IF Witness THEN IF s.streak < Streak THEN s.streak + 1 ELSE s.streak ELSE 0
\* Ground truth the bound must never act against.
SessionBusy == \/ s.old = "running"
               \/ s.screen \in {"working", "draft", "modal", "unknown"}
               \/ (s.screen = "none" /\ s.producer # "ended")

\* ---- owner drive ----
DeliverIdle == s' = [s EXCEPT !.front = "sent", !.attempted = TRUE, !.sends = @ + 1,
                              !.token = "new", !.row = "new", !.due = "none",
                              !.screen = "working", !.transcript = "open"]
Drive == /\ s.up /\ s.front = "queued"
         /\ IF s.token = "none" /\ s.row = "none"
            THEN IF s.due = "expired"
                 \* first-key gate inherits the busy-path due (readiness_hold.rs:183-201)
                 THEN s' = [s EXCEPT !.front = "dropped",
                                     !.notice = IF Policy = "target" THEN "identified"
                                                ELSE "generic"]
                 ELSE DeliverIdle
            ELSE IF s.token = "none" /\ s.row = "old"
            \* start path: foreign_row.rs:75-99 refuses; owner_input.rs:1121 NotSent, unheld
            THEN IF Policy = "target"
                 THEN s' = [s EXCEPT !.front = "held"]
                 ELSE s' = [s EXCEPT !.front = "stalled"]
            ELSE IF s.screen = "working" /\ s.source /\ s.old = "running"
            THEN s' = [s EXCEPT !.front = "sent", !.attempted = TRUE, !.sends = @ + 1]
            ELSE s' = [s EXCEPT !.front = "held",
                                \* #8343: a Held (unpinned source) reading arms a Readiness due
                                !.due = IF Scenario = "clearDrop" /\ s.due = "none"
                                        THEN "armed" ELSE @]

\* held_watch wake: the drive runs again. The bound clock is keyed by the same front.
Rewake == /\ s.up /\ s.front = "held" /\ s.clock < Bound
          /\ s' = [s EXCEPT !.front = "queued", !.clock = @ + 1, !.streak = NextStreak]
\* Current: the 2 s loop continues forever once the abstract clock saturates.
RewakeSaturated == /\ Policy = "current" /\ s.up /\ s.front = "held" /\ s.clock = Bound
                   /\ s' = [s EXCEPT !.front = "queued"]
\* Target: the saturated front stays held; observations keep the streak current.
Observe == /\ Policy = "target" /\ s.up /\ s.front = "held" /\ s.clock = Bound
           /\ s.streak # NextStreak
           /\ s' = [s EXCEPT !.streak = NextStreak]
\* A turn-completion kick re-drives a held front once the blocking lease/row is gone.
TurnEndWake == /\ s.up /\ s.front = "held" /\ s.token = "none" /\ s.row = "none"
               /\ s' = [s EXCEPT !.front = "queued"]

\* Readiness due (10 s): time passes while the front waits.
DueExpire == /\ s.due = "armed" /\ s.front \in {"queued", "held"}
             /\ s' = [s EXCEPT !.due = "expired"]
\* refuse_readiness_front (readiness_hold.rs:111-149) via settle_at_deadline (drive.rs:139).
\* Target R1: a due taken while another turn holds the lease never drops the front.
DueDrop == /\ s.up /\ s.due = "expired" /\ s.front = "held"
           /\ (Policy = "current" \/ s.token = "none")
           /\ s' = [s EXCEPT !.front = "dropped",
                             !.notice = IF Policy = "target" THEN "identified" ELSE "generic"]

\* ---- the bound (target) ----
BoundReady == /\ s.clock = Bound
              /\ ((s.streak = Streak /\ Witness) \/ Mutant = "NoIdle")
              /\ ~s.attempted
BoundFire == /\ Policy = "target" /\ s.up /\ s.front = "held" /\ BoundReady
             /\ s.token # "new" /\ s.row # "new" /\ (s.token = "old" \/ s.row = "old")
             /\ IF Mutant = "NoCleanup"
                THEN s' = [s EXCEPT !.front = "queued", !.clock = 0, !.streak = 0,
                                    !.boundBad = @ \/ SessionBusy]
                ELSE IF Mutant = "NoDedupe"
                \* pastes directly and also leaves the front for the release drain
                THEN s' = [s EXCEPT !.token = "none", !.row = "none", !.ledger = "finalized",
                                    !.released = TRUE, !.notice = "bound",
                                    !.sends = @ + 1, !.front = "queued",
                                    !.boundBad = @ \/ SessionBusy]
                ELSE s' = [s EXCEPT !.token = "none", !.row = "none", !.ledger = "finalized",
                                    !.released = TRUE, !.notice = "bound", !.due = "none",
                                    !.front = "queued", !.boundBad = @ \/ SessionBusy]

\* ---- old turn ----
OldEnds == /\ s.old = "running"
           /\ s' = [s EXCEPT !.old = "ended", !.producer = "ended", !.terminal = "submitted",
                             !.screen = IF s.token = "new" THEN @ ELSE "idle",
                             !.transcript = IF s.token = "new" THEN @ ELSE "ended"]
\* The old terminal settles through the ledger (exactly once per episode, nonce fenced).
OldTerminal == /\ s.terminal = "submitted"
               /\ IF s.ledger = "finalized"
                  THEN IF Mutant = "NoLedgerFence" /\ s.token = "new"
                       THEN s' = [s EXCEPT !.terminal = "done", !.newCleared = TRUE,
                                           !.token = "none"]
                       ELSE s' = [s EXCEPT !.terminal = "done"]
                  ELSE s' = [s EXCEPT !.terminal = "done", !.ledger = "finalized",
                                      !.token = IF s.token = "old" THEN "none" ELSE @,
                                      !.row = IF s.row = "old" THEN "none" ELSE @,
                                      \* target R1: a due armed while the lease was held does not
                                      \* outlive it (code: source_hold never arms one then)
                                      !.due = IF Policy = "target" /\ Mutant # "NoRebase"
                                              THEN "none" ELSE @]
\* "late": the lost terminal of the old turn finally arrives after the bound.
LateTerminal == /\ Scenario = "late" /\ s.terminal = "lost" /\ s.released
                /\ s' = [s EXCEPT !.terminal = "submitted"]
\* The delivered turn runs; its own completion releases its own lease.
NewEnds == /\ s.front = "sent" /\ s.token = "new"
           /\ s' = [s EXCEPT !.front = "done", !.token = "none", !.row = "none",
                             !.screen = "idle", !.transcript = "ended"]

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
                           !.front = "queued"]
Boot == /\ ~s.up /\ s' = [s EXCEPT !.up = TRUE]

Next == \/ Drive \/ Rewake \/ RewakeSaturated \/ Observe \/ TurnEndWake
        \/ DueExpire \/ DueDrop \/ BoundFire
        \/ OldEnds \/ OldTerminal \/ LateTerminal \/ NewEnds
        \/ Flicker \/ ToolGap \/ ToolResume \/ Crash \/ Boot \/ UNCHANGED vars
Fairness == /\ WF_vars(Drive) /\ WF_vars(Rewake) /\ WF_vars(RewakeSaturated)
            /\ WF_vars(Observe) /\ WF_vars(TurnEndWake) /\ WF_vars(DueExpire)
            /\ WF_vars(DueDrop) /\ WF_vars(BoundFire) /\ WF_vars(OldEnds)
            /\ WF_vars(OldTerminal) /\ WF_vars(LateTerminal) /\ WF_vars(NewEnds)
            /\ WF_vars(Boot)
Spec == Init /\ [][Next]_vars /\ Fairness

TypeOK == /\ s.front \in {"queued", "held", "stalled", "sent", "dropped", "done"}
          /\ s.sends \in 0..2 /\ s.clock \in 0..Bound /\ s.streak \in 0..Streak
          /\ s.due \in {"none", "armed", "expired"}
\* ---- INVARIANTS ----
NoDuplicateDelivery == s.sends <= 1
\* A discarded human input always leaves a notice that names it.
NoSilentLoss == s.front = "dropped" => s.notice = "identified"
\* The bound never acts on a working / draft / modal / unobservable session.
NoBoundOnBusySession == ~s.boundBad
NoNewTurnClearedByOld == ~s.newCleared
\* ---- PROPERTIES ----
Delivered == s.front \in {"sent", "done"}
\* If the session stays Idle, the held human input is eventually delivered.
IdleEventuallyDelivered == (<>[](s.screen = "idle")) => <>Delivered
\* #8372: a row whose producer ended (no pane, no token) does not block the reply forever.
OrphanEventuallyDelivered == <>Delivered
=============================================================================
