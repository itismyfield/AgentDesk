---- MODULE HerdrRetiredRowNotice ----
\* Notice projection of HerdrRetiredRow's live end. Keys are <<execution nonce, save generation>>.
\* A Discord success and the runtime receipt are one local step. Boot recovery has its own owner.
EXTENDS Naturals
CONSTANT Mode

Keys == {<<1, 1>>, <<1, 2>>, <<2, 1>>}
VARIABLES key, phase, posts, recorded, restarted, failedSend
vars == <<key, phase, posts, recorded, restarted, failedSend>>

Init ==
    /\ key = <<1, 1>> /\ phase = "bound"
    /\ posts = [k \in Keys |-> 0] /\ recorded = {}
    /\ restarted = FALSE /\ failedSend = FALSE

Retire ==
    /\ phase = "bound" /\ phase' = "retired"
    /\ UNCHANGED <<key, posts, recorded, restarted, failedSend>>

LiveTick ==
    /\ phase = "retired" /\ phase' = "notice"
    /\ UNCHANGED <<key, posts, recorded, restarted, failedSend>>

Publish ==
    /\ phase = "notice" /\ key \notin recorded /\ phase' = "settle"
    /\ posts' = [posts EXCEPT ![key] = IF @ < 2 THEN @ + 1 ELSE 2]
    /\ recorded' = IF Mode = "Target" THEN recorded \cup {key} ELSE recorded
    /\ UNCHANGED <<key, restarted, failedSend>>

DeliveryFailed ==
    /\ phase = "notice" /\ key \notin recorded /\ ~failedSend
    /\ phase' = "retired" /\ failedSend' = TRUE
    /\ UNCHANGED <<key, posts, recorded, restarted>>

UseReceipt ==
    /\ phase = "notice" /\ key \in recorded /\ phase' = "settle"
    /\ UNCHANGED <<key, posts, recorded, restarted, failedSend>>

SettleFailed ==
    /\ phase = "settle" /\ phase' = "retired"
    /\ UNCHANGED <<key, posts, recorded, restarted, failedSend>>

SettleSucceeded ==
    /\ phase = "settle" /\ phase' = "ended"
    /\ UNCHANGED <<key, posts, recorded, restarted, failedSend>>

AdvanceSave ==
    /\ phase = "retired" /\ key = <<1, 1>> /\ posts[key] > 0
    /\ key' = <<1, 2>> /\ UNCHANGED <<phase, posts, recorded, restarted, failedSend>>

Restart ==
    /\ ~restarted /\ phase \in {"retired", "notice", "settle", "ended"}
    /\ restarted' = TRUE /\ recorded' = {} /\ phase' = "prior_generation"
    /\ UNCHANGED <<key, posts, failedSend>>

NewActor ==
    /\ phase = "prior_generation" /\ key' = <<2, 1>> /\ phase' = "bound"
    /\ UNCHANGED <<posts, recorded, restarted, failedSend>>

Next ==
    \/ Retire \/ LiveTick \/ Publish \/ DeliveryFailed \/ UseReceipt
    \/ SettleFailed \/ SettleSucceeded \/ AdvanceSave \/ Restart \/ NewActor

Spec == Init /\ [][Next]_vars /\ WF_vars(LiveTick) /\ WF_vars(Publish) /\ WF_vars(UseReceipt)

\* Pending work may have zero posts. After publication or at the settlement boundary it has one.
NoticeExactlyOnce ==
    /\ \A k \in Keys : posts[k] \in {0, 1}
    /\ phase \in {"settle", "ended"} => posts[key] = 1

\* Fair live ticks and eventual transport success, or the actor leaves at restart.
LiveNoticeProgress ==
    \A k \in Keys : (key = k /\ phase = "retired") ~>
        (posts[k] = 1 \/ key # k \/ phase = "prior_generation")
====
