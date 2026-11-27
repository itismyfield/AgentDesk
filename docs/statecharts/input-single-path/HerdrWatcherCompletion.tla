--------------------- MODULE HerdrWatcherCompletion ---------------------
\* InputOwnership.TurnEnds and MailboxReleases split at the watcher terminal boundary.
\* NativeCompletionRecord's native custody stays with the native recorder.
EXTENDS Naturals
CONSTANTS Mode, Scenario

VARIABLE st
vars == <<st>>

\* The exact fallback cases have a Some(0) prelaunch coordinate and a positive suffix start.
ExactCases == {"StoredAtFrameStart", "StoredInside"}
RejectedCases == {"OtherExecution", "OtherTurn", "OtherFile", "OtherSource", "UnpinnedFile",
                  "MissingStart", "OtherWatcherNonce", "Ownerless", "OrdinaryAtFrameEnd",
                  "StoredAtFrameEnd", "StoredBeforeFrame", "StoredInBufferedTail",
                  "NonPrelaunchBeforeFrame", "MissingOrdinaryStart", "OtherRelayOwner",
                  "OtherRuntime"}
FrameCases == IF Scenario = "Boundaries"
              THEN ExactCases \cup RejectedCases \cup {"OrdinaryInside"}
              ELSE ExactCases
StrictFailures == {"HandleMissing", "Paused", "PausedLive", "Unknown", "DoneWithoutOwnership"}
PersistentFailures == {"Paused", "PausedLive", "Unknown"}

Init == \E proof \in FrameCases :
    st = [host |-> "Busy", source |-> "Prelaunch", proof |-> proof, frame |-> "Unread",
          lease |-> TRUE, priorPosts |-> 0, front |-> "Queued", nextPosts |-> 0,
          probes |-> 0, deadline |-> "Far", recheck |-> "None", wake |-> FALSE,
          runtime |-> "Live", restarted |-> FALSE]

NativeHandoff ==
    /\ st.runtime = "Live" /\ st.source = "Prelaunch"
    /\ st' = [st EXCEPT !.source = "Transcript"]

HostGoesIdle ==
    /\ st.runtime = "Live" /\ st.source = "Transcript" /\ st.host = "Busy"
    \* The presence transition supplies no queue wake while the lease exists.
    /\ st' = [st EXCEPT !.host = "Idle"]

PublishPriorAnswer ==
    /\ st.runtime = "Live" /\ st.source = "Transcript" /\ st.priorPosts = 0
    /\ st' = [st EXCEPT !.priorPosts = @ + 1]

OwnerHoldsFront ==
    /\ st.runtime = "Live" /\ st.host = "Idle" /\ st.lease /\ st.front = "Queued"
    /\ st' = [st EXCEPT !.front = "Held"]

FrameHasAuthority ==
    \/ st.proof = "OrdinaryInside"
    \/ Mode # "Before" /\ st.proof \in ExactCases
    \/ Mode = "IgnoreIdentity" /\ st.proof \in {"OtherExecution", "OtherTurn", "OtherFile"}

WatcherReadsFrame ==
    /\ st.runtime = "Live" /\ st.source = "Transcript"
    /\ st.frame = "Unread"
    /\ st' = [st EXCEPT !.frame = IF FrameHasAuthority THEN "Submitted" ELSE "Rejected"]

ProbeBackstop ==
    /\ st.runtime = "Live" /\ st.lease /\ st.frame = "Rejected" /\ st.deadline = "Far"
    /\ st.priorPosts = 1
    /\ st' = [st EXCEPT !.probes = @ + 1,
                        !.deadline = IF st.probes = 1 THEN "Pulled" ELSE @]

ExtendStrictDeadline ==
    /\ st.runtime = "Live" /\ st.deadline = "Pulled"
    /\ \E reason \in StrictFailures :
        st' = [st EXCEPT !.deadline = "Extended1800", !.probes = 0, !.recheck = reason]

