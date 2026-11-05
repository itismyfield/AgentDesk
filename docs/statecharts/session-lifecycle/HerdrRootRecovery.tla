---- MODULE HerdrRootRecovery ----
\* RootReplaced policy production, based on d508839828. HerdrSession's RowEnd remains unchanged.
EXTENDS Naturals, Integers
CONSTANTS ReplaceServer, PrepareEnabled, OwnerInstalled, OldGrant, InputAfterFirstPass,
          PeerVerified, ConfigExplicitFalse, ConfigBeforeStart, SameDirectory, CacheReadable,
          LocalCurrent, OriginalGone, ReplacementProviderLive, PreclaimEnabled, WakeEnabled,
          HoldBeforeRecovery, InitialRowState

VARIABLES phase, generation, witness, paneRoot, oldLive, replacementLive,
          row, startupWitness, nextRestartReady, target, recovery, q, creates,
          trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes
vars == <<phase, generation, witness, paneRoot, oldLive, replacementLive,
          row, startupWitness, nextRestartReady, target, recovery, q, creates,
          trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
NoWitness == -1
EmptyTarget == [witness |-> NoWitness, startup |-> NoWitness, grant |-> NoWitness]
StartupPossible == PeerVerified /\ ConfigExplicitFalse /\ ConfigBeforeStart /\ SameDirectory /\ CacheReadable
InitialGrant == IF OldGrant THEN 0 ELSE NoWitness
PolicyAdmitted == CacheReadable /\ target.witness = witness
                  /\ (target.startup = witness \/ target.grant = witness)
