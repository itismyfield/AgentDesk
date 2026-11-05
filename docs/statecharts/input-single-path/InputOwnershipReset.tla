------------------------- MODULE InputOwnershipReset -------------------------
EXTENDS InputOwnershipAdmission
CONSTANT RawReset
VARIABLES diskSequence, ownerSequence, thirdPartyWrites, resetDone,
          liveAdmission, catchUpAdmission, committedTickets, originals

resetVars == <<diskSequence, ownerSequence, thirdPartyWrites, resetDone,
               liveAdmission, catchUpAdmission, committedTickets, originals>>
allVars == <<vars, resetVars>>

ResetInit ==
    /\ AdmissionInit
    /\ diskSequence = 1 /\ ownerSequence = 1 /\ thirdPartyWrites = 0
    /\ resetDone = FALSE /\ liveAdmission = "waiting" /\ catchUpAdmission = "waiting"
    /\ committedTickets = 0 /\ originals = 0

\* The raw writer replaces V2 with []; owner API writes advance both, Herdr preserves both.
Reset ==
    /\ ~resetDone
    /\ resetDone' = TRUE
    /\ diskSequence' \in IF RawReset THEN {0} ELSE {diskSequence, diskSequence + 1}
    /\ ownerSequence' = IF RawReset THEN ownerSequence ELSE diskSequence'
    /\ thirdPartyWrites' = IF RawReset THEN thirdPartyWrites + 1 ELSE thirdPartyWrites
    /\ UNCHANGED <<vars, liveAdmission, catchUpAdmission, committedTickets, originals>>

LiveIntake ==
    /\ resetDone /\ liveAdmission = "waiting"
    /\ IF diskSequence = ownerSequence
       THEN /\ liveAdmission' = "committed"
            /\ committedTickets' = 1 /\ originals' = 1
            /\ diskSequence' = diskSequence + 1 /\ ownerSequence' = diskSequence'
       ELSE /\ liveAdmission' = "casRejected"
            /\ UNCHANGED <<diskSequence, ownerSequence, committedTickets, originals>>
    /\ UNCHANGED <<vars, thirdPartyWrites, resetDone, catchUpAdmission>>

\* Catch-up retries the live ticket and original; it cannot readopt an external snapshot.
CatchUp ==
    /\ liveAdmission # "waiting" /\ catchUpAdmission = "waiting"
    /\ catchUpAdmission' = IF diskSequence = ownerSequence THEN "alreadyCommitted" ELSE "casRejected"
    /\ UNCHANGED <<vars, diskSequence, ownerSequence, thirdPartyWrites, resetDone,
                   liveAdmission, committedTickets, originals>>

ResetDone == catchUpAdmission # "waiting" /\ UNCHANGED allVars
ResetNext == Reset \/ LiveIntake \/ CatchUp \/ ResetDone
ResetSpec == ResetInit /\ [][ResetNext]_allVars
             /\ WF_allVars(Reset) /\ WF_allVars(LiveIntake) /\ WF_allVars(CatchUp)

NoThirdPartyQueueWrites == thirdPartyWrites = 0
AtMostOneAdmission == committedTickets <= 1 /\ originals = committedTickets
PendingEventuallyAdmittedOnce == <>(committedTickets = 1 /\ originals = 1)
=============================================================================
