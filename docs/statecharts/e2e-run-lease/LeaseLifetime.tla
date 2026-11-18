------------------------- MODULE LeaseLifetime -------------------------
EXTENDS Naturals, FiniteSets
CONSTANTS Kind, Mut, CrashEnabled
Runs == {"A", "B"}
None == "none"
VARIABLES phase, parentFd, childFd, helperFd, owner, seenFree, aged,
          pending, net, netOwner, ack, twin, badLateSend, foreignCleanup,
          reserved, cursor, seen, advanced, tx
runner == <<reserved, cursor, seen, advanced, tx>>
vars == <<phase, parentFd, childFd, helperFd, owner, seenFree, aged,
          pending, net, netOwner, ack, twin, badLateSend, foreignCleanup, runner>>
\* Runner settings ride on Mut so the earlier configs stay unchanged: "off" and
\* "ignore_off" switch the runner off; the budget configs leave room for one run.
Enabled == Mut \notin {"off", "ignore_off"}
Budget == IF Mut \in {"budget_one", "skip_reserve", "over_budget"} THEN 1 ELSE 2
Spent == Cardinality({r \in Runs: reserved[r]})
Holder(r) == parentFd[r] \/ childFd[r] \/ helperFd[r]
OtherHolders(r) == \E q \in Runs \ {r}: Holder(q)
Active == {r \in Runs: phase[r] \in {"Running", "Prepared", "Draining"}}
Clean == pending = None /\ net \in {"none", "settled"}
ConsumerInitial == [stage |-> "Pending", case |-> "none", view |-> "UNREAD",
    normalized |-> FALSE, heartbeatFresh |-> FALSE, crashed |-> FALSE,
    alert |-> "none", attempts |-> 0, shown |-> FALSE]
TxInitial == [consumer |-> ConsumerInitial, stage |-> "Pending", day |-> 0, deferred |-> FALSE, consumed |-> FALSE,
              locked |-> FALSE, budgetReserved |-> FALSE, providerPaused |-> FALSE,
              started |-> 0, used |-> 0, usage |-> "pending",
              durable |-> FALSE, finished |-> FALSE, result |-> "NONE", applied |-> FALSE, index |-> 0,
              crashed |-> FALSE, summary |-> FALSE, status |-> "NONE",
              outbox |-> "NONE", notifyFailed |-> FALSE]
Init ==
    /\ phase = [r \in Runs |-> "Pending"]
    /\ parentFd = [r \in Runs |-> FALSE]
    /\ childFd = [r \in Runs |-> FALSE]
    /\ helperFd = [r \in Runs |-> FALSE]
    /\ owner = None /\ seenFree = [r \in Runs |-> FALSE] /\ aged = FALSE
    /\ pending = None /\ net = "none" /\ netOwner = None /\ ack = FALSE
    /\ twin = None /\ badLateSend = FALSE /\ foreignCleanup = FALSE
    /\ reserved = [r \in Runs |-> FALSE] /\ cursor = 0
    /\ seen = [r \in Runs |-> 0] /\ advanced = [r \in Runs |-> FALSE]
    /\ tx = TxInitial
ObserveFree(r) ==
    /\ Mut = "non_atomic" /\ phase[r] = "Pending" /\ owner = None /\ ~seenFree[r]
    /\ seenFree' = [seenFree EXCEPT ![r] = TRUE]
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, aged,
                   pending, net, netOwner, ack, twin, badLateSend, foreignCleanup, runner>>
Acquire(r) ==
    /\ phase[r] = "Pending" /\ (Kind = "instance" \/ Enabled \/ Mut = "ignore_off")
    /\ (owner = None \/ (Mut = "ttl_takeover" /\ aged)
                       \/ (Mut = "non_atomic" /\ seenFree[r]))
    /\ owner' = r /\ phase' = [phase EXCEPT ![r] = "Held"]
    /\ parentFd' = [parentFd EXCEPT ![r] = TRUE]
    /\ UNCHANGED <<childFd, helperFd, seenFree, aged, pending, net,
                   netOwner, ack, twin, badLateSend, foreignCleanup, runner>>
