--------------------------- MODULE InputNativeStop ---------------------------
EXTENDS TLC
CONSTANTS Connected, RunningReceipt, AbsorptionEvidence, CompactSummaryNeutral
VARIABLES opened, attempt, child, foreign, foreignRemoval, summary,
          relay, running, pinned, readable, restarted, result
vars == <<opened, attempt, child, foreign, foreignRemoval, summary,
          relay, running, pinned, readable, restarted, result>>

Init ==
    /\ opened = FALSE /\ attempt = "none" /\ child = FALSE /\ foreign = FALSE
    /\ foreignRemoval = FALSE /\ summary = FALSE /\ relay = FALSE
    /\ running = FALSE /\ pinned = FALSE /\ readable = TRUE /\ restarted = FALSE /\ result = "none"

ExternalOpen ==
    /\ ~opened /\ result = "none" /\ opened' = TRUE
    /\ UNCHANGED <<attempt, child, foreign, foreignRemoval, summary,
                   relay, running, pinned, readable, restarted, result>>
OwnerPaste ==
    /\ opened /\ result = "none" /\ attempt = "none"
    /\ attempt' = "enqueued" /\ child' = relay
    /\ UNCHANGED <<opened, foreign, foreignRemoval, summary,
                   relay, running, pinned, readable, restarted, result>>
\* An exact own user row opens a new root after receipt, Applied and native pin observation.
OwnerPasteRunning ==
    /\ opened /\ result = "none" /\ attempt = "none" /\ ~relay
    /\ attempt' = "root" /\ running' = TRUE /\ pinned' = RunningReceipt
    /\ foreign' = FALSE /\ foreignRemoval' = FALSE /\ summary' = FALSE
    /\ UNCHANGED <<opened, child, relay, readable, restarted, result>>
\* The command may follow its own absorption; that second record consumes no other input.
Absorb ==
    /\ opened /\ result = "none" /\ attempt \in {"enqueued", "removed"}
    /\ attempt' = "absorbed"
    /\ UNCHANGED <<opened, child, foreign, foreignRemoval, summary,
                   relay, running, pinned, readable, restarted, result>>
AbsorbOnly ==
    /\ opened /\ result = "none" /\ attempt = "enqueued"
    /\ attempt' = "removed"
    /\ UNCHANGED <<opened, child, foreign, foreignRemoval, summary,
                   relay, running, pinned, readable, restarted, result>>
AbsorbAfterCommand ==
    /\ opened /\ result = "none" /\ attempt = "absorbed" /\ UNCHANGED vars
ForeignInput ==
    /\ opened /\ result = "none" /\ (~foreign \/ foreignRemoval)
    /\ foreign' = TRUE /\ foreignRemoval' = FALSE
    /\ UNCHANGED <<opened, attempt, child, summary,
                   relay, running, pinned, readable, restarted, result>>
ForeignAbsorbOnly ==
    /\ opened /\ result = "none" /\ ~foreign
    /\ foreign' = TRUE /\ foreignRemoval' = TRUE
    /\ UNCHANGED <<opened, attempt, child, summary,
                   relay, running, pinned, readable, restarted, result>>
CompactSummary ==
    /\ opened /\ result = "none" /\ ~summary /\ summary' = TRUE
    /\ UNCHANGED <<opened, attempt, child, foreign, foreignRemoval,
                   relay, running, pinned, readable, restarted, result>>
RelayClaim ==
    /\ opened /\ result = "none" /\ ~relay /\ ~running /\ relay' = TRUE
    /\ UNCHANGED <<opened, attempt, child, foreign, foreignRemoval, summary,
                   running, pinned, readable, restarted, result>>
Restart ==
    /\ opened /\ result = "none" /\ ~restarted /\ ~running
    /\ restarted' = TRUE /\ relay' = FALSE /\ child' = FALSE /\ readable' \in BOOLEAN
    /\ UNCHANGED <<opened, attempt, foreign, foreignRemoval, summary, running, pinned, result>>

OwnEvidence == attempt = "root" \/ (attempt \in {"absorbed", "removed"} /\ readable)
ObservedOwn == OwnEvidence /\ (attempt # "removed" \/ AbsorptionEvidence)
ObservedForeign == foreign /\ (~foreignRemoval \/ AbsorptionEvidence)
Authority ==
    /\ ~ObservedForeign /\ (~summary \/ CompactSummaryNeutral)
    /\ IF running
          THEN pinned
          ELSE IF relay
                  THEN IF attempt = "absorbed"
                          THEN readable /\ (child \/ Connected)
                          ELSE IF attempt = "removed" /\ AbsorptionEvidence
                                  THEN readable /\ (child \/ Connected)
                                  ELSE TRUE
                  ELSE Connected /\ ObservedOwn
Stop ==
    /\ opened /\ result = "none"
    /\ result' = IF Authority THEN "sent" ELSE "denied"
    /\ UNCHANGED <<opened, attempt, child, foreign, foreignRemoval, summary,
                   relay, running, pinned, readable, restarted>>
Done == result # "none" /\ UNCHANGED vars
Next == ExternalOpen \/ OwnerPaste \/ OwnerPasteRunning \/ Absorb \/ AbsorbOnly
        \/ AbsorbAfterCommand \/ ForeignInput \/ ForeignAbsorbOnly \/ CompactSummary
        \/ RelayClaim \/ Stop \/ Restart \/ Done

NoForeignEscape ==
    (result # "none" /\ (foreign \/ (~relay /\ ~OwnEvidence))) => result = "denied"
OwnInputStops ==
    (result # "none" /\ OwnEvidence /\ ~foreign) => result = "sent"
EnqueueNotAuthority ==
    (result # "none" /\ ~relay /\ attempt = "enqueued") => result = "denied"
=============================================================================
