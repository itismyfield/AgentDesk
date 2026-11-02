--------------------------- MODULE InputOwnership ---------------------------
(* #7676 A2+D: one Discord input on one Claude tmux channel, from the owner drive to the post of
   its answer. Busy/idle is only a hint: the input is pasted once into the live pane, and which
   relay answers it is decided afterwards from the transcript row it produced.
   Code: a1c 0b0fbcbfbe (Fix = "main") and the A2+D change (Fix = "a2d").
   The channel is turn mode (the O writer answers every native turn) or legacy (the live relay
   or the Discord turn's own observer answers).                                            *)
EXTENDS Naturals

CONSTANTS
    Fix,   \* "main": a1c; "a2d": the A2+D change
    Mut    \* one guard removed in the a2d model, "none" as written

VARIABLES
    mode,      \* turn or legacy, fixed per channel
    mbx,       \* the mailbox holds an AgentDesk turn: idle or active
    nat,       \* the native Claude turn read from the transcript: idle or busy
    natOwner,  \* who answers the running native turn: none, ad (its observer), adNoObs (an
               \* Answer declaration nobody reads), relay (the live relay), o (the O writer)
    inp,       \* queued, held, retained, sent, enq, absorbed, newturn, answered
    pend,      \* an Answer declaration (pending Discord prompt) was recorded for the input
    obsOk,     \* the idle path can build the provider observer (else idle_unobserved)
    route,     \* the path that pasted the input: none, busyOwned, idleObserved, idleUnobserved
    plain,     \* the pasted bytes are the plain prompt (idle submit), not the inject frame
    sentInto,  \* nat at the paste: idle (a user row follows) or busy (Claude queues it)
    win,       \* the 3 s receipt thread window: open or passed
    rcv,       \* last row kind the receipt thread saw inside its window
    userRow,   \* a non-meta user row for the input exists after the registered offset
    claim,     \* the live relay's decision for the input row: none, joined, handed, deferred
    pastes,    \* paste+Enter count
    lp, op,    \* legacy relay / O writer answer posts for the input
    wake,      \* an owner drive wake is pending
    envLeft    \* direct terminal turns a human may still start

vars == <<mode, mbx, nat, natOwner, inp, pend, obsOk, route, plain, sentInto, win, rcv, userRow,
          claim, pastes, lp, op, wake, envLeft>>

Init ==
    /\ mode \in {"turn", "legacy"}
    /\ \E s \in {<<"idle", "idle", "none">>, <<"idle", "busy", "relay">>,
                 <<"active", "busy", "ad">>, <<"active", "idle", "none">>} :
         mbx = s[1] /\ nat = s[2] /\ natOwner = s[3]
    /\ inp = "queued" /\ pend = FALSE /\ obsOk \in BOOLEAN
    /\ route = "none" /\ plain = FALSE /\ sentInto = "idle"
    /\ win = "open" /\ rcv = "none" /\ userRow = FALSE /\ claim = "none"
    /\ pastes = 0 /\ lp = 0 /\ op = 0 /\ wake = TRUE /\ envLeft = 1

A2D == Fix = "a2d"
Waiting == inp \in {"queued", "held"}
TheTurnOwner == IF mode = "turn" THEN "o" ELSE natOwner

Paste(r, pl, pd) ==
    /\ inp' = "sent" /\ route' = r /\ plain' = pl /\ pend' = pd
    /\ sentInto' = nat /\ pastes' = pastes + 1 /\ win' = "open"

\* -------------------------------------------------------------------------- the owner drive
\* owner_input.rs:146 hand_busy: the mailbox holds a turn. main: inject_owned is BusyOnly
\* (submit.rs:30) and a transcript-idle pane is NotBusy -> retain_not_sent ("send again").
\* a2d: the owned busy entry pastes whatever the transcript reads and declares no Answer.
DriveBusy ==
    /\ wake /\ Waiting /\ mbx = "active"
    /\ wake' = FALSE
    /\ IF A2D \/ nat = "busy"
       THEN Paste("busyOwned", FALSE, Mut = "pendOnBusy")
       ELSE /\ inp' = "retained"
            /\ UNCHANGED <<route, plain, pend, sentInto, pastes, win>>
    /\ UNCHANGED <<mode, mbx, nat, natOwner, obsOk, rcv, userRow, claim, lp, op, envLeft>>

\* start_idle -> intake: a transcript-busy pane with an idle mailbox. main (a1c): the front is
\* deferred to the next wake (intake_turn.rs:1720). a2d: the same owned busy entry pastes now.
DriveIdleBusyPane ==
    /\ wake /\ Waiting /\ mbx = "idle" /\ nat = "busy"
    /\ wake' = FALSE
    /\ IF A2D
       THEN Paste("busyOwned", FALSE, Mut = "pendOnBusy")
       ELSE /\ inp' = "held"
            /\ UNCHANGED <<route, plain, pend, sentInto, pastes, win>>
    /\ UNCHANGED <<mode, mbx, nat, natOwner, obsOk, rcv, userRow, claim, lp, op, envLeft>>

\* An idle pane with its observer: the warm submit records the Answer declaration and the
\* Discord turn's observer reads the AI turn it starts (owner_input.rs:560, submission.rs:165).
\* Without an observer the owner refuses (idle_unobserved) in both models: a Claude observer is
\* missing only when the transcript cannot be resolved (warm_followup.rs:567-573), so no source
\* proves where a paste would land. The front waits for the next wake.
DriveIdle ==
    /\ wake /\ Waiting /\ mbx = "idle" /\ nat = "idle"
    /\ wake' = FALSE
    /\ IF obsOk
       THEN /\ Paste("idleObserved", TRUE, TRUE) /\ mbx' = "active"
       ELSE /\ inp' = "held" /\ UNCHANGED <<mbx, route, plain, pend, sentInto, pastes, win>>
    /\ UNCHANGED <<mode, nat, natOwner, obsOk, rcv, userRow, claim, lp, op, envLeft>>

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
                   wake, envLeft>>

\* Pasted into a busy Claude: Claude queues it (enqueue row only).
RowEnqueued ==
    /\ inp = "sent" /\ sentInto = "busy"
    /\ inp' = "enq"
    /\ rcv' = IF win = "open" THEN "enqueue" ELSE rcv
    /\ UNCHANGED <<mode, mbx, nat, natOwner, pend, obsOk, route, plain, sentInto, win, userRow,
                   claim, pastes, lp, op, wake, envLeft>>

\* Claude takes the queued prompt into the running turn: a queued_command attachment row.
Absorb ==
    /\ inp = "enq" /\ nat = "busy"
    /\ inp' = "absorbed"
    /\ rcv' = IF win = "open" THEN "attachment" ELSE rcv
    /\ UNCHANGED <<mode, mbx, nat, natOwner, pend, obsOk, route, plain, sentInto, win, userRow,
                   claim, pastes, lp, op, wake, envLeft>>

\* The receipt thread stops reading after its window (busy_inject.rs:495).
WindowPasses ==
    /\ win = "open" /\ inp \notin {"queued", "held", "retained"}
    /\ win' = "passed"
    /\ UNCHANGED <<mode, mbx, nat, natOwner, inp, pend, obsOk, route, plain, sentInto, rcv,
                   userRow, claim, pastes, lp, op, wake, envLeft>>

\* ---------------------------------------------------------- who answers the input's turn
\* The relay (legacy) or O (turn mode) reads the input's row once.
Answerer ==
    IF mode = "turn" THEN "o"
    ELSE IF inp = "absorbed" THEN natOwner                       \* joins the running turn
    \* the idle submit's plain prompt: the pending declaration hands it to its observer
    ELSE IF plain THEN (IF pend THEN "ad" ELSE "relay")
    \* main: every recorded Claude frame is Joined(false) (injected_steer.rs:131), owning nothing
    ELSE IF ~A2D \/ Mut = "blanketJoin" THEN "none"
    \* a2d mutant: decide from the receipt thread's kind; a row after the window is never seen
    ELSE IF Mut = "receiptKind" /\ rcv # "user" THEN "none"
    ELSE IF Mut = "timeHanded" THEN "relay"
    \* a2d: a user row after the offset is Handed; an Answer declaration would suppress it
    ELSE IF pend THEN "adNoObs" ELSE "relay"

Observe ==
    /\ claim = "none" /\ inp \in {"absorbed", "newturn"}
    /\ claim' = IF inp = "absorbed" THEN "joined"
                ELSE IF Answerer \in {"none"} THEN "deferred" ELSE "handed"
    /\ natOwner' = IF inp = "absorbed" THEN natOwner ELSE Answerer
    /\ UNCHANGED <<mode, mbx, nat, inp, pend, obsOk, route, plain, sentInto, win, rcv, userRow,
                   pastes, lp, op, wake, envLeft>>

\* a2d mutant: the window passed with only an enqueue and the input is called Handed.
TimeHands ==
    /\ Mut = "timeHanded" /\ A2D /\ mode = "legacy"
    /\ claim = "none" /\ inp = "enq" /\ win = "passed"
    /\ claim' = "handed"
    /\ UNCHANGED <<mode, mbx, nat, natOwner, inp, pend, obsOk, route, plain, sentInto, win, rcv,
                   userRow, pastes, lp, op, wake, envLeft>>

Posts(owner) ==
    IF mode = "turn"
    THEN /\ op' = op + 1 /\ lp' = lp + (IF Mut = "turnLegacy" THEN 1 ELSE 0)
    ELSE /\ lp' = lp + (IF owner \in {"ad", "relay"} THEN 1 ELSE 0) /\ UNCHANGED op

\* The running native turn ends. A turn AgentDesk started publishes its completion (wake);
\* one it did not start may not wake the owner drive.
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
    /\ \E w \in (IF natOwner \in {"ad", "adNoObs"} \/ mbx = "active" THEN {TRUE} ELSE BOOLEAN) :
         wake' = (wake \/ w)
    /\ UNCHANGED <<mode, pend, obsOk, route, plain, sentInto, win, claim, pastes, envLeft>>

\* A person starts a turn at the terminal; the live relay owns it.
HumanStartsTurn ==
    /\ envLeft > 0 /\ nat = "idle" /\ inp # "sent"
    /\ envLeft' = envLeft - 1 /\ nat' = "busy" /\ natOwner' = "relay"
    /\ UNCHANGED <<mode, mbx, inp, pend, obsOk, route, plain, sentInto, win, rcv, userRow,
                   claim, pastes, lp, op, wake>>

\* The mailbox's own turn ends without a transcript turn (rollback, cancel).
MailboxReleases ==
    /\ mbx = "active" /\ nat = "idle" /\ inp # "sent"
    /\ mbx' = "idle" /\ wake' = TRUE
    /\ UNCHANGED <<mode, nat, natOwner, inp, pend, obsOk, route, plain, sentInto, win, rcv,
                   userRow, claim, pastes, lp, op, envLeft>>

\* A wake with nothing the owner may take.
DriveFindsNothing ==
    /\ wake /\ ~Waiting
    /\ wake' = FALSE
    /\ UNCHANGED <<mode, mbx, nat, natOwner, inp, pend, obsOk, route, plain, sentInto, win, rcv,
                   userRow, claim, pastes, lp, op, envLeft>>

Quiet == nat = "idle" /\ ~wake /\ inp \in {"queued", "held", "retained", "answered"}
         /\ envLeft = 0 /\ mbx = "idle"

Done ==
    /\ Quiet
    /\ UNCHANGED vars

Next ==
    \/ DriveBusy \/ DriveIdleBusyPane \/ DriveIdle \/ DriveFindsNothing
    \/ RowOpensTurn \/ RowEnqueued \/ Absorb \/ WindowPasses
    \/ Observe \/ TimeHands \/ TurnEnds
    \/ HumanStartsTurn \/ MailboxReleases \/ Done

Spec == Init /\ [][Next]_vars

\* ------------------------------------------------------------------------------------ rules
\* 1. The input is pasted at most once.
NoDuplicate == pastes <= 1
\* 2. Once nothing else will happen, the input was answered; it is not left waiting or retained.
NoSilentLoss == Quiet => inp = "answered"
\* 3. No input ends asking the person to send it again.
NoRetryRequired == inp # "retained"
\* 4. An answered input was posted exactly once, by the O writer alone on a turn-mode channel.
AnswerOnce == inp = "answered" =>
    IF mode = "turn" THEN op = 1 /\ lp = 0 ELSE lp = 1 /\ op = 0
\* 5. Time alone never makes the input Handed: a Handed claim has its user row.
HandedHasRow == claim = "handed" => userRow
=============================================================================
