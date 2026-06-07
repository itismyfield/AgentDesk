//! #3154 — durable pending synthetic-turn-start records + per-channel
//! serialization for the TUI-direct relay.
//!
//! ## Why this exists (the root cause it fixes)
//! A wakeup/loop (`ScheduleWakeup`, classified slash-command-control) turn can
//! start BEFORE the prior user turn's relay has drained. The synthetic claim
//! used to run INLINE inside the single per-provider observer loop
//! ([`super::tui_prompt_relay::relay_observed_prompt`]): it seeded
//! `turn_start_offset` from the prior relay cursor while the prior tail was
//! still undrained, colliding `response_sent_offset` bookkeeping
//! (`response_sent_offset_monotonic` violations), duplicate relay, or a
//! wrong-turn terminal commit. No claim-path offset manipulation can fix it —
//! the fix is TEMPORAL: defer the synthetic start until the prior turn
//! genuinely finalizes, detached from the shared observer loop.
//!
//! ## Mechanism (LOCKED design — Candidate 1, approach A)
//! 1. Persist a durable [`TuiDirectPendingStart`] under a new runtime_store
//!    root the instant the anchor/lease are created (BEFORE any wait).
//! 2. [`relay_observed_prompt`] returns to the observer loop immediately and a
//!    DETACHED per-`(provider, channel_id)` worker performs the claim — so a
//!    long wait on channel A never starves channel B.
//! 3. The worker serializes per channel ([`channel_lock`]); multiple pending
//!    prompts on the same channel drain FIFO.
//! 4. The worker polls [`prior_turn_finalized`] (~100ms) bounded by an 8s
//!    backstop, then claims with a FRESH `turn_start_offset = relay_last_offset()`
//!    (post-drain == EOF) and `response_sent_offset = 0`.
//! 5. While a pending start exists for a channel, the watcher no-inflight
//!    suppression keeps bytes buffered ([`pending_synthetic_start_present`]) and
//!    the idle queue is blocked for that channel.
//! 6. The record is deleted only AFTER the inflight save succeeds. A crash
//!    between save and delete is healed idempotently (the claim refreshes the
//!    matching anchor's existing inflight); a crash before save resumes waiting.
//!    The provider prompt is NEVER resubmitted.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::SharedData;

/// Conservative poll interval for the wait predicate.
pub(super) const PENDING_START_POLL: Duration = Duration::from_millis(100);

/// Backstop matching `turn_finalizer::GATE_BACKSTOP` (8s). After this, the
/// worker claims anyway rather than leak a pending record forever — the prior
/// turn is presumed wedged and a fresh EOF-seeded claim is still safer than
/// resurrecting the inline-overwrite bug.
pub(super) const PENDING_START_BACKSTOP: Duration = Duration::from_secs(8);

/// Lifecycle state of a durable pending-start record. Kept tiny and
/// string-serialized so a forward/backward dcserver swap reads it tolerantly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(super) enum PendingStartState {
    /// Persisted; worker has not yet completed the claim.
    #[default]
    Waiting,
}

/// Durable record describing a TUI-direct synthetic turn-start that must be
/// claimed only AFTER the prior turn on the same channel finalizes.
///
/// All fields are primitives so the JSON survives a dcserver version swap; the
/// lease is rehydrated from these fields on restart
/// (`record_external_input_turn_lease`), never from a serialized lease struct.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct TuiDirectPendingStart {
    pub provider: String,
    pub channel_id: u64,
    pub tmux_session_name: String,
    pub prompt_text: String,
    pub anchor_message_id: u64,
    /// Lease owner (`ExternalInputRelayOwner::as_str`) captured at persist time.
    pub lease_relay_owner: String,
    /// Lease runtime kind (`RuntimeHandoffKind::as_str`), if known.
    pub lease_runtime_kind: Option<String>,
    pub lease_turn_id: Option<String>,
    pub lease_session_key: Option<String>,
    /// Restart generation at persist time (the `turn_finalizer::TurnKey`
    /// generation the claim registers under).
    pub generation: u64,
    pub created_at_ms: u64,
    pub observed_at_ms: u64,
    #[serde(default)]
    pub state: PendingStartState,
    #[serde(default)]
    pub attempt_count: u32,
}

