--------------------------- MODULE InputNativeStop ---------------------------
EXTENDS TLC
CONSTANTS Connected, RunningReceipt, AbsorptionEvidence, CompactSummaryNeutral, EpisodeChecked, RestoredChecked, FreshSourceAllowed
VARIABLES opened, attempt, child, foreign, foreignRemoval, summary,
          relay, running, pinned, readable, restarted, birthChild, ledgerChild, restoredToken, sourceMissing, result
vars == <<opened, attempt, child, foreign, foreignRemoval, summary,
          relay, running, pinned, readable, restarted, birthChild, ledgerChild, restoredToken, sourceMissing, result>>

Init ==
    /\ opened = FALSE /\ attempt = "none" /\ child = FALSE /\ foreign = FALSE
    /\ foreignRemoval = FALSE /\ summary = FALSE /\ relay = FALSE
    /\ running = FALSE /\ pinned = FALSE /\ readable = TRUE /\ restarted = FALSE
    /\ birthChild = "unbound" /\ ledgerChild = FALSE /\ restoredToken = FALSE /\ sourceMissing = FALSE /\ result = "none"

Waiting == result = "none" /\ birthChild = "unbound" /\ ~sourceMissing

ExternalOpen ==
    /\ ~opened /\ Waiting /\ opened' = TRUE
    /\ UNCHANGED <<attempt, child, foreign, foreignRemoval, summary,
                   relay, running, pinned, readable, restarted, birthChild, ledgerChild, restoredToken, sourceMissing, result>>
OwnerPaste ==
    /\ opened /\ Waiting /\ attempt = "none"
    /\ attempt' = "enqueued" /\ child' = relay
    /\ UNCHANGED <<opened, foreign, foreignRemoval, summary,
                   relay, running, pinned, readable, restarted, birthChild, ledgerChild, restoredToken, sourceMissing, result>>
\* An exact own user row opens a new root after receipt, Applied and native pin observation.
OwnerPasteRunning ==
    /\ opened /\ Waiting /\ attempt = "none" /\ ~relay
    /\ attempt' = "root" /\ running' = TRUE /\ pinned' = RunningReceipt
    /\ foreign' = FALSE /\ foreignRemoval' = FALSE /\ summary' = FALSE
    /\ UNCHANGED <<opened, child, relay, readable, restarted, birthChild, ledgerChild, restoredToken, sourceMissing, result>>
\* The live receipt is accepted before RuntimeReady fills the durable output path.
LiveRootBeforeRuntimeReady ==
    /\ opened /\ Waiting /\ attempt = "none" /\ ~relay /\ ~running
    /\ attempt' = "root" /\ running' = TRUE /\ pinned' = TRUE /\ sourceMissing' = TRUE
    /\ foreign' = FALSE /\ foreignRemoval' = FALSE /\ summary' = FALSE
    /\ UNCHANGED <<opened, child, relay, readable, restarted, birthChild, ledgerChild, restoredToken, result>>
\* The command may follow its own absorption; that second record consumes no other input.
Absorb ==
    /\ opened /\ Waiting /\ attempt \in {"enqueued", "removed"}
    /\ attempt' = "absorbed"
    /\ UNCHANGED <<opened, child, foreign, foreignRemoval, summary,
                   relay, running, pinned, readable, restarted, birthChild, ledgerChild, restoredToken, sourceMissing, result>>
AbsorbOnly ==
    /\ opened /\ Waiting /\ attempt = "enqueued"
    /\ attempt' = "removed"
    /\ UNCHANGED <<opened, child, foreign, foreignRemoval, summary,
                   relay, running, pinned, readable, restarted, birthChild, ledgerChild, restoredToken, sourceMissing, result>>
AbsorbAfterCommand ==
    /\ opened /\ Waiting /\ attempt = "absorbed" /\ UNCHANGED vars
ForeignInput ==
    /\ opened /\ Waiting /\ (~foreign \/ foreignRemoval)
    /\ foreign' = TRUE /\ foreignRemoval' = FALSE
    /\ UNCHANGED <<opened, attempt, child, summary,
                   relay, running, pinned, readable, restarted, birthChild, ledgerChild, restoredToken, sourceMissing, result>>
ForeignAbsorbOnly ==
    /\ opened /\ Waiting /\ ~foreign
    /\ foreign' = TRUE /\ foreignRemoval' = TRUE
    /\ UNCHANGED <<opened, attempt, child, summary,
                   relay, running, pinned, readable, restarted, birthChild, ledgerChild, restoredToken, sourceMissing, result>>
