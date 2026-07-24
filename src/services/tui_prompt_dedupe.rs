use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_prompt_control::{
    classify_local_only_slash_control, is_start_anchored_task_notification_prompt,
};
use chrono::{DateTime, Utc};

mod synthetic_prompt;
use self::synthetic_prompt::{
    is_synthetic_tui_user_prompt_for_provider, reject_synthetic_claude_user_prompt,
    reject_synthetic_tui_user_prompt,
};

const PENDING_PROMPT_TTL: Duration = Duration::from_secs(10);
const RECENT_OBSERVED_TTL: Duration = Duration::from_secs(30);
const SESSION_MAPPING_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const PROMPT_ANCHOR_TTL: Duration = Duration::from_secs(30 * 60);
// #3885 follow-up: the per-`(provider, tmux)` PROMPT ANCHOR must outlive the
// LONGEST realistic in-progress streaming turn. The anchor is stamped ONCE at
// `record_prompt_anchor` (submit time) and is NOT re-stamped while the turn
// streams; it is cleared on completion (`take`/`clear_prompt_anchor_for_response`)
// and overwritten by the next submit (one entry per pane). Under the previous
// 30min purge a build/agent turn that streams 30-60min (routine in the
// issue-pipeline workflow) had its anchor purged MID-STREAM, after which the
// bridge same-input correlation peek (and the watcher ⏳→✅ response match)
// resolved `None` → the #3885 no-response requeue re-fired a duplicate, and a
// long turn's ⏳ could strand. 4h is a generous ceiling over realistic turn
// durations (no hard max-turn-duration constant exists to derive from). This is
// DECOUPLED from `PROMPT_ANCHOR_TTL` on purpose: the `relayed_entry_ids_by_tmux`
// ledger below keeps the 30min window its #3459/#3303 rationale documents, so
// raising the anchor lifetime cannot perturb that missed-prompt dedup. The
// anchor is one-per-pane and overwritten on the next submit, so the longer TTL
// only bounds an idle pane's last (uncleared) anchor — bounded memory, and a
// stale anchor with a DIFFERENT message id can never shadow a new prompt (lookups
// match on `message_id`).
const PROMPT_ANCHOR_SUBMIT_TTL: Duration = Duration::from_secs(4 * 60 * 60);
// Short window matching how long a Discord notify await + transcript flush
// can plausibly take before `record_prompt_anchor` lands. 60s is generous;
// the marker is also cleared explicitly when an anchor is consumed.
const SSH_DIRECT_OBSERVATION_TTL: Duration = Duration::from_secs(60);
const EXTERNAL_INPUT_RELAY_LEASE_TTL: Duration = Duration::from_secs(10 * 60);
// #3174: a deferred ⏳-completion marker only has to survive the gap between the
// watcher's lease-gated completion firing (anchor not yet recorded) and THIS
// turn's `record_prompt_anchor` landing — the `notify-post + ⏳-add` Discord I/O
// window. Bounding it to the SSH-direct observation TTL keeps a stranded marker
// from a turn that never records an anchor (e.g. notify-post failure) from
// leaking onto a much-later same-key turn.
const DEFERRED_ANCHOR_COMPLETION_TTL: Duration = Duration::from_secs(60);
const OBSERVED_PROMPT_BUFFER: usize = 128;
// #3540: per-`(provider, tmux)` ring cap on the relayed-entry-id ledger. A
// single session rarely relays anywhere near this many DISTINCT user prompts
// inside the 30min entry-id TTL; the cap is a belt-and-braces upper bound so a
// pathological long-lived session cannot grow the set without limit (TTL purge
// is the primary bound). Oldest entries are dropped first.
const RELAYED_ENTRY_ID_RING_CAP: usize = 512;

static STATE: LazyLock<Mutex<TuiPromptDedupeState>> =
    LazyLock::new(|| Mutex::new(TuiPromptDedupeState::default()));
#[cfg(test)]
// Tests that also mutate process env must acquire `shared_test_env_lock()` before
// this lock. Keep that env -> dedupe order globally to avoid AB/BA deadlocks.
pub(crate) static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static OBSERVED_PROMPTS: LazyLock<broadcast::Sender<ObservedTuiPrompt>> =
    LazyLock::new(|| broadcast::channel(OBSERVED_PROMPT_BUFFER).0);

/// Process-global monotonic counter that stamps a UNIQUE `generation` onto every
/// external-input relay lease at the moment it is RECORDED. Two leases that are
/// otherwise identical by value — e.g. two newer `Unassigned` (legacy) turns for
/// the same `(provider, tmux_session, channel)` whose `turn_id`/`session_key`/
/// `runtime_kind` are all `None` — therefore receive DISTINCT generations and are
/// no longer indistinguishable. A RAII guard captures the exact recorded lease
/// (with its generation) and on Drop clears ONLY that generation, so a slow OLD
/// delivery's guard can never clobber a NEWER identical lease. #3041 P1-4 codex.
/// Starts at 1 so that 0 stays a reserved "not yet recorded" sentinel.
static EXTERNAL_INPUT_RELAY_LEASE_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Process-global identity for the short SSH-direct observation marker.
static SSH_DIRECT_OBSERVATION_GENERATION: AtomicU64 = AtomicU64::new(1);

/// `generation` sentinel for a freshly constructed lease that has NOT yet been
/// recorded (and therefore not yet stamped with a unique generation).
pub(crate) const EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED: u64 = 0;
pub(crate) const SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED: u64 = 0;

fn next_external_input_relay_lease_generation() -> u64 {
    EXTERNAL_INPUT_RELAY_LEASE_GENERATION.fetch_add(1, Ordering::Relaxed)
}