Quiet == ~q.inflight
ReplacementEvidence == recovery.candidate \/ (OldGrant /\ row.grant # witness)

Init ==
    /\ phase = "running" /\ generation = 0 /\ witness = 0 /\ paneRoot = 0
    /\ oldLive = TRUE /\ replacementLive = FALSE
    /\ row = [state |-> InitialRowState, nonce |-> 1, root |-> 0, grant |-> InitialGrant,
              freshGrant |-> NoWitness, createdFromPending |-> FALSE]
    /\ startupWitness = NoWitness /\ nextRestartReady = FALSE /\ target = EmptyTarget
    /\ recovery = [stage |-> IF InitialRowState = "retired" THEN "settled" ELSE "idle",
                    candidate |-> FALSE, firstPass |-> FALSE, seenIncomplete |-> FALSE,
                    completionPending |-> FALSE, notified |-> FALSE,
                    proof |-> NoWitness, reason |-> "none"]
    /\ q = [state |-> "absent", custody |-> FALSE, inflight |-> FALSE,
             inflightGeneration |-> 0, effects |-> 0, execution |-> 0, heldOnce |-> FALSE]
    /\ creates = 0 /\ trustedWitness = NoWitness /\ trustedEvidence = NoWitness
    /\ retiredUnsafe = FALSE /\ preclaimReads = 0 /\ wakes = 0

\* A dcserver restart loses process evidence, not the durable row or exact nonce grant.
DcRestart ==
    /\ phase = "running"
    /\ phase' = "down" /\ generation' = generation + 1
    /\ startupWitness' = NoWitness /\ nextRestartReady' = FALSE /\ target' = EmptyTarget
    /\ UNCHANGED <<witness, paneRoot, oldLive, replacementLive, row, recovery, q,
                    creates, trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
HerdrReplaces ==
    /\ phase = "down" /\ ReplaceServer
    /\ phase' = "new-server" /\ witness' = 1
    /\ UNCHANGED <<generation, paneRoot, oldLive, replacementLive, row, startupWitness,
                    nextRestartReady, target, recovery, q, creates, trustedWitness,
                    trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
PaneRootReplaces ==
    /\ phase = "new-server"
    /\ phase' = "herdr-ready" /\ paneRoot' = 1
    /\ oldLive' = ~OriginalGone /\ replacementLive' = ReplacementProviderLive
    /\ UNCHANGED <<generation, witness, row, startupWitness, nextRestartReady, target,
                    recovery, q, creates, trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
HerdrSurvives ==
    /\ phase = "down" /\ ~ReplaceServer /\ phase' = "herdr-ready"
    /\ UNCHANGED <<generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, recovery, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
Reconcile ==
    /\ phase = "herdr-ready" /\ phase' = "reconciled"
    /\ UNCHANGED <<generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, recovery, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>

\* herdr_reader.rs:304-327 preserves the exact RootReplaced candidate across passes.
ObserveExecution ==
    /\ phase = "reconciled" /\ row.state = "bound" /\ recovery.stage = "idle"
    /\ (InputAfterFirstPass \/ q.state \in {"inflight", "held", "recovering", "done"})
    /\ recovery' = [recovery EXCEPT !.candidate = paneRoot # row.root,
                        !.stage = IF paneRoot # row.root THEN "prepare" ELSE "settled",
                        !.firstPass = paneRoot = row.root,
                        !.seenIncomplete = paneRoot # row.root]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
UseExactGrant ==
    /\ recovery.stage = "prepare" /\ row.grant = witness
    /\ recovery' = [recovery EXCEPT !.stage = "capture"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
\* S1 calls the existing producer only after local/current/Bound guards. No grant is rewritten.
PreparePolicy ==
    /\ recovery.stage = "prepare" /\ row.grant # witness
    /\ PrepareEnabled /\ LocalCurrent /\ row.state = "bound"
    /\ startupWitness' = IF StartupPossible THEN witness ELSE NoWitness
    /\ nextRestartReady' = PeerVerified /\ ConfigExplicitFalse /\ CacheReadable
    /\ recovery' = [recovery EXCEPT !.stage = "capture"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    target, q, creates, trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
\* Existing server-replaced recovery is bypassed for installed owners (kickoff.rs:120-124).
LegacyPreparePolicy ==
    /\ recovery.stage = "prepare" /\ row.grant # witness
    /\ ~OwnerInstalled /\ ~PrepareEnabled /\ LocalCurrent
    /\ startupWitness' = IF StartupPossible THEN witness ELSE NoWitness
    /\ nextRestartReady' = PeerVerified /\ ConfigExplicitFalse /\ CacheReadable
    /\ recovery' = [recovery EXCEPT !.stage = "capture"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    target, q, creates, trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
SkipPreparation ==
    /\ recovery.stage = "prepare" /\ row.grant # witness
    /\ ((~PrepareEnabled /\ OwnerInstalled) \/ ~LocalCurrent)
    /\ recovery' = [recovery EXCEPT !.stage = "capture"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
\* Evidence is copied into a new target; later cache changes cannot repair an old target.
CaptureTarget ==
    /\ recovery.stage = "capture"
    /\ target' = [witness |-> witness, startup |-> startupWitness, grant |-> row.grant]
    /\ recovery' = [recovery EXCEPT !.stage = "prove"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
\* herdr_gate/root_replaced.rs:23-56 checks admission, lone shell, originals and final witness.
StrictEndProof ==
    /\ recovery.stage = "prove"
    /\ LET proved == LocalCurrent /\ recovery.candidate /\ PolicyAdmitted
                       /\ paneRoot # row.root /\ ~oldLive /\ ~replacementLive
       IN /\ recovery' = [recovery EXCEPT !.stage = IF proved THEN "retire" ELSE "held",
                             !.proof = IF proved THEN witness ELSE NoWitness,
                             !.reason = IF proved THEN "none" ELSE IF ~PolicyAdmitted
                                        THEN "PolicyUnverified" ELSE "end-unverified",
                             !.firstPass = ~proved]
          /\ trustedWitness' = IF proved THEN witness ELSE trustedWitness
          /\ trustedEvidence' = IF proved THEN witness ELSE trustedEvidence
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, q, creates, retiredUnsafe, preclaimReads, wakes>>
RetireReplacedBound ==
    /\ recovery.stage = "retire" /\ LocalCurrent /\ row.state = "bound"
    /\ recovery.proof = witness /\ PolicyAdmitted /\ ~oldLive /\ ~replacementLive /\ Quiet
    /\ row' = [row EXCEPT !.state = "retired"]
    /\ recovery' = [recovery EXCEPT !.stage = "cleanup"]
    /\ retiredUnsafe' = oldLive \/ replacementLive \/ ~Quiet
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive,
                    startupWitness, nextRestartReady, target, q, creates,
                    trustedWitness, trustedEvidence, preclaimReads, wakes>>
