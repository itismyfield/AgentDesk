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

## S2 placement section (`SpecS2`)

Written against main `b04ea56650e5397989d33f1be199be21a83c8d0e` (S1a and S1b landed) for the S2 design prep (`docs/design/i8118/s2-prep.md`).
`SpecS2` is a separate specification in the same module; the S1 `Spec` keeps the new variables constant, and every S1 config above gives the same result as before.
It models where a thread's new execution under a Herdr parent runs, and what restart, thread_gc, idle cleanup and slot reset do to it.
`S2` is the contract the S2 design must meet; no code implements it yet.

### Modes

- `Main`: `b04ea56650`.
- `S2Naive`: the activation alone. `turn_host` sends a thread without a selector to `configured_turn`; no first launch permit, and boot recovery and idle cleanup stay as in main.
- `S2`: the design contract.
- `MutS2*`: `S2` with one rule removed.

### Entries and transitions

| Entry | Meaning |
|---|---|
| `NewThread` | Herdr parent, new thread: no row, no selector (first human or bot message) |
| `FreshAfterKill` | Herdr parent, legacy row, tmux killed and selector cleared (the 11-26 18:12 launch) |
| `ResumeTmux` | Herdr parent, legacy row, tmux Missing, selector kept (an existing tmux thread) |
| `TmuxParentThread` | Thread under a tmux parent |

| Transition | Code |
|---|---|
| `Launch` | `intake_turn.rs:1940` `for_turn` → `turn_host.rs:200` `resolve`. Main answers Tmux at `:239-242`, then `provider_dispatch.rs:360-362`, `claude.rs:1799` and the tmux guards `tui_session_launch.rs:91-94,106-108` (own boot entry only), and `claude_tui/session.rs:499` logs the launch. Herdr: `herdr_launch.rs:550` Pending row. `S2Naive` without a permit: `turn_host.rs:446-458` `OWriterNotReady` |
| `Create` | `session_host/herdr/launch_host.rs:619` create; the answer may be lost |
| `Resolve` | In process: `herdr_launch/unplaced.rs` via `claude/herdr_turn/pending.rs:108`. After a restart: `recovery_engine/herdr_reader.rs:363`, which reads the channel's own boot entry only, so main and `S2Naive` skip a thread |
| `Rejudge` | A replayed input meets an unresolved Pending row: `turn_host.rs:239-241` `HostedHerdrUnconfigured` |
| `SubmitHerdr` | `claude/herdr_turn/first_input.rs` `submit_owned` |
| `Restart2` | Process restart: rows and panes stay |
| `Idle` | The turn ends and the thread stays idle past an hour (`db/dispatched_sessions/thread_gc.rs:69`) |
| `ReleaseIdle` | `discord/session_idle_cleanup.rs:75-81` skips a Herdr decision; `:87` skips a channel with a queued turn |
| `Gc` | `dispatched_sessions/hosted_cleanup/thread_gc.rs:78-89`, migration 0131 `agentdesk_hosted_execution_deletable` (legacy or retired rows only) |
| `SlotReset` | `auto_queue/runtime.rs:498-507` `host_decision_for_rows`: Bound → Herdr, Pending → Hold, both skipped |

### Rules

| Rule | Meaning |
|---|---|
| `NewThreadFollowsParent` | A new execution under a Herdr parent creates no tmux session, after restart, thread_gc and slot reset included |
| `ExistingTmuxStaysTmux` | A thread with a kept selector stays on tmux: no Herdr pane |
| `TmuxParentStaysTmux` | A thread under a tmux parent creates no Herdr pane |
| `NoInputRefused` | The first input is not refused (no loss) |
| `SingleSubmit` | The first input is submitted once (no duplicate) |
| `NoDuplicatePane` | At most one pane per thread |
| `FirstInputResolves` | Liveness: the first input is eventually placed and submitted |
| `EventuallyReleased` | Liveness: an idle execution is eventually released |
| `SlotRotates` | Liveness: an idle slot thread is eventually reset |

### Results