fn next_ssh_direct_observation_generation() -> u64 {
    SSH_DIRECT_OBSERVATION_GENERATION.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedTuiPrompt {
    pub provider: String,
    pub tmux_session_name: String,
    pub prompt: String,
    /// Stable provider entry identity when the transcript/rollout exposes one.
    /// Unlike byte offsets this survives compaction and head rotation.
    pub source_event_id: Option<String>,
    pub observed_at: DateTime<Utc>,
    /// Exact side effects created before this event was published. Local-only
    /// controls carry the unrecorded sentinel for both fields because they
    /// publish no lease/SSH state at all.
    pub(crate) external_input_lease_generation: u64,
    pub(crate) ssh_direct_observation_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TuiPromptAnchor {
    pub channel_id: u64,
    pub message_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExternalInputRelayOwner {
    Unassigned,
    BridgeAdapter,
    TuiPromptRelay,
    TmuxWatcher,
    SessionBoundRelay,
}

impl ExternalInputRelayOwner {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Unassigned => "unassigned",
            Self::BridgeAdapter => "bridge_adapter",
            Self::TuiPromptRelay => "tui_prompt_relay",
            Self::TmuxWatcher => "tmux_watcher",
            Self::SessionBoundRelay => "session_bound_relay",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExternalInputRelayLease {
    pub channel_id: Option<u64>,
    pub turn_id: Option<String>,
    pub session_key: Option<String>,
    pub relay_owner: ExternalInputRelayOwner,
    pub runtime_kind: Option<RuntimeHandoffKind>,
    /// Unique, monotonic per-record identity stamped by
    /// [`record_external_input_turn_lease`] when this lease is inserted into the
    /// state map. A freshly constructed (not-yet-recorded) lease carries
    /// [`EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED`] (0); the record path
    /// overwrites it with a fresh process-global counter value so that two leases
    /// that are otherwise value-equal (notably two `Unassigned` leases whose
    /// trace fields are all `None`) are still DISTINGUISHABLE. A RAII guard
    /// captures the RECORDED lease (via the value returned from the record call)
    /// and clears only its OWN generation, so it can never clobber a newer lease.
    pub generation: u64,
}

impl ExternalInputRelayLease {
    pub(crate) fn unassigned(channel_id: Option<u64>) -> Self {
        Self {
            channel_id,
            turn_id: None,
            session_key: None,
            relay_owner: ExternalInputRelayOwner::Unassigned,
            runtime_kind: None,
            generation: EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TuiRuntimeBinding {
    pub runtime_kind: RuntimeHandoffKind,
    pub output_path: String,
    pub relay_output_path: Option<String>,
    pub input_fifo_path: Option<String>,
    pub session_id: Option<String>,
    pub last_offset: u64,
    pub relay_last_offset: Option<u64>,
}

impl TuiRuntimeBinding {
    pub(crate) fn relay_output_path(&self) -> &str {
        self.relay_output_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
            .unwrap_or(&self.output_path)
    }

    pub(crate) fn relay_last_offset(&self) -> u64 {
        self.relay_last_offset.unwrap_or(self.last_offset)
    }
}

#[derive(Clone, Debug)]
struct TimedValue<T> {
    value: T,
    recorded_at: Instant,
}

#[derive(Default)]
struct TuiPromptDedupeState {
    pending_by_tmux: HashMap<PromptKey, VecDeque<TimedValue<String>>>,
    recent_observed_by_tmux: HashMap<PromptKey, VecDeque<TimedValue<String>>>,
    tmux_by_provider_session: HashMap<PromptKey, TimedValue<String>>,
    channel_by_tmux: HashMap<String, TimedValue<u64>>,
    runtime_by_tmux: HashMap<String, TimedValue<TuiRuntimeBinding>>,
    prompt_anchor_by_tmux: HashMap<PromptKey, TimedValue<TuiPromptAnchor>>,
    // Short-lived marker set the moment an SSH-direct prompt is observed,
    // closing the window before `record_prompt_anchor` runs (the latter has
    // to wait for the Discord notify await to land).
    ssh_direct_observation_by_tmux: HashMap<PromptKey, TimedValue<u64>>,
    // Longer-lived response relay lease set as soon as a direct tmux prompt
    // is observed. Unlike the Discord prompt anchor this survives notify-bot
    // failures; watchers use it to keep post-terminal suppression from eating
    // the response.
    external_input_relay_lease_by_tmux: HashMap<PromptKey, TimedValue<ExternalInputRelayLease>>,
    // #3174: deferred ⏳-completion markers. When the watcher's lease-gated
    // completion fires BEFORE this turn's `record_prompt_anchor` has landed (the
    // provider committed terminal output inside the sub-second `notify-post +
    // ⏳-add` window), the anchor lookup returns None and the completion would be
    // a no-op — stranding the ⏳. Instead the watcher records a marker here; the
    // SAME turn's `record_prompt_anchor` then drains it and the relay finishes
    // the ⏳ → ✅ swap against the just-recorded anchor.
    //
    // #3174 codex P1: the marker carries the TURN IDENTITY — the
    // `generation` of the external-input lease the completion was gated on (a
    // unique monotonic per-record nonce; see [`ExternalInputRelayLease`]). The
    // `(provider, tmux)` key alone is NOT turn-unique: within the marker TTL a
    // NEWER turn on the same provider/tmux could otherwise drain the PREVIOUS
    // turn's marker and complete the wrong turn's ⏳ → ✅. The relay only drains a
    // marker whose stored generation MATCHES the lease generation THIS relay
    // invocation recorded; a marker for a different turn is left untouched.
    deferred_anchor_completion_by_tmux: HashMap<PromptKey, TimedValue<u64>>,
    // #3540: stable JSONL entry-identity (`uuid`) ledger of prompts THIS process
    // has already relayed for a `(provider, tmux)` pair. The root-cause fix for
    // the phantom synthetic inflight: when the relay watermark is reset to 0
    // (jsonl head rotation / session restore), the idle-transcript scanner
    // re-scans from offset 0 and re-encounters already-relayed prompts. The
    // content-keyed `recent_observed_by_tmux` (30s TTL) lets a re-encounter that
    // straddles that window slip through and mint a fresh — phantom — synthetic
    // inflight whose commit will never arrive. This ledger keys on the entry's
    // immutable `uuid`, which is preserved verbatim across head rotation (offset
    // shifts, uuid does not), so an already-relayed entry is suppressed by
    // IDENTITY regardless of the 30s window. A genuinely NEW prompt has a NEW
    // uuid (issued by Claude Code at type time) so it can never collide here —
    // no #3459/#3303 missed-prompt regression. TTL'd by `PROMPT_ANCHOR_TTL`
    // (30min) — long enough to span the rotation+self-loop window, bounded so
    // the set cannot grow without limit; additionally ring-capped per key.
    relayed_entry_ids_by_tmux: HashMap<PromptKey, VecDeque<TimedValue<String>>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PromptKey {
    provider: String,
    key: String,
}

impl PromptKey {
    fn new(provider: &str, key: &str) -> Self {
        Self {
            provider: normalize_provider(provider),
            key: key.trim().to_string(),
        }
    }
}

pub fn subscribe_observed_prompts() -> broadcast::Receiver<ObservedTuiPrompt> {
    OBSERVED_PROMPTS.subscribe()
}

pub fn register_provider_session(
    provider: &str,
    provider_session_id: &str,
    tmux_session_name: &str,
) {
    let provider_session_id = provider_session_id.trim();
    let tmux_session_name = tmux_session_name.trim();
    if provider_session_id.is_empty() || tmux_session_name.is_empty() {
        return;
    }
    {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state.purge_expired();
        state.tmux_by_provider_session.insert(
            PromptKey::new(provider, provider_session_id),
            TimedValue {
                value: tmux_session_name.to_string(),
                recorded_at: Instant::now(),
            },
        );
    }
}

/// Reverse lookup: resolve the provider session id that maps to `tmux_session_name`
/// for `provider`, if one was registered. `register_provider_session` records
/// the forward `provider_session_id -> tmux_session_name` mapping at launch;
/// this scans it for the entry whose value matches the tmux session.
///
/// #tui-hook-ttl-buffer key-match fix: the Claude hook relay buffers under the
/// PROVIDER session UUID (`config.session_id`), but the readiness layer only
/// knows the tmux session name. Callers use this to claim the SAME key the hooks
/// buffered under instead of the tmux fallback (which the buffer never used for a
/// hosted Claude launch). Returns `None` when no mapping is known, in which case
/// the caller should fall back to the tmux session name.
pub fn provider_session_for_tmux(provider: &str, tmux_session_name: &str) -> Option<String> {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return None;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    // The forward map can in principle hold multiple provider session ids that
    // pointed at the same tmux session over time; prefer the most recently
    // recorded survivor. Do not TTL-expire this bridge: long-lived TUI sessions
    // can keep emitting hooks with the same provider UUID after the ordinary
    // prompt-cache TTL has elapsed.
    state
        .tmux_by_provider_session
        .iter()
        .filter(|(promptkey, timed)| {
            promptkey.provider == provider && timed.value == tmux_session_name
        })
        .max_by_key(|(_, timed)| timed.recorded_at)
        .map(|(promptkey, _)| promptkey.key.clone())
}

pub(crate) fn provider_session_is_registered(provider: &str, provider_session_id: &str) -> bool {
    resolve_tmux_session_name(provider, provider_session_id).is_some()
}

pub fn register_tmux_channel(tmux_session_name: &str, channel_id: u64) {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() || channel_id == 0 {
        return;
    }
    if tmux_session_name.contains("-dm-") {
        if let Err(error) =
            crate::services::tmux_common::write_tmux_channel_binding(tmux_session_name, channel_id)
        {
            tracing::warn!(
                tmux_session_name,
                channel_id,
                %error,
                "failed to persist DM tmux channel binding"
            );
        }
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.channel_by_tmux.insert(
        tmux_session_name.to_string(),
        TimedValue {
            value: channel_id,
            recorded_at: Instant::now(),
        },
    );
}

pub(crate) fn register_tmux_runtime_binding(tmux_session_name: &str, binding: TuiRuntimeBinding) {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() || binding.output_path.trim().is_empty() {
        return;
    }
    if binding.relay_output_path().trim().is_empty() {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.runtime_by_tmux.insert(
        tmux_session_name.to_string(),
        TimedValue {
            value: binding,
            recorded_at: Instant::now(),
        },
    );
}

pub(crate) fn register_rehydrated_tmux_runtime_binding(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    binding: TuiRuntimeBinding,
) {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty()
        || tmux_session_name.is_empty()
        || channel_id == 0
        || binding.output_path.trim().is_empty()
        || binding.relay_output_path().trim().is_empty()
    {
        return;
    }
    let session_id = binding.session_id.clone();
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.runtime_by_tmux.insert(
        tmux_session_name.to_string(),
        TimedValue {
            value: binding,
            recorded_at: Instant::now(),
        },
    );
    state.channel_by_tmux.insert(
        tmux_session_name.to_string(),
        TimedValue {
            value: channel_id,
            recorded_at: Instant::now(),
        },
    );
    if let Some(session_id) = session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        state.tmux_by_provider_session.insert(
            PromptKey::new(&provider, session_id),
            TimedValue {
                value: tmux_session_name.to_string(),
                recorded_at: Instant::now(),
            },
        );
    }
}

/// #3018: DIAGNOSTIC / MIRROR USE ONLY.
///
/// This expiry-based cache is NOT the authority for tmux-session→channel
/// resolution. The authoritative source is the `tmux_watchers` registry
/// (`SharedData::tmux_watchers`), which holds the 1:1 routing invariant. This
/// lookup may only be used for best-effort diagnostics / rehydration hints — it
/// must never be used as a reverse authority to route relays, or drift between
/// the two sources will silently mis-route. See
/// `tui_prompt_relay::owner_channel_for_tmux_session`.
pub fn owner_channel_for_tmux_session(tmux_session_name: &str) -> Option<u64> {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return None;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .channel_by_tmux
        .get(tmux_session_name)
        .map(|entry| entry.value)
}

/// Test-only: reset the entire dedupe state. Crate-visible so sibling modules
/// (e.g. `tui_prompt_relay` regression tests) can isolate the shared
/// prompt-anchor slot under `TEST_LOCK`.
#[cfg(test)]
pub(crate) fn reset_state_for_tests() {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    *state = TuiPromptDedupeState::default();
}

/// Test-only: record a prompt anchor whose `recorded_at` is backdated by `age`,
/// so a test can simulate an anchor stamped at submit time for a turn that has
/// been streaming for `age`. Crate-visible so sibling modules (e.g. the
/// `turn_bridge` same-input correlation tests) can pin that a long streaming
/// turn's anchor still resolves past the legacy 30min purge under
/// `PROMPT_ANCHOR_SUBMIT_TTL`.
#[cfg(test)]
pub(crate) fn record_prompt_anchor_aged_for_tests(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    message_id: u64,
    age: Duration,
) {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || channel_id == 0 || message_id == 0 {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.prompt_anchor_by_tmux.insert(
        PromptKey::new(&provider, tmux_session_name),
        TimedValue {
            value: TuiPromptAnchor {
                channel_id,
                message_id,
            },
            recorded_at: Instant::now().checked_sub(age).unwrap_or_else(Instant::now),
        },
    );
}

pub(crate) fn record_prompt_anchor(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    message_id: u64,
) {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || channel_id == 0 || message_id == 0 {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.prompt_anchor_by_tmux.insert(
        PromptKey::new(&provider, tmux_session_name),
        TimedValue {
            value: TuiPromptAnchor {
                channel_id,
                message_id,
            },
            recorded_at: Instant::now(),
        },
    );
}

pub(crate) fn take_prompt_anchor_for_response(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
) -> Option<TuiPromptAnchor> {
    let anchor = prompt_anchor_for_response(provider, tmux_session_name, channel_id)?;
    clear_prompt_anchor_for_response(provider, tmux_session_name, anchor);
    Some(anchor)
}

pub(crate) fn prompt_anchor_for_response(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
) -> Option<TuiPromptAnchor> {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || channel_id == 0 {
        return None;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(&provider, tmux_session_name);
    let anchor = state.prompt_anchor_by_tmux.get(&key)?.value;
    if anchor.channel_id != channel_id {
        return None;
    }
    Some(anchor)
}

pub(crate) fn clear_prompt_anchor_for_response(
    provider: &str,
    tmux_session_name: &str,
    anchor: TuiPromptAnchor,
) -> bool {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return false;
    }
    let removed = {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state.purge_expired();
        let key = PromptKey::new(&provider, tmux_session_name);
        let Some(current) = state
            .prompt_anchor_by_tmux
            .get(&key)
            .map(|entry| entry.value)
        else {
            return false;
        };
        if current != anchor {
            return false;
        }
        state.prompt_anchor_by_tmux.remove(&key);
        true
    };
    if removed {
        clear_ssh_direct_observation_pending(&provider, tmux_session_name);
    }
    removed
}

/// #3956: re-stamp an EXISTING submit prompt anchor's `recorded_at` to "now" on
/// observed streaming activity for `(provider, tmux, channel)`. A turn that
/// streams continuously longer than [`PROMPT_ANCHOR_SUBMIT_TTL`] (4h) would
/// otherwise have its anchor expire mid-stream, so the #3885 same-input
/// follow-up-requeue correlation peek ([`prompt_anchor_for_response`]) resolves
/// `None`, `same_input` reads false, and the no-response requeue re-fires
/// duplicate prose. The tmux watcher's per-pane streaming-observation path calls
/// this on every observed output chunk so the anchor stays live for the whole
/// turn, making the correlation TTL-independent (the issue #3956 full fix).
///
/// This is a REFRESH-on-activity, NOT a new lifecycle, and a SINGLE-MAP op:
///   * it only advances an anchor that ALREADY exists for the MATCHING channel —
///     it never resurrects a different channel's anchor and never CREATES one, so
///     a genuinely-unsubmitted pane stays anchor-less and the bridge still
///     requeues it;
///   * it reads/writes ONLY `prompt_anchor_by_tmux`. Crucially it does NOT call
///     [`TuiPromptDedupeState::purge_expired`]: this fires on EVERY watcher
///     chunk-drain (a #3016 hot path), so a global multi-map purge under the lock
///     would scan/mutate the #3459/#3303 `relayed_entry_ids_by_tmux` ledger and
///     every other dedupe map on each chunk. Leaving the ledger entirely untouched
///     is what makes the #3459/#3303 non-regression REAL, not merely benign — and
///     keeps the hot-path op cheap.
///
/// No-resurrection WITHOUT the global purge: the matching anchor's age is checked
/// INLINE against the 4h ceiling. A still-live anchor (< 4h) is re-stamped; a
/// matching anchor already past 4h belongs to a long-dead turn, so it is NOT
/// refreshed (and is evicted from this one map so it cannot linger). The peek path
/// [`prompt_anchor_for_response`] runs its OWN `purge_expired`, so anchor expiry is
/// still enforced there independently of this path.
/// Returns `true` iff a live matching-channel anchor was present and re-stamped.
pub(crate) fn touch_prompt_anchor_on_activity(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
) -> bool {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || channel_id == 0 {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    let key = PromptKey::new(&provider, tmux_session_name);
    let Some(entry) = state.prompt_anchor_by_tmux.get_mut(&key) else {
        return false;
    };
    if entry.value.channel_id != channel_id {
        return false;
    }
    if entry.recorded_at.elapsed() < PROMPT_ANCHOR_SUBMIT_TTL {
        // Live turn: re-stamp this one entry so the stream's anchor stays fresh.
        entry.recorded_at = Instant::now();
        return true;
    }
    // The matching anchor is already past the 4h ceiling — a long-dead turn's
    // anchor. Do NOT refresh it (no-resurrection guarantee); evict just this one
    // entry so it cannot linger, without scanning or mutating any other map.
    state.prompt_anchor_by_tmux.remove(&key);
    false
}

/// #3174: record a deferred ⏳-completion marker for `(provider, tmux, channel)`,
/// stamped with the TURN IDENTITY `turn_lease_generation`.
///
/// Called by the watcher's lease-gated completion path when the gate fired (the
/// external-input lease for THIS turn was present before relay) but the prompt
/// anchor for this turn has not been recorded yet — the provider committed
/// terminal output inside the sub-second `notify-post + ⏳-add` window. Without
/// this marker the anchor-less completion is a silent no-op and the ⏳ is
/// stranded (the lease is cleared after this delivery, so no later pass
/// reconciles it). The SAME turn's [`record_prompt_anchor`] drains this marker
/// (via [`take_deferred_anchor_completion`]) and the relay finishes the ⏳ → ✅
/// swap against the just-recorded anchor.
///
/// `turn_lease_generation` is the `generation` of the external-input lease the
/// completion was gated on (see [`ExternalInputRelayLease::generation`]) — a
/// unique monotonic per-record nonce that identifies the turn. The drain only
/// consumes a marker whose generation MATCHES the draining turn's, so within the
/// marker TTL a NEWER same-(provider,tmux) turn can never complete the previous
/// turn's ⏳. A marker stamped with the `UNRECORDED` sentinel (0) is never
/// recorded — it carries no turn identity, so it cannot be safely drained.
pub(crate) fn record_deferred_anchor_completion(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    turn_lease_generation: u64,
) {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty()
        || tmux_session_name.is_empty()
        || channel_id == 0
        || turn_lease_generation == EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED
    {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.deferred_anchor_completion_by_tmux.insert(
        PromptKey::new(&provider, tmux_session_name),
        TimedValue {
            value: turn_lease_generation,
            recorded_at: Instant::now(),
        },
    );
}

/// #3174: peek (read, do NOT clear) whether a deferred ⏳-completion marker for
/// `(provider, tmux)` matching `turn_lease_generation` is present. Returns `true`
/// iff a non-expired marker stamped with EXACTLY this turn's generation exists.
///
/// #3174 codex P2 (HTTP fail-open): the relay peeks BEFORE attempting the
/// ⏳ → ✅ delivery, so it can decide whether a swap is owed WITHOUT consuming the
/// marker. The marker is only removed via [`take_deferred_anchor_completion`]
/// once the swap can actually be delivered; if command_http is unavailable the
/// marker is left in place (mirrors the #3164 ⏳-add fail-open: never strand
/// worse than before).
pub(crate) fn deferred_anchor_completion_present_for_turn(
    provider: &str,
    tmux_session_name: &str,
    turn_lease_generation: u64,
) -> bool {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty()
        || tmux_session_name.is_empty()
        || turn_lease_generation == EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED
    {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .deferred_anchor_completion_by_tmux
        .get(&PromptKey::new(&provider, tmux_session_name))
        .is_some_and(|entry| entry.value == turn_lease_generation)
}

/// #3174: drain (read-and-clear) a deferred ⏳-completion marker for
/// `(provider, tmux)` IFF it is stamped with THIS turn's
/// `turn_lease_generation`. Returns `true` iff such a marker was present and was
/// removed.
///
/// Called by [`record_prompt_anchor`]'s site in the relay immediately after the
/// anchor is recorded (and ⏳ added). Turn-identity safe by construction: the
/// marker stores the `generation` of the lease the watcher completion was gated
/// on, and the relay passes the `generation` of the lease THIS same invocation
/// recorded. A marker set by a DIFFERENT turn (older or newer) on the same
/// `(provider, tmux)` carries a different generation and is left untouched — it
/// can never cross-complete the wrong turn's ⏳.
pub(crate) fn take_deferred_anchor_completion(
    provider: &str,
    tmux_session_name: &str,
    turn_lease_generation: u64,
) -> bool {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty()
        || tmux_session_name.is_empty()
        || turn_lease_generation == EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED
    {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(&provider, tmux_session_name);
    let matches = state
        .deferred_anchor_completion_by_tmux
        .get(&key)
        .is_some_and(|entry| entry.value == turn_lease_generation);
    if matches {
        state.deferred_anchor_completion_by_tmux.remove(&key);
    }
    matches
}

pub(crate) fn runtime_binding_for_tmux_session(
    tmux_session_name: &str,
) -> Option<TuiRuntimeBinding> {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return None;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .runtime_by_tmux
        .get(tmux_session_name)
        .map(|entry| entry.value.clone())
}

/// Adopt the actual Claude session UUID reported inside a hook payload while
/// retaining the launch-time UUID as a stable hook-routing alias (#4423).
///
/// Claude continuation keeps the tmux process and hook command alive but moves
/// transcript writes to a new `<uuid>.jsonl`.  The hook command therefore still
/// addresses the old UUID while stdin carries the new one.  This update is
/// deliberately limited to an existing ClaudeTui binding reached through the
/// command UUID and to a real sibling transcript file. For a second or later
/// continuation hop, the candidate must also be newer than the transcript
/// currently bound to that pane. It never guesses across project directories.
pub(crate) fn adopt_claude_continuation_session(
    command_session_id: &str,
    payload_session_id: &str,
) -> Option<(String, String)> {
    let command_session_id = command_session_id.trim();
    let payload_session_id = payload_session_id.trim();
    if command_session_id.is_empty()
        || payload_session_id.is_empty()
        || command_session_id == payload_session_id
        || uuid::Uuid::parse_str(payload_session_id).is_err()
    {
        return None;
    }

    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let command_key = PromptKey::new("claude", command_session_id);
    let tmux_session_name = state
        .tmux_by_provider_session
        .get(&command_key)?
        .value
        .clone();
    let binding = state.runtime_by_tmux.get(&tmux_session_name)?;
    if binding.value.runtime_kind != RuntimeHandoffKind::ClaudeTui {
        return None;
    }
    let old_output_path = PathBuf::from(&binding.value.output_path);
    let new_output_path = old_output_path
        .parent()?
        .join(format!("{payload_session_id}.jsonl"));
    if !new_output_path.is_file() {
        return None;
    }
    if let Some(current_session_id) = binding.value.session_id.as_deref()
        && current_session_id != command_session_id
        && current_session_id != payload_session_id
    {
        let current_mtime = std::fs::metadata(&old_output_path)
            .and_then(|metadata| metadata.modified())
            .ok()?;
        let candidate_mtime = std::fs::metadata(&new_output_path)
            .and_then(|metadata| metadata.modified())
            .ok()?;
        if candidate_mtime <= current_mtime {
            return None;
        }
    }
    let new_output_path = new_output_path.display().to_string();

    if binding.value.session_id.as_deref() == Some(payload_session_id)
        && binding.value.output_path == new_output_path
    {
        // Subsequent hooks still carry the launch-time query UUID. Do not reset
        // the already-adopted continuation cursor to zero on every event.
        state.tmux_by_provider_session.insert(
            PromptKey::new("claude", payload_session_id),
            TimedValue {
                value: tmux_session_name.clone(),
                recorded_at: Instant::now(),
            },
        );
        state.tmux_by_provider_session.insert(
            command_key,
            TimedValue {
                value: tmux_session_name.clone(),
                recorded_at: Instant::now(),
            },
        );
        return Some((tmux_session_name, new_output_path));
    }

    let binding = state.runtime_by_tmux.get_mut(&tmux_session_name)?;
    binding.value.output_path = new_output_path.clone();
    binding.value.relay_output_path = None;
    binding.value.session_id = Some(payload_session_id.to_string());
    // Start conservatively at the new transcript head. Stable prompt-entry
    // identities suppress replay; starting at EOF would silently skip the
    // continuation boundary that taught us the new UUID.
    binding.value.last_offset = 0;
    binding.value.relay_last_offset = None;
    binding.recorded_at = Instant::now();
    state.tmux_by_provider_session.insert(
        PromptKey::new("claude", payload_session_id),
        TimedValue {
            value: tmux_session_name.clone(),
            recorded_at: Instant::now(),
        },
    );
    // The running Claude process can cache the launch-time hook command even
    // after its settings artifact is rewritten. Keep that observed command
    // identity newest for future waits until dcserver rehydration establishes
    // the persisted payload UUID as the sole mapping.
    state.tmux_by_provider_session.insert(
        command_key,
        TimedValue {
            value: tmux_session_name.clone(),
            recorded_at: Instant::now(),
        },
    );
    Some((tmux_session_name, new_output_path))
}

pub(crate) fn refresh_tmux_runtime_binding_activity(
    tmux_session_name: &str,
    output_path: &str,
) -> bool {
    let tmux_session_name = tmux_session_name.trim();
    let output_path = output_path.trim();
    if tmux_session_name.is_empty() || output_path.is_empty() {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let Some(entry) = state.runtime_by_tmux.get_mut(tmux_session_name) else {
        return false;
    };
    if entry.value.output_path == output_path
        || entry.value.relay_output_path.as_deref() == Some(output_path)
    {
        entry.recorded_at = Instant::now();
        return true;
    }
    false
}

pub(crate) fn clear_tmux_runtime_binding(tmux_session_name: &str) -> bool {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return false;
    }
    let removed = {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state.purge_expired();
        let removed_runtime = state.runtime_by_tmux.remove(tmux_session_name).is_some();
        let removed_provider_sessions =
            state.remove_provider_session_mappings_for_tmux(tmux_session_name);
        removed_runtime || removed_provider_sessions
    };
    crate::services::claude_compact_context::clear_launch_provenance_for_tmux(tmux_session_name);
    crate::services::claude_compact_trigger::clear_for_tmux(tmux_session_name);
    removed
}

/// #3105 (codex P1 sub-case B): tombstone-evict every mirror mapping for a tmux
/// session that has been determined dead/orphaned (pane gone AND no live watcher
/// AND no authoritative owner). This removes BOTH the runtime binding (which the
/// idle relay loop iterates) AND the best-effort channel mirror (which the
/// drift-alert resolver reads), so subsequent relay-loop iterations no longer
/// find a stale mapping and stop re-emitting the per-poll drift/skip WARN.
///
/// This is NOT a routing authority change: it only forgets a mirror entry whose
/// session is genuinely gone. A later legitimate re-registration (launch script
/// rehydrate or a fresh watcher) re-populates these maps normally, so a session
/// that comes back relays again.
///
/// Returns `true` when at least one mirror entry was removed (so callers can
/// emit a single bounded incident instead of per-poll spam).
pub(crate) fn evict_dead_tmux_mirror(tmux_session_name: &str) -> bool {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return false;
    }
    let removed = {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state.purge_expired();
        let removed_runtime = state.runtime_by_tmux.remove(tmux_session_name).is_some();
        let removed_channel = state.channel_by_tmux.remove(tmux_session_name).is_some();
        let removed_provider_sessions =
            state.remove_provider_session_mappings_for_tmux(tmux_session_name);
        removed_runtime || removed_channel || removed_provider_sessions
    };
    crate::services::claude_compact_context::clear_launch_provenance_for_tmux(tmux_session_name);
    crate::services::claude_compact_trigger::clear_for_tmux(tmux_session_name);
    removed
}

pub(crate) fn runtime_bindings_for_kind(
    runtime_kind: RuntimeHandoffKind,
) -> Vec<(String, TuiRuntimeBinding)> {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .runtime_by_tmux
        .iter()
        .filter(|(_, entry)| entry.value.runtime_kind == runtime_kind)
        .map(|(tmux_session_name, entry)| (tmux_session_name.clone(), entry.value.clone()))
        .collect()
}

pub(crate) fn advance_tmux_runtime_binding_offset(
    tmux_session_name: &str,
    output_path: &str,
    last_offset: u64,
) -> bool {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() || output_path.trim().is_empty() {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let Some(entry) = state.runtime_by_tmux.get_mut(tmux_session_name) else {
        return false;
    };
    if entry.value.output_path == output_path {
        entry.value.last_offset = last_offset;
        if entry.value.relay_output_path.is_none() {
            entry.value.relay_last_offset = Some(last_offset);
        }
        entry.recorded_at = Instant::now();
        return true;
    }
    if entry.value.relay_output_path.as_deref() != Some(output_path) {
        return false;
    }
    entry.value.relay_last_offset = Some(last_offset);
    entry.recorded_at = Instant::now();
    true
}

pub fn record_discord_originated_prompt(provider: &str, tmux_session_name: &str, prompt: &str) {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() || prompt.trim().is_empty() {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .pending_by_tmux
        .entry(PromptKey::new(provider, tmux_session_name))
        .or_default()
        .push_back(TimedValue {
            value: prompt.to_string(),
            recorded_at: Instant::now(),
        });
}

pub fn remove_discord_originated_prompt(provider: &str, tmux_session_name: &str, prompt: &str) {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(provider, tmux_session_name);
    let Some(queue) = state.pending_by_tmux.get_mut(&key) else {
        return;
    };
    if let Some(index) = queue
        .iter()
        .position(|pending| prompts_match(&pending.value, prompt))
    {
        queue.remove(index);
    }
    if queue.is_empty() {
        state.pending_by_tmux.remove(&key);
    }
}

pub fn observe_prompt_by_provider_session(
    provider: &str,
    provider_session_id: &str,
    prompt: &str,
) -> PromptObservation {
    observe_prompt_by_provider_session_at(provider, provider_session_id, prompt, Utc::now())
}

pub fn observe_prompt_by_provider_session_at(
    provider: &str,
    provider_session_id: &str,
    prompt: &str,
    observed_at: DateTime<Utc>,
) -> PromptObservation {
    let tmux_session_name = resolve_tmux_session_name(provider, provider_session_id)
        .unwrap_or_else(|| provider_session_id.trim().to_string());
    observe_prompt_by_tmux_at(provider, &tmux_session_name, prompt, observed_at)
}

pub fn observe_prompt_by_tmux(
    provider: &str,
    tmux_session_name: &str,
    prompt: &str,
) -> PromptObservation {
    observe_prompt_by_tmux_at(provider, tmux_session_name, prompt, Utc::now())
}

pub fn observe_prompt_by_tmux_at(
    provider: &str,
    tmux_session_name: &str,
    prompt: &str,
    observed_at: DateTime<Utc>,
) -> PromptObservation {
    observe_prompt_candidates_by_tmux_inner(
        provider,
        tmux_session_name,
        &[prompt.to_string()],
        None,
        PromptObservationEffect::NotifyAndLease,
        observed_at,
    )
}

/// #3540: same as [`observe_prompt_by_tmux_at`] but carries the prompt's stable
/// JSONL entry identity (`uuid`). When `entry_id` is `Some` AND that uuid was
/// already relayed for this `(provider, tmux)` pair the call returns
/// [`PromptObservation::SuppressedReplayedEntry`] BEFORE any synthetic turn is
/// minted — closing the watermark-reset / jsonl-head-rotation re-claim window
/// that the 30s content dedup leaves open. `entry_id == None` falls back to the
/// pre-#3540 content-keyed path (no behavior change).
pub fn observe_prompt_by_tmux_with_entry_id_at(
    provider: &str,
    tmux_session_name: &str,
    prompt: &str,
    entry_id: Option<&str>,
    observed_at: DateTime<Utc>,
) -> PromptObservation {
    observe_prompt_candidates_by_tmux_inner(
        provider,
        tmux_session_name,
        &[prompt.to_string()],
        entry_id,
        PromptObservationEffect::NotifyAndLease,
        observed_at,
    )
}

pub fn observe_prompt_candidates_by_tmux(
    provider: &str,
    tmux_session_name: &str,
    prompts: &[String],
) -> PromptObservation {
    observe_prompt_candidates_by_tmux_inner(
        provider,
        tmux_session_name,
        prompts,
        None,
        PromptObservationEffect::NotifyAndLease,
        Utc::now(),
    )
}

pub(crate) fn observe_prompt_candidates_by_tmux_for_relay_lease(
    provider: &str,
    tmux_session_name: &str,
    prompts: &[String],
) -> PromptObservation {
    observe_prompt_candidates_by_tmux_inner(
        provider,
        tmux_session_name,
        prompts,
        None,
        PromptObservationEffect::RelayLeaseOnly,
        Utc::now(),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptObservationEffect {
    NotifyAndLease,
    RelayLeaseOnly,
}

fn observe_prompt_candidates_by_tmux_inner(
    provider: &str,
    tmux_session_name: &str,
    prompts: &[String],
    entry_id: Option<&str>,
    effect: PromptObservationEffect,
    observed_at: DateTime<Utc>,
) -> PromptObservation {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    let entry_id = entry_id.map(str::trim).filter(|value| !value.is_empty());
    let mut candidates = Vec::new();
    for prompt in prompts {
        let prompt = prompt.trim();
        // #3527: skip AgentDesk's own `[User: … (ID: …)]` Discord-relay lines so a
        // re-observation (after the discord-originated ledger entry was consumed)
        // never publishes a spurious SSH-direct turn. Treated like other synthetic
        // prompts → candidates stay empty → `PromptObservation::Ignored`.
        if prompt.is_empty()
            || is_synthetic_tui_user_prompt_for_provider(&provider, prompt)
            || (is_discord_relayed_user_prompt(prompt)
                && !is_user_prefixed_subagent_notification_machine_event(prompt))
        {
            continue;
        }
        if !candidates
            .iter()
            .any(|candidate: &String| prompts_match(candidate, prompt))
        {
            candidates.push(prompt.to_string());
        }
    }
    if provider.is_empty() || tmux_session_name.is_empty() || candidates.is_empty() {
        return PromptObservation::Ignored;
    }
    // #4567: structured task lifecycle records are status events, not positive
    // user-input provenance. Publish them for the task-card/status observer, but
    // deliberately bypass entry-id, pending, recent, lease, and SSH markers.
    if candidates
        .iter()
        .any(|prompt| is_start_anchored_task_notification_prompt(prompt))
    {
        let prompt = candidates
            .iter()
            .find(|prompt| is_start_anchored_task_notification_prompt(prompt))
            .expect("task notification candidate")
            .to_string();
        let event = ObservedTuiPrompt {
            provider,
            tmux_session_name: tmux_session_name.to_string(),
            prompt,
            source_event_id: entry_id.map(str::to_string),
            observed_at,
            external_input_lease_generation: EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
            ssh_direct_observation_generation: SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
        };
        let _ = OBSERVED_PROMPTS.send(event);
        return PromptObservation::PublishedTaskNotification;
    }
    // #3540 (root cause): suppress by STABLE entry identity BEFORE any pending /
    // recent / lease bookkeeping or synthetic-turn mint. If this JSONL entry
    // `uuid` was already relayed for this `(provider, tmux)` pair it is a
    // re-encounter from a watermark reset / jsonl head rotation, NOT a new
    // submission — return early so the scanner never mints a phantom synthetic
    // inflight. This check inspects ONLY the relayed-entry ledger; it never
    // reads inflight / EOF / current_msg_id, so it cannot mis-handle a slow
    // genuine turn. A genuinely new prompt has a fresh uuid (absent from the
    // ledger) and is never suppressed here.
    if let Some(entry_id) = entry_id {
        if relayed_entry_id_already_seen(&provider, tmux_session_name, entry_id) {
            return PromptObservation::SuppressedReplayedEntry;
        }
    }
    let local_only_control = candidates
        .first()
        .and_then(|prompt| classify_local_only_slash_control(prompt));
    if local_only_control.is_none() {
        for prompt in &candidates {
            if take_matching_pending_prompt(&provider, tmux_session_name, prompt) {
                return PromptObservation::SuppressedDiscordDuplicate;
            }
        }
        for prompt in &candidates {
            if take_or_record_recent_observed_prompt(&provider, tmux_session_name, prompt) {
                return PromptObservation::SuppressedRecentDuplicate;
            }
        }
    }
    // Generic direct input keeps the #3540 eager identity record: it is a real
    // relay at this point. A local-only note has no durable side effect until
    // Discord accepts its note, so its id is recorded only by the successful
    // delivery branch in `tui_prompt_relay`.
    if local_only_control.is_none() {
        if let Some(entry_id) = entry_id {
            record_relayed_entry_id(&provider, tmux_session_name, entry_id);
        }
    }
    if effect == PromptObservationEffect::RelayLeaseOnly {
        if local_only_control.is_none() {
            record_external_input_turn_lease(
                &provider,
                tmux_session_name,
                ExternalInputRelayLease::unassigned(None),
            );
        }
        return PromptObservation::PublishedSshDirect;
    }
    let (external_input_lease_generation, ssh_direct_observation_generation) =
        if local_only_control.is_some() {
            (
                EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
                SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
            )
        } else {
            let external_input_lease = record_external_input_turn_lease(
                &provider,
                tmux_session_name,
                ExternalInputRelayLease::unassigned(None),
            );
            (
                external_input_lease.generation,
                mark_ssh_direct_observation_pending(&provider, tmux_session_name),
            )
        };
    let prompt = candidates
        .first()
        .expect("non-empty candidates")
        .to_string();
    let event = ObservedTuiPrompt {
        provider,
        tmux_session_name: tmux_session_name.to_string(),
        prompt,
        source_event_id: entry_id.map(str::to_string),
        observed_at,
        external_input_lease_generation,
        ssh_direct_observation_generation,
    };
    let _ = OBSERVED_PROMPTS.send(event);
    PromptObservation::PublishedSshDirect
}

pub(crate) fn record_external_input_relay_lease(
    provider: &str,
    tmux_session_name: &str,
    channel_id: Option<u64>,
) {
    record_external_input_turn_lease(
        provider,
        tmux_session_name,
        ExternalInputRelayLease::unassigned(channel_id),
    );
}

/// Record an external-input relay lease for `(provider, tmux_session)` and return
/// the EXACT lease that was stored, including the unique `generation` stamped at
/// record time. Callers that need to later release THIS lease (e.g. an RAII
/// guard) MUST capture the returned value, not the pre-record argument: only the
/// returned value carries the recorded generation that
/// [`clear_external_input_relay_lease_if_matches`] /
/// [`clear_external_input_relay_lease_if_generation_matches`] compare against, so
/// a guard never clobbers a newer (even value-identical `Unassigned`) lease.
pub(crate) fn record_external_input_turn_lease(
    provider: &str,
    tmux_session_name: &str,
    mut lease: ExternalInputRelayLease,
) -> ExternalInputRelayLease {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return lease;
    }
    // Stamp a UNIQUE generation at the moment of record so two otherwise
    // value-equal leases for the same key are distinguishable by identity.
    lease.generation = next_external_input_relay_lease_generation();
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.external_input_relay_lease_by_tmux.insert(
        PromptKey::new(&provider, tmux_session_name),
        TimedValue {
            value: lease.clone(),
            recorded_at: Instant::now(),
        },
    );
    lease
}

pub(crate) fn external_input_relay_lease(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
) -> Option<ExternalInputRelayLease> {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || channel_id == 0 {
        return None;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .external_input_relay_lease_by_tmux
        .get(&PromptKey::new(&provider, tmux_session_name))
        .and_then(|entry| match entry.value.channel_id {
            Some(leased) if leased != channel_id => None,
            _ => Some(entry.value.clone()),
        })
}

pub(crate) fn external_input_relay_lease_present(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
) -> bool {
    external_input_relay_lease(provider, tmux_session_name, channel_id).is_some()
}

pub(crate) fn clear_external_input_relay_lease(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
) -> bool {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || channel_id == 0 {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(&provider, tmux_session_name);
    let Some(entry) = state.external_input_relay_lease_by_tmux.get(&key) else {
        return false;
    };
    if entry
        .value
        .channel_id
        .is_some_and(|leased| leased != channel_id)
    {
        return false;
    }
    state.external_input_relay_lease_by_tmux.remove(&key);
    true
}

pub(crate) fn clear_external_input_relay_lease_if_matches(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    expected: &ExternalInputRelayLease,
) -> bool {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || channel_id == 0 {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(&provider, tmux_session_name);
    let Some(entry) = state.external_input_relay_lease_by_tmux.get(&key) else {
        return false;
    };
    if entry
        .value
        .channel_id
        .is_some_and(|leased| leased != channel_id)
    {
        return false;
    }
    if &entry.value != expected {
        return false;
    }
    state.external_input_relay_lease_by_tmux.remove(&key);
    true
}

/// Compare-and-clear the external-input relay lease for `(provider, tmux_session)`
/// by its UNIQUE `generation` (and channel scope) rather than by full value.
///
/// This is the no-clobber primitive for the RAII release guards: the guard
/// captures the generation of the EXACT lease it observed/recorded and on Drop
/// clears only that generation. Two value-identical `Unassigned` leases (all
/// trace fields `None`) for the same key receive distinct generations at record
/// time, so an OLD guard's Drop leaves a NEWER lease — with a different
/// generation — untouched. A guard whose captured lease was never recorded
/// (generation == [`EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED`]) clears
/// nothing.
pub(crate) fn clear_external_input_relay_lease_if_generation_matches(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    expected_generation: u64,
) -> bool {
    if expected_generation == EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED {
        return false;
    }
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || channel_id == 0 {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(&provider, tmux_session_name);
    let Some(entry) = state.external_input_relay_lease_by_tmux.get(&key) else {
        return false;
    };
    if entry
        .value
        .channel_id
        .is_some_and(|leased| leased != channel_id)
    {
        return false;
    }
    if entry.value.generation != expected_generation {
        return false;
    }
    state.external_input_relay_lease_by_tmux.remove(&key);
    true
}

/// Compare-and-clear an external-input lease by generation without requiring a
/// channel binding. Direct prompt observation records an initially unassigned
/// lease before relay ownership has been resolved, so a consumed machine
/// control needs this exact unscoped cleanup path.
fn clear_external_input_relay_lease_if_generation_matches_unscoped(
    provider: &str,
    tmux_session_name: &str,
    expected_generation: u64,
) -> bool {
    if expected_generation == EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED {
        return false;
    }
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(&provider, tmux_session_name);
    if state
        .external_input_relay_lease_by_tmux
        .get(&key)
        .is_none_or(|entry| entry.value.generation != expected_generation)
    {
        return false;
    }
    state.external_input_relay_lease_by_tmux.remove(&key);
    true
}

fn mark_ssh_direct_observation_pending(provider: &str, tmux_session_name: &str) -> u64 {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED;
    }
    let generation = next_ssh_direct_observation_generation();
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.ssh_direct_observation_by_tmux.insert(
        PromptKey::new(&provider, tmux_session_name),
        TimedValue {
            value: generation,
            recorded_at: Instant::now(),
        },
    );
    generation
}

/// True when an SSH-direct prompt has been observed for this
/// `(provider, tmux_session)` pair within `SSH_DIRECT_OBSERVATION_TTL` and
/// the matching anchor has not yet been consumed. Watchers use this to keep
/// the post-terminal suppress guard from killing legitimate direct-input
/// responses during the brief window before `record_prompt_anchor` lands.
pub(crate) fn is_ssh_direct_observation_pending(provider: &str, tmux_session_name: &str) -> bool {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .ssh_direct_observation_by_tmux
        .contains_key(&PromptKey::new(&provider, tmux_session_name))
}

fn clear_ssh_direct_observation_pending(provider: &str, tmux_session_name: &str) {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state
        .ssh_direct_observation_by_tmux
        .remove(&PromptKey::new(&provider, tmux_session_name));
}

fn clear_ssh_direct_observation_pending_if_generation_matches(
    provider: &str,
    tmux_session_name: &str,
    expected_generation: u64,
) -> bool {
    if expected_generation == SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED {
        return false;
    }
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(&provider, tmux_session_name);
    if state
        .ssh_direct_observation_by_tmux
        .get(&key)
        .is_none_or(|entry| entry.value != expected_generation)
    {
        return false;
    }
    state.ssh_direct_observation_by_tmux.remove(&key);
    true
}

pub(crate) fn record_suppressed_discord_origin_prompt(
    provider: &str,
    tmux_session_name: &str,
    prompt: &str,
) {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || prompt.trim().is_empty() {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.record_recent_observed_prompt(&provider, tmux_session_name, prompt);
}

pub fn extract_prompt_from_hook_payload(payload: &Value) -> Option<String> {
    for key in [
        "prompt",
        "user_prompt",
        "userPrompt",
        "message",
        "text",
        "input",
    ] {
        if let Some(prompt) = payload
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(prompt.to_string());
        }
    }
    payload
        .get("payload")
        .and_then(extract_prompt_from_hook_payload)
}

pub fn extract_codex_rollout_user_prompt(json: &Value) -> Option<String> {
    extract_codex_rollout_user_prompt_with_entry_id(json).map(|(prompt, _)| prompt)
}

pub fn extract_codex_rollout_user_prompt_with_entry_id(
    json: &Value,
) -> Option<(String, Option<String>)> {
    let payload = json.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("message")
        || payload.get("role").and_then(Value::as_str) != Some("user")
    {
        return None;
    }
    let prompt = reject_synthetic_tui_user_prompt(extract_message_content_text(payload)?)?;
    let entry_id = extract_codex_rollout_entry_id(json, payload);
    Some((prompt, entry_id))
}

fn extract_codex_rollout_entry_id(json: &Value, payload: &Value) -> Option<String> {
    payload
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| payload.get("item_id").and_then(Value::as_str))
        .or_else(|| json.get("id").and_then(Value::as_str))
        .or_else(|| json.get("item_id").and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub fn extract_claude_transcript_user_prompt(json: &Value) -> Option<String> {
    extract_claude_transcript_user_prompt_with_entry_id(json).map(|(prompt, _)| prompt)
}

/// #3540: same extraction as [`extract_claude_transcript_user_prompt`], but also
/// returns the JSONL entry's STABLE identity (`uuid`) when present.
///
/// Claude Code stamps every transcript `user` entry with a content-stable
/// top-level `uuid` (measured: ~18k user uuids across ~3.8k transcript files
/// with ZERO cross-file collisions — it is a genuine per-entry identity, not a
/// timestamp derivative). The relay-watermark reset path (`/relay-scan`
/// self-loop + jsonl head rotation) re-presents an already-relayed prompt at a
/// shifted byte offset; the rotation is a `truncate_jsonl_head_safe` rename
/// (head clipped, surviving bytes preserved verbatim), so the SAME logical
/// prompt keeps its uuid even though its offset moved. The idle-transcript
/// scanner threads this uuid into the dedupe layer so an already-relayed entry
/// is suppressed by IDENTITY (see [`observe_prompt_candidates_by_tmux`]) without
/// ever inspecting inflight / EOF / current_msg_id — sidestepping the
/// observationally-indistinguishable phantom-vs-slow-live-turn problem entirely.
///
/// Defensive extraction: the uuid is read from the top-level object (where
/// Claude Code places it for `user` entries) with a `message.uuid` fallback for
/// forward/backward tolerance. A missing uuid yields `None`, in which case the
/// scanner falls back to the existing content-keyed 30s recent-observed dedup —
/// no regression, just the same window as before #3540.
pub fn extract_claude_transcript_user_prompt_with_entry_id(
    json: &Value,
) -> Option<(String, Option<String>)> {
    if json.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    if json
        .get("isMeta")
        .and_then(Value::as_bool)
        .is_some_and(|is_meta| is_meta)
    {
        return None;
    }
    let message = json.get("message")?;
    if message
        .get("role")
        .and_then(Value::as_str)
        .is_some_and(|role| role != "user")
    {
        return None;
    }
    let prompt = reject_synthetic_claude_user_prompt(extract_message_content_text(message)?)?;
    let entry_id = extract_claude_transcript_entry_id(json, message);
    Some((prompt, entry_id))
}

/// #3540: pull the stable entry identity for a Claude transcript `user` entry.
/// Prefers the top-level `uuid` (where Claude Code writes it), falls back to a
/// `message.uuid` if a future format ever moves it. Returns a normalized,
/// non-empty `String` or `None` (the scanner treats `None` as "no stable
/// identity available — use the content-keyed fallback").
fn extract_claude_transcript_entry_id(json: &Value, message: &Value) -> Option<String> {
    json.get("uuid")
        .and_then(Value::as_str)
        .or_else(|| message.get("uuid").and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub fn extract_qwen_jsonl_user_prompt(json: &Value) -> Option<String> {
    if json.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let message = json.get("message")?;
    if message
        .get("role")
        .and_then(Value::as_str)
        .is_some_and(|role| role != "user")
    {
        return None;
    }
    reject_synthetic_tui_user_prompt(extract_message_content_text(message)?)
}

/// #3527: `[User: <author> (ID: <digits>)] …` is AgentDesk's OWN Discord→TUI
/// relay format (`discord/router/response_format.rs`), never an external SSH/cron
/// injection — so the observer must not mint a synthetic turn for a re-observed
/// one (the discord-originated ledger only suppresses the first, consumed/
/// TTL-bounded sighting; a quiescence-timeout re-observation slips through).
///
/// The marker can be PRECEDED by prepended context (`[External Recall]`, reply/
/// upload context, …) AND can be collapsed mid-line: the legacy pane observer
/// (`tmux_watcher/prompt_observe.rs`) submits `join("")` / `join(" ")` /
/// `join("\n")` variants of one block, so a line-anchored check would miss the
/// collapsed ones (codex #3527). Scan the WHOLE string: find `[User: `, then any
/// following `(ID: <digits>)]` (author may itself contain parens).
fn is_discord_relayed_user_prompt(prompt: &str) -> bool {
    let Some(user_at) = prompt.find("[User: ") else {
        return false;
    };
    let mut tail = &prompt[user_at + "[User: ".len()..];
    while let Some(id_at) = tail.find("(ID: ") {
        let after_id = &tail[id_at + "(ID: ".len()..];
        if let Some(close) = after_id.find(")]") {
            let digits = &after_id[..close];
            if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return true;
            }
        }
        tail = &tail[id_at + "(ID: ".len()..];
    }
    false
}

fn is_user_prefixed_subagent_notification_machine_event(prompt: &str) -> bool {
    let mut current = prompt.trim_start();
    let mut saw_user_prefix = false;

    loop {
        if let Some(tail) = strip_provider_session_reuse_prologue(current) {
            current = tail.trim_start();
            continue;
        }

        let stripped_chrome = strip_leading_tui_response_chrome(current);
        if stripped_chrome != current {
            current = stripped_chrome.trim_start();
            continue;
        }

        if let Some(tail) = strip_leading_user_author_prefix(current) {
            saw_user_prefix = true;
            current = tail.trim_start();
            continue;
        }

        break;
    }

    saw_user_prefix && starts_with_xmlish_tag(current.trim_start(), "subagent_notification")
}

fn strip_provider_session_reuse_prologue(normalized: &str) -> Option<&str> {
    const RESUMED_THREAD_PROLOGUE: &str = "The prior authoritative Discord, role, and tool \
         instructions already present in this Codex thread still apply. Treat only this turn's \
         user request, reply context, uploaded files, and memory recall below as new actionable \
         input.";
    const FRESH_FORK_PROLOGUE: &str = "The prior authoritative Discord, role, and tool \
         instructions already issued to this role in the current dcserver lifetime still apply. \
         Treat only this turn's user request, reply context, uploaded files, and memory recall \
         below as new actionable input.";

    let rest = normalized
        .strip_prefix("[Provider Session Reuse]")?
        .trim_start();
    provider_reuse_tail(rest, RESUMED_THREAD_PROLOGUE)
        .or_else(|| provider_reuse_tail(rest, FRESH_FORK_PROLOGUE))
}

fn provider_reuse_tail<'a>(rest: &'a str, prologue: &str) -> Option<&'a str> {
    rest.strip_prefix(prologue)
        .and_then(|tail| tail.strip_prefix("\n\n"))
}

fn strip_leading_tui_response_chrome(input: &str) -> &str {
    let mut stripped = input;
    loop {
        let trimmed = stripped.trim_start();
        if let Some(rest) = trimmed.strip_prefix("No response requested.")
            && (rest.is_empty()
                || rest.starts_with('\n')
                || rest.starts_with('\r')
                || rest.chars().next().is_some_and(|ch| !ch.is_whitespace()))
        {
            stripped = rest;
            continue;
        }
        return trimmed;
    }
}

fn strip_leading_user_author_prefix(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("[User: ")?;
    let close = rest.find(']')?;
    Some(rest[close + 1..].trim_start())
}

fn starts_with_xmlish_tag(text: &str, tag: &str) -> bool {
    let Some(rest) = text.strip_prefix('<') else {
        return false;
    };
    let Some(rest) = rest.strip_prefix(tag) else {
        return false;
    };
    rest.starts_with('>') || rest.chars().next().is_some_and(char::is_whitespace)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptObservation {
    PublishedSshDirect,
    /// A structured `<task-notification>` was published for status/card
    /// rendering without creating external-input ownership or a response tail.
    PublishedTaskNotification,
    SuppressedDiscordDuplicate,
    SuppressedRecentDuplicate,
    /// #3540: the observed prompt's stable JSONL entry `uuid` was ALREADY relayed
    /// for this `(provider, tmux)` pair. Distinct from
    /// [`Self::SuppressedRecentDuplicate`]: that is a content match bounded by the
    /// 30s recent window, whereas this is an IDENTITY match bounded only by the
    /// 30min entry-id TTL. The idle-transcript scanner treats it like the other
    /// suppressions — `should_tail_response == false` — so a re-encountered
    /// already-relayed entry (watermark reset / jsonl head rotation) never mints a
    /// phantom synthetic inflight. A genuinely new prompt carries a new uuid and
    /// is never returned here.
    SuppressedReplayedEntry,
    Ignored,
}

fn resolve_tmux_session_name(provider: &str, provider_session_id: &str) -> Option<String> {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .tmux_by_provider_session
        .get(&PromptKey::new(provider, provider_session_id))
        .map(|entry| entry.value.clone())
}

fn take_matching_pending_prompt(provider: &str, tmux_session_name: &str, prompt: &str) -> bool {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(provider, tmux_session_name);
    let Some(queue) = state.pending_by_tmux.get_mut(&key) else {
        return false;
    };
    let matched = queue
        .iter()
        .position(|pending| prompts_match(&pending.value, prompt));
    if let Some(index) = matched {
        queue.remove(index);
    }
    if queue.is_empty() {
        state.pending_by_tmux.remove(&key);
    }
    if matched.is_some() {
        state.record_recent_observed_prompt(provider, tmux_session_name, prompt);
    }
    matched.is_some()
}

fn take_or_record_recent_observed_prompt(
    provider: &str,
    tmux_session_name: &str,
    prompt: &str,
) -> bool {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(provider, tmux_session_name);
    let queue = state.recent_observed_by_tmux.entry(key).or_default();
    if queue
        .iter()
        .any(|observed| prompts_match(&observed.value, prompt))
    {
        return true;
    }
    state.record_recent_observed_prompt(provider, tmux_session_name, prompt);
    false
}

/// #3540: `true` iff `entry_id` is in the already-relayed ledger for this
/// `(provider, tmux)` pair (and not yet purged). Read-only — recording happens
/// separately in [`record_relayed_entry_id`] at the actual relay point.
fn relayed_entry_id_already_seen(provider: &str, tmux_session_name: &str, entry_id: &str) -> bool {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .relayed_entry_ids_by_tmux
        .get(&PromptKey::new(provider, tmux_session_name))
        .is_some_and(|queue| queue.iter().any(|seen| seen.value == entry_id))
}

/// #3540: record `entry_id` as relayed for this `(provider, tmux)` pair. Called
/// only at the actual relay point (after pending/recent dedup pass), so a
/// dedup-suppressed candidate is never mis-recorded as relayed. Idempotent: a
/// re-record of an id already present refreshes nothing and does not duplicate
/// (the identity check would have short-circuited the caller anyway). Ring-capped
/// per key at [`RELAYED_ENTRY_ID_RING_CAP`] (oldest dropped first); TTL-purged by
/// `PROMPT_ANCHOR_TTL`.
fn record_relayed_entry_id(provider: &str, tmux_session_name: &str, entry_id: &str) {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let queue = state
        .relayed_entry_ids_by_tmux
        .entry(PromptKey::new(provider, tmux_session_name))
        .or_default();
    if queue.iter().any(|seen| seen.value == entry_id) {
        return;
    }
    queue.push_back(TimedValue {
        value: entry_id.to_string(),
        recorded_at: Instant::now(),
    });
    while queue.len() > RELAYED_ENTRY_ID_RING_CAP {
        queue.pop_front();
    }
}

fn record_local_only_entry_id(prompt: &ObservedTuiPrompt) {
    if classify_local_only_slash_control(&prompt.prompt).is_none() {
        return;
    }
    let Some(entry_id) = prompt
        .source_event_id
        .as_deref()
        .map(str::trim)
        .filter(|entry_id| !entry_id.is_empty())
    else {
        return;
    };
    record_relayed_entry_id(&prompt.provider, &prompt.tmux_session_name, entry_id);
}

/// Mark a local-only entry as replayed after its Discord session note was accepted.
pub(crate) fn record_local_only_entry_id_after_note_delivery(prompt: &ObservedTuiPrompt) {
    record_local_only_entry_id(prompt);
}

/// Seal a local-only transcript half collapsed into an already-rendered note.
pub(crate) fn seal_deduped_local_only_entry_id(prompt: &ObservedTuiPrompt) {
    record_local_only_entry_id(prompt);
}

pub(crate) fn prompts_match(expected: &str, observed: &str) -> bool {
    let expected_trimmed = normalize_line_endings(expected).trim().to_string();
    let observed_trimmed = normalize_line_endings(observed).trim().to_string();
    if expected_trimmed == observed_trimmed {
        return true;
    }
    if let (Some(expected_command), Some(observed_command)) = (
        slash_command_prompt_key(&expected_trimmed),
        slash_command_prompt_key(&observed_trimmed),
    ) {
        if expected_command == observed_command {
            return true;
        }
    }
    let expected_fuzzy = fuzzy_prompt_key(&expected_trimmed);
    let observed_fuzzy = fuzzy_prompt_key(&observed_trimmed);
    if expected_fuzzy == observed_fuzzy {
        return true;
    }
    false
}

fn normalize_line_endings(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

fn fuzzy_prompt_key(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

#[derive(Debug, PartialEq, Eq)]
struct SlashCommandPromptKey {
    name: String,
    args: String,
}

fn slash_command_prompt_key(value: &str) -> Option<SlashCommandPromptKey> {
    slash_command_xml_prompt_key(value).or_else(|| slash_command_invocation_prompt_key(value))
}

fn slash_command_xml_prompt_key(value: &str) -> Option<SlashCommandPromptKey> {
    let trimmed = value.trim();
    if !(trimmed.starts_with("<command-message>") || trimmed.starts_with("<command-name>")) {
        return None;
    }
    let command_name = extract_xml_tag(trimmed, "command-name")?;
    let (name, name_args) = parse_slash_command_invocation(command_name)?;
    let args = extract_xml_tag(trimmed, "command-args")
        .and_then(non_empty)
        .unwrap_or(name_args);
    Some(SlashCommandPromptKey {
        name,
        args: fuzzy_prompt_key(&args),
    })
}

fn slash_command_invocation_prompt_key(value: &str) -> Option<SlashCommandPromptKey> {
    let (name, args) = parse_slash_command_invocation(value)?;
    Some(SlashCommandPromptKey {
        name,
        args: fuzzy_prompt_key(&args),
    })
}

fn parse_slash_command_invocation(value: &str) -> Option<(String, String)> {
    let trimmed = value.trim();
    let (name, args) = match trimmed.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args),
        None => (trimmed, ""),
    };
    if !name.starts_with('/') || name.len() <= 1 {
        return None;
    }
    Some((name.to_ascii_lowercase(), args.trim().to_string()))
}

fn extract_xml_tag<'a>(value: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let after_open = value.split_once(&open)?.1;
    let (body, _) = after_open.split_once(&close)?;
    Some(body.trim())
}

fn normalize_provider(provider: &str) -> String {
    provider.trim().to_ascii_lowercase()
}

fn extract_message_content_text(payload: &Value) -> Option<String> {
    match payload.get("content")? {
        Value::String(text) => non_empty(text),
        Value::Array(items) => {
            let mut parts = Vec::new();
            for item in items {
                if let Some(text) = item
                    .get("text")
                    .or_else(|| item.get("input_text"))
                    .and_then(Value::as_str)
                    .and_then(non_empty)
                {
                    parts.push(text);
                }
            }
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        _ => None,
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

impl TuiPromptDedupeState {
    fn record_recent_observed_prompt(
        &mut self,
        provider: &str,
        tmux_session_name: &str,
        prompt: &str,
    ) {
        self.recent_observed_by_tmux
            .entry(PromptKey::new(provider, tmux_session_name))
            .or_default()
            .push_back(TimedValue {
                value: prompt.to_string(),
                recorded_at: Instant::now(),
            });
    }

    fn purge_expired(&mut self) {
        let now = Instant::now();
        self.pending_by_tmux.retain(|_, queue| {
            while queue
                .front()
                .is_some_and(|entry| now.duration_since(entry.recorded_at) > PENDING_PROMPT_TTL)
            {
                queue.pop_front();
            }
            !queue.is_empty()
        });
        self.recent_observed_by_tmux.retain(|_, queue| {
            while queue
                .front()
                .is_some_and(|entry| now.duration_since(entry.recorded_at) > RECENT_OBSERVED_TTL)
            {
                queue.pop_front();
            }
            !queue.is_empty()
        });
        self.channel_by_tmux
            .retain(|_, entry| now.duration_since(entry.recorded_at) <= SESSION_MAPPING_TTL);
        self.runtime_by_tmux
            .retain(|_, entry| now.duration_since(entry.recorded_at) <= SESSION_MAPPING_TTL);
        // #3885 follow-up: anchors live `PROMPT_ANCHOR_SUBMIT_TTL` (4h) so a long
        // streaming turn's anchor is not purged mid-stream (see the constant). The
        // relayed-entry ledger below intentionally keeps the 30min
        // `PROMPT_ANCHOR_TTL`.
        self.prompt_anchor_by_tmux
            .retain(|_, entry| now.duration_since(entry.recorded_at) <= PROMPT_ANCHOR_SUBMIT_TTL);
        self.ssh_direct_observation_by_tmux
            .retain(|_, entry| now.duration_since(entry.recorded_at) <= SSH_DIRECT_OBSERVATION_TTL);
        self.external_input_relay_lease_by_tmux.retain(|_, entry| {
            now.duration_since(entry.recorded_at) <= EXTERNAL_INPUT_RELAY_LEASE_TTL
        });
        self.deferred_anchor_completion_by_tmux.retain(|_, entry| {
            now.duration_since(entry.recorded_at) <= DEFERRED_ANCHOR_COMPLETION_TTL
        });
        // #3540: relayed-entry-id ledger — purge ids older than PROMPT_ANCHOR_TTL
        // (30min), long enough to span a watermark-reset / jsonl-rotation +
        // self-loop window while bounding memory growth.
        self.relayed_entry_ids_by_tmux.retain(|_, queue| {
            while queue
                .front()
                .is_some_and(|entry| now.duration_since(entry.recorded_at) > PROMPT_ANCHOR_TTL)
            {
                queue.pop_front();
            }
            !queue.is_empty()
        });
    }

    fn remove_provider_session_mappings_for_tmux(&mut self, tmux_session_name: &str) -> bool {
        let before = self.tmux_by_provider_session.len();
        self.tmux_by_provider_session
            .retain(|_, entry| entry.value != tmux_session_name);
        before != self.tmux_by_provider_session.len()
    }
}

#[cfg(test)]
mod tests;
