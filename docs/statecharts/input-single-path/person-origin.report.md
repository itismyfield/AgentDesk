# Person-origin admission and queue selection

Base: `96a4c4b65776ebf6b2bfc7fd4ce1afbbf31cba3b`; the change adds person-origin selection on this A1 head.

`src` distinguishes Discord and iMessage people from HTTP/routine/scheduled machine starts.
`StartOwnTurn` applies to machine starts. Historical configs retain their previous source domain and expected violations.

| Config | Contract | Expected result |
|---|---|---|
| InputOwnershipPersonBusy | An iMessage handed to a running turn pastes exactly once into that turn | Pass |
| PersonOriginQueue | The earliest person overtakes waiting machine starts; queued binds move with it; people keep their order; the machine gets its own idle turn | Pass |
| PersonOriginQueueSplitBinds | Move the queue without moving its ticket binds | AtomicBinds violation |

All 66 configs using InputOwnership or PersonOriginQueue were rerun with TLC 1.7.4 and matched expected.tsv.
The new configs check safety of the selected transitions. They make no new fairness or liveness claim.
The real owner-drive tests separately exercise the actor, disk commits, failed publish, restart, busy transaction and HTTP admission.