ExpireDiagnostic(r) ==
    /\ Mut = "ttl_takeover" /\ Holder(r) /\ ~aged /\ aged' = TRUE
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, seenFree,
                   pending, net, netOwner, ack, twin, badLateSend, foreignCleanup, runner>>
Start(r) ==
    /\ phase[r] = "Held" /\ parentFd[r] /\ owner = r
    /\ (Kind = "instance" \/ reserved[r] \/ Mut = "skip_reserve")
    /\ seen' = [seen EXCEPT ![r] = cursor]
    /\ childFd' = [childFd EXCEPT ![r] = (Mut # "missing_fd")]
    /\ helperFd' = [helperFd EXCEPT ![r] = (Mut = "leak_fd")]
    /\ phase' = [phase EXCEPT ![r] = IF Clean THEN "Running" ELSE "PausedUncertain"]
    /\ twin' = IF Mut = "duplicate_child" /\ Clean THEN r ELSE twin
    /\ UNCHANGED <<parentFd, owner, seenFree, aged, pending, net,
                   netOwner, ack, badLateSend, foreignCleanup, reserved, cursor, advanced, tx>>
Resume(r) ==
    /\ phase[r] = "PausedUncertain" /\ Clean /\ owner = r /\ childFd[r]
    /\ phase' = [phase EXCEPT ![r] = "Running"]
    /\ UNCHANGED <<parentFd, childFd, helperFd, owner, seenFree, aged,
                   pending, net, netOwner, ack, twin, badLateSend, foreignCleanup, runner>>
Prepare(r) ==
    /\ Kind = "run" /\ phase[r] = "Running" /\ Clean
    /\ pending' = IF Mut = "no_pending" THEN None ELSE r
    /\ phase' = [phase EXCEPT ![r] = "Prepared"]
    /\ UNCHANGED <<parentFd, childFd, helperFd, owner, seenFree, aged,
                   net, netOwner, ack, twin, badLateSend, foreignCleanup, runner>>
Send(r) ==
    /\ Kind = "run"
    /\ (phase[r] = "Prepared" \/ (Mut = "send_after_release" /\ phase[r] = "Released" /\ pending = r))
    /\ net \in {"none", "settled"}
    /\ net' = "sent" /\ netOwner' = r /\ ack' = FALSE
    /\ badLateSend' = (badLateSend \/ phase[r] = "Released")
    /\ phase' = [phase EXCEPT ![r] = IF phase[r] = "Released" THEN "Released" ELSE "Draining"]
    /\ UNCHANGED <<parentFd, childFd, helperFd, owner, seenFree, aged,
                   pending, twin, foreignCleanup, runner>>
ServerAccept ==
    /\ net = "sent" /\ net' = "accepted"
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, seenFree, aged,
                   pending, netOwner, ack, twin, badLateSend, foreignCleanup, runner>>
AckWitness ==
    /\ net \in {"sent", "accepted", "settled"} /\ ~ack /\ ack' = TRUE
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, seenFree, aged,
                   pending, net, netOwner, twin, badLateSend, foreignCleanup, runner>>
ProviderSettles ==
    /\ net = "accepted" /\ net' = "settled"
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, seenFree, aged,
                   pending, netOwner, ack, twin, badLateSend, foreignCleanup, runner>>
Reconcile ==
    /\ pending # None /\ net = "settled" /\ ack /\ pending = netOwner
    /\ pending' = None
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, seenFree, aged,
                   net, netOwner, ack, twin, badLateSend, foreignCleanup, runner>>
