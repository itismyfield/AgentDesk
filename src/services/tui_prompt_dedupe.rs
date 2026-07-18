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

/// Mark a local-only entry as replayed only after its Discord session note was
/// accepted. No subscriber, lagged receiver, missing route/http, or failed send
/// calls this helper, so those paths cannot lose a later exact replay.
pub(crate) fn record_local_only_entry_id_after_note_delivery(prompt: &ObservedTuiPrompt) {
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
mod tests {
    use super::*;

    fn reset_state() {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        *state = TuiPromptDedupeState::default();
    }

    // #tui-hook-ttl-buffer key-match: the reverse lookup must resolve the
    // provider session UUID for a tmux session (the readiness layer only knows
    // the tmux name, but the hooks buffer under the provider UUID), and must
    // stay provider-isolated even when two providers share a tmux name.
    #[test]
    fn provider_session_for_tmux_resolves_reverse_mapping() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        register_provider_session("claude", "uuid-claude-1", "tmux-shared");
        register_provider_session("codex", "uuid-codex-1", "tmux-shared");

        // Resolves the right provider's UUID for the shared tmux name.
        assert_eq!(
            provider_session_for_tmux("claude", "tmux-shared"),
            Some("uuid-claude-1".to_string())
        );
        assert_eq!(
            provider_session_for_tmux("codex", "tmux-shared"),
            Some("uuid-codex-1".to_string())
        );
        // No mapping for an unknown tmux session => None (caller falls back to
        // the tmux name as the registry key).
        assert_eq!(provider_session_for_tmux("claude", "tmux-unknown"), None);
        // Empty inputs are rejected.
        assert_eq!(provider_session_for_tmux("claude", ""), None);
        assert_eq!(provider_session_for_tmux("", "tmux-shared"), None);
    }

    #[test]
    fn provider_session_for_tmux_prefers_most_recent_mapping() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        // A relaunch of the same tmux session under a new provider UUID must
        // resolve to the newest UUID (the prior turn's hooks have expired/moved).
        register_provider_session("claude", "uuid-old", "tmux-relaunch");
        register_provider_session("claude", "uuid-new", "tmux-relaunch");
        assert_eq!(
            provider_session_for_tmux("claude", "tmux-relaunch"),
            Some("uuid-new".to_string())
        );
    }

    #[test]
    fn claude_hook_payload_adopts_sibling_continuation_once_without_cursor_reset() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        let tmp = tempfile::tempdir().unwrap();
        let old_session = uuid::Uuid::new_v4().to_string();
        let new_session = uuid::Uuid::new_v4().to_string();
        let old_path = tmp.path().join(format!("{old_session}.jsonl"));
        let new_path = tmp.path().join(format!("{new_session}.jsonl"));
        std::fs::write(&old_path, b"old\n").unwrap();
        std::fs::write(&new_path, b"new\n").unwrap();
        let tmux = format!("tmux-4423-continuation-{}", std::process::id());
        register_provider_session("claude", &old_session, &tmux);
        register_tmux_runtime_binding(
            &tmux,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: old_path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(old_session.clone()),
                last_offset: 99,
                relay_last_offset: Some(99),
            },
        );

        let adopted = adopt_claude_continuation_session(&old_session, &new_session)
            .expect("safe sibling continuation adoption");
        assert_eq!(adopted.0, tmux);
        assert_eq!(adopted.1, new_path.display().to_string());
        let binding = runtime_binding_for_tmux_session(&tmux).unwrap();
        assert_eq!(binding.session_id.as_deref(), Some(new_session.as_str()));
        assert_eq!(binding.output_path, new_path.display().to_string());
        assert_eq!(binding.last_offset, 0);
        assert_eq!(
            provider_session_for_tmux("claude", &tmux).as_deref(),
            Some(old_session.as_str()),
            "future waits must keep using the live process's cached hook command UUID"
        );

        assert!(adopt_claude_continuation_session(&old_session, &new_session).is_some());
        let mut progressed = runtime_binding_for_tmux_session(&tmux).unwrap();
        progressed.last_offset = 4;
        register_tmux_runtime_binding(&tmux, progressed);
        assert!(adopt_claude_continuation_session(&old_session, &new_session).is_some());
        assert_eq!(
            runtime_binding_for_tmux_session(&tmux).unwrap().last_offset,
            4,
            "subsequent old-query/new-payload hooks must not rewind the adopted cursor"
        );
    }

    #[test]
    fn claude_hook_payload_can_advance_multiple_continuation_hops_but_not_rewind() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        let tmp = tempfile::tempdir().unwrap();
        let command_session = uuid::Uuid::new_v4().to_string();
        let first_continuation = uuid::Uuid::new_v4().to_string();
        let second_continuation = uuid::Uuid::new_v4().to_string();
        let stale_continuation = uuid::Uuid::new_v4().to_string();
        let command_path = tmp.path().join(format!("{command_session}.jsonl"));
        let first_path = tmp.path().join(format!("{first_continuation}.jsonl"));
        let second_path = tmp.path().join(format!("{second_continuation}.jsonl"));
        let stale_path = tmp.path().join(format!("{stale_continuation}.jsonl"));
        for path in [&command_path, &first_path, &second_path, &stale_path] {
            std::fs::write(path, b"{}\n").unwrap();
        }
        filetime::set_file_mtime(&first_path, filetime::FileTime::from_unix_time(20, 0)).unwrap();
        filetime::set_file_mtime(&second_path, filetime::FileTime::from_unix_time(30, 0)).unwrap();
        filetime::set_file_mtime(&stale_path, filetime::FileTime::from_unix_time(10, 0)).unwrap();
        let tmux = format!("tmux-4423-multihop-{}", std::process::id());
        register_provider_session("claude", &command_session, &tmux);
        register_tmux_runtime_binding(
            &tmux,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: command_path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(command_session.clone()),
                last_offset: 7,
                relay_last_offset: None,
            },
        );

        adopt_claude_continuation_session(&command_session, &first_continuation)
            .expect("first continuation hop");
        adopt_claude_continuation_session(&command_session, &second_continuation)
            .expect("newer second continuation hop through cached command UUID");
        let binding = runtime_binding_for_tmux_session(&tmux).unwrap();
        assert_eq!(
            binding.session_id.as_deref(),
            Some(second_continuation.as_str())
        );
        assert!(
            adopt_claude_continuation_session(&command_session, &stale_continuation).is_none(),
            "a delayed historical payload must not rewind the current continuation"
        );
        assert_eq!(
            runtime_binding_for_tmux_session(&tmux)
                .unwrap()
                .session_id
                .as_deref(),
            Some(second_continuation.as_str())
        );
    }

    #[test]
    fn provider_session_mapping_survives_prompt_purge_ttl() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        register_provider_session("claude", "uuid-long-lived", "tmux-long-lived");
        {
            let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
            let key = PromptKey::new("claude", "uuid-long-lived");
            state
                .tmux_by_provider_session
                .get_mut(&key)
                .expect("registered provider-session mapping")
                .recorded_at = Instant::now() - SESSION_MAPPING_TTL - Duration::from_secs(1);
        }

        // Any API that calls purge_expired should not delete the provider UUID
        // bridge while the TUI session can still be alive.
        register_tmux_channel("tmux-other", 42);

        assert_eq!(
            provider_session_for_tmux("claude", "tmux-long-lived"),
            Some("uuid-long-lived".to_string())
        );
    }

    #[test]
    fn provider_session_mapping_is_removed_with_runtime_binding_clear() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        register_provider_session("claude", "uuid-stale", "tmux-stale");
        assert_eq!(
            provider_session_for_tmux("claude", "tmux-stale"),
            Some("uuid-stale".to_string())
        );

        assert!(clear_tmux_runtime_binding("tmux-stale"));
        assert_eq!(
            provider_session_for_tmux("claude", "tmux-stale"),
            None,
            "clearing a tmux runtime binding must also clear stale provider-session reverse mappings"
        );
    }

    #[test]
    fn provider_session_mapping_is_removed_with_dead_tmux_mirror() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        register_provider_session("claude", "uuid-dead", "tmux-dead");
        assert!(evict_dead_tmux_mirror("tmux-dead"));
        assert_eq!(
            provider_session_for_tmux("claude", "tmux-dead"),
            None,
            "dead tmux mirror eviction must not leave provider-session reverse mappings behind"
        );
    }

    // U-14 Provider-keyed channel isolation: registering the same tmux name
    // under both `claude` and `codex` providers must keep two independent
    // mappings — the dedupe state must not collapse them, otherwise cc/cdx
    // turns running side-by-side could cross-relay into each other's
    // channels.
    #[test]
    fn provider_session_mapping_isolates_claude_and_codex_for_same_session_id() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        register_provider_session("claude", "session-shared", "tmux-claude");
        register_provider_session("codex", "session-shared", "tmux-codex");

        assert_eq!(
            resolve_tmux_session_name("claude", "session-shared"),
            Some("tmux-claude".to_string())
        );
        assert_eq!(
            resolve_tmux_session_name("codex", "session-shared"),
            Some("tmux-codex".to_string())
        );

        // Recording a Discord-originated prompt for one provider must not
        // suppress an SSH-direct prompt the other provider observes.
        record_discord_originated_prompt("claude", "tmux-claude", "shared-text");

        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-claude", "shared-text"),
            PromptObservation::SuppressedDiscordDuplicate
        );
        // The codex pane has no pending entry, so the same text is a fresh
        // direct-input observation, not a duplicate.
        assert_eq!(
            observe_prompt_by_tmux("codex", "tmux-codex", "shared-text"),
            PromptObservation::PublishedSshDirect
        );
    }

    // U-12 `relay_output_path` falls back to `output_path` when no dedicated
    // relay path is configured. A blank/whitespace-only override must not
    // shadow the primary output_path — otherwise the relay would tail an
    // empty path and silently drop frames.
    #[test]
    fn relay_output_path_falls_back_to_output_path_when_unset_or_blank() {
        let none_binding = TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: "/tmp/transcript.jsonl".to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        };
        assert_eq!(none_binding.relay_output_path(), "/tmp/transcript.jsonl");

        let blank_binding = TuiRuntimeBinding {
            relay_output_path: Some("   ".to_string()),
            ..none_binding.clone()
        };
        assert_eq!(blank_binding.relay_output_path(), "/tmp/transcript.jsonl");

        let override_binding = TuiRuntimeBinding {
            relay_output_path: Some("/tmp/relay.jsonl".to_string()),
            ..none_binding.clone()
        };
        assert_eq!(override_binding.relay_output_path(), "/tmp/relay.jsonl");
    }

    // U-12 `relay_last_offset()` mirrors `last_offset` when the override is
    // None — without this, the very first idle scan after a rehydrate
    // would tail from byte 0 and replay the entire transcript.
    #[test]
    fn relay_last_offset_falls_back_to_last_offset_when_unset() {
        let binding = TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: "/tmp/transcript.jsonl".to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 4096,
            relay_last_offset: None,
        };
        assert_eq!(binding.relay_last_offset(), 4096);

        let with_override = TuiRuntimeBinding {
            relay_last_offset: Some(1024),
            ..binding
        };
        assert_eq!(with_override.relay_last_offset(), 1024);
    }

    // U-10 `advance_tmux_runtime_binding_offset` is the cold-start entry
    // point used by relay readers to record where they left off. Calls with
    // a mismatched output_path that is not the configured relay override
    // must be rejected — otherwise a sibling reader writing the wrong path
    // could fast-forward our offset past unread frames.
    #[test]
    fn advance_offset_rejects_mismatched_path_when_relay_override_differs() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        register_tmux_runtime_binding(
            "tmux-cold",
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: "/tmp/primary.jsonl".to_string(),
                relay_output_path: Some("/tmp/relay.jsonl".to_string()),
                input_fifo_path: None,
                session_id: None,
                last_offset: 0,
                relay_last_offset: None,
            },
        );

        // Primary path advances `last_offset` and (because relay override
        // is set) leaves `relay_last_offset` alone.
        assert!(advance_tmux_runtime_binding_offset(
            "tmux-cold",
            "/tmp/primary.jsonl",
            500
        ));
        let after_primary = runtime_binding_for_tmux_session("tmux-cold").unwrap();
        assert_eq!(after_primary.last_offset, 500);
        assert!(after_primary.relay_last_offset.is_none());

        // Relay override path advances `relay_last_offset`.
        assert!(advance_tmux_runtime_binding_offset(
            "tmux-cold",
            "/tmp/relay.jsonl",
            900
        ));
        let after_relay = runtime_binding_for_tmux_session("tmux-cold").unwrap();
        assert_eq!(after_relay.relay_last_offset, Some(900));

        // An unrelated path is rejected and does not corrupt either offset.
        assert!(!advance_tmux_runtime_binding_offset(
            "tmux-cold",
            "/tmp/wrong.jsonl",
            9999
        ));
        let after_wrong = runtime_binding_for_tmux_session("tmux-cold").unwrap();
        assert_eq!(after_wrong.last_offset, 500);
        assert_eq!(after_wrong.relay_last_offset, Some(900));
    }

    #[test]
    fn refresh_runtime_binding_activity_extends_mapping_ttl_without_offset_advance() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        register_tmux_runtime_binding(
            "tmux-runtime-activity",
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: "/tmp/live-transcript.jsonl".to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("session-activity".to_string()),
                last_offset: 123,
                relay_last_offset: None,
            },
        );
        {
            let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
            state
                .runtime_by_tmux
                .get_mut("tmux-runtime-activity")
                .expect("runtime binding")
                .recorded_at = Instant::now() - SESSION_MAPPING_TTL + Duration::from_secs(1);
        }

        assert!(refresh_tmux_runtime_binding_activity(
            "tmux-runtime-activity",
            "/tmp/live-transcript.jsonl",
        ));
        let refreshed_age = {
            let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
            state
                .runtime_by_tmux
                .get("tmux-runtime-activity")
                .expect("runtime binding")
                .recorded_at
                .elapsed()
        };
        assert!(
            refreshed_age < Duration::from_secs(1),
            "fresh transcript activity should refresh the purge timestamp"
        );
        let binding = runtime_binding_for_tmux_session("tmux-runtime-activity")
            .expect("binding survives purge after refresh");
        assert_eq!(binding.last_offset, 123);
    }

    #[test]
    fn suppresses_exact_pending_prompt() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        record_discord_originated_prompt("claude", "tmux-a", "hello");

        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-a", "hello"),
            PromptObservation::SuppressedDiscordDuplicate
        );
    }

    #[test]
    fn stores_runtime_binding_by_tmux_session() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        register_tmux_runtime_binding(
            "tmux-runtime",
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: "/tmp/codex-rollout.jsonl".to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("thread-123".to_string()),
                last_offset: 77,
                relay_last_offset: None,
            },
        );

        assert_eq!(
            runtime_binding_for_tmux_session("tmux-runtime"),
            Some(TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: "/tmp/codex-rollout.jsonl".to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("thread-123".to_string()),
                last_offset: 77,
                relay_last_offset: None,
            })
        );
    }

    #[test]
    fn clears_runtime_binding_by_tmux_session() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        register_tmux_runtime_binding(
            "tmux-runtime",
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: "/tmp/claude-transcript.jsonl".to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("session-123".to_string()),
                last_offset: 77,
                relay_last_offset: None,
            },
        );

        assert!(runtime_binding_for_tmux_session("tmux-runtime").is_some());
        assert!(clear_tmux_runtime_binding("tmux-runtime"));
        assert!(runtime_binding_for_tmux_session("tmux-runtime").is_none());
        assert!(!clear_tmux_runtime_binding("tmux-runtime"));
        assert!(!clear_tmux_runtime_binding("   "));
    }

    // #3105 (codex P1 sub-case B): evicting a dead/orphaned mirror must drop BOTH
    // the runtime binding (which the idle relay loop iterates) AND the channel
    // mirror (which the drift-alert resolver reads), so a subsequent relay pass
    // finds no mapping and stops re-emitting the per-poll drift/skip WARN. A
    // later legitimate re-registration must still repopulate both maps.
    #[test]
    fn evict_dead_tmux_mirror_drops_runtime_and_channel_then_allows_reregister() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        let tmux = "AgentDesk-claude-adk-cc-t1504468805772902471";
        register_tmux_runtime_binding(
            tmux,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: "/tmp/claude-transcript.jsonl".to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: None,
                last_offset: 12,
                relay_last_offset: None,
            },
        );
        register_tmux_channel(tmux, 1_504_468_805_772_902_471);
        assert!(runtime_binding_for_tmux_session(tmux).is_some());
        assert_eq!(
            owner_channel_for_tmux_session(tmux),
            Some(1_504_468_805_772_902_471)
        );

        // Eviction removes both mirror maps and reports the change once.
        assert!(evict_dead_tmux_mirror(tmux));
        assert!(
            runtime_binding_for_tmux_session(tmux).is_none(),
            "runtime binding gone → relay loop no longer iterates the dead session"
        );
        assert_eq!(
            owner_channel_for_tmux_session(tmux),
            None,
            "channel mirror gone → drift-alert resolver finds no mapping"
        );
        // Idempotent: a second eviction reports no change (single bounded incident).
        assert!(!evict_dead_tmux_mirror(tmux));
        assert!(!evict_dead_tmux_mirror("   "));

        // A later legitimate re-registration repopulates both maps (session came back).
        register_tmux_runtime_binding(
            tmux,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: "/tmp/claude-transcript.jsonl".to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: None,
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        register_tmux_channel(tmux, 1_504_468_805_772_902_471);
        assert!(runtime_binding_for_tmux_session(tmux).is_some());
        assert_eq!(
            owner_channel_for_tmux_session(tmux),
            Some(1_504_468_805_772_902_471)
        );
    }

    #[test]
    fn lists_runtime_bindings_by_kind() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        register_tmux_runtime_binding(
            "tmux-codex",
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: "/tmp/codex-rollout.jsonl".to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("thread-123".to_string()),
                last_offset: 77,
                relay_last_offset: None,
            },
        );
        register_tmux_runtime_binding(
            "tmux-claude",
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: "/tmp/claude-transcript.jsonl".to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: None,
                last_offset: 88,
                relay_last_offset: None,
            },
        );

        assert_eq!(
            runtime_bindings_for_kind(RuntimeHandoffKind::CodexTui),
            vec![(
                "tmux-codex".to_string(),
                TuiRuntimeBinding {
                    runtime_kind: RuntimeHandoffKind::CodexTui,
                    output_path: "/tmp/codex-rollout.jsonl".to_string(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: Some("thread-123".to_string()),
                    last_offset: 77,
                    relay_last_offset: None,
                },
            )]
        );
    }

    #[test]
    fn prompt_anchor_is_consumed_for_matching_tmux_and_channel() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        record_prompt_anchor("Claude", "tmux-anchor", 42, 9001);

        assert_eq!(
            take_prompt_anchor_for_response("claude", "tmux-anchor", 43),
            None
        );
        assert_eq!(
            take_prompt_anchor_for_response("claude", "tmux-anchor", 42),
            Some(TuiPromptAnchor {
                channel_id: 42,
                message_id: 9001,
            })
        );
        assert_eq!(
            take_prompt_anchor_for_response("claude", "tmux-anchor", 42),
            None
        );
    }

    // #3174: the narrow ordering race — the watcher's lease-gated completion
    // fires BEFORE this turn's `record_prompt_anchor` lands (the provider
    // committed terminal output inside the `notify-post + ⏳-add` window). The
    // anchor-less completion must NOT silently drop the ⏳; it records a deferred
    // marker that the SAME turn's late anchor record drains.
    //
    // This reproduces the EXACT ordering: completion-before-anchor. Before the
    // fix `take_deferred_anchor_completion` did not exist and the anchor-less
    // completion had nowhere to defer to — the ⏳ was stranded (no later pass,
    // because the lease that gated the completion is cleared after delivery).
    #[test]
    fn deferred_anchor_completion_reconciles_when_anchor_recorded_after_completion() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        // 1) Watcher's lease-gated completion runs; the anchor for THIS turn is
        //    not recorded yet (notify-post + ⏳-add still in flight), so the
        //    anchor lookup the completion does returns None.
        assert_eq!(
            prompt_anchor_for_response("claude", "tmux-anchor", 42),
            None,
            "anchor must not exist yet at completion time (the race window)"
        );
        // The anchor-less completion records a deferred marker (stamped with
        // THIS turn's lease generation) instead of dropping the ⏳.
        let turn_gen = 7_u64;
        record_deferred_anchor_completion("Claude", "tmux-anchor", 42, turn_gen);

        // 2) The late `record_prompt_anchor` lands for the SAME turn. Its site
        //    drains the deferred marker → the relay finishes the ⏳ → ✅ swap.
        record_prompt_anchor("Claude", "tmux-anchor", 42, 9001);
        assert!(
            take_deferred_anchor_completion("claude", "tmux-anchor", turn_gen),
            "late anchor record must drain the deferred completion marker"
        );
        // The anchor is present so the relay's completion can act on it.
        assert_eq!(
            prompt_anchor_for_response("claude", "tmux-anchor", 42),
            Some(TuiPromptAnchor {
                channel_id: 42,
                message_id: 9001,
            }),
        );
        // The marker is single-shot: a second drain is a no-op.
        assert!(
            !take_deferred_anchor_completion("claude", "tmux-anchor", turn_gen),
            "deferred marker must be consumed exactly once"
        );
    }

    // #3174: the common (non-racing) path records no deferred marker, so the
    // late anchor record drains nothing — the relay's reconcile is a no-op and
    // the normal watcher completion owns the ⏳ → ✅ swap. Guards against the
    // fix double-completing on every turn.
    #[test]
    fn no_deferred_completion_when_completion_did_not_race_the_anchor() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        // No anchor-less completion happened (provider took the usual seconds),
        // so no marker was recorded.
        record_prompt_anchor("Claude", "tmux-anchor", 42, 9001);
        assert!(
            !take_deferred_anchor_completion("claude", "tmux-anchor", 7),
            "no deferred completion must be drained on the common non-racing path"
        );
    }

    // #3174 turn-identity safety: a deferred marker is keyed to
    // `(provider, tmux)` and must not be drained by a DIFFERENT provider's or a
    // different tmux session's anchor record.
    #[test]
    fn deferred_anchor_completion_is_isolated_by_provider_and_session() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        let turn_gen = 11_u64;
        record_deferred_anchor_completion("claude", "tmux-a", 42, turn_gen);

        // Wrong provider: codex must not drain claude's marker.
        assert!(!take_deferred_anchor_completion(
            "codex", "tmux-a", turn_gen
        ));
        // Wrong session: a different tmux must not drain it.
        assert!(!take_deferred_anchor_completion(
            "claude", "tmux-b", turn_gen
        ));
        // The exact key still drains it.
        assert!(take_deferred_anchor_completion(
            "claude", "tmux-a", turn_gen
        ));
    }

    // #3174 codex P1 (turn-identity isolation): a deferred marker stamped with
    // one turn's lease generation must NOT be drained by a DIFFERENT turn on the
    // SAME provider/tmux. Without the generation stamp the `(provider, tmux)` key
    // alone would let a newer turn within the marker TTL cross-consume the
    // previous turn's marker and complete the wrong turn's ⏳ → ✅.
    #[test]
    fn deferred_anchor_completion_is_not_cross_consumed_by_a_different_turn_same_key() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        // Turn A's anchor-less completion records a marker stamped gen=100.
        let turn_a_gen = 100_u64;
        record_deferred_anchor_completion("claude", "tmux-shared", 42, turn_a_gen);

        // A NEWER turn B on the SAME provider/tmux records its own lease (a
        // different, higher generation) and lands its anchor first. Its drain
        // must NOT consume turn A's marker — generations differ.
        let turn_b_gen = 101_u64;
        assert!(
            !take_deferred_anchor_completion("claude", "tmux-shared", turn_b_gen),
            "a newer turn must not cross-consume the previous turn's deferred marker"
        );
        // peek also reports it as not-present for turn B's identity.
        assert!(
            !deferred_anchor_completion_present_for_turn("claude", "tmux-shared", turn_b_gen),
            "peek must not match a different turn's generation"
        );

        // Turn A's own late anchor record (its matching generation) DOES drain it.
        assert!(
            deferred_anchor_completion_present_for_turn("claude", "tmux-shared", turn_a_gen),
            "peek must match the owning turn's generation"
        );
        assert!(
            take_deferred_anchor_completion("claude", "tmux-shared", turn_a_gen),
            "the owning turn's anchor record must drain its own marker"
        );
    }

    // #3174 codex P2 (HTTP fail-open): the relay PEEKS before consuming, so it
    // can leave the marker intact when command_http is unavailable. Prove peek is
    // non-destructive: a peek leaves the marker drainable by a later attempt.
    #[test]
    fn deferred_anchor_completion_peek_is_non_destructive() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        let turn_gen = 55_u64;
        record_deferred_anchor_completion("claude", "tmux-peek", 42, turn_gen);

        // Simulate the HTTP-unavailable relay path: it peeks (marker is owed) but
        // does NOT take, because there is no command_http to deliver the swap.
        assert!(
            deferred_anchor_completion_present_for_turn("claude", "tmux-peek", turn_gen),
            "peek must report the owed marker"
        );
        assert!(
            deferred_anchor_completion_present_for_turn("claude", "tmux-peek", turn_gen),
            "a second peek must still report it (peek does not consume)"
        );

        // A later attempt (HTTP now available) can still drain it — it was not
        // silently lost by the fail-open path.
        assert!(
            take_deferred_anchor_completion("claude", "tmux-peek", turn_gen),
            "the marker survives a peek and remains drainable"
        );
    }

    #[test]
    fn prompt_anchor_can_be_peeked_until_delivery_commits() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        let anchor = TuiPromptAnchor {
            channel_id: 42,
            message_id: 9001,
        };
        record_prompt_anchor(
            "Claude",
            "tmux-anchor",
            anchor.channel_id,
            anchor.message_id,
        );

        assert_eq!(
            prompt_anchor_for_response("claude", "tmux-anchor", 42),
            Some(anchor)
        );
        assert_eq!(
            prompt_anchor_for_response("claude", "tmux-anchor", 42),
            Some(anchor)
        );
        assert!(!clear_prompt_anchor_for_response(
            "claude",
            "tmux-anchor",
            TuiPromptAnchor {
                channel_id: 42,
                message_id: 9002,
            },
        ));
        assert!(clear_prompt_anchor_for_response(
            "claude",
            "tmux-anchor",
            anchor,
        ));
        assert_eq!(
            prompt_anchor_for_response("claude", "tmux-anchor", 42),
            None
        );
    }

    #[test]
    fn ssh_direct_observation_marker_is_set_on_publish_and_cleared_with_anchor() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        // No observation yet → the bypass signal must stay false so the
        // post-terminal suppress guard keeps catching ghost output.
        assert!(!is_ssh_direct_observation_pending("claude", "tmux-direct"));

        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-direct", "echo direct"),
            PromptObservation::PublishedSshDirect
        );
        // observe → marker is set immediately, before the relay subscriber
        // has even started its Discord notify await. This closes the race
        // window where a very fast TUI response would otherwise hit the
        // watcher with no anchor and get suppressed.
        assert!(is_ssh_direct_observation_pending("claude", "tmux-direct"));

        // Other (provider, tmux) pairs must not see the marker — cc/cdx
        // running side-by-side must not cross-bypass.
        assert!(!is_ssh_direct_observation_pending("codex", "tmux-direct"));
        assert!(!is_ssh_direct_observation_pending("claude", "tmux-other"));

        // Consuming the full anchor (i.e., response delivered to Discord)
        // also clears the pre-anchor marker so subsequent ghost output is
        // again subject to the suppress guard.
        let anchor = TuiPromptAnchor {
            channel_id: 77,
            message_id: 4242,
        };
        record_prompt_anchor(
            "claude",
            "tmux-direct",
            anchor.channel_id,
            anchor.message_id,
        );
        assert!(clear_prompt_anchor_for_response(
            "claude",
            "tmux-direct",
            anchor
        ));
        assert!(!is_ssh_direct_observation_pending("claude", "tmux-direct"));
    }

    #[test]
    fn advances_runtime_binding_offset_for_same_output_path() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        register_tmux_runtime_binding(
            "tmux-runtime",
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: "/tmp/claude-transcript.jsonl".to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: None,
                last_offset: 77,
                relay_last_offset: None,
            },
        );

        assert!(!advance_tmux_runtime_binding_offset(
            "tmux-runtime",
            "/tmp/other.jsonl",
            200
        ));
        assert_eq!(
            runtime_binding_for_tmux_session("tmux-runtime")
                .expect("binding")
                .last_offset,
            77
        );
        assert!(advance_tmux_runtime_binding_offset(
            "tmux-runtime",
            "/tmp/claude-transcript.jsonl",
            200
        ));
        assert_eq!(
            runtime_binding_for_tmux_session("tmux-runtime")
                .expect("binding")
                .last_offset,
            200
        );
    }

    #[test]
    fn advances_runtime_binding_relay_offset_separately_from_runtime_path() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        register_tmux_runtime_binding(
            "tmux-runtime",
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: "/tmp/codex-rollout.jsonl".to_string(),
                relay_output_path: Some("/tmp/tmux-wrapper.jsonl".to_string()),
                input_fifo_path: None,
                session_id: Some("thread-123".to_string()),
                last_offset: 77,
                relay_last_offset: Some(33),
            },
        );

        assert!(advance_tmux_runtime_binding_offset(
            "tmux-runtime",
            "/tmp/tmux-wrapper.jsonl",
            88
        ));
        let binding = runtime_binding_for_tmux_session("tmux-runtime").expect("binding");
        assert_eq!(binding.last_offset, 77);
        assert_eq!(binding.relay_last_offset, Some(88));
        assert!(!advance_tmux_runtime_binding_offset(
            "tmux-runtime",
            "/tmp/other.jsonl",
            99
        ));
    }

    #[test]
    fn suppresses_trailing_newline_pending_prompt() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        record_discord_originated_prompt("claude", "tmux-a", "hello\n");

        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-a", "hello"),
            PromptObservation::SuppressedDiscordDuplicate
        );
    }

    #[test]
    fn suppresses_fuzzy_whitespace_prompt() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        record_discord_originated_prompt("codex", "tmux-b", "Please   inspect\n\nthe failing test");

        assert_eq!(
            observe_prompt_by_tmux("codex", "tmux-b", "please inspect the failing test"),
            PromptObservation::SuppressedDiscordDuplicate
        );
    }

    #[test]
    fn candidate_observation_checks_all_pending_forms_before_direct_publish() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        record_discord_originated_prompt("claude", "tmux-c", "hello wrapped prompt");

        assert_eq!(
            observe_prompt_candidates_by_tmux(
                "claude",
                "tmux-c",
                &[
                    "hellowrappedprompt".to_string(),
                    "hello wrapped prompt".to_string()
                ],
            ),
            PromptObservation::SuppressedDiscordDuplicate
        );
        assert!(
            !external_input_relay_lease_present("claude", "tmux-c", 42),
            "a candidate matching a Discord-origin prompt must not create an ExternalInput lease"
        );
    }

    #[test]
    fn discord_relayed_user_prompt_format_is_recognized_3527() {
        // AgentDesk's own `[User: <author> (ID: <digits>)]` relay lines — author
        // may contain parens; prefix may be followed by a newline (multi-line).
        assert!(is_discord_relayed_user_prompt(
            "[User: 0hbujang (ID: 343742347365974026)] A부턱ㄱ"
        ));
        assert!(is_discord_relayed_user_prompt(
            "[User: Alice (ops) team (ID: 77)] deploy it"
        ));
        assert!(is_discord_relayed_user_prompt(
            "[User: Bob (ID: 5)]\nmultiline\nbody"
        ));
        // genuine external / cron / SSH injections carry no `[User: (ID:)]` prefix
        assert!(!is_discord_relayed_user_prompt(
            "/relay-scan — supervise relays"
        ));
        assert!(!is_discord_relayed_user_prompt(
            "just typed directly via ssh"
        ));
        assert!(!is_discord_relayed_user_prompt("[User: no id here] text"));
        assert!(!is_discord_relayed_user_prompt(
            "[User: x (ID: abc)] non-numeric"
        ));
        assert!(!is_discord_relayed_user_prompt(""));
        // codex #3527: the `[User:]` chunk may be PRECEDED by prepended context
        // ([External Recall], reply/upload context, Codex reuse wrappers) — the
        // marker is not necessarily on the first line, so every line is scanned.
        assert!(is_discord_relayed_user_prompt(
            "[External Recall]\n- prior context\n\n[User: Alice (ID: 77)] deploy it"
        ));
        assert!(is_discord_relayed_user_prompt(
            "[Reply context] ...\n[User: 0hbujang (ID: 343742347365974026)] hi"
        ));
        // codex #3527 r2: the legacy pane observer submits join("")/join(" ")
        // collapsed variants of one block, so the marker can be MID-LINE — the
        // whole-string scan must catch those too, not just the newline variant.
        assert!(is_discord_relayed_user_prompt(
            "[External Recall]- prior context[User: Alice (ID: 77)] deploy it"
        ));
        assert!(is_discord_relayed_user_prompt(
            "[External Recall] - prior context  [User: Alice (ID: 77)] deploy it"
        ));
        // author containing parens, collapsed mid-line
        assert!(is_discord_relayed_user_prompt(
            "ctx [User: Alice (ops) team (ID: 77)] deploy it"
        ));
    }

    #[test]
    fn observe_skips_discord_relayed_user_line_without_ledger_3527() {
        // #3527: a re-observed `[User:]` relay line WITHOUT a discord-originated
        // ledger entry (simulating a quiescence-timeout re-observation after the
        // entry was consumed/expired) must NOT publish an SSH-direct turn and must
        // not record an ExternalInput lease — otherwise it posts a spurious 직접
        // 주입 notice + orphan placeholder panel.
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        assert_eq!(
            observe_prompt_by_tmux(
                "claude",
                "tmux-3527",
                "[User: 0hbujang (ID: 343742347365974026)] A부턱ㄱ"
            ),
            PromptObservation::Ignored
        );
        assert!(
            !external_input_relay_lease_present("claude", "tmux-3527", 42),
            "a [User:] relay re-observation must not create an ExternalInput lease (#3527)"
        );
    }

    #[test]
    fn observe_publishes_user_prefixed_subagent_notification_machine_event_3818() {
        // #3818 regression: Codex subagent completions can be wrapped by
        // Provider Session Reuse and the Discord author prefix before the TUI
        // observer sees them. The #3527 self-relay filter must not swallow these
        // terminal machine events, or the card renderer never gets a chance to
        // hide the raw XML envelope from Discord.
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        let prompt = "[Provider Session Reuse]\n\
The prior authoritative Discord, role, and tool instructions already present in this \
Codex thread still apply. Treat only this turn's user request, reply context, uploaded \
files, and memory recall below as new actionable input.\n\n\
[User: 0hbujang (ID: 343742347365974026)] No response requested.\n\
<subagent_notification>{\"agent_path\":\"/tmp/private\",\"status\":{\"completed\":\"Review complete.\"}}</subagent_notification>";

        assert_eq!(
            observe_prompt_by_tmux("codex", "tmux-3818", prompt),
            PromptObservation::PublishedSshDirect,
            "start-anchored subagent_notification must bypass the [User:] duplicate filter"
        );
        assert!(clear_external_input_relay_lease("codex", "tmux-3818", 42));

        let chrome_before_user = "[Provider Session Reuse]\n\
The prior authoritative Discord, role, and tool instructions already present in this \
Codex thread still apply. Treat only this turn's user request, reply context, uploaded \
files, and memory recall below as new actionable input.\n\n\
No response requested.\n\
[User: 0hbujang (ID: 343742347365974026)] \
<subagent_notification>{\"agent_path\":\"/tmp/private\",\"status\":{\"completed\":\"Review complete.\"}}</subagent_notification>";
        assert_eq!(
            observe_prompt_by_tmux("codex", "tmux-3818-chrome-first", chrome_before_user),
            PromptObservation::PublishedSshDirect,
            "TUI chrome before the Discord author prefix must not re-enable the [User:] duplicate filter"
        );
        assert!(clear_external_input_relay_lease(
            "codex",
            "tmux-3818-chrome-first",
            42
        ));
    }

    #[test]
    fn relay_lease_only_observation_does_not_create_late_prompt_anchor_signal() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        assert_eq!(
            observe_prompt_candidates_by_tmux_for_relay_lease(
                "claude",
                "tmux-lease-only",
                &["typed over ssh".to_string()],
            ),
            PromptObservation::PublishedSshDirect
        );
        assert!(external_input_relay_lease_present(
            "claude",
            "tmux-lease-only",
            42
        ));
        assert!(
            !is_ssh_direct_observation_pending("claude", "tmux-lease-only"),
            "watcher emergency observation must not create a late prompt-anchor signal"
        );
    }

    #[test]
    fn external_input_turn_lease_carries_owner_and_trace_fields() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        record_external_input_turn_lease(
            "codex",
            "tmux-trace",
            ExternalInputRelayLease {
                channel_id: Some(42),
                turn_id: Some("external:codex:42:tmux-trace:123".to_string()),
                session_key: Some("host:tmux-trace".to_string()),
                relay_owner: ExternalInputRelayOwner::SessionBoundRelay,
                runtime_kind: Some(RuntimeHandoffKind::CodexTui),
                generation: EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
            },
        );

        let lease = external_input_relay_lease("codex", "tmux-trace", 42).expect("lease");
        assert_eq!(
            lease.turn_id.as_deref(),
            Some("external:codex:42:tmux-trace:123")
        );
        assert_eq!(lease.session_key.as_deref(), Some("host:tmux-trace"));
        assert_eq!(
            lease.relay_owner,
            ExternalInputRelayOwner::SessionBoundRelay
        );
        assert_eq!(lease.relay_owner.as_str(), "session_bound_relay");
        assert_eq!(lease.runtime_kind, Some(RuntimeHandoffKind::CodexTui));
        assert!(external_input_relay_lease("codex", "tmux-trace", 43).is_none());
    }

    #[test]
    fn clear_external_input_relay_lease_if_matches_preserves_newer_turn() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        let original = ExternalInputRelayLease {
            channel_id: Some(42),
            turn_id: Some("external:codex:42:tmux-trace:1".to_string()),
            session_key: Some("host:tmux-trace".to_string()),
            relay_owner: ExternalInputRelayOwner::BridgeAdapter,
            runtime_kind: Some(RuntimeHandoffKind::CodexTui),
            generation: EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        };
        let newer = ExternalInputRelayLease {
            turn_id: Some("external:codex:42:tmux-trace:2".to_string()),
            ..original.clone()
        };

        // Capture the RECORDED leases (each stamped with a distinct generation) —
        // those are the exact identities `_if_matches` compares against.
        let recorded_original =
            record_external_input_turn_lease("codex", "tmux-trace", original.clone());
        let recorded_newer = record_external_input_turn_lease("codex", "tmux-trace", newer.clone());
        assert_ne!(
            recorded_original.generation, recorded_newer.generation,
            "each recorded lease must get a distinct generation"
        );

        // The OLD recorded lease no longer matches the CURRENT (newer) one.
        assert!(!clear_external_input_relay_lease_if_matches(
            "codex",
            "tmux-trace",
            42,
            &recorded_original
        ));
        assert_eq!(
            external_input_relay_lease("codex", "tmux-trace", 42),
            Some(recorded_newer.clone())
        );
        assert!(clear_external_input_relay_lease_if_matches(
            "codex",
            "tmux-trace",
            42,
            &recorded_newer
        ));
        assert!(external_input_relay_lease("codex", "tmux-trace", 42).is_none());
    }

    #[test]
    fn clear_external_input_relay_lease_if_generation_matches_preserves_newer_unassigned() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        // Two value-identical Unassigned leases (all trace fields None) for the same
        // key receive DISTINCT generations.
        record_external_input_relay_lease("codex", "tmux-gen", Some(99));
        let first = external_input_relay_lease("codex", "tmux-gen", 99).expect("first lease");
        record_external_input_relay_lease("codex", "tmux-gen", Some(99));
        let second = external_input_relay_lease("codex", "tmux-gen", 99).expect("second lease");

        assert_eq!(first.relay_owner, ExternalInputRelayOwner::Unassigned);
        assert_eq!(second.relay_owner, ExternalInputRelayOwner::Unassigned);
        assert_ne!(
            first.generation, second.generation,
            "two Unassigned leases for the same key must get distinct generations"
        );

        // Clearing by the OLD generation must NOT clear the newer lease.
        assert!(!clear_external_input_relay_lease_if_generation_matches(
            "codex",
            "tmux-gen",
            99,
            first.generation
        ));
        assert_eq!(
            external_input_relay_lease("codex", "tmux-gen", 99),
            Some(second.clone()),
            "the newer Unassigned lease must survive a clear by the old generation"
        );

        // The UNRECORDED sentinel generation clears nothing.
        assert!(!clear_external_input_relay_lease_if_generation_matches(
            "codex",
            "tmux-gen",
            99,
            EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED
        ));

        // Clearing by the CURRENT generation clears exactly it.
        assert!(clear_external_input_relay_lease_if_generation_matches(
            "codex",
            "tmux-gen",
            99,
            second.generation
        ));
        assert!(external_input_relay_lease("codex", "tmux-gen", 99).is_none());
    }

    /// Watcher-style no-clobber: turn-1 snapshots the lease generation G1 before its
    /// awaited send; turn-2 records a NEWER same-key lease G2 during that send; turn-1
    /// then clears BY G1 — which must NOT remove turn-2's G2 lease. The snapshot is taken
    /// from a single `external_input_relay_lease` read (the watcher derives both the
    /// presence bool and the generation from that one atomic read).
    #[test]
    fn watcher_snapshot_generation_clear_preserves_newer_same_key_lease() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        // turn-1 records & the watcher snapshots its generation BEFORE the awaited send.
        record_external_input_relay_lease("codex", "tmux-watch", Some(7));
        let g1 = external_input_relay_lease("codex", "tmux-watch", 7)
            .map(|lease| lease.generation)
            .expect("turn-1 generation snapshot");

        // turn-2 records a NEWER same-key lease while turn-1's send is in flight.
        record_external_input_relay_lease("codex", "tmux-watch", Some(7));
        let g2 = external_input_relay_lease("codex", "tmux-watch", 7)
            .map(|lease| lease.generation)
            .expect("turn-2 generation");
        assert_ne!(
            g1, g2,
            "the newer same-key lease must get a distinct generation"
        );

        // turn-1's post-send clear BY G1 must be a no-op (G1 != current G2).
        assert!(
            !clear_external_input_relay_lease_if_generation_matches("codex", "tmux-watch", 7, g1),
            "clear by the stale G1 snapshot must not match the current G2 lease"
        );
        assert_eq!(
            external_input_relay_lease("codex", "tmux-watch", 7).map(|lease| lease.generation),
            Some(g2),
            "turn-2's lease must survive turn-1's stale-snapshot clear (no clobber)"
        );
    }

    #[test]
    fn legacy_external_input_relay_lease_defaults_to_unassigned_owner() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        record_external_input_relay_lease("claude", "tmux-legacy", Some(7));

        let lease = external_input_relay_lease("claude", "tmux-legacy", 7).expect("lease");
        assert_eq!(lease.channel_id, Some(7));
        assert_eq!(lease.turn_id, None);
        assert_eq!(lease.session_key, None);
        assert_eq!(lease.relay_owner, ExternalInputRelayOwner::Unassigned);
        assert_eq!(lease.runtime_kind, None);
    }

    #[test]
    fn merged_draft_does_not_suppress_pending_discord_prompt() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        record_discord_originated_prompt("codex", "tmux-b", "[TUI-REL-OLD] respond with marker");

        assert_eq!(
            observe_prompt_by_tmux(
                "codex",
                "tmux-b",
                "[TUI-REL-OLD] respond with marker [TUI-REL-NEW] respond with marker",
            ),
            PromptObservation::PublishedSshDirect
        );
    }

    #[test]
    fn expired_pending_prompt_publishes_as_direct_input() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        record_discord_originated_prompt("claude", "tmux-a", "hello");
        {
            let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
            let queue = state
                .pending_by_tmux
                .get_mut(&PromptKey::new("claude", "tmux-a"))
                .expect("pending prompt queue");
            queue.front_mut().expect("pending prompt").recorded_at =
                Instant::now() - PENDING_PROMPT_TTL - Duration::from_secs(1);
        }

        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-a", "hello"),
            PromptObservation::PublishedSshDirect
        );
    }

    #[test]
    fn removed_prompt_after_submit_failure_publishes_as_direct_input() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        record_discord_originated_prompt("codex", "tmux-b", "failed submit");
        remove_discord_originated_prompt("codex", "tmux-b", "failed submit");

        assert_eq!(
            observe_prompt_by_tmux("codex", "tmux-b", "failed submit"),
            PromptObservation::PublishedSshDirect
        );
    }

    #[test]
    fn publishes_unmatched_prompt() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-a", "typed over ssh"),
            PromptObservation::PublishedSshDirect
        );
        assert!(
            external_input_relay_lease_present("claude", "tmux-a", 42),
            "prompt observation creates a relay lease before Discord notification/anchor succeeds"
        );
        assert!(clear_external_input_relay_lease("claude", "tmux-a", 42));
        assert!(!external_input_relay_lease_present("claude", "tmux-a", 42));
    }

    #[test]
    fn local_only_control_creates_no_external_turn_effects_without_a_subscriber() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-effect-generation", "/compact"),
            PromptObservation::PublishedSshDirect
        );
        assert!(!external_input_relay_lease_present(
            "claude",
            "tmux-effect-generation",
            42
        ));
        assert!(!is_ssh_direct_observation_pending(
            "claude",
            "tmux-effect-generation"
        ));
        let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        let key = PromptKey::new("claude", "tmux-effect-generation");
        assert!(
            !state.recent_observed_by_tmux.contains_key(&key),
            "local controls must bypass the 30-second direct-input tombstone"
        );
        assert!(
            !state.pending_by_tmux.contains_key(&key),
            "local controls must not create a Discord-originated pending entry"
        );
    }

    #[test]
    fn local_compact_entry_id_is_recorded_only_after_a_successful_note_delivery() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        let now = Utc::now();

        assert_eq!(
            observe_prompt_by_tmux_with_entry_id_at(
                "claude",
                "tmux-local-compact",
                "/compact",
                Some("compact-entry-1"),
                now,
            ),
            PromptObservation::PublishedSshDirect,
        );
        // This test deliberately has no subscriber. A broadcast miss, absent
        // owner/channel/http, or Discord send error never calls the delivery
        // acknowledgement helper, so an exact later replay must still publish.
        assert_eq!(
            observe_prompt_by_tmux_with_entry_id_at(
                "claude",
                "tmux-local-compact",
                "/compact",
                Some("compact-entry-1"),
                now,
            ),
            PromptObservation::PublishedSshDirect,
            "without a confirmed note delivery, an exact local entry replay is not suppressed"
        );

        // The relay calls this only from the successful `channel.say` branch.
        record_local_only_entry_id_after_note_delivery(&ObservedTuiPrompt {
            provider: "claude".to_string(),
            tmux_session_name: "tmux-local-compact".to_string(),
            prompt: "/compact".to_string(),
            source_event_id: Some("compact-entry-1".to_string()),
            observed_at: now,
            external_input_lease_generation: EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
            ssh_direct_observation_generation: SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
        });
        assert_eq!(
            observe_prompt_by_tmux_with_entry_id_at(
                "claude",
                "tmux-local-compact",
                "/compact",
                Some("compact-entry-1"),
                now,
            ),
            PromptObservation::SuppressedReplayedEntry,
            "only a successfully delivered note records the exact local entry identity"
        );
        assert!(!external_input_relay_lease_present(
            "claude",
            "tmux-local-compact",
            42
        ));
        assert!(!is_ssh_direct_observation_pending(
            "claude",
            "tmux-local-compact"
        ));
    }

    #[test]
    fn local_compact_raw_and_envelope_each_publish_without_time_pairing() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        let wrapper = "<command-message>compact</command-message>\n\
                       <command-name>/compact</command-name>\n\
                       <command-args></command-args>";

        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-local-pair", "/compact"),
            PromptObservation::PublishedSshDirect
        );
        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-local-pair", wrapper),
            PromptObservation::PublishedSshDirect,
            "the transcript envelope is allowed to duplicate the raw local note"
        );
        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-local-pair", "/compact"),
            PromptObservation::PublishedSshDirect,
            "a later human /compact is never collapsed by a text/time pair window"
        );
    }

    #[test]
    fn local_note_delivery_ack_does_not_record_nonlocal_entries() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        let now = Utc::now();
        let nonlocal = ObservedTuiPrompt {
            provider: "claude".to_string(),
            tmux_session_name: "tmux-local-ack-scope".to_string(),
            prompt: "normal human prompt".to_string(),
            source_event_id: Some("nonlocal-entry".to_string()),
            observed_at: now,
            external_input_lease_generation: EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
            ssh_direct_observation_generation: SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
        };

        record_local_only_entry_id_after_note_delivery(&nonlocal);
        assert_eq!(
            observe_prompt_by_tmux_with_entry_id_at(
                "claude",
                "tmux-local-ack-scope",
                "normal human prompt",
                Some("nonlocal-entry"),
                now,
            ),
            PromptObservation::PublishedSshDirect,
            "the local-delivery acknowledgement path cannot alter generic entry-id semantics"
        );
    }

    #[test]
    fn task_notification_is_status_only_and_next_prompt_keeps_lease_free() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        let task = "<task-notification><status>killed</status><task-id>stop-1</task-id></task-notification>";
        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-stop-task", task),
            PromptObservation::PublishedTaskNotification
        );
        assert!(
            !external_input_relay_lease_present("claude", "tmux-stop-task", 42),
            "killed task status must not create an external-input lease"
        );
        assert!(!is_ssh_direct_observation_pending(
            "claude",
            "tmux-stop-task"
        ));
        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-stop-task", "the next real prompt"),
            PromptObservation::PublishedSshDirect
        );
        assert!(external_input_relay_lease_present(
            "claude",
            "tmux-stop-task",
            42
        ));
    }

    #[test]
    fn ignores_synthetic_context_prompt_without_relay_lease() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        assert_eq!(
            observe_prompt_by_tmux(
                "codex",
                "tmux-c",
                "<environment_context>\n  <cwd>/tmp/project</cwd>\n</environment_context>",
            ),
            PromptObservation::Ignored
        );
        assert!(
            !external_input_relay_lease_present("codex", "tmux-c", 42),
            "bootstrap context must not create an SSH-direct relay lease"
        );
    }

    #[test]
    fn ignores_claude_interrupt_marker_without_relay_lease() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-stop", "[Request interrupted by user]"),
            PromptObservation::Ignored
        );
        assert!(
            !external_input_relay_lease_present("claude", "tmux-stop", 42),
            "a stop-control transcript marker must not create an SSH-direct relay lease"
        );
    }

    #[test]
    fn interrupt_marker_filter_is_claude_scoped_for_direct_observation() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        let marker = "[Request interrupted by user]";

        assert_eq!(
            observe_prompt_by_tmux("codex", "tmux-codex-stop-text", marker),
            PromptObservation::PublishedSshDirect,
            "Codex direct input with the same text remains a user prompt"
        );
        assert!(clear_external_input_relay_lease(
            "codex",
            "tmux-codex-stop-text",
            42
        ));

        assert_eq!(
            observe_prompt_by_tmux("qwen", "tmux-qwen-stop-text", marker),
            PromptObservation::PublishedSshDirect,
            "Qwen direct input with the same text remains a user prompt"
        );
        assert!(clear_external_input_relay_lease(
            "qwen",
            "tmux-qwen-stop-text",
            42
        ));
    }

    #[test]
    fn external_input_relay_lease_can_be_bound_to_channel_after_observation() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        observe_prompt_by_tmux("claude", "tmux-a", "typed over ssh");
        record_external_input_relay_lease("claude", "tmux-a", Some(42));

        assert!(external_input_relay_lease_present("claude", "tmux-a", 42));
        assert!(!external_input_relay_lease_present("claude", "tmux-a", 43));
    }

    #[test]
    fn suppresses_recent_direct_duplicate_prompt() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        assert_eq!(
            observe_prompt_by_tmux("codex", "tmux-c", "typed over ssh"),
            PromptObservation::PublishedSshDirect
        );
        assert_eq!(
            observe_prompt_by_tmux("codex", "tmux-c", "typed over ssh\n"),
            PromptObservation::SuppressedRecentDuplicate
        );
    }

    #[test]
    fn suppresses_recent_slash_command_xml_and_invocation_forms() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        let wrapper = "<command-message>loop</command-message>\n\
                       <command-name>/loop</command-name>\n\
                       <command-args>check relay gaps every 30m</command-args>";

        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-loop", wrapper),
            PromptObservation::PublishedSshDirect
        );
        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-loop", "/loop  check relay gaps every 30m"),
            PromptObservation::SuppressedRecentDuplicate
        );
    }

    #[test]
    fn slash_command_dedupe_does_not_collapse_raw_args_or_other_commands() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        let wrapper = "<command-message>loop</command-message>\n\
                       <command-name>/loop</command-name>\n\
                       <command-args>check relay gaps every 30m</command-args>";

        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-loop", wrapper),
            PromptObservation::PublishedSshDirect
        );
        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-loop", "check relay gaps every 30m"),
            PromptObservation::PublishedSshDirect,
            "a real raw prompt matching only command args must not be dropped"
        );
        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-loop", "/model check relay gaps every 30m"),
            PromptObservation::PublishedSshDirect,
            "a different slash command with the same args is a new submission"
        );
    }

    #[test]
    fn pending_match_leaves_recent_tombstone_for_second_observer() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        record_discord_originated_prompt("codex", "tmux-c", "from discord");

        assert_eq!(
            observe_prompt_by_tmux("codex", "tmux-c", "from discord"),
            PromptObservation::SuppressedDiscordDuplicate
        );
        assert_eq!(
            observe_prompt_by_tmux("codex", "tmux-c", "from discord"),
            PromptObservation::SuppressedRecentDuplicate
        );
    }

    #[test]
    fn extracts_codex_rollout_user_message_text() {
        let json = serde_json::json!({
            "type": "response_item",
            "payload": {
                "id": "codex-entry-1",
                "type": "message",
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "hello" },
                    { "type": "input_text", "text": "world" }
                ]
            }
        });

        assert_eq!(
            extract_codex_rollout_user_prompt(&json).as_deref(),
            Some("hello\nworld")
        );
        let (prompt, entry_id) =
            extract_codex_rollout_user_prompt_with_entry_id(&json).expect("codex user prompt");
        assert_eq!(prompt, "hello\nworld");
        assert_eq!(entry_id.as_deref(), Some("codex-entry-1"));
    }

    #[test]
    fn extracts_codex_rollout_top_level_entry_id() {
        let json = serde_json::json!({
            "type": "response_item",
            "id": "codex-top-entry",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "hello from codex" }
                ]
            }
        });

        let (prompt, entry_id) =
            extract_codex_rollout_user_prompt_with_entry_id(&json).expect("codex user prompt");
        assert_eq!(prompt, "hello from codex");
        assert_eq!(entry_id.as_deref(), Some("codex-top-entry"));
    }

    #[test]
    fn codex_distinct_message_entry_ids_publish_distinct_direct_prompts() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        let first = serde_json::json!({
            "type": "response_item",
            "id": "codex-turn-container",
            "payload": {
                "id": "codex-message-entry-1",
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "first direct prompt" }]
            }
        });
        let second = serde_json::json!({
            "type": "response_item",
            "id": "codex-turn-container",
            "payload": {
                "id": "codex-message-entry-2",
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "second direct prompt" }]
            }
        });
        let (first_prompt, first_entry_id) =
            extract_codex_rollout_user_prompt_with_entry_id(&first).expect("first codex prompt");
        let (second_prompt, second_entry_id) =
            extract_codex_rollout_user_prompt_with_entry_id(&second).expect("second codex prompt");
        assert_eq!(first_entry_id.as_deref(), Some("codex-message-entry-1"));
        assert_eq!(second_entry_id.as_deref(), Some("codex-message-entry-2"));

        let now = Utc::now();
        assert_eq!(
            observe_prompt_by_tmux_with_entry_id_at(
                "codex",
                "tmux-codex-distinct-ids",
                &first_prompt,
                first_entry_id.as_deref(),
                now,
            ),
            PromptObservation::PublishedSshDirect
        );
        assert_eq!(
            observe_prompt_by_tmux_with_entry_id_at(
                "codex",
                "tmux-codex-distinct-ids",
                &second_prompt,
                second_entry_id.as_deref(),
                now,
            ),
            PromptObservation::PublishedSshDirect,
            "distinct Codex message item ids must not collapse separate direct prompts \
             even when the top-level response_item id is shared"
        );
        assert_eq!(
            observe_prompt_by_tmux_with_entry_id_at(
                "codex",
                "tmux-codex-distinct-ids",
                &first_prompt,
                first_entry_id.as_deref(),
                now,
            ),
            PromptObservation::SuppressedReplayedEntry,
            "only the exact already-relayed Codex message item id is replay-suppressed"
        );
    }

    #[test]
    fn ignores_codex_rollout_environment_context_user_message() {
        let json = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [
                    {
                        "type": "input_text",
                        "text": "<environment_context>\n  <cwd>/tmp/project</cwd>\n</environment_context>"
                    }
                ]
            }
        });

        assert_eq!(extract_codex_rollout_user_prompt(&json), None);
    }

    #[test]
    fn codex_and_qwen_keep_claude_interrupt_text_as_user_prompt() {
        let marker = "[Request interrupted by user]";
        let codex_json = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [
                    {
                        "type": "input_text",
                        "text": marker
                    }
                ]
            }
        });
        let qwen_json = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": marker }
                ]
            }
        });

        assert_eq!(
            extract_codex_rollout_user_prompt(&codex_json).as_deref(),
            Some(marker)
        );
        assert_eq!(
            extract_qwen_jsonl_user_prompt(&qwen_json).as_deref(),
            Some(marker)
        );
    }

    #[test]
    fn extracts_claude_transcript_user_message_text() {
        let json = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": "hello" },
                    { "type": "text", "text": "world" }
                ]
            },
            "sessionId": "sess-tui",
        });

        assert_eq!(
            extract_claude_transcript_user_prompt(&json).as_deref(),
            Some("hello\nworld")
        );
    }

    // Live #3304 reproduction. The duplicate did NOT come from the isMeta
    // skill-expansion entry (that is already filtered to None below): the two
    // observation paths see ASYMMETRIC text for one submission — the hook path
    // records the raw `/loop <args>` invocation echo a ScheduleWakeup writes
    // into the terminal, while the idle transcript relay later extracts the
    // string-content `<command-*>` wrapper entry. Before the slash canonical
    // key their fuzzy keys diverged and the wrapper published a second
    // synthetic turn (2026-06-11 05:15 incident).
    #[test]
    fn suppresses_transcript_command_xml_after_raw_invocation_echo() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        let command_args = "매 주기마다: (1) sonnet 모델 서브에이전트를 스폰해 \
            **adk-cc 채널(1479671298497183835)만** 조사시키고 보고받는다";

        // 1st observation (hook path): raw invocation echo, published normally.
        let invocation_echo = format!("/loop {command_args}");
        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-loop", &invocation_echo),
            PromptObservation::PublishedSshDirect
        );

        // 2nd observation (idle transcript relay): the same submission as a
        // command-XML wrapper. Without the slash canonical key this fuzzy-
        // mismatched the echo and published a duplicate synthetic turn.
        let wrapper = format!(
            "<command-message>loop</command-message>\n\
             <command-name>/loop</command-name>\n\
             <command-args>{command_args}</command-args>"
        );
        let command_json = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": wrapper,
            },
            "timestamp": "2026-06-10T20:15:20.334Z",
        });
        let prompt = extract_claude_transcript_user_prompt(&command_json).expect("command prompt");
        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-loop", &prompt),
            PromptObservation::SuppressedRecentDuplicate,
            "#3304: the XML wrapper form must attribute to the raw invocation echo"
        );

        // The isMeta:true skill-expansion entry is machine context and never
        // reaches dedupe at all (pre-existing filter, unrelated to the bug).
        let skill_expansion_json = serde_json::json!({
            "type": "user",
            "isMeta": true,
            "message": {
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": format!(
                        "# /loop — schedule a recurring or self-paced prompt\n\n\
                         Parse the input below into `[interval] <prompt…>` and schedule it.\n\n\
                         ## Input\n\n{command_args}"
                    ),
                }],
            },
            "timestamp": "2026-06-10T20:15:20.334Z",
        });
        assert_eq!(
            extract_claude_transcript_user_prompt(&skill_expansion_json),
            None,
            "Claude records slash-command skill expansion as isMeta=true machine context"
        );
    }

    #[test]
    fn ignores_claude_transcript_meta_user_message_text() {
        let json = serde_json::json!({
            "type": "user",
            "isMeta": true,
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": "_" }
                ]
            },
            "sessionId": "sess-tui",
        });

        assert_eq!(extract_claude_transcript_user_prompt(&json), None);
    }

    #[test]
    fn ignores_claude_transcript_interrupt_marker_user_message_text() {
        for marker in [
            "[Request interrupted by user]",
            "[Request interrupted by user for tool use]",
        ] {
            let json = serde_json::json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": marker }
                    ]
                },
                "sessionId": "sess-tui",
            });

            assert_eq!(
                extract_claude_transcript_user_prompt(&json),
                None,
                "interrupt marker {marker:?} is control output, not external input"
            );
        }

        let user_prompt = "[Request interrupted by user story idea]";
        let json = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": user_prompt }
                ]
            },
            "sessionId": "sess-tui",
        });

        assert_eq!(
            extract_claude_transcript_user_prompt(&json).as_deref(),
            Some(user_prompt),
            "nearby human text must not be filtered by prefix"
        );
    }

    #[test]
    fn extracts_non_meta_claude_array_user_message_after_slash_command() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        let wrapper = "<command-message>loop</command-message>\n\
                       <command-name>/loop</command-name>\n\
                       <command-args>check relay gaps every 30m</command-args>";
        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-loop", wrapper),
            PromptObservation::PublishedSshDirect
        );

        let json = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": "fresh array prompt" },
                    { "type": "text", "text": "with attachment context" }
                ],
            },
            "sessionId": "sess-tui",
        });
        let prompt = extract_claude_transcript_user_prompt(&json).expect("array prompt");

        assert_eq!(prompt, "fresh array prompt\nwith attachment context");
        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-loop", &prompt),
            PromptObservation::PublishedSshDirect,
            "non-command array user content remains a real user submission"
        );
    }

    #[test]
    fn extracts_qwen_jsonl_user_message_text() {
        let json = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": "hello" },
                    { "type": "text", "text": "world" }
                ]
            }
        });

        assert_eq!(
            extract_qwen_jsonl_user_prompt(&json).as_deref(),
            Some("hello\nworld")
        );
    }

    #[test]
    fn ignores_qwen_tool_result_user_messages() {
        let json = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "content": "done",
                    "is_error": false
                }]
            }
        });

        assert_eq!(extract_qwen_jsonl_user_prompt(&json), None);
    }

    // ----------------------------------------------------------------------
    // #3540: stable JSONL entry-id (uuid) dedup — root-cause prevention of the
    // phantom synthetic inflight on watermark reset / jsonl head rotation.
    // ----------------------------------------------------------------------

    /// #3540: the SAME entry uuid observed twice (the watermark-reset re-scan)
    /// publishes once and is then suppressed by IDENTITY — so the second sighting
    /// never mints a synthetic turn. This is the bound the 30s content window
    /// could not provide.
    #[test]
    fn replayed_entry_id_is_suppressed_on_second_observe() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        let now = Utc::now();

        assert_eq!(
            observe_prompt_by_tmux_with_entry_id_at(
                "claude",
                "tmux-3540",
                "deploy to make=live",
                Some("uuid-A"),
                now,
            ),
            PromptObservation::PublishedSshDirect,
            "first sighting of a fresh entry relays normally"
        );
        assert_eq!(
            observe_prompt_by_tmux_with_entry_id_at(
                "claude",
                "tmux-3540",
                "deploy to make=live",
                Some("uuid-A"),
                now,
            ),
            PromptObservation::SuppressedReplayedEntry,
            "the SAME entry uuid re-encountered (watermark reset / head rotation) \
             is suppressed by identity — no phantom synthetic inflight (#3540)"
        );
    }

    /// #3540 regression guard (#3459/#3303): a genuinely NEW prompt carries a NEW
    /// uuid (Claude Code issues one at type time), so it is NEVER suppressed by
    /// the entry-id ledger — missed-prompt regression cannot recur.
    #[test]
    fn new_entry_id_is_never_suppressed() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        let now = Utc::now();

        assert_eq!(
            observe_prompt_by_tmux_with_entry_id_at(
                "claude",
                "tmux-3540",
                "first prompt",
                Some("uuid-1"),
                now,
            ),
            PromptObservation::PublishedSshDirect
        );
        assert_eq!(
            observe_prompt_by_tmux_with_entry_id_at(
                "claude",
                "tmux-3540",
                "second prompt",
                Some("uuid-2"),
                now,
            ),
            PromptObservation::PublishedSshDirect,
            "a distinct entry uuid carrying distinct content is a distinct \
             submission — always relayed (#3459/#3303 missed-prompt regression \
             guard). The entry-id ledger only suppresses a RE-ENCOUNTER of the \
             EXACT same uuid; a new uuid never collides."
        );
        // A THIRD distinct prompt under a THIRD uuid also relays — the ledger
        // does not accumulate false suppressions across genuinely new prompts.
        assert_eq!(
            observe_prompt_by_tmux_with_entry_id_at(
                "claude",
                "tmux-3540",
                "third prompt",
                Some("uuid-3"),
                now,
            ),
            PromptObservation::PublishedSshDirect,
            "each genuinely new prompt (new uuid + new content) keeps relaying"
        );
    }

    /// #3540: `entry_id == None` (uuid missing / non-Claude provider) falls back
    /// to the pre-#3540 content-keyed 30s recent-observed dedup — no behavior
    /// change, no functional regression.
    #[test]
    fn missing_entry_id_falls_back_to_content_dedup() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        let now = Utc::now();

        assert_eq!(
            observe_prompt_by_tmux_with_entry_id_at(
                "claude",
                "tmux-3540",
                "no-uuid prompt",
                None,
                now,
            ),
            PromptObservation::PublishedSshDirect
        );
        // Same content again with no uuid → the content-keyed recent dedup
        // suppresses it (the existing 30s path), NOT the entry-id path.
        assert_eq!(
            observe_prompt_by_tmux_with_entry_id_at(
                "claude",
                "tmux-3540",
                "no-uuid prompt",
                None,
                now,
            ),
            PromptObservation::SuppressedRecentDuplicate,
            "with no stable id the legacy content-keyed dedup still applies"
        );
    }

    /// #3540: a candidate suppressed by the recent-duplicate path must NOT be
    /// recorded in the entry-id ledger as 'relayed' — only an ACTUAL relay
    /// records the id. (Recording on a dedup-suppressed sighting would be a
    /// false 'seen', a subtle correctness bug.)
    #[test]
    fn dedup_suppressed_candidate_does_not_record_entry_id() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        let now = Utc::now();

        // A discord-originated pending prompt is queued first.
        record_discord_originated_prompt("claude", "tmux-3540", "queued prompt");
        // The transcript scanner then observes the SAME text (with a uuid) — it is
        // suppressed as a Discord duplicate, NOT relayed-as-SSH-direct.
        assert_eq!(
            observe_prompt_by_tmux_with_entry_id_at(
                "claude",
                "tmux-3540",
                "queued prompt",
                Some("uuid-D"),
                now,
            ),
            PromptObservation::SuppressedDiscordDuplicate
        );
        // Because that sighting was dedup-suppressed (not a real SSH-direct
        // relay), its uuid was NOT recorded. A later genuine SSH-direct sighting
        // of a DIFFERENT prompt under that same uuid would still relay — proving
        // the ledger was not poisoned. (We assert the simpler invariant: the same
        // uuid under fresh content publishes, i.e. is not falsely pre-suppressed.)
        assert_eq!(
            observe_prompt_by_tmux_with_entry_id_at(
                "claude",
                "tmux-3540",
                "different fresh text",
                Some("uuid-D"),
                now,
            ),
            PromptObservation::PublishedSshDirect,
            "a uuid seen only on a dedup-suppressed sighting was not recorded as \
             relayed, so it does not falsely suppress a later real relay (#3540)"
        );
    }

    /// #3540: purge_expired drops entry ids older than PROMPT_ANCHOR_TTL so the
    /// ledger cannot grow without bound; a re-encounter after purge relays again
    /// (correct — the watermark-reset window is far shorter than the 30min TTL).
    #[test]
    fn relayed_entry_id_ledger_purges_after_ttl() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        record_relayed_entry_id("claude", "tmux-3540", "uuid-T");
        assert!(relayed_entry_id_already_seen(
            "claude",
            "tmux-3540",
            "uuid-T"
        ));

        // Force the recorded id to look older than the TTL, then purge.
        {
            let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(queue) = state
                .relayed_entry_ids_by_tmux
                .get_mut(&PromptKey::new("claude", "tmux-3540"))
            {
                for entry in queue.iter_mut() {
                    entry.recorded_at =
                        Instant::now() - (PROMPT_ANCHOR_TTL + Duration::from_secs(1));
                }
            }
            state.purge_expired();
        }
        assert!(
            !relayed_entry_id_already_seen("claude", "tmux-3540", "uuid-T"),
            "entry ids older than PROMPT_ANCHOR_TTL are purged (bounded growth)"
        );
    }

    /// #3885 follow-up: a long streaming turn's prompt anchor must survive past
    /// the legacy 30min purge so the bridge same-input correlation peek still
    /// resolves mid-stream (and the #3885 no-response requeue does NOT re-fire a
    /// duplicate). An anchor aged beyond the new 4h ceiling is still purged so the
    /// idle-pane bound stays bounded. Decoupled from the relayed-entry ledger,
    /// which keeps the 30min `PROMPT_ANCHOR_TTL`.
    #[test]
    fn prompt_anchor_survives_long_streaming_turn_past_legacy_30min_ttl() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        let tmux = "tmux-3885-longstream";
        let channel = 7777_u64;
        let streaming_msg = 8_888_u64;

        // An anchor stamped at submit for a turn that has now been streaming 31min
        // (> the legacy 30min purge, < the new 4h ceiling) must STILL resolve.
        record_prompt_anchor_aged_for_tests(
            "claude",
            tmux,
            channel,
            streaming_msg,
            Duration::from_secs(31 * 60),
        );
        assert_eq!(
            prompt_anchor_for_response("claude", tmux, channel),
            Some(TuiPromptAnchor {
                channel_id: channel,
                message_id: streaming_msg,
            }),
            "anchor for a 31min-streaming turn must survive the legacy 30min purge"
        );
        // Sanity: that age is past the OLD 30min TTL (so the win is real) but
        // within the NEW 4h ceiling.
        assert!(Duration::from_secs(31 * 60) > PROMPT_ANCHOR_TTL);
        assert!(Duration::from_secs(31 * 60) < PROMPT_ANCHOR_SUBMIT_TTL);

        // Beyond the 4h ceiling the anchor is purged (bounded idle-pane lifetime).
        record_prompt_anchor_aged_for_tests(
            "claude",
            tmux,
            channel,
            streaming_msg,
            PROMPT_ANCHOR_SUBMIT_TTL + Duration::from_secs(1),
        );
        assert_eq!(
            prompt_anchor_for_response("claude", tmux, channel),
            None,
            "anchor older than PROMPT_ANCHOR_SUBMIT_TTL is purged"
        );
    }

    /// #3956: re-stamp-on-activity. A turn that streams continuously LONGER than
    /// `PROMPT_ANCHOR_SUBMIT_TTL` (4h) must keep a live submit anchor — the watcher
    /// calls `touch_prompt_anchor_on_activity` on every observed streamed chunk,
    /// advancing `recorded_at` so the anchor never reaches the 4h purge mid-stream.
    /// This keeps the #3885 same-input correlation peek resolving for the whole
    /// turn (no duplicate-prose requeue), making the correlation TTL-independent.
    #[test]
    fn streaming_activity_restamps_anchor_so_long_turn_never_loses_it() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        let provider = "claude";
        let tmux = "tmux-3956-restamp";
        let channel = 4444_u64;
        let msg = 5_555_u64;

        // The turn has streamed for nearly the whole 4h ceiling.
        record_prompt_anchor_aged_for_tests(
            provider,
            tmux,
            channel,
            msg,
            PROMPT_ANCHOR_SUBMIT_TTL - Duration::from_secs(60),
        );
        // Control: WITHOUT a refresh, a turn that has streamed past the 4h ceiling
        // already loses its anchor (the #3885 residual this fix closes). Pinned on
        // a SEPARATE key so the refreshed-path assertions below are uncontaminated.
        record_prompt_anchor_aged_for_tests(
            provider,
            "tmux-3956-norefresh",
            channel,
            msg,
            PROMPT_ANCHOR_SUBMIT_TTL + Duration::from_secs(1),
        );
        assert_eq!(
            prompt_anchor_for_response(provider, "tmux-3956-norefresh", channel),
            None,
            "without re-stamp, a >4h stream's anchor is purged (the #3885 residual)"
        );

        // Observed streaming activity re-stamps `recorded_at` to ~now.
        assert!(
            touch_prompt_anchor_on_activity(provider, tmux, channel),
            "an existing anchor for this channel is re-stamped on activity"
        );

        // Simulate ANOTHER (4h - 60s) of continuous streaming elapsing AFTER that
        // re-stamp by backdating the refreshed stamp. Because the re-stamp reset the
        // clock, the effective age is now (4h - 60s) < 4h, so the anchor STILL
        // resolves — whereas the un-refreshed control above (~8h wall-age) was purged.
        {
            let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
            state
                .prompt_anchor_by_tmux
                .get_mut(&PromptKey::new(provider, tmux))
                .expect("anchor present after touch")
                .recorded_at =
                Instant::now() - (PROMPT_ANCHOR_SUBMIT_TTL - Duration::from_secs(60));
        }
        assert_eq!(
            prompt_anchor_for_response(provider, tmux, channel),
            Some(TuiPromptAnchor {
                channel_id: channel,
                message_id: msg,
            }),
            "re-stamped anchor survives well past the wall-clock 4h a single stamp would not"
        );

        // Channel-scoped: a touch for a DIFFERENT channel must not refresh this anchor.
        assert!(
            !touch_prompt_anchor_on_activity(provider, tmux, channel + 1),
            "touch is a no-op when the stored anchor's channel does not match"
        );
        // Refresh-only: a touch with no anchor recorded must NOT create one.
        assert!(
            !touch_prompt_anchor_on_activity(provider, "tmux-3956-absent", channel),
            "touch never CREATES an anchor — refresh-on-activity only"
        );
    }

    /// #3956 codex re-review regression guard: `touch_prompt_anchor_on_activity`
    /// is a SINGLE-MAP op — it must NOT run the global `purge_expired`, so it can
    /// neither scan nor mutate the #3459/#3303 `relayed_entry_ids_by_tmux` ledger
    /// (nor any other dedupe map) on the per-chunk hot path. Proven by leaving a
    /// ledger entry that a full purge WOULD drop in place ACROSS a touch: it
    /// survives the touch byte-for-byte, demonstrating the touch did not trigger
    /// the ledger-purging code at all. The ledger still purges on its OWN 30min
    /// `PROMPT_ANCHOR_TTL` via the normal (purge-running) paths.
    #[test]
    fn touch_anchor_on_activity_does_not_run_global_purge_or_touch_ledger() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        let provider = "claude";
        let tmux = "tmux-3956-ledger";
        let channel = 5555_u64;
        let msg = 6_666_u64;

        record_relayed_entry_id(provider, tmux, "uuid-LEDGER");
        record_prompt_anchor(provider, tmux, channel, msg);

        // Age the ledger entry PAST its 30min TTL so a full `purge_expired` WOULD
        // drop it; the anchor stays fresh (well within 4h). Done via direct state
        // access so no purge-calling helper runs between here and the touch below.
        // Capture the ledger stamp to prove `touch` leaves it byte-for-byte intact.
        let ledger_stamp_before = {
            let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
            let aged = Instant::now() - (PROMPT_ANCHOR_TTL + Duration::from_secs(60));
            state
                .relayed_entry_ids_by_tmux
                .get_mut(&PromptKey::new(provider, tmux))
                .and_then(|queue| queue.front_mut())
                .expect("ledger entry present")
                .recorded_at = aged;
            aged
        };

        // Streaming activity re-stamps the SUBMIT anchor (single-map op).
        assert!(touch_prompt_anchor_on_activity(provider, tmux, channel));

        {
            let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
            // The anchor was refreshed to ~now...
            let anchor_age = state
                .prompt_anchor_by_tmux
                .get(&PromptKey::new(provider, tmux))
                .map(|entry| entry.recorded_at.elapsed())
                .expect("anchor present");
            assert!(
                anchor_age < Duration::from_secs(60),
                "anchor was re-stamped on activity"
            );
            // ...but the OVER-TTL ledger entry is STILL present with its original
            // stamp: `touch` did not run the global purge, so the ledger was never
            // scanned or mutated (the #3459/#3303 non-regression is REAL, not just
            // benign). A full `purge_expired` would have dropped this entry.
            let seen = state
                .relayed_entry_ids_by_tmux
                .get(&PromptKey::new(provider, tmux))
                .and_then(|queue| queue.front())
                .expect("ledger entry still present (touch did not purge it)");
            assert_eq!(seen.value, "uuid-LEDGER");
            assert_eq!(
                seen.recorded_at, ledger_stamp_before,
                "touch left the over-TTL ledger entry byte-for-byte untouched"
            );
        }

        // The ledger DOES purge on its own 30min TTL via the normal purge-running
        // path — `touch` simply is not that path. `relayed_entry_id_already_seen`
        // runs `purge_expired`, dropping the over-TTL entry; the freshly-touched
        // anchor (well within 4h) survives that same purge.
        assert!(
            !relayed_entry_id_already_seen(provider, tmux, "uuid-LEDGER"),
            "over-TTL ledger entry is dropped by the normal (purge-running) path"
        );
        assert_eq!(
            prompt_anchor_for_response(provider, tmux, channel),
            Some(TuiPromptAnchor {
                channel_id: channel,
                message_id: msg,
            }),
            "freshly-touched anchor survives the ledger's independent 30min purge"
        );
    }

    /// #3956 codex re-review: the no-resurrection guarantee must hold WITHOUT the
    /// global purge — a matching anchor already past the 4h ceiling is never
    /// refreshed by `touch` (it is evicted from the single anchor map instead), so
    /// a pane idle 4h+ that suddenly streams cannot revive a long-dead turn's
    /// anchor. The eviction touches only `prompt_anchor_by_tmux`.
    #[test]
    fn touch_anchor_on_activity_evicts_expired_anchor_without_resurrecting_it() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        let provider = "claude";
        let tmux = "tmux-3956-expired";
        let channel = 9999_u64;
        let msg = 1_234_u64;

        // An anchor already past the 4h ceiling (a long-dead turn). Recorded via
        // the aged helper, which does NOT purge, so it is still in the map when the
        // first streaming activity arrives.
        record_prompt_anchor_aged_for_tests(
            provider,
            tmux,
            channel,
            msg,
            PROMPT_ANCHOR_SUBMIT_TTL + Duration::from_secs(1),
        );

        // Activity must NOT refresh the dead anchor...
        assert!(
            !touch_prompt_anchor_on_activity(provider, tmux, channel),
            "an anchor past the 4h ceiling is never re-stamped (no resurrection)"
        );
        // ...and the dead anchor is evicted from the single anchor map.
        {
            let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
            assert!(
                state
                    .prompt_anchor_by_tmux
                    .get(&PromptKey::new(provider, tmux))
                    .is_none(),
                "the over-ceiling anchor was evicted, not resurrected"
            );
        }
        assert_eq!(
            prompt_anchor_for_response(provider, tmux, channel),
            None,
            "no live anchor remains for the dead turn"
        );
    }

    /// #3540: the ring cap bounds per-key growth even before the TTL fires —
    /// the oldest id is evicted once the cap is exceeded.
    #[test]
    fn relayed_entry_id_ledger_is_ring_capped() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        record_relayed_entry_id("claude", "tmux-cap", "uuid-oldest");
        for i in 0..RELAYED_ENTRY_ID_RING_CAP {
            record_relayed_entry_id("claude", "tmux-cap", &format!("uuid-{i}"));
        }
        assert!(
            !relayed_entry_id_already_seen("claude", "tmux-cap", "uuid-oldest"),
            "the oldest id is dropped once the ring cap is exceeded"
        );
        assert!(
            relayed_entry_id_already_seen(
                "claude",
                "tmux-cap",
                &format!("uuid-{}", RELAYED_ENTRY_ID_RING_CAP - 1)
            ),
            "the newest ids remain"
        );
    }

    /// #3540: head-rotation simulation — `extract_claude_transcript_user_prompt_with_entry_id`
    /// returns the SAME top-level uuid regardless of where the entry sits, so a
    /// surviving entry whose byte offset shifted after a head truncation is still
    /// recognized by identity.
    #[test]
    fn extract_returns_stable_top_level_uuid() {
        let json = serde_json::json!({
            "type": "user",
            "uuid": "6c532800-4c8c-4d1d-9e64-d308fab44a1e",
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": "surviving prompt" }],
            },
            "sessionId": "sess-rot",
        });
        let (prompt, entry_id) =
            extract_claude_transcript_user_prompt_with_entry_id(&json).expect("user prompt");
        assert_eq!(prompt, "surviving prompt");
        assert_eq!(
            entry_id.as_deref(),
            Some("6c532800-4c8c-4d1d-9e64-d308fab44a1e"),
            "the stable top-level uuid is extracted; it survives head rotation \
             (offset shifts, uuid does not) so identity dedup recognizes the \
             re-encountered entry (#3540)"
        );
    }

    /// #3540: a `user` entry with no uuid yields `(prompt, None)` — the scanner
    /// then uses the content-keyed fallback (no panic, no regression).
    #[test]
    fn extract_yields_none_entry_id_when_uuid_absent() {
        let json = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": "no uuid here" }],
            },
            "sessionId": "sess-x",
        });
        let (prompt, entry_id) =
            extract_claude_transcript_user_prompt_with_entry_id(&json).expect("user prompt");
        assert_eq!(prompt, "no uuid here");
        assert_eq!(entry_id, None);
    }
}
