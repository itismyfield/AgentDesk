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

`NoBusyMachineWrite` forbids a machine write during Busy. `OnlyOwnActorReleased` forbids releasing a replacement token. `UnprovenHeld` covers a missing witness, unreadable or foreign row, and missing host proof. `IdleFrontEventuallyDelivered` requires an eligible front to advance after the original terminal has proven closure. Liveness assumes the host stays idle, normal finalizer/owner scheduling and successful receipt settlement.

| Configuration | Expected state chart result |
| --- | --- |
| Before | RED. The stopped host and absent projection leave the mailbox held. |
| After | GREEN. The pinned episode settles and its queue-eligible event advances the front. |
| Busy, Unknown, Corrupt, ForeignRow, Unpinned, Successor | GREEN. The unsupported actor releases are unreachable. |
| NoActorGuard | RED. A late terminal releases a successor. |

This projection covers the same-process native primary path. It models the existing owner wake following settlement, and uses a closed, source-verified terminal as its readiness input. Restart custody, source changes, and drives woken before eligibility have separate code paths.