ClearFromIdleSnapshot ==
    /\ Mut = "clear_on_idle" /\ pending # None /\ net = "sent"
    /\ pending' = None
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, seenFree, aged,
                   net, netOwner, ack, twin, badLateSend, foreignCleanup, runner>>
Finish(r) ==
    /\ ((Kind = "instance" /\ phase[r] = "Running")
        \/ (Kind = "run" /\ phase[r] = "Draining" /\ Clean))
    /\ phase' = [phase EXCEPT ![r] = "Released"]
    /\ childFd' = [childFd EXCEPT ![r] = FALSE]
    /\ owner' = IF owner = r /\ ~parentFd[r] /\ ~helperFd[r] THEN None ELSE owner
    /\ twin' = IF twin = r THEN None ELSE twin
    /\ cursor' = IF Kind = "run" THEN seen[r] + 1 ELSE cursor
    /\ advanced' = IF Kind = "run" THEN [advanced EXCEPT ![r] = TRUE] ELSE advanced
    /\ UNCHANGED <<parentFd, helperFd, seenFree, aged, pending, net,
                   netOwner, ack, badLateSend, foreignCleanup, reserved, seen, tx>>
ParentCrash(r) ==
    /\ CrashEnabled /\ parentFd[r] /\ parentFd' = [parentFd EXCEPT ![r] = FALSE]
    /\ phase' = [phase EXCEPT ![r] = IF phase[r] = "Held" THEN "Released" ELSE @]
    /\ owner' = IF owner = r /\ Mut # "crash_no_reclaim"
                    /\ ((~childFd[r] /\ ~helperFd[r]) \/ Mut = "parent_unlock")
                 THEN None ELSE owner
    /\ UNCHANGED <<childFd, helperFd, seenFree, aged, pending, net,
                   netOwner, ack, twin, badLateSend, foreignCleanup, runner>>
ChildCrash(r) ==
    /\ CrashEnabled /\ phase[r] \in {"Running", "Prepared", "Draining", "PausedUncertain"}
    /\ phase' = [phase EXCEPT ![r] = "Released"]
    /\ childFd' = [childFd EXCEPT ![r] = FALSE]
    /\ owner' = IF owner = r /\ Mut # "crash_no_reclaim" /\ ~parentFd[r] /\ ~helperFd[r]
                 THEN None ELSE owner
    /\ twin' = IF twin = r THEN None ELSE twin
    /\ cursor' = IF Mut = "advance_on_crash" THEN seen[r] + 1 ELSE cursor
    /\ UNCHANGED <<parentFd, helperFd, seenFree, aged, pending, net,
                   netOwner, ack, badLateSend, foreignCleanup, reserved, seen, advanced, tx>>
ReleaseParent(r) ==
    /\ phase[r] = "Released" /\ parentFd[r]
    /\ parentFd' = [parentFd EXCEPT ![r] = FALSE]
    /\ owner' = IF owner = r /\ ~childFd[r] /\ ~helperFd[r] THEN None ELSE owner
    /\ UNCHANGED <<phase, childFd, helperFd, seenFree, aged, pending, net,
                   netOwner, ack, twin, badLateSend, foreignCleanup, runner>>
ForeignCleanup ==
    /\ Mut = "foreign_cleanup" /\ owner # None /\ ~foreignCleanup
    /\ foreignCleanup' = TRUE
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, seenFree, aged,
                   pending, net, netOwner, ack, twin, badLateSend, runner>>
\* The runner reserves budget under the channel lock; with no room it gives the lock back.
Reserve(r) ==
    /\ Kind = "run" /\ phase[r] = "Held" /\ owner = r /\ ~reserved[r]
    /\ (Spent < Budget \/ Mut = "over_budget")
    /\ reserved' = [reserved EXCEPT ![r] = TRUE]
    /\ UNCHANGED <<phase, parentFd, childFd, helperFd, owner, seenFree, aged,
                   pending, net, netOwner, ack, twin, badLateSend, foreignCleanup, cursor, seen, advanced, tx>>
