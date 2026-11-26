----------------------- MODULE WriterAbsorbedNotice -----------------------
EXTENDS Naturals, FiniteSets

CONSTANTS Mode, Scope
VARIABLES forms, handed, hookObserved, enqueued, spool, scanned, evidence, prepared,
          effects, posted, restarted, replay, owned, physicalReceipt, commandEvidence, excluded, absorption
vars == <<forms, handed, hookObserved, enqueued, spool, scanned, evidence, prepared, effects,
          posted, restarted, replay, owned, physicalReceipt, commandEvidence, excluded, absorption>>

\* Forms of one human input on a confirmed Claude TranscriptTurns channel.
Forms == {"user", "command", "remove"}
OriginalForms == {{ "user" }, { "command" }, {"user", "command"}}
LogicalKey == "human-input"
OwnedRemove == Scope = "owned-remove"
InputKey(form) == IF Mode = "split-key" /\ form = "command"
                  THEN "attachment-row" ELSE LogicalKey
Eligible(form) == CASE form = "user" -> TRUE
                    [] form = "command" -> Mode # "current"
                    [] form = "remove" -> Mode \notin {"current", "w1", "no-remove"}

Init ==
    /\ CASE Scope = "w1" -> owned = FALSE /\ forms \in OriginalForms
         [] Scope = "remove" -> owned = FALSE /\ forms \in {{"remove"}, Forms}
         [] Scope = "owner" -> owned = TRUE /\ forms = {"command", "user"}
         [] OwnedRemove -> owned = TRUE /\ forms \in {{"remove"}, Forms}
    /\ handed = FALSE /\ hookObserved = FALSE /\ enqueued = FALSE /\ restarted = FALSE
    /\ spool = {} /\ scanned = {} /\ replay = {}
    /\ evidence = {} /\ prepared = {} /\ effects = {} /\ posted = {} /\ excluded = {}
    /\ physicalReceipt = FALSE /\ commandEvidence = FALSE
    /\ absorption = [executed |-> FALSE, reviewed |-> FALSE, retirement |-> "waiting"]

Handoff ==
    /\ ~handed /\ handed' = TRUE
    /\ UNCHANGED <<forms, hookObserved, enqueued, spool, scanned, evidence, prepared,
                    effects, posted, restarted, replay, owned, physicalReceipt, commandEvidence,
                    excluded, absorption>>

\* The live observer records its announced ID after handoff without a Discord POST.
Observe ==
    /\ handed /\ ~owned /\ ~hookObserved /\ hookObserved' = TRUE
    /\ UNCHANGED <<forms, handed, enqueued, spool, scanned, evidence, prepared, effects,
                    posted, restarted, replay, owned, physicalReceipt, commandEvidence, excluded, absorption>>

Enqueue ==
    /\ handed /\ "remove" \in forms /\ ~enqueued /\ enqueued' = TRUE
    /\ UNCHANGED <<forms, handed, hookObserved, spool, scanned, evidence, prepared, effects,
                    posted, restarted, replay, owned, physicalReceipt, commandEvidence, excluded, absorption>>

\* The owner may have authenticated the physical command row before the writer reads it.
RecordPhysicalReceipt ==
    /\ handed /\ owned /\ ~OwnedRemove /\ ~physicalReceipt /\ physicalReceipt' = TRUE
    /\ UNCHANGED <<forms, handed, hookObserved, enqueued, spool, scanned, evidence, prepared,
                    effects, posted, restarted, replay, owned, commandEvidence, excluded, absorption>>

