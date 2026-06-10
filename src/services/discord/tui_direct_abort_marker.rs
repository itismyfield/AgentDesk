//! #3296 — durable aborted-anchor markers: reconcile the anchor reaction after
//! a synthetic turn-start ABORT instead of branding it a failure.
//!
//! ## Why this exists
//! When a TUI-direct synthetic turn-start ABORTs on the backstop escalation
//! budget (`backstop_abort_foreign_inflight_live`, see
//! [`super::tui_direct_pending_start`]), the user's input was ALREADY submitted
//! to the provider — the abort drops only the synthetic OWNERSHIP claim. In
//! every live observation the prior turn's owner then relayed the response
//! normally, yet the #3282-era cleanup swapped the anchor's `⏳` for a `⚠`,
//! permanently branding an ANSWERED message as failed (#3296).
//!
//! ## Mechanism
//! The ABORT path now KEEPS the `⏳` (it is still true: the provider holds the
//! input) and records a durable [`AbortedAnchorMarker`] here. Two reconcilers
//! own the marker afterwards:
//!
//! 1. **Terminal-commit drain** ([`drain_on_terminal_commit`]) — the tmux
//!    watcher's terminal chokepoint calls this on every body-visible normal
//!    commit; a commit on the SAME `(provider, tmux, channel)` AFTER the abort
//!    (and within the TTL, bounding session-recreation aliasing) means the
//!    prior owner covered the input → `⏳ → ✅`, marker drained.
//! 2. **TTL sweep** ([`sweep_expired`]) — the placeholder sweeper's pass: once
//!    [`ABORT_MARKER_TTL`] elapsed with NO live inflight for the session (a
//!    long turn still streaming holds the verdict), nothing ever covered the
//!    anchor → `⏳ → ⚠`, so a genuine failure is still surfaced in bounded
//!    time (no #3282 eternal-hourglass regression: the sweeper is the owner).
//!
//! ## Invariants
//! * **I1 (#3164 add≡remove)**: every reaction op (`⏳` remove, `✅`/`⚠` add)
//!   resolves `shared.serenity_http_or_token_fallback()` INSIDE this module —
//!   the same bot identity that added the `⏳`. No caller-provided http is
//!   accepted, so a watcher/sweeper-bootstrap http can never be misused.
//! * **I4 (turn-identity pin)**: every correction targets ONLY the marker's own
//!   `anchor_message_id` — the shared `prompt_anchor_by_tmux` slot is never
//!   re-read (slot aliasing under rapid injection would hit the wrong turn).
//! * **I5 (zero-id guard)**: a zero anchor id is never recorded or reacted on
//!   (`MessageId::new(0)` panics).
//! * **I6 (fail-open)**: when http is unavailable or a delivery fails, the
//!   marker is PRESERVED (a covering commit stamps `covered_at_ms` so the
//!   sweep retries the `✅`) — never silently dropped.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serenity::{ChannelId, MessageId};

use poise::serenity_prelude as serenity;

use super::SharedData;

/// How long an aborted anchor may wait for a covering terminal commit before
/// the sweep declares it a genuine failure (`⏳ → ⚠`). Rationale: the observed
/// ABORT→covered window is ~30-180s (backstop 32s + prior-owner long turns);
/// 600s comfortably covers a long streaming prior turn while still bounding a
/// truly-lost input to TTL + sweeper initial delay (180s) + pass interval (30s).
pub(super) const ABORT_MARKER_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// Durable record for an anchor whose synthetic turn-start ABORTed while the
/// input was already provider-submitted. All fields are primitives so the JSON
/// survives a dcserver version swap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct AbortedAnchorMarker {
    pub provider: String,
    pub channel_id: u64,
    /// Identity pin (I4): the ONLY message any `✅`/`⚠` correction may target.
    pub anchor_message_id: u64,
    pub tmux_session_name: String,
    /// Wall-clock ms of the ABORT. A covering commit must be strictly later.
    pub aborted_at_ms: u64,
    /// Stamped when a covering terminal commit was seen but the `✅` delivery
    /// failed (or http was unavailable) — the sweep retries the completion
    /// instead of ever degrading a covered anchor to `⚠` (I6).
    #[serde(default)]
    pub covered_at_ms: Option<u64>,
}