Defer(r) ==
    /\ Kind = "run" /\ phase[r] = "Held" /\ owner = r /\ ~reserved[r]
    /\ Spent >= Budget /\ Mut # "over_budget"
    /\ phase' = [phase EXCEPT ![r] = "Released"]
    /\ UNCHANGED <<parentFd, childFd, helperFd, owner, seenFree, aged,
                   pending, net, netOwner, ack, twin, badLateSend, foreignCleanup, runner>>
Wait ==
    /\ (~Enabled \/ (\A r \in Runs: phase[r] = "Released")
       \/ (\E r \in Runs: phase[r] = "PausedUncertain")
       \/ (owner # None /\ \E r \in Runs: phase[r] = "Pending"))
    /\ UNCHANGED vars
LeaseNext == (\E r \in Runs: ObserveFree(r) \/ Acquire(r) \/ ExpireDiagnostic(r)
         \/ Start(r) \/ Resume(r) \/ Prepare(r) \/ Send(r) \/ Finish(r)
         \/ ParentCrash(r) \/ ChildCrash(r) \/ ReleaseParent(r) \/ Reserve(r) \/ Defer(r))
        \/ ServerAccept \/ AckWitness \/ ProviderSettles \/ Reconcile
        \/ ClearFromIdleSnapshot \/ ForeignCleanup \/ Wait

\* The transaction branch separates durable result, projection and notification
\* writes so a process crash can occur at every file boundary.
TransactionMutants == {"tx-good", "tx-off", "tx-null", "tx-old-off", "tx-old-null",
    "tx-unknown-zero", "tx-between-only", "tx-consume-deferred", "tx-apply-first",
    "tx-no-recover", "tx-result-regress", "tx-status-first", "tx-alert-drop", "tx-double-apply", "tx-orphan-no-pause"}
TransactionMode == Mut \in TransactionMutants
TxEnabled == Mut \notin {"tx-off", "tx-old-off"}
TxBudgetKnown == Mut \notin {"tx-null", "tx-old-null"}
lease == <<phase, parentFd, childFd, helperFd, owner, seenFree, aged,
           pending, net, netOwner, ack, twin, badLateSend, foreignCleanup,
           reserved, cursor, seen, advanced>>
TxAcquire ==
    /\ tx.stage = "Pending" /\ ~tx.consumed
    /\ ((TxEnabled /\ TxBudgetKnown) \/ Mut \in {"tx-old-off", "tx-old-null"})
    /\ tx' = [tx EXCEPT !.stage = "Held", !.locked = TRUE]
    /\ UNCHANGED lease
TxDefer ==
    /\ tx.stage = "Held" /\ tx.day = 0
    /\ tx' = [tx EXCEPT !.stage = "Deferred", !.locked = FALSE,
                 !.deferred = TRUE, !.consumed = (Mut = "tx-consume-deferred")]
    /\ UNCHANGED lease
TxNextDay ==
    /\ tx.stage = "Deferred" /\ ~tx.consumed
    /\ tx' = [tx EXCEPT !.stage = "Pending", !.day = 1]
    /\ UNCHANGED lease
TxReserve ==
    /\ tx.stage = "Held" /\ tx.day = 1 /\ ~tx.budgetReserved
    /\ tx' = [tx EXCEPT !.budgetReserved = TRUE]
    /\ UNCHANGED lease
TxBeginTurn ==
    /\ tx.stage \in {"Held", "Running"} /\ tx.day = 1
    /\ tx.budgetReserved /\ ~tx.providerPaused
    /\ tx.started < 2
    /\ ((tx.used < 1 /\ tx.usage # "unknown")
        \/ (Mut = "tx-between-only" /\ tx.started = 1)
        \/ (Mut = "tx-unknown-zero" /\ tx.usage = "unknown"))
    /\ tx' = [tx EXCEPT !.stage = "Running", !.started = @ + 1,
                 !.used = @ + 1, !.usage = "pending"]
    /\ UNCHANGED lease
TxUsage(value) ==
    /\ tx.stage = "Running" /\ tx.usage = "pending"
    /\ tx' = [tx EXCEPT !.usage = value,
                 !.used = IF value = "unknown" /\ Mut = "tx-unknown-zero" THEN 0 ELSE @]
    /\ UNCHANGED lease
