//! #4278 defense-in-depth: sweep ORPHANED `⏳` TUI-direct bot-anchor reactions.
//!
//! ## Why this exists
//! The normal `⏳ → ✅` completion ([`note_tui_anchor_completed`]) and the
//! #3296 aborted-anchor marker + TTL sweep both reconcile a synthetic turn's
//! `⏳`. But a `/loop` system-injection anchor whose synthetic turn-start
//! ABORTed on a FOREIGN stale inflight keeps its `⏳` pinned to the FOREIGN
//! prior turn's commit: while a live inflight on the same tmux session defers
//! the marker sweep (`inflight_defers_sweep` name-match branch), the `⏳`
//! lingers up to the 1-hour hard cap, and if the foreign phantom never commits
//! it only converges to `⚠`. Any path that left a `⏳` WITHOUT a live turn and
//! WITHOUT a covering marker (a lost in-memory pending-removal across restart,
//! a marker write that never landed, a code path that dropped the marker but
//! not the reaction) orphans the hourglass entirely — the channel fills with
//! "stuck work" cards (#4278, #3164 lineage).
//!
//! This sweep is the universal backstop: it reads the reconciler's OWN durable
//! per-target state (the `⏳` this bot added, with its `token_hash` identity —
//! #4049) and removes a `⏳` whose (provider, channel) has NO live inflight row
//! and NO abort marker covering the exact anchor, once the record has aged past
//! a conservative grace window. It never fights the live lifecycle: an active
//! turn (inflight row present) or a valid marker always defers to the primary
//! reconcilers, and the removal uses the SAME persisted bot identity that added
//! the `⏳` (add≡remove, #3164).

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use crate::services::provider::ProviderKind;

use super::*;

/// #4278: grace window before the orphan-`⏳` defense sweep may remove a
/// persisted TUI-direct bot-anchor hourglass. A record younger than this is
/// skipped so a legitimately slow-starting turn (whose inflight row has not
/// landed yet, or whose completion `⏳`-removal is in flight) is never swept.
/// Comfortably above `ABORT_MARKER_TTL` (600s) so the #3296 marker + drain get
/// first claim on every aborted anchor; the orphan sweep only fires when NO
/// live turn and NO marker remain.
const ORPHAN_TUI_ANCHOR_MIN_AGE_SECS: u64 = 900;

/// #4278 defense-in-depth production entry (called by `placeholder_sweeper`'s
/// periodic loop, AFTER the #3296 marker sweep): sweep orphaned `⏳` TUI-direct
/// bot-anchor reactions.
///
/// Reads the turn-view reconciler's OWN durable per-target state (the `⏳` this
/// bot added, with its `token_hash` identity) and removes a `⏳` only when the
/// (provider, channel) has NO live inflight row AND no abort marker covers the
/// exact anchor, once the record has aged past the grace window. This is the
/// universal backstop for a `⏳` that neither the normal `⏳ → ✅` completion nor
/// the #3296 marker + TTL sweep ever cleared (an aborted synthetic turn whose
/// FOREIGN-pinned marker deferred to the 1-hour hard cap, a lost in-memory
/// pending-removal across restart, a marker that never landed). Conservative by
/// construction — an active turn or a valid marker always defers to the primary
/// reconcilers; removal uses the persisted `@me` that added the `⏳` (#3164).
pub(in crate::services::discord) async fn sweep_orphan_tui_anchor_reactions(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) -> usize {
    use crate::services::discord::{inflight, tui_direct_abort_marker, tui_direct_pending_start};
    // codex r1 #2: pass-local marker cache — ONE `load_all` store read per
    // sweep pass feeds every candidate's verdict-time marker probe (the prior
    // per-candidate `load_for_channel` re-read the whole store O(P×M) times).
    let marker_anchors: HashSet<(u64, u64)> = tui_direct_abort_marker::load_all()
        .into_iter()
        .filter(|marker| marker.provider.eq_ignore_ascii_case(provider.as_str()))
        .map(|marker| (marker.channel_id, marker.anchor_message_id))
        .collect();
    let has_live_inflight =
        |channel_id: u64| inflight::load_inflight_state_read_only(provider, channel_id).is_some();
    let has_valid_marker = |channel_id: u64, anchor_message_id: u64| {
        marker_anchors.contains(&(channel_id, anchor_message_id))
    };
    // codex r1 #1: removal-instant re-verification, evaluated FRESH inside the
    // reconciler's target_lock right before the `⏳` removal (never from the
    // pass cache — correctness beats cost here, and it only runs for actual
    // removal candidates). Any of the three holds landing after the verdict
    // aborts the removal: a live inflight row, an abort/deferred-claim marker
    // covering the exact anchor, or a durable pending-start pinned to the
    // exact anchor (its deferring worker owns this ⏳'s lifecycle).
    let holds_before_removal = |channel_id: u64, anchor_message_id: u64| {
        inflight::load_inflight_state_read_only(provider, channel_id).is_some()
            || tui_direct_abort_marker::load_for_channel(provider.as_str(), channel_id)
                .iter()
                .any(|marker| marker.anchor_message_id == anchor_message_id)
            || tui_direct_pending_start::load_all().iter().any(|record| {
                record.provider.eq_ignore_ascii_case(provider.as_str())
                    && record.channel_id == channel_id
                    && record.anchor_message_id == anchor_message_id
            })
    };
    sweep_orphan_tui_anchors_with_probes(
        &shared.turn_view_reconciler,
        shared,
        SystemTime::now(),
        Duration::from_secs(ORPHAN_TUI_ANCHOR_MIN_AGE_SECS),
        &has_live_inflight,
        &has_valid_marker,
        &holds_before_removal,
        "orphan_tui_anchor_sweep",
    )
    .await
}

