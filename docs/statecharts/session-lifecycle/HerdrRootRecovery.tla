---- MODULE HerdrRootRecovery ----
\* RootReplaced policy production, based on d508839828. HerdrSession's RowEnd remains unchanged.
EXTENDS Naturals, Integers
CONSTANTS ReplaceServer, PrepareEnabled, OwnerInstalled, OldGrant, InputAfterFirstPass,
          PeerVerified, ConfigExplicitFalse, ConfigBeforeStart, SameDirectory, CacheReadable,
          LocalCurrent, OriginalGone, ReplacementProviderLive

VARIABLES phase, generation, witness, paneRoot, oldLive, replacementLive,
          row, startupWitness, nextRestartReady, target, recovery, q, creates,
          trustedWitness, trustedEvidence, retiredUnsafe
vars == <<phase, generation, witness, paneRoot, oldLive, replacementLive,
          row, startupWitness, nextRestartReady, target, recovery, q, creates,
          trustedWitness, trustedEvidence, retiredUnsafe>>
NoWitness == -1
EmptyTarget == [witness |-> NoWitness, startup |-> NoWitness, grant |-> NoWitness]
StartupPossible == PeerVerified /\ ConfigExplicitFalse /\ ConfigBeforeStart /\ SameDirectory /\ CacheReadable
InitialGrant == IF OldGrant THEN 0 ELSE NoWitness
PolicyAdmitted == CacheReadable /\ target.witness = witness
                  /\ (target.startup = witness \/ target.grant = witness)
Quiet == ~q.inflight

Init ==
    /\ phase = "running" /\ generation = 0 /\ witness = 0 /\ paneRoot = 0
    /\ oldLive = TRUE /\ replacementLive = FALSE
    /\ row = [state |-> "bound", nonce |-> 1, root |-> 0, grant |-> InitialGrant,
              freshGrant |-> NoWitness, createdFromPending |-> FALSE]
    /\ startupWitness = NoWitness /\ nextRestartReady = FALSE /\ target = EmptyTarget
    /\ recovery = [stage |-> "idle", candidate |-> FALSE, firstPass |-> FALSE,
                    proof |-> NoWitness, reason |-> "none"]
    /\ q = [state |-> "absent", custody |-> FALSE, inflight |-> FALSE,
             inflightGeneration |-> 0, effects |-> 0, execution |-> 0]
    /\ creates = 0 /\ trustedWitness = NoWitness /\ trustedEvidence = NoWitness
    /\ retiredUnsafe = FALSE

\* A dcserver restart loses process evidence, not the durable row or exact nonce grant.
DcRestart ==
    /\ phase = "running"
    /\ phase' = "down" /\ generation' = generation + 1
    /\ startupWitness' = NoWitness /\ nextRestartReady' = FALSE /\ target' = EmptyTarget
    /\ UNCHANGED <<witness, paneRoot, oldLive, replacementLive, row, recovery, q,
                    creates, trustedWitness, trustedEvidence, retiredUnsafe>>
HerdrReplaces ==
    /\ phase = "down" /\ ReplaceServer
    /\ phase' = "new-server" /\ witness' = 1
    /\ UNCHANGED <<generation, paneRoot, oldLive, replacementLive, row, startupWitness,
                    nextRestartReady, target, recovery, q, creates, trustedWitness,
                    trustedEvidence, retiredUnsafe>>
PaneRootReplaces ==
    /\ phase = "new-server"
    /\ phase' = "herdr-ready" /\ paneRoot' = 1
    /\ oldLive' = ~OriginalGone /\ replacementLive' = ReplacementProviderLive
    /\ UNCHANGED <<generation, witness, row, startupWitness, nextRestartReady, target,
                    recovery, q, creates, trustedWitness, trustedEvidence, retiredUnsafe>>
HerdrSurvives ==
    /\ phase = "down" /\ ~ReplaceServer /\ phase' = "herdr-ready"
    /\ UNCHANGED <<generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, recovery, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe>>
Reconcile ==
    /\ phase = "herdr-ready" /\ phase' = "reconciled"
    /\ UNCHANGED <<generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, recovery, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe>>

\* herdr_reader.rs:304-327 preserves the exact RootReplaced candidate across passes.
ObserveExecution ==
    /\ phase = "reconciled" /\ row.state = "bound" /\ recovery.stage = "idle"
    /\ recovery' = [recovery EXCEPT !.candidate = paneRoot # row.root,
                        !.stage = IF paneRoot # row.root THEN "prepare" ELSE "settled",
                        !.firstPass = paneRoot = row.root]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe>>