TxApplyFirst ==
    /\ Mut = "tx-apply-first" /\ tx.stage = "Running" /\ tx.usage # "pending"
    /\ tx' = [tx EXCEPT !.stage = "Precomplete", !.index = 1, !.consumed = TRUE]
    /\ UNCHANGED lease
TxComplete(result) ==
    /\ tx.stage \in {"Running", "Precomplete"} /\ tx.usage # "pending"
    /\ (Mut # "tx-apply-first" \/ tx.stage = "Precomplete")
    /\ tx' = [tx EXCEPT !.stage = "Finished", !.durable = TRUE, !.finished = TRUE, !.result = result]
    /\ UNCHANGED lease
TxApply ==
    /\ tx.finished /\ tx.durable /\ tx.stage = "Finished"
    /\ (~tx.applied \/ (Mut = "tx-double-apply" /\ tx.index < 2))
    /\ tx' = [tx EXCEPT !.stage = "Done",
        !.index = IF Mut = "tx-double-apply" THEN @ + 1 ELSE 1,
        !.consumed = TRUE, !.applied = TRUE]
    /\ UNCHANGED lease
TxCrash ==
    /\ CrashEnabled /\ ~tx.crashed
    /\ tx.stage \in {"Held", "Running", "Precomplete", "Finished", "Done"}
    /\ tx' = [tx EXCEPT !.stage = "Crashed", !.locked = FALSE, !.crashed = TRUE]
    /\ UNCHANGED lease
TxRecover ==
    /\ tx.stage = "Crashed"
    /\ tx' = [tx EXCEPT
        !.stage = IF ~tx.budgetReserved THEN "Deferred"
                  ELSE IF tx.durable /\ Mut # "tx-no-recover" THEN "Finished" ELSE "Done",
        !.result = IF ~tx.budgetReserved THEN "NONE"
                   ELSE IF Mut = "tx-result-regress" \/ ~tx.durable THEN "ABANDONED" ELSE @,
        !.consumed = IF ~tx.budgetReserved THEN (Mut = "tx-consume-deferred")
                     ELSE IF tx.durable THEN @ ELSE TRUE,
        !.deferred = IF ~tx.budgetReserved THEN TRUE ELSE @,
        !.providerPaused = IF tx.budgetReserved /\ ~tx.durable THEN (Mut # "tx-orphan-no-pause") ELSE @,
        !.durable = IF tx.budgetReserved THEN TRUE ELSE @]
    /\ UNCHANGED lease
TxStatusFirst ==
    /\ Mut = "tx-status-first" /\ tx.stage = "Running" /\ ~tx.durable
    /\ tx' = [tx EXCEPT !.status = "PASS"]
    /\ UNCHANGED lease
TxPublish ==
    /\ tx.durable /\ ~tx.summary
    /\ tx' = [tx EXCEPT !.summary = TRUE, !.status = tx.result,
                 !.outbox = IF tx.result # "PASS" THEN "PENDING" ELSE "NONE"]
    /\ UNCHANGED lease
TxNotifyFails ==
    /\ tx.outbox = "PENDING" /\ ~tx.notifyFailed
    /\ tx' = [tx EXCEPT !.notifyFailed = TRUE,
                 !.outbox = IF Mut = "tx-alert-drop" THEN "NONE" ELSE @]
    /\ UNCHANGED lease
TxRelease ==
    /\ tx.stage = "Done" /\ tx.locked
    /\ tx' = [tx EXCEPT !.locked = FALSE]
    /\ UNCHANGED lease
