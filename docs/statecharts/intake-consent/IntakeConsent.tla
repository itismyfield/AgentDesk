-------------------------- MODULE IntakeConsent --------------------------
\* Code SHA: 08e2336e47986d70d0cbe95e56215858377aed5c; production unchanged.
\* Boundary selects private issue/current, public receipt/d1, keyed scope/r2.
EXTENDS Naturals
CONSTANTS Boundary, Mutant, MaxClaims
ASSUME Boundary \in {"current", "d1", "r2"}
ASSUME Mutant \in {"none", "forge", "reuse", "stale", "bookkeeping", "owner-only", "open-key"}
ASSUME MaxClaims \in 2..3
Ids == 1..MaxClaims
EmptyRow == [status |-> "pending", payload |-> "original", source |-> "original",
             claim |-> 0, generation |-> 1, marker |-> "none",
             retry |-> 3, completed |-> 7, created |-> 11]
EmptyAuthority == [actor |-> "none", epoch |-> 0, pc |-> "idle", cas |-> FALSE,
                   permit |-> "none", key |-> "none", proof |-> "unissued",
                   origin |-> "none", spends |-> 0, snap |-> EmptyRow,
                   attempted |-> FALSE, observed |-> FALSE, op |-> "none",
                   candidate |-> EmptyRow, changed |-> FALSE, didEffects |-> FALSE,
                   convertDenied |-> FALSE, rollbackDenied |-> FALSE]
VARIABLE s
vars == <<s>>
Init == s = [row |-> EmptyRow, live |-> TRUE, a |-> [i \in Ids |-> EmptyAuthority],
             next |-> 1, epoch |-> 0, ownerLock |-> 0, rowLock |-> 0,
             effects |-> 0, rearms |-> 0, releases |-> 0,
             badStale |-> FALSE, badRollback |-> FALSE, badAfterEffects |-> FALSE,
             drift |-> FALSE, fs |-> TRUE]
ClaimToken(actor) == (IF actor = "worker" THEN 1 ELSE 2) + 2 * s.epoch
AuthorityId(i) == <<s.a[i].epoch, i>>
SameStamp(i) == s.row.claim = s.a[i].snap.claim /\ s.row.generation = s.a[i].snap.generation
Exact(i) == s.row.status = "spawned" /\ SameStamp(i)
            /\ s.row.payload = s.a[i].snap.payload /\ s.row.source = s.a[i].snap.source
\* Fresh authority ids are (epoch, serial); the same process may REUSE its claim token.
Claim(i, actor) ==
    /\ i = s.next /\ i \in Ids /\ s.a[i].pc = "idle"
    /\ s.row.status = "pending" /\ s.live /\ s.ownerLock = 0 /\ s.rowLock = 0
    /\ s' = [s EXCEPT !.row = [@ EXCEPT !.status = "claimed", !.claim = ClaimToken(actor)],
             !.a[i] = [EmptyAuthority EXCEPT !.actor = actor, !.epoch = s.epoch,
                       !.pc = "claimed", !.snap = [s.row EXCEPT !.status = "claimed", !.claim = ClaimToken(actor)]],
             !.next = @ + 1]
AdvanceStatus(i, from, dest) ==
    /\ s.a[i].pc = from /\ s.row.status = from /\ SameStamp(i) /\ s.live
    /\ s.ownerLock = 0 /\ s.rowLock = 0
    /\ s' = [s EXCEPT !.row.status = dest, !.a[i].pc = dest,
             !.a[i].cas = @ \/ (dest = "spawned"),
             !.a[i].permit = (IF Boundary = "d1" /\ dest = "spawned" THEN "held" ELSE "none")]
AbortStale(i) ==
    /\ s.a[i].pc \in {"claimed", "accepted"}
    /\ ~(s.row.status = s.a[i].pc /\ SameStamp(i) /\ s.live)
    /\ s' = [s EXCEPT !.a[i].pc = "done"]
