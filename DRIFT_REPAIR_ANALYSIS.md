# Issue #3656: Drift Recovery Machine Analysis — Very Thorough Dissection

## Executive Summary

The drift recovery machine in `idle_relay_drift.rs` (lines 241-367) is **NOT suitable for immediate (sub-second) invocation from the drop site** (`tui_prompt_relay.rs::resolve_owner_channel_authoritatively`) due to:

1. **60-second cooldown + single-flight gate** (lines 316-337): blocks rapid re-attempts after the first repair attempt, even on failure.
2. **Async task spawn overhead**: repair is never synchronous — it spawns an observed task (`task_supervisor::spawn_observed`) that runs the full decision core async.
3. **Potential drift re-occurrence after repair**: the restored binding can be cleared by rehydration sweeps or watcher removal, making drift "sticky" with re-trigger risk.

The repair is designed for **periodic autonomous healing** (triggered by the ~500ms idle poll loop), NOT **eager on-demand repair** from the drop site.

---

## 1. `evaluate_drift_repair` (lines 241–291): Decision Core

### Input/Output Contract

**Input (`RepairInputs`, lines 217–231):**
- `settings_channel: Option<u64>` — Settings-derived binding (from `resolve_rehydrated_claude_tmux_channel_id`)
- `db_channel: Option<u64>` — Durable `sessions.channel_id` column
- `db_instance: Option<String>` — Owner instance ID from DB row
- `local_instance: String` — This process's instance ID
- `mirror_channel: Option<u64>` — Dedupe mirror's last-seen channel (drift witness)
- `pane_live: bool` — Whether the tmux session has a live pane

**Output (`RepairDecision`, lines 206–213):**
```rust
pub(super) enum RepairDecision {
    Promote { source: RepairSource, channel: u64 },  // Two sources
    Blocked(BlockReason),                             // Four block reasons
    NoSource,                                         // No durable value
}
```

### Exact Decision Tree (lines 241–291)

