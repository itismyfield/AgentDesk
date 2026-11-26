---- MODULE OwnerInputBusyS1 ----
\* #8328 stage S1 (design w4): Claude only. One person input B on a channel whose earlier turn A
\* may leave its mailbox token behind, and whose transcript source may not be bound yet.
\* Policy Current: the owner drive splits on the mailbox active turn (hand_busy / hand_idle), and
\*   every pre-key path wants a bound source (Herdr BusyObservation::admits path match, the O
\*   writer's Producer::wrap pin_source, the adapters' transcript metadata read).
\* Policy Target (S1): only a hold condition keeps B. The anchor, nonce, frame hash and target
\*   incarnation are registered durably before the key; the source binds later (J2) and a late
\*   source is never a reason to hold or to resend. B may join the running turn A (Claude absorbs
\*   it at A's next tool boundary); then A's one answer carries B's part and B carries the
\*   "joined" mark, set from the absorb evidence only.
\* S1 adds no execution-capacity acquire: a joined B rides A's lease; Codex Tab and the shared
\* channel lease are stage S2 (OwnerInputBusySplit.tla).
\* Attribution: a turn's answer replies to its opener's anchor; a turn with no Discord opener
\* (Scenario "external") replies to the first Discord input that joined it, else to none.
EXTENDS Naturals
CONSTANTS Policy, Scenario, Blocker, BlockerClears, Mutant
VARIABLES a, screen, token, b, keys, aReplies, bReplies, wake, blocker,
          mark, merged, aTo, bTo, src, reg, why
vars == <<a, screen, token, b, keys, aReplies, bReplies, wake, blocker,
          mark, merged, aTo, bTo, src, reg, why>>

Pending == {"queued", "held"}
Delivered == {"pasted", "joined", "running", "ended", "answered"}
\* i8432: the watcher refuses A's soft terminal, so A's token outlives A's answer.
TerminalOk == Scenario # "i8432"
\* A turn AgentDesk did not start has no Discord anchor and no mailbox token.
AnchoredA == Scenario # "external"
ATarget == IF AnchoredA THEN "A" ELSE IF merged THEN "B" ELSE "none"
\* Mutants that keep a pre-key source check: Herdr BusyObservation::admits path match (token path),
\* Producer::wrap pin_source wanting a Bound record (every owner path), adapter transcript read.
SourceGate == \/ Policy = "Current"
              \/ Mutant = "PinSourceFirst"
              \/ Mutant \in {"AdmitsPathMatch", "RequireTranscript"} /\ token # "none"

Init == /\ a = IF Scenario = "fresh" THEN "none" ELSE "running"
        /\ screen = IF Scenario = "fresh" THEN "Idle" ELSE "Busy"
        /\ token = IF Scenario \in {"fresh", "external"} THEN "none" ELSE "A"
        /\ b = "absent" /\ keys = 0 /\ aReplies = 0 /\ bReplies = 0
        /\ wake = FALSE /\ blocker = Blocker
        /\ mark = FALSE /\ merged = FALSE /\ aTo = "unset" /\ bTo = "unset"
        \* fresh: a new session has no transcript until a turn writes it.
        \* rebind: the composer is ready before the writer binds the (re)started source.
        /\ src = IF Scenario \in {"fresh", "rebind"} THEN "none" ELSE "bound"
        /\ reg = FALSE /\ why = "none"

AEnds == /\ a = "running"
         /\ a' = "ended" /\ screen' = "Idle" /\ aReplies' = aReplies + 1
         /\ token' = IF TerminalOk /\ token = "A" THEN "none" ELSE token
         /\ wake' = (wake \/ TerminalOk)
         /\ IF b = "joined"
            THEN /\ b' = "answered" /\ merged' = TRUE
                 /\ bReplies' = IF Mutant = "JoinDoublePost" THEN bReplies + 1 ELSE bReplies
            ELSE UNCHANGED <<b, merged, bReplies>>
         /\ aTo' = CASE Mutant = "NoReference" -> "none"
                      [] AnchoredA -> "A"
                      [] b = "joined" /\ reg -> "B"
                      [] Mutant = "PostOrder" /\ b = "pasted" -> "B"
                      [] OTHER -> "none"
         /\ bTo' = IF Mutant = "JoinDoublePost" /\ b = "joined" THEN "B" ELSE bTo
         /\ UNCHANGED <<keys, blocker, mark, src, reg, why>>

Arrive == /\ b = "absent"
          /\ b' = "queued" /\ wake' = TRUE
          /\ UNCHANGED <<a, screen, token, keys, aReplies, bReplies, blocker>>
          /\ UNCHANGED <<mark, merged, aTo, bTo, src, reg, why>>

Hold(r) == b' = "held" /\ why' = r /\ UNCHANGED <<keys, token, mark, reg>>
\* The one key effect (paste + Enter). Registration (anchor, nonce, frame hash, incarnation)
\* commits before it; RegisterAfterKey mutant registers only after the key. Current marks a paste
\* into a busy screen "joined" at once; Target marks only at TuiAbsorb (MarkOnPaste mutant).
Paste(owner) == /\ b' = "pasted" /\ keys' = keys + 1 /\ why' = "none"
                /\ token' = IF owner = "none" THEN "B" ELSE owner
                /\ reg' = (reg \/ Mutant # "RegisterAfterKey")
                /\ mark' = IF Policy = "Current" \/ Mutant = "MarkOnPaste"
                           THEN screen = "Busy" ELSE mark

DriveCurrent == /\ Policy = "Current" /\ b \in Pending /\ wake
                /\ wake' = FALSE
                /\ IF blocker # "none" THEN Hold("blocker")
                   ELSE IF src = "none" THEN Hold("source")
                   ELSE IF token # "none"
                        THEN IF screen = "Busy" THEN Paste(token) ELSE Hold("busy_unavailable")
                        ELSE Paste("none")
                /\ UNCHANGED <<a, screen, aReplies, bReplies, blocker, merged, aTo, bTo, src>>

DriveTarget == /\ Policy = "Target" /\ b \in Pending /\ wake
               /\ wake' = FALSE
               /\ IF blocker # "none" THEN Hold("blocker")
                  ELSE IF SourceGate /\ src = "none" THEN Hold("source")
                  ELSE Paste(token)
               /\ UNCHANGED <<a, screen, aReplies, bReplies, blocker, merged, aTo, bTo, src>>

RegisterLate == /\ Mutant = "RegisterAfterKey" /\ keys >= 1 /\ ~reg
                /\ reg' = TRUE
                /\ UNCHANGED <<a, screen, token, b, keys, aReplies, bReplies, wake, blocker>>
                /\ UNCHANGED <<mark, merged, aTo, bTo, src, why>>

\* The writer binds the restarted source (rebind window ends).
BindSource == /\ Scenario = "rebind" /\ src = "none"
              /\ src' = "bound"
              /\ UNCHANGED <<a, screen, token, b, keys, aReplies, bReplies, wake, blocker>>
              /\ UNCHANGED <<mark, merged, aTo, bTo, reg, why>>

Retry == /\ b = "held" /\ ~wake
         /\ wake' = TRUE
         /\ UNCHANGED <<a, screen, token, b, keys, aReplies, bReplies, blocker>>
         /\ UNCHANGED <<mark, merged, aTo, bTo, src, reg, why>>

\* Claude takes a pasted B into the running turn at A's next tool boundary. No fairness.
TuiAbsorb == /\ b = "pasted" /\ a = "running"
             /\ b' = "joined"
             /\ mark' = IF Policy = "Target" THEN Mutant # "JoinUnmarked" ELSE mark
             /\ UNCHANGED <<a, screen, token, keys, aReplies, bReplies, wake, blocker>>
             /\ UNCHANGED <<merged, aTo, bTo, src, reg, why>>

\* B's own turn; the TUI writes the transcript, so the source exists from here on.
TuiStart == /\ b = "pasted" /\ screen = "Idle"
            /\ b' = "running" /\ screen' = "Busy" /\ src' = "bound"
            /\ UNCHANGED <<a, token, keys, aReplies, bReplies, wake, blocker>>
            /\ UNCHANGED <<mark, merged, aTo, bTo, reg, why>>

BEnds == /\ b = "running"
         /\ b' = "ended" /\ screen' = "Idle"
         /\ token' = IF TerminalOk /\ token = "B" THEN "none" ELSE token
         /\ UNCHANGED <<a, keys, aReplies, bReplies, wake, blocker>>
         /\ UNCHANGED <<mark, merged, aTo, bTo, src, reg, why>>

\* J2 binds B's echo to B's anchor through the registered nonce; with no registration there is
\* nothing to bind and the answer goes out with no reference.
PostB == /\ b = "ended" /\ src = "bound"
         /\ b' = "answered"
         /\ IF Mutant = "TokenAttribution" /\ token = "A"
            THEN aReplies' = aReplies + 1 /\ UNCHANGED bReplies
            ELSE IF Mutant = "TokenAttribution" /\ token = "none"
            THEN UNCHANGED <<aReplies, bReplies>>
            ELSE bReplies' = bReplies + 1 /\ UNCHANGED aReplies
         /\ bTo' = IF Mutant = "NoReference" \/ ~reg THEN "none" ELSE "B"
         /\ UNCHANGED <<a, screen, token, keys, wake, blocker>>
         /\ UNCHANGED <<mark, merged, aTo, src, reg, why>>

ClearBlocker == /\ BlockerClears /\ blocker # "none"
                /\ blocker' = "none" /\ wake' = TRUE
                /\ UNCHANGED <<a, screen, token, b, keys, aReplies, bReplies>>
                /\ UNCHANGED <<mark, merged, aTo, bTo, src, reg, why>>

ResendBeforeEcho == /\ Mutant = "ResendBeforeEcho" /\ b = "pasted" /\ keys = 1
                    /\ keys' = 2
                    /\ UNCHANGED <<a, screen, token, b, aReplies, bReplies, wake, blocker>>
                    /\ UNCHANGED <<mark, merged, aTo, bTo, src, reg, why>>
TuiDrop == /\ Mutant = "TuiDropsQueued" /\ b = "pasted" /\ screen = "Busy"
           /\ b' = "lost"
           /\ UNCHANGED <<a, screen, token, keys, aReplies, bReplies, wake, blocker>>
           /\ UNCHANGED <<mark, merged, aTo, bTo, src, reg, why>>

Done == b \in {"answered", "lost"} /\ UNCHANGED vars

Next == AEnds \/ Arrive \/ DriveCurrent \/ DriveTarget \/ RegisterLate \/ BindSource \/ Retry
        \/ TuiAbsorb \/ TuiStart \/ BEnds \/ PostB \/ ClearBlocker \/ ResendBeforeEcho
        \/ TuiDrop \/ Done
Fairness == /\ WF_vars(AEnds) /\ WF_vars(Arrive) /\ WF_vars(DriveCurrent)
            /\ WF_vars(DriveTarget) /\ WF_vars(RegisterLate) /\ WF_vars(BindSource)
            /\ WF_vars(Retry) /\ WF_vars(TuiStart) /\ WF_vars(BEnds) /\ WF_vars(PostB)
            /\ WF_vars(ClearBlocker)
Spec == Init /\ [][Next]_vars /\ Fairness

TypeOK == /\ a \in {"none", "running", "ended"} /\ screen \in {"Busy", "Idle"}
          /\ token \in {"none", "A", "B"}
          /\ b \in {"absent", "queued", "held", "pasted", "joined", "running", "ended",
                    "answered", "lost"}
          /\ keys \in 0..2 /\ aReplies \in 0..2 /\ bReplies \in 0..2
          /\ blocker \in {"none", "draft", "modal", "nosession", "attach", "composer", "recovery"}
          /\ mark \in BOOLEAN /\ merged \in BOOLEAN
          /\ aTo \in {"unset", "none", "A", "B"} /\ bTo \in {"unset", "none", "B"}
          /\ src \in {"none", "bound"} /\ reg \in BOOLEAN
          /\ why \in {"none", "blocker", "source", "busy_unavailable"}
NoDuplicate == keys <= 1
NoSilentLoss == b # "lost"
ReplyExactlyOnce == /\ aReplies <= 1 /\ bReplies <= 1
                    /\ (b = "answered" => (bReplies = 1) # merged)
JoinedMarked == merged => mark
MarkOnlyJoined == mark => (b = "joined" \/ merged)
AnswerBound == /\ (aReplies >= 1 => aTo = ATarget)
               /\ (bReplies >= 1 => bTo = "B")
KeyBeforeLeave == b \in Delivered => keys >= 1
\* The anchor, nonce, frame hash and incarnation are durable before any key effect.
KeyAfterRegister == keys >= 1 => reg
\* A normal input is never held because its transcript source is not bound yet.
NoSourceHold == why # "source"

IdleDelivers == (b \in Pending /\ screen = "Idle") ~> (b \in Delivered)
ReplyPosted == <>(bReplies = 1 \/ merged)
=============================================================================