impl AbortedAnchorMarker {
    fn file_stem(&self) -> String {
        format!(
            "{}_{}_{}",
            self.provider, self.channel_id, self.anchor_message_id
        )
    }
}

// ---------------------------------------------------------------------------
// Durable store (mirrors `tui_direct_pending_start`'s store + atomic writes)
// ---------------------------------------------------------------------------

// Thread-local test seam for the durable root (the `TEST_TMUX_ALIVE_OVERRIDE`
// convention, inflight.rs). Tests inject a tempdir here instead of mutating
// the process-global `AGENTDESK_ROOT_DIR` env: env mutation races every test
// that READS the root without holding the crate env lock (e.g. the
// `tui_direct_pending_start` worker tests' `persist()`), and a thread-local
// needs no lock at all (each test thread sees only its own override; the
// current-thread `block_on` runtimes the tests use stay on this thread).
#[cfg(test)]
thread_local! {
    static TEST_ROOT_OVERRIDE: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn set_test_root_override(path: Option<std::path::PathBuf>) {
    TEST_ROOT_OVERRIDE.with(|cell| *cell.borrow_mut() = path);
}

fn root() -> Option<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(path) = TEST_ROOT_OVERRIDE.with(|cell| cell.borrow().clone()) {
        return Some(path);
    }
    super::runtime_store::tui_direct_abort_marker_root()
}

/// Persist (or update) a marker. Recorded by the ABORT path BEFORE any http
/// availability check so a restart or late-arriving http can still reconcile.
/// Zero anchor ids are rejected (I5: nothing could ever be reconciled on them).
pub(super) fn record(marker: &AbortedAnchorMarker) -> Result<(), String> {
    if marker.anchor_message_id == 0 {
        return Err("refusing to record aborted-anchor marker with zero anchor_message_id".into());
    }
    let Some(root) = root() else {
        return Ok(()); // tests / unconfigured root — nothing durable to write
    };
    let path = root.join(format!("{}.json", marker.file_stem()));
    let data = serde_json::to_string_pretty(marker).map_err(|e| e.to_string())?;
    super::runtime_store::critical_atomic_write(
        &path,
        &data,
        super::runtime_store::AtomicWriteContext::new("tui_direct_abort_marker")
            .provider(&marker.provider)
            .channel_id(marker.channel_id),
    )
}

/// Drop a marker once its correction was delivered. Idempotent.
pub(super) fn delete(marker: &AbortedAnchorMarker) {
    if let Some(root) = root() {
        let path = root.join(format!("{}.json", marker.file_stem()));
        let _ = std::fs::remove_file(path);
    }
}

/// Load every durable marker (sweep + restart survival: the store IS the
/// restart state — no in-memory index to rebuild).
pub(super) fn load_all() -> Vec<AbortedAnchorMarker> {
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
            && let Ok(marker) = serde_json::from_str::<AbortedAnchorMarker>(&text)
        {
            out.push(marker);
        }
    }
    out
}

/// Markers scoped to one `(provider, channel)` — the terminal-commit drain's
/// working set.
pub(super) fn load_for_channel(provider: &str, channel_id: u64) -> Vec<AbortedAnchorMarker> {
    load_all()
        .into_iter()
        .filter(|m| m.channel_id == channel_id && m.provider.eq_ignore_ascii_case(provider))
        .collect()
}

// ---------------------------------------------------------------------------
// Pure decision functions (truth-table tested — no I/O, no clock)
// ---------------------------------------------------------------------------

