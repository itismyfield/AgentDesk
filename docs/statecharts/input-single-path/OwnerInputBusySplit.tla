---- MODULE OwnerInputBusySplit ----
\* One person input B on a channel whose earlier turn A may leave its mailbox token behind.
\* Policy Current: the owner drive splits on the mailbox active turn (hand_busy / hand_idle).
\* Policy Target: a live session with no hold condition takes B at once; B's answer is bound
\* to B by its transcript echo, not by the mailbox token.
EXTENDS Naturals
CONSTANTS Policy, Scenario, Blocker, BlockerClears, Cap, Mutant
VARIABLES a, screen, token, b, keys, aReplies, bReplies, wake, blocker
vars == <<a, screen, token, b, keys, aReplies, bReplies, wake, blocker>>

Pending == {"queued", "held"}
Delivered == {"pasted", "running", "ended", "answered"}
\* i8432: the watcher refuses A's soft terminal, so A's token outlives A's answer.
TerminalOk == Scenario # "i8432"

Init == /\ a = IF Scenario = "idle" THEN "none" ELSE "running"
        /\ screen = IF Scenario = "idle" THEN "Idle" ELSE "Busy"
        /\ token = IF Scenario = "idle" THEN "none" ELSE "A"
        /\ b = "absent" /\ keys = 0 /\ aReplies = 0 /\ bReplies = 0
        /\ wake = FALSE /\ blocker = Blocker

\* A's answer is relayed; only a submitted terminal releases A's token and wakes the owner.
AEnds == /\ a = "running"
         /\ a' = "ended" /\ screen' = "Idle" /\ aReplies' = aReplies + 1
         /\ token' = IF TerminalOk /\ token = "A" THEN "none" ELSE token
         /\ wake' = (wake \/ TerminalOk)
         /\ UNCHANGED <<b, keys, bReplies, blocker>>

Arrive == /\ b = "absent"
          /\ b' = "queued" /\ wake' = TRUE
          /\ UNCHANGED <<a, screen, token, keys, aReplies, bReplies, blocker>>

Hold == b' = "held" /\ UNCHANGED <<keys, token>>
Paste(owner) == b' = "pasted" /\ keys' = keys + 1 /\ token' = owner

\* owner_drive: has_active_turn -> hand_busy (Busy screen pastes, Idle screen is
\* NotSent busy_source_unavailable), else hand_idle claims the token for B's own turn.
DriveCurrent == /\ Policy = "Current" /\ b \in Pending /\ wake
                /\ wake' = FALSE
                /\ IF blocker # "none" THEN Hold
                   ELSE IF token # "none"
                        THEN IF screen = "Busy" THEN Paste(token) ELSE Hold
                        ELSE Paste("B")
                /\ UNCHANGED <<a, screen, aReplies, bReplies, blocker>>

\* Target: only a hold condition keeps B; the token and the screen do not.
DriveTarget == /\ Policy = "Target" /\ b \in Pending /\ wake
               /\ wake' = FALSE
               /\ IF blocker # "none" THEN Hold ELSE Paste(token)
               /\ UNCHANGED <<a, screen, aReplies, bReplies, blocker>>

\* held_watch / busy retry re-drive a held front.
Retry == /\ b = "held" /\ ~wake
         /\ wake' = TRUE
         /\ UNCHANGED <<a, screen, token, b, keys, aReplies, bReplies, blocker>>

\* The TUI runs a pasted input when its screen is Idle; B's echo enters the transcript here.
TuiStart == /\ b = "pasted" /\ screen = "Idle"
            /\ b' = "running" /\ screen' = "Busy"
            /\ UNCHANGED <<a, token, keys, aReplies, bReplies, wake, blocker>>

BEnds == /\ b = "running"
         /\ b' = "ended" /\ screen' = "Idle"
         /\ token' = IF TerminalOk /\ token = "B" THEN "none" ELSE token
         /\ UNCHANGED <<a, keys, aReplies, bReplies, wake, blocker>>

\* Current posts through B's own turn or A's busy child; Target posts by B's echo.
\* TokenAttribution mutant: the answer goes to whoever holds the token.
PostB == /\ b = "ended"
         /\ b' = "answered"
         /\ IF Mutant = "TokenAttribution" /\ token = "A"
            THEN aReplies' = aReplies + 1 /\ UNCHANGED bReplies
            ELSE IF Mutant = "TokenAttribution" /\ token = "none"
            THEN UNCHANGED <<aReplies, bReplies>>
            ELSE bReplies' = bReplies + 1 /\ UNCHANGED aReplies
         /\ UNCHANGED <<a, screen, token, keys, wake, blocker>>

\* Draft, modal, missing session or unready attachment ends and wakes the owner.
ClearBlocker == /\ BlockerClears /\ blocker # "none"
                /\ blocker' = "none" /\ wake' = TRUE
                /\ UNCHANGED <<a, screen, token, b, keys, aReplies, bReplies>>

\* Idle cap safety net: a draft is stashed, B is submitted, the draft is restored.
CapStash == /\ Cap = "IdleStash" /\ b = "held" /\ screen = "Idle" /\ blocker = "draft"
            /\ Paste(token)
            /\ UNCHANGED <<a, screen, aReplies, bReplies, wake, blocker>>

\* Mutants: resend after no echo yet; the TUI drops an input pasted while busy.
ResendBeforeEcho == /\ Mutant = "ResendBeforeEcho" /\ b = "pasted" /\ keys = 1
                    /\ keys' = 2
                    /\ UNCHANGED <<a, screen, token, b, aReplies, bReplies, wake, blocker>>
TuiDrop == /\ Mutant = "TuiDropsQueued" /\ b = "pasted" /\ screen = "Busy"
           /\ b' = "lost"
           /\ UNCHANGED <<a, screen, token, keys, aReplies, bReplies, wake, blocker>>

Done == b \in {"answered", "lost"} /\ UNCHANGED vars

Next == AEnds \/ Arrive \/ DriveCurrent \/ DriveTarget \/ Retry \/ TuiStart \/ BEnds
        \/ PostB \/ ClearBlocker \/ CapStash \/ ResendBeforeEcho \/ TuiDrop \/ Done
Fairness == /\ WF_vars(AEnds) /\ WF_vars(Arrive) /\ WF_vars(DriveCurrent)
            /\ WF_vars(DriveTarget) /\ WF_vars(Retry) /\ WF_vars(TuiStart)
            /\ WF_vars(BEnds) /\ WF_vars(PostB) /\ WF_vars(ClearBlocker)
            /\ WF_vars(CapStash)
Spec == Init /\ [][Next]_vars /\ Fairness

TypeOK == /\ a \in {"none", "running", "ended"} /\ screen \in {"Busy", "Idle"}
          /\ token \in {"none", "A", "B"}
          /\ b \in {"absent", "queued", "held", "pasted", "running", "ended", "answered", "lost"}
          /\ keys \in 0..2 /\ aReplies \in 0..2 /\ bReplies \in 0..1
          /\ blocker \in {"none", "draft", "modal", "nosession", "attach"}
NoDuplicate == keys <= 1
NoSilentLoss == b # "lost"
ReplyExactlyOnce == /\ aReplies <= 1 /\ bReplies <= 1
                    /\ (b = "answered" => bReplies = 1)
\* A front leaves the owner queue only through its one key effect.
KeyBeforeLeave == b \in Delivered => keys >= 1

IdleDelivers == (b \in Pending /\ screen = "Idle") ~> (b \in Delivered)
ReplyPosted == <>(bReplies = 1)
=============================================================================