QuietHeld ==
    /\ recovery.stage = "retire" /\ ~Quiet
    /\ recovery' = [recovery EXCEPT !.stage = "held", !.firstPass = TRUE, !.reason = "inflight"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
ReleaseOldHold ==
    /\ recovery.stage = "cleanup"
    /\ recovery' = [recovery EXCEPT !.stage = "settled", !.firstPass = TRUE,
                        !.completionPending = recovery.seenIncomplete /\ q.state # "recovering"]
    /\ q' = IF q.state = "recovering" THEN [q EXCEPT !.state = "queued"] ELSE q
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
RetryReader ==
    /\ recovery.stage = "held"
    /\ recovery' = [recovery EXCEPT !.stage = "idle"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>

AcceptInput ==
    /\ phase = "reconciled" /\ q.state = "absent"
    /\ (~InputAfterFirstPass \/ recovery.firstPass \/ InitialRowState # "bound" \/ ~LocalCurrent)
    /\ q' = [q EXCEPT !.state = "queued", !.custody = TRUE]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, recovery, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
\* Spending the front grant is not an active turn and does not create inflight.
ClaimOldBinding ==
    /\ q.state = "queued" /\ row.state = "bound" /\ paneRoot # row.root
    /\ (~PreclaimEnabled \/ ~LocalCurrent \/ ~ReplacementEvidence)
    /\ q' = [q EXCEPT !.state = "claimed"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, recovery, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
\* The subsequent old-binding start, rather than the claim right, persists current inflight.
BeginOldBindingStart ==
    /\ q.state = "claimed" /\ row.state = "bound" /\ paneRoot # row.root
    /\ q' = [q EXCEPT !.state = "inflight", !.inflight = TRUE,
                         !.inflightGeneration = generation]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, recovery, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
RejectOldBinding ==
    /\ q.state = "inflight" /\ row.state = "bound" /\ paneRoot # row.root
    /\ q' = [q EXCEPT !.state = "held"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, recovery, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
\* Restart settlement cannot erase inflight made by this generation's failed old-binding start.
SettleRestartedInflight ==
    /\ q.inflight /\ q.inflightGeneration < generation
    /\ q' = [q EXCEPT !.inflight = FALSE, !.state = "queued"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, recovery, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
SendSameRoot ==
    /\ q.state = "queued" /\ row.state = "bound" /\ paneRoot = row.root /\ oldLive
    /\ row.grant = witness
    /\ q' = [q EXCEPT !.state = "done", !.custody = FALSE, !.effects = @ + 1, !.execution = 1]
    /\ trustedWitness' = witness /\ trustedEvidence' = row.grant
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, recovery, creates, retiredUnsafe, preclaimReads, wakes>>
StartLaunch ==
    /\ q.state \in {"queued", "claimed"} /\ row.state = "retired" /\ recovery.stage = "settled"
    /\ nextRestartReady
    /\ row' = [row EXCEPT !.state = "pending", !.nonce = 2]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive,
                    startupWitness, nextRestartReady, target, recovery, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
