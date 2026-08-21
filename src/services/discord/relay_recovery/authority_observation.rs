//! #5464 (#5071 T5) S2 — axis-A relay-authority observation. Records, never
//! decides.
//!
//! Axis A is the bridge lifecycle: the **entry gate** deciding whether a turn
//! keeps delivery authority after its durable inflight row is patched, and the
//! **stream-tick gate** deciding whether a Discord-visible mutation is still
//! authorized. AC2-R (design r3 §1.1) forbids a purely structural signal — "the
//! durable row is gone" — from ending a lifecycle; S4 and S7a make that change.
//! This slice measures it first: each gate computes what the shipped predicate
//! answers (`*_old`) and what the AC2-R predicate would answer (`*_new`), records
//! both, and returns `()`.
//!
//! **The no-op claim is about values, not timing.** No caller can consume a
//! verdict from here, so every judgement and delivery value is what it was. With
//! recording ON, though, each stream tick takes [`TURNS`], and publication paths
//! synchronously encode + append: loop exit; a successor's entry gate *before* its
//! own lifecycle gate and anchor work; and each completion ownership read after
//! terminal outcome delivery. A slow filesystem can therefore delay either a new
//! turn or completion. Under the shipped `Legacy`/`0` dial the per-tick path is one
//! relaxed load of [`ACTIVE_TURNS`] and none of the rest runs at all.
//!
//! Two sinks, one of them authority. The JSONL log
//! (`<agentdesk root>/relay_authority/YYYY-MM-DD.jsonl`) is the ONLY canon for
//! the AC3 promotion gate — a ≥7-day, ≥200-turn window cannot be answered from
//! anything a restart clears. The health ring is live triage, not a gate (§5.3).
//! The cohort is asked once, at entry, so a dial moved mid-turn takes effect at
//! the NEXT turn as `RelayAuthorityMode`'s docstring requires.
//!
//! Ticks coalesce because §5.2 caps the log at one record per turn per site and
//! the flush point is loop exit — but the population this slice exists to measure
//! (`Missing` at entry, `AuthorityLost` mid-stream) leaves the bridge *without*
//! reaching post-loop finalize. So the buffer is keyed by channel, holds one turn,
//! and a successor's entry gate evicts and publishes its predecessor (E4-4).
//!
//! **That eviction is conditional publication, not late publication.** A stranded
//! turn is written only if a successor arrives on its channel while the dial is
//! still observing and the process is still alive; otherwise nothing is written for
//! it, and there is no shutdown flush hook. Exactly what the code below guarantees
//! about that loss, and nothing more:
//!
//! * **Bound** — at most one unpublished turn per channel per process: the buffer
//!   holds one turn and a successor's entry gate evicts it.
//! * **Bias** — toward the measured population. `Missing` at entry and
//!   `AuthorityLost` mid-stream are precisely the turns that leave without a loop
//!   exit, so they are the ones that need a successor.
//! * **What the counters see** — `sink_dropped_records` counts axis-A lifecycle
//!   records the sink *attempted* and could not write; a turn lost this way never reaches the sink,
//!   so nothing counts it. `resident_buffers` is a gauge, so under traffic it cannot
//!   separate "in flight" from "stranded". Both are process-local and gone after a
//!   restart, and neither is in the JSONL canon the promotion gate reads.
//! * **Why a coverage floor cannot see it** — [`publish`] writes a turn's three site
//!   records in one call, so a lost turn loses entry, stream and exit together. The
//!   script's per-site floors are ratios *between* those sites, so whole-turn loss
//!   moves numerator and denominator together, and for the entry-only measured
//!   population it *raises* them (0.667 → 1.000, measured in the r2c review).
//!
//! Visible instead: every record carries `publish_reason`, so the promotion report
//! shows what share of a window was published by eviction — the share that was one
//! absent successor away from being lost. Displayed, not gated; whether it becomes a
//! criterion is S4's (§12-2).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};

use serde::Serialize;
use serde_json::Value;

use super::super::inflight::{GuardedSaveOutcome, InflightTurnState};
use super::super::{SharedData, runtime_store};
use super::cohort;
use crate::config::RelayAuthorityMode;

/// Wire format identifier. The promotion script pins it, so a renamed field or a
/// changed meaning must bump this instead of silently re-interpreting windows
/// already archived under the old spelling. `v2` adds `observed_at` (E4-5).
/// `publish_reason` is additive, so it does not bump: the script counts a record
/// without it as `unattributed` rather than assuming the reassuring value. `v3`
/// adds post-flush completion ownership records with `scope` and `scope_reason`.
const OBSERVATION_SCHEMA: &str = "relay_authority.axis_a.v3";
/// Per-channel triage ring depth on `/api/health/detail` (design §5.3).
const TRIAGE_RING_DEPTH: usize = 16;
/// How many channels the triage block keeps rings for, least-recently-recorded
/// evicted first. The ring depth bounds each entry but nothing bounded the map:
/// a long-lived `Observe`/100 process accumulated one entry per channel it ever
/// saw and never released it, growing `/api/health/detail` for the life of the
/// process even after the dial went back to zero.
const TRIAGE_CHANNEL_CAP: usize = 32;

/// What a lifecycle gate answers. `End` is the only value that takes delivery
/// authority away from the turn; `Suppress` withholds one visible mutation and
/// leaves the turn alive, and `ContinueRowless` is the no-durable-row
/// continuation S7a introduces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LifecycleVerdict {
    Continue,
    ContinueRowless,
    Suppress,
    Retry,
    End,
}

impl LifecycleVerdict {
    /// The one property AC2-R constrains: the new predicate may end fewer
    /// lifecycles than the shipped one and never more.
    pub(crate) const fn ends_lifecycle(self) -> bool {
        matches!(self, Self::End)
    }
}

/// Shipped entry gate — the shape of
/// `turn_bridge::bridge_entry_persist::bridge_entry_lifecycle_can_continue`, pinned
/// against it over the whole outcome domain by that file's
/// `recorded_entry_gate_old_mirrors_the_shipped_lifecycle_gate`, so this cannot
/// drift into describing a gate that no longer ships.
pub(in crate::services::discord) const fn entry_gate_old(
    outcome: GuardedSaveOutcome,
) -> LifecycleVerdict {
    match outcome {
        GuardedSaveOutcome::Saved => LifecycleVerdict::Continue,
        _ => LifecycleVerdict::End,
    }
}

/// AC2-R entry gate (S7a's `BridgeEntryDisposition`). `Missing` is the structural
/// signal AC1 forbids ending delivery authority on, so the turn continues rowless;
/// `IdentityMismatch` says another turn owns the row, which is an exact-episode
/// veto and still ends this one, and `IoError` stays fail-closed.
pub(in crate::services::discord) const fn entry_gate_new(
    outcome: GuardedSaveOutcome,
) -> LifecycleVerdict {
    match outcome {
        GuardedSaveOutcome::Saved => LifecycleVerdict::Continue,
        GuardedSaveOutcome::Missing => LifecycleVerdict::ContinueRowless,
        GuardedSaveOutcome::IdentityMismatch | GuardedSaveOutcome::IoError => LifecycleVerdict::End,
    }
}

