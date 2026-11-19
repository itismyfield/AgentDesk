--------------------------- MODULE CampaignLanding ----------------------------
EXTENDS Naturals
CONSTANTS CheckHead, CheckCi, CheckUnique, FailTimeout
VARIABLES job
vars == <<job>>

Init ==
    job = [stage |-> "pending", pinned |-> "a", checkout |-> "a",
           verified |-> "none", remote |-> "none", prHead |-> "none",
           count |-> 0, ci |-> "unknown", pushedInCi |-> FALSE]

Prepare ==
    /\ job.stage = "pending"
    /\ job' = [job EXCEPT !.stage = "preparing", !.pinned = job.checkout]

Preflight(head, mode) ==
    /\ job.stage = "preparing" /\ job.verified = "none"
    /\ head = job.checkout
    /\ ~CheckHead \/ head = job.pinned
    /\ mode \in {"full", "pre_tlc"}
    /\ job' = [job EXCEPT !.verified = head]

Observe(head, ci) ==
    /\ job.stage = "preparing"
    /\ job' = [job EXCEPT !.checkout = head, !.ci = ci]

Push ==
    /\ job.stage = "preparing" /\ job.verified # "none" /\ job.remote = "none"
    /\ ~CheckHead \/ (job.checkout = job.pinned /\ job.verified = job.pinned)
    /\ ~CheckCi \/ job.ci = "idle"
    /\ job' = [job EXCEPT !.remote = job.pinned, !.pushedInCi = job.ci # "idle"]

Open ==
    /\ job.stage \in {"preparing", "journaled", "ci_hold"}
    /\ job.verified # "none" /\ job.remote # "none"
    /\ job.count < 2
    /\ ~CheckHead \/ (job.checkout = job.pinned /\ job.verified = job.remote)
    /\ ~CheckUnique \/ job.count = 0
    /\ job' = [job EXCEPT !.stage = "journaled", !.count = @ + 1, !.prHead = job.remote]

Timeout ==
    /\ FailTimeout /\ job.stage = "preparing"
    /\ job' = [job EXCEPT !.stage = "failed"]

Replay ==
    /\ job.stage = "journaled"
    /\ job' = [job EXCEPT !.stage = "ci_hold"]

Next == Prepare \/ Push \/ Open \/ Timeout \/ Replay \/ UNCHANGED vars
        \/ (\E h \in {"a", "b"}, mode \in {"full", "pre_tlc"}: Preflight(h, mode))
        \/ (\E h \in {"a", "b"}, ci \in {"active", "idle", "unknown"}: Observe(h, ci))

Spec == Init /\ [][Next]_vars /\ WF_vars(Timeout) /\ WF_vars(Replay)
AtMostOnePr == job.count <= 1
PrHeadVerified == job.count > 0 => job.prHead = job.pinned /\ job.prHead = job.verified
NoPushDuringCi == ~job.pushedInCi
PreparationSettles == (job.stage \in {"preparing", "journaled"})
                      ~> (job.stage \in {"ci_hold", "failed"})
 =============================================================================