TxAdvance == TxAcquire \/ TxDefer \/ TxNextDay \/ TxReserve \/ TxBeginTurn
    \/ (\E value \in {"known", "unknown"}: TxUsage(value)) \/ TxApplyFirst
    \/ (\E result \in {"PASS", "FAIL"}: TxComplete(result)) \/ TxApply \/ TxRecover
    \/ TxPublish \/ TxNotifyFails \/ TxRelease
TxNext == TxAdvance \/ TxCrash \/ TxStatusFirst \/ UNCHANGED vars
TxFairSpec == Init /\ [][TxNext]_vars /\ WF_vars(TxAdvance)
OffOrNullNoWrites == (~TxEnabled \/ ~TxBudgetKnown) => tx = TxInitial
DeferredRequestKept == tx.deferred /\ tx.day = 0 => ~tx.consumed
UnreservedCrashPreservesRequest == tx.crashed /\ ~tx.budgetReserved => ~tx.consumed /\ ~tx.durable
ReservedOrphanPausesProvider == tx.result = "ABANDONED" /\ tx.budgetReserved /\ ~tx.finished => tx.providerPaused
UnknownNeverZero == tx.usage = "unknown" => tx.used > 0
EveryTurnWithinBudget == tx.started <= 1
CursorRequiresDurable == tx.index > 0 => tx.durable /\ tx.finished
ProjectionExactlyOnce == tx.index <= 1 /\ (tx.applied => tx.index = 1 /\ tx.consumed /\ tx.durable /\ tx.finished)
CompletedResultNeverRegresses == tx.finished => tx.result \in {"PASS", "FAIL"}
StatusRequiresDurable == tx.status # "NONE" => tx.durable
FailedNotifyRemainsPending == tx.notifyFailed => tx.outbox = "PENDING"
CompletedProjectionRecovers == tx.finished ~> tx.applied
CompletedPublicationRecovers == tx.durable ~>
    (tx.summary /\ tx.status = tx.result /\ (tx.result = "PASS" \/ tx.outbox = "PENDING"))


\* A consumer observes one immutable fixture snapshot. It never acquires the run
\* lease; pending delivery remains observable even when transport cannot confirm.
ConsumerMutants == {"consumer-good", "consumer-nodata-green", "consumer-off-green",
    "consumer-stale-green", "consumer-old-head-green", "consumer-zero-green",
    "consumer-usage-green", "consumer-driver-schema-green", "consumer-qualifier-green",
    "consumer-usage-crash", "consumer-no-budget-heartbeat", "consumer-hide-pending",
    "consumer-stale-masks-head"}
ConsumerMode == Mut \in ConsumerMutants
ConsumerCases == {"good", "no-data", "off", "stale", "old-head", "old-head-stale", "summary-schema",
    "status-schema", "future-clock", "heartbeat-stale", "host-unavailable", "zero",
    "skip", "fixture", "missing-assertion", "usage-unknown", "usage-malformed",
    "fail-usage-malformed", "driver-schema", "qualifier-unknown", "nullable", "budget-deferred"}
MalformedCases == {"usage-malformed", "fail-usage-malformed"}
TroubleCases == {"skip", "fixture", "missing-assertion", "usage-unknown", "usage-malformed",
    "fail-usage-malformed", "driver-schema", "qualifier-unknown"}
c == tx.consumer
ConsumerExpected(k) == CASE k = "off" -> "DISABLED"
    [] k = "no-data" -> "NO_DATA"
    [] k = "stale" -> "STALE"
    [] k \in {"old-head", "old-head-stale"} -> "STALE_HEAD"
    [] k \in {"summary-schema", "status-schema", "future-clock", "heartbeat-stale", "host-unavailable"} -> "UNKNOWN"
    [] k = "fail-usage-malformed" -> "FAIL"
    [] k \in {"good", "nullable", "budget-deferred"} -> "PASS"
    [] OTHER -> "UNEVALUABLE"
