---------------------- MODULE CampaignVerification ----------------------
\* #7438 E3-1: one node, one registered verification execution (V) with a preparation phase (1)
\* and a test phase (2), the Work attempt A that owns it and the Review attempt R that may consume it.
\* The Work session launch may still be in flight when a stop arrives.
\* Code reference 975200a8e7; the constants below pick the design under test.
EXTENDS Naturals
CONSTANTS
    SettleCancelled,     \* settling closes a V whose remaining phases can no longer start
    NotSentReplans,      \* a refused start whose holder is still valid returns to Planned
    FactsAfterStop,      \* start/result/settle facts are recorded after an Immediate stop
    RedeliverFinished,   \* recovery offers a kept result until the server took it
    ClaimIsAuthority,    \* mutant: revocation keeps claims and a claim is enough to launch
    FinishClosesDeadV,   \* a finishing attempt whose V can never pass is closed as interrupted
    InterruptSettles,    \* an interrupted node's attempt closes once its phases are quiet
    FencedStartRefused,  \* landed observe_action rule: no new start receipt after the fence ACK
    MidInitiateCrash,    \* the server may die between the journal intent and the receipt
    IndetAbsentSettles,  \* proposed: an unresolved start with no live process ends as interrupted
    ProvisionalGrant,    \* a finish stop keeps the checks of a launch still in flight until it resolves
    OverwriteSlots,      \* mutant: a bound subject may be bound again to another value
    MaxCrash,
    Launches,            \* the Work session launch at the start: "started" and/or "pending" (needs TwoPhase)
    SubjectValues,       \* the candidate values a subject may be bound to
    Kinds,               \* which checks are explored: "start" (before review) and/or "finish" (review prerequisite)
    TwoPhase,            \* FALSE (chart-sized config): test phase only, output already sealed, result delivered when recorded
    Interruptible        \* FALSE leaves the single-task stop out of the chart-sized config
VARIABLES
    up, crashes,
    control, stop, cv, acked, confirmed,
    gateCv, gateEpoch,
    kind, stage, epoch, interrupted, attA, attR, grant, revoked, launch, prov,
    subject, first, plan, inb, vstate,
    act, init, res,
    lsrc, lcv, lep, llegit, fresh, handle,
    journal, jres, delivered, proc, starts,
    badStart

sys == <<up, crashes>>
ctl == <<control, stop, cv, acked, confirmed>>
gate == <<gateCv, gateEpoch>>
node == <<kind, stage, epoch, interrupted, attA, attR, grant, revoked, launch, prov>>
vv == <<subject, first, plan, inb, vstate>>
ph == <<act, init, res>>
drv == <<lsrc, lcv, lep, llegit, fresh, handle>>
host == <<journal, jres, delivered, proc, starts>>
vars == <<sys, ctl, gate, node, vv, ph, drv, host, badStart>>

P == IF TwoPhase THEN {1, 2} ELSE {1}
All(v) == [p \in P |-> v]

Init ==
    /\ up = TRUE /\ crashes = 0
    /\ control = "running" /\ stop = "none" /\ cv = 0 /\ acked = FALSE /\ confirmed = FALSE
    /\ gateCv = 0 /\ gateEpoch = 1
    /\ kind \in Kinds /\ stage = "impl" /\ epoch = 1 /\ interrupted = FALSE
    /\ attA = "open" /\ attR = "absent" /\ grant = FALSE /\ revoked = FALSE
    /\ launch \in Launches /\ prov = FALSE
    /\ subject = (IF TwoPhase THEN "none" ELSE "a") /\ first = subject /\ plan = FALSE /\ inb = All(FALSE) /\ vstate = "open"
    /\ act = All("planned") /\ init = All("none") /\ res = All("none")
    /\ lsrc = All("none") /\ lcv = All(0) /\ lep = All(0) /\ llegit = All(TRUE)
    /\ fresh = All(FALSE) /\ handle = All(FALSE)
    /\ journal = All("none") /\ jres = All("none") /\ delivered = All(FALSE)
    /\ proc = All("none") /\ starts = All(0)
    /\ badStart = FALSE