/// Shipped stream-tick gate — the operand-for-operand mirror of
/// `stream_tick::guarded_persist::visible_mutation_authority_after_guarded_save`,
/// pinned against it over the full three-operand product by that file's
/// `recorded_stream_gate_old_mirrors_the_shipped_authority_mapping`.
pub(in crate::services::discord) const fn stream_gate_old(
    outcome: GuardedSaveOutcome,
    authority_unchanged: bool,
    bridge_owns_relay: bool,
) -> LifecycleVerdict {
    match outcome {
        GuardedSaveOutcome::Saved if authority_unchanged && bridge_owns_relay => {
            LifecycleVerdict::Continue
        }
        GuardedSaveOutcome::Saved if authority_unchanged => LifecycleVerdict::Suppress,
        GuardedSaveOutcome::Saved
        | GuardedSaveOutcome::Missing
        | GuardedSaveOutcome::IdentityMismatch => LifecycleVerdict::End,
        GuardedSaveOutcome::IoError => LifecycleVerdict::Retry,
    }
}

/// AC2-R stream-tick gate (S4's `Missing → Suppressed`). One cell moves — a
/// vanished durable row suppresses this tick's visible mutation instead of ending
/// the turn, so `post_loop_finalize` stays reachable and the finished answer is not
/// orphaned inside a deleted row. Every other cell is `stream_gate_old` verbatim,
/// which keeps S4 auditable as a single cell rather than a rewritten table.
pub(in crate::services::discord) const fn stream_gate_new(
    outcome: GuardedSaveOutcome,
    authority_unchanged: bool,
    bridge_owns_relay: bool,
) -> LifecycleVerdict {
    match outcome {
        GuardedSaveOutcome::Missing => LifecycleVerdict::Suppress,
        other => stream_gate_old(other, authority_unchanged, bridge_owns_relay),
    }
}

/// Shape of the terminal delivery range in the three cases
/// `BridgeLeaseAcquire::NoRange` is defined over: no tmux end to advance against,
/// an empty/inverted range, or a real one. Design §4.3 makes the first two the
/// `no_range_share` residue — the rowless population where the frontier predicate
/// the G-rowless gate needs is undefined — so the gate has to count them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LeaseRangeShape {
    Absent,
    Empty,
    Advancing,
}

/// Pure classifier over the same two operands the terminal delivery site builds its
/// range from — `inflight_state.turn_start_offset` and `tmux_last_offset`.
pub(crate) const fn lease_range_shape(
    turn_start_offset: Option<u64>,
    tmux_last_offset: Option<u64>,
) -> LeaseRangeShape {
    let Some(end) = tmux_last_offset else {
        return LeaseRangeShape::Absent;
    };
    let start = match turn_start_offset {
        Some(start) => start,
        None => 0,
    };
    if end > start {
        LeaseRangeShape::Advancing
    } else {
        LeaseRangeShape::Empty
    }
}

/// Entry gate. The store outcome is kept because the verdict pair alone cannot
/// separate the two ways a turn ends — `IdentityMismatch` (a real takeover) and
/// `IoError` (transient) both map to `(End, End)` and only one is a defect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct EntryGateObservation {
    guarded_save: &'static str,
    old: LifecycleVerdict,
    new: LifecycleVerdict,
    rowless_continuation: bool,
}

/// Stream-tick gate, coalesced over the turn's ticks. `new_stricter` is the alarm
/// counter and the reason the tally is not just a diff count: a nonzero value means
/// S4 would terminate a turn the shipped gate kept alive, so it must read `0` across
/// the whole promotion window before S4 may enforce.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
struct StreamGateTally {
    ticks: u32,
    old_ended_lifecycle: u32,
    new_ended_lifecycle: u32,
    diff: u32,
    new_stricter: u32,
}

/// Loop exit: this turn's terminal range shape. It does NOT carry
/// `rowless_continuation`: a `Missing`-at-entry turn is ended by the shipped gate
/// before the bridge loop starts, so that field was permanently `false` here and an
/// always-false field is a false-green vector. `rowless_no_range_share` is S7a's to
/// measure once enforcement makes the population reachable (ERRATUM R3-E4/E4-6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct LoopExitObservation {
    lease_range_shape: LeaseRangeShape,
}

/// The §5.2 key set, stamped when a turn's buffer is opened rather than when it is
/// flushed: an evicted straggler is written by a *different* turn's call and has to
/// carry its own runtime identity, not the evictor's.
///
/// `observed_at` is the turn's own start time and `ts` (added by [`encode`]) is the
/// publish time. They are not the same instant and the difference is not noise: an
/// evicted straggler is published when its *successor* arrives, which for a channel
/// idle for hours dates the record hours late. Windowing and fingerprint
/// segmentation therefore key off `observed_at` (ERRATUM R3-E4/E4-5).
#[derive(Clone, Debug, Serialize)]
struct TurnStamp {
    host: String,
    api_port: u16,
    process_generation: u64,
    runtime_ptr: String,
    provider: String,
    channel_id: u64,
    turn_id: u64,
    observed_at: String,
    cohort_fingerprint: String,
}

/// The in-memory turn buffer. At most one per channel. The identity axes are held
/// inline rather than as an `InflightTurnIdentity`, whose fourth field this module
/// can neither compare nor read — holding it write-only claimed an axis the buffer
/// does not have (legA r2c P2-1).
#[derive(Clone, Debug)]
struct TurnObservation {
    stamp: TurnStamp,
    user_msg_id: u64,
    started_at: String,
    turn_start_offset: Option<u64>,
    entry: Option<EntryGateObservation>,
    stream: StreamGateTally,
    exit: Option<LoopExitObservation>,
}

impl TurnObservation {
    /// Whether this buffer belongs to the turn the caller is reporting, judged on
    /// the three `inflight::model::identity` axes a turn cannot change while it is
    /// inside the bridge — not on `(user_msg_id, started_at)` alone. That pair is
    /// not an identity: `identity.rs` keeps `turn_start_offset` precisely because
    /// two consecutive `user_msg_id == 0` TUI-direct turns collide at `started_at`'s
    /// one-second resolution, and mistaking one for the other tallies a
    /// predecessor's ticks into its successor *and* drops the successor's buffer.
    ///
    /// `tmux_session_name` is the canon's fourth axis and is deliberately absent:
    /// a runtime handoff re-assigns it mid-turn at five production sites and
    /// `stream_loop::refresh_stream_tick_expected_identity_after_handoff` re-derives
    /// the expected identity from the live state, so comparing it would strand every
    /// turn that spawns a runtime.
    ///
    /// Two residual collision domains, declared rather than implied. (1) The one this
    /// relaxation opens: two turns agreeing on all three axes and differing only in
    /// session. Not constructible in production — a session change requires a handoff
    /// (tmux spawn plus readiness wait, seconds) and `started_at` has one-second
    /// resolution, so the two turns cannot share it. (2) One the canon shares: two
    /// turns agreeing on all FOUR axes, reachable when a turn appends no bytes and
    /// its successor starts in the same second, since `turn_start_offset` seeds from
    /// the last offset. Strict four-axis comparison does not close that either.
    fn is_turn(&self, state: &InflightTurnState) -> bool {
        self.user_msg_id == state.user_msg_id
            && self.started_at == state.started_at
            && self.turn_start_offset == state.turn_start_offset
    }
}