CurrentPrivateIssue(i) ==
    /\ Boundary = "current" /\ s.a[i].actor = "worker" /\ s.a[i].pc = "spawned"
    /\ s.a[i].cas /\ s.a[i].proof = "unissued"
    /\ s' = [s EXCEPT !.a[i].pc = "scope", !.a[i].proof = "slot", !.a[i].origin = "scope"]
R2PrivateScopeKey(i) ==
    /\ Boundary = "r2" /\ s.a[i].actor = "worker" /\ s.a[i].pc = "spawned"
    /\ s.a[i].cas /\ s.a[i].key = "none"
    /\ s' = [s EXCEPT !.a[i].pc = "keyReady", !.a[i].key = "held"]
R2EnterKeyedScope(i) ==
    /\ Boundary = "r2" /\ s.a[i].pc = "keyReady" /\ s.a[i].key = "held"
    /\ s' = [s EXCEPT !.a[i].pc = "scope", !.a[i].key = "spent",
             !.a[i].proof = "slot", !.a[i].origin = "keyed"]
D1RetainReceipt(i) ==
    /\ Boundary = "d1" /\ s.a[i].pc = "spawned" /\ s.a[i].permit = "held"
    /\ s' = [s EXCEPT !.a[i].pc = "stored"]
D1EnterScope(i) ==
    /\ Boundary = "d1" /\ s.a[i].actor = "worker" /\ s.a[i].pc = "stored"
    /\ s.a[i].permit = "held"
    /\ s' = [s EXCEPT !.a[i].pc = "scope", !.a[i].permit = "spent",
             !.a[i].proof = "slot", !.a[i].origin = "scope"]
\* Public API composition can retain the receipt across proofless executor effects.
D1ConvertOutsideScope(i) ==
    /\ Boundary = "d1" /\ s.a[i].pc \in {"stored", "afterEffects"}
    /\ s.a[i].permit = "held" /\ s.a[i].proof = "unissued"
    /\ s' = [s EXCEPT !.a[i].pc = "entry", !.a[i].permit = "spent",
             !.a[i].proof = "held", !.a[i].origin = "public-conversion"]
Take(i, matches) ==
    /\ s.a[i].pc = "scope" /\ s.a[i].proof = "slot"
    /\ s' = [s EXCEPT !.a[i].pc = (IF matches THEN "entry" ELSE "done"),
             !.a[i].proof = (IF matches THEN "held" ELSE "spent"),
             !.a[i].spends = @ + (IF matches THEN 0 ELSE 1)]
\* Actual first entry takes no task-local proof when invoked outside its worker scope.
EnterExecutorWithoutProof(i) ==
    /\ s.a[i].actor = "sibling" /\ s.a[i].proof = "unissued"
    /\ s.a[i].pc = (IF Boundary = "d1" THEN "stored" ELSE "spawned")
    /\ s' = [s EXCEPT !.a[i].pc = "proofless"]
ExecutorEffectsWithoutProof(i) ==
    /\ s.a[i].pc = "proofless" /\ ~s.a[i].didEffects /\ Exact(i) /\ s.live
    /\ s' = [s EXCEPT !.a[i].pc = "afterEffects", !.a[i].didEffects = TRUE, !.effects = @ + 1]
\* Existing direct take has no slot outside scope; r2's issuer key is not returned by CAS.
RejectOutsideConversion(i) ==
    /\ Boundary # "d1" /\ Mutant # "open-key" /\ s.a[i].pc = "afterEffects"
    /\ ~s.a[i].convertDenied
    /\ s' = [s EXCEPT !.a[i].convertDenied = TRUE]
RejectRollbackWithoutProof(i) ==
    /\ s.a[i].pc = "afterEffects" /\ s.a[i].proof # "held"
    /\ (Boundary = "d1" \/ s.a[i].convertDenied)
    /\ s' = [s EXCEPT !.a[i].rollbackDenied = TRUE, !.a[i].pc = "done",
             !.a[i].permit = "none"]
