-------------------------- MODULE ReplayEvidence --------------------------
EXTENDS Naturals, FiniteSets
CONSTANTS Mode, Boundary, Fault
ASSUME Mode \in {"live", "recovery"}
ASSUME Boundary \in {"current", "proposed"}
ASSUME Fault \in {"none", "forge", "reuse", "take-twice", "root", "own-process", "restore-proof"}

A == [receipt |-> 1, nonce |-> 1, owner |-> 1]
B == [receipt |-> 1, nonce |-> 2, owner |-> 2]
WrongOwner == [receipt |-> 1, nonce |-> 1, owner |-> 2]
EmptyObs == [claimed |-> FALSE, supported |-> TRUE, closed |-> FALSE,
             eof |-> FALSE, waited |-> FALSE, drained |-> FALSE,
             broken |-> FALSE, active |-> FALSE, terminals |-> 0, done |-> FALSE]
Complete(o) == o.claimed /\ o.supported /\ o.closed /\ o.eof /\ o.waited
               /\ o.drained /\ ~o.broken
CanIssue(o, kind) == Complete(o) /\
    IF kind = "terminal" THEN o.terminals = 1 ELSE ~o.active /\ ~o.done
Names(p, a) == p.receipt = a.receipt /\ p.nonce = a.nonce
EmptyProof == [kind |-> "none", receipt |-> 0, nonce |-> 0,
               sealedOrigin |-> FALSE]
VARIABLE s
vars == <<s>>
Init == s \in [obs : {EmptyObs}, frozen : {EmptyObs}, takes : {0},
    issuerKey : {FALSE}, proof : {EmptyProof}, db : {A}, status : {"started"},
    accepted : {FALSE}, exactAtAccept : {TRUE}, namesAtAccept : {TRUE},
    acceptOrigin : {TRUE}, ack : {FALSE}, retrySpends : {0},
    rootHeld : IF Mode = "recovery" THEN {TRUE, FALSE} ELSE {TRUE},
    sameRoot : IF Mode = "recovery" THEN {TRUE, FALSE} ELSE {TRUE},
    understood : IF Mode = "recovery" THEN {TRUE, FALSE} ELSE {TRUE},
    sameIdentity : IF Mode = "recovery" THEN {TRUE, FALSE} ELSE {TRUE},
    nonceValid : IF Mode = "recovery" THEN {TRUE, FALSE} ELSE {TRUE},
    turnValid : IF Mode = "recovery" THEN {TRUE, FALSE} ELSE {TRUE},
    storedOwner : IF Mode = "recovery" THEN {"old", "current", "empty", "absent"} ELSE {"old"},
    observed : {FALSE}, recoveryKey : {FALSE}, localProof : {FALSE},
    rootRetained : {FALSE}, observedRootOK : {FALSE},
    recoveryAccepted : {FALSE}, recoveryOrigin : {TRUE},
    withheld : IF Mode = "recovery" THEN {TRUE, FALSE} ELSE {FALSE},
    noticeReadable : IF Mode = "recovery" THEN {TRUE, FALSE} ELSE {TRUE},
    heldNonce : IF Mode = "recovery" THEN {"valid", "missing", "empty", "mismatch"} ELSE {"valid"},
    restored : {FALSE},
    bootHandle : {TRUE}, globalRootChanged : {FALSE}]

ClaimReader == /\ Mode = "live" /\ ~s.obs.closed /\ ~s.obs.broken
    /\ s' = [s EXCEPT !.obs = [@ EXCEPT !.claimed = TRUE,
                 !.supported = ~s.obs.claimed,
                 !.broken = s.obs.claimed]]
RecordEof == /\ Mode = "live" /\ s.obs.claimed /\ ~s.obs.closed /\ ~s.obs.eof
    /\ s' = [s EXCEPT !.obs.eof = TRUE]
WaitChild == /\ Mode = "live" /\ s.obs.claimed /\ ~s.obs.closed /\ ~s.obs.waited
    /\ s' = [s EXCEPT !.obs.waited = TRUE]
DrainStderr == /\ Mode = "live" /\ s.obs.claimed /\ ~s.obs.closed /\ ~s.obs.drained
    /\ s' = [s EXCEPT !.obs.drained = TRUE]