Cur == IF stage = "impl" THEN attA ELSE IF stage = "review" THEN attR ELSE "none"
CurOpen == Cur = "open"
\* The current attempt owns V (Work) or holds it as a fixed prerequisite (Review of a finish check).
Consumes == stage = "impl" \/ (stage = "review" /\ kind = "finish")
Finishing == control = "stopping" /\ stop = "finish"
\* stage_fsm.rs validate_verification_start: who may start a phase of V right now.
Auth ==
    IF interrupted \/ ~CurOpen \/ ~Consumes \/ vstate # "open" THEN "none"
    ELSE IF control = "running" THEN (IF stage = "impl" THEN "work" ELSE "review")
    ELSE IF Finishing /\ grant THEN "finish"
    ELSE "none"

Live(p) == init[p] \in {"started", "indet"} \/ act[p] \notin {"planned", "reconciled", "cancelled"}
Quiet == \A p \in P : res[p] # "none" \/ ~Live(p)
StartedNoResult(p) ==
    /\ init[p] \in {"started", "indet"} \/ act[p] \in {"started", "unknown", "pending"}
    /\ res[p] = "none"
CanCancelV == \A p \in P : ~StartedNoResult(p)
Unstarted == IF ClaimIsAuthority THEN {"planned"} ELSE {"planned", "claimed"}
CancelUnstarted(a) == [p \in P |-> IF a[p] \in Unstarted THEN "cancelled" ELSE a[p]]
\* stage_fsm.rs revoke_unstarted: a collected V is skipped, an uncancellable V keeps its record open.
RevokedAct == IF vstate = "open" THEN CancelUnstarted(act) ELSE act
RevokedV == IF vstate = "open" /\ CanCancelV THEN "cancelled" ELSE vstate
FactOK == FactsAfterStop \/ stop # "immediate"

\* ---- binding and claim (server commands, need a current holder) ----
\* The sealed output comes from the session, so a Work attempt needs its launch receipt first.
Materialize(v) ==
    /\ up /\ Auth # "none" /\ (stage = "impl" => launch = "started")
    /\ IF OverwriteSlots THEN subject # v ELSE subject = "none"
    /\ subject' = v /\ first' = (IF first = "none" THEN v ELSE first)
    /\ UNCHANGED <<sys, ctl, gate, node, plan, inb, vstate, ph, drv, host, badStart>>
BindInput(p) ==
    /\ up /\ Auth # "none" /\ subject # "none" /\ ~inb[p]
    /\ p = 2 => (res[1] = "ok" /\ plan)
    /\ inb' = [inb EXCEPT ![p] = TRUE]
    /\ UNCHANGED <<sys, ctl, gate, node, subject, first, plan, vstate, ph, drv, host, badStart>>
BindPlan ==
    /\ up /\ TwoPhase /\ Auth # "none" /\ ~plan /\ res[1] = "ok" /\ plan' = TRUE
    /\ UNCHANGED <<sys, ctl, gate, node, subject, first, inb, vstate, ph, drv, host, badStart>>
Claim(p) ==
    /\ up /\ Auth # "none" /\ act[p] = "planned" /\ init[p] = "none" /\ inb[p]
    /\ p = 2 => res[1] = "ok"
    /\ act' = [act EXCEPT ![p] = "claimed"]
    /\ UNCHANGED <<sys, ctl, gate, node, vv, init, res, drv, host, badStart>>
