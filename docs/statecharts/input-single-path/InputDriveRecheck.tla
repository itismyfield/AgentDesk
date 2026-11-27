-------------------------- MODULE InputDriveRecheck --------------------------
EXTENDS Naturals, Sequences

CONSTANTS Backoff, Blocker
VARIABLE drive

schedule == <<1, 2, 5, 10>>

Init ==
    drive = [original |-> IF Blocker = "head" THEN "head" ELSE "items",
             hostReady |-> Blocker \in {"noGrant", "head"},
             ownerGate |-> Blocker \in {"noGrant", "head"},
             wake |-> TRUE, held |-> FALSE, probe |-> FALSE,
             retryArmed |-> (Blocker = "retryWait"),
             phase |-> "ready", step |-> 1, remaining |-> 0,
             hints |-> 2, sends |-> 0]

HasInput == drive.original \in {"items", "head"}
DeliveryAllowed == HasInput /\ drive.hostReady /\ ~drive.ownerGate

\* Readiness, the release of an exact claim, or settlement of an existing head can
\* make progress possible without publishing a wake. Settlement itself is authoritative.
HostReady ==
    /\ HasInput /\ ~drive.hostReady /\ drive.held
    /\ drive' = [drive EXCEPT !.hostReady = TRUE]

ReleaseOwnerGate ==
    /\ HasInput /\ drive.ownerGate /\ drive.held
    /\ drive' = [drive EXCEPT !.ownerGate = FALSE,
                 !.original = IF Blocker = "head" THEN "items" ELSE @]

\* Admission and turn/lifecycle boundaries can wake or be absent.
Wake ==
    /\ HasInput /\ drive.held /\ drive.hints > 0
    /\ drive' = [drive EXCEPT !.wake = TRUE, !.phase = "ready", !.remaining = 0,
                 !.step = 1, !.hints = @ - 1]

Arm ==
    /\ HasInput /\ drive.held /\ Blocker = "handoff" /\ ~drive.probe /\ ~drive.hostReady
    /\ drive' = [drive EXCEPT !.probe = TRUE]

ProbeWake ==
    /\ drive.probe /\ DeliveryAllowed
    /\ drive' = [drive EXCEPT !.probe = FALSE, !.wake = TRUE,
                 !.phase = "ready", !.remaining = 0, !.step = 1]

ProofWake ==
    /\ drive.retryArmed /\ DeliveryAllowed
    /\ drive' = [drive EXCEPT !.retryArmed = FALSE, !.wake = TRUE,
                 !.phase = "ready", !.remaining = 0, !.step = 1]

\* Monitor and timer only request a pass. One pass can hand off the original.
Look ==
    /\ HasInput /\ drive.wake
    /\ drive' = IF Backoff /\ DeliveryAllowed /\ drive.retryArmed
                 THEN [drive EXCEPT !.retryArmed = FALSE]
              ELSE IF DeliveryAllowed /\ ~drive.retryArmed
                 THEN [drive EXCEPT !.original = "delivered", !.sends = @ + 1,
                       !.wake = FALSE, !.held = FALSE, !.probe = FALSE,
                       !.phase = "stopped", !.step = 1, !.remaining = 0]
                 ELSE [drive EXCEPT !.wake = FALSE, !.held = TRUE,
                       !.phase = IF Backoff THEN "waiting" ELSE "stopped",
                       !.remaining = IF Backoff THEN schedule[drive.step] ELSE 0]

Second ==
    /\ drive.phase = "waiting" /\ drive.remaining > 0
    /\ drive' = [drive EXCEPT !.remaining = @ - 1]

Due ==
    /\ drive.phase = "waiting" /\ drive.remaining = 0
    /\ drive' = [drive EXCEPT !.phase = "ready", !.wake = TRUE,
                 !.step = IF @ < Len(schedule) THEN @ + 1 ELSE @]

Cancel ==
    /\ HasInput /\ ~DeliveryAllowed /\ drive.held
    /\ drive' = [drive EXCEPT !.original = "removed", !.wake = FALSE,
                 !.held = FALSE, !.probe = FALSE, !.retryArmed = FALSE, !.phase = "stopped",
                 !.remaining = 0, !.step = 1]

Next == HostReady \/ ReleaseOwnerGate \/ Wake \/ Arm \/ ProbeWake \/ ProofWake
        \/ Look \/ Second \/ Due \/ Cancel

\* Time and a runnable drive advance. Wake or a successful probe is never owed.
Spec == Init /\ [][Next]_drive /\ WF_drive(Look) /\ WF_drive(Second) /\ WF_drive(Due)

QueueEventuallyDelivered == DeliveryAllowed ~> (drive.original = "delivered")
EmptyHasNoWork == ~HasInput =>
    (drive.phase = "stopped" /\ ~drive.wake /\ ~drive.probe /\ drive.remaining = 0)
OneDelivery == drive.sends \in 0..1
NoDeliveryBeforeClear == drive.sends > 0 => drive.hostReady /\ ~drive.ownerGate /\ ~drive.retryArmed
TimerBound == drive.step \in 1..Len(schedule)
              /\ drive.remaining \in 0..schedule[drive.step]
=============================================================================