/// A persisted `tui_direct_bot_anchor` target currently in the `⏳` (Pending)
/// state that MAY be an orphan. The caller (`placeholder_sweeper`) supplies the
/// live-inflight / valid-marker signals the orphan verdict needs.
pub(in crate::services::discord) struct PendingAnchorCandidate {
    pub channel_id: u64,
    pub message_id: u64,
    /// Age since the persisted `⏳` record was last written (file mtime proxy).
    /// Records younger than the grace window are skipped so a legitimately
    /// slow-starting turn (whose inflight row has not landed yet) is never
    /// swept.
    pub attached_age: Duration,
}

/// Pure orphan verdict (conservative by design, #4278): a `⏳` is only swept
/// when it has aged past the grace window AND there is no live inflight row for
/// the channel AND no abort marker covers the exact anchor. Any single hold
/// (young record / live turn / valid marker) defers to the primary reconcilers.
pub(in crate::services::discord) fn orphan_tui_anchor_should_clear(
    attached_age: Duration,
    min_age: Duration,
    has_live_inflight: bool,
    has_valid_marker: bool,
) -> bool {
    attached_age >= min_age && !has_live_inflight && !has_valid_marker
}

/// Drive one orphan-`⏳` sweep pass over the reconciler's persisted
/// `tui_direct_bot_anchor` targets. Generic over the live-inflight and
/// valid-marker probes so the production sweep supplies the real inflight /
/// abort-marker store reads while a test drives the verdict directly. Returns
/// the number of orphan `⏳` reactions resolved this pass.
///
/// `has_live_inflight` / `has_valid_marker` feed the CHEAP pass-level verdict
/// (the caller may serve them from a pass-local cache); `holds_before_removal`
/// is the codex-r1 TOCTOU closer — re-evaluated FRESH inside the removal's
/// `target_lock` (see [`TurnViewReconciler::clear_orphan_pending_tui_anchor`]),
/// so a deferred claim / recovery that saved an inflight row, a marker, or a
/// durable pending-start AFTER the verdict aborts the removal instead of
/// stripping a legitimate `⏳`.
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn sweep_orphan_tui_anchors_with_probes(
    reconciler: &TurnViewReconciler,
    shared: &SharedData,
    now: SystemTime,
    min_age: Duration,
    has_live_inflight: &(dyn Fn(u64) -> bool + Send + Sync),
    has_valid_marker: &(dyn Fn(u64, u64) -> bool + Send + Sync),
    holds_before_removal: &(dyn Fn(u64, u64) -> bool + Send + Sync),
    source: &'static str,
) -> usize {
    let mut cleared = 0usize;
    for candidate in reconciler.persisted_pending_tui_anchors(shared, now) {
        if !orphan_tui_anchor_should_clear(
            candidate.attached_age,
            min_age,
            has_live_inflight(candidate.channel_id),
            has_valid_marker(candidate.channel_id, candidate.message_id),
        ) {
            continue;
        }
        if reconciler
            .clear_orphan_pending_tui_anchor(
                shared,
                ChannelId::new(candidate.channel_id),
                MessageId::new(candidate.message_id),
                holds_before_removal,
                source,
            )
            .await
        {
            cleared += 1;
            tracing::warn!(
                channel_id = candidate.channel_id,
                anchor_message_id = candidate.message_id,
                attached_age_secs = candidate.attached_age.as_secs(),
                source,
                "tui_direct: swept orphan ⏳ anchor (no live turn, no reconcile marker) (#4278)"
            );
        }
    }
    cleared
}

