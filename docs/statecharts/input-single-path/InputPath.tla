---------------------------- MODULE InputPath ----------------------------
(* #7272 single input path for one Claude TUI channel: every Discord input is admitted to
   the owner queue, then one drive task prepares it (Empty -> direct, readable draft -> C-s,
   modal or unreadable -> Held), keys it (C-s, paste, Enter) and settles it.
   Landed code: origin/main 0a9b3f99cf. Unlanded 4f design: design-4f-r1 (+ carry P1).
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
    legacyLeft

vars == <<arrived, queue, head, dl, retU, retNS, drv, cur, wake, heldWait, booting, restarts,
          comp, stash, humanLeft, modal, modalLeft, prov, provQ, stopLeft, busySent, fate,
          consumed, order, harm, legacyLeft>>
envVars == <<humanLeft, modal, modalLeft, stopLeft, legacyLeft>>

Init ==
    /\ arrived = 0 /\ queue = <<>> /\ head = "none" /\ dl = "pending"
    /\ retU = {} /\ retNS = {}
    /\ drv = [k \in 1..2 |-> "idle"] /\ cur = [k \in 1..2 |-> 0]
    /\ wake = FALSE /\ heldWait = FALSE /\ booting = FALSE /\ restarts = 1
    /\ comp = {} /\ stash = {} /\ humanLeft = 1 /\ modal = FALSE /\ modalLeft = 1
    /\ prov \in {"idle", "busy"} /\ provQ = <<>> /\ stopLeft = 1
    /\ busySent = {} /\ fate = [i \in Ids |-> "none"] /\ consumed = [i \in Ids |-> 0]
    /\ order = <<>> /\ harm = "none" /\ legacyLeft = 1

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
                   order, harm>>

\* The human types a draft. Not while a drive task is between its read and its Enter: no pane is
\* read after a key (stash.rs:1), so that race is outside this model.
HumanTypes ==
    /\ humanLeft > 0 /\ ~Keying /\ 0 \notin comp \cup stash
    /\ humanLeft' = humanLeft - 1 /\ comp' = comp \cup {0}
    /\ UNCHANGED <<arrived, queue, head, dl, retU, retNS, drv, cur, wake, heldWait, booting,
                   restarts, stash, modal, modalLeft, prov, provQ, stopLeft, busySent, fate,
                   consumed, order, harm, legacyLeft>>

ModalOpens ==
    /\ modalLeft > 0 /\ ~modal /\ ~Keying
    /\ modalLeft' = modalLeft - 1 /\ modal' = TRUE
    /\ UNCHANGED <<arrived, queue, head, dl, retU, retNS, drv, cur, wake, heldWait, booting,
                   restarts, comp, stash, humanLeft, prov, provQ, stopLeft, busySent, fate,
                   consumed, order, harm, legacyLeft>>

\* With HeldWakes the hold's own wake fires when the modal closes or the next read succeeds.
ModalCloses ==
    /\ modal /\ modal' = FALSE
    /\ IF HeldWakes /\ heldWait THEN heldWait' = FALSE /\ wake' = TRUE ELSE UNCHANGED <<heldWait, wake>>
    /\ UNCHANGED <<arrived, queue, head, dl, retU, retNS, drv, cur, booting, restarts, comp, stash,
                   humanLeft, modalLeft, prov, provQ, stopLeft, busySent, fate, consumed, order,
                   harm, legacyLeft>>

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
                   busySent, harm>>

Quiet == queue = <<>> /\ head = "none" /\ AllIdle /\ prov = "idle" /\ provQ = <<>> /\ ~booting

Done ==
    /\ Quiet /\ arrived = N /\ ~wake
    /\ UNCHANGED vars

Next ==
    \/ Arrive \/ DriveFindsNothing
    \/ \E k \in 1..2 : DriveTakesFront(k) \/ Prepare(k) \/ ArmAndKey(k) \/ Paste(k)
                        \/ WriteUncertain(k) \/ Enter(k)
    \/ DeadlinePasses \/ Backstop \/ Restart \/ Boot
    \/ Absorb \/ TurnEnds \/ Stop
    \/ HumanTypes \/ ModalOpens \/ ModalCloses \/ HoldRetries \/ LegacyStarts
    \/ Done

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
=============================================================================
