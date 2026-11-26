---- MODULE OwnerInputBusySplit ----
\* One person input B on a channel whose earlier turn A may leave its mailbox token behind.
\* Policy Current: the owner drive splits on the mailbox active turn (hand_busy / hand_idle).
\* Policy Target (#8328 design w1, user decision busy-inject-step0-1126 a):
\*   Claude - a live session with no hold condition takes B at once. B may join the running
\*            turn A (Claude absorbs it at A's next tool boundary); then A's one answer carries
\*            B's part and B carries the "joined the running turn" mark instead of its own reply.
\*   Codex  - the busy read only picks the key: Busy -> Tab (TUI queue, own turn after A),
\*            Idle -> Enter, Unknown -> UncertainKey. Enter into a busy turn is a steer that
\*            loses A's work (step-0 measurement, 3 runs).
\* Both: B's part is bound to B by where its transcript echo lands, never by the mailbox token,
\* and a missing echo is never a reason to resend.
EXTENDS Naturals
CONSTANTS Policy, Provider, Scenario, Blocker, BlockerClears, Cap, Mutant,
          TabAtIdle, UncertainKey, Unknowable
VARIABLES a, screen, token, b, keys, aReplies, bReplies, wake, blocker,
          seen, unk, mark, merged, aLost
vars == <<a, screen, token, b, keys, aReplies, bReplies, wake, blocker,
          seen, unk, mark, merged, aLost>>

Pending == {"queued", "held"}
Delivered == {"pasted", "joined", "running", "ended", "answered"}
\* i8432: the watcher refuses A's soft terminal, so A's token outlives A's answer.
TerminalOk == Scenario # "i8432"
Fixed == <<seen, unk, mark, merged, aLost>>

Init == /\ a = IF Scenario = "idle" THEN "none" ELSE "running"
        /\ screen = IF Scenario = "idle" THEN "Idle" ELSE "Busy"
        /\ token = IF Scenario = "idle" THEN "none" ELSE "A"
        /\ b = "absent" /\ keys = 0 /\ aReplies = 0 /\ bReplies = 0
        /\ wake = FALSE /\ blocker = Blocker
        /\ seen = "none" /\ unk = FALSE /\ mark = FALSE /\ merged = FALSE /\ aLost = FALSE

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
         /\ UNCHANGED <<keys, blocker, seen, unk, mark, aLost>>

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
                /\ UNCHANGED <<a, screen, aReplies, bReplies, blocker, seen, unk, merged, aLost>>

\* Target Claude: only a hold condition keeps B; the token and the screen do not.
DriveTargetClaude ==
    /\ Policy = "Target" /\ Provider = "claude" /\ b \in Pending /\ wake
    /\ wake' = FALSE
    /\ IF blocker # "none" THEN Hold /\ UNCHANGED mark ELSE Paste(token)
    /\ UNCHANGED <<a, screen, aReplies, bReplies, blocker, seen, unk, merged, aLost>>

\* Target Codex step 1: hold conditions first, then one screen read picks the key. The read may
\* be Unknown once (Unknowable); A may end between this read and the key (the edge case).
ObserveCodex ==
    /\ Policy = "Target" /\ Provider = "codex" /\ b \in Pending /\ wake
    /\ wake' = FALSE
    /\ IF blocker # "none"
       THEN Hold /\ UNCHANGED <<seen, unk>>
       ELSE \E s \in {screen} \cup (IF Unknowable /\ ~unk THEN {"Unknown"} ELSE {}) :
               /\ seen' = s /\ unk' = (unk \/ s = "Unknown")
               /\ b' = "keying" /\ UNCHANGED <<keys, token>>
    /\ UNCHANGED <<a, screen, aReplies, bReplies, blocker, mark, merged, aLost>>

Key == IF Mutant = "AlwaysEnter" THEN "Enter"
       ELSE IF seen = "Busy" THEN "Tab"
       ELSE IF seen = "Idle" THEN "Enter"
       ELSE UncertainKey

\* Target Codex step 2: the key lands on the screen as it is now.
\* Enter on a busy turn steers into A and loses A's work; Tab on a busy turn queues B in the TUI
\* for its own turn after A; Tab on an idle screen does what TabAtIdle says (measurement leg).
KeyCodex ==
    /\ b = "keying"
    /\ IF Key = "Hold"
       THEN /\ b' = "held" /\ UNCHANGED <<keys, token, aLost>>
       ELSE /\ keys' = keys + 1
            /\ token' = IF token = "none" THEN "B" ELSE token
            /\ IF Key = "Enter" /\ screen = "Busy"
               THEN b' = "joined" /\ aLost' = TRUE
               ELSE IF Key = "Enter" \/ screen = "Busy" \/ TabAtIdle = "submits"
               THEN b' = "pasted" /\ UNCHANGED aLost
               ELSE b' = "stuck" /\ UNCHANGED aLost
    /\ UNCHANGED <<a, screen, aReplies, bReplies, wake, blocker, seen, unk, mark, merged>>

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
         /\ UNCHANGED <<a, screen, token, keys, wake, blocker>>
         /\ UNCHANGED Fixed

\* Draft, modal, missing session, unready attachment/composer or recovery kickoff ends.
ClearBlocker == /\ BlockerClears /\ blocker # "none"
                /\ blocker' = "none" /\ wake' = TRUE
                /\ UNCHANGED <<a, screen, token, b, keys, aReplies, bReplies>>
                /\ UNCHANGED Fixed

\* Idle cap safety net (#8430 option not taken by U1): stash the draft, submit B, restore.
CapStash == /\ Cap = "IdleStash" /\ b = "held" /\ screen = "Idle" /\ blocker = "draft"
            /\ Paste(token)
            /\ UNCHANGED <<a, screen, aReplies, bReplies, wake, blocker, seen, unk, merged, aLost>>

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

Next == AEnds \/ Arrive \/ DriveCurrent \/ DriveTargetClaude \/ ObserveCodex \/ KeyCodex
        \/ Retry \/ TuiAbsorb \/ TuiStart \/ BEnds \/ PostB \/ ClearBlocker \/ CapStash
        \/ ResendBeforeEcho \/ TuiDrop \/ Done
Fairness == /\ WF_vars(AEnds) /\ WF_vars(Arrive) /\ WF_vars(DriveCurrent)
            /\ WF_vars(DriveTargetClaude) /\ WF_vars(ObserveCodex) /\ WF_vars(KeyCodex)
            /\ WF_vars(Retry) /\ WF_vars(TuiStart) /\ WF_vars(BEnds) /\ WF_vars(PostB)
            /\ WF_vars(ClearBlocker) /\ WF_vars(CapStash)
Spec == Init /\ [][Next]_vars /\ Fairness

TypeOK == /\ a \in {"none", "running", "ended"} /\ screen \in {"Busy", "Idle"}
          /\ token \in {"none", "A", "B"}
          /\ b \in {"absent", "queued", "held", "keying", "pasted", "joined", "running",
                    "ended", "answered", "stuck", "lost"}
          /\ keys \in 0..2 /\ aReplies \in 0..2 /\ bReplies \in 0..2
          /\ blocker \in {"none", "draft", "modal", "nosession", "attach", "composer", "recovery"}
          /\ seen \in {"none", "Busy", "Idle", "Unknown"}
          /\ unk \in BOOLEAN /\ mark \in BOOLEAN /\ merged \in BOOLEAN /\ aLost \in BOOLEAN
NoDuplicate == keys <= 1
\* Neither B nor A's work disappears without a notice (a Codex steer loses A's work).
NoSilentLoss == b # "lost" /\ ~aLost
\* Each answer at most once; an answered B has exactly one share: its own reply or A's answer.
ReplyExactlyOnce == /\ aReplies <= 1 /\ bReplies <= 1
                    /\ (b = "answered" => (bReplies = 1) # merged)
\* A Claude B whose part rides in A's answer carries the joined mark, so no reply of its own
\* is expected.
JoinedMarked == (merged /\ Provider = "claude") => mark
\* A front leaves the owner queue only through its one key effect.
KeyBeforeLeave == b \in Delivered => keys >= 1

IdleDelivers == (b \in Pending /\ screen = "Idle") ~> (b \in Delivered)
ReplyPosted == <>(bReplies = 1 \/ merged)
=============================================================================