\* Widening the key constructor admits an arbitrary sibling's late synthetic scope.
OpenKeyOutsideScope(i) ==
    /\ Boundary = "r2" /\ Mutant = "open-key" /\ s.a[i].pc = "afterEffects"
    /\ s' = [s EXCEPT !.a[i].key = "held", !.a[i].pc = "lateKey"]
EnterLateScope(i) ==
    /\ Boundary = "r2" /\ Mutant = "open-key" /\ s.a[i].pc = "lateKey"
    /\ s.a[i].key = "held"
    /\ s' = [s EXCEPT !.a[i].key = "spent", !.a[i].pc = "scope",
             !.a[i].proof = "slot", !.a[i].origin = "open-key"]
DropForNormalExecution(i) ==
    /\ s.a[i].pc = "entry" /\ s.a[i].proof = "held"
    /\ s' = [s EXCEPT !.a[i].proof = "spent", !.a[i].spends = @ + 1, !.a[i].pc = "effectReady"]
ExecutorEffectsAfterDrop(i) ==
    /\ s.a[i].pc = "effectReady" /\ ~s.a[i].didEffects /\ Exact(i) /\ s.live
    /\ s' = [s EXCEPT !.a[i].pc = "done", !.a[i].didEffects = TRUE, !.effects = @ + 1]
RecordAttempt(i) ==
    /\ s.a[i].pc = "entry" /\ s.a[i].proof = "held"
    /\ s' = [s EXCEPT !.a[i].pc = "recording", !.a[i].op = "hold", !.a[i].attempted = TRUE]
RecordError(i) ==
    /\ s.a[i].pc = "recording" /\ s.a[i].proof = "held"
    /\ s' = [s EXCEPT !.a[i].pc = "done", !.a[i].proof = "spent", !.a[i].spends = @ + 1]
ObserveAndMove(i) ==
    /\ s.a[i].pc = "recording" /\ s.a[i].proof = "held"
    /\ s' = [s EXCEPT !.a[i].observed = s.fs, !.a[i].proof = "spent", !.a[i].spends = @ + 1,
             !.a[i].pc = (IF s.fs THEN "wantLock" ELSE "done")]
RollbackMove(i) ==
    /\ s.a[i].pc = "entry" /\ s.a[i].proof = "held"
    /\ s' = [s EXCEPT !.a[i].op = "rollback", !.a[i].pc = "wantLock",
             !.a[i].proof = "spent", !.a[i].spends = @ + 1]
LockOwner(i) ==
    /\ s.a[i].pc = "wantLock" /\ s.ownerLock = 0
    /\ s' = [s EXCEPT !.ownerLock = i, !.a[i].pc = "ownerLocked"]
ReadForUpdate(i) ==
    /\ s.a[i].pc = "ownerLocked" /\ s.ownerLock = i /\ s.rowLock = 0
    /\ s' = [s EXCEPT !.rowLock = i, !.a[i].pc = "read"]
Guard(i) ==
    /\ (Mutant = "stale" \/ Exact(i))
    /\ IF s.a[i].op = "rollback" THEN s.row.marker = "none" /\ s.live
       ELSE s.a[i].observed /\ s.row.marker \in {"none", "rearm"}
PrepareCAS(i) ==
    /\ s.a[i].pc = "read" /\ s.ownerLock = i /\ s.rowLock = i
    /\ s' = [s EXCEPT !.a[i].changed = Guard(i), !.a[i].pc = "commit",
             !.a[i].candidate = (IF ~Guard(i) THEN s.row
                ELSE IF s.a[i].op = "hold" THEN [s.row EXCEPT !.marker = "held"]
                ELSE [s.row EXCEPT !.status = "pending", !.claim = 0, !.marker = "rearm",
                      !.retry = (IF Mutant = "bookkeeping" THEN @ + 1 ELSE @)])]