ConsumerDecision ==
    IF c.case = "off" /\ Mut # "consumer-off-green" THEN "DISABLED"
    ELSE IF c.case = "no-data" /\ Mut # "consumer-nodata-green" THEN "NO_DATA"
    ELSE IF c.case \in {"summary-schema", "status-schema", "future-clock", "heartbeat-stale", "host-unavailable"}
        \/ ~c.heartbeatFresh THEN "UNKNOWN"
    \* An expired result of another head keeps the head mismatch; expiry must not mask it.
    ELSE IF c.case = "old-head-stale" THEN (IF Mut = "consumer-stale-masks-head" THEN "STALE" ELSE "STALE_HEAD")
    ELSE IF c.case = "old-head" /\ Mut # "consumer-old-head-green" THEN "STALE_HEAD"
    ELSE IF c.case = "stale" /\ Mut # "consumer-stale-green" THEN "STALE"
    ELSE IF c.case = "fail-usage-malformed" THEN "FAIL"
    ELSE IF (c.case = "zero" /\ Mut # "consumer-zero-green")
        \/ c.case \in {"skip", "fixture", "missing-assertion"}
        \/ (c.case \in {"usage-unknown", "usage-malformed"} /\ Mut # "consumer-usage-green")
        \/ (c.case = "driver-schema" /\ Mut # "consumer-driver-schema-green")
        \/ (c.case = "qualifier-unknown" /\ Mut # "consumer-qualifier-green") THEN "UNEVALUABLE"
    ELSE "PASS"
ConsumerSelect(k) ==
    /\ c.stage = "Pending"
    /\ tx' = [tx EXCEPT !.consumer = [ConsumerInitial EXCEPT !.stage = "Read", !.case = k,
        !.heartbeatFresh = (k \notin {"heartbeat-stale", "budget-deferred"}),
        !.alert = IF k \in TroubleCases THEN "pending" ELSE "none"]]
    /\ UNCHANGED lease
ConsumerNormalize ==
    /\ c.stage = "Read"
    /\ tx' = [tx EXCEPT !.consumer = [c EXCEPT !.stage = "Ready",
        !.normalized = (c.case \notin MalformedCases \/ Mut # "consumer-usage-crash"),
        !.heartbeatFresh = IF c.case = "budget-deferred" /\ Mut # "consumer-no-budget-heartbeat" THEN TRUE ELSE @]]
    /\ UNCHANGED lease
ConsumerJudge ==
    /\ c.stage = "Ready"
    /\ tx' = [tx EXCEPT !.consumer = [c EXCEPT
        !.stage = IF c.normalized THEN "Judged" ELSE "Crashed",
        !.view = IF c.normalized THEN ConsumerDecision ELSE "UNKNOWN",
        !.crashed = ~c.normalized]]
    /\ UNCHANGED lease
ConsumerShow ==
    /\ c.stage \in {"Judged", "Crashed"}
    /\ tx' = [tx EXCEPT !.consumer = [c EXCEPT !.stage = "Done",
        !.shown = (c.alert = "pending" /\ Mut # "consumer-hide-pending")]]
    /\ UNCHANGED lease
ConsumerDispatch(confirmed) ==
    /\ c.alert = "pending" /\ c.attempts = 0
    /\ tx' = [tx EXCEPT !.consumer = [c EXCEPT !.attempts = 1,
        !.alert = IF confirmed THEN "sent" ELSE @]]
    /\ UNCHANGED lease
ConsumerProgress == (\E k \in ConsumerCases: ConsumerSelect(k)) \/ ConsumerNormalize
    \/ ConsumerJudge \/ ConsumerShow \/ (\E confirmed \in BOOLEAN: ConsumerDispatch(confirmed))
