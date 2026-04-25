# Recovery Paths Contract

Source-of-truth for the three recovery paths that resurrect an inflight turn
after tmux, watcher, or dcserver disruption. Tracks issue **#1074** (905-5)
and absorbs issue #917.

`src/services/discord/recovery_engine.rs` (~4.6k LOC) currently hosts all
three paths in one file. The goal of #1074 is to establish the per-path
contract and SSoT for shared helpers **first**, then split the file
mechanically in follow-up work. This document is the contract.

## Path Inventory

| Path             | Entry point (fn)                                    | Trigger                                                   | Sync / Async |
|------------------|-----------------------------------------------------|-----------------------------------------------------------|--------------|
| restart          | `recovery_engine::restore_inflight_turns`           | dcserver startup, after DB/runtime init                   | async        |
| runtime          | `recovery_engine::reregister_active_turn_from_inflight` | mid-execution: mailbox rediscovers inflight file it does not own | async |
| manual_rebind    | `recovery_engine::rebind_inflight_for_channel`      | operator POST `/api/inflight/rebind` or `/rebind` slash    | async        |

Future layout (deferred — scaffold exists at `src/services/discord/recovery_paths/`;
the `recovery_paths` name avoids shadowing the existing `recovery_engine as recovery`
alias until the mechanical split lands):
- `recovery_paths/restart.rs`
- `recovery_paths/runtime.rs`
- `recovery_paths/manual_rebind.rs`
- `recovery_paths/shared.rs`  ← SSoT helpers (currently: `clear_inflight_by_tmux_name` wrapper)

## Entry Conditions

### restart
- `dcserver` has just booted and the runtime root has been resolved.
- DB is reachable; `ChannelMailboxRegistry::global()` is initialised.
- For each persisted inflight file under `$RUNTIME_ROOT/runtime/discord_inflight/<provider>/*.json`:
  - Provider binary is resolvable via `binary_resolver`.
  - Restart report flush has completed (see `restart_report::flush_restart_reports`).

### runtime
- A turn is already running (watcher registry has a slot), **and**
- An inflight file is discovered that does not match the current watcher
  generation (hot-swap, rebind-in-progress, late restart cleanup).
- The caller holds the mailbox handle for the target channel.

### manual_rebind
- HTTP 200 after auth/authorization check.
- JSON body contains a valid `channel_id` + target `tmux_session_name`.
- The channel mailbox exists and is not in `Shutdown` state.

## Side Effects (per path)

| Effect                         | restart | runtime | manual_rebind |
|--------------------------------|---------|---------|---------------|
| Deletes inflight file          | yes (on recovery terminal) | yes (on replace) | yes (on rebind success) |
| Kills tmux session             | no      | no      | no            |
| Writes handoff file            | yes (interrupted path)     | yes (interrupted path)   | no            |
| Inserts DB turn row            | yes (completion or handoff)| yes (completion)         | no            |
| Emits `recovery_fired` metric  | yes     | yes     | yes           |
| Mutates channel mailbox state  | yes     | yes     | yes           |
| Spawns new watcher             | yes     | yes     | yes           |

## Common Finalizer Shape

All three paths funnel into the mailbox "finish turn" sequence. The common
contract for the finalizer:

1. Transition mailbox state to `FinishingTurn` (exclusive).
2. Persist final DB row (`turns`) with recovered response + token usage.
3. Delete inflight file via the **SSoT** `clear_inflight_by_tmux_name`
   (or the per-channel `clear_inflight_state`).
4. Clear watchdog deadline override.
5. Emit `turn_completed` / `recovery_fired` observability events.
6. Decrement global active counter; check deferred restart.

Normal completion, explicit cancel, recovery, and watchdog-timeout all flow
through the same `finish_recovered_turn_mailbox` / `stop_turn_*` helper
chain. This is the invariant #1074 protects.

## Session Identity

All three paths parse the same session-key shapes. Parsing is centralised in
`src/services/discord/session_identity.rs` (introduced under #1074). Forms:

- legacy: `<host>:<tmux_name>`
- namespaced: `<provider>/<token_hash>/<host>:<tmux_name>`

Existing scattered `split_once(':')` call sites
(`services/queue.rs`, `server/routes/session_activity.rs`, etc.) continue to
compile — they are slated for migration in follow-up cleanups.

## Inflight Cleanup SSoT

**Canonical owner**: `src/services/discord/inflight.rs`
(`pub(super) fn clear_inflight_by_tmux_name`).

Public wrappers:

- `src/services/discord/mod.rs` — re-exports as `pub(crate) fn`.
- `src/services/turn_lifecycle.rs` — thin wrapper that forwards to the discord
  wrapper (no private directory scan).
- `src/services/discord/recovery_paths/shared.rs` — forwards to the discord
  wrapper for eventual use by
  `recovery_paths::{restart,runtime,manual_rebind}`.

Invariant: **exactly one** `std::fs::remove_file` call graph for inflight JSON
files keyed by tmux session. If you add a new caller, route through the
discord or recovery-shared wrapper. Do not re-implement the directory scan.

## Discord Adapter Boundary

Discord adapter layer (`router/`, `gateway.rs`, `discord_io.rs`) MUST NOT
mutate lifecycle state (mailbox state, inflight files, watcher registry
slots) directly. Those mutations belong to:

- the orchestrator/health layer, and
- the recovery paths (restart / runtime / manual_rebind).

Adapter callers invoke those layers via public `pub(super)` / `pub(crate)`
entry points. This boundary is tested indirectly by the high-risk recovery
lane (`docs/high-risk-recovery-lane.md`).

## Deferred Work

Tracked under #1074 follow-ups:

- Mechanical split of `recovery_engine.rs` into the three path modules.
- Migrate remaining inline `split_once(':')` sites to
  `session_identity::SessionIdentity::parse`.
- Move `reregister_active_turn_from_inflight` and
  `rebind_inflight_for_channel` helpers into `recovery::runtime` and
  `recovery::manual_rebind` respectively, leaving `recovery_engine.rs` as a
  thin facade during the transition.
