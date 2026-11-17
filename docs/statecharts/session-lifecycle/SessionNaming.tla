---- MODULE SessionNaming ----
EXTENDS Naturals
VARIABLE n

Physical(kind, logical) ==
    IF logical = "none" THEN "none"
    ELSE IF kind = "herdr" THEN IF logical = "old" THEN "old-pane" ELSE "new-pane"
    ELSE logical

Init(kind) ==
    /\ n \in {[host |-> kind, stage |-> "old", oldLive |-> TRUE,
        absence |-> "unknown", primary |-> "old", current |-> "old",
        target |-> Physical(kind, "old"), fresh |-> FALSE, aliases |-> {},
        rowNonce |-> nonce, oldHookSeen |-> FALSE, staleRejected |-> FALSE,
        kills |-> 0, retriedStop |-> FALSE, probeFailures |-> 0,
        nonceContested |-> FALSE, registeredFromNonce |-> "",
        tx |-> "idle", snapshotNonce |-> "", stagedPrimary |-> "old",
        stagedNonce |-> "", stagedAliases |-> {}, hostWitness |-> "same", witnessChanged |-> FALSE,
        slotIncarnation |-> "none", replacementSeen |-> FALSE,
        restarts |-> 0, crashDurable |-> <<"old", nonce, {}>>] : nonce \in {"old", ""}}

RequestNewName ==
    /\ n.stage = "old"
    /\ n' = [n EXCEPT !.stage = "requested"]

OldHostEnds ==
    /\ n.stage = "requested" /\ n.oldLive
    /\ n' = [n EXCEPT !.oldLive = FALSE, !.current = "none", !.target = "none"]

ObserveOldHost ==
    /\ n.stage = "requested" /\ n.absence = "unknown"
    /\ n' = [n EXCEPT !.absence = IF n.oldLive THEN "present" ELSE "absent"]

ProbeFails ==
    /\ n.stage = "requested" /\ n.absence = "unknown" /\ n.probeFailures = 0
    /\ n' = [n EXCEPT !.absence = "failed", !.probeFailures = 1]

ProbeAgain ==
    /\ n.stage = "requested" /\ n.absence \in {"failed", "present"} /\ ~n.oldLive
    /\ n' = [n EXCEPT !.absence = "unknown"]

OldNonceEnds ==
    /\ n.stage = "requested" /\ ~n.oldLive /\ n.rowNonce = "old"
    /\ n' = [n EXCEPT !.rowNonce = ""]

PrepareSuccessor(mode) ==
    /\ n.stage = "requested" /\ n.rowNonce \in {"", "new"}
    /\ mode = "no-absence" \/ (~n.oldLive /\ n.absence = "absent")
    /\ n' = [n EXCEPT !.fresh = TRUE, !.stage = "prepared", !.slotIncarnation = "a"]

ClaimNewNonce ==
    /\ n.stage = "prepared" /\ n.tx = "rows" /\ n.stagedNonce = ""
    /\ n' = [n EXCEPT !.stagedNonce = "new"]

NonceChangesBeforeRegistration ==
    /\ n.stage = "prepared" /\ ~n.nonceContested
    /\ n' = [n EXCEPT !.rowNonce = "old", !.nonceContested = TRUE]

RefuseConflictingRegistration ==
    /\ n.stage = "prepared" /\ n.tx = "idle" /\ n.rowNonce = "old" /\ n.fresh
    /\ n' = [n EXCEPT !.fresh = FALSE, !.stage = "held"]

TxDiscover ==
    /\ n.stage = "prepared" /\ n.fresh /\ n.tx = "idle"
    /\ n' = [n EXCEPT !.tx = "snapshot", !.snapshotNonce = n.rowNonce]

SameNameIncarnationReplacesPrepared ==
    /\ n.stage = "prepared" /\ n.tx = "snapshot" /\ ~n.replacementSeen
    /\ n.slotIncarnation = "a"
    /\ n' = [n EXCEPT !.slotIncarnation = "b", !.replacementSeen = TRUE,
        !.rowNonce = "old", !.nonceContested = TRUE]

TxLocatorLocks ==
    /\ n.tx = "snapshot"
    /\ n' = [n EXCEPT !.tx = "locators"]

TxIdentityLock ==
    /\ n.tx = "locators"
    /\ n' = [n EXCEPT !.tx = "identity"]

TxRows ==
    /\ n.tx = "identity"
    /\ n' = [n EXCEPT !.tx = "rows"]

TxHostChanges ==
    /\ n.stage = "prepared" /\ n.tx # "idle" /\ ~n.witnessChanged
    /\ \E witness \in {"live", "failed", "unknown", "different-server", "pending"} :
        n' = [n EXCEPT !.hostWitness = witness, !.witnessChanged = TRUE]

TxValidHost(mode) == mode = "no-absence" \/ (~n.oldLive /\ n.absence = "absent" /\ n.hostWitness = "same")
TxValidNonce(mode) == mode = "no-promotion-nonce" \/ (n.rowNonce \in {"", "new"} /\ n.rowNonce = n.snapshotNonce)

PromotePrimary(mode) ==
    /\ n.stage = "prepared" /\ n.fresh /\ n.tx = "rows"
    /\ TxValidHost(mode) /\ TxValidNonce(mode)
    /\ n' = [n EXCEPT !.tx = "updated", !.stagedPrimary = IF mode = "no-promotion" THEN "old" ELSE "new", !.stagedNonce = "new"]

