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
        nonceContested |-> FALSE, registeredFromNonce |-> ""] : nonce \in {"old", ""}}

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
    /\ n' = [n EXCEPT !.fresh = TRUE, !.stage = "prepared"]

ClaimNewNonce ==
    /\ n.stage = "prepared" /\ n.rowNonce = ""
    /\ n' = [n EXCEPT !.rowNonce = "new"]

NonceChangesBeforeRegistration ==
    /\ n.stage = "prepared" /\ ~n.nonceContested
    /\ n' = [n EXCEPT !.rowNonce = "old", !.nonceContested = TRUE]

RefuseConflictingRegistration ==
    /\ n.stage = "prepared" /\ n.rowNonce = "old" /\ n.fresh
    /\ n' = [n EXCEPT !.fresh = FALSE, !.kills = @ + 1, !.stage = "refused"]

PromotePrimary(mode) ==
    /\ n.stage = "prepared" /\ n.fresh
    /\ mode = "no-absence" \/ (~n.oldLive /\ n.absence = "absent")
    /\ mode = "no-promotion-nonce" \/ n.rowNonce \in {"", "new"}
    /\ n' = [n EXCEPT !.registeredFromNonce = n.rowNonce, !.primary = IF mode = "no-promotion" THEN @ ELSE "new",
        !.aliases = {"old"}, !.current = "new", !.target = Physical(n.host, "new"),
        !.fresh = FALSE, !.rowNonce = "new", !.stage = "registered"]

LateOldNonceHook(mode) ==
    /\ n.stage \in {"registered", "stopped"} /\ ~n.oldHookSeen
    /\ n' = [n EXCEPT !.oldHookSeen = TRUE, !.staleRejected = mode # "no-nonce-cas",
        !.primary = IF mode = "no-nonce-cas" THEN "old" ELSE @,
        !.rowNonce = IF mode = "no-nonce-cas" THEN "old" ELSE @]

StopCurrent ==
    /\ n.stage = "registered" /\ n.primary = "new" /\ n.rowNonce = "new"
    /\ n' = [n EXCEPT !.kills = @ + 1, !.current = "none", !.target = "none", !.stage = "stopped"]

RetryStop(mode) ==
    /\ n.stage = "stopped" /\ ~n.retriedStop
    /\ n' = [n EXCEPT !.retriedStop = TRUE,
        !.kills = IF mode = "double-kill" THEN @ + 1 ELSE @]

Done == (n.stage = "refused" \/ (n.stage = "stopped" /\ n.retriedStop /\ n.oldHookSeen)) /\ UNCHANGED n

Next(mode) == RequestNewName \/ OldHostEnds \/ ObserveOldHost \/ ProbeFails \/ ProbeAgain
    \/ OldNonceEnds \/ NonceChangesBeforeRegistration \/ RefuseConflictingRegistration \/ PrepareSuccessor(mode) \/ ClaimNewNonce \/ PromotePrimary(mode)
    \/ LateOldNonceHook(mode) \/ StopCurrent \/ RetryStop(mode) \/ Done

PrimaryTailIsCurrentPhysical == n.current # "none" => n.primary = n.target
PrimaryNamesCurrentLogical == n.current # "none" => n.primary = n.current
HostTargetMatchesCurrent == n.target = Physical(n.host, n.current)
LiveOldNotPromoted == n.oldLive => n.primary = "old" /\ n.current = "old" /\ ~n.fresh
OldAliasRetained == n.stage \in {"registered", "stopped"} => "old" \in n.aliases
PrimaryNotAlias == n.primary \notin n.aliases
NoDuplicateKill == n.kills <= 1
PromotionNonceWasCurrent == n.registeredFromNonce \in {"", "new"}
LateOldHookRejected == n.oldHookSeen => n.staleRejected /\ n.rowNonce = "new"
====
