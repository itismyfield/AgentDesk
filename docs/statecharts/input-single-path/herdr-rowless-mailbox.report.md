# Herdr terminal with a missing inflight projection

Code baseline `9243b528dd50f4ca4ae4e75442afdc8f6bc2227a`. The Rust reproduction enters through the native writer and finalizer and observes the exact mailbox and queue-eligible event.

| Entry point | Model transition and evidence |
| --- | --- |
| `turn_orchestrator.rs:2020,2059,2092` | The mailbox actor installs, restores, or adopts the owning token. |
| `turn_finalizer/native_prepare.rs:375–405` | Native registration confirms the same actor before the first key. The repaired path also captures that episode's current projection. |
| `tmux_watcher/terminal_commit_epilogue.rs:319–334` and `tmux_restart_handoff.rs:430–452` | Projection removal and actor release are separate guarded operations. The model allows a removal between native registration and terminal admission. |
| `turn_presence/activity.rs:38–44` and `turn_finalizer/terminal_handler.rs:488–538` | The host supplies Busy, Idle, or Unknown. Native admission requires its own source and terminal proof. |
| `turn_finalizer/terminal_handler.rs:589–629,710–805` | Admission checks the bound source. A verified terminal settles only its own actor, acknowledges cleanup and publishes queue eligibility. Before the repair, an absent row prevented even admitting the terminal. |
| `router/message_handler/provider_dispatch/owner_input.rs:249–262,770–820` | A machine front needs an empty mailbox and the existing start permit. |

`NoBusyMachineWrite` forbids a machine write during Busy. `OnlyOwnActorReleased` forbids releasing a replacement token. Registration pins actor A and an exact terminal carries that actor independently of the live mailbox. The successor fixture installs B before A's late proof, so the same settlement transition evaluates the actor check for both actors.

The model keeps the terminal dispositions separate. Executed and Cancelled can settle an absent projection with the prepared witness. Withdrawn and Transferred keep the original actor held in this projection. For an idle eligible front, `IdleFrontEventuallyDelivered` requires an eventual send. Each boundary config separately requires a registered Busy, observed Unknown, or reached terminal state, along with the safety rules. Fair scheduling models the existing owner wake and assumes successful receipt settlement.

| Configuration | TLC result |
| --- | --- |
| Before | RED `IdleFrontEventuallyDelivered`. No pin survives the projection loss. |
| After | GREEN. An Executed terminal releases its actor and the front sends. |
| CancelledW5 | RED `IdleFrontEventuallyDelivered`. The pin exists and the disposition blocks settlement. |
| Cancelled | GREEN. A proven interrupted terminal releases its actor and the front sends. |
| Busy, Unknown, Corrupt, ForeignRow, Unpinned, Successor, Withdrawn, Transferred | GREEN. Each config reaches its boundary condition and preserves its held actor. |
| NoActorGuard | RED `OnlyOwnActorReleased`. A's Cancelled proof releases B. |
| NoDispositionGuard | RED `UnprovenHeld`. A transfer without own-run proof releases the actor. |

TLC 1.7.4 matched all 14 expectations. The model covers same-process native primary settlement with the existing owner wake. Restart custody, source changes and PG receipt failure have separate paths.
