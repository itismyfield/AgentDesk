---- MODULE HerdrBusyInput ----
\* Focused host/readiness refinement of InputOwnership.OwnedPaste/F7OwnerWakeHandBusy.
\* One admitted front, existing owner/lease and persistent input custody are assumed.
EXTENDS Naturals
CONSTANTS Policy, Host, OReady, InitialBlock, Clears, RestartAt, ReplacePane, Mutant,
          Entry, ClearState
VARIABLE s
vars == <<s>>

Init == s = [phase |-> "queued", up |-> TRUE, awake |-> TRUE,
             blocked |-> InitialBlock, notice |-> FALSE, watch |-> FALSE,
             effects |-> 0, proof |-> FALSE, restarts |-> 0,
             generation |-> 1, target |-> 1, replaced |-> FALSE,
             wrongTarget |-> FALSE, observed |-> FALSE, diagnostic |-> FALSE,
             route |-> "none", busyEffects |-> 0]

HostReady == Host = "tmux" \/ (Policy = "fixed" /\ Mutant # "ExcludeHerdr")
Eligible == HostReady /\ (Host = "tmux" \/ OReady)
            /\ ~s.blocked /\ s.target = s.generation

\* Common target observation precedes an unrelated legacy tmux diagnostic in early intake.
ObserveEarly == /\ Entry = "early" /\ s.phase = "queued" /\ ~s.observed
                /\ Policy = "fixed" /\ Mutant # "DiagnosticFirst"
                /\ s' = [s EXCEPT !.observed = TRUE]
LegacyDiagnostic == /\ Entry = "early" /\ s.phase = "queued" /\ ~s.observed
                    /\ Policy = "before" \/ Mutant = "DiagnosticFirst"
                    /\ s' = [s EXCEPT !.observed = TRUE, !.diagnostic = TRUE]

\* A verified awaited clear delegates to existing Idle; it cannot borrow the old busy source.
Drive == /\ s.up /\ s.awake /\ s.phase = "queued"
         /\ Entry = "late" \/ s.observed
         /\ IF ClearState = "verified" /\ Mutant # "IgnoreClear"
            THEN s' = [s EXCEPT !.phase = "idle", !.route = "idle"]
            ELSE IF Eligible
            THEN s' = [s EXCEPT !.phase = "may", !.route = "busy"]
            ELSE s' = [s EXCEPT !.phase = "held", !.watch = TRUE]

ExistingIdle == /\ s.up /\ s.awake /\ s.phase = "idle"
                /\ s' = [s EXCEPT !.phase = "sent", !.effects = @ + 1]

\* The durable may-send boundary precedes payload and Enter. No speculative re-paste follows it.
Effect == /\ s.up /\ s.awake /\ s.phase = "may"
          /\ RestartAt # "may" \/ s.restarts = 1
          /\ ~ReplacePane \/ s.replaced
          /\ IF s.target = s.generation \/ Mutant = "RebindTarget"
             THEN s' = [s EXCEPT !.phase = "sent", !.effects = @ + 1,
                                !.busyEffects = @ + 1,
                                !.wrongTarget = (s.target # s.generation)]
             ELSE s' = [s EXCEPT !.phase = "uncertain", !.watch = TRUE]

\* A transcript user/queue row is provider evidence; transport success alone is not settlement.
NativeProof == /\ s.effects = 1 /\ ~s.proof
               /\ s' = [s EXCEPT !.proof = TRUE]
Settle == /\ s.up /\ s.awake /\ s.phase \in {"sent", "uncertain"} /\ s.proof
          /\ RestartAt # "sent" \/ s.restarts = 1
          /\ s' = [s EXCEPT !.phase = "done"]
Notify == /\ s.up /\ s.awake /\ s.phase \in {"held", "uncertain"}
          /\ ~s.notice /\ Mutant # "SilentHold"
          /\ s' = [s EXCEPT !.notice = TRUE]
ClearBlock == /\ Clears /\ s.blocked
              /\ s' = [s EXCEPT !.blocked = FALSE]
RetryHeld == /\ s.up /\ s.awake /\ s.phase = "held" /\ s.watch /\ Eligible
             /\ s' = [s EXCEPT !.phase = "queued", !.watch = FALSE]

Crash == /\ s.up /\ s.restarts = 0 /\ RestartAt # "none"
         /\ s.phase = RestartAt
         /\ s' = [s EXCEPT !.up = FALSE, !.awake = FALSE, !.restarts = 1,
                            !.phase = "uncertain"]
Boot == /\ ~s.up
        /\ s' = [s EXCEPT !.up = TRUE, !.awake = (Mutant # "NoBootWake")]
Replace == /\ ReplacePane /\ ~s.replaced /\ s.phase = "may"
           /\ s' = [s EXCEPT !.generation = 2, !.replaced = TRUE]

\* Negative controls remove one existing custody/attempt rule at a time.
Replay == /\ Mutant = "ReplayUnknown" /\ s.phase = "uncertain"
          /\ s' = [s EXCEPT !.phase = "may"]
Drop == /\ Mutant = "DropHeld" /\ s.phase = "held"
        /\ s' = [s EXCEPT !.phase = "lost"]
Wait == UNCHANGED vars
Next == ObserveEarly \/ LegacyDiagnostic \/ Drive \/ ExistingIdle
        \/ Effect \/ NativeProof \/ Settle \/ Notify \/ ClearBlock \/ RetryHeld
        \/ Crash \/ Boot \/ Replace \/ Replay \/ Drop \/ Wait
Fairness == WF_vars(ObserveEarly) /\ WF_vars(LegacyDiagnostic) /\ WF_vars(Drive)
            /\ WF_vars(ExistingIdle) /\ WF_vars(Effect) /\ WF_vars(NativeProof)
            /\ WF_vars(Settle) /\ WF_vars(Notify) /\ WF_vars(ClearBlock)
            /\ WF_vars(RetryHeld) /\ WF_vars(Crash) /\ WF_vars(Boot) /\ WF_vars(Replace)
Spec == Init /\ [][Next]_vars /\ Fairness

TypeOK == /\ s.phase \in {"queued", "held", "idle", "may", "sent", "uncertain", "done", "lost"}
          /\ s.effects \in 0..2 /\ s.restarts \in 0..1
          /\ s.generation \in 1..2 /\ s.target = 1
InputAtMostOnce == s.effects <= 1
NoSilentLoss == s.phase # "lost"
CompletedExactlyOnce == s.phase = "done" => s.effects = 1 /\ s.proof
NoForeignTarget == ~s.wrongTarget
NoUnrelatedEarlyDiagnostic == Entry = "early" => ~s.diagnostic
VerifiedClearNeverBusy == ClearState = "verified" => s.busyEffects = 0
UnverifiedClearNoDelegation == ClearState = "unverified" => s.route # "idle"
BusyResolvedWithoutTurnEnd == <> (s.effects = 1 \/ (s.phase \in {"held", "uncertain"} /\ s.notice))
\* A visible hold cannot hide this defect when busy readiness is already proven.
HealthyBusyEventuallyInjected == <> (s.effects = 1)
ClearedHoldEventuallyInjected == <> (s.effects = 1)
ReceiptEventuallySettled == (s.effects = 1) ~> (s.phase = "done")
ScenarioReached == IF ReplacePane THEN <> s.replaced ELSE <> (s.restarts = 1)
VerifiedClearEventuallyDelegates == <> (s.route = "idle")
=============================================================================