| cfg | expected | result |
|---|---|---|
| `S2MainFresh` | RED, the 18:12 launch | `NewThreadFollowsParent` in one step: legacy row → tmux launch |
| `S2MainAll` | RED | `NewThreadFollowsParent` |
| `S2NaiveAll` | RED | `NoInputRefused`, `FirstInputResolves`, `EventuallyReleased`, `SlotRotates` |
| `S2All` | GREEN (contract) | GREEN |
| `MutS2NoPermit` | RED | `NoInputRefused`, `FirstInputResolves` |
| `MutS2NoBootResolve` | RED | `NoInputRefused`, `FirstInputResolves` (Pending → restart → replayed input refused, three steps) |
| `MutS2NoRelease` | RED | `EventuallyReleased`, `SlotRotates` |
| `MutS2TabNoMarker` | RED | `NoDuplicatePane`; `FirstInputResolves` comes from the two-pane bound |
| `MutS2SelectorIgnored` | RED | `ExistingTmuxStaysTmux` |
| `MutS2ReplayResubmit` | RED | `SingleSubmit` (a contract mutant: whether a replay path really resubmits is open) |

The chart `thread-host-s2.chart.mmd` draws `S2Replay` (one new thread under the S2 contract, every step of its life) with the view `thread-host-s2.view.mjs`; `S2All` and `S2ExecAll` now exceed the 500-state chart budget.

### Outside the S2 section

- Workspace and tab layout: one pane count stands for placement. The parent-workspace tab is a design question.
- Output protection of the first turn (`source_policy`, the key fence) and placeholder settlement.
- Codex threads, routine process threads (S1r), and consent card lifecycle.
- Whether `FreshAfterKill` should run on Herdr is a design question; the contract follows design delta 4 (no selector → Herdr).

### S2 contract refinement (design-s2-1 §7)

Written against `b52e052e24e13781569050ec940eb35b9496ed54` (prod equal to `b04ea56650`; `origin/main` `c28732c52c` does not touch these files).
The pro design (`design-s2-1`, built on `refs/review/i8118-s2-prep-1`) asked for six refinements; each is a transition or variable with a code coordinate.
The configs of §7.1 keep their results; the new entries and rules live in new configs only.