impl TuiDirectPendingStart {
    /// Stable filename key for the record (one record per anchor; a channel may
    /// briefly hold several queued anchors which all drain FIFO under the lock).
    fn file_stem(&self) -> String {
        format!(
            "{}_{}_{}",
            self.provider, self.channel_id, self.anchor_message_id
        )
    }
}

// ---------------------------------------------------------------------------
// Per-(provider, channel) serialization lock table
// ---------------------------------------------------------------------------

/// Module-static lock table (smaller surface than threading a field through
/// `SharedData`). One `tokio::Mutex` per `(provider, channel_id)`; the worker
/// holds it for the whole wait+claim so same-channel pending prompts serialize
/// FIFO while different channels run fully in parallel.
static CHANNEL_LOCKS: LazyLock<Mutex<HashMap<(String, u64), Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn channel_lock(provider: &str, channel_id: u64) -> Arc<tokio::sync::Mutex<()>> {
    let mut table = CHANNEL_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    table
        .entry((provider.to_string(), channel_id))
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

// ---------------------------------------------------------------------------
// In-memory presence index (cheap gate probe — avoids a filesystem scan on the
// hot watcher / idle-queue paths)
// ---------------------------------------------------------------------------

static PRESENT: LazyLock<Mutex<HashMap<(String, u64), u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn mark_present(provider: &str, channel_id: u64) {
    let mut map = PRESENT.lock().unwrap_or_else(|e| e.into_inner());
    *map.entry((provider.to_string(), channel_id)).or_insert(0) += 1;
}

fn mark_absent(provider: &str, channel_id: u64) {
    let mut map = PRESENT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(count) = map.get_mut(&(provider.to_string(), channel_id)) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            map.remove(&(provider.to_string(), channel_id));
        }
    }
}

/// GATE probe consulted by the watcher no-inflight suppression and the idle
/// queue: is a synthetic turn-start pending (record persisted, inflight not yet
/// saved) for this provider/channel? While true, the watcher must LEAVE bytes
/// buffered and the idle queue must not kick normal work for this channel.
///
/// Cheap (in-memory) so it is safe to call inline on the hot paths. The durable
/// record is the source of truth on restart; this index is rebuilt by
/// `restore_pending_starts`.
pub(super) fn pending_synthetic_start_present(provider: &str, channel_id: u64) -> bool {
    let map = PRESENT.lock().unwrap_or_else(|e| e.into_inner());
    map.get(&(provider.to_string(), channel_id))
        .copied()
        .unwrap_or(0)
        > 0
}

/// Re-mark a record present during restart restore. [`load_all`] reads the
/// durable store but does not touch the in-memory index; this restores the gate
/// state before the respawned worker's first poll. The worker's terminal
/// [`delete`] balances it.
pub(super) fn mark_present_on_restore(provider: &str, channel_id: u64) {
    mark_present(provider, channel_id);
}