Commit(i) ==
    /\ s.a[i].pc = "commit" /\ s.ownerLock = i /\ s.rowLock = i
    /\ s' = [s EXCEPT
             !.badStale = @ \/ (s.a[i].changed /\ ~(Exact(i) /\ s.live)),
             !.badRollback = @ \/ (s.a[i].changed /\ s.a[i].op = "rollback" /\
                (s.a[i].candidate.retry # s.row.retry \/ s.a[i].candidate.completed # s.row.completed \/
                 s.a[i].candidate.created # s.row.created \/ s.row.marker # "none" \/ s.a[i].attempted)),
             !.badAfterEffects = @ \/ (s.a[i].changed /\ s.a[i].op = "rollback" /\ s.effects > 0),
             !.row = s.a[i].candidate, !.ownerLock = 0, !.rowLock = 0,
             !.rearms = @ + (IF s.a[i].changed /\ s.a[i].op = "rollback" THEN 1 ELSE 0),
             !.a[i].pc = "done"]
Release ==
    /\ s.row.marker = "held" /\ s.row.status = "spawned" /\ s.live
    /\ s.ownerLock = 0 /\ s.rowLock = 0
    /\ \E i \in Ids : Exact(i)
    /\ s' = [s EXCEPT !.row = [@ EXCEPT !.status = "pending", !.claim = 0, !.marker = "released"],
             !.releases = @ + 1]
\* Production owner transfer retires open rows in the same transaction.
Drift(kind) ==
    /\ ~s.drift /\ s.ownerLock = 0 /\ s.rowLock = 0
    /\ s.row.status \in {"claimed", "accepted", "spawned"}
    /\ s' = [s EXCEPT !.row = (CASE kind = "retire" /\ Mutant # "owner-only" -> [@ EXCEPT !.status = "failed"]
                [] kind = "marker" -> [@ EXCEPT !.marker = "foreign"]
                [] kind = "payload" -> [@ EXCEPT !.payload = "changed"]
                [] kind = "source" -> [@ EXCEPT !.source = "changed"]
                [] kind = "claim" -> [@ EXCEPT !.claim = 5]
                [] kind = "generation" -> [@ EXCEPT !.generation = 2]
                [] kind = "forceFail" -> [@ EXCEPT !.status = "failed"]
                [] OTHER -> @),
             !.live = (IF kind = "retire" THEN FALSE ELSE @), !.drift = TRUE]
ChangeConsent == s.fs /\ s' = [s EXCEPT !.fs = FALSE]
\* Drop process-local values; do not reconstruct them from the still-durable row.
Restart ==
    /\ s.epoch = 0
    /\ s' = [s EXCEPT !.epoch = 1, !.ownerLock = 0, !.rowLock = 0,
             !.a = [i \in Ids |-> [s.a[i] EXCEPT
                 !.pc = (IF @ = "idle" THEN "idle" ELSE "done"),
                 !.spends = @ + (IF s.a[i].proof \in {"slot", "held"} THEN 1 ELSE 0),
                 !.proof = (IF @ \in {"slot", "held"} THEN "spent" ELSE @),
                 !.permit = "none", !.key = "none"]]]
ReclaimPreAccept ==
    /\ s.epoch = 1 /\ s.row.status = "claimed" /\ s.live
    /\ s.ownerLock = 0 /\ s.rowLock = 0
    /\ \E i \in Ids : s.row.claim = s.a[i].snap.claim /\ s.a[i].epoch = 0 /\ s.a[i].pc = "done"
    /\ s' = [s EXCEPT !.row = [@ EXCEPT !.status = "pending", !.claim = 0]]
\* A stale sweep leaves the claimed worker alive; cancellation stops that worker.
\* Reclaim may reuse its process token, so an older worker can win the later accept CAS.
RearmLivePreAccept(i, cancelled) ==
    /\ s.row.status = "claimed" /\ s.live /\ s.a[i].epoch = s.epoch
    /\ s.a[i].pc = "claimed" /\ SameStamp(i)
    /\ s.ownerLock = 0 /\ s.rowLock = 0
    /\ s' = [s EXCEPT !.row = [@ EXCEPT !.status = "pending", !.claim = 0],
             !.a[i].pc = (IF cancelled THEN "done" ELSE @)]