| Refinement | Model | Code |
|---|---|---|
| Execution facts | Entries `LiveTmuxNoSel` (legacy row, tmux Present, no selector), `SelUnknown` (row read fails), `RetiredNative` (own Retired Herdr row); states `Unread`, `Placed` | `host_decision.rs:283-289` presence; `turn_host.rs:233-238` row read refusal; `turn_host.rs:94-104` a Retired trace is no Herdr trace, so main answers Tmux at `:242` |
| turn_host has no tmux probe | `LiveTmuxSeenDead`: activation alone treats a live tmux without a selector as dead | `turn_host.rs:200-252` (only `return_plan`'s marker at `:133`) |
| Fresh intent vs permit | `late`: an ordinary fresh adoption waits in the owner's WRITER_HELD hold and goes on at `OrdinaryReady`; a late child needs the permit | `channel_policy/children.rs:205-212`; `tui_o/cutover/claude_thread.rs:14-22`; `provider_dispatch/owner_input.rs:192-224`; `tui_o/writer/host/first_init.rs:282` |
| Unsent successor | `unsent`: an unplaced run retired before its create lets the next nonce take a permit | `herdr_launch.rs:684-715` (design Q3) |
| Resolve split | `Resolve` adopts to `Placed`; `Bind` binds after own SessionStart and source | `herdr_launch/unplaced.rs:229-311`; `tui_prompt_relay/herdr_source.rs:185,267` |
| Current seed vs plan | `judged`: set only at `Bind`, cleared at release and restart (S2 seeds a Bound row at boot) | `thread_host.rs:87-97`; `turn_host.rs:165,171-187`; design C9 `runtime_bootstrap.rs:302-311` |
| Slot reads the release state | A Retired row is reset whatever its next host | `auto_queue/runtime.rs:498-507,553-595`, `host_teardown_gate.rs:223-251` (main admits a confirmed legacy row only) |
| Replay unknown | `receipt`: a restart between the key and the receipt keeps the input Unconfirmed; `Settle` | `queued_head/v2/retained.rs:77,122-146` |

New rules: `LiveTmuxStaysTmux`, `RetiredFollowsHerdr`, `SeedMatchesBound`; liveness `FirstInputClassified`, `FirstInputSettles`.

| cfg | expected | result |
|---|---|---|
| `S2ExecMain` (Main; LiveTmuxNoSel, SelUnknown, RetiredNative) | RED | `RetiredFollowsHerdr` (1 step: own Retired row → tmux launch), `NoInputRefused` (1 step: unreadable row refused), `FirstInputResolves` |
| `S2ExecNaive` (activation alone, seven entries) | RED | `LiveTmuxStaysTmux` (1 step), `NoInputRefused`, `SeedMatchesBound` (a late refusal recorded as Herdr), `FirstInputResolves`, `EventuallyReleased`, `SlotRotates` |
| `S2ExecAll` (S2, seven entries) | GREEN | GREEN |
| `MutS2ProbeSkipped` | RED | `LiveTmuxStaysTmux` |
| `MutS2NoFreshSuccessor` | RED | `FirstInputResolves` (Pending → restart → retired unsent → no permit) |
| `MutS2SeedOnAdopt` | RED | `SeedMatchesBound` |
| `MutS2PlanSeeds` | RED | `SeedMatchesBound` |
| `MutS2IgnoreVerified` | RED | `SeedMatchesBound` |
| `MutS2SlotFuturePolicy` | RED | `SlotRotates` (released, then gc'd, still asked its next host) |
| `S2Replay` (S2, NewThread) | RED on the claim only | `FirstInputSettles` (restart between key and receipt leaves Unconfirmed); `FirstInputClassified`, `SingleSubmit`, `NoInputRefused` GREEN |

Counterexamples checked against code: `S2ExecMain` both 1-step paths (`turn_host.rs:233-238`; `turn_host.rs:224,236,98-99,242`). `S2ExecNaive` `LiveTmuxStaysTmux` rests on the absence of a probe in `turn_host.rs:200-252`. Whether a refused input is dropped or returned to its author is outside the model.
The contract assumptions behind the liveness rules are a local endpoint, a placeable parent, an O writer that recovers, fair workers and terminate permission on (design §7.3).

### Design w2 (review-design-s2-1 P1 1, 2, 3, 5, 7)

Written against `b52e052e24e13781569050ec940eb35b9496ed54`.
The independent review of design w1 found eight P1 defects; the model takes the five on the first turn, the boot order and the restart seed.
Mode `S2` is now the w2 contract; each w1 defect is a `MutW1*` mutant.
The new entries and rules live in new configs only, and every earlier config keeps its result.

| P1 | Model | Code |
|---|---|---|
| 1 first key before Bind | `Reserve` then `SubmitHerdr` from `Placed`; `Bind` needs the key (`hkey`). `MutW1BindFirst` keys only a Bound pane | `claude/herdr_turn/first_input.rs:534-601` (bind after the transcript's first record), `claude/herdr_turn.rs:752-777`, `tui_prompt_relay/herdr_source.rs:177-190` |
| 2 exact retry reservation | Entry `RetryNew`; the reservation is required in the common write. `MutW1RetrySkipsReserve` keys without it | `intake_turn/worker_entry.rs:182`, `queue_io/input_owner/ingress.rs:415-417`, `claude/herdr_turn.rs:770` |
| 3 default dispatch reset | Entry `DispatchNew`; `DispatchReset` reads a confirmed vacant child's reset as fresh intent. `MutW1ResetRefuses` (and activation alone) refuse it and end the turn | `src/dispatch/dispatch_context.rs:205-212`, `intake_turn.rs:867-888`, `admin_host_guard.rs:96-142` |
| 5 boot gate and stale Bound | Entry `StaleBound` (a Bound row whose pane is gone, right after a restart) and one other channel's work. `BootNoGate` (main), `BootRetireDead` (w2), `RelayRetire`, `OtherServe`. `MutW1BootGateBlocks` waits on the Bound forever | `discord/runtime_bootstrap.rs:298-340`, `tui_prompt_relay/rehydration.rs:270-274`, `recovery_engine/herdr_reader.rs:436-470` |
| 7 restart seed | Entry `RestartVacant`; `CmdRestartVacant` adopts the V2 owner then seeds. `MutW1RestartNoSeed` has no owner, so `Seed::of` fails | `commands/restart/herdr.rs:279-310`, `queue_io/input_owner/restart_seed.rs:56-58` |

New rules: `KeyAfterReservation` (no first key before its output reservation) and liveness `BootServesOthers` (a stale Bound child never stops other channels' work at boot).

| cfg | expected | result |
|---|---|---|
| `S2W2All` (S2; NewThread, RetryNew, DispatchNew, RestartVacant) | GREEN | GREEN |
| `S2W2Main` (Main; RetryNew, DispatchNew, RestartVacant) | RED | `NewThreadFollowsParent` |
| `S2W2Boot` (S2; StaleBound) | GREEN | GREEN |
| `S2W2BootMain` (Main; StaleBound) | GREEN (control: main has no seed gate) | GREEN |
| `MutW1BindFirst` | RED | `FirstInputResolves` (stuck at `Placed`: Bind waits for the key, the key waits for Bound) |
| `MutW1RetrySkipsReserve` | RED | `KeyAfterReservation` (3 steps) |
| `MutW1ResetRefuses` | RED | `NoInputRefused` (1 step), `FirstInputResolves` |
| `MutW1BootGateBlocks` | RED | `BootServesOthers` (0 steps: the gate waits forever) |
| `MutW1RestartNoSeed` | RED | `FirstInputResolves` |

Each w1 counterexample was checked against the code coordinates in the table above. The other three P1 (4 command unsent after a tab create, 6 slot archive before settlement, 8 per-provider current execution) are covered by production tests in the design, not by this model.

### Design w3 (review-design-s2-2 P1 1, 2)

Written against `b52e052e24e13781569050ec940eb35b9496ed54`.
The second design review left two P1 open and called them outside the model; both fit as a small extension.
Mode `S2` is now the w3 contract; each w2 gap is a `MutW3*` mutant.
The new entries and rules live in new configs only, and every earlier config keeps its result (56/56 configs as expected).

| P1 | Model | Code |
|---|---|---|
| 1 placed Pending whose command never ran | Entry `LostCommand`: an answered create may leave `ran = FALSE` once (a single failed command input); `SubmitHerdr` needs `ran` (attach needs the pane's own logged start). `PlacedResidue` is the shared unplaced residue rule applied to a placed Pending: the shell is closed and the row retired, then the same original goes on in a fresh successor. Entry `LostCommandHeld`: the residue is touched, so it is held | `session_host/herdr/launch_host.rs:645-655` (`CreatedUnconfirmed` for the command input), `herdr_launch.rs:645-650`, `claude/herdr_turn/pending.rs:31-34` (`PendingUnproven`), `herdr_launch/unplaced.rs:173-203` (exact marker, root, pristine), `herdr_launch.rs:695-715` (`retire_own_pending` with the placed location) |
| 2 slot archive after the map moved on | Entry `SlotSuccessor`: after release and slot reset, `Successor` starts a new execution in the same thread. `ArchiveThread` (only `MutW3ArchiveAfterRelease`, design w2's order) archives the thread ID afterwards; w3 never archives a thread released from Herdr | `auto_queue/runtime.rs:387-445` (archive by thread ID, `:423-427` optional Discord side), `intake_turn/adk_thread.rs:154-205` (`active_thread_id` reuse), `router/thread_binding.rs:77-110` (unarchive on reuse) |

New rules: `ResidueNeverRan` (a residue close never ends a pane whose command ran), `TouchedResidueHeld` (a touched shell is never closed), `ArchiveSparesLive` (a release's archive never lands on a live successor).

| cfg | expected | result |
|---|---|---|
| `S2W3Lost` (S2; LostCommand) | GREEN | GREEN |
| `S2W3Held` (S2; LostCommandHeld, safety only: a held residue claims no progress) | GREEN | GREEN |
| `S2W3Slot` (S2; SlotSuccessor) | GREEN | GREEN |
| `S2W3All` (S2; NewThread, RetryNew, DispatchNew, RestartVacant and the three w3 entries, safety) | GREEN | GREEN |
| `MutW3NoPlacedResidue` | RED | `FirstInputResolves` (stuck at a placed pane whose command never ran) |
| `MutW3ResidueByAbsence` | RED | `ResidueNeverRan` (a cold Claude has no transcript before its first key, so the guess closes a running pane), `FirstInputResolves` (every successor is closed the same way) |
| `MutW3NoPristine` | RED | `TouchedResidueHeld` (2 steps after the create) |
| `MutW3ArchiveAfterRelease` | RED | `ArchiveSparesLive` (release, slot reset, successor, archive) |

The model still does not prove what the review's P2-2 lists: the reservation's exact owner and nonce (`reserved` is a bool), Bind after the first submit with the O output done, settlement before release, and a second provider's execution in the same thread. Those stay with the production tests T-S2-1, T-S2-8, T-S2-19 and T-S2-21 of the design.

### Design w4 (review-design-s2-3 P1 1, P2 2)

Written against `b52e052e24e13781569050ec940eb35b9496ed54`.
The third design review found that design w3 left the retire of a placed Pending whose pane is gone to the relay reconnect, which returns early for every Pending.
The model now splits w3's atomic `PlacedResidue` into `ResidueClose` and `PlacedMissingRetire`, so a restart or a lost close answer can fall between them; mode `S2` is the w4 contract and each w3 gap is a `MutW4*` mutant.
New entries live in new configs only; every earlier config keeps its result (62/62 configs as expected).

| Step | Model | Code |
|---|---|---|
| close | `ResidueClose`: the pane goes, the row stays the placed Pending (`gone`, `goneGen`) | `herdr_launch/unplaced.rs:367-377` (`dispose_residue`), `:173-203` (`judge`) |
| restart or lost answer between | `Restart2` or no step: the same state | design s2-design.md "P1-1 contract" |
| retire | `PlacedMissingRetire`: only the generation whose pane was seen gone, only from `Placed` | read rule `session_host/herdr_gate.rs:856-876` (snapshot presence `Missing` on one witness), write `herdr_launch.rs:695-715` (`retire_own_pending` exact CAS) |
| why w3 missed it | `MutW4PendingEarlyReturn` has no such retire | `recovery_engine/herdr_reader.rs:425-427` returns for a Pending before the Missing retire at `:449-458` |
| pane already gone | `PaneVanishes` (entry `LostCommand`, once) | — |
| payload unknown | entry `LostAfterKey`: the pane goes after the first key; the retire keeps the input submitted, `unsent` false | `turn_orchestrator/queued_head/v2/retained.rs:65-83,339-352` (Unconfirmed is never requeued) |
| P2-2 | entry `LostCommandShellUsed`: a used shell that is a lone root shell again is disposed; `LostCommandHeld` now means an observable change (another pane or tab, another occupant) | `session_host/herdr/launch_host.rs:827-841` (pristine is the workspace layout) |

Execution generations saturate at 3 so the relaunch loops of the mutants stay finite; contract paths use at most 2.
New rule: `RetireOwnGeneration` (a Missing retire ends only the generation whose pane it saw gone).

| cfg | expected | result |
|---|---|---|
| `S2W4Lost` (S2; LostCommand, LostAfterKey, LostCommandShellUsed) | GREEN | GREEN |
| `S2W4All` (S2; all w2, w3 and w4 entries, safety) | GREEN | GREEN |
| `MutW4PendingEarlyReturn` (design w3) | RED | `FirstInputResolves` (pane gone, row Pending forever) |
| `MutW4MissingNoCas` | RED | `RetireOwnGeneration`, `NoDuplicatePane` (the successor's pane is orphaned), `FirstInputResolves` |
| `MutW4UnreadAsMissing` | RED | `NoDuplicatePane` (a present pane read as gone, then a second create) |
| `MutW4MissingResubmits` | RED | `SingleSubmit` (Missing taken as a resend permit) |

The model does not separate a list failure from a witness change: both are "no Missing seen", the hold `MutW4UnreadAsMissing` breaks. The production test T-S2-21 names each.
