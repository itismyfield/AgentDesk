---------------------------- MODULE InputPath ----------------------------
(* #7272 single input path for one Claude TUI channel: every Discord input is admitted to
   the owner queue, then one drive task prepares it (Empty -> direct, readable draft -> C-s,
   modal or unreadable -> Held), keys it (C-s, paste, Enter) and settles it.
   Landed code: origin/main 0a9b3f99cf. Unlanded 4f design: design-4f-r1 (+ carry P1).
   Readiness refinement: ReadinessSpec expands the formerly opaque pre-key ready check
   into transcript Idle, scoped Stop wakes and SessionStart; legacy specs stay frozen.
   Input ids are 1..N. 0 stands for the human's own draft in the composer.            *)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    N,                   \* Discord inputs
    UnreadProceeds,      \* unreadable composer pastes without C-s (busy_inject.rs:374, 4f §3, carry P1-7)
    HerdrNoCtrlS,        \* Herdr visible draft pastes behind it (4f r1 D-4f-5; carry P1-7 reverses it)
    StopReturnsQueued,   \* stopping a busy Claude puts its queued prompts back in the composer (assumption)
    HeldWakes,           \* a Held input stays first in line and is woken when the hold clears (plan A19)
    BackstopMayHaveSent, \* the deadline backstop also wakes for a MayHaveSent head (4f §7 names Unknown only)
    Mut                  \* one guard removed, "none" for the model as written

Ids == 1..N
DrivePcs == {"idle", "reserved", "direct", "stash", "keyed", "pasted"}

VARIABLES
    arrived,    \* inputs admitted so far (ids 1..arrived)
    queue,      \* owner items, oldest first
    head,       \* head_injection phase of Head(queue): none, Reserved, MayHaveSent, Unknown
    dl,         \* confirmation deadline of the head: pending or passed
    retU,       \* retained as unconfirmed, with a notice
    retNS,      \* retained as known not sent, with a "retry required" notice
    drv,        \* pc of each drive task (1..2)
    cur,        \* input each drive task holds
    wake,       \* a drive wake is pending
    heldWait,   \* a Held input waits for its hold to clear
    booting,    \* server restarted, boot recovery not run yet
    restarts,   \* server restarts left
    comp,       \* what the composer shows: subset of {0} \cup Ids
    stash,      \* what Claude's C-s stash holds
    humanLeft,  \* human drafts left to type
    modal,      \* a modal is open
    modalLeft,
    prov,       \* idle or busy
    provQ,      \* prompts Claude queued while busy (sequence of sets)
    stopLeft,
    busySent,   \* inputs submitted while Claude was busy
    fate,       \* how each input reached a turn: none, idle, this, next
    consumed,   \* times each input reached a turn
    order,      \* inputs in the order they reached a turn
    harm,       \* none, mixed (draft sent with an input), lost (draft overwritten)
    legacyLeft,
    stopped     \* inputs a user stop took before the drive's first key (the token's NoKeyYet gate)

vars == <<arrived, queue, head, dl, retU, retNS, drv, cur, wake, heldWait, booting, restarts,
          comp, stash, humanLeft, modal, modalLeft, prov, provQ, stopLeft, busySent, fate,
          consumed, order, harm, legacyLeft, stopped>>
\* stopped rides along with the environment: only StopBeforeKey changes it.
envVars == <<humanLeft, modal, modalLeft, stopLeft, legacyLeft, stopped>>

BaseInit ==
    /\ arrived = 0 /\ queue = <<>> /\ head = "none" /\ dl = "pending"
    /\ retU = {} /\ retNS = {}
    /\ drv = [k \in 1..2 |-> "idle"] /\ cur = [k \in 1..2 |-> 0]
    /\ wake = FALSE /\ heldWait = FALSE /\ booting = FALSE /\ restarts = 1
    /\ comp = {} /\ stash = {} /\ humanLeft = 1 /\ modal = FALSE /\ modalLeft = 1
    /\ prov \in {"idle", "busy"} /\ provQ = <<>> /\ stopLeft = 1
    /\ busySent = {} /\ fate = [i \in Ids |-> "none"] /\ consumed = [i \in Ids |-> 0]
    /\ order = <<>> /\ harm = "none" /\ legacyLeft = 1 /\ stopped = {}

AllIdle == \A k \in 1..2 : drv[k] = "idle"
Keying == \E k \in 1..2 : drv[k] \in {"direct", "stash", "keyed", "pasted"}
Owned == {queue[p] : p \in 1..Len(queue)} \cup retU \cup retNS
Pop(q) == Tail(q)

