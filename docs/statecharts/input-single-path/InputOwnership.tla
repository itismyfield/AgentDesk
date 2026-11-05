--------------------------- MODULE InputOwnership ---------------------------
(* #7676 A2+D: one Discord or turn/start input on one Claude tmux channel, from the owner drive
   to the post of its answer. Busy/idle is only a hint: the input is pasted once into the live
   pane, and who answers it is decided afterwards from the transcript row it produced.
   Code: a1c 0b0fbcbfbe (Fix = "main"), the first A2+D change 524adb6190 (Fix = "a2d1"), its
   repair e6d97a192f (Fix = "a2d2"), the second repair 4def1c6289 (Fix = "a2d3") and the third
   (Fix = "a2d4"), then slice C (CX, with only the C transitions a test drives). "cf" is main
   after C (a9d59259e5) with slice F's paths: a restart-carried row, an absorbed input the transcript
   reader never settles, and the same text handed to the session outside AgentDesk; "f" is main
   after F (40c556ae6c); "f4" adds the close's queue wake when the row had no restored episode;
   "f5" lets an intake start whose admission removed a marked row retry its create once.
   "a2b" is f5 with #7340 S2-A2b: a start meeting a native busy turn waits unclaimed, woken by the
   turn-mode idle transition or, on legacy, by a read of that turn's end. A failed read of that pane
   keeps the watch (r3); Mut "nativeReadGone" is r2b, which read the failure as a gone pane.
   "a7772" is a2b with #7772: a read that cannot tell holds a start as a busy one does, before
   its claim and again before its paste, and a start held after its claim is watched too.
   The channel is turn
   mode (the O writer answers every native turn) or legacy (the live relay, or a reader the
   input's own turn set up, answers).                                                        *)
EXTENDS Naturals

CONSTANTS
    Fix,   \* "main": a1c; "a2d1": first A2+D; "a2d2", "a2d3", "a2d4": its repairs; "c": slice C;
           \* "cf": main after C with F's paths; "f": main after F; "f4": its close wake;
           \* "f5": a marked row's start retried; "a2b": f5 with a start waiting out a native busy turn;
           \* "a7772": a2b holding a start on an unknown read too, and watching a late hold
           \* "observerBefore" / "observerWake": the isolated late-observer slice F6
    Mut    \* one guard removed in the a2d4 model, "none" as written

VARIABLES
    mode,      \* turn or legacy, fixed per channel
    src,       \* discord (a person's message) or start (a turn/start such as an iMessage reply)
    mbx,       \* the mailbox holds an AgentDesk turn: idle or active
    nat,       \* the native Claude turn read from the transcript: idle or busy
    natOwner,  \* who answers the running native turn: none, ad (a reader that reads it), adNoObs
               \* (an Answer declaration nobody reads), relay (the live relay), o (the O writer)
    inp,       \* queued, held, retained, failed, sent, enq, absorbed, newturn, answered
    pend,      \* an Answer declaration (pending Discord prompt) was recorded for the input
    reader,    \* a reader of the input's answer was set up before its paste
    obsOk,     \* the idle observer can be built (else idle_unobserved)
    route,     \* the path that pasted the input: none, busyOwned, busyStart, idleObserved
    plain,     \* the pasted bytes are the plain prompt (idle submit), not the inject frame
    sentInto,  \* nat at the paste: idle (a user row follows) or busy (Claude queues it)
    win,       \* the 3 s receipt thread window: open or passed
    rcv,       \* last row kind the receipt thread saw inside its window
    userRow,   \* a non-meta user row for the input exists after the registered offset
    twin,      \* an earlier input with the same author and body has its own row in the transcript
    claim,     \* the live relay's decision for the input row: none, joined, handed, deferred
    pastes,    \* paste+Enter count
    lp, op,    \* legacy-side / O writer answer posts for the input
    screen,    \* what refuses a paste before any key: ok; held, the pane's screen (modal, unread
               \* capture, copy mode); cause, a lock, a buffer load or a missing transcript; gone,
               \* the pane a cause was read through is gone; ledger, the frame ledger refused a busy
               \* paste (a LoadFailed no read sees)
    draft,     \* a person's composer text the owned paste meets: none, or unproven (C-s can't park)
    notices,   \* draft notices sent to the input's original
    wake,      \* an owner drive wake is pending
    timer,     \* a fixed-period recheck is pending
    byTimer,   \* the pending wake came from that timer
    timed,     \* a handoff ran because of the timer alone
    heldBy,    \* why the input is held: none, screen (a refusing screen) or other
    watch,     \* a watch of the refusing pane's screen is armed
    aged,      \* the pending text of a declaration outlived its 10 s window before the row
    twinPend,  \* an earlier identical input's declaration is still pending (it was absorbed)
    envLeft,   \* direct terminal turns a person may still start
    stale,     \* an earlier held front's watch still runs on the channel when this input is held
    old,       \* an older binary retained the input: none, told (its resend ask went out), untold
    acct,      \* the channel's live idle pane runs under another account than the channel's
    row,       \* a durable inflight row on the channel: none, carried (an earlier generation's) or
               \* marked (a finished turn's, kept until its origin receipt is recorded)
    ep,        \* restore re-minted the carried row's mailbox episode, which the close's finalizer ends
    leak,      \* its transcript holds an input Claude absorbed mid-turn with no queued_command row
    outside,   \* a person or another session handed the input's text to the session directly
    observer,  \* slice F6: late attachment and the drive right left by idle_unobserved
    nr,        \* a7772: the pane's native turn reads: ok, unknown (a read that cannot tell), spent
               \* (it read unknown once and recovered; one failure per run bounds the model)
    prov,      \* a7772: a late-held attempt's own unwind: none, mailbox (still held), row (its row)
    seen       \* a7772 ghost: what the read showed when a start was claimed: none, idle, busy, unknown

vars == <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain, sentInto, win,
          rcv, userRow, twin, claim, pastes, lp, op, screen, draft, notices, wake, timer, byTimer,
          timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct, row, ep, leak, outside, observer, nr, prov, seen>>
fvars == <<row, ep, leak, outside, observer, nr, prov, seen>>
\* What the Claude rows and answers touch; the drive and screen keep the rest.
drv == <<screen, draft, notices, timer, byTimer, timed, reader, src, twin, heldBy, watch, aged,
         twinPend, stale, old, acct>>

\* Slice C's transitions; "cf", "f", "f4", "f5" and "a2b" keep them.
CX == Fix \in {"c", "cf", "f", "f4", "f5", "a2b", "a7772"}
\* Slice F's paths are in the model.
F2 == Fix \in {"cf", "f", "f4", "f5", "a2b", "a7772"}

Init ==
    /\ mode \in {"turn", "legacy"}
    /\ src \in {"discord", "start"}
    /\ \E s \in {<<"idle", "idle", "none">>, <<"idle", "busy", "relay">>,
                 <<"active", "busy", "ad">>, <<"active", "idle", "none">>} :
         mbx = s[1] /\ nat = s[2] /\ natOwner = s[3]
    /\ old \in IF CX THEN {"none", "told", "untold"} ELSE {"none"}
    /\ inp = (IF old = "none" THEN "queued" ELSE "retained") /\ pend = FALSE /\ reader = FALSE
    \* In c the drive's observer finds the transcript as the live path does (account home, live
    \* binding), assumed present for an idle live pane; noObserver keeps the default-home lookup.
    /\ obsOk \in IF CX /\ Mut # "noObserver" THEN {TRUE} ELSE BOOLEAN
    /\ route = "none" /\ plain = FALSE /\ sentInto = "idle"
    /\ win = "open" /\ rcv = "none" /\ userRow = FALSE /\ twin \in BOOLEAN /\ claim = "none"
    /\ pastes = 0 /\ lp = 0 /\ op = 0
    /\ screen \in {"ok", "held", "cause"} \cup (IF CX THEN {"ledger"} ELSE {}) /\ draft \in {"none", "unproven"} /\ notices = 0
    /\ wake = (old = "none") /\ timer = FALSE /\ byTimer = FALSE /\ timed = FALSE
    /\ heldBy = "none" /\ watch = FALSE /\ aged = FALSE /\ twinPend \in BOOLEAN
    /\ stale \in IF Fix \in {"a2d3", "a2d4", "c", "cf", "f", "f4", "f5", "a2b", "a7772"} THEN BOOLEAN ELSE {FALSE}
    \* A kept input's boot is checked on one plain channel: no twin, draft, held screen or old watch.
    /\ old # "none" => ~twin /\ ~twinPend /\ draft = "none" /\ screen = "ok" /\ ~stale
    \* A ledger refusal is checked on a busy turn-mode pane, whose turn end wakes the owner (RC).
    /\ screen = "ledger" => nat = "busy" /\ mode = "turn"
    \* In c a live idle pane may run under another account after the channel's account changed;
    \* no person starts a turn in it during the check.
    /\ acct \in IF CX THEN BOOLEAN ELSE {FALSE}
    /\ acct => mbx = "idle" /\ nat = "idle" /\ old = "none" /\ screen = "ok" /\ ~stale
    /\ envLeft = IF acct THEN 0 ELSE 1
    \* An earlier turn's row a restart carried (restore_inflight reattaches a watcher to a live pane,
    \* re-intakes nothing): checked on a plain turn-mode channel whose mailbox holds no turn.
    \* A marked row: the turn ended and released its mailbox, but its origin receipt failed to record,
    \* so the row stays stamped for the next admission to record and remove (receipt_settle.rs).
    \* Checked with the receipt store healthy again, from f4 on.
    /\ row \in IF F2 THEN {"none", "carried"} \cup (IF Fix \in {"f4", "f5", "a2b", "a7772"} THEN {"marked"} ELSE {})
              ELSE {"none"}
    /\ leak \in IF F2 THEN BOOLEAN ELSE {FALSE}
    /\ ep \in IF F2 THEN BOOLEAN ELSE {FALSE}
    /\ ep => row = "carried"
    /\ outside = FALSE
    /\ nr = "ok" /\ prov = "none" /\ seen = "none"
    /\ observer = [attached |-> Mut = "observerInitiallyAttached",
                    initialAttached |-> Mut = "observerInitiallyAttached",
                    pending |-> Mut # "observerNoFront", initialPending |-> Mut # "observerNoFront",
                    wake |-> Mut # "observerNoFront", running |-> FALSE, turnHeld |-> FALSE, observed |-> FALSE,
                    frontOutstanding |-> FALSE, rejected |-> FALSE, pastes |-> 0,
                    attachWakes |-> 0, bindings |-> (IF Mut = "observerInitiallyAttached" THEN 1 ELSE 0)]
    /\ row # "none" => mode = "turn" /\ mbx = "idle" /\ old = "none" /\ ~acct /\ screen = "ok"
                         /\ ~stale /\ ~twin /\ ~twinPend /\ draft = "none"

A2D == Fix \in {"a2d1", "a2d2", "a2d3", "a2d4", "c", "cf", "f", "f4", "f5", "a2b", "a7772"}
R2 == Fix \in {"a2d2", "a2d3", "a2d4", "c", "cf", "f", "f4", "f5", "a2b", "a7772"}
R3 == Fix \in {"a2d3", "a2d4", "c", "cf", "f", "f4", "f5", "a2b", "a7772"}
\* a2d4 keeps the watch through a failed read and ends a watch without touching a newer hold.
R4 == Fix \in {"a2d4", "c", "cf", "f", "f4", "f5", "a2b", "a7772"}
\* Slice C: on a turn-mode channel the presence supervisor reads the owner's V2 front and wakes the
\* owner once at the idle transition (supervisor_tests). Legacy channels are not assumed woken.
RC == CX /\ Mut # "noBoundaryWake"
\* A start's read of its pane, which an unknown read hides (owner_input.rs native_read).
A77 == Fix = "a7772"
View == IF nr = "unknown" THEN "unknown" ELSE nat
Waiting == inp \in {"queued", "held"}

Paste(r, pl, pd) ==
    /\ inp' = "sent" /\ route' = r /\ plain' = pl /\ pend' = pd
    /\ sentInto' = nat /\ pastes' = pastes + 1 /\ win' = "open"

\* The owned busy entry: a held screen sends no key and, from a2d3, leaves a watch of that pane;
\* in c a refusing cause leaves a read of that cause too (noCauseWatch removes it), but a ledger
\* refusal leaves none and waits for the turn's end (watchLedger arms a load read for it);
\* otherwise one paste, and an unparked draft owes its original one notice (a2d2; the outbox key
\* is the original's).
OwnedPaste(r, pd, rd) ==
    IF screen # "ok"
    THEN /\ inp' = "held" /\ timer' = (~R2 \/ Mut = "timer") /\ heldBy' = "screen"
         /\ watch' = (R3 /\ Mut # "noWatch"
                      /\ (screen = "held"
                          \/ (CX /\ Mut # "noCauseWatch" /\ screen # "ledger")
                          \/ (screen = "ledger" /\ Mut = "watchLedger")))
         /\ UNCHANGED <<route, plain, pend, sentInto, pastes, win, notices, reader>>
    ELSE /\ Paste(r, FALSE, pd) /\ reader' = rd /\ UNCHANGED <<timer, watch>>
         /\ heldBy' = "none"
         /\ notices' = IF R2 /\ draft = "unproven" /\ Mut # "noNotice" THEN 1 ELSE notices
\* A branch that neither pastes nor holds on the screen.
Stays(i, why) ==
    /\ inp' = i /\ heldBy' = why
    /\ UNCHANGED <<route, plain, pend, sentInto, pastes, win, notices, reader, timer, watch>>

\* Every handoff records whether only the timer woke it.
Handoff == wake /\ Waiting /\ wake' = FALSE /\ byTimer' = FALSE /\ timed' = (timed \/ byTimer)

\* -------------------------------------------------------------------------- the owner drive
\* hand_busy: the mailbox holds a turn. main: inject_owned is BusyOnly, and a transcript-idle
\* pane is NotBusy -> retain_not_sent ("send again"). a2d: the owned busy entry pastes whatever
\* the transcript reads and declares no Answer. A start waits for an idle mailbox.
DriveBusy ==
    /\ Handoff /\ ~acct /\ src = "discord" /\ mbx = "active"
    /\ IF A2D \/ nat = "busy"
       THEN OwnedPaste("busyOwned", Mut = "pendOnBusy", FALSE)
       ELSE Stays("retained", "none")
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, obsOk, rcv, userRow, twin, claim, lp, op,
                   screen, draft, aged, twinPend, envLeft, stale, old, acct>>
    /\ UNCHANGED fvars

\* An idle start meeting a marked row records and removes it. A headless start then retries its
\* create (foreign_row admit_headless); an intake start up to f4 drops itself (admit), f5 retries
\* once. An owned busy paste creates no row, so a marked row neither holds nor settles it.
Retries == src = "start" \/ (Fix \in {"f5", "a2b", "a7772"} /\ Mut # "noRetry")
RowClear == row = "none" \/ (row = "marked" /\ Retries)

\* start_idle -> intake: a transcript-busy pane with an idle mailbox. main: the front is deferred
\* to the next wake. a2d: the same owned busy entry pastes now.
DriveIdleBusyPane ==
    /\ Handoff /\ ~acct /\ src = "discord" /\ mbx = "idle" /\ nat = "busy"
    /\ row # "carried"
    /\ IF A2D
       THEN OwnedPaste("busyOwned", Mut = "pendOnBusy", FALSE)
       ELSE Stays("held", "other")
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, obsOk, rcv, userRow, twin, claim, lp, op,
                   screen, draft, aged, twinPend, envLeft, stale, old, acct>>
    /\ IF Mut = "ownerClearsMarked"
       THEN /\ row' = "none" /\ UNCHANGED <<ep, leak, outside, observer, nr, prov, seen>>
       ELSE UNCHANGED fvars

\* A turn/start's headless turn on a busy pane: execute_owned. main and a2d1 fix busy=false, so
\* an absent idle observer ends the turn NotSent("idle_unobserved") with a failure notice (the
\* 11-02 iMessage loss). a2d2 reads the pane busy and pastes through the owned busy entry, with a
\* reader of the row the input takes set up before the paste.
DriveStartBusy ==
    /\ Handoff /\ ~acct /\ src = "start" /\ mbx = "idle" /\ nat = "busy"
    /\ RowClear /\ (Fix \notin {"a2b", "a7772"} \/ Mut = "startPastes")
    /\ IF R2 /\ Mut # "startIdle"
       THEN OwnedPaste("busyStart", TRUE, TRUE) /\ mbx' = "active"
       ELSE IF obsOk
       THEN /\ Paste("idleObserved", TRUE, TRUE) /\ reader' = TRUE /\ mbx' = "active"
            /\ heldBy' = "none" /\ UNCHANGED <<notices, timer, watch>>
       ELSE Stays("failed", "none") /\ UNCHANGED mbx
    /\ UNCHANGED <<mode, src, nat, natOwner, obsOk, rcv, userRow, twin, claim, lp, op, screen,
                   draft, aged, twinPend, envLeft, stale, old, acct>>
    /\ row' = IF Mut = "headlessKeepsMarked" THEN row ELSE "none"
    /\ UNCHANGED <<ep, leak, outside, observer, nr, prov, seen>>

\* a2b: a start meeting a native busy turn waits unclaimed and pastes nothing. On a turn-mode
\* channel the supervisor's idle transition wakes it (RC); on legacy the hold leaves a read of that
\* turn's end with the held watch (owner_input.rs native_turn). Mutant nativeNoWatch leaves none.
\* No admission runs, so a marked row stays for the start's later admission (foreign_row.rs).
DriveStartWaits ==
    /\ Handoff /\ ~acct /\ src = "start" /\ mbx = "idle" /\ prov = "none"
    /\ IF A77 /\ mode = "legacy"
       THEN View \in (IF Mut = "earlyUnknownGoes" THEN {"busy"} ELSE {"busy", "unknown"})
       ELSE nat = "busy"
    /\ row # "carried" /\ Fix \in {"a2b", "a7772"} /\ Mut # "startPastes"
    /\ inp' = "held" /\ heldBy' = "native"
    /\ watch' = (mode = "legacy" /\ Mut # "nativeNoWatch")
    /\ UNCHANGED <<route, plain, pend, sentInto, pastes, win, notices, reader, timer>>
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, obsOk, rcv, userRow, twin, claim, lp, op, screen,
                   draft, aged, twinPend, envLeft, stale, old, acct>>
    /\ UNCHANGED fvars

\* An idle pane with its observer: the warm submit records the Answer declaration and the turn's
\* observer reads the AI turn it starts. Without an observer the owner refuses (idle_unobserved)
\* in every model: a Claude observer is missing only when the transcript cannot be resolved, so
\* no source proves where a paste would land. The front waits for the next wake (slice C).
\* In c a pane under another account is no execution of the channel: the live path's driver
\* replaces it and the owner writes the new pane's first prompt, read as an idle paste's turn.
\* Mutant noRelaunch keeps the pane as idle and refuses unobserved.
DriveIdle ==
    /\ Handoff /\ mbx = "idle" /\ nat = "idle" /\ prov = "none"
    /\ ~(A77 /\ src = "start" /\ ~acct)
    /\ RowClear
    /\ IF acct /\ Mut # "noRelaunch"
       THEN /\ Paste("idleObserved", TRUE, TRUE) /\ reader' = TRUE /\ mbx' = "active"
            /\ heldBy' = "none" /\ acct' = FALSE /\ UNCHANGED <<notices, timer, watch>>
       ELSE IF obsOk /\ ~acct
       THEN /\ Paste("idleObserved", TRUE, TRUE) /\ reader' = TRUE /\ mbx' = "active"
            /\ heldBy' = "none" /\ UNCHANGED <<notices, timer, watch, acct>>
       ELSE Stays("held", "other") /\ UNCHANGED <<mbx, acct>>
    /\ UNCHANGED <<mode, src, nat, natOwner, obsOk, rcv, userRow, twin, claim, lp, op, screen,
                   draft, aged, twinPend, envLeft, stale, old>>
    /\ row' = "none" /\ UNCHANGED <<ep, leak, outside, observer, nr, prov, seen>>

\* a7772: a start whose read shows idle is claimed; its headless turn reads the pane again before
\* registering (execute_owned). A turn-mode channel's own pane is read by its activity, not the
\* transcript (start_waits). Mutant earlyUnknownGoes claims on an unknown read too.
DriveStartClaims ==
    /\ Handoff /\ A77 /\ ~acct /\ src = "start" /\ mbx = "idle" /\ prov = "none"
    /\ row # "carried"
    /\ IF mode = "legacy"
       THEN View \in (IF Mut = "earlyUnknownGoes" THEN {"idle", "unknown"} ELSE {"idle"})
       ELSE nat = "idle"
    /\ inp' = "claimed" /\ seen' = (IF mode = "legacy" THEN View ELSE nat)
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, pend, reader, obsOk, route, plain, sentInto,
                   win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft, notices, timer,
                   heldBy, watch, aged, twinPend, envLeft, stale, old, acct, row, ep, leak,
                   outside, observer, nr, prov>>

\* The late read shows idle: the claimed start submits as an idle paste and its admission settles a
\* marked row. Mutant lateUnknownIdle submits on an unknown read too.
DriveStartRuns ==
    /\ A77 /\ inp = "claimed"
    /\ View \in (IF Mut = "lateUnknownIdle" THEN {"idle", "unknown"} ELSE {"idle"})
    /\ IF obsOk
       THEN /\ Paste("idleObserved", TRUE, TRUE) /\ reader' = TRUE /\ mbx' = "active"
            /\ heldBy' = "none" /\ UNCHANGED <<notices, timer, watch>>
       ELSE Stays("held", "other") /\ UNCHANGED mbx
    /\ row' = "none"
    /\ UNCHANGED <<mode, src, nat, natOwner, obsOk, rcv, userRow, twin, claim, lp, op, screen,
                   draft, aged, twinPend, envLeft, stale, old, acct, wake, byTimer, timed>>
    /\ UNCHANGED <<ep, leak, outside, observer, nr, prov, seen>>

\* The late read shows busy or unknown: the claimed start is held unsent with a watch of its pane,
\* and its attempt unwinds (mailbox, then row). Mutant lateNoWatch leaves no watch (main).
DriveStartLateHolds ==
    /\ A77 /\ inp = "claimed"
    /\ View \notin (IF Mut = "lateUnknownIdle" THEN {"idle", "unknown"} ELSE {"idle"})
    /\ inp' = "held" /\ heldBy' = "native" /\ watch' = (Mut # "lateNoWatch") /\ prov' = "mailbox"
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, pend, reader, obsOk, route, plain, sentInto,
                   win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft, notices, wake,
                   timer, byTimer, timed, aged, twinPend, envLeft, stale, old, acct, row, ep, leak,
                   outside, observer, nr, seen>>

\* The held attempt's unwind: its mailbox turn is released, then its own inflight row is cleared.
\* Neither step wakes the owner.
AttemptReleases ==
    /\ prov = "mailbox" /\ prov' = "row"
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain, sentInto,
                   win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft, notices, wake,
                   timer, byTimer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct,
                   row, ep, leak, outside, observer, nr, seen>>

AttemptRowClears ==
    /\ prov = "row" /\ prov' = "none"
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain, sentInto,
                   win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft, notices, wake,
                   timer, byTimer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct,
                   row, ep, leak, outside, observer, nr, seen>>

\* A wake before that unwind ends meets the attempt's mailbox turn or row and is spent unsent.
DriveMeetsUnwind ==
    /\ Handoff /\ A77 /\ prov # "none"
    /\ Stays(inp, heldBy)
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, obsOk, rcv, userRow, twin, claim, lp, op,
                   screen, draft, aged, twinPend, envLeft, stale, old, acct, row, ep, leak,
                   outside, observer, nr, prov, seen>>

\* The pane's read cannot tell once (an unreadable pane or transcript), then recovers.
ReadBreaks ==
    /\ A77 /\ nr = "ok" /\ inp \in {"queued", "claimed", "held"} /\ nr' = "unknown"
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain, sentInto,
                   win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft, notices, wake,
                   timer, byTimer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct,
                   row, ep, leak, outside, observer, prov, seen>>

ReadRecovers ==
    /\ nr = "unknown" /\ nr' = "spent"
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain, sentInto,
                   win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft, notices, wake,
                   timer, byTimer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct,
                   row, ep, leak, outside, observer, prov, seen>>

\* ------------------------------------------------------------------- Claude and its rows
\* Pasted into an idle Claude, or queued by a turn that has since ended: a user row opens a new
\* native turn.
RowOpensTurn ==
    /\ \/ inp = "sent" /\ sentInto = "idle"
       \/ inp = "enq"
    /\ nat = "idle"
    /\ inp' = "newturn" /\ nat' = "busy" /\ natOwner' = "none" /\ userRow' = TRUE
    /\ rcv' = IF win = "open" THEN "user" ELSE rcv
    /\ UNCHANGED <<mode, mbx, pend, obsOk, route, plain, sentInto, win, claim, pastes, lp, op,
                   wake, envLeft, stale>>
    /\ UNCHANGED drv
    /\ UNCHANGED fvars

\* Pasted into a busy Claude: Claude queues it (enqueue row only).
RowEnqueued ==
    /\ inp = "sent" /\ sentInto = "busy"
    /\ inp' = "enq"
    /\ rcv' = IF win = "open" THEN "enqueue" ELSE rcv
    /\ UNCHANGED <<mode, mbx, nat, natOwner, pend, obsOk, route, plain, sentInto, win, userRow,
                   claim, pastes, lp, op, wake, envLeft, stale>>
    /\ UNCHANGED drv
    /\ UNCHANGED fvars

\* Claude takes the queued prompt into the running turn: a queued_command attachment row.
Absorb ==
    /\ inp = "enq" /\ nat = "busy"
    /\ inp' = "absorbed"
    /\ rcv' = IF win = "open" THEN "attachment" ELSE rcv
    /\ UNCHANGED <<mode, mbx, nat, natOwner, pend, obsOk, route, plain, sentInto, win, userRow,
                   claim, pastes, lp, op, wake, envLeft, stale>>
    /\ UNCHANGED drv
    /\ UNCHANGED fvars

\* The receipt thread stops reading after its window.
WindowPasses ==
    /\ win = "open" /\ inp \notin {"queued", "held", "retained", "failed"}
    /\ win' = "passed"
    /\ UNCHANGED <<mode, mbx, nat, natOwner, inp, pend, obsOk, route, plain, sentInto, rcv,
                   userRow, claim, pastes, lp, op, wake, envLeft, stale>>
    /\ UNCHANGED drv
    /\ UNCHANGED fvars

\* ---------------------------------------------------------- who answers the input's turn
\* Whether the relay reads the row as declared. Up to a2d2 it matched pending text kept 10 s, so
\* an aged declaration is lost and an absorbed twin's lingering one is taken for this input's;
\* a2d3 reads the declaration of the attempt that took the row.
Declared ==
    IF R3 /\ Mut \notin {"ttlDecl", "bodyDecl"} THEN pend
    ELSE IF Mut = "ttlDecl" THEN pend /\ ~aged
    ELSE IF Mut = "bodyDecl" THEN pend \/ (twin /\ twinPend)
    ELSE (pend \/ (twin /\ twinPend)) /\ ~aged

\* The relay (legacy) or O (turn mode) reads the input's row once.
Answerer ==
    IF mode = "turn" THEN "o"
    ELSE IF inp = "absorbed" THEN natOwner
    \* the idle submit's plain prompt: the pending declaration hands it to its observer
    ELSE IF plain THEN (IF pend THEN "ad" ELSE "relay")
    \* main: every recorded Claude frame is Joined(false), owning nothing
    ELSE IF ~A2D \/ Mut = "blanketJoin" THEN "none"
    \* a2d1 counts the attempts that could own the row: an identical earlier input makes two,
    \* and the row is Joined. a2d2 pairs rows with attempts in registration order.
    ELSE IF twin /\ (Fix = "a2d1" \/ Mut = "twoCandidates") THEN "none"
    \* a2d mutant: decide from the receipt thread's kind; a row after the window is never seen
    ELSE IF Mut = "receiptKind" /\ rcv # "user" THEN "none"
    ELSE IF Mut = "timeHanded" THEN "relay"
    \* a user row after the offset is Handed unless a declared reader answers it
    ELSE IF Declared THEN (IF reader THEN "ad" ELSE "adNoObs") ELSE "relay"

Observe ==
    /\ claim = "none" /\ inp \in {"absorbed", "newturn"}
    /\ claim' = IF inp = "absorbed" THEN "joined"
                ELSE IF Answerer \in {"none"} THEN "deferred" ELSE "handed"
    /\ natOwner' = IF inp = "absorbed" THEN natOwner ELSE Answerer
    /\ UNCHANGED <<mode, mbx, nat, inp, pend, obsOk, route, plain, sentInto, win, rcv, userRow,
                   pastes, lp, op, wake, envLeft, stale>>
    /\ UNCHANGED drv
    /\ UNCHANGED fvars

\* a2d mutant: the window passed with only an enqueue and the input is called Handed.
TimeHands ==
    /\ Mut = "timeHanded" /\ A2D /\ mode = "legacy"
    /\ claim = "none" /\ inp = "enq" /\ win = "passed"
    /\ claim' = "handed"
    /\ UNCHANGED <<mode, mbx, nat, natOwner, inp, pend, obsOk, route, plain, sentInto, win, rcv,
                   userRow, pastes, lp, op, wake, envLeft, stale>>
    /\ UNCHANGED drv
    /\ UNCHANGED fvars

\* On legacy the turn's owner posts it (the relay, or an AgentDesk reader). A reader set up for the
\* input posts what it read when the relay owns the turn its user row opened, and, up to a2d2,
\* also when the input was taken into a running turn someone else owns (a2d3 posts no body then).
ReaderPosts(owner) ==
    \/ inp = "newturn" /\ reader /\ owner # "ad"
    \/ inp = "absorbed" /\ reader /\ (~R3 \/ Mut = "commandReads")
Posts(owner) ==
    IF mode = "turn"
    THEN /\ op' = op + 1 /\ lp' = lp + (IF Mut = "turnLegacy" THEN 1 ELSE 0)
    ELSE /\ lp' = lp + (IF owner \in {"ad", "relay"} THEN 1 ELSE 0)
                     + (IF ReaderPosts(owner) THEN 1 ELSE 0)
         /\ UNCHANGED op

\* The running native turn ends. A turn AgentDesk started publishes its completion (wake); one it
\* did not start may not wake the owner drive.
TurnEnds ==
    /\ nat = "busy"
    /\ inp \in {"absorbed", "newturn"} => claim # "none"
    /\ IF inp = "enq"
       THEN \* the queued prompt opens the next turn
            /\ inp' = "newturn" /\ userRow' = TRUE /\ natOwner' = "none" /\ UNCHANGED <<nat, lp, op>>
            /\ rcv' = IF win = "open" THEN "user" ELSE rcv
       ELSE IF inp \in {"absorbed", "newturn"}
       THEN /\ Posts(natOwner) /\ inp' = "answered" /\ nat' = "idle" /\ natOwner' = "none"
            /\ UNCHANGED <<userRow, rcv>>
       ELSE /\ nat' = "idle" /\ natOwner' = "none" /\ UNCHANGED <<inp, userRow, rcv, lp, op>>
    /\ mbx' = "idle"
    /\ \E w \in (IF natOwner \in {"ad", "adNoObs"} \/ mbx = "active"
                    \/ (RC /\ Waiting /\ mode = "turn")
                 THEN {TRUE} ELSE BOOLEAN) :
         wake' = (wake \/ w)
    /\ UNCHANGED <<mode, pend, obsOk, route, plain, sentInto, win, claim, pastes, envLeft, stale>>
    /\ UNCHANGED drv
    /\ UNCHANGED fvars

\* A person starts a turn at the terminal; the live relay owns it.
HumanStartsTurn ==
    /\ envLeft > 0 /\ nat = "idle" /\ inp # "sent"
    /\ envLeft' = envLeft - 1 /\ nat' = "busy" /\ natOwner' = "relay"
    /\ UNCHANGED <<mode, mbx, inp, pend, obsOk, route, plain, sentInto, win, rcv, userRow,
                   claim, pastes, lp, op, wake>>
    /\ UNCHANGED drv
    /\ UNCHANGED fvars

\* The mailbox's own turn ends without a transcript turn (rollback, cancel).
MailboxReleases ==
    /\ mbx = "active" /\ nat = "idle" /\ inp # "sent"
    /\ mbx' = "idle" /\ wake' = TRUE
    /\ UNCHANGED <<mode, nat, natOwner, inp, pend, obsOk, route, plain, sentInto, win, rcv,
                   userRow, claim, pastes, lp, op, envLeft, stale>>
    /\ UNCHANGED drv
    /\ UNCHANGED fvars

\* The refusal clears with no event of its own (a modal closed, a lock freed, a load recovered).
ScreenClears ==
    /\ screen \in {"held", "cause", "ledger"}
    /\ screen' = "ok"
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, draft, notices, wake,
                   timer, byTimer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct>>
    /\ UNCHANGED fvars

\* The watch reads the held pane: a screen that would now submit wakes the drive once (a2d3).
\* Mutant watchBlind wakes on a read of the unchanged screen; a load read under a ledger refusal
\* sees a load that works while the ledger still refuses.
WatchSees ==
    /\ watch /\ inp = "held" /\ (screen \in {"ok", "ledger"} \/ Mut = "watchBlind")
    /\ (heldBy = "native" => nat = "idle")
    /\ (A77 /\ heldBy = "native" => (nr # "unknown" \/ Mut = "watchUnknownReady")
                                   /\ (prov = "none" \/ Mut = "lateNoUnwindWait"))
    \* A native hold's watch reads the turn's end, not a refusing screen.
    /\ watch' = FALSE /\ wake' = TRUE
    /\ byTimer' = (~wake /\ screen # "ok" /\ ~(A77 /\ heldBy = "native"))
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft,
                   notices, timer, timed, heldBy, aged, twinPend, envLeft, stale, old, acct>>
    /\ UNCHANGED fvars

\* One read of the held pane or of the queue fails and tmux, or the owner, answers again next time.
\* a2d3 ends the watch on it; a2d4 keeps its probe, so the read changes nothing.
WatchMisreads ==
    /\ watch /\ inp = "held" /\ (Fix = "a2d3" \/ Mut = "unknownEnds")
    /\ watch' = FALSE
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft,
                   notices, wake, timer, byTimer, timed, heldBy, aged, twinPend, envLeft, stale, old, acct>>
    /\ UNCHANGED fvars

\* A read of a native-held pane fails (tmux has-session or list-panes errs); tmux answers next time.
\* The watch keeps its probe; mutant nativeReadGone reads the failure as a gone pane and ends it.
NativeWatchMisreads ==
    /\ Fix \in {"a2b", "a7772"} /\ watch /\ inp = "held" /\ heldBy = "native"
    /\ watch' = (Mut # "nativeReadGone")
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft,
                   notices, wake, timer, byTimer, timed, heldBy, aged, twinPend, envLeft, stale, old, acct>>
    /\ UNCHANGED fvars

\* The earlier front's watch ends because that front no longer leads. a2d3 drops the channel's
\* watch with it, this input's hold included; a2d4 ends only its own generation.
StaleWatchEnds ==
    /\ stale /\ stale' = FALSE
    /\ watch' = IF R4 /\ Mut # "noGeneration" THEN watch ELSE FALSE
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft,
                   notices, wake, timer, byTimer, timed, heldBy, aged, twinPend, envLeft, old, acct>>
    /\ UNCHANGED fvars

\* A cause that clears before its watch's first read: in c that read still sees it and wakes the
\* drive once. Mutant clearedDrops is the arm that dropped a cause it already saw cleared.
CauseClearsBeforeRead ==
    /\ CX /\ watch /\ inp = "held" /\ screen = "cause"
    /\ screen' = "ok" /\ watch' = (Mut # "clearedDrops")
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, draft,
                   notices, wake, timer, byTimer, timed, heldBy, aged, twinPend, envLeft, stale, old, acct>>
    /\ UNCHANGED fvars

\* The pane a cause is read through goes away while that cause still stands.
PaneGoes ==
    /\ CX /\ watch /\ inp = "held" /\ screen = "cause"
    /\ screen' = "gone"
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, draft, watch,
                   notices, wake, timer, byTimer, timed, heldBy, aged, twinPend, envLeft, stale, old, acct>>
    /\ UNCHANGED fvars

\* A cause read through a gone pane ends its watch; mutant causeBlind reads the cause alone.
WatchLosesPane ==
    /\ watch /\ screen = "gone" /\ Mut # "causeBlind"
    /\ watch' = FALSE
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft,
                   notices, wake, timer, byTimer, timed, heldBy, aged, twinPend, envLeft, stale, old, acct>>
    /\ UNCHANGED fvars

\* The watch ends once the held front no longer leads (another wake delivered it).
WatchEnds ==
    /\ watch /\ inp # "held"
    /\ watch' = FALSE
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft,
                   notices, wake, timer, byTimer, timed, heldBy, aged, twinPend, envLeft, stale, old, acct>>
    /\ UNCHANGED fvars

\* More than 10 s pass between the paste and the relay reading the row (a long running turn):
\* pending text recorded for the input, or for an absorbed twin, expires.
DeclarationAges ==
    /\ ~aged /\ (pend \/ twinPend) /\ ~plain /\ claim = "none"
    /\ inp \in {"sent", "enq", "absorbed", "newturn"}
    /\ aged' = TRUE
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft,
                   notices, wake, timer, byTimer, timed, heldBy, watch, twinPend, envLeft, stale, old, acct>>
    /\ UNCHANGED fvars

\* The drive's fixed recheck fires (main and a2d1 drive.rs: sleep(recheck) -> wake).
TimerFires ==
    /\ timer
    /\ timer' = FALSE /\ wake' = TRUE /\ byTimer' = (~wake /\ inp = "held")
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft,
                   notices, timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct>>
    /\ UNCHANGED fvars

\* A wake with nothing the owner may take.
DriveFindsNothing ==
    /\ wake /\ ~Waiting
    /\ wake' = FALSE /\ byTimer' = FALSE
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft,
                   notices, timer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct>>
    /\ UNCHANGED fvars

\* Slice C's boot requeues an input an older binary retained before its resend ask went out, then
\* wakes the owner (notice_tests). One whose ask went out stays retained; mutant requeueTold moves it.
BootRequeues ==
    /\ CX /\ inp = "retained" /\ (old = "untold" \/ Mut = "requeueTold")
    /\ inp' = "queued" /\ wake' = TRUE
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, pend, reader, obsOk, route, plain, sentInto,
                   win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft, notices, timer,
                   byTimer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct>>
    /\ UNCHANGED fvars

\* ------------------------------------------------------------------------------ slice F
\* A front the owner hands while the mailbox is idle starts a turn whose create meets the carried
\* row: foreign_row refuses it (NotSent, not held) and the front keeps its place for a later wake.
\* A marked row the start does not retry past is removed all the same, and the wake is spent.
DriveMeetsRow ==
    /\ Handoff /\ F2 /\ mbx = "idle"
    /\ (row = "carried" \/ (row = "marked" /\ nat = "idle" /\ ~Retries))
    /\ Stays(inp, heldBy)
    /\ row' = IF row = "marked" THEN "none" ELSE row
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, obsOk, rcv, userRow, twin, claim, lp, op,
                   screen, draft, aged, twinPend, envLeft, stale, old, acct, ep, leak, outside, observer, nr, prov, seen>>

\* The carried-turn close archives the row once the transcript reads its native turn closed and
\* O owes nothing (carried_turn_close.rs try_close). In cf the reader counts an absorbed input
\* with no queued_command row as a run still owed, and the dequeue credit it leaves eats later
\* dequeues, so the close never reads a closed boundary. Mutant leakKept keeps that count in f.
\* With a restored episode the finalizer's mailbox release wakes the owner (QueueEligible ->
\* kickoff -> owner_wake); without one, up to f nothing wakes it. f4 kicks the queue after the
\* archive either way; mutant closeNoKick drops that kick.
CarriedCloses ==
    /\ row = "carried" /\ nat = "idle"
    /\ (~leak \/ (Fix \in {"f", "f4", "f5", "a2b", "a7772"} /\ Mut # "leakKept"))
    /\ row' = "none" /\ ep' = FALSE
    /\ wake' = (wake \/ ep \/ (Fix \in {"f4", "f5", "a2b", "a7772"} /\ Mut # "closeNoKick"))
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain, sentInto,
                   win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft, notices, timer,
                   byTimer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct, leak,
                   outside, observer, nr, prov, seen>>

\* While the input waits, a person types its text at the terminal or another session sends it over
\* its own socket. AgentDesk records no identity for that copy.
HandedOutside ==
    /\ F2 /\ Waiting /\ ~outside
    /\ outside' = TRUE
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain, sentInto,
                   win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft, notices, wake,
                   timer, byTimer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct,
                   row, ep, leak, observer, nr, prov, seen>>

\* Loss is judged once the pane's refusal cleared and no watch is left to read it: a screen that
\* never clears holds by design.
Quiet == nat = "idle" /\ ~wake /\ ~timer /\ screen = "ok" /\ ~watch /\ nr # "unknown" /\ prov = "none"
         /\ inp \in {"queued", "held", "retained", "failed", "answered"}
         /\ envLeft = 0 /\ mbx = "idle" /\ ~ENABLED BootRequeues /\ ~ENABLED CarriedCloses

\* A front whose pane is gone waits, unwatched, for a new event; nothing else is left to happen.
Done ==
    /\ \/ Quiet
       \/ nat = "idle" /\ ~wake /\ ~timer /\ mbx = "idle" /\ envLeft = 0 /\ screen = "gone" /\ ~watch
    /\ UNCHANGED vars

\* A carried row refuses every start the idle-mailbox drives make (DriveMeetsRow).
Next ==
    \/ DriveBusy \/ DriveFindsNothing
    \/ DriveIdleBusyPane \/ DriveStartBusy \/ DriveIdle \/ DriveStartWaits
    \/ RowOpensTurn \/ RowEnqueued \/ Absorb \/ WindowPasses
    \/ Observe \/ TimeHands \/ TurnEnds
    \/ HumanStartsTurn \/ MailboxReleases \/ ScreenClears \/ TimerFires
    \/ WatchSees \/ WatchEnds \/ WatchMisreads \/ StaleWatchEnds \/ DeclarationAges \/ Done
    \/ NativeWatchMisreads
    \/ DriveStartClaims \/ DriveStartRuns \/ DriveStartLateHolds \/ AttemptReleases
    \/ AttemptRowClears \/ DriveMeetsUnwind \/ ReadBreaks \/ ReadRecovers
    \/ CauseClearsBeforeRead \/ PaneGoes \/ WatchLosesPane
    \/ BootRequeues
    \/ DriveMeetsRow \/ CarriedCloses \/ HandedOutside

Spec == Init /\ [][Next]_vars
\* Liveness, checked by the *Live cfgs: weak fairness of Next as a whole, not of each action; runs
\* whose reads keep failing or whose native turn never ends are outside what it proves.
LiveSpec == Spec /\ WF_vars(Next)

\* ------------------------------------------------------------------------------------ rules
\* 1. The input is pasted at most once.
NoDuplicate == pastes <= 1
\* 2. Once nothing else will happen, the input was answered; it is not left waiting or retained.
NoSilentLoss == Quiet => inp = "answered" \/ old = "told"
\* 3. No input ends asking the person to send it again.
NoRetryRequired == inp = "retained" => old # "none"
\* 4. An answered input was posted exactly once, by the O writer alone on a turn-mode channel.
AnswerOnce == inp = "answered" =>
    IF mode = "turn" THEN op = 1 /\ lp = 0 ELSE lp = 1 /\ op = 0
\* 5. Time alone never makes the input Handed: a Handed claim has its user row.
HandedHasRow == claim = "handed" => userRow
\* 6. A turn/start into a busy pane is never dropped with a failure for want of an idle observer.
StartNotDropped == inp # "failed"
\* 7. A held input is handed again only by an event (turn boundary, idle, new input), never by
\*    a fixed-period timer alone.
NoTimedRetry == ~timed
\* 8. An owned paste beside a draft C-s could not park tells the original once.
DraftToldOnce ==
    /\ notices <= 1
    /\ (pastes = 1 /\ route \in {"busyOwned", "busyStart"} /\ draft = "unproven") => notices = 1
\* 9. An input held by its screen or a cause is not left waiting once that clears: something
\*    still pending (a wake or the pane's watch) hands it again.
HeldResumes == ~(Quiet /\ inp = "held" /\ heldBy = "screen" /\ screen = "ok" /\ ~watch)
\* 10. On a turn-mode channel with an idle observer, nothing is left waiting once all is quiet: a
\*     turn AgentDesk did not start ends with a wake of the waiting front.
NoSilentLossObserved == (Quiet /\ obsOk /\ mode = "turn") => inp = "answered" \/ old = "told"
\* 11. An input whose resend ask an older binary posted is never pasted again by the owner.
ToldStays == old = "told" => pastes = 0
\* 12. Nothing is pasted into a pane under another account than the channel's.
ForeignUntouched == acct => pastes = 0
\* 13. A watch whose pane is gone ends once nothing else is left to happen.
GoneWatchEnds == ~(nat = "idle" /\ ~wake /\ ~timer /\ mbx = "idle" /\ envLeft = 0
                   /\ screen = "gone" /\ watch /\ ~ENABLED WatchLosesPane)
\* 14. A finished turn's carried row does not stay once nothing else is left to happen. A marked
\*     row may: it holds no paste and the next idle start settles it.
CarriedRowEnds == Quiet => row # "carried"
\* 15. The session sees the input's text once: AgentDesk's paste or the outside copy, not both.
SeenOnce == pastes + (IF outside THEN 1 ELSE 0) <= 1
\* A headless admission records and clears the previous marked row before provider input.
HeadlessSettlesMarked == (src = "start" /\ pastes > 0) => row # "marked"
\* 16. A start a native busy turn holds is not left waiting once all is quiet.
NativeHeldResumes == ~(Quiet /\ inp = "held" /\ heldBy = "native")
\* 17. A start runs as a turn of its own: it is never pasted into a running turn.
StartOwnTurn == (src = "start" /\ pastes = 1) => sentInto = "idle"
\* 18. An answered start was pasted exactly once.
StartOnce == (src = "start" /\ inp = "answered") => pastes = 1
\* 19. (liveness) A start a native busy turn holds is handed again unless its pane is gone, which
\*     leaves it for the next event as Done does.
NativeHeldRuns ==
    (inp = "held" /\ heldBy = "native") ~> (inp # "held" \/ heldBy # "native" \/ screen = "gone")
\* 20. A start is claimed only on a read that showed its pane idle.
EarlyClaimHasIdleProof == seen \in {"none", "idle"}

\* ------------------------------------------------------------------------------ slice F6
\* The old columns keep their transitions; this bounded slice isolates boot's late observer.
ObserverLegacy == <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft,
                   notices, wake, timer, byTimer, timed, heldBy, watch, aged, twinPend, envLeft,
                   stale, old, acct, row, ep, leak, outside, nr, prov, seen>>
ObserverInit ==
    /\ Init
    /\ mode = "turn" /\ src = "discord" /\ mbx = "idle" /\ nat = "idle" /\ natOwner = "none"
    /\ old = "none" /\ obsOk /\ ~twin /\ screen = "ok" /\ draft = "none" /\ ~twinPend
    /\ ~stale /\ ~acct /\ row = "none" /\ ~ep /\ ~leak

\* drive.rs:98-124, 168-180; v2.rs:248-315. A new wake starts with stalled=false and
\* regrants the same front even when the refused pass left its claim outstanding.
ObserverDriveGrants ==
    /\ observer.wake /\ observer.pending /\ ~observer.running /\ ~observer.turnHeld
    /\ observer' = [observer EXCEPT !.wake = FALSE, !.running = TRUE,
                                   !.turnHeld = TRUE, !.observed = observer.attached, !.frontOutstanding = TRUE]
    /\ UNCHANGED ObserverLegacy

\* owner_input.rs:686-687 refuses before reserve or keys; the turn claim remains outstanding.
ObserverIdleUnobserved ==
    /\ observer.running /\ ~observer.observed
    /\ observer' = [observer EXCEPT !.running = FALSE, !.rejected = TRUE]
    /\ UNCHANGED ObserverLegacy

\* The local refusal settles before the bridge drops its mailbox token. The existing
\* turn-finished latch keeps an attachment kickoff pending until that token releases.
ObserverTurnReleases ==
    /\ observer.turnHeld /\ ~observer.running
    /\ observer' = [observer EXCEPT !.turnHeld = FALSE]
    /\ UNCHANGED ObserverLegacy

\* owner_input.rs:706-710 runs the reserved front with its observer exactly once.
ObserverSubmits ==
    /\ observer.running /\ observer.observed
    /\ observer' = [observer EXCEPT !.running = FALSE, !.pending = FALSE,
                                   !.frontOutstanding = FALSE, !.pastes = @ + 1]
    /\ UNCHANGED ObserverLegacy

\* Binding publication makes owner_observer resolvable and schedules the existing kickoff;
\* its pending wake also represents a kickoff waiting for the existing mailbox release latch.
\* Wake only with a pending front; reattaching the same source and an empty queue add no wake.
ObserverAttaches ==
    /\ observer.bindings < 2
    /\ LET kick == ~observer.attached /\ observer.pending /\ Fix = "observerWake"
                   /\ Mut # "observerNoWake"
       IN observer' = [observer EXCEPT !.attached = TRUE, !.bindings = @ + 1,
                        !.wake = @ \/ kick,
                        !.attachWakes = @ + (IF kick THEN (IF Mut = "observerDoubleWake" THEN 2 ELSE 1) ELSE 0)]
    /\ UNCHANGED ObserverLegacy

ObserverDone == UNCHANGED vars
ObserverNext == ObserverDriveGrants \/ ObserverIdleUnobserved \/ ObserverTurnReleases \/ ObserverSubmits
                \/ ObserverAttaches \/ ObserverDone
ObserverSpec == ObserverInit /\ [][ObserverNext]_vars
                /\ WF_vars(ObserverDriveGrants) /\ WF_vars(ObserverIdleUnobserved)
                /\ WF_vars(ObserverSubmits) /\ WF_vars(ObserverAttaches) /\ WF_vars(ObserverTurnReleases)

\* No original disappears, and a front whose observer is ready is never left without a wake.
ObserverNoSilentLoss == (observer.initialPending /\ ~observer.pending) => observer.pastes = 1
ObserverResumes == ~(observer.attached /\ observer.pending /\ ~observer.wake
                     /\ ~observer.running /\ ~observer.turnHeld)
ObserverExactlyOnce == observer.pastes <= 1
ObserverRefusalHasNoKeys == (observer.rejected /\ observer.pending) => observer.pastes = 0
ObserverAttachmentWakesOnce == observer.attachWakes <= 1
ObserverNoExtraWake == (~observer.initialPending \/ observer.initialAttached) => observer.attachWakes = 0
\* Checked separately with plain TLC: the CI invariant checker deliberately rejects PROPERTY.
ObserverEventuallyDelivered == (observer.attached /\ observer.pending) ~> observer.pastes = 1

\* A normal busy arrival while A's observer runs. The runtime binding exists, but Claude's
\* prelaunch inflight row has no output_path (provider_isolation.rs:155-164). The old hand_busy
\* uses that empty source; inject_locked refuses TranscriptUnavailable before a key, and
\* effect::run holds the original with a cause watch of that empty path.
F7Init == Init /\ mode = "turn" /\ src = "discord" /\ mbx = "active" /\ nat = "busy"
          /\ screen = "ok" /\ draft = "none" /\ obsOk /\ ~twin /\ ~twinPend

\* intake_gate.rs:975-990 -> turn_admission.rs:303-306 -> ingress.rs:326-333.
F7BusyArrival ==
    /\ inp = "queued"
    /\ inp' = "enq" /\ wake' = (Mut # "noArrivalWake")
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, pend, reader, obsOk, route, plain, sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft, notices, timer, byTimer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct, row, ep, leak, outside, observer, nr, prov, seen>>

\* owner_input.rs:158-164 -> hand_busy:206-227. A's local settlement already released its lane;
\* a bound live transcript lets B paste, while the empty row source holds with no durable attempt.
F7OwnerWakeHandBusy ==
    /\ inp = "enq" /\ wake
    /\ wake' = FALSE
    /\ IF Fix = "f7" /\ Mut # "noBusyBinding"
       THEN /\ Paste("busyOwned", FALSE, FALSE) /\ heldBy' = "none" /\ watch' = FALSE
       ELSE /\ inp' = "held" /\ heldBy' = "missingSource" /\ watch' = TRUE
            /\ UNCHANGED <<route, plain, pend, sentInto, pastes, win>>
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, reader, obsOk, rcv, userRow, twin, claim, lp, op, screen, draft, notices, timer, byTimer, timed, aged, twinPend, envLeft, stale, old, acct, row, ep, leak, outside, observer, nr, prov, seen>>

\* A wake is serviced before the turn closes. A held empty source cannot clear; only the turn
\* boundary's existing wake retries it (drive.rs:105-107; held_watch.rs:85-103).
F7HeldWaitsBoundary ==
    /\ nat = "busy" /\ inp \in {"enq", "held", "sent"} /\ ~wake
    /\ nat' = "idle" /\ mbx' = "idle" /\ natOwner' = "none"
    /\ wake' = TRUE /\ watch' = FALSE /\ envLeft' = 0
    /\ UNCHANGED <<mode, src, inp, pend, reader, obsOk, route, plain, sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft, notices, timer, byTimer, timed, heldBy, aged, twinPend, stale, old, acct, row, ep, leak, outside, observer, nr, prov, seen>>

\* With the source now available the boundary delivers a held original once on its next turn.
F7BoundaryDelivery ==
    /\ nat = "idle" /\ inp \in {"enq", "held"} /\ wake
    /\ inp' = "answered" /\ pastes' = pastes + 1 /\ op' = 1 /\ wake' = FALSE
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, pend, reader, obsOk, route, plain, sentInto, win, rcv, userRow, twin, claim, lp, screen, draft, notices, timer, byTimer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct, row, ep, leak, outside, observer, nr, prov, seen>>

F7Answer ==
    /\ nat = "idle" /\ inp = "sent"
    /\ inp' = "answered" /\ op' = 1 /\ wake' = FALSE
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, pend, reader, obsOk, route, plain, sentInto, win, rcv, userRow, twin, claim, pastes, lp, screen, draft, notices, timer, byTimer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct, row, ep, leak, outside, observer, nr, prov, seen>>

F7Done == inp = "answered" /\ UNCHANGED vars
F7Next == F7BusyArrival \/ F7OwnerWakeHandBusy \/ F7HeldWaitsBoundary
          \/ F7BoundaryDelivery \/ F7Answer \/ F7Done

\* No real screen, upload or ownership hold exists: B must have been pasted before A ends.
BusyArrivalInTurn == nat = "idle" => pastes = 1

\* Recovery and transition refusals happen before effect::run: no held watch owns a retry.
\* screen=cause is reconcile closed; screen=held is a transition lock. Only /resume and clear
\* wake on release; other short transitions keep the existing end-of-A delivery contract.
F7ResumeInit == Init /\ mode = "turn" /\ src = "discord" /\ mbx = "active"
                /\ nat = "busy" /\ draft = "none" /\ obsOk /\ ~twin /\ ~twinPend
                /\ screen = (IF Mut \in {"transition", "shortTransition"} THEN "held" ELSE "cause")

\* queued_recovery.rs:48-52 -> owner_input::recover; or arrival during /resume.
F7PreflightRefusal ==
    /\ inp = "queued" /\ wake /\ screen # "ok"
    /\ inp' = "held" /\ wake' = FALSE /\ claim' = "deferred" /\ notices' = 1
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, pend, reader, obsOk, route, plain, sentInto, win, rcv, userRow, twin, pastes, lp, op, screen, draft, timer, byTimer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct, row, ep, leak, outside, observer, nr, prov, seen>>

\* Active REST and gateway completions mark reconcile open, then wake adopted owners.
F7ReconcileComplete ==
    /\ claim = "deferred" /\ screen = "cause" /\ Mut # "standby"
    /\ screen' = "ok" /\ claim' = "handed"
    /\ wake' = (Fix = "f7r2")
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain, sentInto, win, rcv, userRow, twin, pastes, lp, op, draft, notices, timer, byTimer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct, row, ep, leak, outside, observer, nr, prov, seen>>

\* Standby runtime_bootstrap.rs:440-449 marks reconcile only, and cannot drive input.
F7StandbyReconcile ==
    /\ claim = "deferred" /\ screen = "cause" /\ Mut = "standby"
    /\ screen' = "ok" /\ claim' = "handed" /\ wake' = (Fix = "standbyWake")
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain, sentInto, win, rcv, userRow, twin, pastes, lp, op, draft, notices, timer, byTimer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct, row, ep, leak, outside, observer, nr, prov, seen>>

\* session_resume.rs:340 and commands/control.rs:514: /resume and clear release their guard
\* before their completion token wakes the owner. Other transition locks do not add a wake.
F7TransitionRelease ==
    /\ claim = "deferred" /\ screen = "held"
    /\ screen' = "ok" /\ claim' = "handed" /\ wake' = (Fix = "f7r2" /\ Mut = "transition")
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain, sentInto, win, rcv, userRow, twin, pastes, lp, op, draft, notices, timer, byTimer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct, row, ep, leak, outside, observer, nr, prov, seen>>

\* The same existing drive re-evaluates the kept original, claims it and calls hand_busy once.
F7ResumeHandBusy ==
    /\ nat = "busy"
    /\ claim = "handed" /\ inp = "held" /\ wake /\ screen = "ok"
    /\ inp' = "sent" /\ pastes' = pastes + 1 /\ wake' = FALSE
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, pend, reader, obsOk, route, plain, sentInto, win, rcv, userRow, twin, claim, lp, op, screen, draft, notices, timer, byTimer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct, row, ep, leak, outside, observer, nr, prov, seen>>

\* Check the completion pass while A remains busy, before any unrelated turn-boundary wake.
F7ResumeSettled ==
    /\ claim = "handed" /\ ~wake
    /\ (Mut # "shortTransition" \/ nat = "idle")
    /\ claim' = "joined"
    /\ UNCHANGED <<wake, mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain, sentInto, win, rcv, userRow, twin, pastes, lp, op, screen, draft, notices, timer, byTimer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old, acct, row, ep, leak, outside, observer, nr, prov, seen>>
F7ResumeDone == claim = "joined" /\ UNCHANGED vars
F7ResumeNext == F7PreflightRefusal \/ F7ReconcileComplete \/ F7StandbyReconcile
                \/ F7TransitionRelease \/ F7ResumeHandBusy \/ F7ResumeSettled \/ F7ResumeDone
                \/ (Mut = "shortTransition" /\ claim = "handed" /\ F7HeldWaitsBoundary)
                \/ (Mut = "shortTransition" /\ F7BoundaryDelivery)
ResumeProgress == claim = "joined" /\ Mut # "standby" =>
                  pastes = 1 /\ inp = (IF Mut = "shortTransition" THEN "answered" ELSE "sent")
StandbyDoesNotInject == Mut = "standby" => pastes = 0

\* Only automatic ingress/handoff calls share a notice by original. Direct held(reason)
\* calls keep their separate per-reason contract. All notice sends here have HTTP available.
AutoHoldFrame == <<mode, src, mbx, nat, natOwner, pend, reader, obsOk, route, plain, sentInto,
                   win, rcv, userRow, twin, pastes, lp, op, draft, timer, byTimer, timed, watch,
                   aged, twinPend, envLeft, stale, old, acct, row, ep, leak, outside, observer, nr, prov, seen>>
AutoHoldInit ==
    /\ Init /\ mode = "turn" /\ src = "discord" /\ mbx = "idle" /\ nat = "idle"
    /\ natOwner = "none" /\ old = "none" /\ ~obsOk /\ ~twin /\ ~twinPend
    /\ screen = "cause" /\ draft = "none" /\ ~stale /\ ~acct
    /\ row = "none" /\ ~ep /\ ~leak

\* ingress::held_once publishes a recovery reason selected before its exact-turn await;
\* a concurrent handoff can publish first. claim records which of these two calls ran.
AutoIngressHold ==
    /\ claim \in {"none", "handed"} /\ inp \in {"queued", "held"}
    /\ claim' = IF claim = "none" THEN "deferred" ELSE "joined"
    /\ notices' = notices + (IF Fix = "autoHoldOnce" /\ Mut # "autoHoldReasonKey"
                              /\ notices > 0 THEN 0 ELSE 1)
    /\ heldBy' = IF notices = 0 THEN "recovery" ELSE heldBy
    /\ UNCHANGED <<inp, wake, screen>> /\ UNCHANGED AutoHoldFrame

\* complete_active_reconcile opens recovery and issues the existing owner wake. screen
\* tracks this barrier only in this slice; the transcript still lacks an idle observer.
AutoRecoveryCompletes ==
    /\ screen = "cause"
    /\ screen' = "ok" /\ wake' = TRUE
    /\ UNCHANGED <<inp, notices, heldBy, claim>> /\ UNCHANGED AutoHoldFrame

\* idle_unobserved maps to ScreenMissing. The notice decision and record are one mutex
\* action, so the repaired automatic path keeps whichever reason won first.
AutoHandoffHold ==
    /\ screen = "ok" /\ wake /\ ~obsOk /\ claim \in {"none", "deferred"}
    /\ claim' = IF claim = "none" THEN "handed" ELSE "joined"
    /\ inp' = "held" /\ wake' = FALSE
    /\ notices' = notices + (IF Fix = "autoHoldOnce" /\ Mut # "autoHoldReasonKey"
                              /\ notices > 0 THEN 0 ELSE 1)
    /\ heldBy' = IF notices = 0 THEN "missingSource" ELSE heldBy
    /\ UNCHANGED screen /\ UNCHANGED AutoHoldFrame

AutoHoldDone == claim = "joined" /\ UNCHANGED vars
AutoHoldNext == AutoIngressHold \/ AutoRecoveryCompletes \/ AutoHandoffHold \/ AutoHoldDone
AutomaticHoldNoticeOnce == claim # "none" => notices = 1

=============================================================================
