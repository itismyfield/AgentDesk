----------------------- MODULE WriterAbsorbedNotice -----------------------
EXTENDS Naturals, FiniteSets

CONSTANTS Mode, Scope
VARIABLES forms, handed, hookObserved, enqueued, spool, scanned, evidence, prepared,
          effects, posted, restarted, replay, owned, physicalReceipt, commandEvidence, excluded
vars == <<forms, handed, hookObserved, enqueued, spool, scanned, evidence, prepared, effects,
          posted, restarted, replay, owned, physicalReceipt, commandEvidence, excluded>>

\* Forms of one human input on a confirmed Claude TranscriptTurns channel.
Forms == {"user", "command", "remove"}
OriginalForms == {{ "user" }, { "command" }, {"user", "command"}}
LogicalKey == "human-input"
InputKey(form) == IF Mode = "split-key" /\ form = "command"
                  THEN "attachment-row" ELSE LogicalKey
Eligible(form) == CASE form = "user" -> TRUE
                    [] form = "command" -> Mode # "current"
                    [] form = "remove" -> Mode \notin {"current", "w1", "no-remove"}

Init ==
    /\ CASE Scope = "w1" -> owned = FALSE /\ forms \in OriginalForms
         [] Scope = "remove" -> owned = FALSE /\ forms \in {{"remove"}, Forms}
         [] Scope = "owner" -> owned = TRUE /\ forms = {"command", "user"}
    /\ handed = FALSE /\ hookObserved = FALSE /\ enqueued = FALSE /\ restarted = FALSE
    /\ spool = {} /\ scanned = {} /\ replay = {}
    /\ evidence = {} /\ prepared = {} /\ effects = {} /\ posted = {} /\ excluded = {}
    /\ physicalReceipt = FALSE /\ commandEvidence = FALSE

Handoff ==
    /\ ~handed /\ handed' = TRUE
    /\ UNCHANGED <<forms, hookObserved, enqueued, spool, scanned, evidence, prepared,
                    effects, posted, restarted, replay, owned, physicalReceipt, commandEvidence,
                    excluded>>

\* The live observer records its announced ID after handoff without a Discord POST.
Observe ==
    /\ handed /\ ~owned /\ ~hookObserved /\ hookObserved' = TRUE
    /\ UNCHANGED <<forms, handed, enqueued, spool, scanned, evidence, prepared, effects,
                    posted, restarted, replay, owned, physicalReceipt, commandEvidence, excluded>>

Enqueue ==
    /\ handed /\ "remove" \in forms /\ ~enqueued /\ enqueued' = TRUE
    /\ UNCHANGED <<forms, handed, hookObserved, spool, scanned, evidence, prepared, effects,
                    posted, restarted, replay, owned, physicalReceipt, commandEvidence, excluded>>

\* The owner may have authenticated the physical command row before the writer reads it.
RecordPhysicalReceipt ==
    /\ handed /\ owned /\ ~physicalReceipt /\ physicalReceipt' = TRUE
    /\ UNCHANGED <<forms, handed, hookObserved, enqueued, spool, scanned, evidence, prepared,
                    effects, posted, restarted, replay, owned, commandEvidence, excluded>>

Capture(form) ==
    /\ handed /\ form \in forms \ spool /\ replay = {}
    /\ (form = "remove" => enqueued)
    /\ (owned /\ form = "user" => "command" \in scanned /\ physicalReceipt)
    /\ spool' = spool \cup {form}
    /\ UNCHANGED <<forms, handed, hookObserved, enqueued, scanned, evidence, prepared,
                    effects, posted, restarted, replay, owned, physicalReceipt, commandEvidence,
                    excluded>>

\* Projection and Materialized append use the same candidate and key for live and replay.
Classify(form) ==
    /\ form \in spool \ scanned
    /\ scanned' = scanned \cup {form}
    /\ LET commandProof == owned /\ form = "command"
           retained == Eligible(form) /\
              ~(commandProof /\ physicalReceipt /\ Mode \in {"w1", "drop-owned-evidence"})
       IN /\ evidence' = IF retained THEN evidence \cup {InputKey(form)} ELSE evidence
          /\ commandEvidence' = (commandEvidence \/ (retained /\ commandProof))
    /\ UNCHANGED <<forms, handed, hookObserved, enqueued, spool, prepared, effects, posted,
                    restarted, owned, physicalReceipt, excluded>>

