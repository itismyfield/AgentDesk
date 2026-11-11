----------------------- MODULE InputOwnershipAdmission -----------------------
EXTENDS InputOwnership
CONSTANTS InitialRows, AdmissionSource, AdmissionPane

AdmissionInit ==
    /\ Init /\ mode = "turn" /\ src = AdmissionSource /\ mbx = "idle" /\ nat = AdmissionPane
    /\ old = "none" /\ obsOk /\ ~twin /\ screen = "ok" /\ draft = "none" /\ ~twinPend
    /\ ~stale /\ ~acct /\ row \in InitialRows /\ ~ep /\ ~leak
AdmissionDone == pastes = 1 /\ UNCHANGED vars
AdmissionNext == DriveIdle \/ DriveStartBusy \/ DriveIdleBusyPane \/ AdmissionDone

\* The ordinary owner's busy shortcut has no create/admit step to clear this marked row.
OwnerBusyKeepsMarked == (src = "discord" /\ nat = "busy" /\ pastes > 0) => row = "marked"


\* Purpose-specific ports refine the existing admission model using its observer slot.
\* Code baseline 92ef5b00b205679965c24c5c017f7237e8b5d57f; S18 is design only.
\* Reservation IDs are values, not grants. The actor remains the grant/receipt owner.
PortInputs == {"a", "b"}
P == observer
PortInit ==
    /\ mode = "turn" /\ src = AdmissionSource /\ mbx = "idle" /\ nat = AdmissionPane
    /\ natOwner = "none" /\ inp = "queued" /\ pend = FALSE /\ reader = FALSE
    /\ obsOk = TRUE /\ route = "none" /\ plain = FALSE /\ sentInto = "idle"
    /\ win = "open" /\ rcv = "none" /\ userRow = FALSE /\ twin = FALSE
    /\ claim = "none" /\ pastes = 0 /\ lp = 0 /\ op = 0 /\ screen = "ok"
    /\ draft = "none" /\ notices = 0 /\ wake = FALSE /\ timer = FALSE
    /\ byTimer = FALSE /\ timed = FALSE /\ heldBy = "none" /\ watch = FALSE
    /\ aged = FALSE /\ twinPend = FALSE /\ envLeft = 0 /\ stale = FALSE
    /\ old = "none" /\ acct = FALSE /\ row \in InitialRows /\ ep = FALSE
    /\ leak = FALSE /\ outside = FALSE /\ nr = "ok" /\ ev = "none"
    /\ prov = "none" /\ seen = "none"
    /\ observer = [boot |-> "restart", failures |-> 0, restarts |-> 0,
        epoch |-> 1, running |-> "none", wake |-> FALSE, oldReceipt |-> 0,
        arrivals |-> 0, rank |-> [i \in PortInputs |-> 0],
        stage |-> [i \in PortInputs |-> "new"],
        admitted |-> {}, grants |-> {}, effects |-> {},
        receipts |-> [i \in PortInputs |-> 0],
        deniedGrantSeen |-> FALSE, deniedStopSeen |-> FALSE,
        unauthorizedGrant |-> FALSE, unauthorizedStop |-> FALSE,
        staleStopSeen |-> FALSE, staleStopApplied |-> FALSE]

\* ExecutionAuthority Prepare/PrepareFails: no admission before a verified boot observation.
PortPrepareFails ==
    /\ P.boot = "restart" /\ P.failures = 0
    /\ observer' = [P EXCEPT !.boot = "held", !.failures = 1]
    /\ UNCHANGED ObserverLegacy
PortPrepare ==
    /\ P.boot \in {"restart", "held"}
    /\ observer' = [P EXCEPT !.boot = "ready", !.wake = TRUE]
    /\ UNCHANGED ObserverLegacy

\* A durable, authorized original under one stable ticket. Pending is never a success.
PortSettleBoot ==
    /\ P.boot = "ready" /\ row = "marked" /\ P.oldReceipt = 0
    /\ observer' = [P EXCEPT !.oldReceipt = 1]
    /\ UNCHANGED ObserverLegacy
PortAdmit(i) ==
    /\ P.boot = "ready" /\ P.stage[i] = "new"
    /\ observer' = [P EXCEPT !.stage[i] = "admitted", !.admitted = @ \cup {i},
                    !.wake = Mut # "portNoWake", !.arrivals = @ + 1, !.rank[i] = P.arrivals + 1]
    /\ UNCHANGED ObserverLegacy
\* drive.rs grant and claim_turn, v2.rs grant_front/reserve_claimed: same actor, exact front.
PortGrant(i) ==
    /\ P.boot = "ready" /\ P.wake /\ P.running = "none"
    /\ P.stage[i] = "admitted"
    /\ (\A j \in PortInputs : P.stage[j] \in {"admitted", "granted", "claimed", "effect"} => P.rank[i] <= P.rank[j])
    /\ observer' = [P EXCEPT !.stage[i] = "granted", !.grants = @ \cup {i}, !.running = i]
    /\ UNCHANGED ObserverLegacy
PortClaim(i) ==
    /\ P.boot = "ready" /\ P.running = i /\ P.stage[i] = "granted"
    /\ observer' = [P EXCEPT !.stage[i] = "claimed"]
    /\ UNCHANGED ObserverLegacy
