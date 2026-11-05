--------------------------- MODULE InputNativeStop ---------------------------
EXTENDS TLC
CONSTANTS Connected, RunningReceipt
VARIABLES opened, attempt, child, foreign, relay, running, pinned, readable, restarted, result
vars == <<opened, attempt, child, foreign, relay, running, pinned, readable, restarted, result>>

Init ==
    /\ opened = FALSE /\ attempt = "none" /\ child = FALSE /\ foreign = FALSE /\ relay = FALSE
    /\ running = FALSE /\ pinned = FALSE /\ readable = TRUE /\ restarted = FALSE /\ result = "none"

ExternalOpen ==
    /\ ~opened /\ result = "none" /\ opened' = TRUE
    /\ UNCHANGED <<attempt, child, foreign, relay, running, pinned, readable, restarted, result>>
OwnerPaste ==
    /\ opened /\ result = "none" /\ attempt = "none"
    /\ attempt' = "enqueued" /\ child' = relay
    /\ UNCHANGED <<opened, foreign, relay, running, pinned, readable, restarted, result>>
\* The running dispatch retains its token; an exact own user row becomes its new root.
\* Receipt setup, an Applied finish and native pin observation are one successful input step.
OwnerPasteRunning ==
    /\ opened /\ result = "none" /\ attempt = "none" /\ ~relay
    /\ attempt' = "root" /\ running' = TRUE /\ pinned' = RunningReceipt /\ foreign' = FALSE
    /\ UNCHANGED <<opened, child, relay, readable, restarted, result>>
Absorb ==
    /\ opened /\ result = "none" /\ attempt = "enqueued"
    /\ attempt' = "absorbed"
    /\ UNCHANGED <<opened, child, foreign, relay, running, pinned, readable, restarted, result>>
ForeignInput ==
    /\ opened /\ result = "none" /\ ~foreign /\ foreign' = TRUE
    /\ UNCHANGED <<opened, attempt, child, relay, running, pinned, readable, restarted, result>>
RelayClaim ==
    /\ opened /\ result = "none" /\ ~relay /\ ~running /\ relay' = TRUE
    /\ UNCHANGED <<opened, attempt, child, foreign, running, pinned, readable, restarted, result>>
Restart ==
    /\ opened /\ result = "none" /\ ~restarted /\ ~running
    /\ restarted' = TRUE /\ relay' = FALSE /\ child' = FALSE /\ readable' \in BOOLEAN
    /\ UNCHANGED <<opened, attempt, foreign, running, pinned, result>>

OwnEvidence == attempt = "root" \/ (attempt = "absorbed" /\ readable)
\* A relay pin already authorizes its external opener; later owner rows need child evidence.
\* A retained running token instead needs its own receipt-derived native user pin.
Authority ==
    /\ ~foreign
    /\ IF running
          THEN pinned
          ELSE IF relay
                  THEN attempt # "absorbed" \/ (readable /\ (child \/ Connected))
                  ELSE Connected /\ OwnEvidence
Stop ==
    /\ opened /\ result = "none"
    /\ result' = IF Authority THEN "sent" ELSE "denied"
    /\ UNCHANGED <<opened, attempt, child, foreign, relay, running, pinned, readable, restarted>>
Done == result # "none" /\ UNCHANGED vars
Next == ExternalOpen \/ OwnerPaste \/ OwnerPasteRunning \/ Absorb \/ ForeignInput \/ RelayClaim \/ Stop \/ Restart \/ Done

NoForeignEscape ==
    (result # "none" /\ (foreign \/ (~relay /\ ~OwnEvidence))) => result = "denied"
OwnInputStops ==
    (result # "none" /\ OwnEvidence /\ ~foreign) => result = "sent"
EnqueueNotAuthority ==
    (result # "none" /\ ~relay /\ attempt = "enqueued") => result = "denied"
=============================================================================