\* Ids of a message in ascending order, appended to the turn order.
RECURSIVE SortedIds(_)
SortedIds(S) == IF S = {} THEN <<>>
                ELSE LET m == CHOOSE x \in S : \A y \in S : x <= y
                     IN <<m>> \o SortedIds(S \ {m})

Consume(msg, how) ==
    LET ids == msg \cap Ids IN
    /\ consumed' = [i \in Ids |-> IF i \in ids THEN consumed[i] + 1 ELSE consumed[i]]
    /\ fate' = [i \in Ids |-> IF i \in ids THEN how ELSE fate[i]]
    /\ order' = order \o SortedIds(ids)

Harm(msg) == IF 0 \in msg /\ msg \cap Ids # {} THEN "mixed" ELSE harm

\* ---------------------------------------------------------------- Discord and the drive task
\* IO/ingress.rs admit -> Accepted -> drive::wake (4f §2.2 wake table, first row)
Arrive ==
    /\ arrived < N /\ ~booting
    /\ arrived' = arrived + 1 /\ queue' = Append(queue, arrived + 1)
    /\ wake' = (Mut # "noArrivalWake")
    /\ UNCHANGED <<head, dl, retU, retNS, drv, cur, heldWait, booting, restarts, comp, stash, prov,
                   provQ, busySent, fate, consumed, order, harm, envVars>>

\* drive::run: single-flight per channel (4f §2.2), owner.front() only with no head_injection,
\* then reserve (v2.rs:86-115 refuses a second active head).
DriveTakesFront(k) ==
    /\ wake /\ ~booting /\ drv[k] = "idle"
    /\ Mut \in {"noSingleFlight", "reserveIgnoresHead"} \/ AllIdle
    /\ Mut = "reserveIgnoresHead" \/ head = "none"
    /\ queue # <<>> /\ ~heldWait
    /\ LET pick == IF Mut = "frontNotHead" THEN Len(queue) ELSE 1 IN
       /\ cur' = [cur EXCEPT ![k] = queue[pick]]
       /\ queue' = IF pick = 1 THEN queue ELSE <<queue[pick]>> \o SubSeq(queue, 1, pick - 1)
    /\ head' = "Reserved" /\ drv' = [drv EXCEPT ![k] = "reserved"] /\ wake' = FALSE
    /\ UNCHANGED <<arrived, dl, retU, retNS, heldWait, booting, restarts, comp, stash, prov, provQ,
                   busySent, fate, consumed, order, harm, envVars>>

\* front() is None: nothing to take now.
DriveFindsNothing ==
    /\ wake /\ ~booting /\ AllIdle
    /\ head # "none" \/ queue = <<>> \/ heldWait
    /\ wake' = FALSE
    /\ UNCHANGED <<arrived, queue, head, dl, retU, retNS, drv, cur, heldWait, booting, restarts,
                   comp, stash, prov, provQ, busySent, fate, consumed, order, harm, envVars>>

\* One read before any key (busy_inject.rs:359-381, 4f §3 Herdr steps 2-3).
\* obs = "read": the composer layout is recognised; "unread": capture fails or layout Unknown.
Decide(obs) ==
    IF modal THEN "held"
    ELSE IF obs = "unread" THEN (IF UnreadProceeds THEN "direct" ELSE "held")
    ELSE IF comp = {} THEN "direct"
    ELSE IF Mut = "noCtrlS" \/ HerdrNoCtrlS THEN "direct"
    \* parkable is false while a stash is shown (screen.rs:114-123): paste behind.
    ELSE IF stash # {} /\ Mut # "stashShownIgnored" THEN "direct"
    ELSE "stash"

Prepare(k) ==
    /\ drv[k] = "reserved"
    /\ \E obs \in {"read", "unread"} :
        LET d == Decide(obs) IN
        IF d # "held"
        THEN /\ drv' = [drv EXCEPT ![k] = d]
             /\ UNCHANGED <<queue, head, retNS, cur, wake, heldWait>>
        ELSE \* NotSent before any key.
             IF HeldWakes
             THEN \* clean: stays first in line, released, woken when the hold clears
                  /\ head' = "none" /\ heldWait' = TRUE /\ wake' = FALSE
                  /\ drv' = [drv EXCEPT ![k] = "idle"] /\ cur' = [cur EXCEPT ![k] = 0]
                  /\ UNCHANGED <<queue, retNS>>
             ELSE \* the only landed V2 NotSent settlement: retain_not_sent (retained.rs:85-114)
                  /\ head' = "none" /\ queue' = Pop(queue) /\ retNS' = retNS \cup {cur[k]}
                  /\ drv' = [drv EXCEPT ![k] = "idle"] /\ cur' = [cur EXCEPT ![k] = 0]
                  /\ wake' = TRUE /\ UNCHANGED heldWait
    /\ UNCHANGED <<arrived, dl, retU, booting, restarts, comp, stash, prov, provQ, busySent, fate,
                   consumed, order, harm, envVars>>