static TURNS: LazyLock<Mutex<HashMap<u64, TurnObservation>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Lock-free "is anything being observed at all" flag for the per-tick path. Under
/// the shipped dormant dial this stays `0` for the life of the process. It doubles
/// as the resident-buffer gauge the triage block publishes: every value in it is a
/// turn whose record has not been written yet.
static ACTIVE_TURNS: AtomicUsize = AtomicUsize::new(0);
/// Records the JSONL sink dropped — an unwritable runtime root, a failed open, a
/// failed line write. Silence in the sink is survivable only if it is visible, and
/// this is the only place it becomes visible (E4-4).
static SINK_DROPPED_RECORDS: AtomicU64 = AtomicU64::new(0);
/// Completion ownership records dropped by their separate best-effort sink path.
static COMPLETION_SINK_DROPPED_RECORDS: AtomicU64 = AtomicU64::new(0);
/// Dial-independent fail-closed completion decisions. Unlike
/// `completion_scopes`, this advances under the shipped Legacy/0 dial.
static COMPLETION_SUPPRESSIONS: AtomicU64 = AtomicU64::new(0);
static TRIAGE: LazyLock<Mutex<RelayAuthorityObservationReport>> = LazyLock::new(Mutex::default);

fn turns() -> MutexGuard<'static, HashMap<u64, TurnObservation>> {
    TURNS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The single cohort question this slice asks, asked once per turn. Both operands
/// veto and both shipped values are the denying one, so a node nobody enrolled never
/// reaches any code below this line. `Enforce` records too — recording is not a
/// decision, so a promoted cohort keeps producing the comparison its rollback
/// decision would be made on.
fn observing_dial(channel_id: u64) -> Option<(RelayAuthorityMode, u8)> {
    let (mode, percent) = crate::config_live_reload::current()
        .map(|config| {
            (
                config.runtime.relay_authority_mode,
                config.runtime.relay_authority_cohort_percent,
            )
        })
        .unwrap_or_default();
    (mode.records_authority_observations() && cohort::admits(mode, percent, channel_id))
        .then_some((mode, percent))
}

/// Entry-gate record point, called from
/// `turn_bridge::bridge_entry_persist::establish_bridge_entry_authority`.
pub(in crate::services::discord) fn record_bridge_entry_gate(
    shared: &SharedData,
    state: &InflightTurnState,
    outcome: GuardedSaveOutcome,
) {
    if let Some(dial) = observing_dial(state.channel_id) {
        open_turn(dial, shared, state, outcome);
    }
}

/// Opens this turn's buffer and evicts whatever predecessor was left on this channel
/// by a bridge exit that never reached post-loop finalize.
///
/// Eviction always flushes, never discards: the displaced buffer is handed to
/// [`publish`] before this turn takes the slot. That is a statement about *this*
/// layer only — the sink [`publish`] hands it to is best-effort by contract and
/// counts axis-A lifecycle records it could not write in `sink_dropped_records`
/// (legA r3c P2-5).
fn open_turn(
    (mode, percent): (RelayAuthorityMode, u8),
    shared: &SharedData,
    state: &InflightTurnState,
    outcome: GuardedSaveOutcome,
) {
    let new = entry_gate_new(outcome);
    let observation = TurnObservation {
        stamp: TurnStamp {
            host: std::env::var("HOSTNAME").unwrap_or_else(|_| "local".to_string()),
            api_port: shared.api_port,
            process_generation: runtime_store::process_generation(),
            runtime_ptr: format!("{:p}", std::ptr::from_ref(shared)),
            provider: state.provider.clone(),
            channel_id: state.channel_id,
            turn_id: state.effective_finalizer_turn_id(),
            observed_at: state.started_at.clone(),
            cohort_fingerprint: cohort::cohort_fingerprint(mode, percent),
        },
        user_msg_id: state.user_msg_id,
        started_at: state.started_at.clone(),
        turn_start_offset: state.turn_start_offset,
        entry: Some(EntryGateObservation {
            guarded_save: guarded_save_label(outcome),
            old: entry_gate_old(outcome),
            new,
            rowless_continuation: new == LifecycleVerdict::ContinueRowless,
        }),
        stream: StreamGateTally::default(),
        exit: None,
    };
    let stranded = {
        let mut turns = turns();
        let stranded = turns.insert(state.channel_id, observation);
        ACTIVE_TURNS.store(turns.len(), Ordering::Relaxed);
        stranded
    };
    if let Some(stranded) = stranded {
        publish(stranded, "evicted");
    }
}

/// Stream-tick gate record point, called from
/// `stream_tick::guarded_persist::visible_mutation_authority_after_guarded_save` so
/// all sixteen `authorize_visible_mutation!` sites are covered by one call. Tallies
/// in memory; the write happens at loop exit.
pub(in crate::services::discord) fn record_stream_loop_gate(
    state: &InflightTurnState,
    outcome: GuardedSaveOutcome,
    authority_unchanged: bool,
    bridge_owns_relay: bool,
) {
    if ACTIVE_TURNS.load(Ordering::Relaxed) == 0 {
        return;
    }
    let old = stream_gate_old(outcome, authority_unchanged, bridge_owns_relay);
    let new = stream_gate_new(outcome, authority_unchanged, bridge_owns_relay);
    let mut turns = turns();
    let Some(turn) = turns
        .get_mut(&state.channel_id)
        .filter(|turn| turn.is_turn(state))
    else {
        return;
    };
    turn.stream.ticks = turn.stream.ticks.saturating_add(1);
    turn.stream.old_ended_lifecycle += u32::from(old.ends_lifecycle());
    turn.stream.new_ended_lifecycle += u32::from(new.ends_lifecycle());
    turn.stream.diff += u32::from(old != new);
    turn.stream.new_stricter += u32::from(!old.ends_lifecycle() && new.ends_lifecycle());
}

/// Loop-exit record point and this turn's single flush, called from
/// `turn_bridge::post_loop_finalize::run_post_loop_finalize`.
pub(in crate::services::discord) fn record_loop_exit(
    state: &InflightTurnState,
    tmux_last_offset: Option<u64>,
) {
    if ACTIVE_TURNS.load(Ordering::Relaxed) == 0 {
        return;
    }
    let finished = {
        let mut turns = turns();
        if !turns
            .get(&state.channel_id)
            .is_some_and(|turn| turn.is_turn(state))
        {
            return;
        }
        let finished = turns.remove(&state.channel_id);
        ACTIVE_TURNS.store(turns.len(), Ordering::Relaxed);
        finished
    };
    let Some(mut turn) = finished else { return };
    turn.exit = Some(LoopExitObservation {
        lease_range_shape: lease_range_shape(state.turn_start_offset, tmux_last_offset),
    });
    publish(turn, "loop_exit");
}