CompactSummary ==
    /\ opened /\ Waiting /\ ~summary /\ summary' = TRUE
    /\ UNCHANGED <<opened, attempt, child, foreign, foreignRemoval,
                   relay, running, pinned, readable, restarted, birthChild, ledgerChild, restoredToken, sourceMissing, result>>
RelayClaim ==
    /\ opened /\ Waiting /\ ~relay /\ ~running /\ relay' = TRUE
    /\ UNCHANGED <<opened, attempt, child, foreign, foreignRemoval, summary,
                   running, pinned, readable, restarted, birthChild, ledgerChild, restoredToken, sourceMissing, result>>
Restart ==
    /\ opened /\ Waiting /\ ~restarted /\ ~running
    /\ restarted' = TRUE /\ relay' = FALSE /\ child' = FALSE /\ readable' \in BOOLEAN
    /\ UNCHANGED <<opened, attempt, foreign, foreignRemoval, summary, running, pinned, birthChild, ledgerChild, restoredToken, sourceMissing, result>>

\* Bound birth evidence is independent of the native pin; one mismatching child is followed by Stop.
NativeChildWindow ==
    /\ opened /\ Waiting /\ (relay \/ running)
    /\ attempt \in {"none", "root"} /\ ~foreign /\ ~summary
    /\ readable /\ ~restarted
OtherEpisodeChild ==
    /\ NativeChildWindow /\ birthChild' = "episode"
    /\ child' = FALSE /\ ledgerChild' = TRUE /\ foreign' = TRUE /\ restarted' = TRUE
    /\ UNCHANGED <<opened, attempt, foreignRemoval, summary,
                   relay, running, pinned, readable, restoredToken, sourceMissing, result>>
OtherParentChild ==
    /\ NativeChildWindow /\ birthChild' = "parent"
    /\ child' = FALSE /\ ledgerChild' = TRUE /\ foreign' = TRUE /\ restarted' = TRUE
    /\ UNCHANGED <<opened, attempt, foreignRemoval, summary,
                   relay, running, pinned, readable, restoredToken, sourceMissing, result>>

\* Without restored child proof the same ledger snapshot can be H1 or a failed-save H2.
RestoreUnprovenChild ==
    /\ NativeChildWindow /\ relay /\ ~running /\ attempt = "none"
    /\ birthChild' = "restored-unproven" /\ restoredToken' = TRUE
    /\ child' = FALSE /\ ledgerChild' = TRUE /\ restarted' = TRUE
    /\ UNCHANGED <<opened, attempt, foreign, foreignRemoval, summary,
                   relay, running, pinned, readable, sourceMissing, result>>

OwnEvidence == attempt = "root" \/ (attempt \in {"absorbed", "removed"} /\ readable)
ObservedOwn == OwnEvidence /\ (attempt # "removed" \/ AbsorptionEvidence)
ObservedForeign ==
    /\ foreign /\ (~foreignRemoval \/ AbsorptionEvidence)
    /\ ~(birthChild # "unbound" /\ ~EpisodeChecked /\ ledgerChild /\ ~child)
Authority ==
    /\ (~sourceMissing \/ FreshSourceAllowed)
    /\ ~(RestoredChecked /\ restoredToken /\ ledgerChild /\ ~child)
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
                   relay, running, pinned, readable, restarted, birthChild, ledgerChild, restoredToken, sourceMissing>>
Done == result # "none" /\ UNCHANGED vars
Next == ExternalOpen \/ OwnerPaste \/ OwnerPasteRunning \/ LiveRootBeforeRuntimeReady \/ Absorb \/ AbsorbOnly
        \/ AbsorbAfterCommand \/ ForeignInput \/ ForeignAbsorbOnly \/ CompactSummary
        \/ RelayClaim \/ OtherEpisodeChild \/ OtherParentChild \/ RestoreUnprovenChild \/ Stop \/ Restart \/ Done

NoForeignEscape ==
    (result # "none" /\ (foreign \/ (~relay /\ ~OwnEvidence))) => result = "denied"
OwnInputStops ==
    (result # "none" /\ OwnEvidence /\ ~foreign) => result = "sent"
NoOtherEpisodeEscape ==
    (result # "none" /\ birthChild = "episode") => result = "denied"
NoOtherParentEscape ==
    (result # "none" /\ birthChild = "parent") => result = "denied"
NoUnprovenRestoredChildEscape ==
    (result # "none" /\ restoredToken /\ ledgerChild /\ ~child) => result = "denied"
LiveUnboundSourceStops ==
    (result # "none" /\ sourceMissing /\ running /\ pinned /\ ~restoredToken /\ ~foreign)
        => result = "sent"
EnqueueNotAuthority ==
    (result # "none" /\ ~relay /\ attempt = "enqueued") => result = "denied"
=============================================================================