Capture(form) ==
    /\ handed /\ form \in forms \ spool /\ replay = {}
    /\ (form = "remove" => enqueued)
    /\ (owned /\ ~OwnedRemove /\ form = "user" => "command" \in scanned /\ physicalReceipt)
    /\ (OwnedRemove /\ form # "remove" => absorption.reviewed)
    /\ spool' = spool \cup {form}
    /\ UNCHANGED <<forms, handed, hookObserved, enqueued, scanned, evidence, prepared,
                    effects, posted, restarted, replay, owned, physicalReceipt, commandEvidence,
                    excluded, absorption>>

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
                    restarted, owned, physicalReceipt, excluded, absorption>>

Project(form) == Classify(form) /\ replay = {} /\ UNCHANGED replay
Replay(form) == Classify(form) /\ form \in replay /\ replay' = replay \ {form}

SettleOwned ==
    /\ physicalReceipt /\ commandEvidence /\ LogicalKey \in evidence \ excluded
    /\ excluded' = excluded \cup {LogicalKey}
    /\ UNCHANGED <<forms, handed, hookObserved, enqueued, spool, scanned, evidence, prepared,
                    effects, posted, restarted, replay, owned, physicalReceipt, commandEvidence, absorption>>

\* In the owned counterexample, another receipt uses the offer before the first delivery drain.
Prepare(key) ==
    /\ replay = {} /\ key \in evidence \ (prepared \cup excluded)
    /\ (~owned \/ (physicalReceipt /\ "user" \in scanned /\ ~commandEvidence))
    /\ prepared' = prepared \cup {key}
    /\ UNCHANGED <<forms, handed, hookObserved, enqueued, spool, scanned, evidence,
                    effects, posted, restarted, replay, owned, physicalReceipt, commandEvidence,
                    excluded, absorption>>

Post(key) ==
    /\ replay = {} /\ key \in prepared \ effects
    /\ effects' = effects \cup {key}
    /\ UNCHANGED <<forms, handed, hookObserved, enqueued, spool, scanned, evidence,
                    prepared, posted, restarted, replay, owned, physicalReceipt, commandEvidence,
                    excluded, absorption>>

Confirm(key) ==
    /\ replay = {} /\ key \in effects \ posted
    /\ posted' = posted \cup {key}
    /\ UNCHANGED <<forms, handed, hookObserved, enqueued, spool, scanned, evidence,
                    prepared, effects, restarted, replay, owned, physicalReceipt, commandEvidence,
                    excluded, absorption>>

\* Retained spool, receipt, exclusion, and known or reconcilable POST effects survive restart.
Restart ==
    /\ ~restarted /\ spool # {}
    /\ restarted' = TRUE /\ scanned' = {} /\ replay' = spool
    /\ UNCHANGED <<forms, handed, hookObserved, enqueued, spool, evidence, prepared, effects,
                    posted, owned, physicalReceipt, commandEvidence, excluded, absorption>>

\* The normal native execution result does not mint a physical receipt for a remove.
NativeExecuted ==
    /\ OwnedRemove /\ "remove" \in spool /\ ~absorption.executed
    /\ absorption' = [absorption EXCEPT !.executed = TRUE]
    /\ UNCHANGED <<forms, handed, hookObserved, enqueued, spool, scanned, evidence, prepared,
                    effects, posted, restarted, replay, owned, physicalReceipt, commandEvidence, excluded>>

\* Re-evaluate a source-bound, uniquely attributed remove at the notice settlement boundary.
ReviewAbsorption ==
    /\ OwnedRemove /\ "remove" \in scanned /\ LogicalKey \in evidence
    /\ ~absorption.reviewed /\ replay = {}
    /\ absorption' = [absorption EXCEPT !.reviewed = TRUE]
    /\ excluded' = IF Mode = "w3" THEN excluded \cup {LogicalKey} ELSE excluded
    /\ UNCHANGED <<forms, handed, hookObserved, enqueued, spool, scanned, evidence, prepared,
                    effects, posted, restarted, replay, owned, physicalReceipt, commandEvidence>>

ReplayComplete ==
    /\ (owned \/ hookObserved) /\ restarted /\ scanned = forms /\ replay = {}
NoticeDebt == Cardinality(evidence \ (posted \cup excluded))

\* Read fresh notice debt once the completed child is otherwise ready to retire.
ReadRetirement ==
    /\ OwnedRemove /\ ReplayComplete /\ absorption.executed /\ absorption.reviewed
    /\ absorption.retirement = "waiting"
    /\ absorption' = [absorption EXCEPT !.retirement =
          IF NoticeDebt = 0 THEN "ready" ELSE "notice-debt"]
    /\ UNCHANGED <<forms, handed, hookObserved, enqueued, spool, scanned, evidence, prepared,
                    effects, posted, restarted, replay, owned, physicalReceipt, commandEvidence, excluded>>

Settled ==
    /\ ReplayComplete /\ NoticeDebt = 0
    /\ (OwnedRemove => absorption.retirement = "ready")
Done == (Settled \/ absorption.retirement = "notice-debt") /\ UNCHANGED vars
Keys == {LogicalKey, "attachment-row"}
Next == Handoff \/ Observe \/ Enqueue \/ RecordPhysicalReceipt \/ SettleOwned \/ Restart \/ Done
        \/ NativeExecuted \/ ReviewAbsorption \/ ReadRetirement
        \/ (\E form \in Forms : Capture(form) \/ Project(form) \/ Replay(form))
        \/ (\E key \in Keys : Prepare(key) \/ Post(key) \/ Confirm(key))
Spec == Init /\ [][Next]_vars
        /\ WF_vars(Handoff) /\ WF_vars(Observe) /\ WF_vars(Enqueue) /\ WF_vars(Restart)
        /\ WF_vars(RecordPhysicalReceipt) /\ WF_vars(SettleOwned)
        /\ WF_vars(NativeExecuted) /\ WF_vars(ReviewAbsorption) /\ WF_vars(ReadRetirement)
        /\ (\A form \in Forms : WF_vars(Capture(form)) /\ WF_vars(Project(form))
                               /\ WF_vars(Replay(form)))
        /\ (\A key \in Keys : WF_vars(Prepare(key)) /\ WF_vars(Post(key))
                             /\ WF_vars(Confirm(key)))

NoticeExactlyOnce == ~owned => /\ Cardinality(effects) <= 1
                               /\ (Settled => Cardinality(effects) = 1)
NoOwnedExtraPost == owned => effects = {}
CanonicalOwnedExclusion == (owned /\ Settled) => LogicalKey \in excluded
OwnedAbsorptionDebtCleared ==
    (OwnedRemove /\ absorption.reviewed) => (NoticeDebt = 0 /\ LogicalKey \in excluded)
NoticeDoesNotBlockRetirement == absorption.retirement # "notice-debt"
EventuallySettles == <>Settled
=============================================================================