/// What the sweep should do with a marker this pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MarkerDisposition {
    /// TTL not elapsed, or a live inflight for the session still holds the
    /// verdict (a long prior turn may yet cover the anchor) — re-evaluate next
    /// pass.
    KeepWaiting,
    /// A covering commit was already seen (`covered_at_ms`) — (re)deliver the
    /// `⏳ → ✅` completion.
    DeliverCompletion,
    /// TTL elapsed with no live inflight and no covering commit — deliver the
    /// `⏳ → ⚠` failure fallback (I10: the only path that may `⚠`).
    DeliverFailureWarn,
    /// Http unavailable — leave every marker intact (I6 fail-open).
    LeftIntactHttpUnavailable,
}

/// The sweep's per-marker verdict. Conservative by design (I10): `⚠` requires
/// BOTH the TTL to have elapsed AND no live inflight for the session, so a
/// long-running prior turn is never falsely branded; `✅` retry requires a
/// previously-seen covering commit.
pub(super) fn decide_marker_disposition(
    now_ms: u64,
    marker: &AbortedAnchorMarker,
    live_inflight_for_session: bool,
    ttl: std::time::Duration,
    http_available: bool,
) -> MarkerDisposition {
    if !http_available {
        return MarkerDisposition::LeftIntactHttpUnavailable;
    }
    if marker.covered_at_ms.is_some() {
        return MarkerDisposition::DeliverCompletion;
    }
    let ttl_elapsed = now_ms.saturating_sub(marker.aborted_at_ms) >= ttl.as_millis() as u64;
    if !ttl_elapsed || live_inflight_for_session {
        return MarkerDisposition::KeepWaiting;
    }
    MarkerDisposition::DeliverFailureWarn
}

/// Does a terminal commit observed at `now_ms` cover this marker? Only a commit
/// STRICTLY AFTER the abort counts (a pre-abort commit belongs to an older
/// turn), and only within the TTL (bounding `✅` mis-fires from an unrelated
/// commit after the session was recycled — SC2/R3).
pub(super) fn terminal_commit_covers_marker(
    now_ms: u64,
    marker: &AbortedAnchorMarker,
    ttl: std::time::Duration,
) -> bool {
    marker.anchor_message_id != 0
        && now_ms > marker.aborted_at_ms
        && now_ms.saturating_sub(marker.aborted_at_ms) <= ttl.as_millis() as u64
}

// ---------------------------------------------------------------------------
// Reaction applier (boxed-fn injection, `ClaimFn`/`AbortCleanupFn` convention)
// ---------------------------------------------------------------------------

/// The reaction correction to apply to the marker's pinned anchor message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReactionOp {
    /// `⏳` remove + `✅` add (anchor covered by the prior owner).
    Complete,
    /// `⏳` remove + `⚠` add (TTL'd genuine failure).
    FailureWarn,
}

/// Outcome of one applier invocation, driving keep/delete of the marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReactionDelivery {
    Delivered,
    Failed,
    HttpUnavailable,
}

