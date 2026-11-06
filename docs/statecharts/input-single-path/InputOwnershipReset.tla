------------------------- MODULE InputOwnershipReset -------------------------
EXTENDS InputOwnershipAdmission
CONSTANTS RawReset, StartPending, RejectCatchUp
VARIABLES diskSequence, ownerSequence, thirdPartyWrites, resetDone,
          liveAdmission, catchUpAdmission, committedTickets, originals,
          storedInput, committedInput

resetVars == <<diskSequence, ownerSequence, thirdPartyWrites, resetDone,
               liveAdmission, catchUpAdmission, committedTickets, originals,
               storedInput, committedInput>>
allVars == <<vars, resetVars>>

ResetInit ==
    /\ AdmissionInit
    /\ diskSequence = 1 /\ ownerSequence = 1 /\ thirdPartyWrites = 0
    /\ resetDone = FALSE
    /\ liveAdmission = IF StartPending THEN "pending" ELSE "waiting"
    /\ catchUpAdmission = "waiting"
    /\ committedTickets = 0 /\ originals = 0
    /\ storedInput = <<"ticket-1", "original-1">> /\ committedInput = <<>>

\* The raw writer replaces V2 with []; owner API writes advance both, Herdr preserves both.
Reset ==
    /\ ~resetDone
    /\ resetDone' = TRUE
    /\ diskSequence' \in IF RawReset THEN {0}
                        ELSE IF StartPending THEN {diskSequence}
                        ELSE {diskSequence, diskSequence + 1}
    /\ ownerSequence' = IF RawReset THEN ownerSequence ELSE diskSequence'
    /\ thirdPartyWrites' = IF RawReset THEN thirdPartyWrites + 1 ELSE thirdPartyWrites
    /\ UNCHANGED <<vars, liveAdmission, catchUpAdmission, committedTickets, originals,
                   storedInput, committedInput>>

LiveIntake ==
    /\ resetDone /\ liveAdmission = "waiting"
    /\ IF diskSequence = ownerSequence
       THEN /\ liveAdmission' = "committed"
            /\ committedTickets' = 1 /\ originals' = 1
            /\ committedInput' = storedInput
            /\ diskSequence' = diskSequence + 1 /\ ownerSequence' = diskSequence'
       ELSE /\ liveAdmission' = "casRejected"
            /\ UNCHANGED <<diskSequence, ownerSequence, committedTickets, originals, committedInput>>
    /\ UNCHANGED <<vars, thirdPartyWrites, resetDone, catchUpAdmission, storedInput>>

\* Catch-up retries the live ticket and original; it cannot readopt an external snapshot.
CatchUp ==
    /\ resetDone /\ liveAdmission # "waiting"
    /\ catchUpAdmission \in {"waiting", "committed", "alreadyCommitted"}
    /\ IF RejectCatchUp \/ diskSequence # ownerSequence
       THEN /\ catchUpAdmission' = "casRejected"
            /\ UNCHANGED <<diskSequence, ownerSequence, committedTickets, originals, committedInput>>
       ELSE IF committedTickets = 0
            THEN /\ catchUpAdmission' = "committed"
                 /\ committedTickets' = 1 /\ originals' = 1 /\ committedInput' = storedInput
                 /\ diskSequence' = diskSequence + 1 /\ ownerSequence' = diskSequence'
            ELSE /\ catchUpAdmission' = "alreadyCommitted"
                 /\ UNCHANGED <<diskSequence, ownerSequence, committedTickets, originals, committedInput>>
    /\ UNCHANGED <<vars, thirdPartyWrites, resetDone, liveAdmission, storedInput>>

ResetDone == catchUpAdmission # "waiting" /\ UNCHANGED allVars
ResetNext == Reset \/ LiveIntake \/ CatchUp \/ ResetDone
ResetSpec == ResetInit /\ [][ResetNext]_allVars
             /\ WF_allVars(Reset) /\ WF_allVars(LiveIntake) /\ WF_allVars(CatchUp)

NoThirdPartyQueueWrites == thirdPartyWrites = 0
PendingOriginalKept == storedInput = <<"ticket-1", "original-1">>
AtMostOneAdmission == committedTickets <= 1 /\ originals = committedTickets
                     /\ (committedTickets = 1 => committedInput = storedInput)
PendingEventuallyAdmittedOnce == <>(committedTickets = 1 /\ originals = 1)
=============================================================================