Activity == /\ Mode = "live" /\ s.obs.claimed /\ ~s.obs.closed /\ ~s.obs.active
    /\ s' = [s EXCEPT !.obs.active = TRUE]
Terminal(kind) == /\ Mode = "live" /\ s.obs.claimed /\ ~s.obs.closed
    /\ s.obs.terminals < 2
    /\ s' = [s EXCEPT !.obs.terminals = @ + 1,
                 !.obs.done = @ \/ (kind = "done")]
ReaderBreak == /\ Mode = "live" /\ ~s.obs.broken
    /\ s' = [s EXCEPT !.obs.broken = TRUE]
CloseReader == /\ Mode = "live" /\ s.obs.claimed /\ ~s.obs.closed
    /\ s' = [s EXCEPT !.obs.closed = TRUE]
TakeDecision == /\ Mode = "live" /\ s.takes = 0
    /\ s' = [s EXCEPT !.takes = 1, !.frozen = s.obs]
TakeAgain == /\ Mode = "live" /\ Fault = "take-twice" /\ s.takes = 1
    /\ s' = [s EXCEPT !.takes = 2]
PrivateLiveKey == /\ Mode = "live" /\ Boundary = "proposed"
    /\ s.takes = 1 /\ ~s.issuerKey
    /\ s' = [s EXCEPT !.issuerKey = TRUE]
IssueEvidence(kind) == /\ Mode = "live" /\ s.proof.kind = "none"
    /\ s.takes = 1 /\ CanIssue(s.frozen, kind)
    /\ (Boundary = "current" \/ s.issuerKey)
    /\ s' = [s EXCEPT !.proof = [kind |-> kind, receipt |-> A.receipt,
                     nonce |-> A.nonce, sealedOrigin |-> TRUE]]
ForgeEvidence == /\ Mode = "live" /\ Fault = "forge" /\ s.proof.kind = "none"
    /\ s' = [s EXCEPT !.proof = [kind |-> "no-effect", receipt |-> A.receipt,
                     nonce |-> A.nonce, sealedOrigin |-> FALSE]]
SupersedeAttempt == /\ Mode = "live" /\ s.db = A /\ ~s.accepted
    /\ s' = [s EXCEPT !.db = B]
Settle(a) == /\ Mode = "live" /\ s.proof.kind # "none" /\ ~s.accepted
    /\ s.status = "started" /\ s.db = a
    /\ (Fault = "reuse" \/ Names(s.proof, a))
    /\ s' = [s EXCEPT !.accepted = TRUE, !.status = "classified",
             !.exactAtAccept = (s.db = a), !.namesAtAccept = Names(s.proof, a),
             !.acceptOrigin = s.proof.sealedOrigin]
CommitAck == /\ Mode = "live" /\ s.accepted /\ ~s.ack
    /\ s' = [s EXCEPT !.ack = TRUE]
SpendRetry == /\ Mode = "live" /\ s.ack /\ s.proof.kind = "no-effect"
    /\ s.retrySpends = 0
    /\ s' = [s EXCEPT !.retrySpends = 1]

ObserveLocal == /\ Mode = "recovery" /\ ~s.observed
    /\ (Fault = "root" \/ (s.rootHeld /\ s.sameRoot))
    /\ s' = [s EXCEPT !.observed = TRUE, !.observedRootOK = s.rootHeld /\ s.sameRoot,
             !.rootRetained = s.rootHeld]
PrivateRecoveryKey == /\ Mode = "recovery" /\ Boundary = "proposed"
    /\ s.observed /\ ~s.recoveryKey
    /\ s' = [s EXCEPT !.recoveryKey = TRUE]
IssueLocalProof == /\ Mode = "recovery" /\ s.observed /\ ~s.localProof
    /\ (Boundary = "current" \/ s.recoveryKey)
    /\ s' = [s EXCEPT !.localProof = TRUE]
DropBootHandle == /\ Mode = "recovery" /\ s.bootHandle
    /\ s' = [s EXCEPT !.bootHandle = FALSE,
              !.rootHeld = s.rootHeld /\ s.rootRetained]
ChangeGlobalRoot == /\ Mode = "recovery" /\ s.observed /\ ~s.globalRootChanged
    /\ s' = [s EXCEPT !.globalRootChanged = TRUE]
