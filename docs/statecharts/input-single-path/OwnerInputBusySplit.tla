---- MODULE OwnerInputBusySplit ----
\* One person input B on a channel whose earlier turn A may leave its mailbox token behind.
\* Policy Current: the owner drive splits on the mailbox active turn (hand_busy / hand_idle).
\* Policy Target (#8328 design w1, user decision busy-inject-step0-1126 a):
\*   Claude - a live session with no hold condition takes B at once. B may join the running
\*            turn A (Claude absorbs it at A's next tool boundary); then A's one answer carries
\*            B's part and B carries the "joined the running turn" mark instead of its own reply.
\*   Codex  - (design w2) no busy read at all: paste, wait until the composer shows the paste,
\*            then Tab, then the queue item or the submitted turn is the acceptance. Tab on an
\*            idle screen submits, on a busy one queues B for its own turn after A (i8328tab1).
\*            Enter into a busy turn is a steer that loses A's work (AlwaysEnter mutant).
\* Both: B's part is bound to B by where its transcript echo lands, never by the mailbox token,
\* and a missing echo is never a reason to resend.
\* Attribution (design w2): every posted answer replies to one input anchor. A turn's answer
\* replies to its opener's anchor; a turn with no Discord opener (Scenario "external": TUI
\* direct input or a background notice) replies to the first Discord input that joined it,
\* else to none. Post order never decides the anchor.
EXTENDS Naturals
CONSTANTS Policy, Provider, Scenario, Blocker, BlockerClears, Cap, Mutant
VARIABLES a, screen, token, b, keys, aReplies, bReplies, wake, blocker,
          refl, mark, merged, aLost, aTo, bTo
vars == <<a, screen, token, b, keys, aReplies, bReplies, wake, blocker,
          refl, mark, merged, aLost, aTo, bTo>>

Pending == {"queued", "held"}
Delivered == {"composing", "pasted", "joined", "running", "ended", "answered"}
\* i8432: the watcher refuses A's soft terminal, so A's token outlives A's answer.
TerminalOk == Scenario # "i8432"
\* A turn AgentDesk did not start has no Discord anchor and no mailbox token.
AnchoredA == Scenario # "external"
Fixed == <<refl, mark, merged, aLost, aTo, bTo>>
\* The anchor A's one answer must reply to.
ATarget == IF AnchoredA THEN "A" ELSE IF merged THEN "B" ELSE "none"

Init == /\ a = IF Scenario = "idle" THEN "none" ELSE "running"
        /\ screen = IF Scenario = "idle" THEN "Idle" ELSE "Busy"
        /\ token = IF Scenario \in {"idle", "external"} THEN "none" ELSE "A"
        /\ b = "absent" /\ keys = 0 /\ aReplies = 0 /\ bReplies = 0
        /\ wake = FALSE /\ blocker = Blocker
        /\ refl = FALSE /\ mark = FALSE /\ merged = FALSE /\ aLost = FALSE
        /\ aTo = "unset" /\ bTo = "unset"

\* A's answer is relayed once; a B that joined A rides in that same answer.
\* Only a submitted terminal releases A's token and wakes the owner.
AEnds == /\ a = "running"
         /\ a' = "ended" /\ screen' = "Idle" /\ aReplies' = aReplies + 1
         /\ token' = IF TerminalOk /\ token = "A" THEN "none" ELSE token
         /\ wake' = (wake \/ TerminalOk)
         /\ IF b = "joined"
            THEN /\ b' = "answered" /\ merged' = TRUE
                 /\ bReplies' = IF Mutant = "JoinDoublePost" THEN bReplies + 1 ELSE bReplies
            ELSE UNCHANGED <<b, merged, bReplies>>
         \* The answer's reply anchor. PostOrder mutant: the oldest keyed, unanswered input.
         \* NoReference mutant: posted with no reply reference.
         /\ aTo' = CASE Mutant = "NoReference" -> "none"
                      [] AnchoredA -> "A"
                      [] b = "joined" -> "B"
                      [] Mutant = "PostOrder" /\ b \in {"composing", "pasted"} -> "B"
                      [] OTHER -> "none"
         /\ bTo' = IF Mutant = "JoinDoublePost" /\ b = "joined" THEN "B" ELSE bTo
         /\ UNCHANGED <<keys, blocker, refl, mark, aLost>>

Arrive == /\ b = "absent"
          /\ b' = "queued" /\ wake' = TRUE
          /\ UNCHANGED <<a, screen, token, keys, aReplies, bReplies, blocker>>
          /\ UNCHANGED Fixed

Hold == b' = "held" /\ UNCHANGED <<keys, token>>
\* Claude's single key effect. With no token the start claims one for B's own turn; with a token
\* (live or left behind) B is pasted under it and no second turn is opened. Pasting into a busy
\* screen marks B "joined the running turn" unless the JoinUnmarked mutant drops the mark.
Paste(owner) == /\ b' = "pasted" /\ keys' = keys + 1
                /\ token' = IF owner = "none" THEN "B" ELSE owner
                /\ mark' = (screen = "Busy" /\ Mutant # "JoinUnmarked")

\* owner_drive: has_active_turn -> hand_busy (Busy screen pastes, Idle screen is
\* NotSent busy_source_unavailable), else hand_idle claims the token for B's own turn.
DriveCurrent == /\ Policy = "Current" /\ b \in Pending /\ wake
                /\ wake' = FALSE
                /\ IF blocker # "none" THEN Hold /\ UNCHANGED mark
                   ELSE IF token # "none"
                        THEN IF screen = "Busy" THEN Paste(token) ELSE Hold /\ UNCHANGED mark
                        ELSE Paste("none")
                /\ UNCHANGED <<a, screen, aReplies, bReplies, blocker, refl, merged, aLost, aTo, bTo>>

\* Target Claude: only a hold condition keeps B; the token and the screen do not.
DriveTargetClaude ==
    /\ Policy = "Target" /\ Provider = "claude" /\ b \in Pending /\ wake
    /\ wake' = FALSE
    /\ IF blocker # "none" THEN Hold /\ UNCHANGED mark ELSE Paste(token)
    /\ UNCHANGED <<a, screen, aReplies, bReplies, blocker, refl, merged, aLost, aTo, bTo>>

\* Target Codex, design w2: no busy read. Hold conditions first (draft, modal, completion
\* popup, unready composer, unaccepted earlier input), then one paste.
PasteCodex ==
    /\ Policy = "Target" /\ Provider = "codex" /\ b \in Pending /\ wake
    /\ wake' = FALSE
    /\ IF blocker # "none"
       THEN Hold /\ UNCHANGED refl
       ELSE /\ b' = "composing" /\ keys' = keys + 1 /\ refl' = FALSE
            /\ token' = IF token = "none" THEN "B" ELSE token
    /\ UNCHANGED <<a, screen, aReplies, bReplies, blocker, mark, merged, aLost, aTo, bTo>>

\* The composer shows the pasted frame (the reflect check reads it in the same lock).
Reflect == /\ b = "composing" /\ ~refl
           /\ refl' = TRUE
           /\ UNCHANGED <<a, screen, token, b, keys, aReplies, bReplies, wake, blocker>>
           /\ UNCHANGED <<mark, merged, aLost, aTo, bTo>>

\* The one key. Tab: idle submits, busy queues B for its own turn after A; either way the TUI
\* accepted it. A key before the paste shows lands on a composer still taking the paste burst
\* and leaves B as a draft (NoReflectCheck mutant). AlwaysEnter mutant: Enter steers into A.
KeyCodex ==
    /\ b = "composing" /\ (refl \/ Mutant = "NoReflectCheck")
    /\ IF ~refl
       THEN b' = "stuck" /\ UNCHANGED aLost
       ELSE IF Mutant = "AlwaysEnter" /\ screen = "Busy"
       THEN b' = "joined" /\ aLost' = TRUE
       ELSE b' = "pasted" /\ UNCHANGED aLost
    /\ UNCHANGED <<a, screen, token, keys, aReplies, bReplies, wake, blocker>>
    /\ UNCHANGED <<refl, mark, merged, aTo, bTo>>

\* held_watch / busy retry re-drive a held front.
Retry == /\ b = "held" /\ ~wake
         /\ wake' = TRUE
         /\ UNCHANGED <<a, screen, token, b, keys, aReplies, bReplies, blocker>>
         /\ UNCHANGED Fixed

\* Claude takes a pasted B into the running turn at A's next tool boundary
\* (transcript: enqueue -> remove absorbed_mid_turn / queued_command inside A's turn).
\* No fairness: a turn with no tool boundary left runs B after A instead.
TuiAbsorb == /\ Provider = "claude" /\ b = "pasted" /\ a = "running"
             /\ b' = "joined"
             /\ UNCHANGED <<a, screen, token, keys, aReplies, bReplies, wake, blocker>>
             /\ UNCHANGED Fixed

\* The TUI runs a pasted (or TUI-queued) input when its screen is Idle; B's echo opens B's turn.
TuiStart == /\ b = "pasted" /\ screen = "Idle"
            /\ b' = "running" /\ screen' = "Busy"
            /\ UNCHANGED <<a, token, keys, aReplies, bReplies, wake, blocker>>
            /\ UNCHANGED Fixed

BEnds == /\ b = "running"
         /\ b' = "ended" /\ screen' = "Idle"
         /\ token' = IF TerminalOk /\ token = "B" THEN "none" ELSE token
         /\ UNCHANGED <<a, keys, aReplies, bReplies, wake, blocker>>
         /\ UNCHANGED Fixed

\* B's own turn is posted by B's echo. TokenAttribution mutant: posted to whoever holds the token.
PostB == /\ b = "ended"
         /\ b' = "answered"
         /\ IF Mutant = "TokenAttribution" /\ token = "A"
            THEN aReplies' = aReplies + 1 /\ UNCHANGED bReplies
            ELSE IF Mutant = "TokenAttribution" /\ token = "none"
            THEN UNCHANGED <<aReplies, bReplies>>
            ELSE bReplies' = bReplies + 1 /\ UNCHANGED aReplies
         /\ bTo' = IF Mutant = "NoReference" THEN "none" ELSE "B"
         /\ UNCHANGED <<a, screen, token, keys, wake, blocker>>
         /\ UNCHANGED <<refl, mark, merged, aLost, aTo>>

\* Draft, modal, missing session, unready attachment/composer or recovery kickoff ends.
ClearBlocker == /\ BlockerClears /\ blocker # "none"
                /\ blocker' = "none" /\ wake' = TRUE
                /\ UNCHANGED <<a, screen, token, b, keys, aReplies, bReplies>>
                /\ UNCHANGED Fixed

\* Idle cap safety net (#8430 option not taken by U1): stash the draft, submit B, restore.
CapStash == /\ Cap = "IdleStash" /\ b = "held" /\ screen = "Idle" /\ blocker = "draft"
            /\ Paste(token)
            /\ UNCHANGED <<a, screen, aReplies, bReplies, wake, blocker, refl, merged, aLost, aTo, bTo>>

\* Mutants: resend after no echo yet; the TUI drops an input pasted while busy.
ResendBeforeEcho == /\ Mutant = "ResendBeforeEcho" /\ b = "pasted" /\ keys = 1
                    /\ keys' = 2
                    /\ UNCHANGED <<a, screen, token, b, aReplies, bReplies, wake, blocker>>
                    /\ UNCHANGED Fixed
TuiDrop == /\ Mutant = "TuiDropsQueued" /\ b = "pasted" /\ screen = "Busy"
           /\ b' = "lost"
           /\ UNCHANGED <<a, screen, token, keys, aReplies, bReplies, wake, blocker>>
           /\ UNCHANGED Fixed

Done == b \in {"answered", "lost", "stuck"} /\ UNCHANGED vars

Next == AEnds \/ Arrive \/ DriveCurrent \/ DriveTargetClaude \/ PasteCodex \/ Reflect \/ KeyCodex
        \/ Retry \/ TuiAbsorb \/ TuiStart \/ BEnds \/ PostB \/ ClearBlocker \/ CapStash
        \/ ResendBeforeEcho \/ TuiDrop \/ Done
Fairness == /\ WF_vars(AEnds) /\ WF_vars(Arrive) /\ WF_vars(DriveCurrent)
            /\ WF_vars(DriveTargetClaude) /\ WF_vars(PasteCodex) /\ WF_vars(Reflect)
            /\ WF_vars(KeyCodex)
            /\ WF_vars(Retry) /\ WF_vars(TuiStart) /\ WF_vars(BEnds) /\ WF_vars(PostB)
            /\ WF_vars(ClearBlocker) /\ WF_vars(CapStash)
Spec == Init /\ [][Next]_vars /\ Fairness

TypeOK == /\ a \in {"none", "running", "ended"} /\ screen \in {"Busy", "Idle"}
          /\ token \in {"none", "A", "B"}
          /\ b \in {"absent", "queued", "held", "composing", "pasted", "joined", "running",
                    "ended", "answered", "stuck", "lost"}
          /\ keys \in 0..2 /\ aReplies \in 0..2 /\ bReplies \in 0..2
          /\ blocker \in {"none", "draft", "modal", "nosession", "attach", "composer", "recovery"}
          /\ refl \in BOOLEAN /\ mark \in BOOLEAN /\ merged \in BOOLEAN /\ aLost \in BOOLEAN
          /\ aTo \in {"unset", "none", "A", "B"} /\ bTo \in {"unset", "none", "B"}
NoDuplicate == keys <= 1
\* Neither B nor A's work disappears without a notice (a Codex steer loses A's work).
NoSilentLoss == b # "lost" /\ ~aLost
\* Each answer at most once; an answered B has exactly one share: its own reply or A's answer.
ReplyExactlyOnce == /\ aReplies <= 1 /\ bReplies <= 1
                    /\ (b = "answered" => (bReplies = 1) # merged)
\* A Claude B whose part rides in A's answer carries the joined mark, so no reply of its own
\* is expected.
JoinedMarked == (merged /\ Provider = "claude") => mark
\* Every posted answer replies to the anchor its turn's echo names, never by post order.
AnswerBound == /\ (aReplies >= 1 => aTo = ATarget)
               /\ (bReplies >= 1 => bTo = "B")
\* A front leaves the owner queue only through its one key effect.
KeyBeforeLeave == b \in Delivered => keys >= 1

IdleDelivers == (b \in Pending /\ screen = "Idle") ~> (b \in Delivered)
ReplyPosted == <>(bReplies = 1 \/ merged)
=============================================================================