\* arm (attempt.rs:81-119: MayHaveSent, deadline now+30s), then C-s when parking (stash.rs:50-58).
\* Claude keeps one stash: a second C-s replaces it.
ArmAndKey(k) ==
    /\ drv[k] \in {"direct", "stash"}
    /\ head' = "MayHaveSent" /\ dl' = "pending"
    /\ IF drv[k] = "stash"
       THEN /\ stash' = comp /\ comp' = {}
            /\ harm' = IF 0 \in stash THEN "lost" ELSE harm
       ELSE UNCHANGED <<stash, comp, harm>>
    /\ drv' = [drv EXCEPT ![k] = "keyed"]
    /\ UNCHANGED <<arrived, queue, retU, retNS, cur, wake, heldWait, booting, restarts, prov, provQ,
                   busySent, fate, consumed, order, envVars>>

Paste(k) ==
    /\ drv[k] = "keyed"
    /\ comp' = comp \cup {cur[k]} /\ drv' = [drv EXCEPT ![k] = "pasted"]
    /\ UNCHANGED <<arrived, queue, head, dl, retU, retNS, cur, wake, heldWait, booting, restarts,
                   stash, prov, provQ, busySent, fate, consumed, order, harm, envVars>>

\* Claude takes the whole composer as one prompt, then restores its stash.
Submit(msg) ==
    /\ harm' = Harm(msg)
    /\ IF prov = "idle"
       THEN /\ Consume(msg, "idle") /\ prov' = "busy" /\ UNCHANGED <<provQ, busySent>>
       ELSE /\ provQ' = Append(provQ, msg) /\ busySent' = busySent \cup (msg \cap Ids)
            /\ UNCHANGED <<consumed, fate, order, prov>>
    /\ comp' = stash /\ stash' = {}

\* Paste or Enter fails or is indeterminate: WriteUncertain -> settle(false) -> Unknown (retained.rs:36-83).
\* The paste may or may not have landed; a failed Enter (stash.rs:88-93) may still have been applied.
WriteUncertain(k) ==
    /\ drv[k] \in {"keyed", "pasted"}
    /\ \E landed \in {FALSE, TRUE} :
        IF drv[k] = "pasted" /\ landed
        THEN Submit(comp)
        ELSE /\ comp' = IF landed \/ drv[k] = "pasted" THEN comp \cup {cur[k]} ELSE comp
             /\ UNCHANGED <<stash, prov, provQ, busySent, fate, consumed, order, harm>>
    /\ head' = "Unknown"
    /\ drv' = [drv EXCEPT ![k] = "idle"] /\ cur' = [cur EXCEPT ![k] = 0] /\ wake' = TRUE
    /\ UNCHANGED <<arrived, queue, dl, retU, retNS, heldWait, booting, restarts, envVars>>

\* Enter applied. The owner settles Submitted on the Enter (4f §3: no receipt wait).
Enter(k) ==
    /\ drv[k] = "pasted"
    /\ Submit(comp)
    /\ head' = "none" /\ queue' = Pop(queue)
    /\ drv' = [drv EXCEPT ![k] = "idle"] /\ cur' = [cur EXCEPT ![k] = 0] /\ wake' = TRUE
    /\ UNCHANGED <<arrived, dl, retU, retNS, heldWait, booting, restarts, envVars>>

