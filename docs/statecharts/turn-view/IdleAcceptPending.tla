------------------------ MODULE IdleAcceptPending ------------------------
EXTENDS Naturals, FiniteSets
\* A person's input A accepted by an idle channel owner (#8398), with B arriving at the same time.
\* Early = TRUE puts the ⏳ on A at acceptance. The settles that retract an early ⏳ no start took
\* over are the links: "handoff" (its return), "recheck" (custody read after the note),
\* "queueExit" (a cancel's exit feedback) and "boot" (the sweep of persisted early marks).
\* HeadVerdict "actor" marks only the input the owner's own turn found as its sole head; "pre"
\* is the old empty-owner check made before admission, which both A and B can pass.
CONSTANTS Early, Links, StartTakesAttempt, HeadVerdict
VARIABLES phase, markers, adds, attempt, recorded, persisted, queued, restarted, exit, bMarked
vars == <<phase, markers, adds, attempt, recorded, persisted, queued, restarted, exit, bMarked>>

Init == /\ phase = "new" /\ markers = {} /\ adds = 0 /\ attempt = "none"
        /\ recorded = FALSE /\ persisted = FALSE /\ queued = FALSE /\ restarted = FALSE
        /\ exit = "none" /\ bMarked = FALSE

\* The early ⏳ is still untaken and this process or the persisted record names it.
Untaken == attempt = "early" /\ (recorded \/ persisted)
Clear == /\ markers' = {} /\ attempt' = "none" /\ recorded' = FALSE /\ persisted' = FALSE

\* A and B are admitted; B queues behind A as its follower.
Admit == /\ phase = "new" /\ phase' = "admitted" /\ queued' = TRUE
         /\ bMarked' = (Early /\ HeadVerdict = "pre")
         /\ UNCHANGED <<markers, adds, attempt, recorded, persisted, restarted, exit>>

\* The reaction lands, then the record (memory and persisted).
Note == /\ phase = "admitted" /\ phase' = "noted"
        /\ IF Early
              THEN /\ markers' = {"hourglass"} /\ adds' = adds + 1 /\ attempt' = "early"
                   /\ recorded' = TRUE /\ persisted' = TRUE
              ELSE UNCHANGED <<markers, adds, attempt, recorded, persisted>>
        /\ UNCHANGED <<queued, restarted, exit, bMarked>>

Recheck == /\ phase = "noted" /\ phase' = "accepted"
           /\ IF "recheck" \in Links /\ ~queued /\ Untaken
                 THEN Clear ELSE UNCHANGED <<markers, attempt, recorded, persisted>>
           /\ UNCHANGED <<adds, queued, restarted, exit, bMarked>>

\* A cancel takes A and B out before any handoff; its exit feedback clears what is recorded.
Cancel == /\ phase \in {"admitted", "noted", "accepted"} /\ queued /\ queued' = FALSE
          /\ IF "queueExit" \in Links /\ Untaken
                THEN Clear ELSE UNCHANGED <<markers, attempt, recorded, persisted>>
          /\ bMarked' = (bMarked /\ "queueExit" \notin Links)
          /\ UNCHANGED <<phase, adds, restarted, exit>>

\* The drive finds no front: no handoff runs.
DriveEmpty == /\ phase = "accepted" /\ ~queued /\ phase' = "settled"
              /\ UNCHANGED <<markers, adds, attempt, recorded, persisted, queued, restarted, exit,
                             bMarked>>

Hand == /\ phase = "accepted" /\ queued /\ phase' = "handing"
        /\ UNCHANGED <<markers, adds, attempt, recorded, persisted, queued, restarted, exit,
                       bMarked>>

\* The start submits A with B merged; over an early ⏳ it only takes a fresh attempt.
Start == /\ phase = "handing" /\ phase' = "returned" /\ exit' = "started" /\ queued' = FALSE
         /\ attempt' = "start" /\ persisted' = FALSE
         /\ IF "hourglass" \in markers /\ StartTakesAttempt
               THEN UNCHANGED <<markers, adds>>
               ELSE /\ markers' = {"hourglass"} /\ adds' = adds + 1
         /\ UNCHANGED <<recorded, restarted, bMarked>>

\* Any exit without a start; custody decides the settle, not the exit's name. The exit's own
\* queue marker is ignored over a ⏳ and kept otherwise. B, still a waiting input of its own, is
\* settled by its own later handoff.
Leave == /\ phase = "handing" /\ phase' = "returned" /\ exit' = "left"
         /\ \E held \in BOOLEAN : queued' = held
         /\ markers' = IF "hourglass" \in markers THEN markers
                       ELSE IF queued' THEN {"mailbox"} ELSE {}
         /\ bMarked' = (bMarked /\ "handoff" \notin Links)
         /\ UNCHANGED <<adds, attempt, recorded, persisted, restarted>>

Settle == /\ phase = "returned" /\ phase' = "settled"
          /\ IF "handoff" \in Links /\ Untaken
                THEN /\ markers' = IF queued THEN {"mailbox"} ELSE {}
                     /\ attempt' = "none" /\ recorded' = FALSE /\ persisted' = FALSE
                ELSE UNCHANGED <<markers, attempt, recorded, persisted>>
          /\ UNCHANGED <<adds, queued, restarted, exit, bMarked>>

\* A restart at any open step loses the record; boot may drop the original (an expired hold);
\* the boot sweep clears a persisted early mark whose input is gone; a held input replays.
Restart == /\ phase \in {"admitted", "noted", "accepted", "handing", "returned"} /\ ~restarted
           /\ restarted' = TRUE
           /\ \E drop \in BOOLEAN : queued' = (queued /\ ~drop)
           /\ IF "boot" \in Links /\ persisted /\ attempt = "early" /\ ~queued'
                 THEN Clear
                 ELSE /\ recorded' = FALSE /\ UNCHANGED <<markers, attempt, persisted>>
           /\ phase' = IF queued' THEN "accepted" ELSE "settled"
           /\ UNCHANGED <<adds, exit, bMarked>>

Next == Admit \/ Note \/ Recheck \/ Cancel \/ DriveEmpty \/ Hand \/ Start \/ Leave \/ Settle
        \/ Restart
Spec == Init /\ [][Next]_vars /\ WF_vars(Admit) /\ WF_vars(Note) /\ WF_vars(Recheck)
        /\ WF_vars(DriveEmpty) /\ WF_vars(Hand) /\ WF_vars(Start \/ Leave) /\ WF_vars(Settle)

\* An accepted input waiting for its start shows ⏳ (a restart before the note never marks it).
AcceptedPromptlyVisible == (phase \in {"accepted", "handing"} /\ queued /\ ~restarted)
                           => markers = {"hourglass"}
NoStuckPending == phase = "settled" => ("hourglass" \in markers => exit = "started")
NoStuckFollower == phase = "settled" => ~bMarked
QueuedLeftoverVisible == (phase = "settled" /\ queued) => markers = {"mailbox"}
StartAddsOnce == exit = "started" => adds = 1
OneMarker == Cardinality(markers) <= 1
Converges == (phase = "admitted")
             ~> (phase = "settled" /\ ~bMarked
                 /\ ("hourglass" \in markers => exit = "started"))
=============================================================================