/// Record completion ownership after [`record_loop_exit`] has removed and flushed
/// the coalesced turn buffer. This path intentionally rebuilds the stamp instead of
/// reviving that buffer or changing S2's loop-exit publication semantics.
pub(in crate::services::discord) struct CompletionScopeRecord<'a> {
    pub(in crate::services::discord) shared: &'a SharedData,
    pub(in crate::services::discord) provider: &'a crate::services::provider::ProviderKind,
    pub(in crate::services::discord) turn_id: u64,
    pub(in crate::services::discord) channel_id: u64,
    pub(in crate::services::discord) site: &'static str,
    pub(in crate::services::discord) turn_source: &'static str,
    pub(in crate::services::discord) scope: &'static str,
    pub(in crate::services::discord) scope_reason: &'static str,
}

pub(in crate::services::discord) fn record_completion_scope(record: CompletionScopeRecord<'_>) {
    let Some(dial) = observing_dial(record.channel_id) else {
        return;
    };
    record_completion_scope_at(dial, record);
}

pub(in crate::services::discord) fn record_completion_suppression() {
    COMPLETION_SUPPRESSIONS.fetch_add(1, Ordering::Relaxed);
}

fn record_completion_scope_at(
    (mode, percent): (RelayAuthorityMode, u8),
    record: CompletionScopeRecord<'_>,
) {
    let CompletionScopeRecord {
        shared,
        provider,
        turn_id,
        channel_id,
        site,
        turn_source,
        scope,
        scope_reason,
    } = record;
    let stamp = TurnStamp {
        host: std::env::var("HOSTNAME").unwrap_or_else(|_| "local".to_string()),
        api_port: shared.api_port,
        process_generation: runtime_store::process_generation(),
        runtime_ptr: format!("{:p}", std::ptr::from_ref(shared)),
        provider: provider.as_str().to_string(),
        channel_id,
        turn_id,
        observed_at: chrono::Local::now().to_rfc3339(),
        cohort_fingerprint: cohort::cohort_fingerprint(mode, percent),
    };
    let Some(line) = encode_scope(&stamp, site, turn_source, scope, scope_reason) else {
        drop_records(&COMPLETION_SINK_DROPPED_RECORDS, 1);
        return;
    };
    append_jsonl(&[line], &COMPLETION_SINK_DROPPED_RECORDS);
    let mut triage = TRIAGE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *triage
        .completion_scopes
        .entry(format!("{site}:{scope}:{scope_reason}"))
        .or_default() += 1;
}

const fn guarded_save_label(outcome: GuardedSaveOutcome) -> &'static str {
    match outcome {
        GuardedSaveOutcome::Saved => "saved",
        GuardedSaveOutcome::Missing => "missing",
        GuardedSaveOutcome::IdentityMismatch => "identity_mismatch",
        GuardedSaveOutcome::IoError => "io_error",
    }
}

/// Write a finished turn to both sinks. Never called with `TURNS` held: the JSONL
/// append is blocking file I/O at the same per-turn cadence `metrics::record_turn`
/// already runs at in the completion path.
///
/// `publish_reason` is this publication's provenance, on every one of the turn's
/// records because one call writes them all. Two values, both pinned by tests since
/// a literal typo here would be silent: `"loop_exit"` from [`record_loop_exit`] and
/// `"evicted"` from a successor's entry gate in [`open_turn`].
fn publish(turn: TurnObservation, publish_reason: &'static str) {
    let stream = (turn.stream.ticks > 0).then_some(turn.stream);
    let sites = [
        ("bridge_entry", turn.entry.map(axis_a)),
        ("stream_loop", stream.map(axis_a)),
        ("loop_exit", turn.exit.map(axis_a)),
    ];
    let lines: Vec<String> = sites
        .into_iter()
        .filter_map(|(site, axis_a)| encode(&turn.stamp, publish_reason, site, axis_a?))
        .collect();
    append_jsonl(&lines, &SINK_DROPPED_RECORDS);
    record_triage(&turn);
}

fn axis_a<T: Serialize>(observation: T) -> Value {
    serde_json::to_value(observation).unwrap_or(Value::Null)
}

fn encode(
    stamp: &TurnStamp,
    publish_reason: &'static str,
    site: &'static str,
    axis_a: Value,
) -> Option<String> {
    #[derive(Serialize)]
    struct ObservationRecord<'a> {
        schema: &'static str,
        ts: String,
        publish_reason: &'static str,
        #[serde(flatten)]
        stamp: &'a TurnStamp,
        site: &'static str,
        axis_a: Value,
    }

    serde_json::to_string(&ObservationRecord {
        schema: OBSERVATION_SCHEMA,
        ts: chrono::Local::now().to_rfc3339(),
        publish_reason,
        stamp,
        site,
        axis_a,
    })
    .ok()
}

fn encode_scope(
    stamp: &TurnStamp,
    site: &'static str,
    turn_source: &'static str,
    scope: &'static str,
    scope_reason: &'static str,
) -> Option<String> {
    #[derive(Serialize)]
    struct CompletionScopeRecord<'a> {
        schema: &'static str,
        ts: String,
        publish_reason: &'static str,
        #[serde(flatten)]
        stamp: &'a TurnStamp,
        site: &'static str,
        turn_source: &'static str,
        scope: &'static str,
        scope_reason: &'static str,
    }

    serde_json::to_string(&CompletionScopeRecord {
        schema: OBSERVATION_SCHEMA,
        ts: chrono::Local::now().to_rfc3339(),
        publish_reason: "post_flush",
        stamp,
        site,
        turn_source,
        scope,
        scope_reason,
    })
    .ok()
}

/// Best-effort append into today's event file, in the shape `metrics::today_file`
/// uses. An unwritable runtime root drops the observation rather than propagate
/// anything back into the turn that produced it — but the drop is counted, because
/// the promotion gate cannot tell a quiet node from a lossy sink on sample counts
/// alone: a window of entry-only turns passes a floor that only counts turns. The
/// per-site coverage floors in `scripts/relay_authority_rollout_report.py` and this
/// counter are the two halves of that answer (legB P1-2).
fn append_jsonl(lines: &[String], dropped_records: &AtomicU64) {
    if lines.is_empty() {
        return;
    }
    let Some(dir) = runtime_store::agentdesk_root().map(|root| root.join("relay_authority")) else {
        drop_records(dropped_records, lines.len());
        return;
    };
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(format!("{}.jsonl", chrono::Local::now().format("%Y-%m-%d")));
    let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) else {
        drop_records(dropped_records, lines.len());
        return;
    };
    for line in lines {
        if writeln!(file, "{line}").is_err() {
            drop_records(dropped_records, 1);
        }
    }
}