\* A user stop while the drive holds an input before its first key (reserved, direct or stash).
\* The writer's token is NoKeyYet: the stop takes the gate, main cancels and the writer sends no
\* key, ending Cancelled{0} as WriteUncertain -> Unknown, never a NotSent the drive resends.
\* Mutants: the writer never sees the gate and keys on, or Cancelled{0} is held as NotSent.
\* busyNoGate: only the busy-pane writer (prov busy, the ClaudeBusy route) skips the gate.
\* refused: after the stop the writer ends on a refusal that records a not-sent proof (a failed
\* buffer load, a refused ready check or a hold). proofOverStop lets that proof outrank the stop.
StopBeforeKey(k) ==
    /\ drv[k] \in {"reserved", "direct", "stash"} /\ stopped = {}
    /\ stopped' = {cur[k]}
    /\ IF Mut = "noKeyGate" \/ (Mut = "busyNoGate" /\ prov = "busy")
       THEN UNCHANGED <<queue, head, drv, cur, wake, heldWait>>
       ELSE /\ drv' = [drv EXCEPT ![k] = "idle"] /\ cur' = [cur EXCEPT ![k] = 0] /\ wake' = TRUE
            /\ \E refused \in BOOLEAN :
               IF Mut = "cancelledNotSent" \/ (Mut = "proofOverStop" /\ refused)
               THEN /\ head' = "none" /\ heldWait' = FALSE /\ UNCHANGED queue
               ELSE /\ head' = "Unknown" /\ UNCHANGED <<queue, heldWait>>
    /\ UNCHANGED <<arrived, dl, retU, retNS, booting, restarts, comp, stash, prov, provQ, busySent,
                   fate, consumed, order, harm, humanLeft, modal, modalLeft, stopLeft, legacyLeft>>

\* ------------------------------------------------------------- deadline, backstop, restart
DeadlinePasses ==
    /\ head \in {"MayHaveSent", "Unknown"} /\ dl = "pending" /\ AllIdle
    /\ dl' = "passed"
    /\ UNCHANGED <<arrived, queue, head, retU, retNS, drv, cur, wake, heldWait, booting, restarts,
                   comp, stash, prov, provQ, busySent, fate, consumed, order, harm, envVars>>

\* tick (retained.rs:4-30): an expired head leaves the line as unconfirmed, with a notice, and is
\* never sent again. Mutant: put it back in line (an automatic resend).
Tick ==
    IF Mut = "resendAfterUnknown"
    THEN /\ head' = "none" /\ UNCHANGED <<queue, retU>>
    ELSE /\ head' = "none" /\ queue' = Pop(queue) /\ retU' = retU \cup {queue[1]}

\* 4f §7: the channel backstop is armed for the earliest Unknown deadline and ticks only if
\* the attempt is still Unknown.
Backstop ==
    /\ ~booting /\ AllIdle /\ dl = "passed"
    /\ head \in (IF BackstopMayHaveSent THEN {"MayHaveSent", "Unknown"} ELSE {"Unknown"})
    /\ Tick /\ wake' = TRUE
    /\ UNCHANGED <<arrived, dl, retNS, drv, cur, heldWait, booting, restarts, comp, stash, prov,
                   provQ, busySent, fate, consumed, order, harm, envVars>>

\* The server process ends: drive tasks and in-memory wakes are gone; the queue file, the pane,
\* the composer and Claude survive.
Restart ==
    /\ restarts > 0 /\ ~booting
    /\ restarts' = restarts - 1 /\ booting' = TRUE
    /\ drv' = [k \in 1..2 |-> "idle"] /\ cur' = [k \in 1..2 |-> 0]
    /\ wake' = FALSE /\ heldWait' = FALSE
    /\ UNCHANGED <<arrived, queue, head, dl, retU, retNS, comp, stash, prov, provQ, busySent, fate,
                   consumed, order, harm, envVars>>

\* B1 (4f §2.5, §7 boot row): adopt -> tick -> drain -> wake. A Reserved head is prepared again
\* (12.1 "durable Reserved"); a keyed head is retained only once its deadline passed.
Boot ==
    /\ booting
    /\ IF head = "Reserved" THEN head' = "none" /\ UNCHANGED <<queue, retU>>
       ELSE IF head \in {"MayHaveSent", "Unknown"} /\ dl = "passed" THEN Tick
       ELSE UNCHANGED <<head, queue, retU>>
    /\ booting' = FALSE /\ wake' = TRUE
    /\ UNCHANGED <<arrived, dl, retNS, drv, cur, heldWait, restarts, comp, stash, prov, provQ,
                   busySent, fate, consumed, order, harm, envVars>>

\* ---------------------------------------------------------------------- Claude and the human
\* Claude takes a queued prompt into the running turn.
Absorb ==
    /\ prov = "busy" /\ provQ # <<>>
    /\ Consume(Head(provQ), "this") /\ provQ' = Tail(provQ)
    /\ UNCHANGED <<arrived, queue, head, dl, retU, retNS, drv, cur, wake, heldWait, booting,
                   restarts, comp, stash, prov, busySent, harm, envVars>>