impl TurnViewReconciler {
    /// Enumerate persisted `tui_direct_bot_anchor` targets currently in the
    /// `⏳` (Pending) state that belong to THIS runtime's provider. Read-only:
    /// never mutates or deletes on-disk state (a malformed / mismatched file is
    /// simply skipped, left for [`load_persisted_target`]'s own repair path).
    pub(in crate::services::discord) fn persisted_pending_tui_anchors(
        &self,
        shared: &SharedData,
        now: SystemTime,
    ) -> Vec<PendingAnchorCandidate> {
        let Some(root) =
            crate::services::discord::runtime_store::discord_turn_view_reconciler_root()
        else {
            return Vec::new();
        };
        let dir = root.join(TurnViewTargetKind::TuiDirectBotAnchor.as_str());
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut candidates = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(record) = serde_json::from_str::<PersistedTargetState>(&text) else {
                continue;
            };
            if record.version != PERSISTED_STATE_VERSION
                || record.provider != shared.provider.as_str()
                || TurnViewTargetKind::from_str(&record.kind)
                    != Some(TurnViewTargetKind::TuiDirectBotAnchor)
            {
                continue;
            }
            if TurnViewState::from_str(&record.applied) != Some(TurnViewState::Pending) {
                continue;
            }
            let attached_age = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|mtime| now.duration_since(mtime).ok())
                .unwrap_or(Duration::ZERO);
            candidates.push(PendingAnchorCandidate {
                channel_id: record.channel_id,
                message_id: record.message_id,
                attached_age,
            });
        }
        candidates
    }

    /// Remove an orphaned `⏳` from a pending `tui_direct_bot_anchor`, then
    /// delete the persisted target. The removal resolves the PERSISTED bot
    /// identity (`token_hash`, #4049) so the same `@me` that added the `⏳`
    /// removes it (add≡remove, #3164). Fail-open: a transient delivery failure
    /// keeps the record for the next pass; a permanently-gone message
    /// terminates the record. Returns `true` iff this call resolved the orphan.
    ///
    /// TOCTOU guard (codex r1): the pass-level orphan verdict and this removal
    /// are not atomic — a deferred synthetic claim / recovery can save an
    /// inflight row, an abort marker, or a durable pending-start in between.
    /// `holds_before_removal(channel_id, anchor_message_id)` is therefore
    /// re-evaluated FRESH here, inside the `target_lock`, immediately before
    /// the reaction removal; any hold that appeared aborts the sweep for this
    /// pass (record preserved, `⏳` untouched).
    pub(in crate::services::discord) async fn clear_orphan_pending_tui_anchor(
        &self,
        shared: &SharedData,
        channel_id: ChannelId,
        message_id: MessageId,
        holds_before_removal: &(dyn Fn(u64, u64) -> bool + Send + Sync),
        source: &'static str,
    ) -> bool {
        let target = TurnViewTarget::tui_direct_bot_anchor(channel_id, message_id);
        let target_lock = self.target_lock(target);
        let _guard = target_lock.lock().await;
        let current = self
            .targets
            .get(&target)
            .map(|entry| entry.clone())
            .or_else(|| self.load_persisted_target(target, shared, source));
        let Some(current) = current else {
            // A concurrent drain / marker sweep / normal completion already
            // resolved this anchor between enumeration and now — nothing to do.
            return false;
        };
        if current.applied != TurnViewState::Pending {
            // A terminal / queue-marker state is owned by the live lifecycle;
            // the orphan sweep only ever removes a stranded `⏳`.
            self.targets.insert(target, current);
            return false;
        }
        if holds_before_removal(channel_id.get(), message_id.get()) {
            // codex r1 TOCTOU: a hold (live inflight / marker / pending-start)
            // landed between the verdict and this locked removal — the `⏳` is
            // legitimate again; defer to the primary reconcilers.
            tracing::debug!(
                channel_id = channel_id.get(),
                anchor_message_id = message_id.get(),
                source,
                "orphan ⏳ sweep aborted: a hold appeared before removal (#4278 codex r1)"
            );
            self.targets.insert(target, current);
            return false;
        }
        let delivery = self
            .apply_diff(
                shared,
                target,
                TurnViewState::Pending,
                TurnViewState::None,
                &current.identity,
                source,
            )
            .await;
        match delivery {
            TurnViewDelivery::Delivered | TurnViewDelivery::FailedPermanent => {
                self.discard_target_locked(target, source, &target_lock);
                true
            }
            TurnViewDelivery::Failed => {
                // Transient (5xx / rate-limit / transport): keep the record so
                // the next sweep pass retries the removal.
                self.targets.insert(target, current);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Isolate each test from the process-global `AGENTDESK_ROOT_DIR` (mirrors
    /// the reconciler `tests.rs` `ScopedRuntimeRoot`): acquire the crate env
    /// lock for the full scope, point the env at a private temp dir, restore on
    /// drop. The guard is held across `.await` points; sound because these run
    /// on the current-thread `#[tokio::test]` runtime (the future never moves
    /// threads).
    struct ScopedRuntimeRoot {
        _lock: std::sync::MutexGuard<'static, ()>,
        _temp: tempfile::TempDir,
        prev: Option<std::ffi::OsString>,
    }

    impl Drop for ScopedRuntimeRoot {
        fn drop(&mut self) {
            unsafe {
                match self.prev.take() {
                    Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                    None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
                }
            }
        }
    }

    #[must_use]
    fn scoped_runtime_root() -> ScopedRuntimeRoot {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
        let temp = tempfile::tempdir().expect("create temp runtime dir for orphan-sweep test");
        unsafe {
            std::env::set_var(
                "AGENTDESK_ROOT_DIR",
                temp.path().to_str().expect("temp path must be valid utf-8"),
            );
        }
        ScopedRuntimeRoot {
            _lock: lock,
            _temp: temp,
            prev,
        }
    }

    fn anchor_target(channel_id: u64, message_id: u64) -> TurnViewTarget {
        TurnViewTarget::tui_direct_bot_anchor(
            ChannelId::new(channel_id),
            MessageId::new(message_id),
        )
    }

    fn write_pending_anchor(shared: &SharedData, channel_id: u64, message_id: u64) {
        let target = anchor_target(channel_id, message_id);
        let record = PersistedTargetState {
            version: PERSISTED_STATE_VERSION,
            provider: shared.provider.as_str().to_string(),
            kind: target.kind.as_str().to_string(),
            channel_id,
            message_id,
            owner_generation: 7,
            owner_turn_id: "turn-orphan".to_string(),
            applied: TurnViewState::Pending.as_str().to_string(),
            identity_label: target.kind.identity_label().to_string(),
            token_hash: Some(shared.token_hash.clone()),
            start_attempt_id: None,
        };
        let path = TurnViewReconciler::persisted_target_path(target).expect("persisted path");
        let json = serde_json::to_string_pretty(&record).expect("serialize persisted anchor");
        crate::services::discord::runtime_store::atomic_write(&path, &json)
            .expect("write persisted anchor");
    }

    fn persisted_exists(channel_id: u64, message_id: u64) -> bool {
        TurnViewReconciler::persisted_target_path(anchor_target(channel_id, message_id))
            .expect("persisted path")
            .exists()
    }

    fn hourglass_removed(reconciler: &TurnViewReconciler, message_id: u64) -> bool {
        reconciler
            .ops()
            .iter()
            .any(|op| op.target.message_id.get() == message_id && !op.add && op.emoji == '⏳')
    }

    // Pure verdict truth table: the `⏳` is swept ONLY when aged AND no live
    // turn AND no covering marker; any single hold defers to the reconcilers.
    #[test]
    fn orphan_verdict_requires_aged_and_no_live_turn_and_no_marker() {
        let min = Duration::from_secs(600);
        let aged = Duration::from_secs(900);
        let young = Duration::from_secs(60);

        assert!(orphan_tui_anchor_should_clear(aged, min, false, false));
        // young record → skip (a turn may still be starting).
        assert!(!orphan_tui_anchor_should_clear(young, min, false, false));
        // live inflight row → skip (active turn owns the ⏳).
        assert!(!orphan_tui_anchor_should_clear(aged, min, true, false));
        // valid abort marker → skip (marker + drain own the reconcile).
        assert!(!orphan_tui_anchor_should_clear(aged, min, false, true));
    }

    // Enumeration returns only THIS provider's pending anchors; a non-pending
    // (e.g. completed) persisted target and a different-provider record are
    // excluded.
    #[tokio::test]
    async fn enumerate_returns_only_matching_provider_pending_anchors() {
        let _root = scoped_runtime_root();
        let reconciler = TurnViewReconciler::default();
        let shared = crate::services::discord::make_shared_data_for_tests();

        write_pending_anchor(&shared, 5001, 6001);
        // A completed target must not be enumerated (only ⏳ is swept).
        let completed = anchor_target(5002, 6002);
        let completed_record = PersistedTargetState {
            version: PERSISTED_STATE_VERSION,
            provider: shared.provider.as_str().to_string(),
            kind: completed.kind.as_str().to_string(),
            channel_id: 5002,
            message_id: 6002,
            owner_generation: 7,
            owner_turn_id: "turn-done".to_string(),
            applied: TurnViewState::Completed.as_str().to_string(),
            identity_label: completed.kind.identity_label().to_string(),
            token_hash: Some(shared.token_hash.clone()),
            start_attempt_id: None,
        };
        let path = TurnViewReconciler::persisted_target_path(completed).expect("path");
        crate::services::discord::runtime_store::atomic_write(
            &path,
            &serde_json::to_string_pretty(&completed_record).unwrap(),
        )
        .unwrap();

        let candidates = reconciler.persisted_pending_tui_anchors(&shared, SystemTime::now());
        assert_eq!(candidates.len(), 1, "only the pending anchor is enumerated");
        assert_eq!(candidates[0].channel_id, 5001);
        assert_eq!(candidates[0].message_id, 6001);
    }

    // Full sweep: an aged orphan `⏳` with no live turn and no marker is
    // removed (⏳ reaction removed + persisted record deleted).
    #[tokio::test]
    async fn sweep_removes_aged_orphan_hourglass() {
        let _root = scoped_runtime_root();
        let reconciler = TurnViewReconciler::default();
        let shared = crate::services::discord::make_shared_data_for_tests();
        write_pending_anchor(&shared, 7001, 8001);

        let no_inflight = |_: u64| false;
        let no_marker = |_: u64, _: u64| false;
        let no_hold = |_: u64, _: u64| false;
        let cleared = sweep_orphan_tui_anchors_with_probes(
            &reconciler,
            &shared,
            // Force the record to read as aged: sweep "now" far in the future.
            SystemTime::now() + Duration::from_secs(3600),
            Duration::from_secs(600),
            &no_inflight,
            &no_marker,
            &no_hold,
            "test_orphan_sweep",
        )
        .await;

        assert_eq!(cleared, 1, "the orphan ⏳ is swept");
        assert!(hourglass_removed(&reconciler, 8001), "⏳ reaction removed");
        assert!(
            !persisted_exists(7001, 8001),
            "persisted orphan record deleted after removal"
        );
    }

    // Conservative skip: a live inflight row (active turn) OR a valid marker
    // defers the sweep — the `⏳` survives untouched.
    #[tokio::test]
    async fn sweep_skips_when_live_turn_or_marker_present() {
        let _root = scoped_runtime_root();
        let shared = crate::services::discord::make_shared_data_for_tests();

        for (channel, message, live, marker) in
            [(7101_u64, 8101_u64, true, false), (7102, 8102, false, true)]
        {
            let reconciler = TurnViewReconciler::default();
            write_pending_anchor(&shared, channel, message);
            let inflight = move |_: u64| live;
            let has_marker = move |_: u64, _: u64| marker;
            let no_hold = |_: u64, _: u64| false;
            let cleared = sweep_orphan_tui_anchors_with_probes(
                &reconciler,
                &shared,
                SystemTime::now() + Duration::from_secs(3600),
                Duration::from_secs(600),
                &inflight,
                &has_marker,
                &no_hold,
                "test_orphan_sweep",
            )
            .await;
            assert_eq!(cleared, 0, "an active turn / valid marker defers the sweep");
            assert!(
                !hourglass_removed(&reconciler, message),
                "⏳ must not be removed while held"
            );
            assert!(
                persisted_exists(channel, message),
                "persisted record survives a held sweep"
            );
        }
    }

    // Young record skip: a freshly-attached `⏳` (within the grace window) is
    // never swept even with no live turn and no marker.
    #[tokio::test]
    async fn sweep_skips_young_record_within_grace_window() {
        let _root = scoped_runtime_root();
        let reconciler = TurnViewReconciler::default();
        let shared = crate::services::discord::make_shared_data_for_tests();
        write_pending_anchor(&shared, 7201, 8201);

        let no_inflight = |_: u64| false;
        let no_marker = |_: u64, _: u64| false;
        let no_hold = |_: u64, _: u64| false;
        let cleared = sweep_orphan_tui_anchors_with_probes(
            &reconciler,
            &shared,
            SystemTime::now(), // record just written → age ~0 < grace
            Duration::from_secs(600),
            &no_inflight,
            &no_marker,
            &no_hold,
            "test_orphan_sweep",
        )
        .await;

        assert_eq!(cleared, 0, "a young ⏳ is within the grace window");
        assert!(persisted_exists(7201, 8201), "young record survives");
    }

    // codex r1 TOCTOU interleaving: the pass-level verdict sees NO holds (the
    // anchor looks orphaned), but a hold (e.g. a deferred claim saving its
    // abort marker / inflight row) lands BEFORE the locked removal runs. The
    // fresh `holds_before_removal` re-verification inside the target_lock must
    // abort the removal: no `⏳` reaction op, record preserved. RED if the
    // removal trusts the stale verdict.
    #[tokio::test]
    async fn sweep_aborts_when_hold_appears_between_verdict_and_removal() {
        let _root = scoped_runtime_root();
        let reconciler = TurnViewReconciler::default();
        let shared = crate::services::discord::make_shared_data_for_tests();
        write_pending_anchor(&shared, 7301, 8301);

        // Verdict-time probes: no live turn, no marker → candidate passes.
        let no_inflight = |_: u64| false;
        let no_marker = |_: u64, _: u64| false;
        // Removal-time fresh re-probe: the marker/inflight save has now landed.
        let hold_appeared = |channel_id: u64, anchor_message_id: u64| {
            assert_eq!(channel_id, 7301, "re-probe targets the candidate channel");
            assert_eq!(anchor_message_id, 8301, "re-probe targets the exact anchor");
            true
        };
        let cleared = sweep_orphan_tui_anchors_with_probes(
            &reconciler,
            &shared,
            SystemTime::now() + Duration::from_secs(3600),
            Duration::from_secs(600),
            &no_inflight,
            &no_marker,
            &hold_appeared,
            "test_orphan_sweep",
        )
        .await;

        assert_eq!(
            cleared, 0,
            "a hold appearing after the verdict must abort the removal"
        );
        assert!(
            !hourglass_removed(&reconciler, 8301),
            "the now-legitimate ⏳ must not be removed"
        );
        assert!(
            persisted_exists(7301, 8301),
            "the persisted record survives the aborted removal"
        );
    }
}
