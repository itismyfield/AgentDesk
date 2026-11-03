--------------------------- MODULE InputOwnership ---------------------------
(* #7676 A2+D: one Discord or turn/start input on one Claude tmux channel, from the owner drive
   to the post of its answer. Busy/idle is only a hint: the input is pasted once into the live
   pane, and who answers it is decided afterwards from the transcript row it produced.
   Code: a1c 0b0fbcbfbe (Fix = "main"), the first A2+D change 524adb6190 (Fix = "a2d1"), its
   repair e6d97a192f (Fix = "a2d2"), the second repair 4def1c6289 (Fix = "a2d3") and the third
   (Fix = "a2d4"), then slice C (Fix = "c", with only the C transitions a test drives). The channel is turn
   mode (the O writer answers every native turn) or legacy (the live relay, or a reader the
   input's own turn set up, answers).                                                        *)
EXTENDS Naturals

CONSTANTS
    Fix,   \* "main": a1c; "a2d1": first A2+D; "a2d2", "a2d3", "a2d4": its repairs; "c": slice C
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
    screen,    \* the pane's screen: ok, or held (modal, unread capture, copy mode)
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
    old        \* an older binary retained the input: none, told (its resend ask went out), untold

vars == <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain, sentInto, win,
          rcv, userRow, twin, claim, pastes, lp, op, screen, draft, notices, wake, timer, byTimer,
          timed, heldBy, watch, aged, twinPend, envLeft, stale, old>>
\* What the Claude rows and answers touch; the drive and screen keep the rest.
drv == <<screen, draft, notices, timer, byTimer, timed, reader, src, twin, heldBy, watch, aged,
         twinPend, stale, old>>

Init ==
    /\ mode \in {"turn", "legacy"}
    /\ src \in {"discord", "start"}
    /\ \E s \in {<<"idle", "idle", "none">>, <<"idle", "busy", "relay">>,
                 <<"active", "busy", "ad">>, <<"active", "idle", "none">>} :
         mbx = s[1] /\ nat = s[2] /\ natOwner = s[3]
    /\ old \in IF Fix = "c" THEN {"none", "told", "untold"} ELSE {"none"}
    /\ inp = (IF old = "none" THEN "queued" ELSE "retained") /\ pend = FALSE /\ reader = FALSE /\ obsOk \in BOOLEAN
    /\ route = "none" /\ plain = FALSE /\ sentInto = "idle"
    /\ win = "open" /\ rcv = "none" /\ userRow = FALSE /\ twin \in BOOLEAN /\ claim = "none"
    /\ pastes = 0 /\ lp = 0 /\ op = 0
    /\ screen \in {"ok", "held"} /\ draft \in {"none", "unproven"} /\ notices = 0
    /\ wake = (old = "none") /\ timer = FALSE /\ byTimer = FALSE /\ timed = FALSE /\ envLeft = 1
    /\ heldBy = "none" /\ watch = FALSE /\ aged = FALSE /\ twinPend \in BOOLEAN
    /\ stale \in IF Fix \in {"a2d3", "a2d4", "c"} THEN BOOLEAN ELSE {FALSE}
    \* A kept input's boot is checked on one plain channel: no twin, draft, held screen or old watch.
    /\ old # "none" => ~twin /\ ~twinPend /\ draft = "none" /\ screen = "ok" /\ ~stale

A2D == Fix \in {"a2d1", "a2d2", "a2d3", "a2d4", "c"}
R2 == Fix \in {"a2d2", "a2d3", "a2d4", "c"}
R3 == Fix \in {"a2d3", "a2d4", "c"}
\* a2d4 keeps the watch through a failed read and ends a watch without touching a newer hold.
R4 == Fix \in {"a2d4", "c"}
\* Slice C: on a turn-mode channel the presence supervisor reads the owner's V2 front and wakes the
\* owner once at the idle transition (supervisor_tests). Legacy channels are not assumed woken.
RC == Fix = "c" /\ Mut # "noBoundaryWake"
Waiting == inp \in {"queued", "held"}

Paste(r, pl, pd) ==
    /\ inp' = "sent" /\ route' = r /\ plain' = pl /\ pend' = pd
    /\ sentInto' = nat /\ pastes' = pastes + 1 /\ win' = "open"

\* The owned busy entry: a held screen sends no key and, from a2d3, leaves a watch of that pane;
\* otherwise one paste, and an unparked draft owes its original one notice (a2d2; the outbox key
\* is the original's).
OwnedPaste(r, pd, rd) ==
    IF screen = "held"
    THEN /\ inp' = "held" /\ timer' = (~R2 \/ Mut = "timer") /\ heldBy' = "screen"
         /\ watch' = (R3 /\ Mut # "noWatch")
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
    /\ Handoff /\ src = "discord" /\ mbx = "active"
    /\ IF A2D \/ nat = "busy"
       THEN OwnedPaste("busyOwned", Mut = "pendOnBusy", FALSE)
       ELSE Stays("retained", "none")
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, obsOk, rcv, userRow, twin, claim, lp, op,
                   screen, draft, aged, twinPend, envLeft, stale, old>>

\* start_idle -> intake: a transcript-busy pane with an idle mailbox. main: the front is deferred
\* to the next wake. a2d: the same owned busy entry pastes now.
DriveIdleBusyPane ==
    /\ Handoff /\ src = "discord" /\ mbx = "idle" /\ nat = "busy"
    /\ IF A2D
       THEN OwnedPaste("busyOwned", Mut = "pendOnBusy", FALSE)
       ELSE Stays("held", "other")
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, obsOk, rcv, userRow, twin, claim, lp, op,
                   screen, draft, aged, twinPend, envLeft, stale, old>>

\* A turn/start's headless turn on a busy pane: execute_owned. main and a2d1 fix busy=false, so
\* an absent idle observer ends the turn NotSent("idle_unobserved") with a failure notice (the
\* 11-02 iMessage loss). a2d2 reads the pane busy and pastes through the owned busy entry, with a
\* reader of the row the input takes set up before the paste.
DriveStartBusy ==
    /\ Handoff /\ src = "start" /\ mbx = "idle" /\ nat = "busy"
    /\ IF R2 /\ Mut # "startIdle"
       THEN OwnedPaste("busyStart", TRUE, TRUE) /\ mbx' = "active"
       ELSE IF obsOk
       THEN /\ Paste("idleObserved", TRUE, TRUE) /\ reader' = TRUE /\ mbx' = "active"
            /\ heldBy' = "none" /\ UNCHANGED <<notices, timer, watch>>
       ELSE Stays("failed", "none") /\ UNCHANGED mbx
    /\ UNCHANGED <<mode, src, nat, natOwner, obsOk, rcv, userRow, twin, claim, lp, op, screen,
                   draft, aged, twinPend, envLeft, stale, old>>

\* An idle pane with its observer: the warm submit records the Answer declaration and the turn's
\* observer reads the AI turn it starts. Without an observer the owner refuses (idle_unobserved)
\* in every model: a Claude observer is missing only when the transcript cannot be resolved, so
\* no source proves where a paste would land. The front waits for the next wake (slice C).
DriveIdle ==
    /\ Handoff /\ mbx = "idle" /\ nat = "idle"
    /\ IF obsOk
       THEN /\ Paste("idleObserved", TRUE, TRUE) /\ reader' = TRUE /\ mbx' = "active"
            /\ heldBy' = "none" /\ UNCHANGED <<notices, timer, watch>>
       ELSE Stays("held", "other") /\ UNCHANGED mbx
    /\ UNCHANGED <<mode, src, nat, natOwner, obsOk, rcv, userRow, twin, claim, lp, op, screen,
                   draft, aged, twinPend, envLeft, stale, old>>

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

\* Pasted into a busy Claude: Claude queues it (enqueue row only).
RowEnqueued ==
    /\ inp = "sent" /\ sentInto = "busy"
    /\ inp' = "enq"
    /\ rcv' = IF win = "open" THEN "enqueue" ELSE rcv
    /\ UNCHANGED <<mode, mbx, nat, natOwner, pend, obsOk, route, plain, sentInto, win, userRow,
                   claim, pastes, lp, op, wake, envLeft, stale>>
    /\ UNCHANGED drv

\* Claude takes the queued prompt into the running turn: a queued_command attachment row.
Absorb ==
    /\ inp = "enq" /\ nat = "busy"
    /\ inp' = "absorbed"
    /\ rcv' = IF win = "open" THEN "attachment" ELSE rcv
    /\ UNCHANGED <<mode, mbx, nat, natOwner, pend, obsOk, route, plain, sentInto, win, userRow,
                   claim, pastes, lp, op, wake, envLeft, stale>>
    /\ UNCHANGED drv

\* The receipt thread stops reading after its window.
WindowPasses ==
    /\ win = "open" /\ inp \notin {"queued", "held", "retained", "failed"}
    /\ win' = "passed"
    /\ UNCHANGED <<mode, mbx, nat, natOwner, inp, pend, obsOk, route, plain, sentInto, rcv,
                   userRow, claim, pastes, lp, op, wake, envLeft, stale>>
    /\ UNCHANGED drv

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

\* a2d mutant: the window passed with only an enqueue and the input is called Handed.
TimeHands ==
    /\ Mut = "timeHanded" /\ A2D /\ mode = "legacy"
    /\ claim = "none" /\ inp = "enq" /\ win = "passed"
    /\ claim' = "handed"
    /\ UNCHANGED <<mode, mbx, nat, natOwner, inp, pend, obsOk, route, plain, sentInto, win, rcv,
                   userRow, pastes, lp, op, wake, envLeft, stale>>
    /\ UNCHANGED drv

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

\* A person starts a turn at the terminal; the live relay owns it.
HumanStartsTurn ==
    /\ envLeft > 0 /\ nat = "idle" /\ inp # "sent"
    /\ envLeft' = envLeft - 1 /\ nat' = "busy" /\ natOwner' = "relay"
    /\ UNCHANGED <<mode, mbx, inp, pend, obsOk, route, plain, sentInto, win, rcv, userRow,
                   claim, pastes, lp, op, wake>>
    /\ UNCHANGED drv

\* The mailbox's own turn ends without a transcript turn (rollback, cancel).
MailboxReleases ==
    /\ mbx = "active" /\ nat = "idle" /\ inp # "sent"
    /\ mbx' = "idle" /\ wake' = TRUE
    /\ UNCHANGED <<mode, nat, natOwner, inp, pend, obsOk, route, plain, sentInto, win, rcv,
                   userRow, claim, pastes, lp, op, envLeft, stale>>
    /\ UNCHANGED drv

\* The held screen clears with no event of its own (a modal or copy mode closed).
ScreenClears ==
    /\ screen = "held"
    /\ screen' = "ok"
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, draft, notices, wake,
                   timer, byTimer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old>>

\* The watch reads the held pane: a screen that would now submit wakes the drive once (a2d3).
\* Mutant watchBlind wakes on a read of the unchanged screen.
WatchSees ==
    /\ watch /\ inp = "held" /\ (screen = "ok" \/ Mut = "watchBlind")
    /\ watch' = FALSE /\ wake' = TRUE /\ byTimer' = (~wake /\ screen = "held")
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft,
                   notices, timer, timed, heldBy, aged, twinPend, envLeft, stale, old>>

\* One read of the held pane or of the queue fails and tmux, or the owner, answers again next time.
\* a2d3 ends the watch on it; a2d4 keeps its probe, so the read changes nothing.
WatchMisreads ==
    /\ watch /\ inp = "held" /\ (Fix = "a2d3" \/ Mut = "unknownEnds")
    /\ watch' = FALSE
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft,
                   notices, wake, timer, byTimer, timed, heldBy, aged, twinPend, envLeft, stale, old>>

\* The earlier front's watch ends because that front no longer leads. a2d3 drops the channel's
\* watch with it, this input's hold included; a2d4 ends only its own generation.
StaleWatchEnds ==
    /\ stale /\ stale' = FALSE
    /\ watch' = IF R4 /\ Mut # "noGeneration" THEN watch ELSE FALSE
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft,
                   notices, wake, timer, byTimer, timed, heldBy, aged, twinPend, envLeft, old>>

\* The watch ends once the held front no longer leads (another wake delivered it).
WatchEnds ==
    /\ watch /\ inp # "held"
    /\ watch' = FALSE
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft,
                   notices, wake, timer, byTimer, timed, heldBy, aged, twinPend, envLeft, stale, old>>

\* More than 10 s pass between the paste and the relay reading the row (a long running turn):
\* pending text recorded for the input, or for an absorbed twin, expires.
DeclarationAges ==
    /\ ~aged /\ (pend \/ twinPend) /\ ~plain /\ claim = "none"
    /\ inp \in {"sent", "enq", "absorbed", "newturn"}
    /\ aged' = TRUE
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft,
                   notices, wake, timer, byTimer, timed, heldBy, watch, twinPend, envLeft, stale, old>>

\* The drive's fixed recheck fires (main and a2d1 drive.rs: sleep(recheck) -> wake).
TimerFires ==
    /\ timer
    /\ timer' = FALSE /\ wake' = TRUE /\ byTimer' = (~wake /\ inp = "held")
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft,
                   notices, timed, heldBy, watch, aged, twinPend, envLeft, stale, old>>

\* A wake with nothing the owner may take.
DriveFindsNothing ==
    /\ wake /\ ~Waiting
    /\ wake' = FALSE /\ byTimer' = FALSE
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, inp, pend, reader, obsOk, route, plain,
                   sentInto, win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft,
                   notices, timer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old>>

\* Slice C's boot requeues an input an older binary retained before its resend ask went out, then
\* wakes the owner (notice_tests). One whose ask went out stays retained; mutant requeueTold moves it.
BootRequeues ==
    /\ Fix = "c" /\ inp = "retained" /\ (old = "untold" \/ Mut = "requeueTold")
    /\ inp' = "queued" /\ wake' = TRUE
    /\ UNCHANGED <<mode, src, mbx, nat, natOwner, pend, reader, obsOk, route, plain, sentInto,
                   win, rcv, userRow, twin, claim, pastes, lp, op, screen, draft, notices, timer,
                   byTimer, timed, heldBy, watch, aged, twinPend, envLeft, stale, old>>

Quiet == nat = "idle" /\ ~wake /\ ~timer
         /\ inp \in {"queued", "held", "retained", "failed", "answered"}
         /\ envLeft = 0 /\ mbx = "idle" /\ ~ENABLED BootRequeues

Done ==
    /\ Quiet
    /\ UNCHANGED vars

Next ==
    \/ DriveBusy \/ DriveIdleBusyPane \/ DriveStartBusy \/ DriveIdle \/ DriveFindsNothing
    \/ RowOpensTurn \/ RowEnqueued \/ Absorb \/ WindowPasses
    \/ Observe \/ TimeHands \/ TurnEnds
    \/ HumanStartsTurn \/ MailboxReleases \/ ScreenClears \/ TimerFires
    \/ WatchSees \/ WatchEnds \/ WatchMisreads \/ StaleWatchEnds \/ DeclarationAges \/ Done
    \/ BootRequeues

Spec == Init /\ [][Next]_vars

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
\* 9. An input held by its screen is not left waiting once that screen would submit: something
\*    still pending (a wake or the pane's watch) hands it again.
HeldResumes == ~(Quiet /\ inp = "held" /\ heldBy = "screen" /\ screen = "ok" /\ ~watch)
\* 10. On a turn-mode channel with an idle observer, nothing is left waiting once all is quiet: a
\*     turn AgentDesk did not start ends with a wake of the waiting front.
NoSilentLossObserved == (Quiet /\ obsOk /\ mode = "turn") => inp = "answered" \/ old = "told"
\* 11. An input whose resend ask an older binary posted is never pasted again by the owner.
ToldStays == old = "told" => pastes = 0
=============================================================================