\* The turn ends; a queued prompt starts the next turn. Completion wakes the drive (4f §2.3 D3).
TurnEnds ==
    /\ prov = "busy"
    /\ IF provQ # <<>>
       THEN /\ Consume(Head(provQ), "next") /\ provQ' = Tail(provQ) /\ UNCHANGED prov
       ELSE /\ prov' = "idle" /\ UNCHANGED <<provQ, consumed, fate, order>>
    /\ wake' = IF Mut = "noCompletionWake" THEN wake ELSE TRUE
    /\ UNCHANGED <<arrived, queue, head, dl, retU, retNS, drv, cur, heldWait, booting, restarts,
                   comp, stash, busySent, harm, envVars>>

\* /stop ends the AI turn only (4f §12.2).
Stop ==
    /\ stopLeft > 0 /\ prov = "busy" /\ StopReturnsQueued /\ provQ # <<>>
    /\ stopLeft' = stopLeft - 1 /\ prov' = "idle"
    /\ comp' = comp \cup UNION {provQ[p] : p \in 1..Len(provQ)} /\ provQ' = <<>>
    /\ wake' = IF Mut = "noCompletionWake" THEN wake ELSE TRUE
    /\ UNCHANGED <<arrived, queue, head, dl, retU, retNS, drv, cur, heldWait, booting, restarts,
                   stash, humanLeft, modal, modalLeft, legacyLeft, busySent, fate, consumed,
                   order, harm, stopped>>

\* The human types a draft. Not while a drive task is between its read and its Enter: no pane is
\* read after a key (stash.rs:1), so that race is outside this model.
HumanTypes ==
    /\ humanLeft > 0 /\ ~Keying /\ 0 \notin comp \cup stash
    /\ humanLeft' = humanLeft - 1 /\ comp' = comp \cup {0}
    /\ UNCHANGED <<arrived, queue, head, dl, retU, retNS, drv, cur, wake, heldWait, booting,
                   restarts, stash, modal, modalLeft, prov, provQ, stopLeft, busySent, fate,
                   consumed, order, harm, legacyLeft, stopped>>

ModalOpens ==
    /\ modalLeft > 0 /\ ~modal /\ ~Keying
    /\ modalLeft' = modalLeft - 1 /\ modal' = TRUE
    /\ UNCHANGED <<arrived, queue, head, dl, retU, retNS, drv, cur, wake, heldWait, booting,
                   restarts, comp, stash, humanLeft, prov, provQ, stopLeft, busySent, fate,
                   consumed, order, harm, legacyLeft, stopped>>

\* With HeldWakes the hold's own wake fires when the modal closes or the next read succeeds.
ModalCloses ==
    /\ modal /\ modal' = FALSE
    /\ IF HeldWakes /\ heldWait THEN heldWait' = FALSE /\ wake' = TRUE ELSE UNCHANGED <<heldWait, wake>>
    /\ UNCHANGED <<arrived, queue, head, dl, retU, retNS, drv, cur, booting, restarts, comp, stash,
                   humanLeft, modalLeft, prov, provQ, stopLeft, busySent, fate, consumed, order,
                   harm, legacyLeft, stopped>>

HoldRetries ==
    /\ HeldWakes /\ heldWait /\ ~modal
    /\ heldWait' = FALSE /\ wake' = TRUE
    /\ UNCHANGED <<arrived, queue, head, dl, retU, retNS, drv, cur, booting, restarts, comp, stash,
                   prov, provQ, busySent, fate, consumed, order, harm, envVars>>

\* A non-owner start (Process or recovery claim) of a source the owner already holds.
\* 4f §5 typed claim: OwnerHeld is refused. Mutant: the source check is gone.
LegacyStarts ==
    /\ Mut = "sourceCheckRemoved" /\ legacyLeft > 0
    /\ \E i \in Owned :
        /\ legacyLeft' = legacyLeft - 1
        /\ Consume({i}, "idle")
    /\ UNCHANGED <<arrived, queue, head, dl, retU, retNS, drv, cur, wake, heldWait, booting,
                   restarts, comp, stash, humanLeft, modal, modalLeft, prov, provQ, stopLeft,
                   busySent, harm, stopped>>

Quiet == queue = <<>> /\ head = "none" /\ AllIdle /\ prov = "idle" /\ provQ = <<>> /\ ~booting

Done ==
    /\ Quiet /\ arrived = N /\ ~wake
    /\ UNCHANGED vars