PortEffect(i) ==
    /\ P.boot = "ready" /\ P.running = i /\ P.stage[i] = "claimed"
    /\ (row # "marked" \/ P.oldReceipt = 1 \/ Mut = "portBootBypass")
    /\ observer' = [P EXCEPT !.stage[i] = "effect", !.effects = @ \cup {i}]
    /\ UNCHANGED ObserverLegacy
PortEnd(i) ==
    /\ P.stage[i] = "effect"
    /\ observer' = [P EXCEPT !.stage[i] = "terminal", !.running = "none", !.wake = TRUE]
    /\ UNCHANGED ObserverLegacy
\* Receipt is a logical committed row, not the number of retrying INSERT attempts.
PortReceipt(i) ==
    /\ P.stage[i] = "terminal"
    /\ observer' = [P EXCEPT !.stage[i] = "done", !.receipts[i] = 1]
    /\ UNCHANGED ObserverLegacy
PortReceiptRetry(i) ==
    /\ P.stage[i] = "done" /\ P.receipts[i] < 2
    /\ observer' = [P EXCEPT !.receipts[i] = IF Mut = "portDuplicateReceipt" THEN @ + 1 ELSE @]
    /\ UNCHANGED ObserverLegacy

\* One crash: admission/terminal identity survives; volatile, unused rights do not.
\* Native effect uncertainty and replay proof remain in the existing owner/native-stop models.
PortRestart ==
    /\ P.boot = "ready" /\ P.restarts = 0
    /\ observer' = [P EXCEPT !.boot = "restart", !.restarts = 1, !.epoch = 2,
        !.running = "none", !.wake = FALSE,
        !.stage = [i \in PortInputs |-> CASE P.stage[i] \in {"granted", "claimed"} -> "admitted"
                                           [] P.stage[i] = "effect" -> "terminal"
                                           [] OTHER -> P.stage[i]]]
    /\ UNCHANGED ObserverLegacy

\* A lookup-only caller tries both operations; refusal is explored even on the good design.
PortDeniedGrant ==
    /\ ~P.deniedGrantSeen
    /\ observer' = [P EXCEPT !.deniedGrantSeen = TRUE,
        !.unauthorizedGrant = Mut = "portGrantLeak",
        !.grants = IF Mut = "portGrantLeak" THEN @ \cup {"a"} ELSE @]
    /\ UNCHANGED ObserverLegacy
PortDeniedStop ==
    /\ ~P.deniedStopSeen
    /\ observer' = [P EXCEPT !.deniedStopSeen = TRUE,
                            !.unauthorizedStop = Mut = "portStopLeak"]
    /\ UNCHANGED ObserverLegacy
PortStaleStop ==
    /\ P.epoch = 2 /\ ~P.staleStopSeen
    /\ observer' = [P EXCEPT !.staleStopSeen = TRUE,
                            !.staleStopApplied = Mut = "portLooseStop"]
    /\ UNCHANGED ObserverLegacy
\* The only modeled authorized stop consumes the current channel execution, not a cached lookup.
PortAuthorizedStop(i) ==
    /\ P.boot = "ready" /\ P.running = i /\ P.stage[i] \in {"claimed", "effect"}
    /\ observer' = [P EXCEPT !.stage[i] = "terminal", !.running = "none", !.wake = TRUE]
    /\ UNCHANGED ObserverLegacy

PortNext == PortPrepareFails \/ PortPrepare \/ PortSettleBoot \/ PortRestart \/ PortDeniedGrant \/ PortDeniedStop
            \/ PortStaleStop \/ (\E i \in PortInputs : PortAdmit(i) \/ PortGrant(i) \/ PortClaim(i)
                \/ PortEffect(i) \/ PortEnd(i) \/ PortReceipt(i) \/ PortReceiptRetry(i) \/ PortAuthorizedStop(i))
PortFair == WF_vars(PortPrepare) /\ WF_vars(PortSettleBoot)
            /\ (\A i \in PortInputs : WF_vars(PortAdmit(i)) /\ WF_vars(PortGrant(i))
                /\ WF_vars(PortClaim(i)) /\ WF_vars(PortEffect(i)) /\ WF_vars(PortEnd(i))
                /\ WF_vars(PortReceipt(i)))
PortSpec == PortInit /\ [][PortNext]_vars /\ PortFair
PortSingleActive == \A i,j \in PortInputs :
    (P.stage[i] \in {"granted", "claimed", "effect"} /\ P.stage[j] \in {"granted", "claimed", "effect"}) => i = j
PortNoGrantWithoutAdmission == P.grants \subseteq P.admitted
PortReceiptAtMostOnce == \A i \in PortInputs : P.receipts[i] \in 0..1
PortReceiptSettledExactlyOnce == \A i \in PortInputs : (P.stage[i] = "done") => P.receipts[i] = 1
PortReceiptAfterTerminal == \A i \in PortInputs : (P.receipts[i] > 0) => P.stage[i] = "done"
PortNoUnauthorizedCalls == ~P.unauthorizedGrant /\ ~P.unauthorizedStop
PortStopSameEpoch == ~P.staleStopApplied
PortBootReceiptBeforeEffect == P.effects # {} /\ row = "marked" => P.oldReceipt = 1
\* No permanent hold under recovered authority, healthy receipt storage and a fair owner drive.
PortAdmissionResumes == (P.boot = "held") ~> P.boot = "ready"
PortInputsSettle == \A i \in PortInputs : (i \in P.admitted) ~> (P.receipts[i] = 1)

=============================================================================