ConsumerNext == ConsumerProgress \/ UNCHANGED vars
ConsumerSpec == Init /\ [][ConsumerNext]_vars /\ WF_vars(ConsumerProgress)
ConsumerTerminal == c.stage \in {"Judged", "Crashed", "Done"}
ConsumerStateMapping == ConsumerTerminal => c.view = ConsumerExpected(c.case)
NoGroundlessPass == c.view = "PASS" => c.case \in {"good", "nullable", "budget-deferred"}
NoOldHeadGreen == c.case \in {"old-head", "old-head-stale"} => c.view # "PASS"
MalformedUsageCompletes == c.case \in MalformedCases => ~c.crashed
FunctionalFailRetained == ConsumerTerminal /\ c.case = "fail-usage-malformed" => c.view = "FAIL"
DeferredHeartbeatIsLive == c.case = "budget-deferred" /\ c.stage \in {"Ready", "Judged", "Done"} => c.heartbeatFresh
ConsumerIsPure == tx = [TxInitial EXCEPT !.consumer = c] /\ owner = None /\ cursor = 0
    /\ phase = [r \in Runs |-> "Pending"] /\ reserved = [r \in Runs |-> FALSE]
PendingEventuallySentOrShown == c.alert = "pending" ~> (c.alert = "sent" \/ c.shown)

Next == IF ConsumerMode THEN ConsumerNext ELSE IF TransactionMode THEN TxNext ELSE LeaseNext
SafetySpec == Init /\ [][Next]_vars
Spec == SafetySpec /\ (\A r \in Runs: WF_vars(Acquire(r)))
FairSpec == Spec /\ (\A r \in Runs:
    WF_vars(Start(r)) /\ WF_vars(Resume(r)) /\ WF_vars(Prepare(r)) /\ WF_vars(Send(r))
    /\ WF_vars(Finish(r)) /\ WF_vars(ReleaseParent(r)) /\ WF_vars(Reserve(r)) /\ WF_vars(Defer(r)))
    /\ WF_vars(ServerAccept) /\ WF_vars(AckWitness) /\ WF_vars(ProviderSettles) /\ WF_vars(Reconcile)
NoDualWriter == Cardinality(Active) + (IF twin = None THEN 0 ELSE 1) <= 1
NoDualGateway == NoDualWriter
NoHolderTheft == \A r \in Runs: Holder(r) => owner = r
WriterHasInheritedFd == \A r \in Runs: phase[r] \in {"Running", "Prepared", "Draining"} => childFd[r]
UncertainSendJournaled == net \in {"sent", "accepted"} => pending = netOwner
NoNewWriterDuringUncertainty == \A r \in Active: pending # None => pending = r
NoSendAfterRelease == ~badLateSend
NoForeignCleanup == ~foreignCleanup
\* The kernel frees the lock when the holder's last FD closes, crash included.
NoOrphanOwner == \A r \in Runs: owner = r => Holder(r)
NextRequestProgress == \A r \in Runs:
    (<>[](Clean /\ (\A q \in Runs: ~parentFd[q] /\ ~childFd[q])))
    => <>(phase[r] # "Pending")
NextRequestStarts == \A r \in Runs: <>(phase[r] = "Running")
\* A held lease is eventually given up: the run ends and every FD it holds closes.
HeldEventuallyEnds == \A r \in Runs: (owner = r) ~> (owner # r)
\* Off means no runner run ever takes the channel lock.
OffNoRun == ~Enabled => (owner = None /\ \A r \in Runs: phase[r] = "Pending")
RunHasReservation == Kind = "run" =>
    \A r \in Runs: phase[r] \in {"Running", "Prepared", "Draining", "PausedUncertain"} => reserved[r]
WithinBudget == Spent <= Budget
\* The cursor moves once per finished run and never for a crashed or deferred one.
CursorCountsFinishedRuns == cursor = Cardinality({r \in Runs: advanced[r]})
=============================================================================