UseExactGrant ==
    /\ recovery.stage = "prepare" /\ row.grant = witness
    /\ recovery' = [recovery EXCEPT !.stage = "capture"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe>>
\* S1 calls the existing producer only after local/current/Bound guards. No grant is rewritten.
PreparePolicy ==
    /\ recovery.stage = "prepare" /\ row.grant # witness
    /\ PrepareEnabled /\ LocalCurrent /\ row.state = "bound"
    /\ startupWitness' = IF StartupPossible THEN witness ELSE NoWitness
    /\ nextRestartReady' = PeerVerified /\ ConfigExplicitFalse /\ CacheReadable
    /\ recovery' = [recovery EXCEPT !.stage = "capture"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    target, q, creates, trustedWitness, trustedEvidence, retiredUnsafe>>
\* Existing server-replaced recovery is bypassed for installed owners (kickoff.rs:120-124).
LegacyPreparePolicy ==
    /\ recovery.stage = "prepare" /\ row.grant # witness
    /\ ~OwnerInstalled /\ ~PrepareEnabled /\ LocalCurrent
    /\ startupWitness' = IF StartupPossible THEN witness ELSE NoWitness
    /\ nextRestartReady' = PeerVerified /\ ConfigExplicitFalse /\ CacheReadable
    /\ recovery' = [recovery EXCEPT !.stage = "capture"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    target, q, creates, trustedWitness, trustedEvidence, retiredUnsafe>>
SkipPreparation ==
    /\ recovery.stage = "prepare" /\ row.grant # witness
    /\ ((~PrepareEnabled /\ OwnerInstalled) \/ ~LocalCurrent)
    /\ recovery' = [recovery EXCEPT !.stage = "capture"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe>>
\* Evidence is copied into a new target; later cache changes cannot repair an old target.
CaptureTarget ==
    /\ recovery.stage = "capture"
    /\ target' = [witness |-> witness, startup |-> startupWitness, grant |-> row.grant]
    /\ recovery' = [recovery EXCEPT !.stage = "prove"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe>>
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
                    startupWitness, nextRestartReady, target, q, creates, retiredUnsafe>>
RetireReplacedBound ==
    /\ recovery.stage = "retire" /\ LocalCurrent /\ row.state = "bound"
    /\ recovery.proof = witness /\ PolicyAdmitted /\ ~oldLive /\ ~replacementLive /\ Quiet
    /\ row' = [row EXCEPT !.state = "retired"]
    /\ recovery' = [recovery EXCEPT !.stage = "cleanup"]
    /\ retiredUnsafe' = oldLive \/ replacementLive \/ ~Quiet
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive,
                    startupWitness, nextRestartReady, target, q, creates,
                    trustedWitness, trustedEvidence>>
QuietHeld ==
    /\ recovery.stage = "retire" /\ ~Quiet
    /\ recovery' = [recovery EXCEPT !.stage = "held", !.firstPass = TRUE, !.reason = "inflight"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe>>
ReleaseOldHold ==
    /\ recovery.stage = "cleanup"
    /\ recovery' = [recovery EXCEPT !.stage = "settled", !.firstPass = TRUE]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe>>
RetryReader ==
    /\ recovery.stage = "held"
    /\ recovery' = [recovery EXCEPT !.stage = "idle"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe>>

AcceptInput ==
    /\ phase = "reconciled" /\ q.state = "absent"
    /\ (~InputAfterFirstPass \/ recovery.firstPass)
    /\ q' = [q EXCEPT !.state = "queued", !.custody = TRUE]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, recovery, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe>>
\* The unrepaired early-input path claims first, then the old binding gate refuses the pane.
ClaimOldBinding ==
    /\ q.state = "queued" /\ row.state = "bound" /\ paneRoot # row.root
    /\ q' = [q EXCEPT !.state = "inflight", !.inflight = TRUE,
                         !.inflightGeneration = generation]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, recovery, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe>>
RejectOldBinding ==
    /\ q.state = "inflight" /\ row.state = "bound" /\ paneRoot # row.root
    /\ q' = [q EXCEPT !.state = "held"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, recovery, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe>>
\* Restart settlement cannot erase inflight made by this generation's failed old-binding start.
SettleRestartedInflight ==
    /\ q.inflight /\ q.inflightGeneration < generation
    /\ q' = [q EXCEPT !.inflight = FALSE, !.state = "queued"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, recovery, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe>>