RecoverHold == /\ Mode = "recovery" /\ ~s.withheld /\ s.localProof /\ ~s.recoveryAccepted
    /\ s.understood /\ s.sameIdentity /\ s.nonceValid /\ s.turnValid
    /\ (s.storedOwner = "old" \/ (Fault = "own-process" /\ s.storedOwner = "current"))
    /\ s' = [s EXCEPT !.recoveryAccepted = TRUE, !.withheld = TRUE,
             !.recoveryOrigin = s.observedRootOK /\ s.rootRetained]
RestoreWithheld == /\ Mode = "recovery" /\ s.withheld /\ ~s.restored
    /\ s.noticeReadable /\ s.heldNonce \in {"valid", "missing"}
    /\ (Fault # "restore-proof" \/ s.localProof)
    /\ s' = [s EXCEPT !.restored = TRUE]
LiveEnd == /\ Mode = "live"
    /\ (s.ack \/ s.takes > 1
        \/ (s.takes = 1 /\ ~CanIssue(s.frozen, "terminal")
                         /\ ~CanIssue(s.frozen, "no-effect"))
        \/ (s.proof.kind # "none" /\ ~s.accepted /\ s.db = B /\ Fault # "reuse"))
RecoveryDenied == ~s.understood \/ ~s.sameIdentity \/ ~s.nonceValid \/ ~s.turnValid
    \/ ~(s.storedOwner = "old" \/ (Fault = "own-process" /\ s.storedOwner = "current"))
RecoveryEnd == /\ Mode = "recovery"
    /\ (s.recoveryAccepted
        \/ (~s.withheld /\ ~s.observed /\ Fault # "root" /\ (~s.rootHeld \/ ~s.sameRoot))
        \/ (~s.withheld /\ s.localProof /\ RecoveryDenied)
        \/ (s.withheld /\ (s.restored \/ ~s.noticeReadable
              \/ s.heldNonce \notin {"valid", "missing"}
              \/ (Fault = "restore-proof" /\ ~s.localProof))))
Idle == /\ (LiveEnd \/ RecoveryEnd)
        /\ UNCHANGED vars
Next == ClaimReader \/ RecordEof \/ WaitChild \/ DrainStderr \/ Activity
    \/ Terminal("done") \/ Terminal("error") \/ ReaderBreak \/ CloseReader
    \/ TakeDecision \/ TakeAgain \/ PrivateLiveKey
    \/ IssueEvidence("terminal") \/ IssueEvidence("no-effect") \/ ForgeEvidence
    \/ SupersedeAttempt \/ Settle(A) \/ Settle(B) \/ Settle(WrongOwner)
    \/ CommitAck \/ SpendRetry \/ ObserveLocal \/ PrivateRecoveryKey
    \/ IssueLocalProof \/ DropBootHandle \/ ChangeGlobalRoot \/ RecoverHold \/ RestoreWithheld \/ Idle

PrivateIssueOnly == s.proof.kind # "none" => s.proof.sealedOrigin
CompleteEvidence == s.proof.sealedOrigin => CanIssue(s.frozen, s.proof.kind)
OneDecision == s.takes <= 1
ProofNamesAttempt == s.accepted => s.namesAtAccept
ExactCas == s.accepted => s.exactAtAccept
ClassificationNeedsEvidence == s.accepted => s.acceptOrigin
OneRetryAfterAck == s.retrySpends <= 1 /\ (s.retrySpends > 0 => s.ack)
RecoveryBoundRoot == s.recoveryAccepted => s.recoveryOrigin
RecoveryStaleOnly == s.recoveryAccepted => s.storedOwner = "old"
RecoveryExactIdentity == s.recoveryAccepted =>
    (s.understood /\ s.sameIdentity /\ s.nonceValid /\ s.turnValid)
RestorationNoStart == s.restored => (s.retrySpends = 0 /\ ~s.ack)
RootlessWithheldInit == /\ Init /\ s.withheld /\ ~s.rootHeld /\ ~s.sameRoot
    /\ s.storedOwner = "current" /\ s.noticeReadable /\ s.heldNonce = "missing"
RootlessSpec == RootlessWithheldInit /\ [][RestoreWithheld \/ Idle]_vars /\ WF_vars(RestoreWithheld)
RootlessNoNewAuthority == ~s.localProof /\ ~s.recoveryAccepted /\ s.retrySpends = 0
RestoreEventually == <>s.restored
=============================================================================