Project(form) == Classify(form) /\ replay = {} /\ UNCHANGED replay
Replay(form) == Classify(form) /\ form \in replay /\ replay' = replay \ {form}

SettleOwned ==
    /\ physicalReceipt /\ commandEvidence /\ LogicalKey \in evidence \ excluded
    /\ excluded' = excluded \cup {LogicalKey}
    /\ UNCHANGED <<forms, handed, hookObserved, enqueued, spool, scanned, evidence, prepared,
                    effects, posted, restarted, replay, owned, physicalReceipt, commandEvidence>>

\* In the owned counterexample, another receipt uses the offer before the first delivery drain.
Prepare(key) ==
    /\ replay = {} /\ key \in evidence \ (prepared \cup excluded)
    /\ (~owned \/ (physicalReceipt /\ "user" \in scanned /\ ~commandEvidence))
    /\ prepared' = prepared \cup {key}
    /\ UNCHANGED <<forms, handed, hookObserved, enqueued, spool, scanned, evidence,
                    effects, posted, restarted, replay, owned, physicalReceipt, commandEvidence,
                    excluded>>

Post(key) ==
    /\ replay = {} /\ key \in prepared \ effects
    /\ effects' = effects \cup {key}
    /\ UNCHANGED <<forms, handed, hookObserved, enqueued, spool, scanned, evidence,
                    prepared, posted, restarted, replay, owned, physicalReceipt, commandEvidence,
                    excluded>>

Confirm(key) ==
    /\ replay = {} /\ key \in effects \ posted
    /\ posted' = posted \cup {key}
    /\ UNCHANGED <<forms, handed, hookObserved, enqueued, spool, scanned, evidence,
                    prepared, effects, restarted, replay, owned, physicalReceipt, commandEvidence,
                    excluded>>

\* Retained spool, receipt, exclusion, and known or reconcilable POST effects survive restart.
Restart ==
    /\ ~restarted /\ spool # {}
    /\ restarted' = TRUE /\ scanned' = {} /\ replay' = spool
    /\ UNCHANGED <<forms, handed, hookObserved, enqueued, spool, evidence, prepared, effects,
                    posted, owned, physicalReceipt, commandEvidence, excluded>>

Settled ==
    /\ (owned \/ hookObserved) /\ restarted /\ scanned = forms /\ replay = {}
    /\ posted \cup excluded = evidence
Done == Settled /\ UNCHANGED vars
Keys == {LogicalKey, "attachment-row"}
Next == Handoff \/ Observe \/ Enqueue \/ RecordPhysicalReceipt \/ SettleOwned \/ Restart \/ Done
        \/ (\E form \in Forms : Capture(form) \/ Project(form) \/ Replay(form))
        \/ (\E key \in Keys : Prepare(key) \/ Post(key) \/ Confirm(key))
Spec == Init /\ [][Next]_vars
        /\ WF_vars(Handoff) /\ WF_vars(Observe) /\ WF_vars(Enqueue) /\ WF_vars(Restart)
        /\ WF_vars(RecordPhysicalReceipt) /\ WF_vars(SettleOwned)
        /\ (\A form \in Forms : WF_vars(Capture(form)) /\ WF_vars(Project(form))
                               /\ WF_vars(Replay(form)))
        /\ (\A key \in Keys : WF_vars(Prepare(key)) /\ WF_vars(Post(key))
                             /\ WF_vars(Confirm(key)))

NoticeExactlyOnce == ~owned => /\ Cardinality(effects) <= 1
                               /\ (Settled => Cardinality(effects) = 1)
NoOwnedExtraPost == owned => effects = {}
CanonicalOwnedExclusion == (owned /\ Settled) => LogicalKey \in excluded
EventuallySettles == <>Settled
=============================================================================