**Gate 1: Dead Pane Check (lines 242–250)**
```rust
if !inputs.pane_live {
    if inputs.settings_channel.is_none() && inputs.db_channel.is_none() {
        return RepairDecision::NoSource;
    }
    return RepairDecision::Blocked(BlockReason::DeadPane);
}
```
- **Line 242**: If pane is dead → can NEVER promote (mirrors #3105 dead-pane semantics)
- **Line 246**: If no candidate at all → `NoSource` (not a spurious block)
- **Line 249**: Otherwise → `Blocked(DeadPane)` (candidate exists but pane is dead)

**Gate 2: Settings Priority (lines 252–257)**
```rust
if let Some(channel) = inputs.settings_channel {
    return RepairDecision::Promote {
        source: RepairSource::Settings,
        channel,
    };
}
```
- **Line 252–256**: Settings channel ALWAYS wins when pane is live.
- **No guard**: settings binding is already trust-verified (#3105 trust level).
- **Immediate return**: no further checks if settings hits.

**Gate 3: DB Channel Requirement (lines 259–261)**
```rust
let Some(db_channel) = inputs.db_channel else {
    return RepairDecision::NoSource;
};
```
- **Line 259–260**: If no DB source → `NoSource` (keep the drop, #3018 semantics preserved).

**Guard (b): Instance Isolation (lines 266–275)**
```rust
if let Some(db_instance) = inputs.db_instance.as_deref().map(str::trim)
    .filter(|value| !value.is_empty())
{
    if db_instance != inputs.local_instance.trim() {
        return RepairDecision::Blocked(BlockReason::ForeignInstance);
    }
}
```
- **Line 266–270**: Extract non-empty trimmed instance ID from DB row.
- **Line 272**: ONLY block if instance differs from local instance.
- **Implication**: Missing `db_instance` or empty string → **NOT blocked** (silently accepted as local).

**Guard (c): Mirror Witness + Agreement (lines 281–290)**
```rust
match inputs.mirror_channel {
    None => RepairDecision::Blocked(BlockReason::NoMirrorWitness),
    Some(mirror_channel) if mirror_channel != db_channel => {
        RepairDecision::Blocked(BlockReason::MirrorMismatch)
    }
    Some(_) => RepairDecision::Promote {
        source: RepairSource::SessionsTable,
        channel: db_channel,
    },
}
```
- **Line 282**: No mirror → `Blocked(NoMirrorWitness)` — drift condition itself requires mirror presence.
- **Line 283–284**: Mirror disagrees with DB (name reuse / rebind window) → `Blocked(MirrorMismatch)`.
  - **Misdelivery protection**: NEW dispatch already recorded different channel, old DB row is stale.
- **Line 286–288**: Mirror agrees → `Promote(SessionsTable, db_channel)`.

### Summary of Branches (line-by-line)

| Condition | Line | Return |
|-----------|------|--------|
| `!pane_live && no_candidate` | 246 | `NoSource` |
| `!pane_live && candidate_exists` | 249 | `Blocked(DeadPane)` |
| `settings_channel.is_some() && pane_live` | 254–256 | `Promote(Settings, channel)` |
| `db_channel.is_none()` | 260 | `NoSource` |
| `db_instance != local_instance` | 273 | `Blocked(ForeignInstance)` |
| `mirror_channel.is_none()` | 282 | `Blocked(NoMirrorWitness)` |
| `mirror_channel != db_channel` | 284 | `Blocked(MirrorMismatch)` |
| `mirror_channel == db_channel` | 287–289 | `Promote(SessionsTable, db_channel)` |

---

## 2. Cooldown + Single-Flight State Machine (lines 316–337)

### `try_begin_repair` Function Signature

```rust
#[cfg(unix)]
fn try_begin_repair(tmux_session_name: &str, now: Instant) -> Option<RepairInflightGuard> {
```

**Returns:**
- `Some(guard)` → caller may proceed; guard must be held until repair completes.
- `None` → repair already inflight OR 60s cooldown not elapsed.

### Exact State Machine (lines 316–337)

**Step 1: Lock & Cleanup (lines 318–320)**
```rust
let mut map = DRIFT_STATE.lock().unwrap_or_else(|error| error.into_inner());
purge_expired_locked(&mut map, now);
```
- Acquire global `DRIFT_STATE` mutex (process-wide).
- Purge any per-session state untouched >24h (line 85: `DRIFT_STATE_TTL`).

**Step 2: Get or Initialize Session State (lines 321–323)**
```rust
let state = map
    .entry(tmux_session_name.to_string())
    .or_insert_with(|| DriftState::new(now));
```
- Create `DriftState` if not present (line 99–108):
  - `first_seen_at: now`
  - `last_touched_at: now`
  - `last_warn_at: None`
  - `suppressed_count: 0`
  - `last_repair_attempt_at: None`
  - `repair_inflight: false`

**Step 3: Single-Flight Check (lines 324–326)**
```rust
if state.repair_inflight {
    return None;
}
```
- If another repair is already running → **IMMEDIATE REJECT** (no backoff timer).
- Single-flight prevents concurrent repairs for the same session.

**Step 4: Cooldown Check (lines 327–330)**
```rust
if let Some(last) = state.last_repair_attempt_at {
    if now.saturating_duration_since(last) < DRIFT_REPAIR_COOLDOWN {
        return None;
    }
}
```
- **Line 81**: `DRIFT_REPAIR_COOLDOWN = Duration::from_secs(60)`.
- If `last_repair_attempt_at` is set AND less than 60s ago → **BLOCK** (cooldown active).
- If `last_repair_attempt_at` is `None` → **ALLOW** (first attempt, no cooldown yet).

**Step 5: Claim Slot (lines 332–337)**
```rust
state.last_repair_attempt_at = Some(now);
state.repair_inflight = true;
Some(RepairInflightGuard {
    tmux_session_name: tmux_session_name.to_string(),
})
```
- Mark the time of this repair attempt.
- Set `repair_inflight` flag to block concurrent attempts.
- Return guard; **on guard drop** (line 301–309), `repair_inflight` is cleared by RAII.

### Scenario: Cooldown Blocks Eager Repair

**Timeline:**
1. **t=0s**: `on_idle_relay_drift` called → `try_begin_repair` → ALLOWED (first attempt).
   - `last_repair_attempt_at = t0`; `repair_inflight = true`.
   - Async task spawned.
2. **t=0.5s**: Same session drift detected again (next idle poll, ~500ms cycle).
   - `on_idle_relay_drift` called → `try_begin_repair` → **REJECTED** (single-flight active).
   - Repair task from step 1 still running.
3. **t=5s**: Repair task completes, `repair_inflight` cleared by guard drop.
   - Drift detected again (idle poll) → `try_begin_repair` → **REJECTED** (cooldown active, 60s not elapsed).
   - Repair is **rate-limited** until **t=60s**.
4. **t=60s+**: Cooldown expires → `try_begin_repair` → **ALLOWED**.

**Impact**: Even if the first repair **FAILS**, the 60s cooldown still activates, blocking eager re-attempts.

---

## 3. `on_idle_relay_drift` / `attempt_drift_repair` (lines 343–443)

### Entry Point: `on_idle_relay_drift` (lines 343–367)

```rust
pub(super) fn on_idle_relay_drift(
    shared: &Arc<SharedData>,
    provider: ProviderKind,
    tmux_session_name: &str,
) {
```

**Step 1: Provider Filter (lines 351–353)**
```rust
if provider != ProviderKind::Claude {
    return;
}
```
- **Repair is Claude-only** (settings resolver is Claude-specific; routine hook flow is Claude/routine only).
- Codex drift gets rate-limited WARN only, no repair.

**Step 2: Try Claim Slot (lines 355–358)**
```rust
let now = Instant::now();
let Some(guard) = try_begin_repair(tmux_session_name, now) else {
    return;
};
```
- **Immediate return** if cooldown or single-flight blocks.
- **No async fallback**: if the slot is occupied or cooldown is active, the function exits synchronously.

**Step 3: Async Spawn (lines 360–366)**
```rust
let shared = shared.clone();
let tmux_session_name = tmux_session_name.to_string();
super::task_supervisor::spawn_observed("idle_relay_drift_repair", async move {
    attempt_drift_repair(&shared, &tmux_session_name, guard).await;
});
```
- **Fully async**: spawns on the Tokio runtime via `task_supervisor::spawn_observed`.
- Guard is **moved into the task** → kept held for the entire async operation.
- **No synchronous completion**: function returns immediately after spawn.

### Async Repair Task: `attempt_drift_repair` (lines 370–443)

**Data Collection Phase (lines 375–405):**

1. **Settings Channel (line 377)** — Synchronous, no lock:
   ```rust
   let settings_channel = super::tui_prompt_relay::
       resolve_rehydrated_claude_tmux_channel_id(tmux_session_name);
   ```
   - **No await**: purely synchronous list scan over registered bindings.

2. **DB Channel + Instance (line 381)** — Async DB query:
   ```rust
   let (db_channel, db_instance) = load_db_channel(shared, tmux_session_name).await;
   ```
   - **Awaited**: may block on DB I/O (lines 446–472).
   - **Candidates**: tries both namespaced and legacy session keys.

3. **Mirror Channel (line 385)** — Synchronous, read-only:
   ```rust
   let mirror_channel = crate::services::tui_prompt_dedupe::
       owner_channel_for_tmux_session(tmux_session_name);
   ```
   - **No lock**: read-only access to in-memory dedupe mirror map.

4. **Pane Live Check (lines 388–395)** — Blocking pool task:
   ```rust
   let pane_live = {
       let probe_name = tmux_session_name.to_string();
       tokio::task::spawn_blocking(move || {
           crate::services::tmux_diagnostics::tmux_session_has_live_pane(&probe_name)
       })
       .await
       .unwrap_or(false)
   };
   ```
   - **Spawned on blocking pool**: synchronous tmux subprocess call.
   - **Awaited**: may block on tmux I/O.
   - **Fallback**: if the task is cancelled/panics → `false` (conservative, blocks repair).

**Decision Phase (line 407):**
```rust
match evaluate_drift_repair(&inputs) {
    RepairDecision::Promote { source, channel } => {
        let repaired = shared
            .tmux_watchers
            .restore_owner_channel_for_tmux_session(tmux_session_name, ChannelId::new(channel));
        if repaired {
            tracing::warn!(...);
        }
    }
    RepairDecision::Blocked(reason) => {
        tracing::warn!(...);
    }
    RepairDecision::NoSource => {
        tracing::debug!(...);
    }
}
```

**Registry Promotion (line 413):**
- **Synchronous, lock-held**: `restore_owner_channel_for_tmux_session` takes the registry lock briefly.
- **Idempotent**: no-ops if value unchanged or live watcher already owns it.
- Returns `true` only on first/changed registration.

---

## 4. Settings Channel Behavior (agentdesk.yaml Registration)

### Does `settings_channel` Always Resolve for Registered Bindings?

**Answer: NO.** Settings binding is a **best-effort match** that fails silently if:

1. **Tmux session name doesn't match** any registered binding pattern.
   - Routine sessions like `claude-routine-token-daily-report-…` match NO settings channel.
   - That's the **core problem** #3306 solves: permanent drift for routine sessions.

2. **Ambiguous match** (multiple conflicting bindings).
   - If two settings bindings claim the same session name, the second is skipped (line not shown, but implied by rehydration logic).

3. **Channel ID mismatch** during rehydration.
   - Segments are tried in order (channel_id, fallback_name); if a segment resolves but conflicts, it's skipped.

**Relevant code** (`resolve_rehydrated_claude_tmux_channel_id`, lines 4263+):
- Iterates registered bindings.
- For each binding, tries segment matching (channel_id text, fallback_name).
- Returns the FIRST unambiguous hit; **returns `None` if no hit**.

### What Gets Written to Registry on Promotion?

**Function**: `restore_owner_channel_for_tmux_session` (mod.rs lines 1012–1033)

```rust
pub(super) fn restore_owner_channel_for_tmux_session(
    &self,
    tmux_session_name: &str,
    channel_id: ChannelId,
) -> bool {
    let _guard = lock_tmux_watcher_registry();
    if self.owner_channel_by_tmux_session.contains_key(tmux_session_name) {
        self.restored_owner_by_tmux_session.remove(tmux_session_name);
        return false;
    }
    let changed = self.restored_owner_by_tmux_session
        .get(tmux_session_name)
        .map(|entry| *entry.value())
        != Some(channel_id);
    self.restored_owner_by_tmux_session.insert(tmux_session_name.to_string(), channel_id);
    changed
}
```

**Registry Map Written To**: `restored_owner_by_tmux_session` (DashMap<String, ChannelId>).

**Reads From**: `owner_channel_for_tmux_session` (mod.rs lines 978–995)
- Checks `owner_channel_by_tmux_session` (live watcher, authority).
- Falls back to `restored_owner_by_tmux_session` (#3105 restored, still authoritative).
- **Never checks the mirror** (preserves #3018 "never route from mirror" invariant).

---

## 5. Access from Drop Site: Reusability & Barriers

### Drop Site Signature & Available Context

**Resolver Location**: `tui_prompt_relay.rs::resolve_owner_channel_authoritatively` (line 4203)

```rust
fn resolve_owner_channel_authoritatively(
    tmux_session_name: &str,
    registry_owner: Option<ChannelId>,
    dedupe_owner: Option<u64>,
) -> Option<ChannelId> {
```

**Caller**: `owner_channel_for_tmux_session` (line 4195)

```rust
fn owner_channel_for_tmux_session(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
) -> Option<ChannelId> {
    let registry_owner = shared
        .tmux_watchers
        .owner_channel_for_tmux_session(tmux_session_name);
    let dedupe_owner = crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(tmux_session_name);
    resolve_owner_channel_authoritatively(tmux_session_name, registry_owner, dedupe_owner)
}
```

**Invocation Site** (idle loop, line ~4180):

```rust
for (tmux_session_name, binding) in
    crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(RuntimeHandoffKind::ClaudeTui)
{
    let Some(channel_id) = owner_channel_for_tmux_session(&shared, &tmux_session_name) else {
        super::idle_relay_drift::on_idle_relay_drift(
            &shared,
            ProviderKind::Claude,
            &tmux_session_name,
        );
        continue;
    }
    // Route to channel_id...
}
```

### Available State at Drop Site

| Item | Available? | Synchronicity | Notes |
|------|-----------|---|---|
| `shared: &Arc<SharedData>` | ✓ | sync | Contains `tmux_watchers` registry, `pg_pool`, instance ID. |
| `tmux_session_name: &str` | ✓ | sync | From dedupe runtime bindings. |
| `ProviderKind` | ✓ | sync | Known at invocation (Claude). |
| Registry query results | ✓ | sync | Already resolved in `owner_channel_for_tmux_session`. |
| Mirror query results | ✓ | sync | Already resolved in `owner_channel_for_tmux_session`. |

### What Repair Needs vs. What's Available

**`on_idle_relay_drift` signature** (line 343):
```rust
pub(super) fn on_idle_relay_drift(
    shared: &Arc<SharedData>,
    provider: ProviderKind,
    tmux_session_name: &str,
)
```

**Requirements → Available?**
- `shared` → ✓ Available (passed to idle loop).
- `provider` → ✓ Available (known as Claude at drop site).
- `tmux_session_name` → ✓ Available (parameter).

**Conclusion: Repair IS directly callable from drop site.**

### Required Handles for Synchronous Invocation

If someone attempted to call repair **synchronously** from drop site (NOT async spawn):

1. **`shared.tmux_watchers`** → needed for `restore_owner_channel_for_tmux_session` (registry lock).
2. **`shared.pg_pool`** → needed for `load_db_channel` (DB queries).
3. **`shared.token_hash`** → needed for session key candidates.
4. **Tokio runtime** → needed for `spawn_blocking` (pane live check).

All are accessible via `&Arc<SharedData>`.

---

## 6. Drift Persistence & Re-occurrence Risk ("Stickiness")

### Promotion Persistence

**Question**: Does the promoted binding "stick" or can it be cleared, causing drift to re-occur?

**Answer: Binding can be cleared** (not sticky), re-enabling drift.

### Clearing Paths

**Path 1: Rehydration Sweep** (`tui_prompt_relay/rehydration.rs`)

```rust
// When a dead pane is detected:
shared.tmux_watchers.clear_restored_owner_for_tmux_session(&tmux_session_name);
if crate::services::tui_prompt_dedupe::evict_dead_tmux_mirror(&tmux_session_name) {
    tracing::warn!(...);
}
```

- The rehydration loop (line ~4170, periodic pass) **actively clears** restored owners for dead panes.
- If the watcher gets cleaned up and the pane is declared dead, the restoration is erased.
- **Next idle poll**: drift re-occurs (mirror still present, registry now empty).

**Path 2: Watcher Removal** (`mod.rs::remove_tmux_session_locked`)

```rust
fn remove_tmux_session_locked(
    &self,
    _guard: &std::sync::MutexGuard<'_>,
    tmux_session_name: &str,
) -> Option<(ChannelId, TmuxWatcherHandle)> {
    let (_, owner_channel_id) = self
        .owner_channel_by_tmux_session
        .remove(tmux_session_name)?;
    self.tmux_session_by_channel.remove(&owner_channel_id);
    self.by_tmux_session
        .remove(tmux_session_name)
        .map(|(_, handle)| (owner_channel_id, handle))
}
```

- Removes from `owner_channel_by_tmux_session` (live watcher authority).
- **Does NOT clear** `restored_owner_by_tmux_session` (that requires explicit call to `clear_restored_owner_for_tmux_session`).

**Path 3: Watcher Respawn**

- If a watcher reconnects (`rehydrate_existing_claude_tui_bindings`, line ~4170), it:
  - **Re-claims** `owner_channel_by_tmux_session` via `claim_owner_channel_for_tmux_session`.
  - Does **not automatically clear** the restored binding.
  - If new watcher has the same channel → no problem (redundant).
  - If new watcher has a different channel (rebind) → **restored binding is stale** but not cleared.

### Drift Re-occurrence Scenario

**Timeline:**
1. **t=0s**: Drift detected, repair triggered, settings binding promoted to `restored_owner_by_tmux_session[session]`.
   - Idle relay routes again.
2. **t=10s**: Rehydration sweep detects pane is dead → calls `clear_restored_owner_for_tmux_session`.
   - `restored_owner_by_tmux_session[session]` cleared.
3. **t=15s**: Pane comes back live (reconnect).
   - Dedupe mirror still has old channel (24h TTL in-memory).
   - Registry is empty (watcher removed, restoration cleared).
   - **Drift re-occurs** on next idle poll (~500ms).
4. **t=15.5s+**: New repair triggered by drift, BUT...
   - If last repair was at t=0s and it's now t=15.5s, **cooldown expired** (60s threshold). ✓ Repair allowed.
   - If last repair was at t=10s (e.g., a second repair attempt before pane died), **cooldown may still be active**. ✗ Repair blocked until t=70s.

### Stickiness Assessment

**Sticky: NO** — the restored binding is not permanently protected. It can be cleared by rehydration or manual cleanup. Drift can re-occur if:
- The pane is re-evaluated and declared dead.
- A watcher respawn clears the old restoration.
- Explicit `clear_restored_owner_for_tmux_session` is called.

**Risk**: If drift re-occurs within 60s of the previous repair attempt (even if repair failed), it's rate-limited and cannot be repaired again. This could cause a gap window.

---

## Summary Table: Repair Machine Characteristics

| Aspect | Behavior | Details |
|--------|----------|---------|
| **Entry Point** | `on_idle_relay_drift(&shared, provider, tmux_name)` | Called from idle loop (~500ms) on drift detection. |
| **Provider Scope** | Claude-only | Codex gets rate-limited WARN only, no repair. |
| **Synchronicity** | Fully async | Spawned as observed task; returns immediately. |
| **Cooldown** | 60 seconds | After ANY repair attempt (success/fail), blocks retries for 60s. |
| **Single-Flight** | Per-session | One repair in-flight per tmux session; concurrent attempts rejected. |
| **Decision Core** | IO-free pure function | `evaluate_drift_repair(&inputs) -> RepairDecision` (unit-testable). |
| **Settings Priority** | Always wins (pane-live) | If settings resolves → promote immediately, skip DB/mirror checks. |
| **DB Source Guards** | 3-fold | (1) live pane, (2) instance match, (3) mirror agreement. |
| **Mirror Use** | Block-only gate | Never routes from mirror; only validates DB channel. |
| **Registry Promotion** | Idempotent | Writes to `restored_owner_by_tmux_session` map. |
| **Stickiness** | Non-persistent | Binding can be cleared by rehydration or watcher removal. |
| **Callable from Drop Site** | YES | All required handles (`shared`, `provider`, `tmux_name`) available. |
| **Eager Invocation** | **BLOCKED by cooldown** | Cannot be called synchronously from drop site; async spawn required; 60s cooldown prevents rapid re-attempts. |

---

## Recommendations

### For On-Drop (Immediate) Repair via Drop Site

**Current Design**: NOT suitable. The repair machine is designed for periodic autonomous healing via the idle loop.

**If immediate repair is desired:**

1. **Decouple decision from cooldown**: Create a separate synchronous `evaluate_drift_repair_immediately()` that does NOT consult `DRIFT_STATE` cooldown. Call it from drop site to emit a *synchronous* decision log (no async spawn).

2. **Eager synchronous restoration**: If the pure decision core returns `Promote { source, channel }`, immediately call `restore_owner_channel_for_tmux_session` **synchronously** from drop site (before returning `None`).
   - **Trade-off**: Blocks the idle loop briefly on each drift (registry lock is synchronous anyway).
   - **Benefit**: Repair is immediate (sub-millisecond, not 60s delayed).

3. **Keep async repair as background opt**: Keep the current `on_idle_relay_drift` async spawn as a fallback for **complex sources** (e.g., DB queries on high latency). Drop site repair handles fast paths only.

### For Preventing Drift Re-occurrence

1. **Protect restoration from rehydration clearing**: Add a flag to `DriftState` tracking "repair succeeded"; don't clear the restored binding during rehydration if repair recently succeeded.

2. **Extend cooldown after rehydration clears**: If rehydration clears a binding, don't reset the cooldown; let the next repair (after cooldown expires) re-promote.

3. **Monitor drift lifecycle**: Track drift first-seen → repaired → cleared timeline to detect pathological re-repair loops.

---

## Code Locations (Exact Lines)

| Component | File | Lines |
|-----------|------|-------|
| `evaluate_drift_repair` | `idle_relay_drift.rs` | 241–291 |
| `RepairInputs` / `RepairDecision` | `idle_relay_drift.rs` | 206–231 |
| `try_begin_repair` | `idle_relay_drift.rs` | 316–337 |
| `on_idle_relay_drift` | `idle_relay_drift.rs` | 343–367 |
| `attempt_drift_repair` | `idle_relay_drift.rs` | 370–443 |
| `resolve_owner_channel_authoritatively` | `tui_prompt_relay.rs` | 4203–4228 |
| `owner_channel_for_tmux_session` | `tui_prompt_relay.rs` | 4195–4200 |
| Drop site idle loop | `tui_prompt_relay.rs` | ~4180–4190 |
| `restore_owner_channel_for_tmux_session` | `mod.rs` | 1012–1033 |
| `owner_channel_for_tmux_session` (registry) | `mod.rs` | 978–995 |