fn drop_records(counter: &AtomicU64, count: usize) {
    counter.fetch_add(count as u64, Ordering::Relaxed);
}

/// One turn as `/api/health/detail` shows it.
#[derive(Clone, Debug, Serialize)]
struct TurnTriageEntry {
    turn_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    entry: Option<EntryGateObservation>,
    stream: StreamGateTally,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit: Option<LoopExitObservation>,
}

/// Read-only axis-A observation block for `/api/health/detail`, and the state behind
/// it. Triage only, for the same reason `cohort::RelayAuthorityRolloutReport` is:
/// this is one process's sample since its last restart, and the AC3 promotion gate
/// reads the JSONL log instead (design §5.3).
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct RelayAuthorityObservationReport {
    turns_recorded: u64,
    rowless_continuations: u64,
    stream_diff_ticks: u64,
    /// Must stay `0`: a nonzero value is the new predicate ending a lifecycle the
    /// shipped one kept, which AC2-R's monotone-relaxing contract forbids.
    new_stricter_verdicts: u64,
    /// Turns buffered right now with nothing written for them yet. A value that
    /// does not fall back toward zero is the stranded population of the module
    /// docstring: buffers whose only publisher is a successor that never came.
    resident_buffers: u64,
    /// Cumulative axis-A lifecycle records the JSONL sink threw away. Nonzero
    /// means the promotion canon is incomplete and its window is not trustworthy.
    sink_dropped_records: u64,
    /// Cumulative completion ownership records their sink path threw away.
    completion_sink_dropped_records: u64,
    /// Fail-closed completion decisions, counted regardless of rollout dial/cohort.
    pub(crate) completion_suppressions: u64,
    /// Completion-time ownership samples keyed by `site:scope:scope_reason`.
    pub(crate) completion_scopes: BTreeMap<String, u64>,
    channels: BTreeMap<String, VecDeque<TurnTriageEntry>>,
    /// Channel keys in least-recently-recorded order, bounding `channels`. Not
    /// published: it is the eviction order, not triage data.
    #[serde(skip)]
    channel_order: VecDeque<String>,
}

fn record_triage(turn: &TurnObservation) {
    let mut triage = TRIAGE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    triage.turns_recorded += 1;
    triage.rowless_continuations +=
        u64::from(turn.entry.is_some_and(|entry| entry.rowless_continuation));
    triage.stream_diff_ticks += u64::from(turn.stream.diff);
    triage.new_stricter_verdicts += u64::from(turn.stream.new_stricter);
    let channel = turn.stamp.channel_id.to_string();
    let ring = triage.channels.entry(channel.clone()).or_default();
    ring.push_back(TurnTriageEntry {
        turn_id: turn.stamp.turn_id,
        entry: turn.entry,
        stream: turn.stream,
        exit: turn.exit,
    });
    while ring.len() > TRIAGE_RING_DEPTH {
        ring.pop_front();
    }
    triage.channel_order.retain(|key| key != &channel);
    triage.channel_order.push_back(channel);
    while triage.channel_order.len() > TRIAGE_CHANNEL_CAP {
        if let Some(evicted) = triage.channel_order.pop_front() {
            triage.channels.remove(&evicted);
        }
    }
}

