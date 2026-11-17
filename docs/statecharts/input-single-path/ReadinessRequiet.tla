---- MODULE ReadinessRequiet ----
\* One restarted Claude source, one owner front, one readiness wait. Only the transcript, the
\* provider turn and its hook evidence move; no later input arrives to wake the held front.
EXTENDS Naturals
CONSTANTS Path, Policy, Mutant, StartTurn, Restorable, MetaKind, MaxMeta, Churn, PersonMay, Rounds
VARIABLES phase, turn, proof, restorable, marker, moved, metaLeft, personLeft, round,
          keys, retains, notices, bad
vars == <<phase, turn, proof, restorable, marker, moved, metaLeft, personLeft, round,
          keys, retains, notices, bad>>

Fixed == Policy = "Fixed" /\ Mutant # "StampOnceKeeps"
\* The transcript and the ledger: no open turn, no fence, no session marker at the tail.
ContentIdle == turn \in {"idle", "unrecorded"} /\ ~marker
\* A turn the transcript shows, or a pending/continuation/compact fence the ledger holds.
Open == turn \in {"running", "fenced"}
\* The screen shows a turn the transcript may not show yet.
PaneBusy == turn \in {"unrecorded", "running"}
\* Current evidence, or (Herdr) the restart proof over an idle screen.
Deliverable == turn = "idle" /\ ~marker /\ (proof \/ restorable)
\* What `Watched::unproven` reads apart from the Stamp.
Settleable == ~Deliverable /\ ContentIdle
SettleableM == IF Mutant = "IgnoreRunning" THEN ~Deliverable ELSE Settleable
PaneBusyM == Mutant # "IgnoreScreen" /\ PaneBusy
Exhausted == Mutant # "NoRoundCap" /\ round + 1 >= Rounds

Init == /\ phase = "restarted" /\ turn = StartTurn /\ proof = FALSE
        /\ restorable = (Path = "herdr" /\ Restorable /\ StartTurn = "idle")
        /\ marker = FALSE /\ moved = FALSE /\ metaLeft = MaxMeta /\ personLeft = PersonMay
        /\ round = 0 /\ keys = 0 /\ retains = 0 /\ notices = 0 /\ bad = {}

Env == <<turn, proof, restorable, marker, metaLeft, personLeft>>
Out == <<keys, retains, notices, bad>>

OwnerInput == /\ phase = "restarted" /\ phase' = "queued"
              /\ UNCHANGED <<Env, Out, moved, round>>
\* The wait's first look keeps the Stamp it compares at the budget's end.
StartWait == /\ phase = "queued" /\ phase' = "waiting" /\ moved' = FALSE /\ round' = 0
             /\ UNCHANGED <<Env, Out>>
Deliver == /\ phase \in {"waiting", "requiet"} /\ Deliverable
           /\ phase' = "done" /\ keys' = keys + 1
           /\ UNCHANGED <<Env, moved, round, retains, notices, bad>>
Retain(origin) ==
    /\ phase' = "retained" /\ retains' = retains + 1
    /\ bad' = bad \cup (IF Open THEN {"open"} ELSE {})
                  \cup (IF origin = "moved" /\ PaneBusy THEN {"busy"} ELSE {})
                  \cup (IF origin = "unmoved" /\ turn = "unrecorded" THEN {"gap"} ELSE {})
    /\ UNCHANGED <<Env, moved, round, keys, notices>>
Hold == /\ phase' = "held" /\ UNCHANGED <<Env, Out, moved, round>>
\* The budget ends without a deliverable proof.
Timeout == /\ phase = "waiting" /\ ~Deliverable
           /\ IF Settleable /\ ~moved THEN Retain("unmoved")
              ELSE IF Fixed /\ moved /\ SettleableM
                   THEN /\ phase' = "requiet" /\ moved' = FALSE /\ round' = 0
                        /\ UNCHANGED <<Env, Out>>
              ELSE Hold
