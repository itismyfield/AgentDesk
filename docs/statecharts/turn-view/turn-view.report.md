# Busy input presentation

Code baseline: `5bcbb82297f5cc3ab8897b81ddb9891459e0321b`. The repair shares the busy-result predicate, handles pre-injection returns, and reads pending source membership from the V2 owner when present.
The baseline index and #8007 commit `e6fda3f69` have no turn-view model (#8007 explicitly says state chart not applicable). This small model records its EmptyMailbox branch and adds BusyActiveTurn with the same eligibility predicate.

The change affects presentation. Queue admission, ownership, injection, handback, recovery and delivery transitions stay unchanged. `eligible` abstracts an owned human input to gated Claude without a Herdr endpoint. `QueueMembershipVisible` records whether the pending-reaction adapter reads that source from its actual V2 owner instead of the empty V1 list. Noneligible input keeps the queue presentation. `Restart` preserves the durable accepted input; fairness assumes the existing owner/recovery drive eventually settles it. It does not prove real I/O availability or Discord delivery.

The rules check exactly one busy result, one visible settled result, pending presentation on closed gates, durable accepted custody, and eventual visible settlement. Accepted input can be temporarily unmarked while the owner decides the result, as in #8007.

`EarlyHold` models the confirmed baseline pre-injection return at `provider_dispatch/owner_input.rs:219-237` (session transition) and `:417-424` (payload preparation). `BusyActiveNoFallback` models suppressing the admission marker without repairing these returns. It breaks OneBusyResult, NoInvisibleSettled and AcceptedEventuallyVisible. `BusyActiveFixed` models the complete repair with this fallback and correct queue membership. EmptyMailbox excludes this newly investigated early-return case and does not claim to repair #8007's documented attachment gap.

The live tests confirmed a second baseline defect: `queue_effects.rs:65-72` removes a marker based on `snapshot.intervention_queue`, while `episode_identity.rs:448-449` keeps the empty V1 queue separate from `v2_queue`. The test keeps the actual V2 input queued with its owner drive paused, yet sees a mailbox add followed by a remove. `LegacySweep` models this false cleanup without changing queue custody. `QueueMembershipBefore` breaks NoInvisibleSettled, ClosedGateKeepsPending and AcceptedEventuallyVisible. The other configurations isolate the earlier busy-result and pre-injection-return rules with correct membership assumed.

TLC via `python3 scripts/ci/statecharts_tlc.py --root <isolated model copy> --jar /home/adk/.local/share/tla2tools/tla2tools-1.7.4.jar --java /usr/bin/java`: 5/5 configurations matched expectations in 16.4 seconds, 15 TLC runs plus 5 fairness prechecks. BusyActiveBefore breaks OneBusyResult; BusyActiveNoFallback breaks the three recorded early-return rules; QueueMembershipBefore breaks the three membership rules above. EmptyMailbox and BusyActiveFixed pass. Raw log: `/home/adk/adk/lane-runs/i8113/logs/statechart-final.log`. Modeling remains within the brief's 20-minute limit.

The chart is a presentation projection written directly from these actions. The Rust tests are separate evidence: all 20 single-marker cases pass, including the original 18 and the held-follower/reserved-source cases. The queue transaction, adapter, reconciler and adoption-gate suites also pass (13, 19, 63 and 3 cases). No delivery, ownership or queue transition is changed by the display repair.


## Held front and second input

The additional `HeldFrontFollower.tla` models two distinct originals. A is handed back by a permission modal (`FrontHeld = TRUE`); B is accepted behind A and does not merge. Retrying A never forces B to settle. There is no fairness premise that finishes B while the modal remains held.

`HeldFrontFollowerBefore.cfg` maps the channel-owner-only deferral to `FirstOnly = FALSE`: HeldFollowerVisible and FollowerEventuallyVisible fail. `HeldFrontFollower.cfg` maps the empty-items/no-head admission check to `FirstOnly = TRUE`: all rules hold. Both keep the same durable queue and ownership transitions; only B's admission marker differs. This does not prove that a modal eventually closes or that an idle drive eventually delivers.

Final TLC run: seven configurations match their expectations in 20.0 seconds. The two-input before/fixed cases take 2.9/2.1 seconds respectively. Raw log: `/home/adk/adk/lane-runs/i8113/logs/impl4/statechart.log`. Modeling finished within 30 minutes. The earlier single-input model remains scoped to one candidate's result and is not used as proof of follower visibility.

Reserved-attempt membership and late HTTP reaction operations are checked by the Rust adapter case; these models do not include a reserved phase or a network interleaving.
