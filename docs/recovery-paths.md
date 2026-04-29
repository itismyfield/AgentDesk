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

## Watcher Ownership

Normal watcher registration has one policy for all recovery paths: a tmux
session has at most one live watcher. Duplicate attach attempts for the same
`tmux_session_name` reuse the existing live handle and return that handle's
owning Discord channel; callers must pause, resume, or mark delivery on the
owner slot, not necessarily the requested channel. A same-session handle is
replaced only when the existing handle is already cancelled, which is the
registry's provable stale marker. A different tmux session on the same Discord
channel may still replace the channel slot so new-turn recovery is not blocked
by an older session.

The missing-inflight reattach fallback repairs metadata without breaking tmux
ownership. When a live watcher already owns the same tmux session, the fallback
persists a synthetic rebind-origin inflight record and reuses that owner slot;
it does not cancel the incumbent watcher or spawn a fresh generation. A new
watcher is spawned only when no live watcher owns the tmux session.

## Watcher Lifecycle And Route Ownership

Watcher lifetime follows the tmux session, not the Discord route that most
recently noticed it. The route that wins the watcher claim becomes the owner
channel for that `tmux_session_name`; later start/attach, restart recovery, or
manual rebind calls for the same live tmux session must reuse that owner slot.
Those callers must apply turn rotation state (`paused`, `pause_epoch`,
`resume_offset`, and `turn_delivered`) to the returned owner channel. Applying
the state to the requested channel can strand the live relayer and recreate
duplicate relay races.

Normal watcher shutdown is tmux-liveness driven. A terminal-success event does
not detach the watcher while the tmux pane remains alive; the watcher stops
only after tmux death is observed, then removes its slot quietly. Operator
stop/cancel paths that report `killed=false` preserve watcher ownership and do
not raise the watcher's cancel flag. They also preserve persistent inflight
state for live-session handoff, so the live tmux session remains the watcher's
lifecycle authority. Force-kill and hard-stop paths are different: they remove
the watcher slot, clear inflight state, and raise the watcher's cancel flag so
the loop exits without issuing session-ended relay noise.

Manual rebind is route adoption, not relay multiplication. If a live watcher
already owns the target tmux session, `rebind_inflight_for_channel` returns a
non-spawning reuse result and leaves delivery with the incumbent watcher.

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