\* A successful strict recheck can still leave native custody in charge.
KeepNativeCustody ==
    /\ Scenario = "Boundaries" /\ st.runtime = "Live" /\ st.deadline = "Pulled"
    /\ st' = [st EXCEPT !.deadline = "NativeCustody", !.recheck = "NativeInput"]

NaturalDeadlineRechecks ==
    /\ st.runtime = "Live" /\ st.deadline = "Extended1800"
    /\ st' = [st EXCEPT !.deadline = IF st.recheck \in PersistentFailures
                                       THEN "RetryFar" ELSE "BackstopTerminal"]

RetryFarDeadline ==
    /\ st.runtime = "Live" /\ st.deadline = "RetryFar"
    /\ st' = [st EXCEPT !.deadline = "Extended1800"]

TerminalAvailable == st.frame = "Submitted" \/ st.deadline = "BackstopTerminal"
CanonicalRelease ==
    /\ st.runtime = "Live" /\ TerminalAvailable /\ st.lease /\ st.priorPosts = 1
    /\ st' = [st EXCEPT !.lease = FALSE, !.wake = TRUE]

OwnerDeliversFront ==
    /\ st.runtime = "Live" /\ st.wake /\ ~st.lease /\ st.host = "Idle"
    /\ st.front \in {"Queued", "Held"}
    /\ st' = [st EXCEPT !.front = "Delivered", !.wake = FALSE]

PublishNextAnswer ==
    /\ st.front = "Delivered"
    /\ st' = [st EXCEPT !.front = "Answered", !.nextPosts = @ + 1]

Restart ==
    /\ Scenario = "Restart" /\ st.runtime = "Live" /\ ~st.restarted
    /\ st.source = "Transcript" /\ st.frame = "Unread"
    /\ st' = [st EXCEPT !.runtime = "Restoring", !.restarted = TRUE]

RestoreInflight ==
    /\ st.runtime = "Restoring"
    \* Reloads the same durable row/source/episode, including its pinned Herdr start.
    /\ st' = [st EXCEPT !.runtime = "Live"]

Next == NativeHandoff \/ HostGoesIdle \/ PublishPriorAnswer \/ OwnerHoldsFront \/ WatcherReadsFrame
     \/ ProbeBackstop \/ ExtendStrictDeadline \/ KeepNativeCustody \/ NaturalDeadlineRechecks
     \/ RetryFarDeadline \/ CanonicalRelease
     \/ OwnerDeliversFront \/ PublishNextAnswer \/ Restart \/ RestoreInflight

Spec == Init /\ [][Next]_vars
    /\ WF_vars(NativeHandoff) /\ WF_vars(HostGoesIdle) /\ WF_vars(PublishPriorAnswer)
    /\ WF_vars(OwnerHoldsFront) /\ WF_vars(WatcherReadsFrame) /\ WF_vars(ProbeBackstop)
    /\ WF_vars(ExtendStrictDeadline) /\ WF_vars(NaturalDeadlineRechecks) /\ WF_vars(RetryFarDeadline)
    /\ WF_vars(CanonicalRelease) /\ WF_vars(OwnerDeliversFront)
    /\ WF_vars(PublishNextAnswer) /\ WF_vars(RestoreInflight)

AnswerOnce == /\ st.priorPosts <= 1
              /\ st.nextPosts = (IF st.front = "Answered" THEN 1 ELSE 0)
ExactFrameAuthority == st.frame = "Submitted" => st.proof \in ExactCases \cup {"OrdinaryInside"}
ReleaseNeedsTerminal == ~st.lease => TerminalAvailable
NoSilentLoss == ~(st.host = "Idle" /\ st.priorPosts = 1 /\ st.front = "Held"
                  /\ st.proof \in ExactCases /\ st.deadline \in {"Extended1800", "RetryFar"}
                  /\ st.recheck \in PersistentFailures /\ ~st.wake)
HeldEventuallyDelivered == (st.host = "Idle" /\ st.priorPosts = 1 /\ st.front = "Held")
                          ~> st.front \in {"Delivered", "Answered"}
DeliveredEventuallyAnswered == st.front = "Delivered" ~> st.front = "Answered"
=============================================================================
