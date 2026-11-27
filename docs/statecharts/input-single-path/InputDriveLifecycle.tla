------------------------- MODULE InputDriveLifecycle -------------------------
EXTENDS Naturals

CONSTANT Enabled
VARIABLE lane

Initial ==
    [actor |-> 1, pending |-> FALSE, adopted |-> TRUE, retired |-> FALSE,
     runner |-> 0, timer |-> 0, requested |-> 0, stale |-> 0,
     ready |-> FALSE, delivered |-> 0, last |-> <<0, 0>>, rebooted |-> FALSE]

Init == lane = Initial

Offer ==
    /\ ~lane.pending /\ lane.delivered < 2 /\ lane.adopted /\ ~lane.retired
    /\ lane' = [lane EXCEPT !.pending = TRUE, !.requested = lane.actor, !.ready = FALSE]

Restart ==
    /\ ~lane.rebooted /\ lane.pending /\ lane.actor < 2
    /\ lane' = [lane EXCEPT !.rebooted = TRUE, !.actor = @ + 1,
                 !.adopted = FALSE, !.timer = 0, !.runner = 0, !.requested = 0]

Adopt ==
    /\ ~lane.adopted
    /\ lane' = [lane EXCEPT !.adopted = TRUE,
                 !.requested = IF lane.pending THEN lane.actor ELSE 0]

Start ==
    /\ lane.runner = 0 /\ lane.requested = lane.actor
    /\ lane.pending /\ lane.adopted /\ ~lane.retired
    /\ lane' = [lane EXCEPT !.runner = lane.actor, !.requested = 0]

Rearm ==
    /\ lane.runner = lane.actor /\ lane.pending /\ ~lane.retired /\ lane.timer = 0
    /\ lane' = [lane EXCEPT !.timer = lane.runner]

HostClears ==
    /\ lane.pending /\ ~lane.ready
    /\ lane' = [lane EXCEPT !.ready = TRUE]

RecheckHeld ==
    /\ lane.timer = lane.actor /\ lane.pending /\ ~lane.ready
    /\ lane' = [lane EXCEPT !.timer = 0]

Recheck ==
    /\ lane.timer = lane.runner /\ lane.runner = lane.actor
    /\ lane.pending /\ ~lane.retired /\ lane.ready
    /\ lane' = [lane EXCEPT !.pending = FALSE, !.timer = 0,
                 !.delivered = @ + 1, !.last = <<lane.runner, lane.actor>>]

Clear ==
    /\ lane.pending
    /\ lane' = [lane EXCEPT !.pending = FALSE, !.timer = 0, !.requested = 0]

Retire ==
    /\ ~lane.pending /\ ~lane.retired /\ lane.adopted /\ lane.actor < 2
    /\ lane' = [lane EXCEPT !.retired = TRUE, !.timer = 0, !.stale = lane.runner]

Successor ==
    /\ lane.retired
    /\ lane' = [lane EXCEPT !.actor = @ + 1, !.retired = FALSE,
                 !.adopted = TRUE, !.pending = TRUE, !.ready = FALSE,
                 !.requested = lane.actor + 1]

Exit ==
    /\ lane.runner # 0
    /\ (~lane.pending \/ lane.retired \/ lane.runner # lane.actor)
    /\ lane' = [lane EXCEPT !.runner = 0, !.timer = 0]

Late ==
    /\ lane.stale # 0
    /\ lane' = [lane EXCEPT !.stale = 0]

UnchangedConfig == ~Enabled /\ lane = Initial
NoTimerWithoutLiveWork == lane.timer # 0 =>
    (lane.pending /\ ~lane.retired /\ lane.runner = lane.actor /\ lane.timer = lane.runner)
NoForeignEffect == lane.last[1] = lane.last[2]

Behavior == Offer \/ Restart \/ Adopt \/ Start \/ Rearm \/ HostClears \/ RecheckHeld \/ Recheck \/ Clear
            \/ Retire \/ Successor \/ Exit \/ Late
Next == Enabled /\ Behavior
Spec == Init /\ [][Next]_lane
=============================================================================
