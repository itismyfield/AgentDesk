# Thread host decision consumers, S1b (#8118)

Model `ThreadHostS1b.tla`, written against `2668bdeefd` (S1a head) plus the S1b change.
It covers what S1b changes: which host a command, a cleanup or an input consent acts on, and what a thread's recorded execution keeps after a refused turn.
It also models two things the S1a model did not separate (S1a review P2-1): what each synchronous reader *does* on `Unknown`, and the restart/restore boundary of the record.
Placement, launch, the first input and Herdr fresh threads are S2 and are not modelled.

## Modes

- `S1bFirst`: the first S1b head `9e28c137ae`. The key holders A2/A6/A9 derived a token from the key (`token_hash_of`), and A9 read provider and key only from the slot's `thread_channel_id` row.
- `Before`: the S1a head. A1-A9, `#24` and the consent check read the channel's own boot entry; a refused turn forgets the record.
- `Rev4`: design delta 4 taken literally. One `Option<String>` read from the process record by channel only; a thread's input always asks its parent's consent.
- `After`: the S1b code (`session_host/host_decision.rs`, `turn_host.rs` `recorded_execution`).
- `S2Seed`: the restore contract the activation slice must meet: boot restore seeds the record of a Bound thread and readers wait for it.
- `Mut*`: `After` (or `S2Seed` for `MutSeedLate`) with one rule removed.

## Entries and transitions

| Entry | Meaning |
|---|---|
| `BootHerdr` | Channel with its own boot Herdr entry |
| `BootTmux` | Channel no boot entry names |
| `LiveTmux` | Thread under a Herdr parent, legacy row, tmux Present |
| `DeadTmux` | Thread under a Herdr parent, legacy row, tmux Missing |
| `ProbeFail` | Thread under a Herdr parent, legacy row, tmux probe failed |
| `HerdrBound` | Thread under a Herdr parent, Bound row at the parent's endpoint (no production path before S2) |
| `Unresolved` | Thread whose parent the boot view holds unresolved |
| `SlotGc` | Slot thread under a Herdr parent whose `sessions` rows `thread_gc` deleted (`db/dispatched_sessions/thread_gc.rs:92-118`); `thread_id_map` still names it |
| `LegacyKey` | Thread under a Herdr parent, live tmux, a legacy `host:tmux` key that `SessionIdentity::parse` reads with no token (`domain/session_identity.rs:59-64`) |

`SlotGc` and `LegacyKey` come from the key holders in the entry table: A2 `session_resume.rs:408`, A6 `inflight/host_recovery_guard.rs:105`, A9 `auto_queue/runtime.rs:487`. At `9e28c137ae` the `token_hash_of` callers were `session_resume.rs:406`, `host_recovery_guard.rs:104` and `auto_queue/runtime.rs:552`.

| Transition | Code |
|---|---|
| `CmdRestart` | `discord/commands/restart.rs` `restart_channel` (A1, matches all three decisions) |
| `CmdTmuxOp` (key holders) | `S1bFirst`: `auto_queue/runtime.rs:552` gave `token_hash_of(None)`, then `host_decision.rs:119` `ProviderUnknown` (SlotGc); a legacy key gave no token, then `host_decision.rs:152` "no database or token" (LegacyKey). After: `host_decision.rs:105-117` reads every row of the thread, and none at all means `:203` Nothing, so Tmux; `:86` uses the key in hand, `:250` finds the row by key, and the tmux probe gives Tmux. The slot binding is released at `auto_queue/runtime.rs:380` |
| `CmdTmuxOp` | A2 `session_resume.rs`, A3/A4 `admin_host_guard.rs`, A5 `session_locator.rs`, A6 `inflight/host_recovery_guard.rs`, A7 `session_idle_cleanup.rs`, A8 `tmux_watcher/host_gate.rs`, A9 `auto_queue/runtime.rs`: `HostDecision::tmux_refusal` |
| `CmdRelaunch` | `commands/config/provider_settings.rs` `claude_local_tui_relaunch_applies` via R1-R4 |
| `Input` | `router/herdr_consent/admission.rs` `check` (W2) |
| `Turn` | `turn_host.rs` `for_turn` → `recorded_execution` → `thread_host::record_judged` |
| `Restart` | `thread_host.rs` `JUDGED` is process memory |
| `BootRestore` | S2 contract only (`recovery_engine/herdr_reader.rs` Bound restore) |
| `SyncRead` | S1a readers `#15` `turn_mode_tmux`, `#43` `nameless_gate`, `#45` `defer_owned_busy_view`, as behaviour: recorded Herdr takes the Herdr path, anything else keeps the tmux path |

## Rules