TxPreserveAlias ==
    /\ n.tx = "updated"
    /\ n' = [n EXCEPT !.tx = "aliased", !.stagedAliases = {"old"}]

TxCommit(mode) ==
    /\ n.stage = "prepared" /\ n.tx = "aliased"
    /\ TxValidHost(mode) /\ TxValidNonce(mode)
    /\ n' = [n EXCEPT !.registeredFromNonce = n.rowNonce, !.primary = n.stagedPrimary,
        !.aliases = n.stagedAliases, !.current = "new", !.target = Physical(n.host, "new"),
        !.fresh = FALSE, !.rowNonce = n.stagedNonce, !.stage = "registered", !.tx = "committed"]

TxRollback(mode) ==
    /\ n.stage = "prepared" /\ n.tx # "idle"
    /\ ~TxValidHost(mode) \/ ~TxValidNonce(mode)
    /\ n' = [n EXCEPT !.tx = "rolled-back", !.stage = "held", !.fresh = FALSE,
         !.stagedPrimary = n.primary, !.stagedAliases = n.aliases, !.stagedNonce = n.rowNonce]

DaemonCrashes ==
    /\ n.stage \in {"prepared", "registered"} /\ n.restarts = 0
    /\ n.tx \in {"snapshot", "locators", "identity", "rows", "updated", "aliased", "committed"}
    /\ n' = [n EXCEPT !.stage = "restarting", !.tx = "crashed", !.fresh = FALSE,
        !.restarts = 1, !.crashDurable = <<n.primary, n.rowNonce, n.aliases>>,
        !.stagedPrimary = n.primary, !.stagedNonce = n.rowNonce, !.stagedAliases = n.aliases]

RestartReadsDurableRow ==
    /\ n.stage = "restarting"
    /\ n' = [n EXCEPT !.stage = IF n.primary = "new" THEN "registered" ELSE "held",
        !.tx = IF n.primary = "new" THEN "committed" ELSE "rolled-back"]

LateOldNonceHook(mode) ==
    /\ n.stage \in {"registered", "stopped"} /\ ~n.oldHookSeen
    /\ n' = [n EXCEPT !.oldHookSeen = TRUE, !.staleRejected = mode # "no-nonce-cas",
        !.primary = IF mode = "no-nonce-cas" THEN "old" ELSE @,
        !.rowNonce = IF mode = "no-nonce-cas" THEN "old" ELSE @]

StopCurrent ==
    /\ n.stage = "registered" /\ n.primary = "new" /\ n.rowNonce = "new"
    /\ n' = [n EXCEPT !.kills = @ + 1, !.current = "none", !.target = "none", !.stage = "stopped", !.slotIncarnation = "none"]

RetryStop(mode) ==
    /\ n.stage = "stopped" /\ ~n.retriedStop
    /\ n' = [n EXCEPT !.retriedStop = TRUE,
        !.kills = IF mode = "double-kill" THEN @ + 1 ELSE @]

Done == (n.stage = "held" \/ (n.stage = "stopped" /\ n.retriedStop /\ n.oldHookSeen)) /\ UNCHANGED n

Next(mode) == RequestNewName \/ OldHostEnds \/ ObserveOldHost \/ ProbeFails \/ ProbeAgain
    \/ OldNonceEnds \/ NonceChangesBeforeRegistration \/ RefuseConflictingRegistration \/ PrepareSuccessor(mode) \/ ClaimNewNonce \/ PromotePrimary(mode)
    \/ TxDiscover \/ SameNameIncarnationReplacesPrepared \/ TxLocatorLocks \/ TxIdentityLock \/ TxRows \/ TxHostChanges
    \/ TxPreserveAlias \/ TxCommit(mode) \/ TxRollback(mode)
    \/ LateOldNonceHook(mode) \/ StopCurrent \/ RetryStop(mode) \/ Done

NextWithRestart == Next("fixed") \/ DaemonCrashes \/ RestartReadsDurableRow

PrimaryTailIsCurrentPhysical == n.current # "none" => n.primary = n.target
PrimaryNamesCurrentLogical == n.current # "none" => n.primary = n.current
HostTargetMatchesCurrent == n.target = Physical(n.host, n.current)
LiveOldNotPromoted == n.oldLive => n.primary = "old" /\ n.current = "old" /\ ~n.fresh
OldAliasRetained == n.stage \in {"registered", "stopped"} => "old" \in n.aliases
PrimaryNotAlias == n.primary \notin n.aliases
NoDuplicateKill == n.kills <= 1
PromotionNonceWasCurrent == n.registeredFromNonce \in {"", "new"}
LateOldHookRejected == n.oldHookSeen => n.staleRejected /\ n.rowNonce = "new"
RollbackHasNoPartialPromotion == n.tx = "rolled-back" => n.primary = "old" /\ n.aliases = {}
RegistrationRefusalDoesNotKill == n.stage = "held" => n.kills = 0
SameNameReplacementPreserved == n.replacementSeen => n.slotIncarnation = "b"
RestartKeepsDurableIdentity == n.restarts = 1 => <<n.primary, n.rowNonce, n.aliases>> = n.crashDurable
CanonicalPrimaryEventuallyCurrent == (n.stage = "prepared") ~> (n.stage = "held" \/ (n.primary = "new" /\ n.rowNonce = "new" /\ "old" \in n.aliases))
====
