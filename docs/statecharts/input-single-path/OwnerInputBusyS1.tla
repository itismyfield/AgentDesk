---- MODULE OwnerInputBusyS1 ----
\* #8328 stage S1 (design w4-w5): Claude only. One person input B on a channel whose earlier turn
\* A may leave its mailbox token behind, and whose transcript source may not be bound yet.
\* Policy Current: the owner drive splits on the mailbox active turn (hand_busy / hand_idle), and
\*   every pre-key path wants a bound source: the tmux self-turn observer (warm_followup
\*   owner_observer -> idle_unobserved), Herdr Context::read (Held without a source -> admits
\*   false), BusyObservation::admits path match, Producer::wrap pin_source, adapter metadata.
\* Policy Target (S1): only a hold condition keeps B. The anchor, nonce, frame hash and target
\*   incarnation are registered durably before the key; with no Bound record the pin is deferred.
\*   design w5:
\*   - transcript creation (trans) and AgentDesk's source binding (src) are separate facts;
\*   - a deferred pin is confirmed once, BindingIntent.selected None -> Some (Select);
\*   - J2 binds both an untagged Idle/Cold opener A (native membership receipt: ticket, row,
\*     range, frame hash) and a tagged B; the joined mark and every answer wait for that binding.
\* S1 adds no execution-capacity acquire; Codex Tab and the shared lease are stage S2.
EXTENDS Naturals
CONSTANTS Policy, Scenario, Blocker, BlockerClears, Mutant
VARIABLES a, screen, token, b, keys, aReplies, bReplies, wake, blocker,
          mark, merged, aTo, bTo, trans, src, reg, why, pin, sel, aBound, bBound
vars == <<a, screen, token, b, keys, aReplies, bReplies, wake, blocker,
          mark, merged, aTo, bTo, trans, src, reg, why, pin, sel, aBound, bBound>>
Bind == <<pin, sel, aBound, bBound>>

Pending == {"queued", "held"}
Delivered == {"pasted", "joined", "running", "ended", "answered"}
TerminalOk == Scenario # "i8432"
\* A turn AgentDesk did not start has no Discord anchor and no mailbox token. In "nearmiss" that
\* external prompt equals B's frame only after trimming: no exact receipt names it, so it binds
\* nothing (ClassHashFallback mutant binds it to B by the trimmed class hash).
External == Scenario \in {"external", "nearmiss"}
AnchoredA == ~External
ATarget == IF AnchoredA THEN "A" ELSE IF merged THEN "B" ELSE "none"
\* Pre-key checks that want a bound source (mutants of Target; all of them in Current).
SourceGate == \/ Policy = "Current"
              \/ Mutant \in {"PinSourceFirst", "RequireTranscript"}
              \/ Mutant \in {"AdmitsPathMatch", "RequireBusyReadStateWithoutSource"}
                    /\ token # "none"
              \/ Mutant = "RequireIdleObserverBeforeKey" /\ token = "none"