SweepPreAccept(i) == RearmLivePreAccept(i, FALSE)
CancelPreAccept(i) == RearmLivePreAccept(i, TRUE)
Forge(i) ==
    /\ Mutant = "forge" /\ i = s.next /\ i \in Ids /\ s.a[i].pc = "idle"
    /\ s' = [s EXCEPT !.a[i].pc = "scope", !.a[i].proof = "slot", !.a[i].origin = "forged",
             !.next = @ + 1]
Reuse(i) ==
    /\ Mutant = "reuse" /\ s.a[i].proof = "spent" /\ s.a[i].spends = 1
    /\ s' = [s EXCEPT !.a[i].spends = 2]
Advance(i) == AdvanceStatus(i, "claimed", "accepted") \/ AdvanceStatus(i, "accepted", "spawned")
    \/ AbortStale(i) \/ CurrentPrivateIssue(i) \/ R2PrivateScopeKey(i) \/ R2EnterKeyedScope(i)
    \/ D1RetainReceipt(i) \/ D1EnterScope(i) \/ D1ConvertOutsideScope(i)
    \/ Take(i, TRUE) \/ Take(i, FALSE) \/ EnterExecutorWithoutProof(i) \/ ExecutorEffectsWithoutProof(i)
    \/ RejectOutsideConversion(i) \/ RejectRollbackWithoutProof(i) \/ OpenKeyOutsideScope(i) \/ EnterLateScope(i)
    \/ DropForNormalExecution(i) \/ ExecutorEffectsAfterDrop(i) \/ RecordAttempt(i) \/ RecordError(i)
    \/ ObserveAndMove(i) \/ RollbackMove(i) \/ LockOwner(i) \/ ReadForUpdate(i) \/ PrepareCAS(i) \/ Commit(i)
Next == (\E i \in Ids : (\E actor \in {"worker", "sibling"} : Claim(i, actor)) \/ Advance(i) \/ Forge(i) \/ Reuse(i))
        \/ Release \/ Restart \/ ReclaimPreAccept \/ ChangeConsent
        \/ (\E i \in Ids : SweepPreAccept(i) \/ CancelPreAccept(i))
        \/ (\E k \in {"payload", "source", "claim", "generation", "forceFail", "retire", "marker"} : Drift(k))
Spec == Init /\ [][Next]_vars /\ (\A i \in Ids : WF_vars(Advance(i))) /\ WF_vars(Release)
TypeOK == s.row.status \in {"pending", "claimed", "accepted", "spawned", "failed"}
    /\ s.ownerLock \in 0..MaxClaims /\ s.rowLock \in 0..MaxClaims /\ s.epoch \in 0..1
    /\ s.next \in 1..(MaxClaims + 1) /\ s.effects \in 0..MaxClaims
NoUnauthorizedIssue == \A i \in Ids : s.a[i].proof # "unissued" =>
    (s.a[i].cas /\ s.a[i].actor = "worker" /\ s.a[i].origin \in {"scope", "keyed"})
ProofConsumedOnce == \A i \in Ids : s.a[i].spends <= 1
    /\ (s.a[i].proof = "spent" => s.a[i].spends = 1)
    /\ (s.a[i].proof \in {"slot", "held"} => s.a[i].spends = 0)
NoStaleProofWrite == ~s.badStale
RollbackExactAndBookkeeping == ~s.badRollback /\ s.rearms <= 1
ReleaseOnce == s.releases <= 1
NoEffectsThenRollback == ~s.badAfterEffects /\ s.effects <= 1
ProofEventuallySettles == \A i \in Ids : (s.a[i].proof \in {"slot", "held"}) ~> (s.a[i].proof = "spent")
TransactionEventuallyEnds == \A i \in Ids : (s.a[i].pc \in {"wantLock", "ownerLocked", "read", "commit"}) ~> (s.a[i].pc = "done")
=============================================================================