\* One bounded window of the same wait on a fresh Stamp.
WindowEnds ==
    /\ phase = "requiet" /\ ~Deliverable
    /\ IF ~SettleableM THEN Hold
       ELSE IF ~moved \/ Exhausted
            THEN IF Mutant = "QuietSendsKey"
                 THEN /\ phase' = "done" /\ keys' = keys + 1 /\ bad' = bad \cup {"key"}
                      /\ UNCHANGED <<Env, moved, round, retains, notices>>
                 ELSE IF PaneBusyM \/ (Mutant = "ExhaustHolds" /\ moved) THEN Hold
                 ELSE Retain("moved")
       ELSE /\ round' = (IF Mutant = "NoRoundCap" THEN round ELSE round + 1)
            /\ moved' = FALSE /\ UNCHANGED <<phase, Env, Out>>
\* The held watch wakes the drive only on a proof; nothing wakes it by time.
Wake == /\ phase = "held" /\ Deliverable /\ phase' = "queued"
        /\ UNCHANGED <<Env, Out, moved, round>>
Notice == /\ phase = "retained" /\ phase' = "explained" /\ notices' = notices + 1
          /\ UNCHANGED <<Env, moved, round, keys, retains, bad>>

\* Session metadata appended by the idle pane; a marker (`permission-mode`) reads Unknown.
MetaMoves == /\ phase \in {"queued", "waiting", "requiet", "held"}
             /\ Churn \/ metaLeft > 0
             /\ moved' = TRUE
             /\ metaLeft' = IF Churn THEN metaLeft ELSE metaLeft - 1
             /\ marker' = (marker \/ MetaKind = "marker")
             /\ restorable' = (restorable /\ MetaKind # "marker")
             /\ UNCHANGED <<phase, turn, proof, personLeft, round, Out>>
\* A person's Enter whose hook this run lost: only the screen shows the turn at first.
PersonSubmits == /\ personLeft > 0 /\ turn = "idle" /\ phase \in {"waiting", "requiet", "held"}
                 /\ turn' = "unrecorded" /\ proof' = FALSE /\ restorable' = FALSE
                 /\ personLeft' = personLeft - 1
                 /\ UNCHANGED <<phase, marker, moved, metaLeft, round, Out>>
LateRecord == /\ turn = "unrecorded" /\ turn' = "running" /\ moved' = TRUE /\ marker' = FALSE
              /\ UNCHANGED <<phase, proof, restorable, metaLeft, personLeft, round, Out>>
\* The turn's end, or the fence's close, is evidence of this run.
TurnEnds == /\ Open /\ turn' = "idle" /\ proof' = TRUE /\ moved' = TRUE
            /\ marker' = FALSE
            /\ UNCHANGED <<phase, restorable, metaLeft, personLeft, round, Out>>
Quiet == /\ phase \in {"held", "done", "explained"} /\ UNCHANGED vars

Next == OwnerInput \/ StartWait \/ Deliver \/ Timeout \/ WindowEnds \/ Wake \/ Notice
        \/ MetaMoves \/ PersonSubmits \/ LateRecord \/ TurnEnds \/ Quiet
\* The pane's metadata and a person's Enter are free; a started turn ends by itself.
Fairness == WF_vars(OwnerInput) /\ WF_vars(StartWait) /\ WF_vars(Deliver) /\ WF_vars(Timeout)
            /\ WF_vars(WindowEnds) /\ WF_vars(Wake) /\ WF_vars(Notice)
            /\ WF_vars(LateRecord) /\ WF_vars(TurnEnds)
Spec == Init /\ [][Next]_vars /\ Fairness

TypeOK == /\ phase \in {"restarted", "queued", "waiting", "requiet", "held", "retained",
                        "explained", "done"}
          /\ turn \in {"idle", "unrecorded", "running", "fenced"}
          /\ round \in 0..Rounds /\ keys \in 0..1 /\ retains \in 0..1 /\ notices \in 0..1
NoKeyWithoutEvidence == "key" \notin bad
NoSettleOverOpenTurn == "open" \notin bad
NoRequietSettleOverBusyPane == "busy" \notin bad
\* Existing gap, not widened: the unmoved branch never reads the screen.
UnmovedSettleSeesPane == "gap" \notin bad
OneEnd == keys + retains <= 1 /\ notices <= retains
RequietIsBounded == phase = "requiet" /\ Mutant # "NoRoundCap" => round < Rounds
ScenarioReached == <>(phase = "queued")
InputEventuallyResolves == (phase = "queued") ~> (phase \in {"done", "explained"})
=============================================================================
