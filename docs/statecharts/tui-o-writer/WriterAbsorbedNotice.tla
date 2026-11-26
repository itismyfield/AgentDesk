----------------------- MODULE WriterAbsorbedNotice -----------------------
EXTENDS Naturals, FiniteSets

CONSTANT Mode
VARIABLES forms, handed, hookObserved, spool, scanned, evidence, prepared,
          effects, posted, restarted, replay
vars == <<forms, handed, hookObserved, spool, scanned, evidence, prepared,
          effects, posted, restarted, replay>>

\* Forms of one eligible human input on a confirmed Claude TranscriptTurns channel.
Forms == {"user", "command"}
InputKey(form) == IF Mode = "split-key" /\ form = "command"
                  THEN "attachment-row" ELSE "human-input"
Eligible(form) == form = "user" \/ Mode # "current"

Init ==
    /\ forms \in {{ "user" }, { "command" }, Forms}
    /\ handed = FALSE /\ hookObserved = FALSE /\ restarted = FALSE
    /\ spool = {} /\ scanned = {} /\ replay = {}
    /\ evidence = {} /\ prepared = {} /\ effects = {} /\ posted = {}

Handoff ==
    /\ ~handed /\ handed' = TRUE
    /\ UNCHANGED <<forms, hookObserved, spool, scanned, evidence, prepared,
                    effects, posted, restarted, replay>>

\* The live observer records its announced ID after handoff without a Discord POST.
Observe ==
    /\ handed /\ ~hookObserved /\ hookObserved' = TRUE
    /\ UNCHANGED <<forms, handed, spool, scanned, evidence, prepared,
                    effects, posted, restarted, replay>>

Capture(form) ==
    /\ handed /\ form \in forms \ spool /\ replay = {}
    /\ spool' = spool \cup {form}
    /\ UNCHANGED <<forms, handed, hookObserved, scanned, evidence, prepared,
                    effects, posted, restarted, replay>>

\* Projection and Materialized append use the same candidate and key for live and replay.
Classify(form) ==
    /\ form \in spool \ scanned
    /\ scanned' = scanned \cup {form}
    /\ evidence' = IF Eligible(form) THEN evidence \cup {InputKey(form)} ELSE evidence
    /\ UNCHANGED <<forms, handed, hookObserved, spool, prepared, effects, posted, restarted>>

Project(form) == Classify(form) /\ replay = {} /\ UNCHANGED replay
Replay(form) == Classify(form) /\ form \in replay /\ replay' = replay \ {form}

Prepare(key) ==
    /\ replay = {} /\ key \in evidence \ prepared
    /\ prepared' = prepared \cup {key}
    /\ UNCHANGED <<forms, handed, hookObserved, spool, scanned, evidence,
                    effects, posted, restarted, replay>>

Post(key) ==
    /\ replay = {} /\ key \in prepared \ effects
    /\ effects' = effects \cup {key}
    /\ UNCHANGED <<forms, handed, hookObserved, spool, scanned, evidence,
                    prepared, posted, restarted, replay>>

Confirm(key) ==
    /\ replay = {} /\ key \in effects \ posted
    /\ posted' = posted \cup {key}
    /\ UNCHANGED <<forms, handed, hookObserved, spool, scanned, evidence,
                    prepared, effects, restarted, replay>>

\* The captured bytes, Materialized, Prepared, and known or reconcilable effects survive.
Restart ==
    /\ ~restarted /\ spool # {}
    /\ restarted' = TRUE /\ scanned' = {} /\ replay' = spool
    /\ UNCHANGED <<forms, handed, hookObserved, spool, evidence, prepared, effects, posted>>

Settled == hookObserved /\ restarted /\ scanned = forms /\ replay = {} /\ posted = evidence
Done == Settled /\ UNCHANGED vars
Keys == {"human-input", "attachment-row"}
Next == Handoff \/ Observe \/ Restart \/ Done
        \/ (\E form \in Forms : Capture(form) \/ Project(form) \/ Replay(form))
        \/ (\E key \in Keys : Prepare(key) \/ Post(key) \/ Confirm(key))
Spec == Init /\ [][Next]_vars
        /\ WF_vars(Handoff) /\ WF_vars(Observe) /\ WF_vars(Restart)
        /\ (\A form \in Forms : WF_vars(Capture(form)) /\ WF_vars(Project(form))
                               /\ WF_vars(Replay(form)))
        /\ (\A key \in Keys : WF_vars(Prepare(key)) /\ WF_vars(Post(key))
                             /\ WF_vars(Confirm(key)))

NoticeExactlyOnce == /\ Cardinality(effects) <= 1
                     /\ (Settled => Cardinality(effects) = 1)
EventuallySettles == <>Settled
=============================================================================
