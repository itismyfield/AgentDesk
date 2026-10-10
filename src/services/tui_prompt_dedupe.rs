pub(crate) mod binding_context;
pub(crate) mod binding_events;
pub(crate) mod pending;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
#[cfg(not(test))]
use std::time::Instant;
#[cfg(test)]
pub(crate) use test_clock::Instant;
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
// Per-`(provider, tmux)` cap on the uuid and prompt-id rings, oldest dropped
// first; TTL purge is the primary bound. Prompt-id entries also hold their text.
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
    /// Hook-submitted Claude `prompt_id`, held unannounced until the relay's
    /// announcement POST result settles it.
    pub(crate) hook_prompt_id: Option<String>,
    /// A transcript row's `prompt_id` that was still unannounced when the scanner published the
    /// row; only looked up again before the relay announces, never settled by it.
    pub(crate) row_prompt_id: Option<String>,
    /// Codex native turn this input opened, when its hook or record named one.
    pub(crate) native_turn_id: Option<String>,
    /// A steering input that joined its running native turn: its text is shown, nothing is owned.
    pub(crate) steer_echo: bool,
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
    // Hook `prompt_id` -> its text, so the idle scanner's later row (`promptId`,
    // fresh uuid) is suppressed as the same input once the hook was announced.
    relayed_prompt_ids_by_tmux: HashMap<PromptKey, VecDeque<TimedValue<RelayedPromptId>>>,
    // Latest Codex native turn an observed input opened; a later input naming it steers that turn.
    native_turn_by_tmux: HashMap<PromptKey, TimedValue<NativeTurnSlot>>,
    // Nonces pasted into a running Codex turn; the first observation naming a native turn settles each.
    injected_steer_by_tmux: HashMap<PromptKey, VecDeque<TimedValue<InjectedSteer>>>,
}

#[derive(Clone, Debug)]
struct NativeTurnSlot {
    turn: String,
    /// The opening observation's generation: only its relay may withdraw the slot.
    observed_by: u64,
    owner: NativeTurnOwner,
}

/// Who answers an open native turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeTurnOwner {
    /// The observing relay, until it adopts, hands over or withdraws the turn.
    Relay,
    /// The deferred start worker of this anchor message: its claim keeps the turn, its exit drops it.
    Worker(u64),
    /// A row or a Discord turn.
    Answer,
}

#[derive(Clone, Debug)]
struct RelayedPromptId {
    prompt_id: String,
    prompt: String,
    ambiguous: bool,
    /// False until the relay's announcement POST was sent or may have been; an
    /// unannounced id still tracks text conflicts but suppresses nothing.
    announced: bool,
    /// Observation generation of the hook that recorded the id; only its POST result settles it.
    observed_by: u64,
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

mod extract;
mod injected_steer;
pub(crate) mod native_clear;
mod observation;
mod prompt_identity;
mod runtime_binding;
mod session_rotation;
mod shadow_peek;
mod state;

pub(crate) use session_rotation::*;
#[cfg(test)]
pub(crate) use shadow_peek::hold_binding_peek_lock_for_tests;
pub(crate) use shadow_peek::{
    BindingPeekBusy, peek_tmux_runtime_binding, try_peek_tmux_runtime_binding,
};

pub use extract::*;
use extract::{
    is_discord_relayed_user_prompt, is_user_prefixed_subagent_notification_machine_event,
    native_turn_is_open, normalize_line_endings, normalize_provider, open_native_turn,
    record_relayed_entry_id, relayed_entry_id_already_seen, take_matching_pending_prompt,
    take_or_record_recent_observed_prompt,
};
use injected_steer::{Claim, InjectedSteer, claim_injected_steer};
pub(crate) use injected_steer::{register_injected_steer, withdraw_injected_steer};
use observation::clear_ssh_direct_observation_pending;
pub use observation::*;
#[cfg(test)]
pub(crate) use prompt_identity::age_observed_prompt_records_for_tests;
pub use prompt_identity::{
    ClaudePromptId, extract_claude_transcript_prompt_id, extract_prompt_id_from_hook_payload,
    record_announced_prompt_id, withdraw_unannounced_prompt_id,
};
use prompt_identity::{PromptIdMatch, check_relayed_prompt_id, record_observed_hook_prompt_id};
pub use runtime_binding::*;

#[cfg(test)]
mod injected_steer_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod test_clock {
    use super::*;
    use std::ops::{Add, AddAssign, Sub, SubAssign};

    // The shared state and Tokio workers must observe the same test clock.
    static OFFSET: Mutex<Duration> = Mutex::new(Duration::ZERO);

    fn offset() -> std::sync::MutexGuard<'static, Duration> {
        OFFSET.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub(crate) struct Instant(std::time::Instant);

    impl Instant {
        pub(crate) fn now() -> Self {
            Self(std::time::Instant::now() + *offset())
        }

        pub(crate) fn reset() {
            *offset() = Duration::ZERO;
        }

        pub(crate) fn elapsed(self) -> Duration {
            Self::now().duration_since(self)
        }

        pub(crate) fn duration_since(self, earlier: Self) -> Duration {
            self.0.duration_since(earlier.0)
        }
    }

    impl Add<Duration> for Instant {
        type Output = Self;
        fn add(self, by: Duration) -> Self {
            Self(self.0 + by)
        }
    }
    impl AddAssign<Duration> for Instant {
        fn add_assign(&mut self, by: Duration) {
            self.0 += by;
        }
    }
    impl Sub<Duration> for Instant {
        type Output = Self;
        fn sub(self, by: Duration) -> Self {
            Self(self.0 - by)
        }
    }
    impl SubAssign<Duration> for Instant {
        fn sub_assign(&mut self, by: Duration) {
            self.0 -= by;
        }
    }

    impl TuiPromptDedupeState {
        // Move the clock and existing stamps forward together, preserving their ages.
        pub(super) fn aged_stamp_for_tests(&mut self, age: Duration) -> Instant {
            let recorded = Instant::now();
            macro_rules! shift {
                ($($field:ident),*) => { $(
                    for entry in self.$field.values_mut() { entry.recorded_at += age; }
                )* };
            }
            macro_rules! shift_queues {
                ($($field:ident),*) => { $(
                    for queue in self.$field.values_mut() {
                        for entry in queue { entry.recorded_at += age; }
                    }
                )* };
            }
            shift!(
                tmux_by_provider_session,
                channel_by_tmux,
                runtime_by_tmux,
                prompt_anchor_by_tmux,
                ssh_direct_observation_by_tmux,
                external_input_relay_lease_by_tmux,
                deferred_anchor_completion_by_tmux
            );
            shift_queues!(
                pending_by_tmux,
                recent_observed_by_tmux,
                relayed_entry_ids_by_tmux,
                relayed_prompt_ids_by_tmux,
                injected_steer_by_tmux
            );
            *offset() += age;
            recorded
        }
    }
}