SendSameRoot ==
    /\ q.state = "queued" /\ row.state = "bound" /\ paneRoot = row.root /\ oldLive
    /\ row.grant = witness
    /\ q' = [q EXCEPT !.state = "done", !.custody = FALSE, !.effects = @ + 1, !.execution = 1]
    /\ trustedWitness' = witness /\ trustedEvidence' = row.grant
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, recovery, creates, retiredUnsafe>>
StartLaunch ==
    /\ q.state = "queued" /\ row.state = "retired" /\ recovery.stage = "settled"
    /\ row' = [row EXCEPT !.state = "pending", !.nonce = 2]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive,
                    startupWitness, nextRestartReady, target, recovery, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe>>
CreateOk ==
    /\ row.state = "pending" /\ nextRestartReady /\ CacheReadable
    /\ row' = [row EXCEPT !.state = "placed", !.root = 2, !.freshGrant = witness,
                               !.createdFromPending = row.state = "pending"] /\ paneRoot' = 2
    /\ creates' = creates + 1
    /\ UNCHANGED <<phase, generation, witness, oldLive, replacementLive,
                    startupWitness, nextRestartReady, target, recovery, q,
                    trustedWitness, trustedEvidence, retiredUnsafe>>
RecordEvidenceAndBind ==
    /\ row.state = "placed" /\ row.nonce = 2
    /\ row' = [row EXCEPT !.state = "fresh-bound"]
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive,
                    startupWitness, nextRestartReady, target, recovery, q, creates,
                    trustedWitness, trustedEvidence, retiredUnsafe>>
SendFresh ==
    /\ row.state = "fresh-bound" /\ q.state = "queued" /\ row.freshGrant = witness
    /\ q' = [q EXCEPT !.state = "done", !.custody = FALSE, !.effects = @ + 1, !.execution = 2]
    /\ trustedWitness' = witness /\ trustedEvidence' = row.freshGrant
    /\ UNCHANGED <<phase, generation, witness, paneRoot, oldLive, replacementLive, row,
                    startupWitness, nextRestartReady, target, recovery, creates, retiredUnsafe>>
Done == q.state = "done" /\ UNCHANGED vars

Next == DcRestart \/ HerdrReplaces \/ PaneRootReplaces \/ HerdrSurvives \/ Reconcile \/ ObserveExecution
        \/ UseExactGrant \/ PreparePolicy \/ LegacyPreparePolicy \/ SkipPreparation
        \/ CaptureTarget \/ StrictEndProof \/ RetireReplacedBound \/ QuietHeld
        \/ ReleaseOldHold \/ RetryReader \/ AcceptInput \/ ClaimOldBinding \/ RejectOldBinding
        \/ SettleRestartedInflight \/ SendSameRoot \/ StartLaunch \/ CreateOk
        \/ RecordEvidenceAndBind \/ SendFresh \/ Done

Spec == Init /\ [][Next]_vars
        /\ WF_vars(DcRestart) /\ WF_vars(HerdrReplaces) /\ WF_vars(PaneRootReplaces)
        /\ WF_vars(HerdrSurvives)
        /\ WF_vars(Reconcile) /\ WF_vars(ObserveExecution) /\ WF_vars(UseExactGrant)
        /\ (IF PrepareEnabled THEN WF_vars(PreparePolicy) ELSE TRUE)
        /\ WF_vars(LegacyPreparePolicy) /\ WF_vars(SkipPreparation) /\ WF_vars(CaptureTarget)
        /\ WF_vars(StrictEndProof) /\ WF_vars(RetireReplacedBound) /\ WF_vars(QuietHeld)
        /\ WF_vars(ReleaseOldHold) /\ WF_vars(RetryReader) /\ WF_vars(AcceptInput)
        /\ WF_vars(ClaimOldBinding) /\ WF_vars(RejectOldBinding)
        /\ WF_vars(SettleRestartedInflight) /\ WF_vars(SendSameRoot)
        /\ WF_vars(StartLaunch) /\ WF_vars(CreateOk) /\ WF_vars(RecordEvidenceAndBind)
        /\ WF_vars(SendFresh)

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
StableRecoverableReplacement == ReplaceServer /\ LocalCurrent /\ OriginalGone
                               /\ ~ReplacementProviderLive /\ StartupPossible
ReplacedInputProgress ==
    (q.state # "absent" /\ StableRecoverableReplacement) ~> (q.effects = 1)
SameRootInputProgress == (~ReplaceServer /\ q.state # "absent") ~> (q.effects = 1)
====