CreateOk ==
    /\ row.state = "pending" /\ row.nonce = 2 /\ nextRestartReady /\ CacheReadable
    /\ row' = [row EXCEPT !.state = "placed", !.root = 2, !.freshGrant = witness,
                               !.createdFromPending = row.state = "pending"] /\ paneRoot' = 2
    /\ creates' = creates + 1
    /\ UNCHANGED <<phase, generation, witness, oldLive, replacementLive,
                    startupWitness, nextRestartReady, target, recovery, q,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
RecordEvidenceAndBind ==
    /\ row.state = "placed" /\ row.nonce = 2
    /\ row' = [row EXCEPT !.state = "fresh-bound"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive,
                    startupWitness, nextRestartReady, target, recovery, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
SendFresh ==
    /\ row.state = "fresh-bound" /\ q.state \in {"queued", "claimed"} /\ row.freshGrant = witness
    /\ q' = [q EXCEPT !.state = "done", !.custody = FALSE, !.effects = @ + 1, !.execution = 2]
    /\ trustedWitness' = witness /\ trustedEvidence' = row.freshGrant
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, recovery, creates, retiredUnsafe, preclaimReads, wakes>>
\* Existing evidence gates the fresh preclaim read; awaiting recovery creates no inflight.
PreclaimRecover ==
    /\ PreclaimEnabled /\ phase = "reconciled" /\ q.state = "queued"
    /\ row.state = "bound" /\ LocalCurrent /\ ReplacementEvidence
    /\ paneRoot # row.root /\ recovery.stage \in {"idle", "held", "prepare"}
    /\ (~HoldBeforeRecovery \/ q.heldOnce)
    /\ recovery' = [recovery EXCEPT !.candidate = TRUE, !.stage = "prepare",
                        !.seenIncomplete = TRUE]
    /\ q' = [q EXCEPT !.state = "recovering"] /\ preclaimReads' = preclaimReads + 1
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, wakes>>
\* A temporary guard refuses before claim; the original remains owned without inflight.
HoldBeforeClaim ==
    /\ PreclaimEnabled /\ HoldBeforeRecovery /\ q.state = "queued" /\ ~q.heldOnce
    /\ row.state = "bound" /\ LocalCurrent /\ ReplacementEvidence
    /\ q' = [q EXCEPT !.state = "held", !.heldOnce = TRUE]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, recovery, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
\* Completion only notifies an incomplete row seen by this process, once, after publication.
WakeOriginalInput ==
    /\ WakeEnabled /\ recovery.stage = "settled" /\ recovery.completionPending
    /\ ~recovery.notified
    /\ recovery' = [recovery EXCEPT !.notified = TRUE]
    /\ q' = IF q.state = "held" THEN [q EXCEPT !.state = "queued"] ELSE q
    /\ wakes' = wakes + 1
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads>>
\* An already Retired row follows the existing fresh launch, not root recovery.
PrepareFresh ==
    /\ q.state \in {"queued", "claimed"} /\ row.state = "retired" /\ ~nextRestartReady
    /\ PeerVerified /\ ConfigExplicitFalse /\ CacheReadable
    /\ nextRestartReady' = TRUE
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, target, recovery, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe, preclaimReads, wakes>>
\* Pending and nonlocal rows belong to their existing owners outside this root model.
OtherOwner ==
    /\ phase = "reconciled" /\ (InitialRowState = "pending" \/ ~LocalCurrent)
    /\ UNCHANGED vars

AwaitWake ==
    /\ q.state = "held" /\ row.state = "retired" /\ recovery.stage = "settled"
    /\ UNCHANGED vars
Done == q.state = "done" /\ UNCHANGED vars