#[cfg(test)]
pub(super) fn reset_present_for_tests() {
    PRESENT.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

// ---------------------------------------------------------------------------
// Durable store
// ---------------------------------------------------------------------------

fn root() -> Option<std::path::PathBuf> {
    super::runtime_store::tui_direct_pending_start_root()
}

/// Persist (or update) a pending-start record and mark it present in the
/// in-memory index. Called BEFORE any wait, immediately after the anchor/lease
/// are created.
pub(super) fn persist(record: &TuiDirectPendingStart) -> Result<(), String> {
    mark_present(&record.provider, record.channel_id);
    let Some(root) = root() else {
        // No runtime root (tests / unconfigured): the in-memory presence index
        // still gates the watcher / idle queue for this process lifetime.
        return Ok(());
    };
    let path = root.join(format!("{}.json", record.file_stem()));
    let data = serde_json::to_string_pretty(record).map_err(|e| e.to_string())?;
    super::runtime_store::critical_atomic_write(
        &path,
        &data,
        super::runtime_store::AtomicWriteContext::new("tui_direct_pending_start")
            .provider(&record.provider)
            .channel_id(record.channel_id),
    )
}

/// Delete a pending-start record AFTER the inflight save succeeds (or when the
/// worker gives up). Idempotent.
pub(super) fn delete(record: &TuiDirectPendingStart) {
    mark_absent(&record.provider, record.channel_id);
    if let Some(root) = root() {
        let path = root.join(format!("{}.json", record.file_stem()));
        let _ = std::fs::remove_file(path);
    }
}

/// Load all durable pending-start records (restart restore).
pub(super) fn load_all() -> Vec<TuiDirectPendingStart> {
    let Some(root) = root() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(record) = serde_json::from_str::<TuiDirectPendingStart>(&text)
        {
            out.push(record);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Pure decision functions (truth-table tested — no I/O, no clock)
// ---------------------------------------------------------------------------

/// Inputs to [`prior_turn_finalized`]. Captured by the worker each poll from
/// inflight/mailbox/runtime-binding state so the decision is pure and testable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PriorTurnView {
    /// An inflight row exists for this provider/channel.
    pub inflight_present: bool,
    /// The present inflight (if any) is THIS pending start's own anchor — a
    /// crash-after-save-before-delete restore, idempotently adoptable.
    pub inflight_is_own_anchor: bool,
    /// The mailbox has an active blocking (non-background) turn.
    pub mailbox_blocking_turn_present: bool,
    /// The mailbox's active turn (if any) is THIS pending start's own anchor.
    pub mailbox_turn_is_own_anchor: bool,
    /// A runtime binding resolves for the tmux session (needed to seed a fresh
    /// EOF offset at claim time).
    pub runtime_binding_present: bool,
}

/// The prior turn is finalized (relay drained) iff:
/// (a) there is no prior inflight for this provider/channel, OR the existing
///     inflight is THIS anchor (idempotent restore); AND
/// (b) the mailbox has no active blocking turn, OR it is THIS anchor; AND
/// (c) a runtime binding exists (so the claim can seed a fresh EOF offset).
///
/// "Prior" is the discriminator: an inflight/mailbox-turn that is OUR OWN anchor
/// is not a blocker — it is the partially-applied result of THIS pending start
/// (e.g. crash-recovery) and is adopted idempotently.
pub(super) fn prior_turn_finalized(view: PriorTurnView) -> bool {
    let inflight_ok = !view.inflight_present || view.inflight_is_own_anchor;
    let mailbox_ok = !view.mailbox_blocking_turn_present || view.mailbox_turn_is_own_anchor;
    inflight_ok && mailbox_ok && view.runtime_binding_present
}

/// Decide whether [`relay_observed_prompt`] must DEFER the synthetic turn-start
/// off the observer loop (persist a record + spawn the worker) instead of
/// claiming inline.
///
/// Defer when the prior turn is NOT finalized — i.e. claiming inline now would
/// reproduce the offset collision. When the prior turn is already finalized the
/// inline claim is safe and the deferral machinery is skipped entirely (keeps
/// the common no-interleave path on its existing fast path).
pub(super) fn should_defer_synthetic_turn_start(prior: PriorTurnView) -> bool {
    !prior_turn_finalized(prior)
}

// ---------------------------------------------------------------------------
// Detached worker
// ---------------------------------------------------------------------------

/// The claim action the worker runs once the prior turn is finalized. Provided
/// by [`super::tui_prompt_relay`] (where `claim_tui_direct_synthetic_turn` is
/// private). Returns `true` when an inflight was saved (claimed), `false`
/// otherwise (the worker still deletes the record to avoid a leak).
pub(super) type ClaimFn = Box<
    dyn for<'a> Fn(
            &'a Arc<SharedData>,
            &'a TuiDirectPendingStart,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>
        + Send
        + Sync,
>;

/// Build the per-poll [`PriorTurnView`]. Provided by [`super::tui_prompt_relay`]
/// (it owns inflight/mailbox/runtime-binding access). Returns `None` when the
/// view cannot be computed yet (e.g. mailbox unavailable) — treated as "not
/// finalized" so the worker keeps waiting.
pub(super) type ViewFn = Box<
    dyn for<'a> Fn(
            &'a Arc<SharedData>,
            &'a TuiDirectPendingStart,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Option<PriorTurnView>> + Send + 'a>,
        > + Send
        + Sync,
>;

/// Spawn the DETACHED per-channel worker. Acquires the channel lock (FIFO
/// serialization), polls the wait predicate until the prior turn finalizes (or
/// the 8s backstop fires), runs the claim, and deletes the record. Returns
/// immediately so the observer loop is never blocked.
pub(super) fn spawn_worker(
    shared: Arc<SharedData>,
    record: TuiDirectPendingStart,
    view_fn: ViewFn,
    claim_fn: ClaimFn,
) {
    super::task_supervisor::spawn_observed("tui_direct_pending_start_worker", async move {
        run_worker(shared, record, view_fn, claim_fn).await;
    });
}

async fn run_worker(
    shared: Arc<SharedData>,
    record: TuiDirectPendingStart,
    view_fn: ViewFn,
    claim_fn: ClaimFn,
) {
    let lock = channel_lock(&record.provider, record.channel_id);
    let _guard = lock.lock().await;

    let start = tokio::time::Instant::now();
    loop {
        let finalized = match view_fn(&shared, &record).await {
            Some(view) => prior_turn_finalized(view),
            None => false,
        };
        if finalized {
            break;
        }
        if start.elapsed() >= PENDING_START_BACKSTOP {
            tracing::warn!(
                provider = %record.provider,
                channel_id = record.channel_id,
                tmux_session_name = %record.tmux_session_name,
                anchor_message_id = record.anchor_message_id,
                backstop_ms = PENDING_START_BACKSTOP.as_millis(),
                "tui_direct_pending_start: prior turn did not finalize within backstop; claiming anyway with fresh EOF offset"
            );
            break;
        }
        tokio::time::sleep(PENDING_START_POLL).await;
    }

    let claimed = claim_fn(&shared, &record).await;
    tracing::info!(
        provider = %record.provider,
        channel_id = record.channel_id,
        tmux_session_name = %record.tmux_session_name,
        anchor_message_id = record.anchor_message_id,
        waited_ms = start.elapsed().as_millis(),
        claimed,
        "tui_direct_pending_start: deferred synthetic turn-start claimed after prior turn finalized"
    );
    // Delete AFTER the claim (whether it saved an inflight or not) so we never
    // leak a record. A crash between inflight-save and this delete is healed on
    // restart: the worker re-runs, the claim adopts the matching anchor's
    // existing inflight idempotently, then deletes.
    delete(&record);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_view() -> PriorTurnView {
        PriorTurnView {
            inflight_present: false,
            inflight_is_own_anchor: false,
            mailbox_blocking_turn_present: false,
            mailbox_turn_is_own_anchor: false,
            runtime_binding_present: true,
        }
    }

    #[test]
    fn finalized_when_no_prior_inflight_and_no_blocking_turn_and_binding() {
        assert!(prior_turn_finalized(base_view()));
        assert!(!should_defer_synthetic_turn_start(base_view()));
    }

    #[test]
    fn not_finalized_while_prior_inflight_undrained() {
        let view = PriorTurnView {
            inflight_present: true,
            ..base_view()
        };
        assert!(!prior_turn_finalized(view));
        assert!(
            should_defer_synthetic_turn_start(view),
            "an undrained prior inflight (the interleave bug) MUST defer"
        );
    }

    #[test]
    fn own_anchor_inflight_does_not_block_idempotent_restore() {
        let view = PriorTurnView {
            inflight_present: true,
            inflight_is_own_anchor: true,
            ..base_view()
        };
        assert!(
            prior_turn_finalized(view),
            "a crash-restored inflight for OUR OWN anchor is adopted, not waited on"
        );
    }

    #[test]
    fn not_finalized_while_mailbox_blocking_turn_present() {
        let view = PriorTurnView {
            mailbox_blocking_turn_present: true,
            ..base_view()
        };
        assert!(!prior_turn_finalized(view));
        assert!(should_defer_synthetic_turn_start(view));
    }

    #[test]
    fn own_anchor_mailbox_turn_does_not_block() {
        let view = PriorTurnView {
            mailbox_blocking_turn_present: true,
            mailbox_turn_is_own_anchor: true,
            ..base_view()
        };
        assert!(prior_turn_finalized(view));
    }

    #[test]
    fn not_finalized_without_runtime_binding() {
        let view = PriorTurnView {
            runtime_binding_present: false,
            ..base_view()
        };
        assert!(
            !prior_turn_finalized(view),
            "no runtime binding → cannot seed a fresh EOF offset → keep waiting"
        );
    }

    #[test]
    fn presence_index_marks_and_clears() {
        reset_present_for_tests();
        let provider = "claude";
        let channel = 777u64;
        assert!(!pending_synthetic_start_present(provider, channel));
        mark_present(provider, channel);
        assert!(pending_synthetic_start_present(provider, channel));
        mark_present(provider, channel);
        mark_absent(provider, channel);
        assert!(
            pending_synthetic_start_present(provider, channel),
            "two pending starts on a channel: still present after one clears"
        );
        mark_absent(provider, channel);
        assert!(!pending_synthetic_start_present(provider, channel));
        reset_present_for_tests();
    }

    #[test]
    fn record_roundtrips_through_json() {
        let record = TuiDirectPendingStart {
            provider: "claude".to_string(),
            channel_id: 42,
            tmux_session_name: "tmux-abc".to_string(),
            prompt_text: "/loop do the thing".to_string(),
            anchor_message_id: 9001,
            lease_relay_owner: "bridge_adapter".to_string(),
            lease_runtime_kind: Some("claude_tui".to_string()),
            lease_turn_id: Some("turn-1".to_string()),
            lease_session_key: Some("sess-1".to_string()),
            generation: 7,
            created_at_ms: 1234,
            observed_at_ms: 1230,
            state: PendingStartState::Waiting,
            attempt_count: 0,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: TuiDirectPendingStart = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
    }

    fn record(provider: &str, channel_id: u64, anchor: u64) -> TuiDirectPendingStart {
        TuiDirectPendingStart {
            provider: provider.to_string(),
            channel_id,
            tmux_session_name: format!("tmux-{channel_id}"),
            prompt_text: "/loop tick".to_string(),
            anchor_message_id: anchor,
            lease_relay_owner: "bridge_adapter".to_string(),
            lease_runtime_kind: Some("claude_tui".to_string()),
            lease_turn_id: None,
            lease_session_key: None,
            generation: 0,
            created_at_ms: 0,
            observed_at_ms: 0,
            state: PendingStartState::Waiting,
            attempt_count: 0,
        }
    }

    /// #3154 interleave integration test (design point: tokio interleave with
    /// `tokio::time::pause()`):
    ///   - channel A's wakeup DEFERS while a seeded turn1 inflight is undrained;
    ///   - channel B relays FIRST (no cross-channel starvation: B's worker is on
    ///     a different channel lock and finishes immediately);
    ///   - A claims ONLY after turn1's inflight clears, and the EOF offset the
    ///     claim reads at THAT moment is recorded (asserting the claim is seeded
    ///     post-drain, never from the stale prior cursor).
    #[tokio::test(start_paused = true)]
    async fn channel_a_defers_until_prior_clears_while_channel_b_does_not_starve() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

        reset_present_for_tests();
        let shared = super::super::make_shared_data_for_tests();

        // ---- Channel A: prior turn1 inflight is UNDRAINED at first. ----
        let a_prior_undrained = Arc::new(AtomicBool::new(true));
        // The "EOF offset" the claim would read: starts at the stale prior
        // cursor (100) and only advances to the post-drain frontier (250) once
        // turn1's inflight clears. The claim must capture 250, not 100.
        let a_eof_when_claimed = Arc::new(AtomicU64::new(0));

        let a_undrained_for_view = a_prior_undrained.clone();
        let a_view: ViewFn = Box::new(move |_shared, _record| {
            let undrained = a_undrained_for_view.clone();
            Box::pin(async move {
                Some(PriorTurnView {
                    // turn1 inflight present until drained.
                    inflight_present: undrained.load(Ordering::SeqCst),
                    inflight_is_own_anchor: false,
                    mailbox_blocking_turn_present: false,
                    mailbox_turn_is_own_anchor: false,
                    runtime_binding_present: true,
                })
            })
        });
        let a_undrained_for_claim = a_prior_undrained.clone();
        let a_eof_for_claim = a_eof_when_claimed.clone();
        let a_claim: ClaimFn = Box::new(move |_shared, _record| {
            let undrained = a_undrained_for_claim.clone();
            let eof = a_eof_for_claim.clone();
            Box::pin(async move {
                // The relay cursor is the stale 100 while undrained, EOF 250 once
                // drained. The claim reads it FRESH at claim time.
                let offset = if undrained.load(Ordering::SeqCst) {
                    100
                } else {
                    250
                };
                eof.store(offset, Ordering::SeqCst);
                true
            })
        });

        let rec_a = record("claude", 1, 11);
        persist(&rec_a).unwrap();
        assert!(
            pending_synthetic_start_present("claude", 1),
            "A's pending start gates the watcher/idle-queue immediately"
        );
        let a_handle = tokio::spawn(run_worker(shared.clone(), rec_a, a_view, a_claim));

        // ---- Channel B: prior turn already finalized → relays immediately. ----
        let b_claimed = Arc::new(AtomicBool::new(false));
        let b_view: ViewFn = Box::new(move |_shared, _record| {
            Box::pin(async move {
                Some(PriorTurnView {
                    inflight_present: false,
                    inflight_is_own_anchor: false,
                    mailbox_blocking_turn_present: false,
                    mailbox_turn_is_own_anchor: false,
                    runtime_binding_present: true,
                })
            })
        });
        let b_claimed_for_claim = b_claimed.clone();
        let b_claim: ClaimFn = Box::new(move |_shared, _record| {
            let claimed = b_claimed_for_claim.clone();
            Box::pin(async move {
                claimed.store(true, Ordering::SeqCst);
                true
            })
        });
        let rec_b = record("claude", 2, 22);
        persist(&rec_b).unwrap();
        let b_handle = tokio::spawn(run_worker(shared.clone(), rec_b, b_view, b_claim));

        // B is on a DIFFERENT channel lock; it must finish without waiting for A.
        b_handle.await.unwrap();
        assert!(
            b_claimed.load(Ordering::SeqCst),
            "channel B must NOT be starved by channel A's deferral (no inline cross-channel wait)"
        );
        assert!(
            a_eof_when_claimed.load(Ordering::SeqCst) == 0,
            "channel A must STILL be waiting (its prior turn1 inflight has not drained)"
        );
        assert!(
            pending_synthetic_start_present("claude", 1),
            "A's pending start still gates while it waits"
        );

        // Now drain turn1 (the prior user turn's relay completes).
        a_prior_undrained.store(false, Ordering::SeqCst);
        // Let the ~100ms poll elapse under paused time.
        tokio::time::advance(PENDING_START_POLL * 2).await;
        a_handle.await.unwrap();

        assert_eq!(
            a_eof_when_claimed.load(Ordering::SeqCst),
            250,
            "channel A claimed ONLY after turn1 drained, seeding the FRESH post-drain EOF (250), \
             never the stale prior cursor (100) — this is what prevents the response_sent_offset \
             collision"
        );
        assert!(
            !pending_synthetic_start_present("claude", 1),
            "A's pending start cleared after the claim (gate releases)"
        );
        reset_present_for_tests();
    }
}
