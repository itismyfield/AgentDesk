------------------------ MODULE IdleAcceptPending ------------------------
EXTENDS Naturals, FiniteSets
\* One human input accepted by an idle channel owner (#8398). Early = TRUE puts the ⏳ on it at
\* acceptance; the owner handoff then starts it or leaves through one exit, and the settle after
\* the handoff returns retracts an early ⏳ that no start took over.
CONSTANTS Early, RetractExits, RestartCleanup, StartTakesAttempt
Exits == {"busy", "raceLoss", "replayRefused", "replayDeferred", "blocked", "writerBlocked",
          "consumed"}
\* Exits that leave the original in the owner queue; the others remove it.
StillQueued == {"busy", "raceLoss", "replayDeferred", "writerBlocked"}
VARIABLES phase, markers, adds, attempt, recorded, queued, restarted, exit
vars == <<phase, markers, adds, attempt, recorded, queued, restarted, exit>>

Init == /\ phase = "new" /\ markers = {} /\ adds = 0 /\ attempt = "none"
        /\ recorded = FALSE /\ queued = FALSE /\ restarted = FALSE /\ exit = "none"

Accept == /\ phase = "new" /\ phase' = "accepted" /\ queued' = TRUE
          /\ markers' = IF Early THEN {"hourglass"} ELSE {}
          /\ adds' = IF Early THEN 1 ELSE 0
          /\ attempt' = IF Early THEN "early" ELSE "none"
          /\ recorded' = Early
          /\ UNCHANGED <<restarted, exit>>

\* A restart loses the in-memory record; the durable original replays into a new handoff.
Restart == /\ phase \in {"accepted", "handing"} /\ ~restarted
           /\ restarted' = TRUE /\ recorded' = FALSE /\ phase' = "accepted"
           /\ UNCHANGED <<markers, adds, attempt, queued, exit>>

Hand == /\ phase = "accepted" /\ phase' = "handing"
        /\ UNCHANGED <<markers, adds, attempt, recorded, queued, restarted, exit>>

\* The start site re-notes Pending: over an early ⏳ it only takes a fresh attempt (no add).
Start == /\ phase = "handing" /\ phase' = "returned" /\ exit' = "started" /\ queued' = FALSE
         /\ attempt' = "start"
         /\ IF "hourglass" \in markers /\ StartTakesAttempt
               THEN UNCHANGED <<markers, adds>>
               ELSE /\ markers' = {"hourglass"} /\ adds' = adds + 1
         /\ UNCHANGED <<recorded, restarted>>

\* An exit's own queue marker is ignored over a started-looking ⏳; the settle publishes it.
Leave(e) == /\ phase = "handing" /\ phase' = "returned" /\ exit' = e
            /\ queued' = (e \in StillQueued)
            /\ markers' = IF Early THEN markers
                          ELSE IF e \in StillQueued THEN {"mailbox"} ELSE {}
            /\ UNCHANGED <<adds, attempt, recorded, restarted>>

\* After the handoff returns: an early attempt nobody took is rolled back to 📬 or cleared. A
\* restart's record survives only in the persisted older-generation Pending state.
Owned == recorded \/ (restarted /\ RestartCleanup)
Settle == /\ phase = "returned" /\ phase' = "settled"
          /\ IF attempt = "early" /\ Owned /\ exit \in RetractExits
                THEN /\ markers' = IF queued THEN {"mailbox"} ELSE {}
                     /\ attempt' = "none"
                ELSE UNCHANGED <<markers, attempt>>
          /\ recorded' = FALSE
          /\ UNCHANGED <<adds, queued, restarted, exit>>

Next == Accept \/ Restart \/ Hand \/ Start \/ (\E e \in Exits : Leave(e)) \/ Settle
Spec == Init /\ [][Next]_vars /\ WF_vars(Accept) /\ WF_vars(Hand) /\ WF_vars(Settle)
        /\ WF_vars(Start \/ \E e \in Exits : Leave(e))

\* The rules: an accepted input is never invisible while the owner decides; a ⏳ stays only on
\* a started turn; a queued leftover shows 📬; a start over an early ⏳ adds nothing.
AcceptedPromptlyVisible == phase \in {"accepted", "handing"} => markers = {"hourglass"}
NoStuckPending == phase = "settled" => ("hourglass" \in markers => exit = "started")
QueuedLeftoverVisible == (phase = "settled" /\ queued) => markers = {"mailbox"}
StartAddsOnce == exit = "started" => adds = 1
OneMarker == Cardinality(markers) <= 1
Converges == (phase = "accepted")
             ~> (phase = "settled" /\ ("hourglass" \in markers => exit = "started"))
=============================================================================