/// Boxed applier so tests record ops instead of calling Discord. The PRODUCTION
/// applier is [`shared_reaction_applier`]; per I1 it does NOT accept an http
/// parameter — it resolves `shared.serenity_http_or_token_fallback()` per call.
pub(super) type ReactionApplierFn = Box<
    dyn Fn(
            &AbortedAnchorMarker,
            ReactionOp,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ReactionDelivery> + Send>>
        + Send
        + Sync,
>;

/// The production applier. Bot identity (#3164 add≡remove, I1): the `⏳` was
/// added via the relay's `shared.serenity_http_or_token_fallback()` (the
/// provider/command bot), and `remove_reaction_raw` only removes `@me`'s
/// reaction — resolving the SAME source here guarantees the removal targets
/// exactly the reaction the add created. Success is keyed on the `✅`/`⚠`
/// create (the remove is best-effort, mirroring
/// `complete_tui_direct_prompt_anchor_lifecycle_if_present`).
pub(super) fn shared_reaction_applier(shared: Arc<SharedData>) -> ReactionApplierFn {
    Box::new(move |marker, op| {
        let shared = shared.clone();
        let provider = marker.provider.clone();
        let channel_id = marker.channel_id;
        let anchor_message_id = marker.anchor_message_id;
        Box::pin(async move {
            if anchor_message_id == 0 {
                return ReactionDelivery::Failed; // I5 (defensive; record() already rejects)
            }
            let Some(http) = shared.serenity_http_or_token_fallback() else {
                return ReactionDelivery::HttpUnavailable;
            };
            let channel = ChannelId::new(channel_id);
            let message = MessageId::new(anchor_message_id);
            super::formatting::remove_reaction_raw(&http, channel, message, '⏳').await;
            let emoji = match op {
                ReactionOp::Complete => '✅',
                ReactionOp::FailureWarn => '⚠',
            };
            let reaction = serenity::ReactionType::Unicode(emoji.to_string());
            match channel.create_reaction(&http, message, reaction).await {
                Ok(_) => ReactionDelivery::Delivered,
                Err(error) => {
                    tracing::warn!(
                        provider = %provider,
                        channel_id,
                        anchor_message_id,
                        op = ?op,
                        error = %error,
                        "tui_direct_abort_marker: reaction correction delivery failed; marker preserved for retry (I6)"
                    );
                    ReactionDelivery::Failed
                }
            }
        })
    })
}

// ---------------------------------------------------------------------------
// Reconcilers
// ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// Watcher terminal-commit chokepoint: a body-visible normal commit for
/// `(provider, tmux, channel)` covers every matching marker → `⏳ → ✅`.
/// Returns the number of markers fully drained.
pub(super) async fn drain_on_terminal_commit(
    shared: &Arc<SharedData>,
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
) -> usize {
    let applier = shared_reaction_applier(shared.clone());
    drain_on_terminal_commit_with_applier(
        provider,
        tmux_session_name,
        channel_id,
        now_ms(),
        &applier,
    )
    .await
}

pub(super) async fn drain_on_terminal_commit_with_applier(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    now_ms: u64,
    applier: &ReactionApplierFn,
) -> usize {
    let mut drained = 0usize;
    for mut marker in load_for_channel(provider, channel_id) {
        if marker.tmux_session_name != tmux_session_name {
            continue; // I4: a different session's marker is never this commit's
        }
        if !terminal_commit_covers_marker(now_ms, &marker, ABORT_MARKER_TTL) {
            continue;
        }
        match applier(&marker, ReactionOp::Complete).await {
            ReactionDelivery::Delivered => {
                delete(&marker);
                drained += 1;
                tracing::info!(
                    provider = %marker.provider,
                    channel_id = marker.channel_id,
                    tmux_session_name = %marker.tmux_session_name,
                    anchor_message_id = marker.anchor_message_id,
                    "tui_direct_abort_marker: aborted anchor covered by prior-owner terminal commit; ⏳ → ✅ delivered and marker drained (#3296)"
                );
            }
            ReactionDelivery::Failed | ReactionDelivery::HttpUnavailable => {
                // I6 fail-open: the anchor IS covered — stamp it so the sweep
                // retries the ✅ (and can never degrade it to ⚠).
                marker.covered_at_ms = Some(now_ms);
                let _ = record(&marker);
            }
        }
    }
    drained
}

/// Placeholder-sweeper pass: retry `✅` for covered markers; apply the TTL'd
/// `⏳ → ⚠` fallback for anchors no commit ever covered (held while a live
/// inflight for the session may still cover them). Returns markers resolved.
pub(super) async fn sweep_expired(
    shared: &Arc<SharedData>,
    provider: &super::ProviderKind,
) -> usize {
    let http_available = shared.serenity_http_or_token_fallback().is_some();
    let applier = shared_reaction_applier(shared.clone());
    let live_inflight = |marker: &AbortedAnchorMarker| -> bool {
        super::inflight::load_inflight_state(provider, marker.channel_id).is_some_and(|state| {
            // Conservative (I10): an inflight without a tmux name COULD be the
            // covering prior owner — hold rather than risk a false ⚠.
            state
                .tmux_session_name
                .as_deref()
                .is_none_or(|name| name == marker.tmux_session_name)
        })
    };
    sweep_expired_with_applier(
        provider.as_str(),
        now_ms(),
        http_available,
        &live_inflight,
        &applier,
    )
    .await
}

pub(super) async fn sweep_expired_with_applier(
    provider: &str,
    now_ms: u64,
    http_available: bool,
    live_inflight_for_session: &(dyn Fn(&AbortedAnchorMarker) -> bool + Send + Sync),
    applier: &ReactionApplierFn,
) -> usize {
    let mut resolved = 0usize;
    for marker in load_all() {
        if !marker.provider.eq_ignore_ascii_case(provider) {
            continue;
        }
        if marker.anchor_message_id == 0 {
            delete(&marker); // I5: corrupt record — nothing could ever target it
            continue;
        }
        let disposition = decide_marker_disposition(
            now_ms,
            &marker,
            live_inflight_for_session(&marker),
            ABORT_MARKER_TTL,
            http_available,
        );
        let op = match disposition {
            MarkerDisposition::KeepWaiting | MarkerDisposition::LeftIntactHttpUnavailable => {
                continue;
            }
            MarkerDisposition::DeliverCompletion => ReactionOp::Complete,
            MarkerDisposition::DeliverFailureWarn => ReactionOp::FailureWarn,
        };
        match applier(&marker, op).await {
            ReactionDelivery::Delivered => {
                delete(&marker);
                resolved += 1;
                tracing::info!(
                    provider = %marker.provider,
                    channel_id = marker.channel_id,
                    tmux_session_name = %marker.tmux_session_name,
                    anchor_message_id = marker.anchor_message_id,
                    op = ?op,
                    "tui_direct_abort_marker: sweep resolved aborted anchor (#3296)"
                );
            }
            // I6: keep the marker for the next pass (delivery failed late).
            ReactionDelivery::Failed | ReactionDelivery::HttpUnavailable => {}
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Injects a per-test tempdir as the durable root via the THREAD-LOCAL
    /// override (never the process-global `AGENTDESK_ROOT_DIR` env — mutating
    /// that races every test that reads the root without the crate env lock,
    /// e.g. the `tui_direct_pending_start` worker tests' `persist()`). No lock
    /// is needed: each test thread sees only its own override.
    struct TestRoot {
        _temp: tempfile::TempDir,
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            set_test_root_override(None);
        }
    }

    fn test_root() -> TestRoot {
        let temp = tempfile::tempdir().unwrap();
        set_test_root_override(Some(temp.path().join("discord_tui_direct_abort_marker")));
        std::fs::create_dir_all(root().expect("durable root configured under temp")).unwrap();
        TestRoot { _temp: temp }
    }

    /// A current-thread runtime keeps the async drains on THIS thread so the
    /// thread-local root override resolves inside them (and no
    /// `await_holding_lock` allow sites are needed — the repo ratchet is
    /// frozen at its baseline).
    fn test_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn marker(
        provider: &str,
        channel: u64,
        anchor: u64,
        aborted_at_ms: u64,
    ) -> AbortedAnchorMarker {
        AbortedAnchorMarker {
            provider: provider.to_string(),
            channel_id: channel,
            anchor_message_id: anchor,
            tmux_session_name: format!("tmux-{channel}"),
            aborted_at_ms,
            covered_at_ms: None,
        }
    }

    type RecordedOps = Arc<Mutex<Vec<(u64, ReactionOp)>>>;

    /// Recording applier (the `recording_abort_cleanup` convention): captures
    /// `(anchor_message_id, op)` so tests pin the identity-pinned target (I4)
    /// and returns a fixed delivery outcome.
    fn recording_applier(outcome: ReactionDelivery) -> (ReactionApplierFn, RecordedOps) {
        let calls: RecordedOps = Arc::new(Mutex::new(Vec::new()));
        let calls_for_fn = calls.clone();
        let applier: ReactionApplierFn = Box::new(move |marker, op| {
            let calls = calls_for_fn.clone();
            let anchor = marker.anchor_message_id;
            Box::pin(async move {
                calls.lock().unwrap().push((anchor, op));
                outcome
            })
        });
        (applier, calls)
    }

    const TTL_MS: u64 = ABORT_MARKER_TTL.as_millis() as u64;

    /// RED-4: the full {ttl}×{live inflight}×{covered}×{http} truth table.
    #[test]
    fn decide_marker_disposition_truth_table() {
        let base = marker("claude", 1, 10, 1_000);
        let covered = AbortedAnchorMarker {
            covered_at_ms: Some(2_000),
            ..base.clone()
        };
        let pre_ttl = 1_000 + TTL_MS - 1;
        let post_ttl = 1_000 + TTL_MS;
        for (now, m, live, http, want) in [
            // http unavailable → ALWAYS left intact (I6), regardless of the rest.
            (
                pre_ttl,
                &base,
                false,
                false,
                MarkerDisposition::LeftIntactHttpUnavailable,
            ),
            (
                pre_ttl,
                &base,
                true,
                false,
                MarkerDisposition::LeftIntactHttpUnavailable,
            ),
            (
                post_ttl,
                &base,
                false,
                false,
                MarkerDisposition::LeftIntactHttpUnavailable,
            ),
            (
                post_ttl,
                &base,
                true,
                false,
                MarkerDisposition::LeftIntactHttpUnavailable,
            ),
            (
                pre_ttl,
                &covered,
                false,
                false,
                MarkerDisposition::LeftIntactHttpUnavailable,
            ),
            (
                pre_ttl,
                &covered,
                true,
                false,
                MarkerDisposition::LeftIntactHttpUnavailable,
            ),
            (
                post_ttl,
                &covered,
                false,
                false,
                MarkerDisposition::LeftIntactHttpUnavailable,
            ),
            (
                post_ttl,
                &covered,
                true,
                false,
                MarkerDisposition::LeftIntactHttpUnavailable,
            ),
            // covered → completion retry, before AND after the TTL, inflight or not.
            (
                pre_ttl,
                &covered,
                false,
                true,
                MarkerDisposition::DeliverCompletion,
            ),
            (
                pre_ttl,
                &covered,
                true,
                true,
                MarkerDisposition::DeliverCompletion,
            ),
            (
                post_ttl,
                &covered,
                false,
                true,
                MarkerDisposition::DeliverCompletion,
            ),
            (
                post_ttl,
                &covered,
                true,
                true,
                MarkerDisposition::DeliverCompletion,
            ),
            // uncovered, TTL not elapsed → wait (no premature ⚠, RED if ⚠ here).
            (pre_ttl, &base, false, true, MarkerDisposition::KeepWaiting),
            (pre_ttl, &base, true, true, MarkerDisposition::KeepWaiting),
            // uncovered, TTL elapsed, live inflight → HOLD (long-turn ⚠ guard, I10).
            (post_ttl, &base, true, true, MarkerDisposition::KeepWaiting),
            // uncovered, TTL elapsed, no inflight → the ONLY ⚠ path (I10).
            (
                post_ttl,
                &base,
                false,
                true,
                MarkerDisposition::DeliverFailureWarn,
            ),
        ] {
            assert_eq!(
                decide_marker_disposition(now, m, live, ABORT_MARKER_TTL, http),
                want,
                "now={now} covered={:?} live={live} http={http}",
                m.covered_at_ms
            );
        }
    }

    #[test]
    fn terminal_commit_cover_requires_post_abort_within_ttl() {
        let m = marker("claude", 1, 10, 5_000);
        // Strictly-after-abort: a commit AT or BEFORE the abort instant belongs
        // to an older turn and must not cover (RED if `>=`).
        assert!(!terminal_commit_covers_marker(5_000, &m, ABORT_MARKER_TTL));
        assert!(!terminal_commit_covers_marker(4_000, &m, ABORT_MARKER_TTL));
        assert!(terminal_commit_covers_marker(5_001, &m, ABORT_MARKER_TTL));
        assert!(terminal_commit_covers_marker(
            5_000 + TTL_MS,
            &m,
            ABORT_MARKER_TTL
        ));
        // Past the TTL an unrelated commit (recycled session) must not ✅ (SC2).
        assert!(!terminal_commit_covers_marker(
            5_000 + TTL_MS + 1,
            &m,
            ABORT_MARKER_TTL
        ));
        // Zero anchor id never covers (I5).
        let zero = AbortedAnchorMarker {
            anchor_message_id: 0,
            ..m
        };
        assert!(!terminal_commit_covers_marker(
            5_001,
            &zero,
            ABORT_MARKER_TTL
        ));
    }

    /// I5: the recorder refuses zero anchor ids outright.
    #[test]
    fn record_rejects_zero_anchor_id() {
        let _root = test_root();
        let zero = AbortedAnchorMarker {
            anchor_message_id: 0,
            ..marker("claude", 7, 1, 100)
        };
        assert!(record(&zero).is_err());
        assert!(load_all().is_empty());
    }

    /// Restart survival: a persisted marker reloads with full field fidelity
    /// so the post-restart sweep handles it identically.
    #[test]
    fn durable_roundtrip_survives_reload() {
        let _root = test_root();
        let mut m = marker("codex", 42, 9001, 123_456);
        m.covered_at_ms = Some(123_999);
        record(&m).unwrap();
        let loaded = load_for_channel("codex", 42);
        assert_eq!(loaded, vec![m.clone()]);
        delete(&m);
        assert!(load_for_channel("codex", 42).is_empty());
    }

    /// RED-1 (covered direction): a same-(provider,tmux,channel) terminal
    /// commit after the abort drains the marker with EXACTLY ONE `Complete`
    /// op on the pinned anchor id — and never a `⚠`.
    #[test]
    fn drain_on_terminal_commit_completes_covered_marker() {
        let _root = test_root();
        let m = marker("claude", 100, 555, 10_000);
        record(&m).unwrap();
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let drained = test_rt().block_on(drain_on_terminal_commit_with_applier(
            "claude", "tmux-100", 100, 10_500, // commit strictly after the abort, within TTL
            &applier,
        ));
        assert_eq!(drained, 1);
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            &[(555, ReactionOp::Complete)],
            "exactly one ⏳→✅ on the marker's own anchor id (I4), ⚠ never — \
             RED if the drain skips the marker (the 10:52 ⚠-on-answered case) \
             or targets a shared slot"
        );
        assert!(
            load_for_channel("claude", 100).is_empty(),
            "delivered completion must drain the durable marker"
        );
    }

    /// I4/R3 + identity scoping: a commit for a DIFFERENT tmux session or a
    /// commit at/before the abort instant must not touch the marker.
    #[test]
    fn drain_skips_foreign_session_and_pre_abort_commit() {
        let _root = test_root();
        let m = marker("claude", 100, 556, 10_000);
        record(&m).unwrap();
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let rt = test_rt();
        // Foreign tmux session on the same channel → no-op.
        let drained = rt.block_on(drain_on_terminal_commit_with_applier(
            "claude",
            "tmux-other",
            100,
            10_500,
            &applier,
        ));
        assert_eq!(drained, 0);
        // Commit not after the abort → no-op (an older turn's commit).
        let drained = rt.block_on(drain_on_terminal_commit_with_applier(
            "claude", "tmux-100", 100, 10_000, &applier,
        ));
        assert_eq!(drained, 0);
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(load_for_channel("claude", 100).len(), 1, "marker retained");
    }

    /// I6: a covering commit whose ✅ delivery FAILS preserves the marker with
    /// `covered_at_ms` stamped, and the next sweep retries the COMPLETION
    /// (never degrades the covered anchor to ⚠ even past the TTL).
    #[test]
    fn failed_delivery_stamps_covered_and_sweep_retries_completion() {
        let _root = test_root();
        let m = marker("claude", 100, 557, 10_000);
        record(&m).unwrap();
        let rt = test_rt();
        let (failing, _calls) = recording_applier(ReactionDelivery::Failed);
        let drained = rt.block_on(drain_on_terminal_commit_with_applier(
            "claude", "tmux-100", 100, 10_500, &failing,
        ));
        assert_eq!(drained, 0);
        let kept = load_for_channel("claude", 100);
        assert_eq!(kept.len(), 1);
        assert_eq!(
            kept[0].covered_at_ms,
            Some(10_500),
            "failed ✅ delivery must stamp covered_at and keep the marker (I6) — \
             RED if the marker is dropped (silent loss) or left unstamped (would ⚠ a covered anchor)"
        );
        // Sweep far past the TTL with no inflight: still retries ✅, never ⚠.
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let resolved = rt.block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS * 2,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 1);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(557, ReactionOp::Complete)]
        );
        assert!(load_for_channel("claude", 100).is_empty());
    }

    /// RED-2 (a): TTL elapsed but a live inflight for the session exists —
    /// the sweep HOLDS (no reaction op, marker preserved).
    #[test]
    fn sweep_holds_while_live_inflight_present() {
        let _root = test_root();
        let m = marker("claude", 100, 558, 10_000);
        record(&m).unwrap();
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let resolved = test_rt().block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS + 1,
            true,
            &|_| true, // live inflight for the session
            &applier,
        ));
        assert_eq!(resolved, 0);
        assert!(
            calls.lock().unwrap().is_empty(),
            "a long-running prior turn must hold the ⚠ verdict (I10) — \
             RED if the sweep warns while an inflight is live (false ⚠ on a long turn)"
        );
        assert_eq!(load_for_channel("claude", 100).len(), 1);
    }

    /// RED-2 (b): TTL elapsed and NO live inflight — the sweep delivers the
    /// `⏳ → ⚠` fallback exactly once on the pinned anchor and drains the
    /// marker (bounded convergence: no #3282 eternal hourglass).
    #[test]
    fn sweep_warns_after_ttl_without_inflight() {
        let _root = test_root();
        let m = marker("claude", 100, 559, 10_000);
        record(&m).unwrap();
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let resolved = test_rt().block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS + 1,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 1);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(559, ReactionOp::FailureWarn)],
            "a genuinely-uncovered anchor must reach ⚠ in bounded time — \
             RED if the sweep never warns (the ⏳ would linger forever, #3282)"
        );
        assert!(load_for_channel("claude", 100).is_empty());
    }

    /// RED-2 (c) / I6: http unavailable — EVERY marker is preserved untouched.
    #[test]
    fn sweep_preserves_all_when_http_unavailable() {
        let _root = test_root();
        let m = marker("claude", 100, 560, 10_000);
        record(&m).unwrap();
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let resolved = test_rt().block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS + 1,
            false, // http unavailable
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 0);
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(
            load_for_channel("claude", 100).len(),
            1,
            "http-unavailable must fail open (marker preserved for the next pass, I6)"
        );
    }

    /// Sweep scoping: another provider's markers are never touched.
    #[test]
    fn sweep_is_provider_scoped() {
        let _root = test_root();
        let m = marker("codex", 100, 561, 10_000);
        record(&m).unwrap();
        let (applier, calls) = recording_applier(ReactionDelivery::Delivered);
        let resolved = test_rt().block_on(sweep_expired_with_applier(
            "claude",
            10_000 + TTL_MS + 1,
            true,
            &|_| false,
            &applier,
        ));
        assert_eq!(resolved, 0);
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(load_for_channel("codex", 100).len(), 1);
    }
}