| Rule | Meaning |
|---|---|
| `UnknownRunsNoHost` | An undecided execution starts no host, changes no tmux session and takes no relaunch |
| `TmuxThreadNoHerdrOp` | A tmux thread is never restarted on Herdr |
| `TmuxThreadTmuxOpsAdmitted` | A tmux thread's tmux operations are not refused |
| `LiveTmuxRelaunchApplies` | A live tmux thread takes its relaunch, right after a restart included |
| `HerdrNoTmuxOp` | A Herdr execution is never restarted or changed as tmux |
| `BootUnchanged` | Own-boot and plain channels act as before |
| `TmuxInputNotConsentHeld` | A tmux thread's input never waits on its parent's consent |
| `HerdrInputUnderConsent` | A Herdr thread's input is asked under its parent's consent |
| `S1bChildInputKeepsMain` | Before activation, every thread input's consent result is the S1a head's |
| `TmuxThreadKeepsTmuxPath` | P2-1: a live tmux thread's synchronous readers keep the tmux path on `Unknown` |
| `RefusalKeepsLivePane` | P2-4: only a restart before the next turn may leave a live Herdr pane read as tmux |
| `SlotRotationProceeds` | P1-1: a GC'd slot thread and a legacy-keyed thread reach the row checks; the slot binding is not refused forever |
| `RestoredBeforeRead` | P2-1: a live Herdr pane is never read as tmux, a restart included (needs the S2 seed) |
| `CommandsAnswer` | Every command and the input are answered (liveness) |

## Results

| cfg | expected | result |
|---|---|---|
| `BeforeAll` | RED | `HerdrInputUnderConsent`, `HerdrNoTmuxOp`, `RefusalKeepsLivePane`, `RestoredBeforeRead`, `UnknownRunsNoHost` |
| `BeforeS1b` (no `HerdrBound`) | RED, intended change only | `UnknownRunsNoHost` |
| `BeforeNoProbe` | GREEN (control) | GREEN: without undecided threads S1b equals the S1a head |
| `S1bFirstAll` | RED (reviewed head) | `SlotRotationProceeds`, plus the known gap `RestoredBeforeRead` |
| `MutRowlessHold` | RED | `SlotRotationProceeds` (rows read, none: `ProviderUnknown` again) |
| `MutKeyDropped` | RED | `SlotRotationProceeds` (key ignored, token derived from it) |
| `Rev4All` | RED | `LiveTmuxRelaunchApplies`, `RestoredBeforeRead`, `S1bChildInputKeepsMain`, `SlotRotationProceeds`, `TmuxInputNotConsentHeld`, `TmuxThreadNoHerdrOp`, `TmuxThreadTmuxOpsAdmitted`, `UnknownRunsNoHost` |
| `Rev4S1b` | RED | the same less `RestoredBeforeRead` |

`SlotGc` and `LegacyKey` are in every cfg's entries; `Before`, `After`, `S2Seed` and the older mutants answer them as before (GREEN on `SlotRotationProceeds`).
| `AfterS1b` | GREEN | GREEN |
| `AfterAll` | RED, known gap | `RestoredBeforeRead` only |
| `S2SeedAll` | GREEN (S2 contract) | GREEN |
| `MutSeedLate` | RED | `RestoredBeforeRead` (seed present, readers not gated on it) |
| `MutRestartSome` | RED | `UnknownRunsNoHost` |
| `MutCacheOnly` | RED | `HerdrInputUnderConsent`, `LiveTmuxRelaunchApplies`, `TmuxThreadTmuxOpsAdmitted`, `UnknownRunsNoHost` |
| `MutConsentAlways` | RED | `S1bChildInputKeepsMain`, `TmuxInputNotConsentHeld` |
| `MutConsentSkip` | RED | `HerdrInputUnderConsent` |
| `MutNoStageGate` | RED | `S1bChildInputKeepsMain`, `TmuxInputNotConsentHeld`, `TmuxThreadNoHerdrOp`, `TmuxThreadTmuxOpsAdmitted` |
| `MutRefusedForgets` | RED | `RefusalKeepsLivePane` |
| `MutUnknownSkipsTmux` | RED | `TmuxThreadKeepsTmuxPath` |

Mutant cfgs leave `RestoredBeforeRead` out (it is the known gap of every non-seeded mode) so each RED belongs to its own mutant; `MutSeedLate` keeps it.

## Outside the model

- Downstream row checks (`keyed_teardown`, `slot_thread_host_refusal`, `resume_host_refusal`): `Done` means only that the tmux path was entered.
- Codex threads, selectors and Herdr fresh threads (S2 `next_host`), the consent card lifecycle (S2 P1-C).
- Each consumer's code beyond its decision is covered by unit tests, not by this model.
