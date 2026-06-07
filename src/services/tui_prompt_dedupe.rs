use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

use crate::services::agent_protocol::RuntimeHandoffKind;
use chrono::{DateTime, Utc};

const PENDING_PROMPT_TTL: Duration = Duration::from_secs(10);
const RECENT_OBSERVED_TTL: Duration = Duration::from_secs(30);
const SESSION_MAPPING_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const PROMPT_ANCHOR_TTL: Duration = Duration::from_secs(30 * 60);
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

static STATE: LazyLock<Mutex<TuiPromptDedupeState>> =
    LazyLock::new(|| Mutex::new(TuiPromptDedupeState::default()));
#[cfg(test)]
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

/// `generation` sentinel for a freshly constructed lease that has NOT yet been
/// recorded (and therefore not yet stamped with a unique generation).
pub(crate) const EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED: u64 = 0;

fn next_external_input_relay_lease_generation() -> u64 {
    EXTERNAL_INPUT_RELAY_LEASE_GENERATION.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedTuiPrompt {
    pub provider: String,
    pub tmux_session_name: String,
    pub prompt: String,
    pub observed_at: DateTime<Utc>,
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
    ssh_direct_observation_by_tmux: HashMap<PromptKey, TimedValue<()>>,
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
    // the ⏳ → ✅ swap against the just-recorded anchor. Turn-identity safe: the
    // lease that gated the completion and the anchor that drains the marker are
    // recorded by the SAME relay invocation, so a newer same-key turn (which
    // records its own lease/anchor) can never consume this marker.
    deferred_anchor_completion_by_tmux: HashMap<PromptKey, TimedValue<()>>,
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

/// #3174: record a deferred ⏳-completion marker for `(provider, tmux, channel)`.
///
/// Called by the watcher's lease-gated completion path when the gate fired (the
/// external-input lease for THIS turn was present before relay) but the prompt
/// anchor for this turn has not been recorded yet — the provider committed
/// terminal output inside the sub-second `notify-post + ⏳-add` window. Without
/// this marker the anchor-less completion is a silent no-op and the ⏳ is
/// stranded (the lease is cleared after this delivery, so no later pass
/// reconciles it). The SAME turn's [`record_prompt_anchor`] drains this marker
/// and the relay finishes the ⏳ → ✅ swap against the just-recorded anchor.
pub(crate) fn record_deferred_anchor_completion(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
) {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || channel_id == 0 {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.deferred_anchor_completion_by_tmux.insert(
        PromptKey::new(&provider, tmux_session_name),
        TimedValue {
            value: (),
            recorded_at: Instant::now(),
        },
    );
}

/// #3174: drain (read-and-clear) a deferred ⏳-completion marker for
/// `(provider, tmux)`. Returns `true` iff a non-expired marker was present.
///
/// Called by [`record_prompt_anchor`]'s site in the relay immediately after the
/// anchor is recorded (and ⏳ added). Turn-identity safe: the marker can only
/// have been set by a watcher completion gated on THIS turn's external-input
/// lease, and that lease + this anchor are recorded by the SAME relay
/// invocation, so a newer same-key turn never drains another turn's marker.
pub(crate) fn take_deferred_anchor_completion(provider: &str, tmux_session_name: &str) -> bool {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(&provider, tmux_session_name);
    state
        .deferred_anchor_completion_by_tmux
        .remove(&key)
        .is_some()
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

pub(crate) fn clear_tmux_runtime_binding(tmux_session_name: &str) -> bool {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.runtime_by_tmux.remove(tmux_session_name).is_some()
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
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let removed_runtime = state.runtime_by_tmux.remove(tmux_session_name).is_some();
    let removed_channel = state.channel_by_tmux.remove(tmux_session_name).is_some();
    removed_runtime || removed_channel
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
    effect: PromptObservationEffect,
    observed_at: DateTime<Utc>,
) -> PromptObservation {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    let mut candidates = Vec::new();
    for prompt in prompts {
        let prompt = prompt.trim();
        if prompt.is_empty() || is_synthetic_tui_user_prompt(prompt) {
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
    record_external_input_relay_lease(&provider, tmux_session_name, None);
    if effect == PromptObservationEffect::RelayLeaseOnly {
        return PromptObservation::PublishedSshDirect;
    }
    mark_ssh_direct_observation_pending(&provider, tmux_session_name);
    let prompt = candidates
        .first()
        .expect("non-empty candidates")
        .to_string();
    let event = ObservedTuiPrompt {
        provider,
        tmux_session_name: tmux_session_name.to_string(),
        prompt,
        observed_at,
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

fn mark_ssh_direct_observation_pending(provider: &str, tmux_session_name: &str) {
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.ssh_direct_observation_by_tmux.insert(
        PromptKey::new(provider, tmux_session_name),
        TimedValue {
            value: (),
            recorded_at: Instant::now(),
        },
    );
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
    let payload = json.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("message")
        || payload.get("role").and_then(Value::as_str) != Some("user")
    {
        return None;
    }
    reject_synthetic_tui_user_prompt(extract_message_content_text(payload)?)
}

pub fn extract_claude_transcript_user_prompt(json: &Value) -> Option<String> {
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
    reject_synthetic_tui_user_prompt(extract_message_content_text(message)?)
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

fn reject_synthetic_tui_user_prompt(prompt: String) -> Option<String> {
    (!is_synthetic_tui_user_prompt(&prompt)).then_some(prompt)
}

fn is_synthetic_tui_user_prompt(prompt: &str) -> bool {
    let prompt = prompt.trim();
    if prompt.starts_with("<environment_context>") && prompt.ends_with("</environment_context>") {
        return true;
    }
    prompt.starts_with("[Shared Agent Knowledge]\n")
        || prompt.starts_with("[Proactive Memory Guidance]\n")
        || prompt == "No response requested."
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptObservation {
    PublishedSshDirect,
    SuppressedDiscordDuplicate,
    SuppressedRecentDuplicate,
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

pub(crate) fn prompts_match(expected: &str, observed: &str) -> bool {
    let expected_trimmed = normalize_line_endings(expected).trim().to_string();
    let observed_trimmed = normalize_line_endings(observed).trim().to_string();
    if expected_trimmed == observed_trimmed {
        return true;
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
        self.tmux_by_provider_session
            .retain(|_, entry| now.duration_since(entry.recorded_at) <= SESSION_MAPPING_TTL);
        self.channel_by_tmux
            .retain(|_, entry| now.duration_since(entry.recorded_at) <= SESSION_MAPPING_TTL);
        self.runtime_by_tmux
            .retain(|_, entry| now.duration_since(entry.recorded_at) <= SESSION_MAPPING_TTL);
        self.prompt_anchor_by_tmux
            .retain(|_, entry| now.duration_since(entry.recorded_at) <= PROMPT_ANCHOR_TTL);
        self.ssh_direct_observation_by_tmux
            .retain(|_, entry| now.duration_since(entry.recorded_at) <= SSH_DIRECT_OBSERVATION_TTL);
        self.external_input_relay_lease_by_tmux.retain(|_, entry| {
            now.duration_since(entry.recorded_at) <= EXTERNAL_INPUT_RELAY_LEASE_TTL
        });
        self.deferred_anchor_completion_by_tmux.retain(|_, entry| {
            now.duration_since(entry.recorded_at) <= DEFERRED_ANCHOR_COMPLETION_TTL
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset_state() {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        *state = TuiPromptDedupeState::default();
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
        // The anchor-less completion records a deferred marker instead of
        // dropping the ⏳.
        record_deferred_anchor_completion("Claude", "tmux-anchor", 42);

        // 2) The late `record_prompt_anchor` lands for the SAME turn. Its site
        //    drains the deferred marker → the relay finishes the ⏳ → ✅ swap.
        record_prompt_anchor("Claude", "tmux-anchor", 42, 9001);
        assert!(
            take_deferred_anchor_completion("claude", "tmux-anchor"),
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
            !take_deferred_anchor_completion("claude", "tmux-anchor"),
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
            !take_deferred_anchor_completion("claude", "tmux-anchor"),
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

        record_deferred_anchor_completion("claude", "tmux-a", 42);

        // Wrong provider: codex must not drain claude's marker.
        assert!(!take_deferred_anchor_completion("codex", "tmux-a"));
        // Wrong session: a different tmux must not drain it.
        assert!(!take_deferred_anchor_completion("claude", "tmux-b"));
        // The exact key still drains it.
        assert!(take_deferred_anchor_completion("claude", "tmux-a"));
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
}