BaseNext ==
    \/ Arrive \/ DriveFindsNothing
    \/ \E k \in 1..2 : DriveTakesFront(k) \/ Prepare(k) \/ ArmAndKey(k) \/ Paste(k)
                        \/ WriteUncertain(k) \/ Enter(k) \/ StopBeforeKey(k)
    \/ DeadlinePasses \/ Backstop \/ Restart \/ Boot
    \/ Absorb \/ TurnEnds \/ Stop
    \/ HumanTypes \/ ModalOpens \/ ModalCloses \/ HoldRetries \/ LegacyStarts
    \/ Done

\* Boot intake refinement: value domains and code anchors are in i8055/model-values.md.
\* A gateway action is the completed ingress transaction, not an attempted notice.
\* Failed storage + failed notice does not complete in the design; availability is external.
VARIABLES biReceived, biHeld, biAccepted, biVisible, biPermanent, biCheckpoint,
          biExecutions, biWriter, biWake, biRestarts, biHalts, biOrigin, biOwner,
          biProviderMatch, biPayload
legacyBootIngressVars == <<biReceived,biHeld,biAccepted,biVisible,biPermanent,biCheckpoint,
                    biExecutions,biWriter,biWake,biRestarts,biHalts,biOrigin,biOwner,
                    biProviderMatch,biPayload>>
\* A concrete readiness boundary shared by ordinary and first Herdr input design.
VARIABLE inputReadiness
Readiness == INSTANCE ClaudeReadiness WITH N <- N, Mode <- Mut, r <- inputReadiness
bootIngressVars == <<legacyBootIngressVars, inputReadiness>>
biMode == Mut \in {"boot-current", "boot-design", "boot-no-ready-wake", "boot-checkpoint-early"}
biFixed == Mut # "boot-current"
biLocal == biOwner = "local" /\ biProviderMatch
biReady == biWriter = "ready" /\ biLocal
biDone == {i \in Ids : biExecutions[i] > 0}
BootIngressInit ==
    /\ Readiness!Init
    /\ biOrigin \in (IF biMode THEN {"live", "queued", "claimed"} ELSE {"live"})
    /\ biOwner \in (IF biMode THEN {"local", "remote", "lost"} ELSE {"local"})
    /\ biProviderMatch \in (IF biMode THEN BOOLEAN ELSE {TRUE})
    /\ biReceived = {} /\ biVisible = {} /\ biPermanent = {} /\ biCheckpoint = {}
    /\ biHeld = (IF biOrigin = "live" THEN {} ELSE Ids)
    /\ biAccepted = biHeld
    /\ biExecutions = [i \in Ids |-> 0]
    /\ biPayload = [i \in Ids |-> "original"]
    /\ biWriter = "booting" /\ biWake = FALSE /\ biRestarts = 1 /\ biHalts = 1


Init == BaseInit /\ BootIngressInit
Next == BaseNext /\ UNCHANGED bootIngressVars

Spec == Init /\ [][Next]_vars