Pend(p) ==
    /\ up /\ (Auth # "none" \/ ClaimIsAuthority) /\ act[p] = "claimed"
    /\ act' = [act EXCEPT ![p] = "pending"]
    /\ UNCHANGED <<sys, ctl, gate, node, vv, init, res, drv, host, badStart>>

\* ---- the driver builds the approved launch from stored state, the gate decides ----
Launch(p) ==
    /\ up /\ act[p] = "pending" /\ lsrc[p] = "none" /\ journal[p] = "none"
    /\ Auth # "none" \/ ClaimIsAuthority
    /\ lsrc' = [lsrc EXCEPT ![p] = IF Auth # "none" THEN Auth ELSE "claim"]
    /\ lcv' = [lcv EXCEPT ![p] = cv] /\ lep' = [lep EXCEPT ![p] = epoch]
    /\ llegit' = [llegit EXCEPT ![p] = Auth # "none"]
    /\ UNCHANGED <<sys, ctl, gate, node, vv, ph, fresh, handle, host, badStart>>
GateOk(p) == lcv[p] >= gateCv /\ lep[p] >= gateEpoch
GateInitiate(p) ==
    /\ up /\ lsrc[p] # "none" /\ journal[p] = "none" /\ GateOk(p)
    /\ gateCv' = lcv[p] /\ gateEpoch' = lep[p]
    /\ journal' = [journal EXCEPT ![p] = "started"] /\ proc' = [proc EXCEPT ![p] = "alive"]
    /\ starts' = [starts EXCEPT ![p] = @ + 1]
    /\ handle' = [handle EXCEPT ![p] = TRUE] /\ lsrc' = [lsrc EXCEPT ![p] = "none"]
    /\ badStart' = (badStart \/ ~llegit[p])
    /\ UNCHANGED <<sys, ctl, node, vv, ph, lcv, lep, llegit, fresh, jres, delivered>>
\* The gate refuses a launch older than its fences; the driver records the proven non-start.
GateRefuse(p) ==
    /\ up /\ lsrc[p] # "none" /\ ~GateOk(p)
    /\ lsrc' = [lsrc EXCEPT ![p] = "none"]
    /\ act' = IF act[p] = "pending" /\ FactOK
              THEN [act EXCEPT ![p] = IF NotSentReplans /\ Auth # "none" THEN "planned" ELSE "cancelled"]
              ELSE act
    /\ UNCHANGED <<sys, ctl, gate, node, vv, init, res, lcv, lep, llegit, fresh, handle, host, badStart>>
\* A start-pending phase nobody may start any more: apply the current fences, then record it unsent.
ClassifyUnsent(p) ==
    /\ up /\ FactOK /\ act[p] = "pending" /\ journal[p] = "none" /\ Auth = "none"
    /\ gateCv' = cv /\ gateEpoch' = epoch
    /\ act' = [act EXCEPT ![p] = "cancelled"] /\ lsrc' = [lsrc EXCEPT ![p] = "none"]
    /\ UNCHANGED <<sys, ctl, node, vv, init, res, lcv, lep, llegit, fresh, handle, host, badStart>>
ApplyNodeFence ==
    /\ up /\ gateEpoch < epoch /\ gateEpoch' = epoch
    /\ UNCHANGED <<sys, ctl, gateCv, node, vv, ph, drv, host, badStart>>

\* ---- facts: start receipt, exit, result, delivery ----
RecordStart(p) ==
    /\ up /\ FactOK /\ journal[p] \in {"started", "finished"} /\ act[p] \in {"pending", "unknown"}
    /\ FencedStartRefused => (~(acked \/ confirmed) \/ grant)
    /\ act' = [act EXCEPT ![p] = "started"] /\ init' = [init EXCEPT ![p] = "started"]
    /\ UNCHANGED <<sys, ctl, gate, node, vv, res, drv, host, badStart>>
ProcExit(p) ==
    /\ proc[p] = "alive" /\ proc' = [proc EXCEPT ![p] = "exited"]
    /\ UNCHANGED <<sys, ctl, gate, node, vv, ph, drv, journal, jres, delivered, starts, badStart>>
\* Without the runner handle (after a restart) the exit status is gone: the result is an interruption.
GateFinish(p) ==
    /\ up /\ journal[p] = "started" /\ proc[p] = "exited"
    /\ journal' = [journal EXCEPT ![p] = "finished"]
    /\ \E r \in (IF handle[p] THEN {"ok", "fail"} ELSE {"interrupted"}) : jres' = [jres EXCEPT ![p] = r]
    /\ fresh' = [fresh EXCEPT ![p] = TRUE]
    /\ UNCHANGED <<sys, ctl, gate, node, vv, ph, lsrc, lcv, lep, llegit, handle, delivered, proc, starts, badStart>>
RecordResult(p) ==
    /\ up /\ FactOK /\ journal[p] = "finished" /\ act[p] = "started" /\ res[p] = "none"
    /\ fresh[p] \/ RedeliverFinished
    /\ res' = [res EXCEPT ![p] = jres[p]] /\ act' = [act EXCEPT ![p] = "reconciled"]
    /\ delivered' = IF TwoPhase THEN delivered ELSE [delivered EXCEPT ![p] = TRUE]
    /\ UNCHANGED <<sys, ctl, gate, node, vv, init, drv, journal, jres, proc, starts, badStart>>
Deliver(p) ==
    /\ up /\ journal[p] \in {"finished", "settled"} /\ ~delivered[p] /\ res[p] # "none"
    /\ delivered' = [delivered EXCEPT ![p] = TRUE]
    /\ UNCHANGED <<sys, ctl, gate, node, vv, ph, drv, journal, jres, proc, starts, badStart>>

\* ---- a start lost in a crash ----
CrashMidInitiate(p) ==
    /\ MidInitiateCrash /\ up /\ crashes < MaxCrash
    /\ lsrc[p] # "none" /\ journal[p] = "none" /\ GateOk(p)
    /\ gateCv' = lcv[p] /\ gateEpoch' = lep[p]
    /\ journal' = [journal EXCEPT ![p] = "indet"]
    /\ \E spawned \in BOOLEAN :
        /\ proc' = [proc EXCEPT ![p] = IF spawned THEN "alive" ELSE "none"]
        /\ starts' = [starts EXCEPT ![p] = IF spawned THEN @ + 1 ELSE @]
        /\ badStart' = (badStart \/ (spawned /\ ~llegit[p]))
    /\ up' = FALSE /\ crashes' = crashes + 1
    /\ lsrc' = All("none") /\ fresh' = All(FALSE) /\ handle' = All(FALSE)
    /\ UNCHANGED <<ctl, node, vv, ph, lcv, lep, llegit, jres, delivered>>
RecordIndet(p) ==
    /\ up /\ FactOK /\ journal[p] = "indet" /\ act[p] = "pending"
    /\ act' = [act EXCEPT ![p] = "unknown"] /\ init' = [init EXCEPT ![p] = "indet"]
    /\ UNCHANGED <<sys, ctl, gate, node, vv, res, drv, host, badStart>>
\* Only a live process carrying the same nonce proves the start.
ObserveNonce(p) ==
    /\ up /\ journal[p] = "indet" /\ proc[p] = "alive"
    /\ journal' = [journal EXCEPT ![p] = "started"]
    /\ UNCHANGED <<sys, ctl, gate, node, vv, ph, drv, jres, delivered, proc, starts, badStart>>
SettleIndet(p) ==
    /\ up /\ IndetAbsentSettles /\ journal[p] = "indet" /\ proc[p] # "alive"
    /\ journal' = [journal EXCEPT ![p] = "settled"] /\ jres' = [jres EXCEPT ![p] = "interrupted"]
    /\ fresh' = [fresh EXCEPT ![p] = TRUE]
    /\ UNCHANGED <<sys, ctl, gate, node, vv, ph, lsrc, lcv, lep, llegit, handle, delivered, proc, starts, badStart>>
\* Neither started nor unsent is proven; the phase ends as an interruption and is never rerun.
RecordIndetSettled(p) ==
    /\ up /\ FactOK /\ journal[p] = "settled" /\ act[p] \in {"pending", "unknown"}
    /\ res' = [res EXCEPT ![p] = "interrupted"] /\ act' = [act EXCEPT ![p] = "reconciled"]
    /\ init' = [init EXCEPT ![p] = "settled"]
    /\ UNCHANGED <<sys, ctl, gate, node, vv, drv, host, badStart>>

\* ---- settle: collect, or close a V that can no longer run ----
Outcome ==
    IF \E p \in P : res[p] = "interrupted" THEN "collected_int"
    ELSE IF res[1] = "fail" THEN "collected_fail"
    ELSE IF res[1] = "ok" /\ (~TwoPhase \/ res[2] = "ok") THEN "collected_pass"
    ELSE IF res[1] = "ok" /\ TwoPhase /\ res[2] = "fail" THEN "collected_fail"
    ELSE IF SettleCancelled /\ \E p \in P : act[p] = "cancelled" THEN "cancelled"
    ELSE "open"
Settle ==
    /\ up /\ FactOK /\ vstate = "open" /\ Quiet /\ Outcome # "open"
    /\ vstate' = Outcome
    /\ act' = IF SettleCancelled
              THEN [p \in P |-> IF act[p] \in {"planned", "claimed"} THEN "cancelled" ELSE act[p]]
              ELSE act
    /\ UNCHANGED <<sys, ctl, gate, node, subject, first, plan, inb, init, res, drv, host, badStart>>

\* ---- stage results ----
Close ==
    /\ stage' = "closed" /\ epoch' = epoch + 1 /\ revoked' = TRUE
    /\ act' = RevokedAct /\ vstate' = RevokedV
    /\ UNCHANGED <<sys, ctl, gate, kind, interrupted, grant, launch, prov, subject, first, plan, inb, init, res, drv, host, badStart>>
BeginReview(k) ==
    /\ kind = k /\ up /\ control = "running" /\ stage = "impl" /\ attA = "open" /\ ~interrupted /\ launch = "started"
    /\ kind = "start" => vstate = "collected_pass"
    /\ attA' = "passed" /\ attR' = "open" /\ stage' = "review" /\ epoch' = epoch + 1
    /\ UNCHANGED <<sys, ctl, gate, kind, interrupted, grant, revoked, launch, prov, vv, ph, drv, host, badStart>>
AcceptWork(k) ==
    /\ kind = k /\ up /\ stage = "impl" /\ attA = "open" /\ ~interrupted /\ (control = "running" \/ Finishing)
    /\ launch = "started"
    /\ \/ kind = "start" /\ vstate = "collected_fail" /\ attA' = "failed"
       \/ kind = "start" /\ vstate = "collected_pass" /\ Finishing /\ attA' = "passed"
       \/ kind = "finish" /\ Finishing /\ attA' = "passed"
    /\ attR' = attR /\ Close
AcceptReview ==
    /\ up /\ stage = "review" /\ attR = "open" /\ ~interrupted /\ (control = "running" \/ Finishing)
    /\ \/ kind = "finish" /\ vstate = "collected_pass" /\ attR' = "passed"
       \/ kind = "finish" /\ vstate = "collected_fail" /\ attR' = "failed"
       \/ kind = "start" /\ attR' \in {"passed", "failed"}
    /\ attA' = attA /\ Close
CloseDeadFinish ==
    /\ up /\ FinishClosesDeadV /\ Finishing /\ CurOpen /\ ~interrupted /\ Consumes
    /\ vstate \in {"collected_int", "cancelled"} /\ launch # "pending"
    /\ attA' = IF stage = "impl" THEN "interrupted" ELSE attA
    /\ attR' = IF stage = "review" THEN "interrupted" ELSE attR
    /\ Close
ConfirmInterrupted ==
    /\ up /\ InterruptSettles /\ interrupted /\ CurOpen /\ launch # "pending"
    /\ \A p \in P : act[p] \notin {"claimed", "pending", "started", "unknown"}
    /\ attA' = IF stage = "impl" THEN "interrupted" ELSE attA
    /\ attR' = IF stage = "review" THEN "interrupted" ELSE attR
    /\ UNCHANGED <<sys, ctl, gate, kind, stage, epoch, interrupted, grant, revoked, launch, prov, vv, ph, drv, host, badStart>>

\* ---- the Work session launch resolves late ----
\* A journal-proven launch receipt is recorded whatever the stop state; a provisional scope becomes the grant.
LaunchStarted ==
    /\ up /\ FactOK /\ launch = "pending" /\ launch' = "started" /\ prov' = FALSE
    /\ grant' = (grant \/ (prov /\ Finishing /\ attA = "open" /\ ~interrupted /\ vstate = "open"))
    /\ UNCHANGED <<sys, ctl, gate, kind, stage, epoch, interrupted, attA, attR, revoked, vv, ph, drv, host, badStart>>
\* The launch is proven unsent after its holder was revoked: its checks are revoked and the attempt closes.
LaunchUnsent ==
    /\ up /\ FactOK /\ launch = "pending" /\ (control = "stopping" \/ interrupted)
    /\ launch' = "unsent" /\ prov' = FALSE /\ revoked' = TRUE
    /\ act' = RevokedAct /\ vstate' = RevokedV
    /\ attA' = IF attA = "open" /\ \A p \in P : RevokedAct[p] \in {"reconciled", "cancelled"}
               THEN "interrupted" ELSE attA
    /\ UNCHANGED <<sys, ctl, gate, kind, stage, epoch, interrupted, attR, grant, subject, first, plan, inb, init, res, drv, host, badStart>>

\* ---- stops ----
Interrupt ==
    /\ up /\ Interruptible /\ control \in {"running", "stopping"} /\ ~interrupted /\ CurOpen
    /\ interrupted' = TRUE /\ epoch' = epoch + 1 /\ grant' = FALSE /\ prov' = FALSE /\ revoked' = TRUE
    /\ act' = RevokedAct /\ vstate' = RevokedV
    /\ UNCHANGED <<sys, ctl, gate, kind, stage, attA, attR, launch, subject, first, plan, inb, init, res, drv, host, badStart>>
Halt(a, m) == IF m = "immediate" /\ a = "open" THEN "interrupted" ELSE a
RequestStop(m) ==
    /\ up /\ control = "running"
    /\ control' = "stopping" /\ stop' = m /\ cv' = cv + 1 /\ acked' = FALSE /\ confirmed' = FALSE
    /\ revoked' = TRUE
    /\ LET g == m = "finish" /\ CurOpen /\ ~interrupted /\ Consumes /\ vstate = "open"
           late == stage = "impl" /\ launch = "pending"
           keep == g /\ (~late \/ ProvisionalGrant)
       IN /\ grant' = (g /\ ~late) /\ prov' = (keep /\ late)
          /\ act' = IF keep THEN act ELSE CancelUnstarted(act)
    /\ attA' = Halt(attA, m) /\ attR' = Halt(attR, m)
    /\ UNCHANGED <<sys, gate, kind, stage, epoch, interrupted, launch, vv, init, res, drv, host, badStart>>
Escalate ==
    /\ up /\ Finishing
    /\ stop' = "immediate" /\ cv' = cv + 1 /\ acked' = FALSE /\ confirmed' = FALSE /\ grant' = FALSE
    /\ prov' = FALSE /\ act' = CancelUnstarted(act)
    /\ attA' = Halt(attA, "immediate") /\ attR' = Halt(attR, "immediate")
    /\ UNCHANGED <<sys, control, gate, kind, stage, epoch, interrupted, revoked, launch, vv, init, res, drv, host, badStart>>
NoInFlight == launch # "pending" /\ \A p \in P : act[p] \notin {"pending", "unknown"}
AckFence ==
    /\ up /\ control = "stopping" /\ ~acked /\ NoInFlight
    /\ acked' = TRUE /\ gateCv' = cv
    /\ UNCHANGED <<sys, control, stop, cv, confirmed, gateEpoch, node, vv, ph, drv, host, badStart>>
ConfirmFence ==
    /\ up /\ control = "stopping" /\ acked /\ ~confirmed /\ NoInFlight /\ confirmed' = TRUE
    /\ UNCHANGED <<sys, control, stop, cv, acked, gate, node, vv, ph, drv, host, badStart>>
ConfirmStopped ==
    /\ up /\ control = "stopping" /\ confirmed
    /\ \A p \in P : act[p] \in {"reconciled", "cancelled"}
    /\ attA # "open" /\ attR # "open" /\ launch # "pending"
    /\ control' = "stopped"
    /\ UNCHANGED <<sys, stop, cv, acked, confirmed, gate, node, vv, ph, drv, host, badStart>>

\* ---- server restart: driver memory is lost, the journal and the processes are not ----
Crash ==
    /\ up /\ crashes < MaxCrash /\ up' = FALSE /\ crashes' = crashes + 1
    /\ lsrc' = All("none") /\ fresh' = All(FALSE) /\ handle' = All(FALSE)
    /\ UNCHANGED <<ctl, gate, node, vv, ph, lcv, lep, llegit, host, badStart>>
Restart == ~up /\ up' = TRUE /\ UNCHANGED <<crashes, ctl, gate, node, vv, ph, drv, host, badStart>>
Idle == up /\ UNCHANGED vars

Driver ==
    \/ BindPlan \/ ApplyNodeFence \/ Settle \/ CloseDeadFinish \/ ConfirmInterrupted
    \/ LaunchStarted \/ LaunchUnsent \/ AckFence \/ ConfirmFence \/ ConfirmStopped
    \/ BeginReview("start") \/ AcceptWork("start")
    \/ \E p \in P : \/ BindInput(p) \/ Claim(p) \/ Pend(p) \/ Launch(p)
                    \/ GateInitiate(p) \/ GateRefuse(p) \/ ClassifyUnsent(p)
                    \/ RecordStart(p) \/ GateFinish(p) \/ RecordResult(p) \/ Deliver(p)
                    \/ RecordIndet(p) \/ ObserveNonce(p) \/ SettleIndet(p) \/ RecordIndetSettled(p)
ProcStep == \E p \in P : ProcExit(p)
\* Steps that need the session's own submission: the output that gets sealed, a work result that
\* no start check decides, and the review verdict.
SessionAnswers ==
    \/ \E v \in SubjectValues : Materialize(v)
    \/ BeginReview("finish") \/ AcceptWork("finish")
    \/ AcceptReview
Environment ==
    \/ Interrupt \/ RequestStop("immediate") \/ RequestStop("finish") \/ Escalate \/ Crash
    \/ \E p \in P : CrashMidInitiate(p)
Next == Driver \/ ProcStep \/ SessionAnswers \/ Environment \/ Restart \/ Idle
\* SpecQuietSession assumes nothing about the session; Spec adds that it eventually submits or is stopped.
SpecQuietSession == Init /\ [][Next]_vars /\ WF_vars(Driver) /\ WF_vars(ProcStep) /\ WF_vars(Restart)
Spec == SpecQuietSession /\ WF_vars(SessionAnswers)

\* ---- rules ----
NoStartWithoutAuthority == ~badStart
StartedAtMostOnce == \A p \in P : starts[p] <= 1
NoPassWithoutCollection ==
    /\ (kind = "start" /\ attA = "passed") => vstate = "collected_pass"
    /\ (kind = "finish" /\ attR = "passed") => vstate = "collected_pass"
ResultNeedsStart == \A p \in P : res[p] # "none" => init[p] \in {"started", "settled"}
StoppedMeansQuiet ==
    control = "stopped" => \A p \in P : act[p] \in {"reconciled", "cancelled"} /\ proc[p] # "alive"
ImmediateRevokes == stop = "immediate" => \A p \in P : act[p] \notin {"planned", "claimed"}
NoCancelWhileAuthorized == (\E p \in P : act[p] = "cancelled") => (revoked \/ vstate # "open")
FillOnce == subject = first
\* A finishing attempt whose session did start keeps the authority to run its open check.
FinishScopeKept ==
    (Finishing /\ CurOpen /\ ~interrupted /\ Consumes /\ vstate = "open"
        /\ (stage = "impl" => launch = "started")) => grant

\* An Immediate stop needs no answer from the session; a finish stop waits for one.
StopNowCompletes == (control = "stopping" /\ stop = "immediate") ~> (control = "stopped")
StopFinishCompletes == (control = "stopping" /\ stop = "finish") ~> (control = "stopped" \/ stop = "immediate")
\* A V that was registered but never got a sealed subject is closed once the campaign stopped.
UnsealedClosed == (control = "stopped" /\ subject = "none") ~> (vstate = "cancelled")
VSettles == <>(vstate # "open")
DeliveredEventually == \A p \in P : (journal[p] \in {"finished", "settled"}) ~> delivered[p]
=========================================================================