Next == DcRestart \/ HerdrReplaces \/ PaneRootReplaces \/ HerdrSurvives \/ Reconcile \/ ObserveExecution
        \/ UseExactGrant \/ PreparePolicy \/ LegacyPreparePolicy \/ SkipPreparation
        \/ CaptureTarget \/ StrictEndProof \/ RetireReplacedBound \/ QuietHeld
        \/ ReleaseOldHold \/ RetryReader \/ AcceptInput \/ ClaimOldBinding \/ BeginOldBindingStart \/ RejectOldBinding
        \/ SettleRestartedInflight \/ SendSameRoot \/ StartLaunch \/ CreateOk
        \/ RecordEvidenceAndBind \/ SendFresh \/ PreclaimRecover \/ HoldBeforeClaim
        \/ WakeOriginalInput \/ PrepareFresh \/ OtherOwner \/ AwaitWake \/ Done

Spec == Init /\ [][Next]_vars
        /\ WF_vars(DcRestart) /\ WF_vars(HerdrReplaces) /\ WF_vars(PaneRootReplaces)
        /\ WF_vars(HerdrSurvives)
        /\ WF_vars(Reconcile) /\ WF_vars(ObserveExecution) /\ WF_vars(UseExactGrant)
        /\ (IF PrepareEnabled THEN WF_vars(PreparePolicy) ELSE TRUE)
        /\ WF_vars(LegacyPreparePolicy) /\ WF_vars(SkipPreparation) /\ WF_vars(CaptureTarget)
        /\ WF_vars(StrictEndProof) /\ WF_vars(RetireReplacedBound) /\ WF_vars(QuietHeld)
        /\ WF_vars(ReleaseOldHold) /\ WF_vars(RetryReader) /\ WF_vars(AcceptInput)
        /\ WF_vars(ClaimOldBinding) /\ WF_vars(BeginOldBindingStart) /\ WF_vars(RejectOldBinding)
        /\ WF_vars(SettleRestartedInflight) /\ WF_vars(SendSameRoot)
        /\ WF_vars(StartLaunch) /\ WF_vars(CreateOk) /\ WF_vars(RecordEvidenceAndBind)
        /\ WF_vars(SendFresh) /\ WF_vars(PrepareFresh)
        /\ (IF PreclaimEnabled THEN WF_vars(PreclaimRecover) /\ WF_vars(HoldBeforeClaim) ELSE TRUE)
        /\ (IF WakeEnabled THEN WF_vars(WakeOriginalInput) ELSE TRUE)

SameRootNoCreate == ~ReplaceServer => creates = 0
InputAtMostOnce == q.effects <= 1
NoRetireLiveExecution == ~retiredUnsafe
PreserveHeldInput == (q.state # "absent" /\ q.effects = 0) => q.custody
NoUnverifiedServerTrust == trustedWitness = NoWitness \/
                          (trustedWitness = witness /\ trustedEvidence = witness
                           /\ (startupWitness = witness \/ row.grant = witness \/ row.freshGrant = witness))
InputUsesCurrentExecution == q.effects = 1 => q.execution = IF ReplaceServer THEN 2 ELSE 1
PendingBeforeCreate == creates > 0 => (row.nonce = 2 /\ row.createdFromPending)
OldGrantUnchanged == row.grant = InitialGrant
StableRecoverableReplacement == InitialRowState = "bound" /\ ReplaceServer /\ LocalCurrent /\ OriginalGone
                               /\ ~ReplacementProviderLive /\ StartupPossible
ReplacedInputProgress ==
    (q.state # "absent" /\ StableRecoverableReplacement) ~> (q.effects = 1)
SameRootInputProgress == (~ReplaceServer /\ InitialRowState = "bound" /\ LocalCurrent
                          /\ q.state # "absent") ~> (q.effects = 1)
NonApplicableNoPreclaimRead == (InitialRowState # "bound" \/ ~LocalCurrent) => preclaimReads = 0
HealthyNoPreclaimRead == (~ReplaceServer /\ ~recovery.candidate) => preclaimReads = 0
CompletionWakeAtMostOnce == wakes <= 1
OldRetiredNoWake == InitialRowState = "retired" => wakes = 0
RetiredInputProgress == (InitialRowState = "retired" /\ LocalCurrent /\ q.state # "absent")
                       ~> (q.effects = 1)
====