\* ------------------------------------------------------------------------------------ rules
\* 1. No input reaches Claude twice.
NoDuplicate == \A i \in Ids : consumed[i] <= 1
\* 2. Once the channel is quiet, each input reached Claude or sits retained with a notice.
NoSilentLoss == Quiet => \A i \in 1..arrived : consumed[i] = 1 \/ i \in retU \cup retNS
\* 3. The human's draft is never sent with an input nor overwritten.
DraftSafe == harm = "none"
\* 4. At most one drive task executes an input on the channel.
OneExecutor == Cardinality({k \in 1..2 : drv[k] # "idle"}) <= 1
\* 5. Inputs reach Claude in arrival order.
Fifo == \A p, q \in 1..Len(order) : p < q => order[p] < order[q]
\* 6. A Held input is not dropped from the line just because it was Held.
HeldNotDropped == retNS = {}
\* 7. An input sent while Claude was busy reached this turn or the next one.
BusyInputLands == Quiet => \A i \in busySent : fate[i] \in {"this", "next"}
\* 8. An input a stop took before its first key never reaches the pane or Claude.
StoppedBeforeKey == \A i \in stopped : consumed[i] = 0 /\ i \notin comp \cup stash \cup busySent

\* ---------------------------------------------------------------------------- liveness
\* Weak fairness on every step: a drive, deadline, backstop or boot that stays enabled runs.
LiveSpec == Spec /\ WF_vars(Next)
\* 9. An input a stop took before its first key leaves the line, retained unconfirmed.
StoppedLeaves == \A i \in Ids : (i \in stopped) ~> (i \in retU \/ consumed[i] = 1)
\* 10. After that stop the line unblocks: no head is held and every drive is idle.
StopUnblocks == (stopped # {}) ~> (head = "none" /\ AllIdle)

\* Ordinary existing owners keep their originals when admission is blocked.
BootGatewayTransient ==
    /\ biLocal /\ ~biReady
    /\ \E i \in Ids \ biReceived, saved \in BOOLEAN, visibleAck \in BOOLEAN :
       /\ biReceived' = biReceived \cup {i}
       /\ IF biOrigin # "live" \/ (saved /\ biFixed)
          THEN /\ biHeld' = biHeld \cup {i} /\ biAccepted' = biAccepted \cup {i}
               /\ biCheckpoint' = biCheckpoint \cup {i}
               /\ UNCHANGED biVisible
          ELSE /\ (~biFixed \/ visibleAck)
               /\ biVisible' = IF visibleAck THEN biVisible \cup {i} ELSE biVisible
               /\ biCheckpoint' = IF ~biFixed \/ Mut = "boot-checkpoint-early"
                                   THEN biCheckpoint \cup {i} ELSE biCheckpoint
               /\ UNCHANGED <<biHeld,biAccepted>>
    /\ UNCHANGED <<biPermanent,biExecutions,biWriter,biWake,biRestarts,biHalts,
                   biOrigin,biOwner,biProviderMatch,biPayload,vars>>

BootGatewayReady ==
    /\ biReady
    /\ \E i \in Ids \ biReceived :
       /\ biReceived' = biReceived \cup {i}
       /\ biHeld' = biHeld \cup {i} /\ biAccepted' = biAccepted \cup {i}
       /\ biCheckpoint' = biCheckpoint \cup {i} /\ biWake' = TRUE
    /\ UNCHANGED <<biVisible,biPermanent,biExecutions,biWriter,biRestarts,biHalts,
                   biOrigin,biOwner,biProviderMatch,biPayload,vars>>

BootPermanentReject ==
    /\ ~biLocal
    /\ \E i \in Ids \ biReceived :
       /\ biReceived' = biReceived \cup {i} /\ biVisible' = biVisible \cup {i}
       /\ biPermanent' = biPermanent \cup {i}
    /\ UNCHANGED <<biHeld,biAccepted,biCheckpoint,biExecutions,biWriter,biWake,
                   biRestarts,biHalts,biOrigin,biOwner,biProviderMatch,biPayload,vars>>

\* Gateway redelivery and catch-up use the same durable source ID; no second executor.
BootCatchUp ==
    /\ biLocal /\ biReceived # {}
    /\ \E i \in biReceived \ biPermanent :
       /\ biHeld' = IF i \in biDone THEN biHeld ELSE biHeld \cup {i}
       /\ biAccepted' = biAccepted \cup {i} /\ biCheckpoint' = biCheckpoint \cup {i}
       /\ biWake' = biReady
    /\ UNCHANGED <<biReceived,biVisible,biPermanent,biExecutions,biWriter,biRestarts,
                   biHalts,biOrigin,biOwner,biProviderMatch,biPayload,vars>>
BootGatewayRedelivery ==
    /\ biReceived # {} /\ UNCHANGED <<bootIngressVars,vars>>
BootBeginResume ==
    /\ biWriter = "booting" /\ biWriter' = "resuming"
    /\ UNCHANGED <<biReceived,biHeld,biAccepted,biVisible,biPermanent,biCheckpoint,
                   biExecutions,biWake,biRestarts,biHalts,biOrigin,biOwner,
                   biProviderMatch,biPayload,vars>>
BootReady ==
    /\ biWriter = "resuming" /\ biWriter' = "ready"
    /\ biWake' = ((Mut # "boot-no-ready-wake") /\ biHeld # {})
    /\ UNCHANGED <<biReceived,biHeld,biAccepted,biVisible,biPermanent,biCheckpoint,
                   biExecutions,biRestarts,biHalts,biOrigin,biOwner,
                   biProviderMatch,biPayload,vars>>
BootHalt ==
    /\ biWriter = "resuming" /\ biHalts > 0
    /\ biWriter' = "halted" /\ biHalts' = biHalts - 1 /\ biWake' = FALSE
    /\ UNCHANGED <<biReceived,biHeld,biAccepted,biVisible,biPermanent,biCheckpoint,
                   biExecutions,biRestarts,biOrigin,biOwner,biProviderMatch,biPayload,vars>>
BootRefreshHost ==
    /\ biWriter = "halted" /\ biWriter' = "booting"
    /\ UNCHANGED <<biReceived,biHeld,biAccepted,biVisible,biPermanent,biCheckpoint,
                   biExecutions,biWake,biRestarts,biHalts,biOrigin,biOwner,
                   biProviderMatch,biPayload,vars>>
BootProcessRestart ==
    /\ biRestarts > 0
    /\ biWriter' = "booting" /\ biRestarts' = biRestarts - 1 /\ biWake' = FALSE
    /\ UNCHANGED <<biReceived,biHeld,biAccepted,biVisible,biPermanent,biCheckpoint,
                   biExecutions,biHalts,biOrigin,biOwner,biProviderMatch,biPayload,vars>>
BootExecute(i) ==
    /\ biReady /\ biWake /\ i \in biHeld /\ i \notin biPermanent
    /\ biExecutions[i] = 0
    /\ biExecutions' = [biExecutions EXCEPT ![i] = @ + 1]
    /\ biHeld' = biHeld \ {i}
    /\ UNCHANGED <<biReceived,biAccepted,biVisible,biPermanent,biCheckpoint,
                   biWriter,biWake,biRestarts,biHalts,biOrigin,biOwner,
                   biProviderMatch,biPayload,vars>>
BootRest == UNCHANGED <<bootIngressVars,vars>>
LegacyBootNext == BootGatewayTransient \/ BootGatewayReady \/ BootPermanentReject \/
            BootCatchUp \/ BootGatewayRedelivery \/ BootBeginResume \/ BootReady \/
            BootHalt \/ BootRefreshHost \/ BootProcessRestart \/
            (\E i \in Ids : BootExecute(i)) \/ BootRest
BootNext == LegacyBootNext /\ UNCHANGED inputReadiness
BootSpec == Init /\ [][BootNext]_<<vars,bootIngressVars>>
            /\ WF_<<vars,legacyBootIngressVars>>(BootBeginResume)
            /\ WF_<<vars,legacyBootIngressVars>>(BootReady)
            /\ WF_<<vars,legacyBootIngressVars>>(BootRefreshHost)
            /\ (\A i \in Ids : WF_<<vars,legacyBootIngressVars>>(BootExecute(i)))
BootNoSilentLoss == biReceived \subseteq biHeld \cup biDone \cup biVisible
BootNoDuplicate == \A i \in Ids : biExecutions[i] <= 1
BootCheckpointBacked == biCheckpoint \subseteq biAccepted \cup biDone
BootPermanentNeverExecutes == \A i \in biPermanent : biExecutions[i] = 0
BootPayloadIntact == \A i \in biAccepted : biPayload[i] = "original"
BootHeldEventuallyExecutes == \A i \in Ids : (i \in biHeld /\ biReady) ~> (i \in biDone)

\* Readiness-only refinement holds the older custody/lifecycle state fixed.
ReadinessNext == Readiness!Next /\ UNCHANGED <<vars, legacyBootIngressVars>>
ReadinessSpec == Init /\ [][ReadinessNext]_<<vars, bootIngressVars>> /\ Readiness!Fairness
ReadyNoModalOpenWake == Readiness!NoModalOpenWake
ReadyModalCloseEventuallySends == Readiness!ModalCloseEventuallySends
ReadyTypeOK == Readiness!TypeOK
ReadyNoDuplicate == Readiness!NoDuplicate
ReadyNoSilentLoss == Readiness!NoSilentLoss
ReadyNoIdleRunningInjection == Readiness!NoIdleRunningInjection
ReadyNoHumanDraftExecution == Readiness!NoHumanDraftExecution
ReadyNoEarlyInput == Readiness!NoEarlyInput
ReadyNoClearAuthorityViolation == Readiness!NoClearAuthorityViolation
ReadyClearHeldEventuallySends == Readiness!ClearHeldEventuallySends
ReadyCompactAfterStopIdle == Readiness!CompactAfterStopIdle
ReadyPendingEventuallyExecutes == Readiness!PendingEventuallyExecutes
ReadyCompactEventuallyExecutes == Readiness!CompactEventuallyExecutes
ReadySelfCompactEventuallyReleases == Readiness!SelfCompactEventuallyReleases
ReadyNoContinuationEarlyInput == Readiness!NoContinuationEarlyInput
ReadyBusyKeyPlanPreserved == Readiness!BusyKeyPlanPreserved
ReadyBusyEventuallyQueues == Readiness!BusyEventuallyQueues

=============================================================================
