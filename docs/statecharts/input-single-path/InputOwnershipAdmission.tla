----------------------- MODULE InputOwnershipAdmission -----------------------
EXTENDS InputOwnership
CONSTANTS InitialRows, AdmissionSource, AdmissionPane

AdmissionInit ==
    /\ Init /\ mode = "turn" /\ src = AdmissionSource /\ mbx = "idle" /\ nat = AdmissionPane
    /\ old = "none" /\ obsOk /\ ~twin /\ screen = "ok" /\ draft = "none" /\ ~twinPend
    /\ ~stale /\ ~acct /\ row \in InitialRows /\ ~ep /\ ~leak
AdmissionDone == pastes = 1 /\ UNCHANGED vars
AdmissionNext == DriveIdle \/ DriveStartBusy \/ DriveIdleBusyPane \/ AdmissionDone

\* The ordinary owner's busy shortcut has no create/admit step to clear this marked row.
OwnerBusyKeepsMarked == (src = "discord" /\ nat = "busy" /\ pastes > 0) => row = "marked"
=============================================================================