Init == /\ a = IF Scenario = "fresh" THEN "none" ELSE "running"
        /\ screen = IF Scenario = "fresh" THEN "Idle" ELSE "Busy"
        /\ token = IF Scenario = "fresh" \/ External THEN "none" ELSE "A"
        /\ b = "absent" /\ keys = 0 /\ aReplies = 0 /\ bReplies = 0
        /\ wake = FALSE /\ blocker = Blocker
        /\ mark = FALSE /\ merged = FALSE /\ aTo = "unset" /\ bTo = "unset"
        \* fresh: no transcript file until a turn writes it. rebind: the file exists but the
        \* writer has not recorded the (re)started execution's Bound yet.
        /\ trans = (Scenario # "fresh")
        /\ src = IF Scenario \in {"fresh", "rebind"} THEN "none" ELSE "bound"
        /\ reg = FALSE /\ why = "none"
        /\ pin = "none" /\ sel = FALSE /\ aBound = FALSE /\ bBound = FALSE

\* A's answer goes out once its anchor is bound (rule 6.2-5); a joined B must be bound too.
\* TaggedHeaderOnly mutant: an untagged opener is never bound, so A posts without waiting and the
\* anchor falls to "the first joined input".
AEnds == /\ a = "running"
         /\ ~AnchoredA \/ aBound \/ Mutant = "TaggedHeaderOnly"
         /\ b = "joined" => bBound
         /\ a' = "ended" /\ screen' = "Idle" /\ aReplies' = aReplies + 1
         /\ token' = IF TerminalOk /\ token = "A" THEN "none" ELSE token
         /\ wake' = (wake \/ TerminalOk)
         /\ IF b = "joined"
            THEN /\ b' = "answered" /\ merged' = TRUE
                 /\ bReplies' = IF Mutant = "JoinDoublePost" THEN bReplies + 1 ELSE bReplies
            ELSE UNCHANGED <<b, merged, bReplies>>
         /\ aTo' = CASE Mutant = "NoReference" -> "none"
                      [] AnchoredA /\ aBound -> "A"
                      [] b = "joined" /\ bBound -> "B"
                      [] Scenario = "nearmiss" /\ Mutant = "ClassHashFallback" /\ reg -> "B"
                      [] Mutant = "PostOrder" /\ b = "pasted" -> "B"
                      [] OTHER -> "none"
         /\ bTo' = IF Mutant = "JoinDoublePost" /\ b = "joined" THEN "B" ELSE bTo
         /\ UNCHANGED <<keys, blocker, mark, trans, src, reg, why>> /\ UNCHANGED Bind

Arrive == /\ b = "absent"
          /\ b' = "queued" /\ wake' = TRUE
          /\ UNCHANGED <<a, screen, token, keys, aReplies, bReplies, blocker>>
          /\ UNCHANGED <<mark, merged, aTo, bTo, trans, src, reg, why>> /\ UNCHANGED Bind

Hold(r) == b' = "held" /\ why' = r /\ UNCHANGED <<keys, token, mark, reg, pin>>
\* The one key effect (paste + Enter), after the durable registration (RegisterAfterKey mutant:
\* after the key). The pin is recorded when a Bound exists, else deferred. Current marks a busy
\* paste "joined" at once; Target marks only from J2's absorb binding (MarkOnPaste mutant).
Paste(owner) == /\ b' = "pasted" /\ keys' = keys + 1 /\ why' = "none"
                /\ token' = IF owner = "none" THEN "B" ELSE owner
                /\ reg' = (reg \/ Mutant # "RegisterAfterKey")
                /\ pin' = IF src = "bound" THEN "recorded" ELSE "deferred"
                /\ mark' = IF Policy = "Current" \/ Mutant = "MarkOnPaste"
                           THEN screen = "Busy" ELSE mark

DriveCurrent == /\ Policy = "Current" /\ b \in Pending /\ wake
                /\ wake' = FALSE
                /\ IF blocker # "none" THEN Hold("blocker")
                   ELSE IF src = "none" THEN Hold("source")
                   ELSE IF token # "none"
                        THEN IF screen = "Busy" THEN Paste(token) ELSE Hold("busy_unavailable")
                        ELSE Paste("none")
                /\ UNCHANGED <<a, screen, aReplies, bReplies, blocker, merged, aTo, bTo>>
                /\ UNCHANGED <<trans, src, sel, aBound, bBound>>

DriveTarget == /\ Policy = "Target" /\ b \in Pending /\ wake
               /\ wake' = FALSE
               /\ IF blocker # "none" THEN Hold("blocker")
                  ELSE IF SourceGate /\ src = "none" THEN Hold("source")
                  ELSE Paste(token)
               /\ UNCHANGED <<a, screen, aReplies, bReplies, blocker, merged, aTo, bTo>>
               /\ UNCHANGED <<trans, src, sel, aBound, bBound>>

RegisterLate == /\ Mutant = "RegisterAfterKey" /\ keys >= 1 /\ ~reg
                /\ reg' = TRUE
                /\ UNCHANGED <<a, screen, token, b, keys, aReplies, bReplies, wake, blocker>>
                /\ UNCHANGED <<mark, merged, aTo, bTo, trans, src, why>> /\ UNCHANGED Bind

\* The writer records the execution's Bound once a transcript exists. NoLateBind mutant: never.
BindSource == /\ src = "none" /\ trans /\ Mutant # "NoLateBind"
              /\ src' = "bound"
              /\ UNCHANGED <<a, screen, token, b, keys, aReplies, bReplies, wake, blocker>>
              /\ UNCHANGED <<mark, merged, aTo, bTo, trans, reg, why>> /\ UNCHANGED Bind

\* The single confirmation point: BindingIntent.selected None -> Some, once. A recorded pin
\* matches its source; a deferred pin takes the first same-lineage Bound after its watermark.
\* DeferredEquality mutant: the deferred pin is still compared with its unset session/path.
Select == /\ pin # "none" /\ ~sel /\ src = "bound"
          /\ ~(Mutant = "DeferredEquality" /\ pin = "deferred")
          /\ sel' = TRUE
          /\ UNCHANGED <<a, screen, token, b, keys, aReplies, bReplies, wake, blocker>>
          /\ UNCHANGED <<mark, merged, aTo, bTo, trans, src, reg, why, pin, aBound, bBound>>

\* J2 binds the untagged opener A by its membership receipt (needs A's rows in a bound source).
BindA == /\ AnchoredA /\ a # "none" /\ ~aBound /\ src = "bound"
         /\ Mutant # "TaggedHeaderOnly"
         /\ aBound' = TRUE
         /\ UNCHANGED <<a, screen, token, b, keys, aReplies, bReplies, wake, blocker>>
         /\ UNCHANGED <<mark, merged, aTo, bTo, trans, src, reg, why, pin, sel, bBound>>

\* J2 binds B (receipt or nonce tag) after the history binding and custody ACK of its source.
\* An absorbed B gets the joined mark here (JoinUnmarked mutant drops it).
BindB == /\ b \in {"joined", "running", "ended"} /\ ~bBound /\ sel /\ reg
         /\ bBound' = TRUE
         /\ mark' = IF Policy = "Target" /\ b = "joined" THEN Mutant # "JoinUnmarked" ELSE mark
         /\ UNCHANGED <<a, screen, token, b, keys, aReplies, bReplies, wake, blocker>>
         /\ UNCHANGED <<merged, aTo, bTo, trans, src, reg, why, pin, sel, aBound>>

Retry == /\ b = "held" /\ ~wake
         /\ wake' = TRUE
         /\ UNCHANGED <<a, screen, token, b, keys, aReplies, bReplies, blocker>>
         /\ UNCHANGED <<mark, merged, aTo, bTo, trans, src, reg, why>> /\ UNCHANGED Bind

\* Claude takes a pasted B into the running turn at A's next tool boundary. No fairness.
TuiAbsorb == /\ b = "pasted" /\ a = "running"
             /\ b' = "joined"
             /\ UNCHANGED <<a, screen, token, keys, aReplies, bReplies, wake, blocker>>
             /\ UNCHANGED <<mark, merged, aTo, bTo, trans, src, reg, why>> /\ UNCHANGED Bind

\* B's own turn: the TUI writes the transcript (creation only; binding is BindSource).
TuiStart == /\ b = "pasted" /\ screen = "Idle"
            /\ b' = "running" /\ screen' = "Busy" /\ trans' = TRUE
            /\ UNCHANGED <<a, token, keys, aReplies, bReplies, wake, blocker>>
            /\ UNCHANGED <<mark, merged, aTo, bTo, src, reg, why>> /\ UNCHANGED Bind

BEnds == /\ b = "running"
         /\ b' = "ended" /\ screen' = "Idle"
         /\ token' = IF TerminalOk /\ token = "B" THEN "none" ELSE token
         /\ UNCHANGED <<a, keys, aReplies, bReplies, wake, blocker>>
         /\ UNCHANGED <<mark, merged, aTo, bTo, trans, src, reg, why>> /\ UNCHANGED Bind

PostB == /\ b = "ended" /\ bBound
         /\ b' = "answered"
         /\ IF Mutant = "TokenAttribution" /\ token = "A"
            THEN aReplies' = aReplies + 1 /\ UNCHANGED bReplies
            ELSE IF Mutant = "TokenAttribution" /\ token = "none"
            THEN UNCHANGED <<aReplies, bReplies>>
            ELSE bReplies' = bReplies + 1 /\ UNCHANGED aReplies
         /\ bTo' = IF Mutant = "NoReference" THEN "none" ELSE "B"
         /\ UNCHANGED <<a, screen, token, keys, wake, blocker>>
         /\ UNCHANGED <<mark, merged, aTo, trans, src, reg, why>> /\ UNCHANGED Bind

ClearBlocker == /\ BlockerClears /\ blocker # "none"
                /\ blocker' = "none" /\ wake' = TRUE
                /\ UNCHANGED <<a, screen, token, b, keys, aReplies, bReplies>>
                /\ UNCHANGED <<mark, merged, aTo, bTo, trans, src, reg, why>> /\ UNCHANGED Bind

ResendBeforeEcho == /\ Mutant = "ResendBeforeEcho" /\ b = "pasted" /\ keys = 1
                    /\ keys' = 2
                    /\ UNCHANGED <<a, screen, token, b, aReplies, bReplies, wake, blocker>>
                    /\ UNCHANGED <<mark, merged, aTo, bTo, trans, src, reg, why>>
                    /\ UNCHANGED Bind
TuiDrop == /\ Mutant = "TuiDropsQueued" /\ b = "pasted" /\ screen = "Busy"
           /\ b' = "lost"
           /\ UNCHANGED <<a, screen, token, keys, aReplies, bReplies, wake, blocker>>
           /\ UNCHANGED <<mark, merged, aTo, bTo, trans, src, reg, why>> /\ UNCHANGED Bind

\* Stuttering is always allowed; liveness comes from fairness, not from deadlock.
Done == UNCHANGED vars

Next == AEnds \/ Arrive \/ DriveCurrent \/ DriveTarget \/ RegisterLate \/ BindSource \/ Select
        \/ BindA \/ BindB \/ Retry \/ TuiAbsorb \/ TuiStart \/ BEnds \/ PostB \/ ClearBlocker
        \/ ResendBeforeEcho \/ TuiDrop \/ Done
Fairness == /\ WF_vars(AEnds) /\ WF_vars(Arrive) /\ WF_vars(DriveCurrent)
            /\ WF_vars(DriveTarget) /\ WF_vars(RegisterLate) /\ WF_vars(BindSource)
            /\ WF_vars(Select) /\ WF_vars(BindA) /\ WF_vars(BindB)
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
          /\ trans \in BOOLEAN /\ src \in {"none", "bound"} /\ reg \in BOOLEAN
          /\ why \in {"none", "blocker", "source", "busy_unavailable"}
          /\ pin \in {"none", "recorded", "deferred"}
          /\ sel \in BOOLEAN /\ aBound \in BOOLEAN /\ bBound \in BOOLEAN
NoDuplicate == keys <= 1
NoSilentLoss == b # "lost"
ReplyExactlyOnce == /\ aReplies <= 1 /\ bReplies <= 1
                    /\ (b = "answered" => (bReplies = 1) # merged)
JoinedMarked == merged => mark
MarkOnlyJoined == mark => (b = "joined" \/ merged)
AnswerBound == /\ (aReplies >= 1 => aTo = ATarget)
               /\ (bReplies >= 1 => bTo = "B")
KeyBeforeLeave == b \in Delivered => keys >= 1
KeyAfterRegister == keys >= 1 => reg
\* A normal input is never held because its transcript source is not bound yet.
NoSourceHold == why # "source"

IdleDelivers == (b \in Pending /\ screen = "Idle") ~> (b \in Delivered)
ReplyPosted == <>(bReplies = 1 \/ merged)
\* The first input is sent even though only its own turn would create the transcript.
NoCircularWait == (b \in Pending /\ ~trans /\ blocker = "none") ~> (keys >= 1)
=============================================================================
