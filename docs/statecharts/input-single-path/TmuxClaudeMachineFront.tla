----------------------- MODULE TmuxClaudeMachineFront -----------------------
EXTENDS Naturals
CONSTANT Broken, WakeOnTurnEnd, GuardFirstKey
VARIABLE st
vars == <<st>>

\* One live tmux Claude pane with the machine original at the owner front.
\* The wake, claim, and first-key check are the existing path's boundaries.
Init == \E evidence \in {"Idle", "Busy", "Unknown"} :
    st = [evidence |-> evidence, phase |-> "Unread", wake |-> TRUE,
          delivered |-> FALSE, busyWrites |-> 0]

Observe ==
    /\ st.wake /\ ~st.delivered /\ st.phase = "Unread"
    /\ st' = [st EXCEPT !.phase =
        IF Broken THEN "Busy" ELSE IF st.evidence = "Unknown" THEN "Held" ELSE st.evidence,
        !.wake = FALSE]

Hold ==
    /\ st.phase \in {"Busy", "Held"}
    /\ st' = [st EXCEPT !.phase = "Unread"]

TurnEnd ==
    /\ st.evidence \in {"Busy", "Unknown"} /\ ~st.delivered
    /\ st' = [st EXCEPT !.evidence = "Idle", !.wake = @ \/ WakeOnTurnEnd]

NewTurn ==
    /\ st.phase = "Idle" /\ st.evidence = "Idle" /\ ~st.delivered
    /\ st' = [st EXCEPT !.evidence = "Busy"]

FirstKey ==
    /\ st.phase = "Idle" /\ ~st.delivered
    /\ st' = IF GuardFirstKey /\ st.evidence # "Idle"
             THEN [st EXCEPT !.phase = "Unread"]
             ELSE [st EXCEPT !.delivered = TRUE,
                              !.busyWrites = @ + (IF st.evidence = "Busy" THEN 1 ELSE 0)]

Next == Observe \/ Hold \/ TurnEnd \/ NewTurn \/ FirstKey
Spec == Init /\ [][Next]_vars /\ WF_vars(Observe) /\ WF_vars(Hold)
        /\ WF_vars(FirstKey) /\ WF_vars(TurnEnd)

NoMachineWriteWhileBusy == st.busyWrites = 0
IdleFrontEventuallyDelivered == (<>[](st.evidence = "Idle")) => <>st.delivered
=============================================================================