pub(crate) fn observation_report() -> RelayAuthorityObservationReport {
    let mut report = TRIAGE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    // Both live outside the triage lock: one is the buffer map's own size and the
    // other is written from the sink, which never takes this lock.
    report.resident_buffers = ACTIVE_TURNS.load(Ordering::Relaxed) as u64;
    report.sink_dropped_records = SINK_DROPPED_RECORDS.load(Ordering::Relaxed);
    report.completion_sink_dropped_records =
        COMPLETION_SINK_DROPPED_RECORDS.load(Ordering::Relaxed);
    report.completion_suppressions = COMPLETION_SUPPRESSIONS.load(Ordering::Relaxed);
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::provider::ProviderKind;

    const OUTCOMES: [GuardedSaveOutcome; 4] = [
        GuardedSaveOutcome::Saved,
        GuardedSaveOutcome::Missing,
        GuardedSaveOutcome::IdentityMismatch,
        GuardedSaveOutcome::IoError,
    ];
    /// The dial an operator has to move to before any of this runs.
    const OBSERVING: (RelayAuthorityMode, u8) = (RelayAuthorityMode::Observe, 100);

    fn state(channel_id: u64, user_msg_id: u64) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-authority-observation".to_string()),
            343_742_347_365_974_026,
            user_msg_id,
            0,
            "prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-authority-observation".to_string()),
            Some("/tmp/AgentDesk-authority-observation.jsonl".to_string()),
            Some("/tmp/AgentDesk-authority-observation.input".to_string()),
            512,
        )
    }

    /// A TUI-direct turn: `user_msg_id == 0` and a fixed `started_at`, so two of
    /// these collide on the pair the buffer used to key on and differ only on the
    /// `turn_start_offset` axis `identity.rs` added for exactly this collision.
    /// Clearing `finalizer_turn_id` makes `effective_finalizer_turn_id` re-derive
    /// the synthetic id, which folds the offset in — so the two turns are also
    /// distinguishable in the published records.
    fn tui_direct_state(channel_id: u64, turn_start_offset: u64) -> InflightTurnState {
        let mut state = state(channel_id, 0);
        state.started_at = "2026-08-20 23:07:49".to_string();
        state.turn_start_offset = Some(turn_start_offset);
        state.finalizer_turn_id = 0;
        state
    }

    fn read_events(root: &std::path::Path) -> Vec<serde_json::Value> {
        let dir = root.join("relay_authority");
        let mut events = Vec::new();
        let Ok(entries) = fs::read_dir(&dir) else {
            return events;
        };
        for entry in entries {
            let path = entry.expect("event log entry").path();
            for line in fs::read_to_string(&path)
                .expect("event log file")
                .lines()
                .filter(|line| !line.trim().is_empty())
            {
                events.push(serde_json::from_str(line).expect("event log line is JSON"));
            }
        }
        events
    }

    fn events_for(root: &std::path::Path, channel_id: u64) -> Vec<serde_json::Value> {
        read_events(root)
            .into_iter()
            .filter(|event| event["channel_id"].as_u64() == Some(channel_id))
            .collect()
    }

    #[test]
    fn completion_scope_is_a_post_flush_record_and_live_triage_counter() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel = 4_259_218;

        record_completion_scope_at(
            OBSERVING,
            CompletionScopeRecord {
                shared: &shared,
                provider: &ProviderKind::Claude,
                turn_id: 77_080,
                channel_id: channel,
                site: "completion_r1",
                turn_source: "external_input",
                scope: "unprovable",
                scope_reason: "mailbox_absent",
            },
        );

        let events = events_for(temp.path(), channel);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event["schema"].as_str(), Some(OBSERVATION_SCHEMA));
        assert_eq!(event["publish_reason"].as_str(), Some("post_flush"));
        assert_eq!(event["site"].as_str(), Some("completion_r1"));
        assert_eq!(event["turn_source"].as_str(), Some("external_input"));
        assert_eq!(event["scope"].as_str(), Some("unprovable"));
        assert_eq!(event["scope_reason"].as_str(), Some("mailbox_absent"));
        assert!(event.get("axis_a").is_none());
        assert_eq!(
            observation_report()
                .completion_scopes
                .get("completion_r1:unprovable:mailbox_absent"),
            Some(&1),
            "mailbox-handle absence must remain visible in rollout-scoped triage"
        );
    }

    /// AC2-R's monotone-relaxing contract, asserted as a property over the full
    /// input domain of both gates rather than on the cells that happen to move:
    /// no input exists for which the new predicate ends a lifecycle the shipped
    /// one continued. This is the precondition S4 and S7a are allowed to
    /// enforce under, so it has to fail here if either table is ever widened.
    #[test]
    fn neither_new_gate_ends_a_lifecycle_the_shipped_gate_continued() {
        for outcome in OUTCOMES {
            assert!(
                !(!entry_gate_old(outcome).ends_lifecycle()
                    && entry_gate_new(outcome).ends_lifecycle()),
                "{outcome:?}: the new entry gate ended a lifecycle the shipped gate kept"
            );
            for authority_unchanged in [true, false] {
                for bridge_owns_relay in [true, false] {
                    let old = stream_gate_old(outcome, authority_unchanged, bridge_owns_relay);
                    let new = stream_gate_new(outcome, authority_unchanged, bridge_owns_relay);
                    assert!(
                        !(!old.ends_lifecycle() && new.ends_lifecycle()),
                        "{outcome:?}/{authority_unchanged}/{bridge_owns_relay}: the new stream \
                         gate ended a lifecycle the shipped gate kept"
                    );
                }
            }
        }
    }

    /// The two cells the AC2-R change actually moves, and the two that must not
    /// move with them — an exact-episode veto and a store I/O failure are not
    /// structural signals, so AC1 does not reach them.
    #[test]
    fn only_the_missing_row_cell_differs_between_old_and_new() {
        assert_eq!(
            (
                entry_gate_old(GuardedSaveOutcome::Missing),
                entry_gate_new(GuardedSaveOutcome::Missing)
            ),
            (LifecycleVerdict::End, LifecycleVerdict::ContinueRowless),
        );
        assert_eq!(
            (
                stream_gate_old(GuardedSaveOutcome::Missing, true, true),
                stream_gate_new(GuardedSaveOutcome::Missing, true, true)
            ),
            (LifecycleVerdict::End, LifecycleVerdict::Suppress),
        );
        for unchanged in [
            GuardedSaveOutcome::IdentityMismatch,
            GuardedSaveOutcome::IoError,
        ] {
            assert_eq!(entry_gate_old(unchanged), entry_gate_new(unchanged));
            assert_eq!(
                stream_gate_old(unchanged, true, true),
                stream_gate_new(unchanged, true, true),
            );
        }
        assert_eq!(
            entry_gate_new(GuardedSaveOutcome::IdentityMismatch),
            LifecycleVerdict::End,
            "an exact-episode veto is not a structural signal and still ends the turn",
        );
    }

    #[test]
    fn lease_range_shape_matches_the_no_range_definition() {
        assert_eq!(lease_range_shape(Some(10), None), LeaseRangeShape::Absent);
        assert_eq!(
            lease_range_shape(Some(10), Some(10)),
            LeaseRangeShape::Empty
        );
        assert_eq!(lease_range_shape(Some(10), Some(9)), LeaseRangeShape::Empty);
        assert_eq!(lease_range_shape(None, Some(0)), LeaseRangeShape::Empty);
        assert_eq!(
            lease_range_shape(Some(10), Some(11)),
            LeaseRangeShape::Advancing
        );
        assert_eq!(lease_range_shape(None, Some(1)), LeaseRangeShape::Advancing);
    }

    /// The deployment no-op, stated as the property that makes it one: under
    /// the SHIPPED dial the cohort question answers `false` for every channel,
    /// so the entry gate — the only site that asks it — opens no buffer, and
    /// the other two sites have nothing to attach to.
    #[test]
    fn the_shipped_dial_admits_no_channel_to_the_observation_cohort() {
        let defaults = crate::config::RuntimeSettingsConfig::default();
        assert_eq!(defaults.relay_authority_mode, RelayAuthorityMode::Legacy);
        assert!(
            !defaults
                .relay_authority_mode
                .records_authority_observations()
        );
        for channel_id in (0..2_000u64).map(|index| 1_234_567_890_123_456_789 + index * 7) {
            assert!(
                observing_dial(channel_id).is_none(),
                "channel {channel_id} was admitted to the observation cohort by the shipped dial"
            );
        }
    }

    /// With the dial moved, one turn produces exactly the three site events
    /// §5.2 specifies, each carrying the whole §5.2 key set.
    #[test]
    fn an_observed_turn_emits_one_event_per_site_with_the_full_key_set() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel = 4_259_211;
        let state = state(channel, 77_010);

        open_turn(OBSERVING, &shared, &state, GuardedSaveOutcome::Missing);
        record_stream_loop_gate(&state, GuardedSaveOutcome::Missing, true, true);
        record_stream_loop_gate(&state, GuardedSaveOutcome::Saved, true, true);
        record_loop_exit(&state, Some(4_096));

        let events = events_for(temp.path(), channel);
        assert_eq!(
            events
                .iter()
                .map(|event| event["site"].as_str().unwrap_or_default())
                .collect::<Vec<_>>(),
            ["bridge_entry", "stream_loop", "loop_exit"],
        );
        for event in &events {
            let mut keys: Vec<&str> = event
                .as_object()
                .expect("event is an object")
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                [
                    "api_port",
                    "axis_a",
                    "channel_id",
                    "cohort_fingerprint",
                    "host",
                    "observed_at",
                    "process_generation",
                    "provider",
                    "publish_reason",
                    "runtime_ptr",
                    "schema",
                    "site",
                    "ts",
                    "turn_id",
                ],
                "every event carries the whole design §5.2 key set"
            );
            assert_eq!(event["schema"].as_str(), Some(OBSERVATION_SCHEMA));
            assert_eq!(
                event["publish_reason"].as_str(),
                Some("loop_exit"),
                "a turn that reached post-loop finalize was published by its own flush",
            );
            assert_eq!(
                event["observed_at"].as_str(),
                Some(state.started_at.as_str()),
                "the record carries the turn's own start time, not just the publish time"
            );
            assert_eq!(
                event["cohort_fingerprint"].as_str(),
                Some(cohort::cohort_fingerprint(OBSERVING.0, OBSERVING.1).as_str()),
            );
            assert_eq!(event["turn_id"].as_u64(), Some(77_010));
        }
        assert_eq!(
            events[0]["axis_a"]["guarded_save"].as_str(),
            Some("missing")
        );
        assert_eq!(events[0]["axis_a"]["old"].as_str(), Some("end"));
        assert_eq!(
            events[0]["axis_a"]["new"].as_str(),
            Some("continue_rowless")
        );
        assert_eq!(events[1]["axis_a"]["ticks"].as_u64(), Some(2));
        assert_eq!(events[1]["axis_a"]["diff"].as_u64(), Some(1));
        assert_eq!(events[1]["axis_a"]["old_ended_lifecycle"].as_u64(), Some(1));
        assert_eq!(events[1]["axis_a"]["new_ended_lifecycle"].as_u64(), Some(0));
        assert_eq!(events[1]["axis_a"]["new_stricter"].as_u64(), Some(0));
        assert_eq!(
            events[2]["axis_a"]["lease_range_shape"].as_str(),
            Some("advancing")
        );
        assert!(
            events[2]["axis_a"].get("rowless_continuation").is_none(),
            "E4-6: loop exit does not emit a field production can only ever set false"
        );
    }

    /// The `AuthorityLost` population is exactly the one that never reaches
    /// post-loop finalize, so a buffer that only flushed there would lose the
    /// evidence this slice exists to collect. The next turn on the channel must
    /// publish its predecessor.
    #[test]
    fn a_turn_that_never_reaches_loop_exit_is_flushed_by_its_successor() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel = 4_259_212;
        let stranded = state(channel, 77_020);
        let successor = state(channel, 77_021);

        open_turn(OBSERVING, &shared, &stranded, GuardedSaveOutcome::Saved);
        record_stream_loop_gate(&stranded, GuardedSaveOutcome::IdentityMismatch, true, true);
        assert!(
            events_for(temp.path(), channel).is_empty(),
            "an in-flight turn must not be written tick by tick",
        );

        open_turn(OBSERVING, &shared, &successor, GuardedSaveOutcome::Saved);
        let events = events_for(temp.path(), channel);
        assert_eq!(
            events
                .iter()
                .map(|event| event["site"].as_str().unwrap_or_default())
                .collect::<Vec<_>>(),
            ["bridge_entry", "stream_loop"],
            "the stranded turn is published without a loop-exit event, not dropped",
        );
        assert_eq!(events[0]["turn_id"].as_u64(), Some(77_020));
        assert_eq!(events[1]["axis_a"]["old_ended_lifecycle"].as_u64(), Some(1));
        for event in &events {
            assert_eq!(
                event["publish_reason"].as_str(),
                Some("evicted"),
                "the canon says this turn only reached the log because a successor came",
            );
        }

        // The successor owns the channel now, so the stranded turn's own late
        // loop-exit call must not resurrect or re-tally it.
        record_loop_exit(&stranded, Some(4_096));
        assert_eq!(events_for(temp.path(), channel).len(), 2);
        record_loop_exit(&successor, Some(4_096));
        assert_eq!(events_for(temp.path(), channel).len(), 4);
    }

    /// legB P1-1's counterexample, which the `(user_msg_id, started_at)` matcher
    /// could not survive: two TUI-direct turns on one channel, both
    /// `user_msg_id == 0`, both starting inside the same second, differing only on
    /// `turn_start_offset`. Under the two-field matcher the predecessor's late tick
    /// and late loop exit both claimed the successor's buffer — inflating the
    /// successor's tally and then publishing its record with the predecessor's
    /// range shape, which also destroyed the successor's own loop-exit event.
    #[test]
    fn a_same_second_tui_direct_successor_is_not_confused_with_its_predecessor() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel = 4_259_216;
        let predecessor = tui_direct_state(channel, 4_096);
        let successor = tui_direct_state(channel, 8_192);
        assert_eq!(
            (predecessor.user_msg_id, predecessor.started_at.as_str()),
            (successor.user_msg_id, successor.started_at.as_str()),
            "the counterexample requires the old matcher's two fields to collide",
        );
        let (old_turn, new_turn) = (
            predecessor.effective_finalizer_turn_id(),
            successor.effective_finalizer_turn_id(),
        );
        assert_ne!(old_turn, new_turn);

        open_turn(OBSERVING, &shared, &predecessor, GuardedSaveOutcome::Saved);
        record_stream_loop_gate(
            &predecessor,
            GuardedSaveOutcome::IdentityMismatch,
            true,
            true,
        );
        open_turn(OBSERVING, &shared, &successor, GuardedSaveOutcome::Saved);
        // Everything after this line is the predecessor arriving late.
        record_stream_loop_gate(
            &predecessor,
            GuardedSaveOutcome::IdentityMismatch,
            true,
            true,
        );
        record_loop_exit(&predecessor, Some(16_384));
        // Two ticks, so the successor's own tally is a value the predecessor's
        // single late tick cannot coincidentally produce: under the two-field
        // matcher the late tick lands here and the late loop exit publishes the
        // buffer with one tick, which a `ticks == 1` assertion could not tell from
        // the correct outcome (legA r2c P2-2).
        record_stream_loop_gate(&successor, GuardedSaveOutcome::Saved, true, true);
        record_stream_loop_gate(&successor, GuardedSaveOutcome::Saved, true, true);
        record_loop_exit(&successor, Some(16_384));

        let events = events_for(temp.path(), channel);
        let sites_of = |turn_id: u64| -> Vec<String> {
            events
                .iter()
                .filter(|event| event["turn_id"].as_u64() == Some(turn_id))
                .map(|event| event["site"].as_str().unwrap_or_default().to_string())
                .collect()
        };
        assert_eq!(
            sites_of(old_turn),
            ["bridge_entry", "stream_loop"],
            "the evicted predecessor keeps its own two events and gains no loop exit",
        );
        assert_eq!(
            sites_of(new_turn),
            ["bridge_entry", "stream_loop", "loop_exit"],
            "the successor's own record survives its predecessor's late calls",
        );
        let stream_of = |turn_id: u64| -> serde_json::Value {
            events
                .iter()
                .find(|event| {
                    event["turn_id"].as_u64() == Some(turn_id)
                        && event["site"].as_str() == Some("stream_loop")
                })
                .expect("stream event")["axis_a"]
                .clone()
        };
        assert_eq!(stream_of(old_turn)["ticks"].as_u64(), Some(1));
        assert_eq!(stream_of(old_turn)["old_ended_lifecycle"].as_u64(), Some(1));
        assert_eq!(
            stream_of(new_turn)["ticks"].as_u64(),
            Some(2),
            "the successor's record holds its own two ticks and not the predecessor's",
        );
        assert_eq!(stream_of(new_turn)["old_ended_lifecycle"].as_u64(), Some(0));
    }

    /// What the three-axis matcher buys, stated as what it actually pins: a turn
    /// whose `tmux_session_name` is re-based mid-turn by a runtime handoff still
    /// matches its own buffer, still tallies, and still flushes at its own loop
    /// exit. A strict four-axis comparison would strand every turn that spawns a
    /// runtime — the exact failure the stranded counters exist to make visible,
    /// manufactured by the fix for the identity finding. There is no adopt step to
    /// verify: the fourth axis is not held here at all, because nothing in this
    /// module could read it (legA r2c P2-1).
    #[test]
    fn a_turn_whose_tmux_session_is_rebased_mid_turn_is_not_stranded() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel = 4_259_217;
        let mut turn = state(channel, 77_040);
        let resident_before = observation_report().resident_buffers;

        open_turn(OBSERVING, &shared, &turn, GuardedSaveOutcome::Saved);
        assert_eq!(
            observation_report().resident_buffers,
            resident_before + 1,
            "an open buffer is an unpublished turn and the triage block says so",
        );
        turn.tmux_session_name = Some("AgentDesk-authority-observation-handoff".to_string());
        record_stream_loop_gate(&turn, GuardedSaveOutcome::Saved, true, true);
        record_loop_exit(&turn, Some(4_096));

        let events = events_for(temp.path(), channel);
        assert_eq!(
            events
                .iter()
                .map(|event| event["site"].as_str().unwrap_or_default())
                .collect::<Vec<_>>(),
            ["bridge_entry", "stream_loop", "loop_exit"],
            "the renamed turn is still tallied and still flushes at its own loop exit",
        );
        assert_eq!(
            observation_report().resident_buffers,
            resident_before,
            "publishing releases the buffer",
        );
    }

    /// An unwritable runtime root still must not reach the turn that produced the
    /// observation — but the silence has to be countable, or a window of lost
    /// records is indistinguishable from a quiet one.
    #[test]
    fn a_sink_that_cannot_write_counts_what_it_dropped() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let blocked = temp.path().join("root-is-a-file");
        fs::write(&blocked, b"not a directory").expect("occupy the runtime root path");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", &blocked);
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let turn = state(4_259_218, 77_050);
        let before = observation_report().sink_dropped_records;
        let completion_before = observation_report().completion_sink_dropped_records;

        open_turn(OBSERVING, &shared, &turn, GuardedSaveOutcome::Saved);
        record_stream_loop_gate(&turn, GuardedSaveOutcome::Saved, true, true);
        record_loop_exit(&turn, Some(4_096));
        record_completion_scope_at(
            OBSERVING,
            CompletionScopeRecord {
                shared: &shared,
                provider: &ProviderKind::Claude,
                turn_id: 77_051,
                channel_id: 4_259_218,
                site: "completion_r0",
                turn_source: "managed",
                scope: "foreign",
                scope_reason: "foreign_episode",
            },
        );

        let report = observation_report();
        assert_eq!(
            report.sink_dropped_records - before,
            3,
            "only the three axis-A lifecycle records increment the canon drop counter",
        );
        assert_eq!(
            report.completion_sink_dropped_records - completion_before,
            1,
            "the completion record increments only its separate drop counter",
        );
        assert!(
            report.turns_recorded > 0,
            "the triage sink still recorded the turn the durable sink lost",
        );
    }

    /// Cohort membership is a per-channel, per-turn-entry question, so a turn
    /// whose entry gate was never observed contributes nothing even if the dial
    /// moves under it mid-turn.
    #[test]
    fn a_turn_whose_entry_was_not_observed_tallies_nothing() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let observed = state(4_259_213, 77_030);
        let unobserved = state(4_259_214, 77_031);

        open_turn(OBSERVING, &shared, &observed, GuardedSaveOutcome::Saved);
        record_stream_loop_gate(&unobserved, GuardedSaveOutcome::Missing, true, true);
        record_loop_exit(&unobserved, Some(4_096));
        record_loop_exit(&observed, None);

        assert!(events_for(temp.path(), 4_259_214).is_empty());
        let events = events_for(temp.path(), 4_259_213);
        assert_eq!(events.len(), 2, "entry + loop exit, and no stream event");
        assert_eq!(events[1]["site"].as_str(), Some("loop_exit"));
        assert_eq!(
            events[1]["axis_a"]["lease_range_shape"].as_str(),
            Some("absent")
        );
    }

    /// The triage block is bounded per channel and reports the counters the
    /// promotion runbook watches.
    #[test]
    fn the_health_ring_is_bounded_and_reports_the_alarm_counter() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel = 4_259_215;
        let before = observation_report();

        for index in 0..(TRIAGE_RING_DEPTH as u64 + 4) {
            let turn = state(channel, 77_100 + index);
            open_turn(OBSERVING, &shared, &turn, GuardedSaveOutcome::Missing);
            record_loop_exit(&turn, Some(4_096));
        }

        let report = observation_report();
        let ring = &report.channels[&channel.to_string()];
        assert_eq!(ring.len(), TRIAGE_RING_DEPTH, "the ring is bounded");
        assert_eq!(
            ring.back().expect("newest entry").turn_id,
            77_100 + TRIAGE_RING_DEPTH as u64 + 3,
            "the ring keeps the newest turns"
        );
        assert_eq!(
            report.turns_recorded - before.turns_recorded,
            TRIAGE_RING_DEPTH as u64 + 4
        );
        assert_eq!(
            report.rowless_continuations - before.rowless_continuations,
            TRIAGE_RING_DEPTH as u64 + 4,
            "every one of these turns is a rowless continuation under the new gate"
        );
        assert_eq!(
            report.new_stricter_verdicts, before.new_stricter_verdicts,
            "the monotone-relaxing alarm counter must never move"
        );
        assert!(serde_json::to_value(&report).is_ok_and(|json| json["new_stricter_verdicts"] == 0));
    }

    /// The ring depth bounded each channel's entry; nothing bounded the number of
    /// entries, so `/api/health/detail` grew for the life of an `Observe`/100
    /// process and never shrank. The map is now capped and evicts the channel that
    /// recorded least recently.
    #[test]
    fn the_health_block_bounds_how_many_channels_it_keeps() {
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channels: Vec<u64> = (0..(TRIAGE_CHANNEL_CAP as u64 + 3))
            .map(|index| 4_300_000 + index)
            .collect();

        for (index, channel) in channels.iter().enumerate() {
            let turn = state(*channel, 77_200 + index as u64);
            open_turn(OBSERVING, &shared, &turn, GuardedSaveOutcome::Saved);
            record_loop_exit(&turn, Some(4_096));
        }

        let report = observation_report();
        assert!(
            report.channels.len() <= TRIAGE_CHANNEL_CAP,
            "the channel map is bounded, not just each ring inside it"
        );
        for channel in &channels[..3] {
            assert!(
                !report.channels.contains_key(&channel.to_string()),
                "the least recently recorded channels are evicted first"
            );
        }
        for channel in &channels[channels.len() - 3..] {
            assert!(
                report.channels.contains_key(&channel.to_string()),
                "the most recently recorded channels are kept"
            );
        }
    }
}
