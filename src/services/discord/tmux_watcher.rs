use super::*;
use crate::services::discord::InflightTurnState;
use crate::services::discord::http::{edit_channel_message, send_channel_message};
use crate::services::discord::outbound::delivery_record as dr; // #3089 B2b
use crate::services::discord::replace_outcome_policy::watcher_partial_continuation_retry_plan;

#[path = "tmux_watcher/liveness.rs"]
mod liveness;

pub(super) use self::liveness::watcher_lifecycle_terminal_delivery_observed;
use self::liveness::*;

#[path = "tmux_watcher/panel_decisions.rs"]
mod panel_decisions;

use self::panel_decisions::*;

// #3805 P2 (PR-C): two-message status-panel WATCHER creation-order parity — the
// small PURE gate/generation/completion predicates the watcher loop and its
// single_message_footer.rs completion call thread through thinly (logic here, not
// in the EXTREME giant nor the 700-capped footer sibling).
#[path = "tmux_watcher/two_message_panel.rs"]
mod two_message_panel;

use self::two_message_panel::*;

#[path = "tmux_watcher/prompt_observe.rs"]
mod prompt_observe;

use self::prompt_observe::*;

#[path = "tmux_watcher/turn_identity.rs"]
mod turn_identity;

pub(in crate::services::discord) use self::turn_identity::emit_explicit_inflight_cleanup_signal;
use self::turn_identity::*;

#[path = "tmux_watcher/completion_gate.rs"]
mod completion_gate;

use self::completion_gate::*;

#[path = "tmux_watcher/commit_decisions.rs"]
mod commit_decisions;

use self::commit_decisions::*;

#[path = "tmux_watcher/controller_heartbeat.rs"]
mod controller_heartbeat;

#[path = "tmux_watcher/placeholder_reclaim.rs"]
mod placeholder_reclaim;

#[path = "tmux_watcher/single_message_footer.rs"]
mod single_message_footer;

#[path = "tmux_watcher/terminal_send.rs"]
mod terminal_send;

#[path = "tmux_watcher/terminal_long_chunks.rs"]
mod terminal_long_chunks;

// #3479 item-2: the watcher-direct orphan status-panel cleanup/completion/refresh
// cluster extracted to a sibling submodule (pure move, zero logic change). Items
// are `pub(super)` there and re-imported below so the watcher loop's call sites —
// and the sibling `single_message_footer.rs` completion call — stay byte-identical.
#[path = "tmux_watcher/orphan_status_panel_cleanup.rs"]
mod orphan_status_panel_cleanup;

use self::orphan_status_panel_cleanup::{
    cleanup_orphan_external_input_status_panel, complete_watcher_status_panel_v2,
    refresh_watcher_session_panel_from_lifecycle,
};

// #3479 item-2: provider-session selector resolution + persistence cluster
// extracted to a sibling submodule (pure move, zero logic change). Items are
// `pub(super)` there and re-imported here so the watcher loop's call sites stay
// byte-identical.
#[path = "tmux_watcher/provider_session_persistence.rs"]
mod provider_session_persistence;

use self::provider_session_persistence::persist_watcher_provider_session_id;

// #3479 Phase-1 rank-1: the supervisor relay-forward + session-bound terminal ACK
// cluster extracted to sibling submodules (pure move, zero logic change). Split
// into two cohesive files only to keep each within the tmux_watcher/** namespace
// LoC cap: `supervisor_relay` holds the forward half (+ the shared
// `SessionBoundRelayAckTarget` type), `session_bound_ack` holds the ACK-outcome /
// terminal-resend / emission-slot-guard half. Items are `pub(super)` there and
// re-imported here so the watcher loop's call sites stay byte-identical.
#[path = "tmux_watcher/supervisor_relay.rs"]
mod supervisor_relay;

#[path = "tmux_watcher/session_bound_ack.rs"]
mod session_bound_ack;

// #3479 Phase-1 rank-2: two more cohesive PURE clusters extracted to sibling
// submodules (pure move, zero logic change). `utf8_chunk_decoder` holds the
// streaming UTF-8 chunk decoder; `terminal_readiness` holds the synchronous
// terminal-readiness / inflight-classification predicates and the pure
// buffer/message-id reconcilers. The async `shared`-touching
// `commit_watcher_direct_terminal_session_idle` (which sits between the two
// readiness clusters in this root) deliberately STAYS here. Items are
// `pub(super)` there and re-imported here so the watcher loop's call sites stay
// byte-identical.
#[path = "tmux_watcher/terminal_readiness.rs"]
mod terminal_readiness;

#[path = "tmux_watcher/utf8_chunk_decoder.rs"]
mod utf8_chunk_decoder;

#[path = "tmux_watcher/stall_exit.rs"]
mod stall_exit;

pub(in crate::services::discord) use self::completion_gate::{
    TuiCompletionGateOutcome, run_tui_completion_gate,
};
use self::placeholder_reclaim::*;
use self::session_bound_ack::*;
use self::single_message_footer::*;
use self::stall_exit::*;
use self::supervisor_relay::*;
use self::terminal_readiness::*;
use self::utf8_chunk_decoder::*;

#[cfg(unix)]
async fn commit_watcher_direct_terminal_session_idle(
    shared: &std::sync::Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: &str,
    terminal_kind: Option<WatcherTerminalKind>,
    data_start_offset: u64,
    current_offset: u64,
) -> bool {
    if shared.mailbox(channel_id).cancel_token().await.is_some() {
        tracing::debug!(
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            provider = %provider.as_str(),
            "skipping watcher-direct terminal session-idle commit; mailbox turn is active"
        );
        return false;
    }

    if crate::services::discord::inflight::load_inflight_state(provider, channel_id.get()).is_some()
    {
        tracing::debug!(
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            provider = %provider.as_str(),
            "skipping watcher-direct terminal session-idle commit; inflight state is active"
        );
        return false;
    }

    let session_key = crate::services::discord::adk_session::build_namespaced_session_key(
        &shared.token_hash,
        provider,
        tmux_session_name,
    );
    let channel_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone())
    };
    let agent_id =
        crate::services::discord::resolve_channel_role_binding(channel_id, channel_name.as_deref())
            .map(|binding| binding.role_id);
    let terminal_committed_at = chrono::Utc::now();

    match crate::services::discord::internal_api::mark_session_idle_if_not_newer_live(
        &session_key,
        provider.as_str(),
        agent_id.as_deref(),
        terminal_committed_at,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!(
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                provider = %provider.as_str(),
                session_key = %session_key,
                data_start_offset,
                current_offset,
                terminal_kind = terminal_kind.map(WatcherTerminalKind::as_str).unwrap_or("unknown"),
                "skipping watcher-direct terminal session-idle commit; session row is absent or newer live"
            );
            return false;
        }
        Err(error) => {
            tracing::warn!(
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                provider = %provider.as_str(),
                session_key = %session_key,
                data_start_offset,
                current_offset,
                terminal_kind = terminal_kind.map(WatcherTerminalKind::as_str).unwrap_or("unknown"),
                error = %error,
                "failed to commit watcher-direct terminal session idle"
            );
            return false;
        }
    }

    tracing::info!(
        channel_id = channel_id.get(),
        tmux_session_name = %tmux_session_name,
        provider = %provider.as_str(),
        session_key = %session_key,
        data_start_offset,
        current_offset,
        terminal_kind = terminal_kind.map(WatcherTerminalKind::as_str).unwrap_or("unknown"),
        "watcher-direct terminal response committed session idle"
    );
    true
}

pub(in crate::services::discord) async fn tmux_output_watcher(
    channel_id: ChannelId,
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    output_path: String,
    tmux_session_name: String,
    initial_offset: u64,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    paused: Arc<std::sync::atomic::AtomicBool>,
    resume_offset: Arc<std::sync::Mutex<Option<u64>>>,
    pause_epoch: Arc<std::sync::atomic::AtomicU64>,
    turn_delivered: Arc<std::sync::atomic::AtomicBool>,
    last_heartbeat_ts_ms: Arc<std::sync::atomic::AtomicI64>,
) {
    tmux_output_watcher_with_restore(
        channel_id,
        http,
        shared,
        output_path,
        tmux_session_name,
        initial_offset,
        cancel,
        paused,
        resume_offset,
        pause_epoch,
        turn_delivered,
        last_heartbeat_ts_ms,
        None,
    )
    .await;
}

/// Background watcher variant used by restart recovery to continue editing an
/// existing streaming placeholder instead of creating a new one.
pub(in crate::services::discord) async fn tmux_output_watcher_with_restore(
    channel_id: ChannelId,
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    output_path: String,
    tmux_session_name: String,
    initial_offset: u64,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    paused: Arc<std::sync::atomic::AtomicBool>,
    resume_offset: Arc<std::sync::Mutex<Option<u64>>>,
    pause_epoch: Arc<std::sync::atomic::AtomicU64>,
    turn_delivered: Arc<std::sync::atomic::AtomicBool>,
    last_heartbeat_ts_ms: Arc<std::sync::atomic::AtomicI64>,
    restored_turn: Option<RestoredWatcherTurn>,
) {
    use std::io::{Read, Seek, SeekFrom};

    // #3041 P1-1: this watcher instance's delivery-lease holder id. Minted once
    // per spawn so a replacement watcher cannot release/commit (or be mistaken
    // for) this instance's lease across a reattach (§5.2 B2). #3277 (Defect B):
    // minted BEFORE the start log so start/stop pairs are attributable — in the
    // incident two overlapping instances' unlabeled start/stop lines were
    // misread as one watcher dying.
    let watcher_instance_id = next_watcher_instance_id();
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 👁 tmux watcher started for #{tmux_session_name} at offset {initial_offset} (instance {watcher_instance_id})"
    );

    // E5 (#2412): cache the supervisor-owned StreamRelay producer for this
    // tmux session, if the supervisor is running and has matched the
    // session. `None` covers three legitimate cases:
    //   1. `cluster.session_bound_relay_enabled = false` (supervisor never
    //      spawned, registry empty).
    //   2. SessionDiscovery hasn't yet observed this session — the cache is
    //      refreshed below per chunk-read in that case.
    //   3. This watcher attached to a session the registry doesn't know
    //      (e.g. legacy session name pattern). The watcher keeps the legacy
    //      fallback path for envelopes the supervisor-owned relay cannot own.
    let producer_registry =
        crate::services::cluster::relay_producer_registry::global_relay_producer_registry();
    // Cached clone so we don't take the registry RwLock on every chunk. The
    // supervisor only ever publishes ONE producer per session name, but it
    // CAN republish after an Updated event (channel rebind). We refresh on
    // miss and after every send-failure (relay torn down → producer stale).
    let mut cached_relay_producer = producer_registry.get_producer(&tmux_session_name);

    // #1134: mark the attach moment so `record_first_relay` (below) can compute
    // attach→first-relay latency. Single instrumentation point covers all
    // spawn sites (recovery_engine, turn_bridge, tmux self-recovery).
    crate::services::observability::watcher_latency::record_attach(channel_id.get());

    let (watcher_provider, watcher_channel_name) =
        parse_provider_and_channel_from_tmux_name(&tmux_session_name).unwrap_or((
            crate::services::provider::ProviderKind::Claude,
            String::new(),
        ));
    let watcher_thread_channel_id =
        crate::services::discord::adk_session::parse_thread_channel_id_from_name(
            &watcher_channel_name,
        );
    let mut current_offset = initial_offset;
    let input_fifo_path =
        crate::services::discord::turn_bridge::tmux_runtime_paths(&tmux_session_name).1;
    // #1216: leftover JSONL bytes from a buffer that contained more than one
    // turn-terminating event. `process_watcher_lines` now stops at the first
    // `result`/auth/overload event and leaves the rest in the buffer; this
    // outer-scope `all_data` carries that leftover into the next watcher loop
    // iteration so the next turn does not need to wait for fresh disk reads.
    let mut all_data = String::new();
    let mut all_data_start_offset = current_offset;
    let mut all_data_fully_mirrored_to_session_relay = true;
    let mut all_data_session_bound_relay_ack: Option<SessionBoundRelayAckTarget> = None;
    let mut all_data_first_forwarded_relay_sequence: Option<u64> = None;
    let mut utf8_decoder = Utf8ChunkDecoder::default();
    let mut prompt_too_long_killed = false;
    let mut turn_result_relayed = false;
    let mut terminal_delivery_observed = false;
    let mut last_activity_heartbeat_at: Option<std::time::Instant> = None;
    // #1137: 1-shot guard so the "post-terminal-success continuation" log
    // is emitted exactly once per dispatch. Real-world traces (codex
    // G2/G3/G4 on 2026-04-22T23:34:13Z) showed multi-second continuation
    // bursts; logging every chunk would spam the timeline.
    let mut post_terminal_continuation_logged = false;
    let mut last_post_terminal_suppressed_range: Option<(u64, u64)> = None;
    // #3107: 1-shot guard so the "self-heal: re-acquired watcher-owned inflight
    // for an actively-streaming pane that lost its inflight" incident log is
    // emitted at most once per dispatch (mirrors the one-shot suppressed-range
    // logs above). The re-acquire itself is idempotent (no-op when an inflight
    // already exists), so this only bounds the log, not the heal.
    let mut active_stream_inflight_reacquire_logged = false;
    let mut completion_footer_idle = WatcherCompletionFooterIdleState::default();
    let mut completion_footer_spin_idx: usize = 0;
    let mut restored_turn = restored_turn;
    // #3107 codex re-review (P2#3, F3): the #3099 hourglass anchor
    // (`injected_prompt_message_id`) pinned by the restored turn, captured ONCE
    // up front before `restored_turn` is consumed by the streaming path's
    // `restored_turn.take()`. The streaming-interval re-acquire site fires later
    // in the same dispatch, by which point `restored_turn` is already gone — so
    // we stash the anchor here and thread it through. This keeps a
    // hourglass-anchored turn that loses its inflight MID-STREAM re-acquiring an
    // inflight that still carries the pinned message id, so the `⏳ → ✅`
    // completion cleanup can find its own message instead of orphaning it.
    let restored_injected_prompt_message_id = restored_turn
        .as_ref()
        .and_then(|turn| turn.injected_prompt_message_id);
    // Guard against duplicate relay: track the offset from which the last relay was sent.
    // If the outer loop circles back and current_offset hasn't advanced past this point,
    // the relay is suppressed.
    // Initialize from persisted inflight state so replacement watcher instances skip
    // already-delivered output (fixes double-reply on stale watcher replacement).
    // #1270: load both the persisted offset AND its matching
    // `.generation` mtime so a replacement watcher can correctly classify
    // an output regression on restored state. When we have a persisted
    // mtime, it labels the wrapper that produced the persisted offset:
    //   - matches current `.generation` mtime → same wrapper after
    //     `truncate_jsonl_head_safe` → pin to EOF (don't re-flood
    //     surviving content; codex P2 on PR #1271).
    //   - differs from current `.generation` mtime → cancel→respawn into
    //     the same session name → reset to 0 to pick up the fresh
    //     response.
    // When the persisted state predates this field (legacy `None`), we
    // fall back to "no baseline known" semantics — the regression check
    // treats it as a first observation and resets to 0, which is the
    // safer choice for not silently dropping a fresh response.
    let restored_inflight =
        parse_provider_and_channel_from_tmux_name(&tmux_session_name).and_then(|(pk, _)| {
            crate::services::discord::inflight::load_inflight_state(&pk, channel_id.get())
        });
    let mut watcher_turn_identity =
        matching_watcher_turn_identity(restored_inflight.as_ref(), &tmux_session_name);
    let mut last_relayed_offset: Option<u64> = restored_inflight
        .as_ref()
        .and_then(|s| s.last_watcher_relayed_offset);
    let mut last_observed_generation_mtime_ns: Option<i64> = restored_inflight
        .as_ref()
        .and_then(|s| s.last_watcher_relayed_generation_mtime_ns);
    if let Ok(meta) = std::fs::metadata(&output_path) {
        let observed_output_end = meta.len();
        reset_stale_relay_watermark_if_output_regressed(
            &shared,
            channel_id,
            &tmux_session_name,
            observed_output_end,
            "watcher_start",
        );
        reset_stale_local_relay_offset_if_output_regressed(
            &mut last_relayed_offset,
            &mut last_observed_generation_mtime_ns,
            channel_id,
            &tmux_session_name,
            observed_output_end,
            "watcher_start",
        );
    }
    // Rolling-size-cap rotation state. The watcher loop spins predictably
    // (~250ms sleeps) so a mod-N gate on an iteration counter gives a
    // regular-ish cadence for the size check without hitting the fs every
    // spin. See issue #892.
    let mut rotation_tick: u32 = 0;
    const ROTATION_CHECK_EVERY: u32 = 120; // ~30s at 250ms base cadence

    // #2441 (H1) — spawn a single `notify`-crate-backed JsonlWatcher
    // keyed on the session output path. Its `Notify` is awaited alongside
    // each polling `sleep()` in this function so a real wrapper write
    // wakes us immediately while the sleep still bounds the maximum
    // wake-up latency. The watcher is dropped automatically when this
    // task exits (or the wrapper rotates the file away).
    let jsonl_watcher = crate::services::discord::jsonl_watcher::JsonlWatcher::spawn(
        std::path::PathBuf::from(&output_path),
    );
    let jsonl_notify = jsonl_watcher.notify();
    let dead_marker_watcher =
        crate::services::discord::jsonl_watcher::JsonlWatcher::spawn(std::path::PathBuf::from(
            crate::services::tmux_common::session_dead_marker_path(&tmux_session_name),
        ));
    let dead_marker_notify = dead_marker_watcher.notify();

    'watcher_loop: loop {
        last_heartbeat_ts_ms.store(
            crate::services::discord::tmux_watcher_now_ms(),
            std::sync::atomic::Ordering::Release,
        );
        // Always consume resume_offset first — the turn bridge may have set it
        // between the previous paused check and now, so reading it here prevents
        // the watcher from using a stale current_offset after unpausing.
        if let Some(new_offset) = resume_offset.lock().ok().and_then(|mut g| g.take()) {
            current_offset = new_offset;
            let bridge_delivered_turn = turn_delivered.load(Ordering::Acquire);
            terminal_delivery_observed = watcher_lifecycle_terminal_delivery_observed(
                terminal_delivery_observed,
                bridge_delivered_turn,
            );
            // If the bridge already delivered the previous turn, treat this resume
            // point as already consumed once so the watcher doesn't re-relay the
            // same batch after unpausing.
            last_relayed_offset = if bridge_delivered_turn {
                Some(new_offset)
            } else {
                None
            };
            // #1275 P2 #2: snapshot the current `.generation` mtime alongside
            // the resumed offset. Without this, the local mtime baseline stays
            // at whatever the previous setter left it (often `None` for
            // restored offsets that haven't gone through a relay/rotation
            // cycle yet). A later same-wrapper jsonl rotation would then take
            // the fresh-wrapper branch in `watermark_after_output_regression`,
            // clear `last_relayed_offset`, and re-relay surviving bytes.
            // Pair the mtime with the offset only when we keep the offset (the
            // turn_delivered branch); otherwise the next loop walks from 0
            // anyway and a baseline would be misleading.
            if last_relayed_offset.is_some() {
                last_observed_generation_mtime_ns =
                    Some(read_generation_file_mtime_ns(&tmux_session_name));
            }
            // Clear turn_delivered after preserving the duplicate-relay guard so
            // future turns beyond this resume point can be relayed normally.
            turn_delivered.store(false, Ordering::Relaxed);
        }

        // Check cancel or global shutdown (no "session ended" message). #3277
        // (Defect B): log the stop reason — a silent break here made a
        // replaced incumbent's exit look like an unexplained watcher death.
        if cancel.load(Ordering::Relaxed) || shared.restart.shutting_down.load(Ordering::Relaxed) {
            tracing::info!(
                instance = watcher_instance_id,
                cancel = cancel.load(Ordering::Relaxed),
                shutting_down = shared.restart.shutting_down.load(Ordering::Relaxed),
                "tmux watcher stopping for #{tmux_session_name}: cancelled/shutdown"
            );
            break;
        }

        refresh_watcher_turn_identity(
            &mut watcher_turn_identity,
            &watcher_provider,
            channel_id,
            &tmux_session_name,
        );

        // If paused (Discord handler is processing its own turn), keep the
        // liveness monitor active so a dead pane still clears watcher state.
        if paused.load(Ordering::Relaxed) {
            match tmux_liveness_decision(
                cancel.load(Ordering::Relaxed),
                shared.restart.shutting_down.load(Ordering::Relaxed),
                probe_tmux_session_liveness(&tmux_session_name).await,
            ) {
                TmuxLivenessDecision::Continue => {
                    // #2441 (H1) — graduate the fixed 200ms paused-loop
                    // poll onto the notify-backed JsonlWatcher. A wrapper
                    // write wakes us early; the sleep stays as the upper
                    // bound.
                    sleep_or_jsonl_event(
                        tokio::time::Duration::from_millis(200),
                        &jsonl_notify,
                        &dead_marker_notify,
                    )
                    .await;
                    continue;
                }
                TmuxLivenessDecision::QuietStop => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 👁 tmux session {tmux_session_name} ended during shutdown, exiting quietly"
                    );
                    break;
                }
                TmuxLivenessDecision::TmuxDied => {
                    handle_tmux_watcher_observed_death(
                        channel_id,
                        &http,
                        &shared,
                        &tmux_session_name,
                        &output_path,
                        &watcher_provider,
                        prompt_too_long_killed,
                        watcher_lifecycle_terminal_delivery_observed(
                            terminal_delivery_observed,
                            turn_delivered.load(Ordering::Acquire),
                        ),
                    )
                    .await;
                    break;
                }
            }
        }

        // Periodic size-cap rotation for the session jsonl. Running this off
        // the watcher loop keeps the wrapper child process simple while
        // still enforcing a 20 MB soft cap (see issue #892).
        rotation_tick = rotation_tick.wrapping_add(1);

        if rotation_tick % ROTATION_CHECK_EVERY == 0 {
            let path = output_path.clone();
            let session = tmux_session_name.clone();
            let prev_offset = current_offset;
            let rotation = tokio::task::spawn_blocking(move || {
                crate::services::tmux_common::truncate_jsonl_head_safe(
                    &path,
                    crate::services::tmux_common::JSONL_SIZE_CAP_BYTES,
                    crate::services::tmux_common::JSONL_TARGET_KEEP_BYTES,
                )
                .map_err(|e| e.to_string())
            })
            .await
            .unwrap_or_else(|e| Err(format!("join error: {e}")));
            match rotation {
                Ok(Some(new_size)) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ✂ rotated jsonl for {} — new size {} bytes (was beyond cap)",
                        session,
                        new_size
                    );
                    // File was rewritten from the head: reset reader offset
                    // so the watcher doesn't seek past the new EOF. Also
                    // reset the duplicate-relay guard.
                    if prev_offset > new_size {
                        current_offset = new_size;
                        last_relayed_offset = Some(new_size);
                        // #1270 codex P2: snapshot the current `.generation`
                        // mtime alongside the local offset so a later regression
                        // check has a real baseline. Without this, the local
                        // mtime would still be `None` after a normal relay path
                        // and any subsequent regression would misclassify
                        // same-wrapper rotation as fresh-respawn and clear the
                        // local offset to None — re-relaying surviving content.
                        last_observed_generation_mtime_ns =
                            Some(read_generation_file_mtime_ns(&tmux_session_name));
                        reset_stale_relay_watermark_if_output_regressed(
                            &shared,
                            channel_id,
                            &tmux_session_name,
                            new_size,
                            "jsonl_rotation",
                        );
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!("  [{ts}] ⚠ jsonl rotation failed for {}: {}", session, e);
                }
            }
        }

        // Snapshot pause epoch — if this changes later, a Discord turn claimed this data
        let epoch_snapshot = pause_epoch.load(Ordering::Relaxed);

        // Try to read new data from output file
        let read_result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking({
                let path = output_path.clone();
                let offset = current_offset;
                move || -> Result<(Vec<u8>, u64), String> {
                    let mut file =
                        std::fs::File::open(&path).map_err(|e| format!("open: {}", e))?;
                    file.seek(SeekFrom::Start(offset))
                        .map_err(|e| format!("seek: {}", e))?;
                    let mut buf = vec![0u8; 16384];
                    let n = file.read(&mut buf).map_err(|e| format!("read: {}", e))?;
                    buf.truncate(n);
                    Ok((buf, offset + n as u64))
                }
            }),
        )
        .await;

        let (data, new_offset) = match read_result {
            Ok(Ok(Ok((data, off)))) => (data, off),
            _ => {
                match tmux_liveness_decision(
                    cancel.load(Ordering::Relaxed),
                    shared.restart.shutting_down.load(Ordering::Relaxed),
                    probe_tmux_session_liveness(&tmux_session_name).await,
                ) {
                    TmuxLivenessDecision::Continue => {
                        // #2441 (H1) — notify-backed wake-up for the
                        // initial-read failure retry.
                        sleep_or_jsonl_event(
                            tokio::time::Duration::from_millis(250),
                            &jsonl_notify,
                            &dead_marker_notify,
                        )
                        .await;
                        continue;
                    }
                    TmuxLivenessDecision::QuietStop => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 👁 tmux session {tmux_session_name} ended during shutdown, exiting quietly"
                        );
                        break;
                    }
                    TmuxLivenessDecision::TmuxDied => {
                        handle_tmux_watcher_observed_death(
                            channel_id,
                            &http,
                            &shared,
                            &tmux_session_name,
                            &output_path,
                            &watcher_provider,
                            prompt_too_long_killed,
                            watcher_lifecycle_terminal_delivery_observed(
                                terminal_delivery_observed,
                                turn_delivered.load(Ordering::Acquire),
                            ),
                        )
                        .await;
                        break;
                    }
                }
            }
        };

        let bytes_available = data.len().saturating_add(all_data.len());
        let poll_decision = if bytes_available == 0 {
            watcher_output_poll_decision(
                bytes_available,
                Some(tmux_liveness_decision(
                    cancel.load(Ordering::Relaxed),
                    shared.restart.shutting_down.load(Ordering::Relaxed),
                    probe_tmux_session_liveness(&tmux_session_name).await,
                )),
            )
        } else {
            watcher_output_poll_decision(bytes_available, None)
        };
        match poll_decision {
            WatcherOutputPollDecision::DrainOutput => {}
            WatcherOutputPollDecision::Continue => {
                refresh_watcher_completion_footer_if_due(
                    &http,
                    &shared,
                    channel_id,
                    shared.ui.status_panel_v2_enabled,
                    &mut completion_footer_idle,
                )
                .await;
                // #2441 (H1) — notify-backed wake-up for the
                // poll-decision "wait more" branch.
                sleep_or_jsonl_event(
                    tokio::time::Duration::from_millis(250),
                    &jsonl_notify,
                    &dead_marker_notify,
                )
                .await;
                continue;
            }
            WatcherOutputPollDecision::QuietStop => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 👁 tmux session {tmux_session_name} ended during shutdown, exiting quietly"
                );
                break;
            }
            WatcherOutputPollDecision::TmuxDied => {
                handle_tmux_watcher_observed_death(
                    channel_id,
                    &http,
                    &shared,
                    &tmux_session_name,
                    &output_path,
                    &watcher_provider,
                    prompt_too_long_killed,
                    watcher_lifecycle_terminal_delivery_observed(
                        terminal_delivery_observed,
                        turn_delivered.load(Ordering::Acquire),
                    ),
                )
                .await;
                break;
            }
        }

        // We got new data while not paused — this means terminal input triggered a response
        let data_start_offset = current_offset; // offset where this read batch started
        current_offset = new_offset;
        // #3956: re-stamp the submit prompt anchor on this observed streaming output
        // so a turn streaming continuously past PROMPT_ANCHOR_SUBMIT_TTL (4h) keeps a
        // live anchor for the #3885 same-input follow-up-requeue peek (no duplicate
        // prose). No-op unless an anchor already exists for THIS channel; the helper
        // touches only the submit anchor and never the #3459/#3303 relayed-entry
        // ledger (its own decoupled 30min TTL). Refresh-on-activity, not a lifecycle.
        crate::services::tui_prompt_dedupe::touch_prompt_anchor_on_activity(
            watcher_provider.as_str(),
            &tmux_session_name,
            channel_id.get(),
        );
        // #1137: surface a single warning when output keeps arriving after a
        // terminal-success relay. The watcher will keep running (the legacy
        // single-event exit was the bug); this log makes the continuation
        // observable in the operational timeline.
        if turn_result_relayed && !post_terminal_continuation_logged {
            post_terminal_continuation_logged = true;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 👁 post-terminal-success continuation: new output arrived for {tmux_session_name} after terminal success (offset {data_start_offset} -> {new_offset}); watcher staying alive"
            );
        }
        // Compute the SSH-direct bypass signal lazily — the dedupe state
        // lookup grabs a global Mutex and walks the purge maps, so we only
        // pay that cost when the cheap (terminal + no-inflight) prefix is
        // already true and we are about to suppress.
        let post_terminal_inflight_missing =
            crate::services::discord::inflight::load_inflight_state(
                &watcher_provider,
                channel_id.get(),
            )
            .is_none();
        let runtime_kind_marker = if turn_result_relayed && post_terminal_inflight_missing {
            crate::services::tmux_common::resolve_tmux_runtime_kind_marker(&tmux_session_name)
        } else {
            None
        };
        if matches!(
            runtime_kind_marker,
            Some(crate::services::agent_protocol::RuntimeHandoffKind::LegacyTmuxWrapper)
        ) && watcher_batch_contains_relayable_response(&data)
        {
            let _ = observe_legacy_wrapper_direct_prompt_from_pane(
                &watcher_provider,
                &tmux_session_name,
                channel_id,
                data_start_offset,
                current_offset,
            );
        }
        let ssh_direct_prompt_pending = if turn_result_relayed && post_terminal_inflight_missing {
            crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
                watcher_provider.as_str(),
                &tmux_session_name,
                channel_id.get(),
            )
            .is_some()
                || crate::services::tui_prompt_dedupe::is_ssh_direct_observation_pending(
                    watcher_provider.as_str(),
                    &tmux_session_name,
                )
        } else {
            false
        };
        let external_input_lease_present = if turn_result_relayed && post_terminal_inflight_missing
        {
            crate::services::tui_prompt_dedupe::external_input_relay_lease_present(
                watcher_provider.as_str(),
                &tmux_session_name,
                channel_id.get(),
            )
        } else {
            false
        };
        let post_terminal_payload_allows_external_relay =
            if turn_result_relayed && post_terminal_inflight_missing {
                let mut post_terminal_payload = String::with_capacity(all_data.len() + data.len());
                post_terminal_payload.push_str(&all_data);
                post_terminal_payload.push_str(&String::from_utf8_lossy(&data));
                post_terminal_jsonl_payload_contains_init_without_user_event(
                    post_terminal_payload.as_bytes(),
                )
            } else {
                false
            };
        // #3107: lazy pane-busy probe — capture the pane only when the cheap
        // (terminal + no-inflight) prefix already holds (keeps `tmux capture-pane` off the hot path).
        let post_terminal_pane_actively_streaming = turn_result_relayed
            && post_terminal_inflight_missing
            && watcher_pane_actively_streaming(&tmux_session_name);
        if post_terminal_pane_actively_streaming {
            // Self-heal: a live turn lost its inflight but kept streaming post-terminal;
            // re-establish a watcher-owned inflight (reusing the restored turn's persisted ids).
            let restored_panel = restored_turn
                .as_ref()
                .and_then(|turn| turn.status_message_id);
            let restored_placeholder = restored_turn
                .as_ref()
                .and_then(|turn| (turn.current_msg_id.get() != 0).then_some(turn.current_msg_id));
            let reacquired = reacquire_watcher_inflight_for_active_stream(
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                &output_path,
                data_start_offset,
                restored_panel,
                restored_placeholder,
                restored_injected_prompt_message_id,
            );
            if reacquired && !active_stream_inflight_reacquire_logged {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] 🩹 watcher: re-acquired watcher-owned inflight for actively-streaming pane after post-terminal output without inflight (channel {}, tmux={}, range {}..{})",
                    channel_id.get(),
                    tmux_session_name,
                    data_start_offset,
                    current_offset
                );
                active_stream_inflight_reacquire_logged = true;
            }
        }
        // #3154: a deferred synthetic turn-start pending for this channel means
        // the per-channel worker has not yet saved the matching inflight; keep
        // the bytes buffered (do NOT suppress / advance confirmed offset) so the
        // wakeup turn's response batch survives the wait window.
        let pending_synthetic_start_present = post_terminal_inflight_missing
            && crate::services::discord::tui_direct_pending_start::pending_synthetic_start_present(
                watcher_provider.as_str(),
                channel_id.get(),
            );
        let post_terminal_no_inflight_should_suppress =
            should_suppress_post_terminal_output_without_inflight(
                turn_result_relayed,
                post_terminal_inflight_missing,
                ssh_direct_prompt_pending,
                external_input_lease_present,
                watcher_batch_contains_assistant_event(&data),
                post_terminal_pane_actively_streaming,
                pending_synthetic_start_present,
            ) && !post_terminal_payload_allows_external_relay;
        if post_terminal_payload_allows_external_relay {
            tracing::info!(
                channel = channel_id.get(),
                tmux_session = %tmux_session_name,
                range_start = data_start_offset,
                range_end = current_offset,
                "watcher allowed post-terminal no-inflight JSONL init payload for external relay"
            );
        }
        if post_terminal_no_inflight_should_suppress {
            let suppressed_range = (data_start_offset, current_offset);
            if last_post_terminal_suppressed_range != Some(suppressed_range) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] 🛑 watcher: suppressed post-terminal output without inflight for channel {} (tmux={}, range {}..{})",
                    channel_id.get(),
                    tmux_session_name,
                    data_start_offset,
                    current_offset
                );
                last_post_terminal_suppressed_range = Some(suppressed_range);
            } else {
                tracing::debug!(
                    channel_id = channel_id.get(),
                    tmux_session = %tmux_session_name,
                    range_start = data_start_offset,
                    range_end = current_offset,
                    "watcher: repeated post-terminal suppress for same range"
                );
            }
            last_relayed_offset = Some(current_offset);
            last_observed_generation_mtime_ns =
                Some(read_generation_file_mtime_ns(&tmux_session_name));
            advance_watcher_confirmed_end(
                &shared,
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                current_offset,
                "src/services/discord/tmux.rs:post_terminal_no_inflight_suppressed_output",
            );
            // #3053: suppressing post-terminal output is NOT idleness — the
            // wrapper is still alive and producing JSONL. The original code
            // `continue`d here before reaching the heartbeat refresh below, so
            // a live TUI session that only ever emitted post-terminal output
            // (e.g. provider selector continuation) never refreshed its
            // idle-kill heartbeat and was killed as "idle". Touch it here too.
            touch_session_activity(
                None::<&crate::db::Db>,
                shared.pg_pool.as_ref(),
                &shared.token_hash,
                &watcher_provider,
                &tmux_session_name,
                watcher_thread_channel_id,
                "post_terminal_suppressed_output_while_tmux_alive",
                "tmux_watcher.rs:post_terminal_no_inflight_suppressed_output",
            );
            utf8_decoder.clear_pending();
            continue;
        }
        maybe_refresh_watcher_activity_heartbeat(
            None::<&crate::db::Db>,
            shared.pg_pool.as_ref(),
            &shared.token_hash,
            &watcher_provider,
            &tmux_session_name,
            watcher_thread_channel_id,
            &mut last_activity_heartbeat_at,
        );

        // Collect the full turn: keep reading until we see a "result" event.
        // #1216: append to the outer-scope `all_data` so any leftover from a
        // previous iteration (multi-turn buffer split at the first `result`)
        // is processed before the new disk read.
        let decoded_data = utf8_decoder.decode(&data, data_start_offset);
        // #3041 P1-3 (Part a, B1): the forward of this outer-read chunk is
        // DEFERRED until AFTER `process_watcher_lines` below so the result-bearing
        // chunk can ride a TERMINAL frame carrying the commit fence. Set only the
        // buffer START offset here (independent of the forward); the mirror flags +
        // ack target are set from the deferred forward result (see the
        // `data_mirrored_to_session_relay` binding after the initial parse).
        let initial_buffer_was_empty = all_data.is_empty();
        if initial_buffer_was_empty {
            all_data_start_offset = decoded_data.start_offset.unwrap_or(data_start_offset);
        }
        if decoded_data.text.is_empty() && all_data.is_empty() {
            continue;
        }
        all_data.push_str(&decoded_data.text);
        let turn_data_start_offset = all_data_start_offset;
        // #3041 P1-3 R7: reset carried ACKs after terminal/next-turn splits so later turns cannot inherit them and black-hole.
        let mut split_trailing_turn_follows = false;
        let mut state = StreamLineState::new();
        let restored_turn_seed = restored_turn.take();
        let restored_seed_undelivered_body_len = restored_turn_seed
            .as_ref()
            .and_then(|seed| seed.full_response.get(seed.response_sent_offset..))
            .map(|body| body.trim().chars().count())
            .unwrap_or(0);
        let restored_seed_has_body = restored_seed_undelivered_body_len > 0;
        let prompt_anchor_present_for_seed_discard =
            crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
                watcher_provider.as_str(),
                &tmux_session_name,
                channel_id.get(),
            )
            .is_some();
        let discard_restored_seed = should_discard_restored_seed_for_idle_direct_prompt(
            restored_turn_seed.is_some(),
            prompt_anchor_present_for_seed_discard,
            restored_seed_has_body,
        );
        if !discard_restored_seed
            && prompt_anchor_present_for_seed_discard
            && restored_seed_has_body
        {
            tracing::info!(
                channel = channel_id.get(),
                body_len = restored_seed_undelivered_body_len,
                tmux_session = %tmux_session_name,
                "watcher: preserving restored stream seed with undelivered body for idle SSH-direct prompt"
            );
        }
        if discard_restored_seed {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 watcher: discarding restored stream seed for idle SSH-direct prompt on channel {} (tmux={})",
                channel_id.get(),
                tmux_session_name
            );
        }
        let stream_seed = watcher_stream_seed(if discard_restored_seed {
            None
        } else {
            restored_turn_seed
        });
        let restored_response_seed = stream_seed.full_response.clone();
        let restored_assistant_text_seen = !restored_response_seed.trim().is_empty();
        // #3041 P1-3 (Part a, B1): the `restored_assistant_text_seen` →
        // "not fully mirrored" reset is now applied where
        // `session_bound_relay_turn_fully_mirrored` is DECLARED (after the deferred
        // initial forward below). A restored response prefix came from watcher
        // state, not from chunks mirrored into the session-bound StreamRelay
        // parser, so the legacy watcher delivery owner keeps this terminal envelope
        // (we do not delegate a partial response).
        let mut full_response = stream_seed.full_response;
        let mut tool_state = WatcherToolState::new();

        // Create a placeholder message for real-time status display
        const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let mut spin_idx: usize = 0;
        let mut placeholder_msg_id: Option<serenity::MessageId> = stream_seed.placeholder_msg_id;
        let mut placeholder_from_restored_inflight = placeholder_msg_id.is_some();
        let mut status_panel_msg_id: Option<serenity::MessageId> = stream_seed.status_panel_msg_id;
        let single_message_panel_footer_mode =
            watcher_single_message_panel_footer_enabled(shared.ui.status_panel_v2_enabled);
        if single_message_panel_footer_mode {
            status_panel_msg_id = None;
        }
        // #3003 (codex P2 r4): cache whether this turn is a TUI-direct
        // external-input turn while the inflight row is still present, so the
        // orphan-panel reclaim can run after a stop/cancel clears inflight.
        let startup_inflight_snapshot = crate::services::discord::inflight::load_inflight_state(
            &watcher_provider,
            channel_id.get(),
        );
        // #3805 P2 (PR-C): this turn's status-panel generation epoch, SEEDED from
        // the on-disk row so a restart re-hydrating an existing panel carries the
        // SAME epoch it was created with (a stale-epoch completion is thus never
        // falsely skipped). The two-message create bumps it (opens the epoch on a
        // fresh bind); the completion guard proves it against the on-disk epoch.
        // Inert on the default-OFF path (stays 0) and while no mid-turn re-anchor
        // exists yet (PR-D) — this turn's epoch always equals the on-disk epoch.
        let mut this_turn_status_panel_generation: u64 = startup_inflight_snapshot
            .as_ref()
            .map(|state| state.status_panel_generation)
            .unwrap_or(0);
        // status-panel-v2: panel eligibility (external-input OR synthetic
        // monitor/self-paced-loop) drives the panel-lifecycle sites that read
        // this flag. The lease/⏳-anchor sites keep the narrower external-input
        // predicate and are untouched.
        let mut turn_is_external_input_for_session = watcher_inflight_is_panel_eligible_for_session(
            startup_inflight_snapshot.as_ref(),
            &tmux_session_name,
        );
        // #3003 (codex P2 r11): snapshot this turn's identity so the abandon check
        // can treat a *replaced* inflight (a new turn on the same channel) as
        // abandoned, not just a missing one. user_msg_id is 0 for external input,
        // so `started_at` is the discriminator between consecutive TUI-direct turns.
        let mut turn_identity_for_panel = startup_inflight_snapshot
            .as_ref()
            .filter(|state| state.tmux_session_name.as_deref() == Some(tmux_session_name.as_str()))
            .map(crate::services::discord::inflight::InflightTurnIdentity::from_state);
        let (status_panel_started_at, footer_owner) =
            make_owner_now(turn_identity_for_panel.as_ref());
        // #3003 P2: rehydrate a watcher-owned persisted panel id while the row
        // still exists; footer mode intentionally has no separate panel handle.
        if !single_message_panel_footer_mode
            && status_panel_msg_id.is_none()
            && turn_is_external_input_for_session
        {
            status_panel_msg_id = watcher_persisted_status_panel_msg_id(
                startup_inflight_snapshot.as_ref(),
                &tmux_session_name,
            );
        }
        // #3003 P2: reset per-channel live-status state on a genuinely fresh
        // watcher frame. This is deliberately not gated on external-input because
        // the inflight row may not exist yet; restored/bridge-owned frames are
        // excluded by the seed guards.
        let watcher_fresh_turn_frame = placeholder_msg_id.is_none()
            && status_panel_msg_id.is_none()
            && !restored_assistant_text_seen;
        if watcher_fresh_turn_frame
            && (shared.ui.placeholder_live_events_enabled || shared.ui.status_panel_v2_enabled)
        {
            if single_message_panel_footer_mode {
                supersede_watcher_footer(&http, &shared, channel_id, footer_owner).await;
                shared
                    .ui
                    .placeholder_live_events
                    .clear_channel_preserving_footer_residuals(channel_id);
            } else {
                shared.ui.placeholder_live_events.clear_channel(channel_id);
            }
        }
        let mut last_status_panel_text = String::new();
        let mut last_edit_text = stream_seed.last_edit_text;
        let mut response_sent_offset = stream_seed.response_sent_offset;
        // #3871: ids of streamed rollover prefixes frozen for this turn; deleted on a
        // terminal full-body fallback so the frozen prose is not duplicated (sink parity).
        // SEEDED from the persisted row so prefixes frozen in an earlier `'watcher_loop`
        // iteration / before a watcher restart survive to the fallback (no residual dup).
        let mut watcher_streaming_rollover_frozen_msg_ids: Vec<serenity::MessageId> =
            stream_seed.streaming_rollover_frozen_msg_ids.clone();
        let finish_mailbox_on_completion = stream_seed.finish_mailbox_on_completion;
        let mut monitor_auto_turn_claimed = false;
        let mut monitor_auto_turn_deferred = false;
        let mut monitor_auto_turn_finished = false;
        let mut completion_footer_terminal_target = None;
        // #3016 P1: the synthetic mailbox message id + process-monotonic ledger
        // generation the active monitor turn started under, threaded to
        // `finish_monitor_auto_turn_if_claimed` so it finalizes the EXACT monitor
        // turn (distinct ledger entries for sequential monitor turns even when
        // the byte-offset-derived synthetic id repeats after a wrapper respawn).
        let mut monitor_auto_turn_synthetic_msg_id: Option<MessageId> = None;
        let mut monitor_auto_turn_ledger_generation: Option<u64> = None;
        // #1009: 1-shot tracker for the monitor-auto-turn preamble hint so the
        // hint text is emitted exactly once per watcher turn frame.
        let mut monitor_auto_turn_preamble_injected = false;
        // Process any complete lines we already have
        let initial_buffer_len = all_data.len();
        observe_qwen_user_prompts_in_buffer(&all_data, &watcher_provider, &tmux_session_name);
        let initial_outcome = process_watcher_lines(
            &mut all_data,
            &mut state,
            &mut full_response,
            &mut tool_state,
        );
        // #3041 P1-3 (Part a, B1): DEFERRED forward of the outer-read chunk. We now
        // know — from `initial_outcome.found_result` — whether THIS chunk is the
        // RESULT-bearing (terminal) one. If so, forward it as a TERMINAL frame
        // carrying the commit fence (`terminal_event_consumed_offset(..)` + the
        // pinned turn identity loaded at turn start), so the SAME frame that
        // triggers the sink's terminal delivery carries the consumed_end + identity
        // (FIFO single-task: a separate later frame would arrive after the sink
        // already dispatched). Non-terminal chunks forward exactly as before (no
        // fence, no streaming-latency change beyond the synchronous parse reorder).
        // The ACK target is captured from THIS forward, so the watcher's wait now
        // correlates to the terminal frame's sequence (more precise).
        let initial_terminal_fence = watcher_terminal_commit_fence(
            initial_outcome.found_result,
            turn_data_start_offset,
            terminal_event_consumed_offset(current_offset, &all_data),
            turn_identity_for_panel.as_ref(),
            &tmux_session_name,
        );
        let data_mirrored_to_session_relay = match initial_terminal_fence {
            // #3041 P1-3 (codex P1-3 issue 1): a single physical chunk may carry
            // turn A's result PLUS turn B's first bytes. `all_data` after the parse
            // holds turn B's leftover; split the decoded chunk at that boundary so
            // the TERMINAL frame carries only turn A's bytes and turn B's tail rides
            // a separate non-terminal frame (no black-hole, no shared-ACK reuse).
            Some(fence) => forward_terminal_chunk_with_trailing_to_supervisor_relay(
                &tmux_session_name,
                &decoded_data.text,
                all_data.len(),
                &producer_registry,
                &mut cached_relay_producer,
                fence,
            ),
            None => forward_chunk_to_supervisor_relay_for_turn(
                &tmux_session_name,
                &decoded_data.text,
                &producer_registry,
                &mut cached_relay_producer,
                turn_identity_for_panel.as_ref(),
            ),
        };
        let supervisor_turn_state = apply_initial_supervisor_relay_forward(
            &mut all_data_fully_mirrored_to_session_relay,
            &mut all_data_session_bound_relay_ack,
            &mut all_data_first_forwarded_relay_sequence,
            &mut split_trailing_turn_follows,
            &data_mirrored_to_session_relay,
            initial_buffer_was_empty,
            all_data.is_empty(),
            restored_assistant_text_seen,
            turn_identity_for_panel.as_ref(),
        );
        let mut session_bound_relay_turn_fully_mirrored = supervisor_turn_state.fully_mirrored;
        let mut session_bound_relay_turn_first_forwarded_sequence =
            supervisor_turn_state.first_forwarded_sequence;
        all_data_start_offset =
            advance_buffer_start_offset(turn_data_start_offset, initial_buffer_len, all_data.len());
        let live_events_dirty = flush_placeholder_live_events(&shared, channel_id, &mut tool_state);
        let mut found_result = initial_outcome.found_result;
        let mut terminal_kind = initial_outcome.terminal_kind;
        let mut soft_terminal_seen_at = if initial_outcome.soft_terminal_candidate {
            Some(tokio::time::Instant::now())
        } else {
            None
        };
        let mut is_prompt_too_long = initial_outcome.is_prompt_too_long;
        let mut is_auth_error = initial_outcome.is_auth_error;
        let mut auth_error_message = initial_outcome.auth_error_message;
        let mut is_provider_overloaded = initial_outcome.is_provider_overloaded;
        let mut provider_overload_message = initial_outcome.provider_overload_message;
        let mut stale_resume_detected = initial_outcome.stale_resume_detected;
        let mut auto_compaction_lifecycle_attempted = false;
        let mut task_notification_kind = stream_seed.task_notification_kind;
        let mut assistant_text_seen =
            restored_assistant_text_seen || initial_outcome.assistant_text_seen;
        let mut fresh_assistant_text_seen = initial_outcome.assistant_text_seen;
        if let Some(kind) = initial_outcome.task_notification_kind {
            task_notification_kind = merge_task_notification_kind(task_notification_kind, kind);
        }
        if initial_outcome.auto_compacted {
            auto_compaction_lifecycle_attempted = emit_context_compacted_lifecycle_from_watcher(
                &shared,
                channel_id,
                &watcher_provider,
                state.last_model.as_deref(),
                stream_line_state_token_usage(&state),
            )
            .await;
        }
        let post_terminal_success_continuation_flush =
            should_flush_post_terminal_success_continuation(
                turn_result_relayed,
                found_result,
                current_offset > data_start_offset,
                &full_response,
            );
        if post_terminal_success_continuation_flush {
            found_result = true;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 👁 post-terminal-success continuation: flushing relayed output for {tmux_session_name} immediately (offset {data_start_offset} -> {current_offset})"
            );
        }
        if matches!(
            task_notification_kind,
            Some(TaskNotificationKind::MonitorAutoTurn)
        ) {
            let start = start_monitor_auto_turn_when_available(
                &shared,
                &watcher_provider,
                channel_id,
                data_start_offset,
                cancel.as_ref(),
            )
            .await;
            monitor_auto_turn_claimed = start.acquired;
            monitor_auto_turn_deferred = monitor_auto_turn_deferred || start.deferred;
            if start.acquired {
                monitor_auto_turn_synthetic_msg_id = start.synthetic_message_id;
                monitor_auto_turn_ledger_generation = start.ledger_generation;
            }
            if !start.acquired {
                all_data.clear();
                all_data_start_offset = current_offset;
                all_data_fully_mirrored_to_session_relay = true;
                all_data_session_bound_relay_ack = None;
                all_data_first_forwarded_relay_sequence = None;
                continue;
            }
            ensure_monitor_auto_turn_inflight(
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                &output_path,
                &input_fifo_path,
                state.last_session_id.as_deref(),
                data_start_offset,
                current_offset,
            );
            if let Some(hint) =
                consume_monitor_auto_turn_preamble_once(&mut monitor_auto_turn_preamble_injected)
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔔 monitor auto-turn preamble hint injected (channel {}): {}",
                    channel_id.get(),
                    hint
                );
            }
        }

        // Keep reading until result or timeout
        // Check if a Discord turn claimed this data since our epoch snapshot
        let epoch_changed = pause_epoch.load(Ordering::Relaxed) != epoch_snapshot;
        let mut was_paused = paused.load(Ordering::Relaxed) || epoch_changed;
        if was_paused && !monitor_auto_turn_deferred {
            // A Discord turn took over — discard what we read
            all_data.clear();
            all_data_start_offset = current_offset;
            all_data_fully_mirrored_to_session_relay = true;
            all_data_session_bound_relay_ack = None;
            all_data_first_forwarded_relay_sequence = None;
            continue;
        }
        if !found_result {
            let turn_start = tokio::time::Instant::now();
            let turn_timeout = crate::services::discord::turn_watchdog_timeout();
            let turn_idle_timeout = crate::services::discord::turn_idle_timeout();
            let mut last_status_update = tokio::time::Instant::now();
            let mut last_output_at = tokio::time::Instant::now();
            if watcher_live_events_dirty_should_force_status_update(
                live_events_dirty,
                single_message_panel_footer_mode,
            ) {
                force_next_watcher_status_update(&mut last_status_update);
            }
            let mut ready_for_input_tracker =
                crate::services::provider::ReadyForInputIdleTracker::default();
            let mut last_ready_probe_at: Option<std::time::Instant> = None;
            let mut last_liveness_probe_at = tokio::time::Instant::now();
            let mut tmux_death_observed = false;
            let mut ready_for_input_failure_notice: Option<String> = None;
            let mut ready_for_input_stall_dispatch_id: Option<String> = None;
            let mut ready_for_input_stall_inflight_snapshot: Option<
                crate::services::discord::inflight::InflightTurnState,
            > = None;
            let mut streaming_suppressed_by_recent_stop = false;
            let mut streaming_suppressed_by_missing_inflight = false;
            let mut fresh_ready_for_input_idle = false;

            // #3419 B: read while ACTIVE — a real byte within the IDLE window
            // (`last_output_at` advances only on a non-empty read) under a generous
            // cap; shared predicate with the finalize gate (single authority).
            while !found_result
                && watcher_turn_still_active(
                    last_output_at.elapsed(),
                    turn_idle_timeout,
                    turn_start.elapsed(),
                    turn_timeout,
                )
            {
                // Loop can wait minutes for a long tool/test; keep the registry heartbeat
                // fresh so the sweeper does not cancel relay on a healthy streaming watcher.
                last_heartbeat_ts_ms.store(
                    crate::services::discord::tmux_watcher_now_ms(),
                    std::sync::atomic::Ordering::Release,
                );
                if cancel.load(Ordering::Relaxed)
                    || shared.restart.shutting_down.load(Ordering::Relaxed)
                {
                    break;
                }
                if paused.load(Ordering::Relaxed) {
                    was_paused = true;
                    break;
                }

                let read_more = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    tokio::task::spawn_blocking({
                        let path = output_path.clone();
                        let offset = current_offset;
                        move || -> Result<(Vec<u8>, u64), String> {
                            let mut file =
                                std::fs::File::open(&path).map_err(|e| format!("open: {}", e))?;
                            file.seek(SeekFrom::Start(offset))
                                .map_err(|e| format!("seek: {}", e))?;
                            let mut buf = vec![0u8; 16384];
                            let n = file.read(&mut buf).map_err(|e| format!("read: {}", e))?;
                            buf.truncate(n);
                            Ok((buf, offset + n as u64))
                        }
                    }),
                )
                .await;

                match read_more {
                    Ok(Ok(Ok((chunk, off)))) if !chunk.is_empty() => {
                        current_offset = off;
                        maybe_refresh_watcher_activity_heartbeat(
                            None::<&crate::db::Db>,
                            shared.pg_pool.as_ref(),
                            &shared.token_hash,
                            &watcher_provider,
                            &tmux_session_name,
                            watcher_thread_channel_id,
                            &mut last_activity_heartbeat_at,
                        );
                        ready_for_input_tracker.record_output();
                        let chunk_start_offset = current_offset.saturating_sub(chunk.len() as u64);
                        let decoded_chunk = utf8_decoder.decode(&chunk, chunk_start_offset);
                        // #3041 P1-3 (Part a, B1): DEFER the forward until AFTER the
                        // parse so the RESULT-bearing streaming chunk rides a TERMINAL
                        // frame carrying the commit fence. Set only the buffer START
                        // offset here (independent of the forward).
                        let chunk_buffer_was_empty = all_data.is_empty();
                        if chunk_buffer_was_empty {
                            all_data_start_offset =
                                decoded_chunk.start_offset.unwrap_or(chunk_start_offset);
                        }
                        if decoded_chunk.text.is_empty() && all_data.is_empty() {
                            continue;
                        }
                        all_data.push_str(&decoded_chunk.text);
                        let chunk_buffer_start_offset = all_data_start_offset;
                        let chunk_buffer_len = all_data.len();
                        observe_qwen_user_prompts_in_buffer(
                            &all_data,
                            &watcher_provider,
                            &tmux_session_name,
                        );
                        let outcome = process_watcher_lines(
                            &mut all_data,
                            &mut state,
                            &mut full_response,
                            &mut tool_state,
                        );
                        // #3041 P1-3 (Part a, B1): deferred forward of THIS streaming
                        // chunk. `outcome.found_result` now tells us whether this is
                        // the RESULT-bearing chunk; if so it rides a TERMINAL frame
                        // carrying the commit fence (consumed_end + pinned identity).
                        // E5 (#2412): every decoded chunk is still pushed into the
                        // relay MPSC; only the terminality of the frame changed.
                        let streaming_terminal_fence = watcher_terminal_commit_fence(
                            outcome.found_result,
                            chunk_buffer_start_offset,
                            terminal_event_consumed_offset(current_offset, &all_data),
                            turn_identity_for_panel.as_ref(),
                            &tmux_session_name,
                        );
                        let chunk_forwarded_to_session_relay = match streaming_terminal_fence {
                            // #3041 P1-3 (codex P1-3 issue 1): split a result+next-turn
                            // physical chunk at the leftover boundary so turn A's
                            // terminal frame carries only A's bytes and turn B's tail
                            // rides a separate non-terminal frame (no black-hole).
                            Some(fence) => {
                                forward_terminal_chunk_with_trailing_to_supervisor_relay(
                                    &tmux_session_name,
                                    &decoded_chunk.text,
                                    all_data.len(),
                                    &producer_registry,
                                    &mut cached_relay_producer,
                                    fence,
                                )
                            }
                            None => forward_chunk_to_supervisor_relay_for_turn(
                                &tmux_session_name,
                                &decoded_chunk.text,
                                &producer_registry,
                                &mut cached_relay_producer,
                                turn_identity_for_panel.as_ref(),
                            ),
                        };
                        apply_streaming_supervisor_relay_forward(
                            &mut all_data_fully_mirrored_to_session_relay,
                            &mut all_data_session_bound_relay_ack,
                            &mut all_data_first_forwarded_relay_sequence,
                            &mut session_bound_relay_turn_fully_mirrored,
                            &mut session_bound_relay_turn_first_forwarded_sequence,
                            &mut split_trailing_turn_follows,
                            &chunk_forwarded_to_session_relay,
                            chunk_buffer_was_empty,
                            all_data.is_empty(),
                            turn_identity_for_panel.as_ref(),
                        );
                        last_output_at = tokio::time::Instant::now();
                        all_data_start_offset = advance_buffer_start_offset(
                            chunk_buffer_start_offset,
                            chunk_buffer_len,
                            all_data.len(),
                        );
                        if watcher_live_events_dirty_should_force_status_update(
                            flush_placeholder_live_events(&shared, channel_id, &mut tool_state),
                            single_message_panel_footer_mode,
                        ) {
                            force_next_watcher_status_update(&mut last_status_update);
                        }
                        found_result = found_result || outcome.found_result;
                        if outcome.found_result {
                            terminal_kind = outcome.terminal_kind.or(terminal_kind);
                        }
                        if outcome.soft_terminal_candidate && soft_terminal_seen_at.is_none() {
                            soft_terminal_seen_at = Some(tokio::time::Instant::now());
                            terminal_kind = outcome
                                .terminal_kind
                                .or(terminal_kind)
                                .or(Some(WatcherTerminalKind::SoftStopHookSummary));
                        }
                        is_prompt_too_long = is_prompt_too_long || outcome.is_prompt_too_long;
                        is_auth_error = is_auth_error || outcome.is_auth_error;
                        if auth_error_message.is_none() {
                            auth_error_message = outcome.auth_error_message;
                        }
                        is_provider_overloaded =
                            is_provider_overloaded || outcome.is_provider_overloaded;
                        stale_resume_detected =
                            stale_resume_detected || outcome.stale_resume_detected;
                        if let Some(kind) = outcome.task_notification_kind {
                            task_notification_kind =
                                merge_task_notification_kind(task_notification_kind, kind);
                        }
                        assistant_text_seen |= outcome.assistant_text_seen;
                        fresh_assistant_text_seen |= outcome.assistant_text_seen;
                        if matches!(
                            task_notification_kind,
                            Some(TaskNotificationKind::MonitorAutoTurn)
                        ) {
                            if !monitor_auto_turn_claimed {
                                let start = start_monitor_auto_turn_when_available(
                                    &shared,
                                    &watcher_provider,
                                    channel_id,
                                    data_start_offset,
                                    cancel.as_ref(),
                                )
                                .await;
                                monitor_auto_turn_claimed = start.acquired;
                                monitor_auto_turn_deferred =
                                    monitor_auto_turn_deferred || start.deferred;
                                if start.acquired {
                                    monitor_auto_turn_synthetic_msg_id = start.synthetic_message_id;
                                    monitor_auto_turn_ledger_generation = start.ledger_generation;
                                }
                                if !start.acquired {
                                    was_paused = true;
                                    break;
                                }
                            }
                            ensure_monitor_auto_turn_inflight(
                                &watcher_provider,
                                channel_id,
                                &tmux_session_name,
                                &output_path,
                                &input_fifo_path,
                                state.last_session_id.as_deref(),
                                data_start_offset,
                                current_offset,
                            );
                            if let Some(hint) = consume_monitor_auto_turn_preamble_once(
                                &mut monitor_auto_turn_preamble_injected,
                            ) {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!(
                                    "  [{ts}] 🔔 monitor auto-turn preamble hint injected (channel {}): {}",
                                    channel_id.get(),
                                    hint
                                );
                            }
                        }
                        if provider_overload_message.is_none() {
                            provider_overload_message = outcome.provider_overload_message;
                        }
                        if outcome.auto_compacted && !auto_compaction_lifecycle_attempted {
                            auto_compaction_lifecycle_attempted =
                                emit_context_compacted_lifecycle_from_watcher(
                                    &shared,
                                    channel_id,
                                    &watcher_provider,
                                    state.last_model.as_deref(),
                                    stream_line_state_token_usage(&state),
                                )
                                .await;
                        }
                    }
                    Ok(Ok(Ok((_, off)))) => {
                        current_offset = off;
                        if should_probe_tmux_liveness(
                            last_liveness_probe_at.elapsed(),
                            tmux_dead_marker_exists(&tmux_session_name),
                        ) {
                            last_liveness_probe_at = tokio::time::Instant::now();
                            match watcher_output_poll_decision(
                                0,
                                Some(tmux_liveness_decision(
                                    cancel.load(Ordering::Relaxed),
                                    shared.restart.shutting_down.load(Ordering::Relaxed),
                                    probe_tmux_session_liveness(&tmux_session_name).await,
                                )),
                            ) {
                                WatcherOutputPollDecision::DrainOutput => {}
                                WatcherOutputPollDecision::Continue => {}
                                WatcherOutputPollDecision::QuietStop => break,
                                WatcherOutputPollDecision::TmuxDied => {
                                    tmux_death_observed = true;
                                    break;
                                }
                            }
                        }
                        // #2441 (H1) — notify-backed wake-up for the
                        // "no new bytes, waiting for more" tail of the
                        // inner streaming loop. A wrapper write wakes us
                        // immediately; the sleep stays as the upper
                        // bound.
                        sleep_or_jsonl_event(
                            tokio::time::Duration::from_millis(200),
                            &jsonl_notify,
                            &dead_marker_notify,
                        )
                        .await;
                        let now = std::time::Instant::now();
                        // #2442 (H3) — wrapper emits a `ready_for_input` JSONL
                        // sentinel on transitioning back to accepting stdin; seeing
                        // it in the tail bytes is a free readiness signal that
                        // short-circuits the 2s probe cadence (legacy
                        // `should_probe_ready` stays a SIGKILL/sentinel-lost fallback).
                        // Claude TUI is transcript-backed (composer can stay on-screen
                        // during work) so completion uses JSONL turn state, not chrome.
                        let sentinel_ready =
                            !matches!(
                                watcher_provider,
                                crate::services::provider::ProviderKind::Claude
                            ) && jsonl_tail_contains_ready_for_input_sentinel(&output_path);
                        let should_probe_ready = sentinel_ready
                            || last_ready_probe_at
                                .map(|last| {
                                    now.duration_since(last) >= READY_FOR_INPUT_IDLE_PROBE_INTERVAL
                                })
                                .unwrap_or(true);
                        if should_probe_ready {
                            last_ready_probe_at = Some(now);
                            let ready_for_input = if sentinel_ready {
                                true
                            } else {
                                tokio::time::timeout(
                                    std::time::Duration::from_secs(5),
                                    tokio::task::spawn_blocking({
                                        let name = tmux_session_name.clone();
                                        let provider = watcher_provider.clone();
                                        let path = output_path.clone();
                                        let offset = current_offset;
                                        move || {
                                            watcher_session_ready_for_input(
                                                &name, &provider, &path, offset,
                                            )
                                        }
                                    }),
                                )
                                .await
                                .unwrap_or(Ok(false))
                                .unwrap_or(false)
                            };
                            if soft_terminal_seen_at.is_some()
                                && ready_for_input
                                && !full_response.trim().is_empty()
                            {
                                terminal_kind
                                    .get_or_insert(WatcherTerminalKind::SoftStopHookSummary);
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!(
                                    "  [{ts}] 👁 watcher committed soft stop_hook_summary after ready-for-input for {tmux_session_name} at offset {current_offset}"
                                );
                                break;
                            }
                            let post_work_observed = watcher_has_post_work_ready_evidence(
                                &full_response,
                                &tool_state,
                                task_notification_kind,
                            );
                            match watcher_ready_for_input_turn_completed(
                                &mut ready_for_input_tracker,
                                data_start_offset,
                                current_offset,
                                ready_for_input,
                                post_work_observed,
                                now,
                            ) {
                                crate::services::provider::ReadyForInputIdleState::None => {}
                                crate::services::provider::ReadyForInputIdleState::FreshIdle => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!(
                                        "  [{ts}] 👁 watcher observed fresh ready-for-input idle for {tmux_session_name} at offset {current_offset}; leaving session untouched"
                                    );
                                    fresh_ready_for_input_idle = true;
                                    break;
                                }
                                crate::services::provider::ReadyForInputIdleState::PostWorkIdleTimeout => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    let stall_inflight_snapshot =
                                        crate::services::discord::inflight::load_inflight_state(
                                            &watcher_provider,
                                            channel_id.get(),
                                        );
                                    let dispatch_id = resolve_dispatched_thread_dispatch_from_db(
                                        shared.pg_pool.as_ref(),
                                        watcher_thread_channel_id.unwrap_or_else(|| channel_id.get()),
                                    )
                                    .or_else(|| {
                                        stall_inflight_snapshot
                                            .as_ref()
                                            .and_then(|state| state.dispatch_id.clone())
                                    });
                                    ready_for_input_stall_inflight_snapshot =
                                        stall_inflight_snapshot;
                                    if let Some(dispatch_id) = dispatch_id {
                                        ready_for_input_stall_dispatch_id = Some(dispatch_id);
                                        ready_for_input_failure_notice = Some(format!(
                                            "⚠️ 작업 후 `Ready for input` 상태에서 멈춰 dispatch를 실패 처리합니다.\n사유: {READY_FOR_INPUT_STUCK_REASON}"
                                        ));
                                    } else {
                                        tracing::info!(
                                            "  [{ts}] 👁 watcher detected post-work Ready-for-input idle for {} with no dispatch; suppressing dispatch-failure notice",
                                            tmux_session_name
                                        );
                                    }
                                    full_response.clear();
                                    break;
                                }
                            }
                        }
                        if soft_terminal_seen_at.is_some()
                            && !full_response.trim().is_empty()
                            && last_output_at.elapsed() >= SOFT_TERMINAL_DEBOUNCE
                        {
                            terminal_kind.get_or_insert(WatcherTerminalKind::SoftStopHookSummary);
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::info!(
                                "  [{ts}] 👁 watcher committed soft stop_hook_summary after debounce for {tmux_session_name} at offset {current_offset}"
                            );
                            break;
                        }
                    }
                    _ => {
                        // #2441 (H1) — notify-backed wake-up for the
                        // inner-loop read-error retry path.
                        sleep_or_jsonl_event(
                            tokio::time::Duration::from_millis(200),
                            &jsonl_notify,
                            &dead_marker_notify,
                        )
                        .await;
                    }
                }

                // Check for stale session error during streaming — abort relay immediately.
                // Only structured error/result events can trip this flag.
                if stale_resume_detected {
                    break;
                }

                // Update Discord placeholder at configurable interval
                if last_status_update.elapsed()
                    >= crate::services::discord::status_update_interval()
                {
                    last_status_update = tokio::time::Instant::now();
                    let indicator = SPINNER[spin_idx % SPINNER.len()];
                    spin_idx += 1;

                    let tick_placeholder_reclaim = watcher_should_reclaim_orphan_turn_placeholder(
                        turn_is_external_input_for_session,
                        placeholder_msg_id,
                        !full_response.trim().is_empty(),
                        &last_edit_text,
                    );
                    if turn_is_external_input_for_session
                        && (status_panel_msg_id.is_some() || tick_placeholder_reclaim)
                        && watcher_external_input_turn_abandoned(
                            &watcher_provider,
                            channel_id,
                            &tmux_session_name,
                            &output_path,
                            data_start_offset,
                            turn_identity_for_panel.as_ref(),
                        )
                    {
                        cleanup_orphan_external_input_status_panel(
                            &http,
                            &shared,
                            channel_id,
                            &mut status_panel_msg_id,
                            &watcher_provider,
                            &tmux_session_name,
                            turn_is_external_input_for_session,
                        )
                        .await;
                        if tick_placeholder_reclaim {
                            reclaim_orphan_external_input_placeholder(
                                &http,
                                &shared,
                                channel_id,
                                &mut placeholder_msg_id,
                                &mut placeholder_from_restored_inflight,
                                &mut last_edit_text,
                                &watcher_provider,
                                &tmux_session_name,
                            )
                            .await;
                        }
                    }

                    // Headless silent trigger (metadata.silent=true): skip both
                    // status-panel and streaming-chunk edits to keep the channel
                    // at zero bytes for the assistant turn.
                    let streaming_silent_turn =
                        crate::services::discord::inflight::load_inflight_state(
                            &watcher_provider,
                            channel_id.get(),
                        )
                        .map(|state| state.silent_turn)
                        .unwrap_or(false);
                    if streaming_silent_turn {
                        continue;
                    }

                    if shared.ui.status_panel_v2_enabled
                        && (single_message_panel_footer_mode || status_panel_msg_id.is_some())
                    {
                        // #3055: re-derive this turn's session lifecycle panel
                        // line on the throttled status tick, matching bridge
                        // behavior and avoiding stale per-channel snapshots.
                        refresh_watcher_session_panel_from_lifecycle(
                            &shared,
                            channel_id,
                            turn_identity_for_panel
                                .as_ref()
                                .map(|identity| identity.user_msg_id)
                                .unwrap_or(0),
                            &tmux_session_name,
                            &watcher_provider, // #3983 item4: one-shot session banner render
                        )
                        .await;
                    }
                    if watcher_separate_status_panel_enabled(shared.ui.status_panel_v2_enabled)
                        && let Some(status_msg_id) = status_panel_msg_id
                    {
                        let panel_text = shared.ui.placeholder_live_events.render_status_panel(
                            channel_id,
                            &watcher_provider,
                            status_panel_started_at,
                        );
                        if panel_text != last_status_panel_text {
                            rate_limit_wait(&shared, channel_id).await;
                            match crate::services::discord::http::edit_channel_message(
                                &http,
                                channel_id,
                                status_msg_id,
                                &panel_text,
                            )
                            .await
                            {
                                Ok(_) => {
                                    last_status_panel_text = panel_text;
                                }
                                Err(error) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::warn!(
                                        "  [{ts}] ⚠ tmux status-panel-v2 edit failed for msg {} in channel {}: {}",
                                        status_msg_id.get(),
                                        channel_id.get(),
                                        error
                                    );
                                }
                            }
                        }
                    }

                    let has_assistant_response_for_streaming = !full_response.trim().is_empty();
                    if watcher_should_suppress_streaming_after_bridge_delivery(
                        turn_delivered.load(Ordering::Relaxed),
                        has_assistant_response_for_streaming,
                    ) {
                        if let Some(msg_id) = placeholder_msg_id {
                            if watcher_should_delete_suppressed_placeholder(
                                placeholder_from_restored_inflight,
                            ) {
                                let outcome = delete_nonterminal_placeholder(
                                    &http,
                                    channel_id,
                                    &shared,
                                    &watcher_provider,
                                    &tmux_session_name,
                                    msg_id,
                                    "watcher_streaming_bridge_delivered_cleanup",
                                )
                                .await;
                                if outcome.is_committed() {
                                    placeholder_msg_id = None;
                                    placeholder_from_restored_inflight = false;
                                    last_edit_text.clear();
                                }
                            } else {
                                // This placeholder id came from the active inflight row.
                                // In status-panel-v2 bridge-owned delivery, the bridge
                                // edits that exact message into the final response. The
                                // watcher must drop local ownership without deleting it.
                                placeholder_msg_id = None;
                                placeholder_from_restored_inflight = false;
                                last_edit_text.clear();
                            }
                        }
                        if !streaming_suppressed_by_recent_stop {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] 🛑 watcher: suppressed streaming placeholder output for channel {} after bridge delivered turn (tmux={}, range {}..{})",
                                channel_id.get(),
                                tmux_session_name,
                                data_start_offset,
                                current_offset
                            );
                            streaming_suppressed_by_recent_stop = true;
                        }
                        continue;
                    }
                    let recent_stop_for_streaming = if has_assistant_response_for_streaming {
                        recent_turn_stop_for_watcher_range(
                            channel_id,
                            &tmux_session_name,
                            data_start_offset,
                        )
                    } else {
                        None
                    };
                    let inflight_missing_for_streaming =
                        crate::services::discord::inflight::load_inflight_state(
                            &watcher_provider,
                            channel_id.get(),
                        )
                        .is_none();
                    // #3107: lazy pane-capture probe — only when inflight is
                    // missing (expensive signal stays off the hot path).
                    let pane_actively_streaming_for_streaming = inflight_missing_for_streaming
                        && watcher_pane_actively_streaming(&tmux_session_name);
                    if inflight_missing_for_streaming && pane_actively_streaming_for_streaming {
                        // #3107 self-heal: pane live but inflight cleared mid-turn —
                        // re-establish a watcher-owned inflight (idempotent + 1-shot log).
                        let reacquired = reacquire_watcher_inflight_for_active_stream(
                            &watcher_provider,
                            channel_id,
                            &tmux_session_name,
                            &output_path,
                            data_start_offset,
                            status_panel_msg_id,
                            placeholder_msg_id,
                            // #3107 (P2#3, F3): thread the #3099 hourglass anchor
                            // (captured before `restored_turn` was `.take()`n) so a
                            // mid-stream inflight loss keeps the `⏳ → ✅` cleanup anchor.
                            restored_injected_prompt_message_id,
                        );
                        if reacquired && !active_stream_inflight_reacquire_logged {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] 🩹 watcher: re-acquired watcher-owned inflight for actively-streaming pane that lost its inflight (channel {}, tmux={}, range {}..{})",
                                channel_id.get(),
                                tmux_session_name,
                                data_start_offset,
                                current_offset
                            );
                            active_stream_inflight_reacquire_logged = true;
                        }
                    }
                    if should_skip_streaming_placeholder_without_inflight(
                        inflight_missing_for_streaming,
                        pane_actively_streaming_for_streaming,
                    ) {
                        if !streaming_suppressed_by_missing_inflight {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] 🛑 watcher: suppressed streaming placeholder edit for channel {} because inflight state is missing (tmux={}, range {}..{})",
                                channel_id.get(),
                                tmux_session_name,
                                data_start_offset,
                                current_offset
                            );
                            streaming_suppressed_by_missing_inflight = true;
                        }
                        continue;
                    }
                    if should_suppress_streaming_placeholder_after_recent_stop(
                        has_assistant_response_for_streaming,
                        inflight_missing_for_streaming,
                        recent_stop_for_streaming.is_some(),
                    ) {
                        if let Some(msg_id) = placeholder_msg_id {
                            if watcher_should_delete_suppressed_placeholder(
                                placeholder_from_restored_inflight,
                            ) {
                                let outcome = delete_nonterminal_placeholder(
                                    &http,
                                    channel_id,
                                    &shared,
                                    &watcher_provider,
                                    &tmux_session_name,
                                    msg_id,
                                    "watcher_streaming_recent_stop_cleanup",
                                )
                                .await;
                                if outcome.is_committed() {
                                    placeholder_msg_id = None;
                                    placeholder_from_restored_inflight = false;
                                    last_edit_text.clear();
                                }
                            } else {
                                placeholder_msg_id = None;
                                placeholder_from_restored_inflight = false;
                                last_edit_text.clear();
                            }
                        }
                        if !streaming_suppressed_by_recent_stop {
                            if let Some(stop) = recent_stop_for_streaming {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::warn!(
                                    "  [{ts}] 🛑 watcher: suppressed streaming placeholder output for channel {} after recent turn stop ({}, tmux={}, range {}..{})",
                                    channel_id.get(),
                                    stop.reason,
                                    tmux_session_name,
                                    data_start_offset,
                                    current_offset
                                );
                            }
                            streaming_suppressed_by_recent_stop = true;
                        }
                        // #3003: the stopped-turn panel reclaim now runs at the
                        // single chokepoint at the top of this interval block, before
                        // this recent-stop `continue` and the inflight-missing guard
                        // can bypass it.
                        continue;
                    }

                    // #3003: TUI-direct turns lack a prior Discord message to
                    // re-designate, so flag-off creates a dedicated v2 panel here
                    // after suppression guards and only once visible work exists.
                    let has_visible_streaming_work = !full_response
                        .get(response_sent_offset..)
                        .unwrap_or("")
                        .trim()
                        .is_empty()
                        || watcher_should_render_status_only_placeholder(
                            placeholder_msg_id.is_some(),
                            tool_state.current_tool_line.as_deref(),
                            task_notification_kind,
                        );
                    if watcher_separate_status_panel_enabled(shared.ui.status_panel_v2_enabled)
                        && status_panel_msg_id.is_none()
                        && has_visible_streaming_work
                        // #3805 P2 (PR-C): under the two-message flag, defer panel
                        // creation until the answer placeholder exists so the panel
                        // is created BELOW it (answer-first). OFF: always true →
                        // the legacy creation block runs byte-identical.
                        && watcher_two_message_panel_creation_gated_by_answer(
                            shared.ui.two_message_panel_enabled,
                            placeholder_msg_id.is_some(),
                        )
                    {
                        let inflight_for_panel =
                            crate::services::discord::inflight::load_inflight_state(
                                &watcher_provider,
                                channel_id.get(),
                            );
                        let persisted_panel_msg_id = watcher_persisted_status_panel_msg_id(
                            inflight_for_panel.as_ref(),
                            &tmux_session_name,
                        );
                        // status-panel-v2: panel eligibility (external-input OR
                        // synthetic monitor/self-paced-loop) drives panel
                        // creation here; the lease/⏳-anchor sites keep the
                        // narrower external-input predicate.
                        let panel_eligible_turn = watcher_inflight_is_panel_eligible_for_session(
                            inflight_for_panel.as_ref(),
                            &tmux_session_name,
                        );
                        if panel_eligible_turn {
                            turn_is_external_input_for_session = true;
                            // #3003 P2: if startup predated inflight creation,
                            // capture identity now so abandon detects replacement.
                            if turn_identity_for_panel.is_none() {
                                turn_identity_for_panel = inflight_for_panel
                                    .as_ref()
                                    .filter(|state| {
                                        state.tmux_session_name.as_deref()
                                            == Some(tmux_session_name.as_str())
                                    })
                                    .map(crate::services::discord::inflight::InflightTurnIdentity::from_state);
                            }
                            // #3003 P2: no late live-event clear here; the fresh-frame
                            // reset above preserved this turn's initial flush.
                        }
                        if let Some(persisted) = persisted_panel_msg_id {
                            // Restart-safe adoption: the panel already exists and was
                            // persisted on this turn's inflight; reuse it instead of
                            // publishing a duplicate (#3003 codex P2). Synthetic headless
                            // ids are already filtered by the persisted helper.
                            status_panel_msg_id = Some(persisted);
                        } else if watcher_should_create_separate_status_panel(
                            single_message_panel_footer_mode,
                            shared.ui.status_panel_v2_enabled,
                            status_panel_msg_id.is_some(),
                            panel_eligible_turn,
                        ) && !watcher_external_input_turn_abandoned(
                            &watcher_provider,
                            channel_id,
                            &tmux_session_name,
                            &output_path,
                            data_start_offset,
                            turn_identity_for_panel.as_ref(),
                        ) {
                            // #3003 (codex P2 r18): do NOT create a panel for an already
                            // stopped/abandoned turn. A stop tombstone can be recorded
                            // before the inflight row is removed; without this guard the
                            // interval-top reclaim would delete the panel and this branch
                            // would immediately recreate one for the same stopped turn.
                            // Snapshot the turn identity *before* the await so a
                            // stop/cancel/next-turn during send cannot persist stale
                            // state onto a different turn (codex P2 r4).
                            let pre_send_identity = inflight_for_panel
                                .as_ref()
                                .map(crate::services::discord::inflight::InflightTurnIdentity::from_state);
                            let panel_seed =
                                crate::services::discord::formatting::build_processing_status_block(
                                    indicator,
                                );
                            rate_limit_wait(&shared, channel_id).await;
                            match crate::services::discord::http::send_channel_message(
                                &http,
                                channel_id,
                                &panel_seed,
                            )
                            .await
                            {
                                Ok(panel_msg) => {
                                    preregister_watcher_two_message_panel_orphan(
                                        shared.ui.two_message_panel_enabled,
                                        shared.as_ref(),
                                        &watcher_provider,
                                        channel_id,
                                        panel_msg.id,
                                    );
                                    let fresh_inflight =
                                        crate::services::discord::inflight::load_inflight_state(
                                            &watcher_provider,
                                            channel_id.get(),
                                        );
                                    let identity_matches = matches!(
                                        (&pre_send_identity, &fresh_inflight),
                                        (Some(pre), Some(fresh))
                                            if pre == &crate::services::discord::inflight::InflightTurnIdentity::from_state(fresh)
                                    );
                                    // #3003 (codex P2 r18): another overlapping watcher may
                                    // have already published+persisted a panel for this turn
                                    // during our send await. If the fresh inflight already
                                    // carries a real status_message_id, our send is a
                                    // duplicate — reclaim it instead of overwriting the
                                    // canonical id (which would orphan the other panel).
                                    let fresh_panel_already_set = fresh_inflight.as_ref().is_some_and(|fresh| {
                                        crate::services::discord::turn_bridge::normalize_status_panel_message_id(
                                            fresh.status_message_id.map(serenity::MessageId::new),
                                        )
                                        .is_some()
                                    });
                                    if identity_matches
                                        && !fresh_panel_already_set
                                        && fresh_inflight.is_some()
                                    {
                                        // #3077: bind through the typed op so the
                                        // identity guard + "don't clobber an already-set
                                        // panel" check are re-validated atomically under
                                        // the inflight flock — closing the window where an
                                        // overlapping watcher rebinds between our snapshot
                                        // load and this write (#3003).
                                        let bind_outcome = crate::services::discord::inflight::bind_status_panel(
                                            &watcher_provider,
                                            channel_id.get(),
                                            panel_msg.id.get(),
                                            &crate::services::discord::inflight::StatusPanelBindGuard {
                                                require_identity: pre_send_identity.clone(),
                                                skip_if_panel_already_set: true,
                                                // #3805 P2: when the two-message
                                                // flag is ON, open this turn's
                                                // panel epoch from the on-disk
                                                // row inside the bind flock.
                                                // OFF leaves the field untouched.
                                                bump_status_panel_generation:
                                                    shared.ui.two_message_panel_enabled,
                                                ..Default::default()
                                            },
                                        );
                                        // #3077 (codex P1): the pre-send snapshot narrows but does
                                        // NOT close the race (an overlapping watcher can rebind
                                        // between our load and this atomic bind). The bind is the
                                        // single source of truth for whether THIS panel is recorded,
                                        // so the adopted handle MUST come from its return — adopting
                                        // `panel_msg.id` unconditionally leaks a sent-but-unrecorded panel.
                                        let decision =
                                            resolve_tui_status_panel_bind_decision(bind_outcome);
                                        if decision.delete_sent_panel {
                                            // The inflight row did NOT record our panel:
                                            //  - SkippedPanelAlreadySet → the row already carries a
                                            //    DIFFERENT (real) panel id; ours is a duplicate.
                                            //  - GuardMismatch / Missing / IoError → the bind never
                                            //    happened (the row changed/disappeared or a guard
                                            //    failed); we must not claim ownership of a panel the
                                            //    row doesn't know about.
                                            // Delete the just-sent duplicate so it never leaks. This
                                            // reuses the same delete path the "inflight changed
                                            // during send" branch below uses
                                            // (delete_nonterminal_placeholder → tmux.rs:803). It
                                            // never double-deletes a legitimately-bound panel: we
                                            // only reach here when our bind did NOT record
                                            // `panel_msg.id`, so the row's owned panel (if any) is a
                                            // *different* id we never delete.
                                            let discard_outcome = delete_nonterminal_placeholder(
                                                &http,
                                                channel_id,
                                                &shared,
                                                &watcher_provider,
                                                &tmux_session_name,
                                                panel_msg.id,
                                                "watcher_external_input_status_panel_bind_unowned",
                                            )
                                            .await;
                                            if !discard_outcome.is_committed()
                                                && !discard_outcome.is_permanent_failure()
                                            {
                                                // Transient delete failure: the duplicate panel
                                                // still exists and this path does not persist it to
                                                // inflight, so record it in the durable store for
                                                // the sweeper drain to reclaim independent of turn
                                                // lifecycle (#3003 codex P2 r14 pattern).
                                                enqueue_watcher_status_panel_orphan(
                                                    shared.as_ref(),
                                                    &watcher_provider,
                                                    channel_id,
                                                    panel_msg.id,
                                                );
                                            } else {
                                                remove_watcher_two_message_panel_orphan_registration(
                                                    shared.ui.two_message_panel_enabled,
                                                    shared.as_ref(),
                                                    &watcher_provider,
                                                    channel_id,
                                                    panel_msg.id,
                                                );
                                            }
                                            // Resolve the handle from the row's CURRENT owned id as
                                            // observed by the bind (`decision.owned_panel_id`), never
                                            // the just-sent duplicate nor the (possibly stale) pre-bind
                                            // `fresh_inflight` snapshot (#3077 codex P2 #2). It is
                                            // `None` for GuardMismatch/Missing/IoError (no panel we may
                                            // claim → handle unset). Adopt only for the SAME turn we
                                            // sent for; a replacement turn's panel belongs to it.
                                            let resolved_handle = if identity_matches {
                                                decision
                                                    .owned_panel_id
                                                    .map(serenity::MessageId::new)
                                            } else {
                                                None
                                            };
                                            status_panel_msg_id = resolved_handle;
                                            let ts = chrono::Local::now().format("%H:%M:%S");
                                            // Single bounded incident log per unowned-bind event.
                                            tracing::warn!(
                                                "  [{ts}] ⚠ watcher: status-panel-v2 bind did not record our panel for TUI-direct turn in channel {} (outcome={:?}, panel_msg={}, delete_committed={}, adopted_handle={:?}); discarded duplicate instead of leaking it",
                                                channel_id.get(),
                                                bind_outcome,
                                                panel_msg.id.get(),
                                                discard_outcome.is_committed(),
                                                resolved_handle.map(serenity::MessageId::get)
                                            );
                                        } else {
                                            // Bound / AlreadyBound: the row now owns this exact id.
                                            debug_assert!(decision.adopt_sent_panel);
                                            remove_watcher_two_message_panel_orphan_registration(
                                                shared.ui.two_message_panel_enabled,
                                                shared.as_ref(),
                                                &watcher_provider,
                                                channel_id,
                                                panel_msg.id,
                                            );
                                            status_panel_msg_id = Some(panel_msg.id);
                                            // #3805 P2 (PR-C): a FRESH Bound opened this
                                            // turn's panel epoch (the generation the
                                            // guard just persisted); mirror it into the
                                            // local so the completion guard proves the
                                            // SAME epoch. AlreadyBound re-binds do NOT
                                            // re-open it (the local already carries the
                                            // on-disk seed). None/OFF → local untouched.
                                            if shared.ui.two_message_panel_enabled
                                                && let Some(opened) =
                                                    bind_outcome.bound_status_panel_generation()
                                            {
                                                this_turn_status_panel_generation = opened;
                                            }
                                            let ts = chrono::Local::now().format("%H:%M:%S");
                                            tracing::info!(
                                                "  [{ts}] 🪧 watcher: created status-panel-v2 for TUI-direct turn (channel {}, tmux={}, panel_msg={})",
                                                channel_id.get(),
                                                tmux_session_name,
                                                panel_msg.id.get()
                                            );
                                        }
                                    } else {
                                        // The turn vanished/changed during the send await, or an
                                        // overlapping watcher already owns the panel; ours is a
                                        // duplicate/orphan — reclaim it instead of persisting stale
                                        // state (the next interval adopts the canonical panel).
                                        let discard_outcome = delete_nonterminal_placeholder(
                                            &http,
                                            channel_id,
                                            &shared,
                                            &watcher_provider,
                                            &tmux_session_name,
                                            panel_msg.id,
                                            "watcher_external_input_status_panel_turn_changed",
                                        )
                                        .await;
                                        if !discard_outcome.is_committed()
                                            && !discard_outcome.is_permanent_failure()
                                        {
                                            // #3003 (codex P2 r14): transient delete failure but the
                                            // duplicate exists and this path never persists it —
                                            // record it for the sweeper drain to reclaim.
                                            enqueue_watcher_status_panel_orphan(
                                                shared.as_ref(),
                                                &watcher_provider,
                                                channel_id,
                                                panel_msg.id,
                                            );
                                            // #3003 (codex P2 r19/r22): adopt the CANONICAL persisted
                                            // panel ONLY for a same-turn overlapping-watcher duplicate
                                            // (`identity_matches`), so edits/completion hit the real
                                            // panel. For a *replacement* turn the persisted id is the
                                            // new turn's; adopting it would let the old frame's abandon
                                            // cleanup delete it — keep the just-sent duplicate locally.
                                            if fresh_panel_already_set && identity_matches {
                                                status_panel_msg_id =
                                                    watcher_persisted_status_panel_msg_id(
                                                        fresh_inflight.as_ref(),
                                                        &tmux_session_name,
                                                    );
                                            } else {
                                                status_panel_msg_id = Some(panel_msg.id);
                                            }
                                        } else {
                                            remove_watcher_two_message_panel_orphan_registration(
                                                shared.ui.two_message_panel_enabled,
                                                shared.as_ref(),
                                                &watcher_provider,
                                                channel_id,
                                                panel_msg.id,
                                            );
                                        }
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::warn!(
                                            "  [{ts}] ⚠ watcher: discarded status-panel-v2 for TUI-direct turn in channel {} — inflight changed during send (panel_msg={}, delete_committed={})",
                                            channel_id.get(),
                                            panel_msg.id.get(),
                                            discard_outcome.is_committed()
                                        );
                                    }
                                }
                                Err(error) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::warn!(
                                        "  [{ts}] ⚠ watcher: failed to create status-panel-v2 for TUI-direct turn in channel {}: {}",
                                        channel_id.get(),
                                        error
                                    );
                                }
                            }
                        }
                    }

                    // #3805 P2 (PR-D): track whether an answer rollover created a
                    // fresh tail message this interval, so the two-message status
                    // panel is re-anchored BELOW it exactly once (not on quiet
                    // intervals). OFF-inert.
                    let mut watcher_did_rollover_this_interval = false;
                    loop {
                        let current_portion =
                            full_response.get(response_sent_offset..).unwrap_or("");
                        if current_portion.is_empty() {
                            break;
                        }

                        let status_block = build_watcher_single_message_panel_status_block(
                            &shared,
                            channel_id,
                            &watcher_provider,
                            status_panel_started_at,
                            indicator,
                            tool_state.prev_tool_status.as_deref(),
                            tool_state.current_tool_line.as_deref(),
                            &full_response,
                            status_panel_msg_id,
                        );
                        let Some(msg_id) = placeholder_msg_id else {
                            break;
                        };
                        if watcher_streaming_rollover_should_skip(current_portion) {
                            break;
                        }
                        let Some(plan) = plan_streaming_rollover(current_portion, &status_block)
                        else {
                            break;
                        };

                        rate_limit_wait(&shared, channel_id).await;
                        match crate::services::discord::http::edit_channel_message(
                            &http,
                            channel_id,
                            msg_id,
                            &plan.frozen_chunk,
                        )
                        .await
                        {
                            Ok(_) => {
                                rate_limit_wait(&shared, channel_id).await;
                                match crate::services::discord::http::send_channel_message(
                                    &http,
                                    channel_id,
                                    &status_block,
                                )
                                .await
                                {
                                    Ok(message) => {
                                        // #3871: `msg_id` is now a FROZEN prefix — record it for terminal full-body dedup.
                                        watcher_streaming_rollover_frozen_msg_ids.push(msg_id);
                                        placeholder_msg_id = Some(message.id);
                                        placeholder_from_restored_inflight = false;
                                        watcher_did_rollover_this_interval = true;
                                        response_sent_offset += plan.split_at;
                                        last_edit_text = status_block;
                                        persist_watcher_stream_progress(
                                            &watcher_provider,
                                            channel_id,
                                            &tmux_session_name,
                                            turn_identity_for_panel.as_ref(),
                                            placeholder_msg_id,
                                            &full_response,
                                            response_sent_offset,
                                            tool_state.current_tool_line.as_deref(),
                                            tool_state.prev_tool_status.as_deref(),
                                            task_notification_kind,
                                            tool_state.any_tool_used,
                                            tool_state.has_post_tool_text,
                                            &watcher_streaming_rollover_frozen_msg_ids,
                                        );
                                    }
                                    Err(error) => {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::warn!(
                                            "  [{ts}] ⚠ tmux rollover placeholder send failed in channel {}: {}",
                                            channel_id.get(),
                                            error
                                        );
                                        rate_limit_wait(&shared, channel_id).await;
                                        let _ =
                                            crate::services::discord::http::edit_channel_message(
                                                &http,
                                                channel_id,
                                                msg_id,
                                                &plan.display_snapshot,
                                            )
                                            .await;
                                        last_edit_text = plan.display_snapshot;
                                        break;
                                    }
                                }
                            }
                            Err(error) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::warn!(
                                    "  [{ts}] ⚠ tmux rollover freeze failed for msg {} in channel {}: {}",
                                    msg_id.get(),
                                    channel_id.get(),
                                    error
                                );
                                break;
                            }
                        }
                    }

                    // #3805 P2 (PR-D): after answer rollover the live status
                    // panel is stranded ABOVE the new tail answer. Flag ON
                    // re-anchors it BELOW; flag OFF stays byte-identical.
                    let two_message_panel_enabled = shared.ui.two_message_panel_enabled;
                    let inflight_for_reanchor =
                        if two_message_panel_enabled && watcher_did_rollover_this_interval {
                            crate::services::discord::inflight::load_inflight_state(
                                &watcher_provider,
                                channel_id.get(),
                            )
                        } else {
                            None
                        };
                    if watcher_did_rollover_this_interval
                        && watcher_two_message_should_reanchor_panel_on_rollover(
                            two_message_panel_enabled,
                            status_panel_msg_id.is_some(),
                            inflight_for_reanchor.as_ref(),
                            &tmux_session_name,
                        )
                    {
                        let panel_text = shared.ui.placeholder_live_events.render_status_panel(
                            channel_id,
                            &watcher_provider,
                            status_panel_started_at,
                        );
                        reanchor_watcher_two_message_status_panel_below_answer(
                            &http,
                            &shared,
                            channel_id,
                            &watcher_provider,
                            &tmux_session_name,
                            turn_identity_for_panel.clone(),
                            &panel_text,
                            &mut status_panel_msg_id,
                            &mut this_turn_status_panel_generation,
                            &mut last_status_panel_text,
                        )
                        .await;
                    }

                    let status_block = build_watcher_single_message_panel_status_block(
                        &shared,
                        channel_id,
                        &watcher_provider,
                        status_panel_started_at,
                        indicator,
                        tool_state.prev_tool_status.as_deref(),
                        tool_state.current_tool_line.as_deref(),
                        &full_response,
                        status_panel_msg_id,
                    );
                    let current_portion = full_response.get(response_sent_offset..).unwrap_or("");
                    if current_portion.trim().is_empty()
                        && !watcher_should_render_status_only_placeholder(
                            placeholder_msg_id.is_some(),
                            tool_state.current_tool_line.as_deref(),
                            task_notification_kind,
                        )
                    {
                        continue;
                    }
                    let display_text = build_watcher_streaming_edit_text(
                        shared.ui.status_panel_v2_enabled,
                        current_portion,
                        &status_block,
                        &watcher_provider,
                    );

                    if crate::services::discord::single_message_panel::streaming_footer_text_changed(
                        single_message_panel_footer_mode,
                        &last_edit_text,
                        &display_text,
                    ) {
                        let edit_committed = match placeholder_msg_id {
                            Some(msg_id) => {
                                rate_limit_wait(&shared, channel_id).await;
                                edit_channel_message(&http, channel_id, msg_id, &display_text)
                                    .await
                                    .is_ok()
                            }
                            None => {
                                if let Ok(msg) =
                                    send_channel_message(&http, channel_id, &display_text).await
                                {
                                    placeholder_msg_id = Some(msg.id);
                                    placeholder_from_restored_inflight = false;
                                    true
                                } else {
                                    false
                                }
                            }
                        };
                        if edit_committed {
                            last_edit_text = display_text;
                            persist_watcher_stream_progress(
                                &watcher_provider,
                                channel_id,
                                &tmux_session_name,
                                turn_identity_for_panel.as_ref(),
                                placeholder_msg_id,
                                &full_response,
                                response_sent_offset,
                                tool_state.current_tool_line.as_deref(),
                                tool_state.prev_tool_status.as_deref(),
                                task_notification_kind,
                                tool_state.any_tool_used,
                                tool_state.has_post_tool_text,
                                &watcher_streaming_rollover_frozen_msg_ids,
                            );
                        }
                    }
                }
            }

            if fresh_ready_for_input_idle {
                // #3016 S3: the STRUCTURAL completion signal — the authority that
                // finally distinguishes "turn done" from "paused-live" (which the
                // old flag-only path could not). Resolve the runtime kind exactly
                // as `watcher_session_ready_for_input` does (runtime binding →
                // tmux marker), then read the relay-offset-independent strict
                // terminator probe via the S1 read-only API. `output_path` is the
                // provider's on-disk JSONL transcript for this session.
                let watcher_runtime_kind =
                    crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
                        &tmux_session_name,
                    )
                    .map(|binding| binding.runtime_kind)
                    .or_else(|| {
                        crate::services::tmux_common::resolve_tmux_runtime_kind_marker(
                            &tmux_session_name,
                        )
                    });
                let fresh_idle_completion_signal = shared.turn_finalizer.completion_signal_state(
                    &watcher_provider,
                    watcher_runtime_kind,
                    std::path::Path::new(&output_path),
                );
                // #3016 S3 (A2 wrong-turn race fix): pin the finalize id from a
                // snapshot taken NOW — BEFORE the cleanup `.await`s below — and
                // gate it on the SAME output-range relationship the canonical
                // normal-completion site uses. A LATE re-read after the cleanup
                // awaits could observe a follow-up turn that became current on the
                // SAME session and rewrote inflight, finalizing the WRONG turn.
                let pinned_pre_cleanup_inflight =
                    crate::services::discord::inflight::load_inflight_state(
                        &watcher_provider,
                        channel_id.get(),
                    );
                // #3016 S3 / phase-5b1 (codex HIGH fix): the DEFER decision keys on
                // the STRUCTURAL TERMINATOR and — for non-JSONL `Unknown` runtimes —
                // on response EMPTINESS, NOT on the `mailbox_finalize_owed` flag. This
                // is the flag-independent reconstruction of the OLD (pre-5b1) defer
                // condition (`delegated_finalize_owed && empty`): `owed` was ~always
                // true for a delegated `Unknown` turn at this arm, so the old gate was
                // effectively "empty → defer". Re-keying on emptiness alone reproduces
                // it without the flag. Rationale: non-JSONL runtimes (Gemini / OpenCode
                // / Qwen / LegacyTmuxWrapper) have NO structured PausedLive signal — a
                // turn awaiting a selector / permission / interactive prompt can look
                // idle (ready_for_input sustained over the timeout) with EMPTY output.
                // Finalizing it here would kill the turn mid-work; instead we defer and
                // let the 5a 1800s far-backstop (which re-checks pane-idle at the
                // deadline) be its finalizer. NON-empty `Unknown` finalizes promptly
                // (the intended 5b1 improvement, flag-independent). `PausedLive` (no
                // terminator) always defers. `Done` (JSONL terminator proven) never
                // defers and finalizes even when empty. The wrong-turn-race guards in
                // `watcher_fresh_idle_finalize_decision` (paused/epoch abort, stale-skip)
                // still handle the follow-up-took-over cases for the finalize arms.
                let defer_fresh_idle = match fresh_idle_completion_signal {
                    crate::services::discord::turn_finalizer::CompletionSignal::PausedLive => true,
                    crate::services::discord::turn_finalizer::CompletionSignal::Done => false,
                    crate::services::discord::turn_finalizer::CompletionSignal::Unknown => {
                        full_response.trim().is_empty()
                    }
                };
                if defer_fresh_idle {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 👁 watcher observed fresh ready-for-input idle for {tmux_session_name} at offset {current_offset}, but no structural completion terminator yet (signal={fresh_idle_completion_signal:?}); preserving inflight and waiting for terminal commit"
                    );
                    all_data.clear();
                    all_data_start_offset = current_offset;
                    all_data_fully_mirrored_to_session_relay = true;
                    all_data_session_bound_relay_ack = None;
                    all_data_first_forwarded_relay_sequence = None;
                    last_observed_generation_mtime_ns =
                        Some(read_generation_file_mtime_ns(&tmux_session_name));
                    finish_monitor_auto_turn_if_claimed(
                        &shared,
                        &watcher_provider,
                        channel_id,
                        &mut monitor_auto_turn_claimed,
                        &mut monitor_auto_turn_finished,
                        &mut monitor_auto_turn_synthetic_msg_id,
                        &mut monitor_auto_turn_ledger_generation,
                    )
                    .await;
                    continue;
                }
                let cleanup_committed = if let Some(msg_id) = placeholder_msg_id {
                    if watcher_should_delete_suppressed_placeholder(
                        placeholder_from_restored_inflight,
                    ) {
                        let outcome = delete_nonterminal_placeholder(
                            &http,
                            channel_id,
                            &shared,
                            &watcher_provider,
                            &tmux_session_name,
                            msg_id,
                            "watcher_fresh_ready_for_input_idle_cleanup",
                        )
                        .await;
                        if outcome.is_committed() {
                            let _ = placeholder_msg_id.take();
                            placeholder_from_restored_inflight = false;
                            last_edit_text.clear();
                            true
                        } else {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ watcher: fresh ready-for-input cleanup did not commit for channel {} msg {}; preserving inflight for retry",
                                channel_id.get(),
                                msg_id.get()
                            );
                            false
                        }
                    } else if watcher_should_reclaim_orphan_turn_placeholder(
                        turn_is_external_input_for_session,
                        placeholder_msg_id,
                        !full_response.trim().is_empty(),
                        &last_edit_text,
                    ) {
                        // #3351 (codex r2 #1): route the restored placeholder through the
                        // gated reclaim instead of stranding it; transient failure defers
                        // finalization like the panel guard above.
                        reclaim_orphan_external_input_placeholder(
                            &http,
                            &shared,
                            channel_id,
                            &mut placeholder_msg_id,
                            &mut placeholder_from_restored_inflight,
                            &mut last_edit_text,
                            &watcher_provider,
                            &tmux_session_name,
                        )
                        .await
                    } else {
                        let _ = placeholder_msg_id.take();
                        placeholder_from_restored_inflight = false;
                        last_edit_text.clear();
                        true
                    }
                } else {
                    true
                };
                if !cleanup_committed {
                    continue;
                }
                // #3003 (codex P2 r3): fresh idle with no committed response means the
                // terminal completion path will not run, so reclaim any watcher-created
                // status panel before it orphans at "계속 처리 중". Self-gated to
                // external-input turns on this session (bridge-owned panels untouched).
                // #3003 (codex P2 r5): if the panel delete did not commit, defer
                // finalization — clearing the inflight here would drop the persisted
                // status_message_id and strand the panel with no retry path. Re-enter
                // fresh idle next iteration to retry, mirroring the placeholder guard.
                let panel_cleanup_committed = cleanup_orphan_external_input_status_panel(
                    &http,
                    &shared,
                    channel_id,
                    &mut status_panel_msg_id,
                    &watcher_provider,
                    &tmux_session_name,
                    turn_is_external_input_for_session,
                )
                .await;
                if !panel_cleanup_committed {
                    continue;
                }
                // #3016 phase-5b2: the legacy `mailbox_finalize_owed` flag is
                // removed. The finalize DECISION never depended on it — both `Done`
                // and `Unknown` route to the structural / pane-idle `Finalize` arm
                // with `normal_completion = true`; the residual `swap(false)` (whose
                // value fed only the observability payload) is gone with the field.
                // #3016 S3 / phase-5b1 (codex HIGH fix): the finalize DECISION,
                // computed by the same pure helper the unit tests drive. The defer
                // gate above already deferred `PausedLive` and EMPTY `Unknown`, so
                // here the signal is `Done` (empty or not) or NON-empty `Unknown` —
                // both route to the `Finalize` arm. Emptiness is threaded in
                // flag-independently so the helper can re-assert the empty-`Unknown`
                // defer defensively (it is the unreachable mirror of the gate above).
                let fresh_idle_decision = watcher_fresh_idle_finalize_decision(
                    fresh_idle_completion_signal,
                    full_response.trim().is_empty(),
                    paused.load(Ordering::Relaxed),
                    pause_epoch.load(Ordering::Relaxed) != epoch_snapshot,
                    pinned_pre_cleanup_inflight.as_ref(),
                    &tmux_session_name,
                    current_offset,
                );
                match fresh_idle_decision {
                    FreshIdleFinalizeDecision::DeferPausedLive => {
                        // Unreachable: PausedLive was deferred at the defer gate
                        // above. Treat defensively as a defer (preserve inflight).
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] 👁 watcher fresh ready-for-input idle for {tmux_session_name}: PausedLive reached the finalize gate unexpectedly; preserving inflight"
                        );
                        all_data.clear();
                        all_data_start_offset = current_offset;
                        all_data_fully_mirrored_to_session_relay = true;
                        all_data_session_bound_relay_ack = None;
                        all_data_first_forwarded_relay_sequence = None;
                        continue;
                    }
                    FreshIdleFinalizeDecision::DeferEmptyUnknown => {
                        // Unreachable: empty `Unknown` was deferred at the defer gate
                        // above. Treat defensively as a defer (preserve inflight) —
                        // the 5a 1800s far-backstop finalizes the empty turn later.
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] 👁 watcher fresh ready-for-input idle for {tmux_session_name}: empty Unknown reached the finalize gate unexpectedly; preserving inflight (far-backstop will finalize)"
                        );
                        all_data.clear();
                        all_data_start_offset = current_offset;
                        all_data_fully_mirrored_to_session_relay = true;
                        all_data_session_bound_relay_ack = None;
                        all_data_first_forwarded_relay_sequence = None;
                        continue;
                    }
                    FreshIdleFinalizeDecision::AbortFollowupTookOver => {
                        // #3016 S3 (A2 wrong-turn race fix): a Discord turn claimed
                        // this session during the cleanup `.await`s (paused / epoch
                        // bumped at handoff). The canonical pause/epoch guard sits
                        // AFTER this branch's `continue`, so we mirror it HERE,
                        // before the destructive clear, to avoid releasing the
                        // follow-up turn.
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 👁 watcher fresh ready-for-input idle for {tmux_session_name} aborted before finalize: follow-up turn took over (paused/epoch changed); preserving inflight"
                        );
                        all_data.clear();
                        all_data_start_offset = current_offset;
                        all_data_fully_mirrored_to_session_relay = true;
                        all_data_session_bound_relay_ack = None;
                        all_data_first_forwarded_relay_sequence = None;
                        continue;
                    }
                    FreshIdleFinalizeDecision::SkipStale { pinned_user_msg_id } => {
                        // #3016 S3 (A2 wrong-turn race fix): the pinned pre-cleanup
                        // snapshot is a NEWER turn that began AT/AFTER this
                        // committed range; finalizing would release the follow-up.
                        // Skip and preserve inflight for the current/newer turn.
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 👁 watcher fresh ready-for-input idle for {tmux_session_name} skipped finalize: pinned id {pinned_user_msg_id} is stale for a newer turn at offset {current_offset}; preserving inflight"
                        );
                        all_data.clear();
                        all_data_start_offset = current_offset;
                        all_data_fully_mirrored_to_session_relay = true;
                        all_data_session_bound_relay_ack = None;
                        all_data_first_forwarded_relay_sequence = None;
                        continue;
                    }
                    FreshIdleFinalizeDecision::Finalize { user_msg_id } => {
                        // #3016 S3 (the A2 / phase-5 enabler): a structural JSONL
                        // terminator is PROVEN on disk for this turn (Done) AND no
                        // follow-up took over — finalize via the single-authority
                        // path with `normal_completion = true`, FLAG-INDEPENDENT,
                        // so an EMPTY-but-terminated completion finalizes too (the
                        // old flag-gated path could not tell it from a paused-live
                        // turn). The finalizer is idempotent (`AlreadyFinalized`),
                        // and `user_msg_id` is PINNED from the pre-cleanup snapshot
                        // at this `current_offset` (never a late re-read), so the
                        // ledger match is the CURRENT turn's real, non-zero id.
                        //
                        // #3016 S3 (Concern 2 — residual TOCTOU): the destructive
                        // on-disk clear must not wipe a FOLLOW-UP turn's inflight.
                        // The earlier read→check→unconditional-clear spanned TWO
                        // locks, so a follow-up saved on another worker thread in
                        // the gap was wiped. `clear_inflight_state_if_matches_identity`
                        // (inflight.rs) closes the window atomically: read +
                        // validate + unlink under ONE sidecar lock, deleting only
                        // while the on-disk identity (`user_msg_id` + `started_at`
                        // + `tmux_session_name`) still equals the PINNED turn's
                        // (`pinned_pre_cleanup_inflight`, the same snapshot that
                        // derived `user_msg_id` above) — a follow-up's identity
                        // differs (`UserMsgMismatch`), guaranteed no-op. The
                        // finalize-skip for a NEWER pinned turn stays a SEPARATE
                        // decision in `watcher_fresh_idle_finalize_decision`;
                        // finalize below runs on the PINNED id (idempotent)
                        // regardless of the clear outcome.
                        let pinned_clear_identity = pinned_pre_cleanup_inflight.as_ref().map(
                            crate::services::discord::inflight::InflightTurnIdentity::from_state,
                        );
                        if let Some(pinned_clear_identity) = pinned_clear_identity.as_ref() {
                            let clear_outcome =
                                crate::services::discord::inflight::clear_inflight_state_if_matches_identity(
                                    &watcher_provider,
                                    channel_id.get(),
                                    pinned_clear_identity,
                                );
                            match clear_outcome {
                                crate::services::discord::inflight::GuardedClearOutcome::Cleared => {
                                    crate::services::observability::emit_inflight_lifecycle_event(
                                        watcher_provider.as_str(),
                                        channel_id.get(),
                                        None,
                                        None,
                                        None,
                                        "cleared_by_watcher_fresh_idle",
                                        serde_json::json!({
                                            "finish_mailbox_on_completion": finish_mailbox_on_completion,
                                            // #3016 phase-5b1: Done (structural) OR
                                            // Unknown (pane-idle proxy) both reach here.
                                            "completion_signal": format!("{fresh_idle_completion_signal:?}"),
                                            "tmux_session": tmux_session_name.as_str(),
                                            "offset": current_offset,
                                        }),
                                    );
                                }
                                other => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!(
                                        "  [{ts}] 👁 watcher fresh ready-for-input idle for {tmux_session_name}: atomic identity-matched clear was a no-op (outcome={other:?}) at offset {current_offset} — on-disk inflight is no longer the pinned turn (follow-up preserved); finalizing the pinned current turn only"
                                    );
                                }
                            }
                        } else {
                            // No pinned snapshot identity available — there is
                            // nothing safe to clear by identity. Skip the clear and
                            // finalize on the pinned id only. (Unreachable on the
                            // `Finalize` arm, since `pinned_finalize_user_msg_id`
                            // requires a non-zero pinned snapshot to return a
                            // finalizable id; kept defensive.)
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] 👁 watcher fresh ready-for-input idle for {tmux_session_name}: no pinned snapshot identity for the atomic clear at offset {current_offset}; skipping the on-disk clear and finalizing the pinned current turn only"
                            );
                        }
                        finish_restored_watcher_active_turn(
                            &shared,
                            &watcher_provider,
                            channel_id,
                            user_msg_id,
                            finish_mailbox_on_completion,
                            // #3016 S3 / phase-5b1: Done = confirmed structural
                            // completion; Unknown = non-JSONL runtime at proven
                            // pane-idle. Both drive the finalizer on the
                            // normal-completion authority, independent of the legacy
                            // flag (removed in #3016 phase-5b2).
                            true,
                            true,
                            // #3350 codex r1-1: the row was cleared above — the
                            // finalize-time marker ensure authenticates against
                            // this pre-clear snapshot instead of a no-op re-load.
                            pinned_pre_cleanup_inflight.as_ref().map(
                                crate::services::discord::turn_finalizer::SyntheticClaimSnapshot::from_row,
                            ),
                            "watcher fresh ready-for-input idle (structural/pane-idle completion)",
                        )
                        .await;
                    }
                }
                all_data.clear();
                all_data_start_offset = current_offset;
                all_data_fully_mirrored_to_session_relay = true;
                all_data_session_bound_relay_ack = None;
                all_data_first_forwarded_relay_sequence = None;
                last_relayed_offset = Some(current_offset);
                last_observed_generation_mtime_ns =
                    Some(read_generation_file_mtime_ns(&tmux_session_name));
                advance_watcher_confirmed_end(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &tmux_session_name,
                    current_offset,
                    "src/services/discord/tmux.rs:ready_for_input_fresh_idle",
                );
                finish_monitor_auto_turn_if_claimed(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &mut monitor_auto_turn_claimed,
                    &mut monitor_auto_turn_finished,
                    &mut monitor_auto_turn_synthetic_msg_id,
                    &mut monitor_auto_turn_ledger_generation,
                )
                .await;
                continue;
            }

            if tmux_death_observed {
                handle_tmux_watcher_observed_death(
                    channel_id,
                    &http,
                    &shared,
                    &tmux_session_name,
                    &output_path,
                    &watcher_provider,
                    prompt_too_long_killed,
                    watcher_lifecycle_terminal_delivery_observed(
                        terminal_delivery_observed,
                        turn_delivered.load(Ordering::Acquire),
                    ),
                )
                .await;
                finish_monitor_auto_turn_if_claimed(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &mut monitor_auto_turn_claimed,
                    &mut monitor_auto_turn_finished,
                    &mut monitor_auto_turn_synthetic_msg_id,
                    &mut monitor_auto_turn_ledger_generation,
                )
                .await;
                break 'watcher_loop;
            }

            if cancel.load(Ordering::Relaxed)
                || shared.restart.shutting_down.load(Ordering::Relaxed)
            {
                // #3277 (Defect B): same stop-reason visibility as the early break.
                tracing::info!(
                    instance = watcher_instance_id,
                    cancel = cancel.load(Ordering::Relaxed),
                    shutting_down = shared.restart.shutting_down.load(Ordering::Relaxed),
                    "tmux watcher stopping for #{tmux_session_name}: cancelled/shutdown"
                );
                finish_monitor_auto_turn_if_claimed(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &mut monitor_auto_turn_claimed,
                    &mut monitor_auto_turn_finished,
                    &mut monitor_auto_turn_synthetic_msg_id,
                    &mut monitor_auto_turn_ledger_generation,
                )
                .await;
                break 'watcher_loop;
            }

            if let Some(notice) = ready_for_input_failure_notice {
                let notice_ok = match placeholder_msg_id {
                    Some(msg_id) => {
                        rate_limit_wait(&shared, channel_id).await;
                        crate::services::discord::http::edit_channel_message(
                            &http, channel_id, msg_id, &notice,
                        )
                        .await
                        .is_ok()
                    }
                    None => crate::services::discord::http::send_channel_message(
                        &http, channel_id, &notice,
                    )
                    .await
                    .is_ok(),
                };
                if !notice_ok {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ watcher: Ready-for-input stall notice failed before dispatch failure — preserving inflight for retry"
                    );
                    finish_monitor_auto_turn_if_claimed(
                        &shared,
                        &watcher_provider,
                        channel_id,
                        &mut monitor_auto_turn_claimed,
                        &mut monitor_auto_turn_finished,
                        &mut monitor_auto_turn_synthetic_msg_id,
                        &mut monitor_auto_turn_ledger_generation,
                    )
                    .await;
                    continue;
                }

                if let Some(dispatch_id) = ready_for_input_stall_dispatch_id {
                    match fail_dispatch_for_ready_for_input_stall(
                        &shared,
                        &dispatch_id,
                        &tmux_session_name,
                    )
                    .await
                    {
                        Ok(result) => {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ watcher marked post-work Ready-for-input stall as failed for {} / dispatch {} (card={:?}, card_marked={}, human_alert_sent={})",
                                tmux_session_name,
                                dispatch_id,
                                result.card_id,
                                result.card_marked,
                                result.human_alert_sent
                            );
                            // Skip rebind-origin (synthetic, no real user
                            // message) and user_msg_id == 0 (a TUI-direct turn
                            // with no anchored Discord user message): there is
                            // no message to react against, and
                            // `MessageId::new(0)` would panic.
                            if let Some(state) = ready_for_input_stall_inflight_snapshot
                                .as_ref()
                                .filter(|state| !state.rebind_origin && state.user_msg_id != 0)
                            {
                                let user_msg_id = serenity::MessageId::new(state.user_msg_id);
                                crate::services::discord::formatting::remove_reaction_raw(
                                    &http,
                                    channel_id,
                                    user_msg_id,
                                    '⏳',
                                )
                                .await;
                                crate::services::discord::formatting::add_reaction_raw(
                                    &http,
                                    channel_id,
                                    user_msg_id,
                                    '⚠',
                                )
                                .await;
                            }
                            finalize_pinned_watcher_exit(
                                &shared,
                                &watcher_provider,
                                channel_id,
                                ready_for_input_stall_inflight_snapshot.as_ref(),
                                "watcher_ready_for_input_stall",
                            )
                            .await;
                        }
                        Err(error) => {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ watcher failed to persist Ready-for-input stall failure for {} / dispatch {}: {}",
                                tmux_session_name,
                                dispatch_id,
                                error
                            );
                            let failure_notice = format!(
                                "⚠️ 작업 후 `Ready for input` 상태에서 멈췄지만 dispatch 실패 처리를 저장하지 못했습니다.\n사유: {}",
                                truncate_str(&error, 300)
                            );
                            match placeholder_msg_id {
                                Some(msg_id) => {
                                    rate_limit_wait(&shared, channel_id).await;
                                    let _ = crate::services::discord::http::edit_channel_message(
                                        &http,
                                        channel_id,
                                        msg_id,
                                        &failure_notice,
                                    )
                                    .await;
                                }
                                None => {
                                    let _ = crate::services::discord::http::send_channel_message(
                                        &http,
                                        channel_id,
                                        &failure_notice,
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                }
                clear_provider_overload_retry_state(channel_id);
                finish_monitor_auto_turn_if_claimed(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &mut monitor_auto_turn_claimed,
                    &mut monitor_auto_turn_finished,
                    &mut monitor_auto_turn_synthetic_msg_id,
                    &mut monitor_auto_turn_ledger_generation,
                )
                .await;
                continue;
            }

            // #3419 R2: turn-watchdog timeout fall-through (`!found_result` past the
            // fresh-idle / tmux-death / cancel / notice exits). Pre-#3419 this left
            // the turn UN-finalized (TurnFinalizer never ran, mailbox cancel_token
            // leaked, soft-queue wedged). Route through the SAME
            // `finish_restored_watcher_active_turn` entry normal completion uses (no
            // new authority; once-gate makes a later normal finalize idempotent).
            // Skip when paused/epoch-bumped or an error branch owns cleanup (below).
            // #3419 R3 (codex HIGH — drain re-acquire id-0 wedge, no steal): key the
            // decision on the LIVE MAILBOX active-turn id, not the on-disk inflight
            // (the mailbox token wedges the queue; re-acquire can mint an id-0
            // inflight while pinned A's token is still active, so R2's on-disk test
            // Skipped A and left it wedged). Finalize ONLY when the mailbox still
            // holds pinned A's token; a DIFFERENT live turn B / no active turn → Skip.
            // The submit is A's REAL pinned id via identity-guarded
            // `mailbox_finish_turn_if_matches`, so B can't be stolen / id-0 submitted.
            // #3419 B: NOT-active (idle OR cap expired) routes the stuck turn
            // through this C finalize; same predicate as the loop (single authority).
            if !found_result
                && !watcher_turn_still_active(
                    last_output_at.elapsed(),
                    turn_idle_timeout,
                    turn_start.elapsed(),
                    turn_timeout,
                )
                && !was_paused
                && pause_epoch.load(Ordering::Relaxed) == epoch_snapshot
                && !is_prompt_too_long
                && !is_auth_error
                && !is_provider_overloaded
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                // Wedge is the mailbox token; decide on its CURRENT active-turn id (different/absent = B took over / released).
                let mailbox_active_user_msg_id = shared
                    .mailbox(channel_id)
                    .snapshot()
                    .await
                    .active_user_message_id
                    .map(serenity::MessageId::get);
                match watcher_timeout_finalize_decision(
                    startup_inflight_snapshot.as_ref(),
                    mailbox_active_user_msg_id,
                    &tmux_session_name,
                ) {
                    TimeoutFinalizeDecision::Skip { pinned_user_msg_id } => {
                        tracing::warn!(
                            "  [{ts}] ⚠ #3419: watcher turn watchdog timed out for {tmux_session_name} after {}s, but pinned turn {pinned_user_msg_id} no longer holds the mailbox token (id-0 / no active turn / newer turn took over); NOT finalizing — the live turn finalizes itself",
                            turn_start.elapsed().as_secs()
                        );
                    }
                    TimeoutFinalizeDecision::Finalize { user_msg_id } => {
                        tracing::warn!(
                            "  [{ts}] ⚠ #3419: watcher turn watchdog timed out for {tmux_session_name} after {}s (pinned turn {user_msg_id} still holds the mailbox token); routing through the single-authority finalizer to release the token and drain the queue",
                            turn_start.elapsed().as_secs()
                        );
                        // Identity-matched clear: removes the row ONLY while still
                        // the pinned turn (same identity INCL. turn_start_offset, so
                        // clear key == decision key). A re-acquired id-0 / newer row →
                        // `UserMsgMismatch` no-op (drain frees the token, stale row untouched).
                        if let Some(pinned) = startup_inflight_snapshot.as_ref() {
                            let _ = crate::services::discord::inflight::clear_inflight_state_if_matches_identity(
                                &watcher_provider,
                                channel_id.get(),
                                &crate::services::discord::inflight::InflightTurnIdentity::from_state(pinned),
                            );
                        }
                        // finish_mailbox=true releases the watcher token (wedge fix);
                        // normal_completion=false; kickoff_queue=true admits the next
                        // turn. The REAL pinned id keys IDENTITY-GUARDED
                        // `mailbox_finish_turn_if_matches` (can't release a newer turn).
                        finish_restored_watcher_active_turn(
                            &shared,
                            &watcher_provider,
                            channel_id,
                            user_msg_id,
                            true,
                            false,
                            true,
                            startup_inflight_snapshot.as_ref().map(
                                crate::services::discord::turn_finalizer::SyntheticClaimSnapshot::from_row,
                            ),
                            "watcher turn watchdog timeout (#3419)",
                        )
                        .await;
                    }
                }
                finish_monitor_auto_turn_if_claimed(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &mut monitor_auto_turn_claimed,
                    &mut monitor_auto_turn_finished,
                    &mut monitor_auto_turn_synthetic_msg_id,
                    &mut monitor_auto_turn_ledger_generation,
                )
                .await;
                continue;
            }
        }

        // Discard partial data if paused while reading (even if now unpaused), or if the epoch
        // changed (a Discord turn claimed this data even when paused is now false).
        let paused_now = paused.load(Ordering::Relaxed);
        let epoch_changed_now = pause_epoch.load(Ordering::Relaxed) != epoch_snapshot;
        let deferred_monitor_ready =
            monitor_auto_turn_claimed && monitor_auto_turn_deferred && !paused_now;
        if (was_paused || paused_now || epoch_changed_now) && !deferred_monitor_ready {
            // Clean up placeholder if we created one (#3610/Phase-B defer: no emit_relay_delete here — tmux_watcher.rs raw-ratchet is at ceiling)
            if let Some(msg_id) = placeholder_msg_id {
                if watcher_should_delete_suppressed_placeholder(placeholder_from_restored_inflight)
                {
                    if let Err(error) = channel_id.delete_message(&http, msg_id).await {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ watcher pause/epoch placeholder cleanup failed for channel {} msg {}: {}",
                            channel_id.get(),
                            msg_id.get(),
                            error
                        );
                    }
                } else {
                    placeholder_from_restored_inflight = false;
                    last_edit_text.clear();
                }
            }
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
                &mut monitor_auto_turn_synthetic_msg_id,
                &mut monitor_auto_turn_ledger_generation,
            )
            .await;
            all_data.clear();
            all_data_start_offset = current_offset;
            all_data_fully_mirrored_to_session_relay = true;
            all_data_session_bound_relay_ack = None;
            all_data_first_forwarded_relay_sequence = None;
            continue;
        }

        // Handle prompt-too-long: kill session so next message creates a fresh one
        if is_prompt_too_long {
            clear_provider_overload_retry_state(channel_id);
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Prompt too long detected in watcher for {tmux_session_name}, killing session"
            );
            prompt_too_long_killed = true;

            let sess = tmux_session_name.clone();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::task::spawn_blocking(move || {
                    crate::services::termination_audit::record_termination_for_tmux(
                        &sess,
                        None,
                        "tmux_watcher",
                        "prompt_too_long",
                        Some("watcher cleanup: prompt too long"),
                        None,
                    );
                    record_tmux_exit_reason(&sess, "watcher cleanup: prompt too long");
                    crate::services::platform::tmux::kill_session(
                        &sess,
                        "watcher cleanup: prompt too long",
                    );
                }),
            )
            .await;

            let notice = "⚠️ 컨텍스트 한도 초과로 세션을 초기화했습니다. 다음 메시지부터 새 세션으로 처리됩니다.";
            match placeholder_msg_id {
                Some(msg_id) => {
                    rate_limit_wait(&shared, channel_id).await;
                    let _ = crate::services::discord::http::edit_channel_message(
                        &http, channel_id, msg_id, notice,
                    )
                    .await;
                }
                None => {
                    let _ = crate::services::discord::http::send_channel_message(
                        &http, channel_id, notice,
                    )
                    .await;
                }
            }
            // Don't break — let the watcher exit naturally when session-alive check fails
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
                &mut monitor_auto_turn_synthetic_msg_id,
                &mut monitor_auto_turn_ledger_generation,
            )
            .await;
            continue;
        }

        // Handle auth error: kill session and notify user to re-authenticate
        if is_auth_error {
            clear_provider_overload_retry_state(channel_id);
            let inflight_state = crate::services::discord::inflight::load_inflight_state(
                &watcher_provider,
                channel_id.get(),
            );
            let fallback_session_id = inflight_state
                .as_ref()
                .and_then(|state| state.session_id.as_deref());
            let dispatch_id =
                resolve_watcher_dispatch_id(&shared, channel_id, inflight_state.as_ref()).await;
            let auth_detail = auth_error_message
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("authentication expired");
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Auth error detected in watcher for {tmux_session_name}: {}",
                truncate_str(auth_detail, 300)
            );
            prompt_too_long_killed = true; // reuse flag to suppress duplicate "session ended" message

            clear_provider_session_for_retry(
                &shared,
                channel_id,
                &tmux_session_name,
                fallback_session_id,
            )
            .await;

            let sess = tmux_session_name.clone();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::task::spawn_blocking(move || {
                    crate::services::termination_audit::record_termination_for_tmux(
                        &sess,
                        None,
                        "tmux_watcher",
                        "auth_error",
                        Some("watcher cleanup: authentication failed"),
                        None,
                    );
                    record_tmux_exit_reason(&sess, "watcher cleanup: authentication failed");
                    crate::services::platform::tmux::kill_session(
                        &sess,
                        "watcher cleanup: authentication failed",
                    );
                }),
            )
            .await;

            let notice = format!(
                "⚠️ 인증이 만료되어 현재 dispatch를 실패 처리했습니다. 세션을 종료합니다.\n관리자가 CLI에서 재인증(`/login`)을 완료한 후 다시 디스패치해주세요.\n\n사유: {}",
                truncate_str(auth_detail, 300)
            );
            let notice_ok = match placeholder_msg_id {
                Some(msg_id) => {
                    rate_limit_wait(&shared, channel_id).await;
                    crate::services::discord::http::edit_channel_message(
                        &http, channel_id, msg_id, &notice,
                    )
                    .await
                    .is_ok()
                }
                None => {
                    crate::services::discord::http::send_channel_message(&http, channel_id, &notice)
                        .await
                        .is_ok()
                }
            };
            if !notice_ok {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ watcher: auth error notice failed before dispatch failure — preserving inflight for retry"
                );
                finish_monitor_auto_turn_if_claimed(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &mut monitor_auto_turn_claimed,
                    &mut monitor_auto_turn_finished,
                    &mut monitor_auto_turn_synthetic_msg_id,
                    &mut monitor_auto_turn_ledger_generation,
                )
                .await;
                continue;
            }
            // #897 round-3 Medium: skip reaction work for `rebind_origin`
            // inflights — their `user_msg_id=0` identifies no real Discord
            // message so issuing reactions against it just produces API
            // errors. The synthetic state was created by
            // `/api/inflight/rebind` to adopt a live tmux session. The same
            // holds for any user_msg_id == 0 (e.g. a TUI-direct turn) — there
            // is no message to react against and `MessageId::new(0)` panics.
            if let Some(state) = inflight_state
                .as_ref()
                .filter(|s| !s.rebind_origin && s.user_msg_id != 0)
            {
                let user_msg_id = serenity::MessageId::new(state.user_msg_id);
                crate::services::discord::formatting::remove_reaction_raw(
                    &http,
                    channel_id,
                    user_msg_id,
                    '⏳',
                )
                .await;
                crate::services::discord::formatting::add_reaction_raw(
                    &http,
                    channel_id,
                    user_msg_id,
                    '⚠',
                )
                .await;
            }
            finalize_pinned_watcher_exit(
                &shared,
                &watcher_provider,
                channel_id,
                inflight_state.as_ref(),
                "watcher_auth_error_exit",
            )
            .await;
            let failure_text = format!(
                "authentication expired; re-authentication required: {}",
                truncate_str(auth_detail, 300)
            );
            crate::services::discord::turn_bridge::fail_dispatch_auth_expired(
                shared.api_port,
                dispatch_id.as_deref(),
                &failure_text,
            )
            .await;
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
                &mut monitor_auto_turn_synthetic_msg_id,
                &mut monitor_auto_turn_ledger_generation,
            )
            .await;
            continue;
        }

        if is_provider_overloaded {
            let overload_message = provider_overload_message
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("provider overload detected");
            let inflight_state = crate::services::discord::inflight::load_inflight_state(
                &watcher_provider,
                channel_id.get(),
            );
            let retry_text = inflight_state
                .as_ref()
                .map(|state| state.user_text.clone())
                .filter(|text| !text.trim().is_empty());
            let fallback_session_id = inflight_state
                .as_ref()
                .and_then(|state| state.session_id.as_deref());
            let dispatch_id =
                resolve_watcher_dispatch_id(&shared, channel_id, inflight_state.as_ref()).await;

            let decision = retry_text
                .as_deref()
                .map(|text| record_provider_overload_retry(channel_id, text))
                .unwrap_or(ProviderOverloadDecision::Exhausted);
            let retry_notice = match &decision {
                ProviderOverloadDecision::Retry { attempt, delay, .. } => format!(
                    "⚠️ 모델 capacity 상태를 감지해 세션을 정리했습니다. {}분 후 자동 재시도합니다. ({}/{})",
                    delay.as_secs() / 60,
                    attempt,
                    PROVIDER_OVERLOAD_MAX_RETRIES
                ),
                ProviderOverloadDecision::Exhausted => format!(
                    "⚠️ 모델 capacity 상태가 계속되어 자동 재시도를 중단했습니다. 잠시 후 다시 시도해 주세요.\n\n사유: {}",
                    truncate_str(overload_message, 300)
                ),
            };

            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Provider overload detected in watcher for {}: {}",
                tmux_session_name,
                overload_message
            );
            prompt_too_long_killed = true;

            clear_provider_session_for_retry(
                &shared,
                channel_id,
                &tmux_session_name,
                fallback_session_id,
            )
            .await;

            let sess = tmux_session_name.clone();
            let termination_reason = match &decision {
                ProviderOverloadDecision::Retry { .. } => "provider_overload_retry",
                ProviderOverloadDecision::Exhausted => "provider_overload_exhausted",
            };
            let termination_detail = format!("watcher cleanup: {overload_message}");
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::task::spawn_blocking(move || {
                    crate::services::termination_audit::record_termination_for_tmux(
                        &sess,
                        None,
                        "tmux_watcher",
                        termination_reason,
                        Some(&termination_detail),
                        None,
                    );
                    record_tmux_exit_reason(&sess, &termination_detail);
                    crate::services::platform::tmux::kill_session(&sess, &termination_detail);
                }),
            )
            .await;

            let notice_ok = match placeholder_msg_id {
                Some(msg_id) => {
                    rate_limit_wait(&shared, channel_id).await;
                    crate::services::discord::http::edit_channel_message(
                        &http,
                        channel_id,
                        msg_id,
                        &retry_notice,
                    )
                    .await
                    .is_ok()
                }
                None => crate::services::discord::http::send_channel_message(
                    &http,
                    channel_id,
                    &retry_notice,
                )
                .await
                .is_ok(),
            };
            if !notice_ok {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ watcher: provider overload notice failed before retry/failure handling — preserving inflight for retry"
                );
                finish_monitor_auto_turn_if_claimed(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &mut monitor_auto_turn_claimed,
                    &mut monitor_auto_turn_finished,
                    &mut monitor_auto_turn_synthetic_msg_id,
                    &mut monitor_auto_turn_ledger_generation,
                )
                .await;
                continue;
            }

            // #897 round-3 Medium: skip reaction + retry scheduling for
            // `rebind_origin` inflights — they have no real user message
            // to react against and no real user text to re-prompt. The same
            // holds for user_msg_id == 0 (e.g. a TUI-direct turn): no message
            // to react against, and `MessageId::new(0)` would panic.
            if let Some(state) = inflight_state
                .as_ref()
                .filter(|s| !s.rebind_origin && s.user_msg_id != 0)
            {
                let user_msg_id = serenity::MessageId::new(state.user_msg_id);
                crate::services::discord::formatting::remove_reaction_raw(
                    &http,
                    channel_id,
                    user_msg_id,
                    '⏳',
                )
                .await;
                if matches!(&decision, ProviderOverloadDecision::Exhausted) {
                    crate::services::discord::formatting::add_reaction_raw(
                        &http,
                        channel_id,
                        user_msg_id,
                        '⚠',
                    )
                    .await;
                }
            }
            finalize_pinned_watcher_exit(
                &shared,
                &watcher_provider,
                channel_id,
                inflight_state.as_ref(),
                "watcher_provider_overload_exit",
            )
            .await;

            match decision {
                ProviderOverloadDecision::Retry {
                    attempt,
                    delay,
                    fingerprint,
                } => {
                    if let Some(retry_text) = retry_text {
                        // A turn with no anchored user message (rebind_origin or
                        // user_msg_id == 0, e.g. a TUI-direct turn) has no
                        // message to re-prompt against; clear retry state
                        // instead of building `MessageId::new(0)` (panics).
                        if let Some(state) = inflight_state
                            .as_ref()
                            .filter(|s| !s.rebind_origin && s.user_msg_id != 0)
                        {
                            schedule_provider_overload_retry(
                                shared.clone(),
                                http.clone(),
                                watcher_provider.clone(),
                                channel_id,
                                serenity::MessageId::new(state.user_msg_id),
                                retry_text,
                                attempt,
                                delay,
                                fingerprint,
                            );
                        } else {
                            clear_provider_overload_retry_state(channel_id);
                        }
                    } else {
                        clear_provider_overload_retry_state(channel_id);
                    }
                }
                ProviderOverloadDecision::Exhausted => {
                    let failure_text = format!(
                        "provider overloaded after {} auto-retries: {}",
                        PROVIDER_OVERLOAD_MAX_RETRIES,
                        truncate_str(overload_message, 300)
                    );
                    crate::services::discord::turn_bridge::fail_dispatch_with_retry(
                        shared.api_port,
                        dispatch_id.as_deref(),
                        &failure_text,
                    )
                    .await;
                }
            }
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
                &mut monitor_auto_turn_synthetic_msg_id,
                &mut monitor_auto_turn_ledger_generation,
            )
            .await;
            continue;
        }

        // Final guard: re-check epoch and turn_delivered right before relay.
        // Closes the race window where a Discord turn starts between the epoch check
        // above (line 277) and this relay — the turn_bridge may have already delivered
        // the same response to its own placeholder.
        let paused_now = paused.load(Ordering::Relaxed);
        let epoch_changed_now = pause_epoch.load(Ordering::Relaxed) != epoch_snapshot;
        let turn_delivered_now = turn_delivered.load(Ordering::Relaxed);
        let deferred_monitor_ready =
            monitor_auto_turn_claimed && monitor_auto_turn_deferred && !paused_now;
        if should_suppress_relay_before_emit(
            paused_now,
            epoch_changed_now,
            turn_delivered_now,
            deferred_monitor_ready,
        ) {
            if let Some(msg_id) = placeholder_msg_id {
                let _ = delete_nonterminal_placeholder(
                    &http,
                    channel_id,
                    &shared,
                    &watcher_provider,
                    &tmux_session_name,
                    msg_id,
                    "watcher_late_epoch_guard_cleanup",
                )
                .await;
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 👁 Late epoch/delivered guard: suppressed duplicate relay for {}",
                tmux_session_name
            );
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
                &mut monitor_auto_turn_synthetic_msg_id,
                &mut monitor_auto_turn_ledger_generation,
            )
            .await;
            discard_watcher_pending_buffer_after_suppressed_turn(
                &mut all_data,
                &mut all_data_start_offset,
                &mut all_data_fully_mirrored_to_session_relay,
                &mut all_data_session_bound_relay_ack,
                &mut all_data_first_forwarded_relay_sequence,
                current_offset,
            );
            continue;
        }

        if watcher_should_yield_to_active_bridge_turn(
            &watcher_provider,
            channel_id,
            &tmux_session_name,
            data_start_offset,
            current_offset,
        ) {
            let matched_reattach = matching_recent_watcher_reattach_offset(
                channel_id,
                &tmux_session_name,
                data_start_offset,
            );
            let reattach_detail = matched_reattach.as_ref().map(|r| {
                format!(
                    "{} range {}..{} matches reattach at {}",
                    tmux_session_name, data_start_offset, current_offset, r.offset
                )
            });
            let ctx = PlaceholderSuppressContext {
                origin: PlaceholderSuppressOrigin::ActiveBridgeTurnGuard,
                provider: &watcher_provider,
                placeholder_msg_id,
                response_sent_offset,
                last_edit_text: &last_edit_text,
                inflight_state: None,
                has_active_turn: false,
                tmux_session_name: &tmux_session_name,
                task_notification_kind: None,
                reattach_offset_match: matched_reattach.is_some(),
            };
            apply_placeholder_suppression(
                &http,
                channel_id,
                &shared,
                &watcher_provider,
                &tmux_session_name,
                placeholder_msg_id,
                ctx.origin,
                decide_placeholder_suppression(&ctx),
                reattach_detail.as_deref(),
            )
            .await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 👁 Active bridge turn guard: suppressed duplicate relay for {} (range {}..{})",
                tmux_session_name,
                data_start_offset,
                current_offset
            );
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
                &mut monitor_auto_turn_synthetic_msg_id,
                &mut monitor_auto_turn_ledger_generation,
            )
            .await;
            discard_watcher_pending_buffer_after_suppressed_turn(
                &mut all_data,
                &mut all_data_start_offset,
                &mut all_data_fully_mirrored_to_session_relay,
                &mut all_data_session_bound_relay_ack,
                &mut all_data_first_forwarded_relay_sequence,
                current_offset,
            );
            continue;
        }

        // Duplicate-relay guard: if we already relayed from this same data
        // range, suppress. Use strict `<` so output starting exactly at the
        // previous boundary is treated as the next turn rather than a re-read.
        if let Ok(meta) = std::fs::metadata(&output_path) {
            let observed_output_end = meta.len();
            reset_stale_relay_watermark_if_output_regressed(
                &shared,
                channel_id,
                &tmux_session_name,
                observed_output_end,
                "pre_local_dedupe",
            );
            reset_stale_local_relay_offset_if_output_regressed(
                &mut last_relayed_offset,
                &mut last_observed_generation_mtime_ns,
                channel_id,
                &tmux_session_name,
                observed_output_end,
                "pre_local_dedupe",
            );
        }
        if let Some(prev_offset) = last_relayed_offset {
            if data_start_offset < prev_offset {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] 👁 Duplicate relay guard: suppressed re-relay for {} (data_start={}, last_relayed={:?})",
                    tmux_session_name,
                    data_start_offset,
                    last_relayed_offset,
                );
                if let Some(msg_id) = placeholder_msg_id {
                    let _ = delete_nonterminal_placeholder(
                        &http,
                        channel_id,
                        &shared,
                        &watcher_provider,
                        &tmux_session_name,
                        msg_id,
                        "watcher_duplicate_relay_guard_cleanup",
                    )
                    .await;
                }
                finish_monitor_auto_turn_if_claimed(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &mut monitor_auto_turn_claimed,
                    &mut monitor_auto_turn_finished,
                    &mut monitor_auto_turn_synthetic_msg_id,
                    &mut monitor_auto_turn_ledger_generation,
                )
                .await;
                discard_watcher_pending_buffer_after_suppressed_turn(
                    &mut all_data,
                    &mut all_data_start_offset,
                    &mut all_data_fully_mirrored_to_session_relay,
                    &mut all_data_session_bound_relay_ack,
                    &mut all_data_first_forwarded_relay_sequence,
                    current_offset,
                );
                continue;
            }
        }

        // Detect stale session resume failure in watcher output
        let is_stale_resume = stale_resume_detected;
        if is_stale_resume {
            clear_provider_overload_retry_state(channel_id);
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ Watcher detected stale session resume failure (channel {}), clearing session_id",
                channel_id
            );
            let stale_sid = {
                let mut data = shared.core.lock().await;
                let old = data
                    .sessions
                    .get(&channel_id)
                    .and_then(|s| s.session_id.clone());
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    session.clear_provider_session();
                }
                old
            };
            // Clear DB session_id
            {
                let hostname = crate::services::platform::hostname_short();
                let session_key = format!("{}:{}", hostname, tmux_session_name);
                crate::services::discord::adk_session::clear_provider_session_id(
                    &session_key,
                    shared.api_port,
                )
                .await;
            }
            if let Some(ref sid) = stale_sid {
                let _ = crate::services::discord::internal_api::clear_stale_session_id(sid).await;
            }
            crate::services::termination_audit::record_termination_for_tmux(
                &tmux_session_name,
                None,
                "tmux_watcher",
                "stale_resume_retry",
                Some("stale session resume detected — forcing fresh session before auto-retry"),
                None,
            );
            record_tmux_exit_reason(
                &tmux_session_name,
                "stale session resume detected — forcing fresh session before auto-retry",
            );
            crate::services::platform::tmux::kill_session(
                &tmux_session_name,
                "stale session resume detected — forcing fresh session before auto-retry",
            );
            // Replace placeholder with recovery notice (don't delete — avoids visual gap)
            if let Some(msg_id) = placeholder_msg_id {
                let _ = crate::services::discord::http::edit_channel_message(
                    &http,
                    channel_id,
                    msg_id,
                    "↻ 세션 복구 중... 잠시 후 자동으로 이어갑니다.",
                )
                .await;
            }
            // Auto-retry: persist Discord history for LLM injection, then queue the
            // original user message as an internal follow-up instead of self-routing
            // through /api/discord/send announce.
            //
            // #897 round-4 Medium: a `rebind_origin` inflight has no real
            // user message or text to retry with (`user_msg_id=0`,
            // user_text="/api/inflight/rebind"), so auto-retry would
            // enqueue a garbage internal follow-up. Skip the retry; the
            // operator is expected to re-invoke `/api/inflight/rebind`
            // once the tmux session is healthy again.
            match crate::services::discord::inflight::load_inflight_state(
                &watcher_provider,
                channel_id.get(),
            ) {
                Some(state) if state.rebind_origin || state.user_msg_id == 0 => {
                    // rebind_origin and user_msg_id == 0 (e.g. a TUI-direct
                    // turn) both have no anchored user message to retry against;
                    // `MessageId::new(0)` would panic.
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ Watcher auto-retry skipped for channel {} — inflight has no user message to retry",
                        channel_id
                    );
                }
                Some(state) => {
                    crate::services::discord::tmux_overload_retry::schedule_discord_retry_with_history_completion_release(
                        shared.clone(),
                        http.clone(),
                        watcher_provider.clone(),
                        channel_id,
                        serenity::MessageId::new(state.user_msg_id),
                        state.user_text,
                    );
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ↻ Watcher auto-retry queued for channel {}",
                        channel_id
                    );
                }
                None => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ Watcher auto-retry skipped: inflight state missing for channel {}",
                        channel_id
                    );
                }
            }
            // Skip normal response relay
            full_response = String::new();
        }

        let prompt_anchor_present_before_relay =
            crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
                watcher_provider.as_str(),
                &tmux_session_name,
                channel_id.get(),
            )
            .is_some();
        // #3041 P1-4 codex: snapshot the external-input lease ONCE under a single STATE
        // lock and derive BOTH the presence bool and the generation from that one atomic
        // read. Two separate accessor calls (present + generation) re-lock STATE between
        // them, so a concurrently-started turn could record a NEWER same-key lease in the
        // gap — leaving the bool reflecting turn-1 but the generation captured from
        // turn-2's lease (present/generation TOCTOU). The post-delivery clear uses this
        // generation so it only removes the EXACT lease this relay consumed; a NEWER
        // same-key lease recorded by a concurrently-started turn during the slow send
        // survives (no stale-snapshot clobber).
        let external_input_lease_before_relay_snapshot =
            crate::services::tui_prompt_dedupe::external_input_relay_lease(
                watcher_provider.as_str(),
                &tmux_session_name,
                channel_id.get(),
            );
        let external_input_lease_before_relay =
            external_input_lease_before_relay_snapshot.is_some();
        let external_input_lease_generation_before_relay =
            external_input_lease_before_relay_snapshot
                .as_ref()
                .map(|lease| lease.generation);
        let inflight_before_relay = crate::services::discord::inflight::load_inflight_state(
            &watcher_provider,
            channel_id.get(),
        );
        let inflight_identity_before_relay =
            matching_watcher_turn_identity(inflight_before_relay.as_ref(), &tmux_session_name);
        let should_adopt_inflight_terminal_message_ids = !external_input_lease_before_relay
            || watcher_inflight_represents_external_input(inflight_before_relay.as_ref());
        // #3142: skip adopting the pre-relay snapshot's terminal message ids when it
        // is a STALE NEWER follow-up turn (turn_start_offset >= current_offset) — else
        // the older range aliases the newer turn's status panel. Uses the id==0-
        // INCLUSIVE anchor variant (None 2nd arg sound: is_some_and → false) so
        // external-input turns are caught; in-range id==0 turns adopt (OFFSET-keyed).
        let inflight_before_relay_is_stale_newer_turn =
            committed_anchor_cleanup_is_stale_for_newer_turn(
                inflight_before_relay.as_ref(),
                None,
                &tmux_session_name,
                current_offset,
            );
        if should_adopt_inflight_terminal_message_ids
            && !inflight_before_relay_is_stale_newer_turn
            && let Some(inflight) = inflight_before_relay.as_ref()
        {
            adopt_watcher_terminal_message_ids_from_inflight(
                &mut placeholder_msg_id,
                &mut placeholder_from_restored_inflight,
                &mut status_panel_msg_id,
                inflight,
                &tmux_session_name,
            );
            if single_message_panel_footer_mode {
                status_panel_msg_id = None;
            }
        }
        if discard_restored_response_seed_before_no_inflight_terminal_relay(
            &mut full_response,
            &mut response_sent_offset,
            &mut last_edit_text,
            &restored_response_seed,
            inflight_before_relay.is_some(),
            fresh_assistant_text_seen,
        ) {
            tracing::info!(
                provider = %watcher_provider.as_str(),
                channel = channel_id.get(),
                tmux_session = %tmux_session_name,
                restored_response_seed_len = restored_response_seed.len(),
                fresh_response_len = full_response.len(),
                "watcher: discarded restored response seed before no-inflight terminal relay"
            );
        }
        let has_assistant_response = !full_response.trim().is_empty();
        let current_response = full_response.get(response_sent_offset..).unwrap_or("");
        let has_current_response = !current_response.trim().is_empty();

        // #3041 P1-3 (Part a, B1 — FRAME-CARRIED, codex): the watcher's
        // AUTHORITATIVE consumed-terminal END is NO LONGER persisted to the inflight
        // FILE here. The old inflight-persist Part (a) was RACY (the sink read the
        // end back from the file in `deliver_response`, a separate read/write across
        // the relay's async drain). It is REPLACED by the frame-carried commit
        // fence: the RESULT-bearing `StreamFrame` itself carries `consumed_end` +
        // the pinned turn identity (forwarded during line collection above), and the
        // sink advances `confirmed_end_offset` identity-gated on its CONFIRMED POST —
        // POST + advance atomic per-frame, no file race. See
        // `watcher_terminal_commit_fence` (producer) and
        // `advance_offset_for_confirmed_delegated_terminal` (sink).

        let recent_stop_for_output =
            recent_turn_stop_for_watcher_range(channel_id, &tmux_session_name, data_start_offset);
        let inflight_missing_before_relay = inflight_before_relay.is_none();
        // #3003 single terminal chokepoint: every turn termination converges on
        // this terminal-relay block, including a fast `result` that breaks out of
        // the streaming loop before the periodic interval reclaim runs again.
        // Reclaim a watcher-created external-input panel here when the turn will
        // not finalize it — no assistant text (status-only/no-response), a recent
        // turn-stop tombstone, or a cleared inflight (stop/cancel). A turn that has
        // assistant text, is not stopped, and still has its inflight is left for
        // the committed relay path to complete (or a failed send to preserve for
        // retry). Runs before every terminal sub-path (stale-id clear, silent,
        // recent-stop suppression, no-response).
        //
        // The no-response arm excludes task-notification turns (codex P2 r15): a
        // status-only `task_notification_kind` turn is relay-suppressed-and-
        // committed below, so `complete_watcher_status_panel_v2` still finalizes
        // its panel — deleting it here would erase a panel that is about to
        // complete. Stopped/abandoned such turns are still reclaimed via the
        // abandon arm.
        // #3351: same-turn relay placeholder reclaim rides the identical orphan
        // context; gated so a placeholder already edited into a real response (or
        // a turn with assistant text — owned by the recent-stop/stale-clear arms)
        // is never deleted here.
        let terminal_placeholder_reclaim = watcher_should_reclaim_orphan_turn_placeholder(
            turn_is_external_input_for_session,
            placeholder_msg_id,
            has_assistant_response,
            &last_edit_text,
        );
        let terminal_orphan_context = turn_is_external_input_for_session
            && (status_panel_msg_id.is_some() || terminal_placeholder_reclaim)
            && ((!has_assistant_response && task_notification_kind.is_none())
                || watcher_external_input_turn_abandoned(
                    &watcher_provider,
                    channel_id,
                    &tmux_session_name,
                    &output_path,
                    data_start_offset,
                    turn_identity_for_panel.as_ref(),
                ));
        let terminal_panel_reclaim_committed =
            if terminal_orphan_context && status_panel_msg_id.is_some() {
                cleanup_orphan_external_input_status_panel(
                    &http,
                    &shared,
                    channel_id,
                    &mut status_panel_msg_id,
                    &watcher_provider,
                    &tmux_session_name,
                    turn_is_external_input_for_session,
                )
                .await
            } else {
                true
            };
        if terminal_orphan_context && terminal_placeholder_reclaim {
            reclaim_orphan_external_input_placeholder(
                &http,
                &shared,
                channel_id,
                &mut placeholder_msg_id,
                &mut placeholder_from_restored_inflight,
                &mut last_edit_text,
                &watcher_provider,
                &tmux_session_name,
            )
            .await;
        }
        let inflight_silent_turn = inflight_before_relay
            .as_ref()
            .map(|state| state.silent_turn)
            .unwrap_or(false);
        if watcher_should_clear_stale_terminal_message_ids(
            inflight_before_relay.is_some(),
            has_assistant_response,
            placeholder_msg_id,
        ) {
            if let Some(stale_msg_id) = placeholder_msg_id {
                tracing::info!(
                    provider = %watcher_provider.as_str(),
                    channel = channel_id.get(),
                    tmux_session = %tmux_session_name,
                    stale_placeholder_msg_id = stale_msg_id.get(),
                    status_panel_msg_id = status_panel_msg_id.map(|id| id.get()).unwrap_or(0),
                    "watcher: clearing stale terminal message ids before no-inflight terminal relay"
                );
            }
            placeholder_msg_id = None;
            // #3003 (codex P2 r12): only drop the local panel id if the terminal
            // reclaim above actually committed its delete. When the delete failed
            // transiently the id is held for retry (the persisted id, if any, also
            // survives for the sweeper); nulling it here would strand the still-
            // visible "계속 처리 중" panel with no handle.
            if terminal_panel_reclaim_committed {
                status_panel_msg_id = None;
            }
            placeholder_from_restored_inflight = false;
            last_edit_text.clear();
        }
        if inflight_silent_turn && has_assistant_response {
            // Headless silent trigger (metadata.silent=true) — suppress assistant
            // text relay to the channel entirely, but keep the watcher state
            // machine advancing so the turn finalizes normally. Lifecycle/error/
            // cancel notifications continue to post via their own paths.
            let cleanup_committed = if let Some(msg_id) = placeholder_msg_id {
                delete_nonterminal_placeholder(
                    &http,
                    channel_id,
                    &shared,
                    &watcher_provider,
                    &tmux_session_name,
                    msg_id,
                    "watcher_silent_turn_suppress_cleanup",
                )
                .await
                .is_committed()
            } else {
                true
            };
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🤫 watcher: silent_turn suppressed terminal output for channel {} (tmux={}, range {}..{})",
                channel_id.get(),
                tmux_session_name,
                data_start_offset,
                current_offset
            );
            if cleanup_committed {
                last_relayed_offset = Some(current_offset);
                last_observed_generation_mtime_ns =
                    Some(read_generation_file_mtime_ns(&tmux_session_name));
                advance_watcher_confirmed_end(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &tmux_session_name,
                    current_offset,
                    "src/services/discord/tmux.rs:silent_turn_suppressed_terminal_output",
                );
            }
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
                &mut monitor_auto_turn_synthetic_msg_id,
                &mut monitor_auto_turn_ledger_generation,
            )
            .await;
            continue;
        }
        if should_suppress_terminal_output_after_recent_stop(
            has_assistant_response,
            inflight_missing_before_relay,
            recent_stop_for_output.is_some(),
        ) {
            let stop = recent_stop_for_output.expect("recent stop checked above");
            let cleanup_committed = if let Some(msg_id) = placeholder_msg_id {
                if watcher_should_delete_suppressed_placeholder(placeholder_from_restored_inflight)
                {
                    let committed = delete_nonterminal_placeholder(
                        &http,
                        channel_id,
                        &shared,
                        &watcher_provider,
                        &tmux_session_name,
                        msg_id,
                        "watcher_terminal_recent_stop_cleanup",
                    )
                    .await
                    .is_committed();
                    if committed {
                        placeholder_from_restored_inflight = false;
                        last_edit_text.clear();
                    }
                    committed
                } else {
                    placeholder_from_restored_inflight = false;
                    last_edit_text.clear();
                    true
                }
            } else {
                true
            };
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 🛑 watcher: suppressed terminal output for channel {} after recent turn stop ({}, tmux={}, range {}..{})",
                channel_id.get(),
                stop.reason,
                tmux_session_name,
                data_start_offset,
                current_offset
            );
            if cleanup_committed {
                last_relayed_offset = Some(current_offset);
                // #1270 codex P2: snapshot the current `.generation` mtime so
                // the local regression check has a real baseline (see the
                // matching snapshot in the rotation path).
                last_observed_generation_mtime_ns =
                    Some(read_generation_file_mtime_ns(&tmux_session_name));
                advance_watcher_confirmed_end(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &tmux_session_name,
                    current_offset,
                    "src/services/discord/tmux.rs:cancel_tombstone_suppressed_terminal_output",
                );
            }
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
                &mut monitor_auto_turn_synthetic_msg_id,
                &mut monitor_auto_turn_ledger_generation,
            )
            .await;
            continue;
        }

        // #3017 single output-offset authority — cross-actor relay dedup for
        // the inflight-less wake / idle-background / monitor turn (E-13). When
        // there is NO inflight, the idle-JSONL relay
        // (`session_relay_sink::run_idle_jsonl_relay_loop`) reads the SAME
        // JSONL and can relay this exact range. If it already committed the
        // authoritative relayed offset at/past this turn's END, that range was
        // already delivered to Discord — so the watcher must SKIP to avoid the
        // duplicate `[E2E:E13:WAKE]`. This is deliberately gated on
        // `inflight_missing_before_relay`: a normal Discord-origin turn
        // (inflight present) keeps the watcher as the sole relay owner and is
        // NEVER suppressed by the shared watermark (the long-standing
        // invariant), so this only de-duplicates the un-owned wake/idle paths.
        if inflight_missing_before_relay
            && has_current_response
            && current_offset > turn_data_start_offset
        {
            // Codex P1: a stale-high `confirmed_end_offset` left by a PREVIOUS
            // wrapper (before any actor ran the regression reset) would make a
            // FRESH wake/idle response with a lower `current_offset` look already
            // delivered and get dropped. Run the SAME generation-aware
            // regression reset BEFORE reading the watermark (a truncated /
            // respawned JSONL resets it to 0 for a fresh wrapper), exactly as
            // the idle relay path does. The unconditional pre-relay reset below
            // at `pre_relay` is for the general path; this one guards the
            // no-inflight dedup read specifically.
            if let Ok(meta) = std::fs::metadata(&output_path) {
                reset_stale_relay_watermark_if_output_regressed(
                    &shared,
                    channel_id,
                    &tmux_session_name,
                    meta.len(),
                    "no_inflight_dedup",
                );
            }
            // Codex r6 P2: `reset_stale_relay_watermark_if_output_regressed` only resets when the
            // current EOF is LOWER than the stored watermark. A respawned same-named wrapper whose
            // fresh JSONL ALREADY grew PAST the prior watermark would NOT trip that EOF-regression
            // check → fresh output wrongly suppressed. Independently reset when the `.generation`
            // mtime CHANGED since commit (fresh wrapper = different byte stream). Shared with idle.
            reset_relay_watermark_on_generation_change(
                &shared,
                channel_id,
                &tmux_session_name,
                "watcher_no_inflight_dedup",
            );
            // Read-only check against the authority: if the sink (idle-JSONL relay or the watcher's
            // own session-bound delegation) already COMMITTED at/past this turn's END, that range
            // was delivered → skip the duplicate. The watcher does NOT claim here (claim + relay
            // failure would mark delivered while dropping it); it advances only on a CONFIRMED relay
            // at `advance_watcher_confirmed_end` below.
            // Codex r5 P2: compare against this TURN's consumed terminal end, NOT the whole read
            // batch end (`current_offset`) — a batch can hold a completed turn PLUS a later turn's
            // trailing JSONL; `process_watcher_lines` stops at the first result, so the turn ends at
            // `current_offset - all_data.len()` (== the normal commit path's
            // `runtime_binding_candidate_offset`). Using `current_offset` would MISS a prior commit
            // at that smaller consumed end and re-relay the already-committed terminal.
            let turn_consumed_offset = terminal_event_consumed_offset(current_offset, &all_data);
            let committed = dr::effective_committed_offset(
                &shared,
                &watcher_provider,
                channel_id,
                &tmux_session_name,
            );
            if committed >= turn_consumed_offset && turn_consumed_offset > turn_data_start_offset {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 👁 watcher: suppressed no-inflight terminal relay for channel {} — range {}..{} already committed by another relay actor (offset authority, committed_end={})",
                    channel_id.get(),
                    turn_data_start_offset,
                    turn_consumed_offset,
                    committed
                );
                last_relayed_offset = Some(current_offset);
                last_observed_generation_mtime_ns =
                    Some(read_generation_file_mtime_ns(&tmux_session_name));
                finish_monitor_auto_turn_if_claimed(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &mut monitor_auto_turn_claimed,
                    &mut monitor_auto_turn_finished,
                    &mut monitor_auto_turn_synthetic_msg_id,
                    &mut monitor_auto_turn_ledger_generation,
                )
                .await;
                continue;
            }
        }

        // Relay coordination is limited to serialization plus telemetry. The
        // local `last_relayed_offset` guard handles self-duplicate relays, and
        // watcher registration enforces one live owner per tmux session. Do
        // not suppress a valid owner solely because another watcher advanced
        // the shared confirmed_end watermark.
        let relay_coord = shared.tmux_relay_coord(channel_id);
        if let Ok(meta) = std::fs::metadata(&output_path) {
            reset_stale_relay_watermark_if_output_regressed(
                &shared,
                channel_id,
                &tmux_session_name,
                meta.len(),
                "pre_relay",
            );
        }
        // CAS the emission slot. `0` = free; any non-zero value = a watcher
        // is mid-emission with that start offset. `.max(1)` guarantees the
        // stored value is non-zero even when `data_start_offset == 0`.
        let slot_claim_token = data_start_offset.max(1);
        if relay_coord
            .relay_slot
            .compare_exchange(
                0,
                slot_claim_token,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 👁 Cross-watcher serialization: slot busy, skipped relay for {} (data_start={})",
                tmux_session_name,
                data_start_offset
            );
            if let Some(msg_id) = placeholder_msg_id {
                let _ = delete_nonterminal_placeholder(
                    &http,
                    channel_id,
                    &shared,
                    &watcher_provider,
                    &tmux_session_name,
                    msg_id,
                    "watcher_cross_watcher_slot_busy_cleanup",
                )
                .await;
            }
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
                &mut monitor_auto_turn_synthetic_msg_id,
                &mut monitor_auto_turn_ledger_generation,
            )
            .await;
            continue;
        }

        // #2840: the CAS above acquired the emission slot. Hold it via an RAII
        // guard so ANY exit from here on (early `continue`, `?`, panic, task
        // abort) frees the slot on Drop instead of wedging the channel for
        // every replacement watcher. The two intended release points below call
        // `slot_guard.release()` explicitly to preserve their timing.
        let mut slot_guard = RelaySlotGuard::new(relay_coord.relay_slot.clone());

        // Send the terminal response to Discord, or delegate it to the
        // supervisor-owned StreamRelay sink when the matched session's
        // inflight metadata says session-bound delivery owns this terminal
        // envelope.
        let relay_decision = terminal_relay_decision(
            has_assistant_response,
            task_notification_kind,
            assistant_text_seen,
        );
        debug_assert!(
            !relay_decision.should_enqueue_notify_outbox,
            "monitor/task-notification watcher relays must not use notify-bot outbox"
        );
        let session_bound_discord_delivery_enabled =
            crate::services::discord::session_relay_sink::session_bound_discord_delivery_enabled();
        let relay_producer_session_name = cached_relay_producer
            .as_ref()
            .map(|producer| producer.session_name());
        // #3579: INIT the ack outcome to the watcher-owned NON-attempt sentinel.
        // When `session_bound_relay_should_own_terminal_delivery` returns false
        // (e.g. relay_owner=Watcher) the ack-wait block below is SKIPPED and this
        // init value is what the flight recorder logs as `frame_ack_outcome`. It
        // is BENIGN (the watcher owns terminal delivery; the sink-delegated ack
        // path is intentionally not taken) — distinct from `MissingTarget`, which
        // `wait_for_session_bound_relay_delivery_ack` returns only when the block
        // ACTUALLY RAN but had no target (a real unconfirmed). Before #3579 this
        // init was `MissingTarget`, conflating the two and inflating relay-loss
        // tallies. Behavior is unchanged: `NotAttempted` folds to the same
        // `DeliveryOutcome::Unknown` as `MissingTarget` for the resend decision.
        let mut session_bound_ack_outcome = SessionBoundRelayAckOutcome::NotAttempted;
        let session_bound_terminal_delivery_attempted =
            session_bound_relay_should_own_terminal_delivery(
                relay_decision.should_direct_send,
                session_bound_discord_delivery_enabled,
                session_bound_relay_turn_fully_mirrored,
                relay_producer_session_name,
                inflight_before_relay.as_ref(),
                &tmux_session_name,
            );
        let session_bound_relay_owns_terminal_delivery =
            if session_bound_terminal_delivery_attempted {
                let ack_outcome = wait_for_session_bound_relay_delivery_ack(
                    all_data_session_bound_relay_ack.as_ref(),
                    std::time::Duration::from_secs(10),
                )
                .await;
                let ack_outcome = session_bound_ack_outcome_after_resolve_time_mirror_check(
                    ack_outcome,
                    &mut session_bound_relay_turn_fully_mirrored,
                    all_data_session_bound_relay_ack.as_ref(),
                    session_bound_relay_turn_first_forwarded_sequence,
                );
                session_bound_ack_outcome = ack_outcome;
                let delivered = session_bound_relay_turn_fully_mirrored
                    && matches!(ack_outcome, SessionBoundRelayAckOutcome::Delivered);
                if !delivered {
                    tracing::warn!(
                        provider = watcher_provider.as_str(),
                        channel = channel_id.get(),
                        tmux_session = %tmux_session_name,
                        ?ack_outcome,
                        "session-bound StreamRelay terminal delivery was not acknowledged"
                    );
                }
                delivered
            } else {
                false
            };
        let prompt_anchor_present = prompt_anchor_present_before_relay;
        let ssh_direct_pending = prompt_anchor_present
            || crate::services::tui_prompt_dedupe::is_ssh_direct_observation_pending(
                watcher_provider.as_str(),
                &tmux_session_name,
            );
        let external_input_lease_present = external_input_lease_before_relay;
        let recent_stop_reason =
            recent_turn_stop_for_watcher_range(channel_id, &tmux_session_name, data_start_offset)
                .map(|stop| stop.reason);
        // #3042: an ownerless turn (`inflight_present=false` or
        // `relay_owner_kind=none`, the post-restart restore_inflight gap) has no
        // reliable terminal-commit ACK path, so a `TimedOut` there must not drive
        // the watcher-direct re-send. Mirror the relay_flight_recorder fields used
        // below so the gate sees exactly what is logged.
        let relay_owner_present = inflight_before_relay.as_ref().is_some_and(|state| {
            !matches!(
                state.effective_relay_owner_kind(),
                crate::services::discord::inflight::RelayOwnerKind::None
            )
        });
        let watcher_direct_fallback_intended = watcher_should_direct_send_after_session_bound_ack(
            relay_decision.should_direct_send,
            session_bound_ack_outcome,
            relay_owner_present,
        );
        // #3041 P1-3 (Part b, §3.2): REPLACE the blind re-send. Before re-sending the
        // terminal body after a non-`Delivered` session-bound ACK (the
        // `relay_terminal_ack_timeout` duplicate vector), reconcile against the offset
        // authority FIRST, over the SAME consumed range `[data_start_offset, terminal_event_consumed_offset(current_offset, all_data))`.
        // Part (a) advances `committed_relay_offset` to the watcher's own `end` on a
        // confirmed sink delivery, so the consult is exact: committed >= end → SKIP (sink
        // delivered; ACK lagged → no duplicate, failure-mode-①); committed < end → re-send
        // the FULL response (no black-hole). codex BLOCKER 2: NO partial-suffix send (render
        // coordinate not derivable from the JSONL byte offset), delegation all-or-nothing so
        // `committed` is never strictly between start/end. Reconcile ONLY on the session-bound re-send path; plain watcher-direct unchanged.
        let watcher_resend_range_start = data_start_offset;
        let watcher_resend_range_end = terminal_event_consumed_offset(current_offset, &all_data);
        // #3593: self-heal a stale-high watermark BEFORE the resend-dedup `committed` read (no-inflight-gate parity; generation change → committed 0 → no false skip).
        reset_relay_watermark_on_generation_change(
            &shared,
            channel_id,
            &tmux_session_name,
            "watcher_terminal_resend_dedup",
        );
        let watcher_resend_committed = dr::committed_floor_for_resend_dedup(
            &shared,
            &watcher_provider,
            channel_id,
            &tmux_session_name,
        ); // #3089 B2b + #3593 (codex HIGH): in-memory committed ∪ flag-independent durable frontier
        let watcher_resend_reconciled = session_bound_terminal_delivery_attempted
            && watcher_direct_fallback_intended
            && !matches!(
                session_bound_ack_outcome,
                SessionBoundRelayAckOutcome::Delivered
            );
        let watcher_resend_action = if watcher_resend_reconciled {
            // #3593: the stale-high self-heal ran unconditionally above (codex P2).
            // #3151: gate the re-send on the in-flight sink-delivery marker BEFORE
            // the committed-offset reconciliation. The marker is a `Leased{Sink}`
            // state on the SAME per-channel `DeliveryLeaseCell` the watcher's own
            // direct-send path acquires (B2). Read a coherent snapshot, then:
            //   * Leased{Sink, fresh}  → WaitInFlight: a sink POST is in flight; do
            //     NOT re-send this pass (the slow-sink-in-flight duplicate #3151).
            //   * Leased{Sink, expired} → reclaim the dead sink's marker, then
            //     SendFull (committed<end) — the no-black-hole arm.
            //   * Committed{Sink} → reconcile vs committed offset: committed>=end → Skip
            //     (delivered), committed<end → SendFull (#3159: refused/NotDelivered re-sends).
            //   * Unleased / non-Sink holder → unchanged (defer to the existing
            //     committed-offset reconciliation).
            let gate_cell = shared.delivery_lease(channel_id);
            let snapshot = gate_cell.read();
            // #3159 BUG 1 (codex race-1): read `committed` AFTER the lease snapshot. The sink's
            // CLEAR protocol advances `committed` FIRST, THEN commits the marker (`Committed{Sink}`),
            // so observing `Committed{Sink}` happens-after the committed write → reading `committed`
            // next sees the advanced value (committed>=end for a real Delivered → Skip). Reading it
            // BEFORE the snapshot could pair a pre-advance `committed < end` with a now-Committed
            // marker → a spurious SendFull duplicate.
            let committed = dr::effective_committed_offset(
                &shared,
                &watcher_provider,
                channel_id,
                &tmux_session_name,
            );
            let now_ms = crate::services::discord::lease_now_ms();
            let (action, reclaim_expired_sink) = watcher_terminal_resend_action_gated(
                &snapshot,
                committed,
                watcher_resend_range_start,
                watcher_resend_range_end,
                now_ms,
            );
            if reclaim_expired_sink {
                // Force the dead sink's marker Unleased so the watcher-direct path
                // below can re-acquire and SendFull (no black-hole). Deadline-only /
                // identity-agnostic — a LIVE sink (fresh deadline) is never reached.
                gate_cell.reclaim_if_expired(now_ms);
            }
            Some(action)
        } else if watcher_direct_fallback_intended
            && dr::range_already_committed(watcher_resend_range_end, watcher_resend_committed)
        {
            // #3593: already-delivered range (`committed >= end`) on the non-reconciled
            // synthetic-resume path (the placeholder path the #3520 new-message-only floor
            // missed) → EXISTING non-destructive `SkipAlreadyCommitted` arm, which PRESERVES
            // the restored placeholder (flipping `has_direct_terminal_response`/the fallback
            // flag would delete the already-delivered body — #3520 codex BLOCKER).
            Some(WatcherTerminalResendAction::SkipAlreadyCommitted)
        } else {
            None
        };
        // #3151: WaitInFlight suppresses BOTH the re-send and the skip-log this
        // pass — the watcher's NEXT terminal pass re-evaluates (bounded by the
        // sink's lease deadline). It must NOT be treated as "send" by the fallback.
        let watcher_resend_wait_in_flight = matches!(
            watcher_resend_action,
            Some(WatcherTerminalResendAction::WaitInFlight)
        );
        if watcher_resend_wait_in_flight {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                provider = watcher_provider.as_str(),
                channel = channel_id.get(),
                tmux_session = %tmux_session_name,
                start = watcher_resend_range_start,
                end = watcher_resend_range_end,
                committed = watcher_resend_committed,
                ?session_bound_ack_outcome,
                "  [{ts}] 👁 #3151: deferred watcher terminal re-send — sink POST in flight (Leased{{Sink}}, fresh); will re-evaluate next pass (no duplicate)"
            );
        }
        if matches!(
            watcher_resend_action,
            Some(WatcherTerminalResendAction::SkipAlreadyCommitted)
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                provider = watcher_provider.as_str(),
                channel = channel_id.get(),
                tmux_session = %tmux_session_name,
                start = watcher_resend_range_start,
                end = watcher_resend_range_end,
                committed = watcher_resend_committed,
                ?session_bound_ack_outcome,
                "  [{ts}] 👁 #3041 P1-3 §3.2: skipped watcher terminal re-send — range already committed by the sink (offset authority); no duplicate"
            );
        }
        // The watcher actually direct-sends only when the reconciliation did NOT
        // skip the range AND is not WAITING on an in-flight sink POST.
        // `SkipAlreadyCommitted` suppresses the re-send (no dup); `WaitInFlight`
        // (#3151) suppresses it this pass (re-evaluated next pass); `SendFull`/the
        // non-reconciled path proceed to send.
        let watcher_direct_fallback_after_session_bound_ack = watcher_direct_fallback_intended
            && !matches!(
                watcher_resend_action,
                Some(
                    WatcherTerminalResendAction::SkipAlreadyCommitted
                        | WatcherTerminalResendAction::WaitInFlight
                )
            );
        // codex BLOCKER 2: on a non-skip reconciled re-send the action is always
        // `SendFull` (the watcher response-text coordinate cannot be derived from
        // the JSONL `committed` offset, and the sink delegation is all-or-nothing,
        // so no partial-suffix variant exists). The full body is re-sent: no
        // black-hole when committed<end, and never a mis-offset
        // `full_response[response_sent_offset..]` slice driven by an unrelated
        // streaming offset. The non-reconciled path keeps the existing full-body
        // fallback semantics.
        let session_bound_fallback_uses_full_body = session_bound_terminal_delivery_attempted
            && watcher_direct_fallback_after_session_bound_ack;
        let direct_terminal_response = watcher_terminal_response_for_direct_send(
            &full_response,
            response_sent_offset,
            session_bound_fallback_uses_full_body,
        );
        let has_direct_terminal_response = !direct_terminal_response.trim().is_empty();
        // #2838/#3042 (relay-stability P0-1): count the primary duplicate-emit vector — a
        // session-bound terminal ACK that timed out while the watcher direct-sends (sink may
        // have already posted; rising counts ⇒ P1 dual-authority lease overdue). Gate on the
        // raw `TimedOut` + original `should_direct_send` intent (records even when the ownerless-timeout suppression turned the fallback off).
        if relay_decision.should_direct_send
            && matches!(
                session_bound_ack_outcome,
                SessionBoundRelayAckOutcome::TimedOut
            )
        {
            crate::services::observability::metrics::record_relay_terminal_ack_timeout(
                channel_id.get(),
                watcher_provider.as_str(),
            );
        }
        // #3646 OBSERVATION-ONLY owner split: this is the INFLIGHT-snapshot owner
        // ONLY. The collapsed `="none"` could mean either a real None-ledger turn
        // OR "bridge cleared inflight but the ledger is still Watcher/finalized" —
        // the #3607 ambiguity. The finalizer-side `finalizer_ledger_owner` event
        // (ledger entry's relay_owner, same turn_id) supplies the second signal and
        // the two JOIN in PG. Computed once so we can emit it under BOTH the new
        // `inflight_relay_owner` name AND the legacy `relay_owner_kind` alias
        // (codex review #3678: keep the old field so existing dashboards/alerts/
        // runbooks that grep `relay_owner_kind=` don't break).
        let inflight_relay_owner_kind = inflight_before_relay
            .as_ref()
            .map(|state| state.effective_relay_owner_kind().as_str())
            .unwrap_or("none");
        tracing::info!(
            target: "agentdesk::relay_flight_recorder",
            provider = watcher_provider.as_str(),
            channel = channel_id.get(),
            tmux_session = %tmux_session_name,
            data_start_offset,
            current_offset,
            terminal_kind = terminal_kind.map(WatcherTerminalKind::as_str).unwrap_or("unknown"),
            full_response_len = current_response.len(),
            assistant_text_seen,
            any_tool_used = tool_state.any_tool_used,
            has_post_tool_text = tool_state.has_post_tool_text,
            inflight_present = inflight_before_relay.is_some(),
            // #3646: new disambiguated name. Field rename/add only — control flow
            // unchanged (these are tracing fields, not branches).
            inflight_relay_owner = inflight_relay_owner_kind,
            // #3646: legacy alias preserved for backward-compatible log greps.
            relay_owner_kind = inflight_relay_owner_kind,
            session_bound_enabled = session_bound_discord_delivery_enabled,
            fully_mirrored = session_bound_relay_turn_fully_mirrored,
            frame_ack = session_bound_relay_frame_ack_reached(all_data_session_bound_relay_ack.as_ref()),
            terminal_commit_ack = session_bound_relay_owns_terminal_delivery,
            route = if session_bound_relay_owns_terminal_delivery {
                "session_bound"
            } else if watcher_direct_fallback_after_session_bound_ack {
                "watcher_direct"
            } else if relay_decision.suppressed {
                "suppressed"
            } else {
                "none"
            },
            prompt_anchor_present,
            ssh_direct_pending,
            external_input_lease_present,
            recent_stop_reason = recent_stop_reason.as_deref().unwrap_or("none"),
            placeholder_msg_id = placeholder_msg_id.map(|id| id.get()).unwrap_or(0),
            status_panel_msg_id = status_panel_msg_id.map(|id| id.get()).unwrap_or(0),
            frame_ack_outcome = ?session_bound_ack_outcome,
            "relay flight recorder"
        );
        // #3041 P1-3 (codex P1-3 R7): turn-boundary ACK reset. THIS turn's terminal
        // ACK has now been waited on (`session_bound_ack_outcome` is captured) and
        // logged. If a forward on this pass SPLIT a result-bearing chunk with a
        // trailing tail, a LATER turn (B) follows in the leftover buffer. B is
        // processed on a SUBSEQUENT pass — possibly while `turn_identity_for_panel`
        // is STILL pinned to THIS turn's offset (B's inflight not yet established),
        // which would make `carry_session_bound_ack_for_turn` KEEP this turn's stale
        // ack and let this turn's `Delivered` falsely satisfy B's ACK → B
        // black-holed. RESET the stored ack to `None` HERE, AFTER this turn consumed
        // it, so B starts with NO inherited ack → MissingTarget → §3.2 reconcile
        // (committed-offset SendFull-or-Skip) → B is never black-holed (worst case a
        // duplicate, the #3151-deferred edge). This is the primary R7 guarantee and
        // is independent of whether the pinned identity refreshes.
        if split_trailing_turn_follows {
            all_data_session_bound_relay_ack = None;
        }
        let mut watcher_direct_terminal_idle_committed = false;
        let mut tui_direct_anchor_terminal_body_visible = false;
        let mut tui_direct_anchor_or_lease_present_for_lifecycle =
            prompt_anchor_present_before_relay || external_input_lease_before_relay;

        // #3041 P1-1: acquire the delivery lease BEFORE the watcher direct-sends. Lease
        // identity = the turn-pinned id (`pinned_finalizer_turn_id`, the #3141
        // id-pinning) + the byte range `[data_start_offset, terminal_event_consumed_offset)`
        // — the SAME consumed end the commit/advance uses, so acquire and commit carry one
        // identity. Acquire only on the watcher-direct path (delegation is the sink's lease
        // P1-2; suppression/no-response deliver nothing).
        //
        // B2 (single-holder, §5.2): if a DIFFERENT watcher instance holds this cell
        // (Leased) `try_acquire` fails → this watcher MUST NOT direct-send (skip arm
        // below). Acquire is the atomic fast-path (B4); commit/advance/release run INLINE
        // (preserving the pre-P1-1 advance timing, avoiding an actor-deferral duplicate).
        // The actor CommitDelivery/ReleaseDelivery messages remain dormant.
        let (watcher_lease_turn, watcher_lease_key, watcher_lease_holder) =
            pinned_watcher_delivery_lease_identity(
                channel_id,
                shared.restart.current_generation,
                watcher_instance_id,
                inflight_before_relay.as_ref(),
                &tmux_session_name,
                current_offset,
            );
        let watcher_lease_start = data_start_offset;
        let watcher_lease_end = terminal_event_consumed_offset(current_offset, &all_data);
        // #3610 PR-1d: the legacy long-chunk fallback arm's terminal anchor (last
        // sent chunk msg id), captured at the send arm but RECORDED only at the
        // post-advance M4 site below (Some here ⇒ this turn took the long-chunk arm
        // AND fully committed). Declared at lease scope so it survives the
        // `let relay_ok = if … { … }` block that holds the send arm.
        let mut watcher_long_chunk_anchor_msg_id: Option<MessageId> = None;
        let watcher_lease_cell = shared.delivery_lease(channel_id);
        // Only the watcher-direct fallback path direct-sends; acquire exactly when it
        // runs with a real body so the lease identity matches the delivered bytes (a
        // zero/inverted range never delivers, so do not lease it).
        let watcher_will_direct_send =
            watcher_direct_fallback_after_session_bound_ack && has_direct_terminal_response;
        // #3089 A4: cut the watcher short-replace branch onto the unified controller
        // behind a flag (default OFF). When ON the CONTROLLER owns the single lease, so
        // the watcher's own acquire/heartbeat/b2-skip/commit/advance/release are skipped
        // (no double-acquire). OFF is byte-identical (flag short-circuits before
        // formatting). Empty bodies / TUI-gated turns stay legacy; anchored long
        // chunks route via the controller's anchor-delete plan when the flag is ON.
        // No-placeholder new-message fresh-send remains legacy: anchor-less
        // fresh-send is not yet a controller verb (#3998 flip re-evaluation).
        let cutover_short_replace = terminal_send::watcher_short_replace_cutover_decision(
            terminal_send::watcher_terminal_controller_enabled(),
            shared.ui.status_panel_v2_enabled,
            relay_decision.should_tag_monitor_origin,
            &watcher_provider,
            &direct_terminal_response,
            watcher_will_direct_send,
            watcher_lease_end > watcher_lease_start,
            placeholder_msg_id.is_some(),
            session_bound_fallback_uses_full_body,
            watcher_terminal_kind_requires_tui_completion_gate(terminal_kind),
        );
        // Pure no-double-acquire gate: `None` when cut over (the controller owns the
        // lease), so the watcher's own acquire below is skipped.
        let watcher_terminal_lease_range = terminal_send::watcher_terminal_lease_range(
            (watcher_will_direct_send && watcher_lease_end > watcher_lease_start)
                .then_some((watcher_lease_start, watcher_lease_end)),
            cutover_short_replace,
        );
        let watcher_lease_acquired = watcher_terminal_lease_range.is_some()
            // #3041 P1-1 (B3, Issue 1): SELF-HEALING acquire — reclaim an ELAPSED
            // `Leased` lease (dead holder that died before commit/release) against the
            // SAME monotonic `lease_now_ms()` clock, so a LIVE holder mid-send (deadline
            // pushed forward by the heartbeat) is NOT reclaimed and still correctly
            // B2-skips (§5.2). PRIMARY black-hole guarantee, bounded to the deadline,
            // no finalizer `SharedData` dependency; reconcile-tick reclaim is secondary.
            && try_acquire_watcher_delivery_lease(
                &watcher_lease_cell,
                watcher_lease_holder,
                &watcher_lease_key,
                watcher_lease_start,
                watcher_lease_end,
            );
        // B2 skip flag: intended to direct-send but a different holder owns the range →
        // skip arm (no duplicate send). #3089 A4: EXCLUDE the cut-over turn
        // (`!cutover_short_replace`) — its lost-acquire B2-skip is handled INSIDE the
        // controller (`AcquireFailureMode::Transient`) so the chain reaches arm 5. (P1-3
        // residual: the 10s ACK-timeout blind re-send stays; a same-holder re-send
        // re-acquires/re-commits the SAME range but the offset advance is a monotonic CAS
        // — cannot double-advance, bounded, idempotent.)
        let watcher_lease_b2_skip = watcher_will_direct_send
            && watcher_lease_end > watcher_lease_start
            && !watcher_lease_acquired
            && !cutover_short_replace;

        // #3041 P1-1 (codex R2 Issue-2, BLOCKER B5 — DEFERRED, NOT a regression): the
        // lease range is the FULL `[data_start_offset, consumed_end)`. A crash AFTER
        // chunk 1 but BEFORE commit lets a replacement reclaim the EXPIRED lease and
        // re-send the WHOLE range → partial DUPLICATE. Exact-once on a partial
        // multi-chunk crash needs per-message-id partial-commit state, EXPLICITLY
        // deferred to B5. NOT a regression: the heartbeat below means a LIVE holder is
        // never reclaimed mid-send, so this matches pre-P1-1 crash behaviour (no lease
        // then either); P1-1 only adds a bounded (≤ deadline) re-delivery delay.
        //
        // #3041 P1-1 (§3, codex R2 Issue-1): keep the lease alive WHILE the send is in
        // flight. The deadline is short (15s) for fast dead-holder recovery; a long
        // legitimate send (60+ rate-limited chunks past any FIXED deadline) is covered by
        // this background heartbeat `renew()`ing every 5s. `stop()`ped BEFORE the inline
        // commit (and aborts on drop), so it never races the commit. Spawned ONLY when we
        // acquired; the B2-skip / no-send / #3089-A4-cutover arms have no lease to renew.
        let watcher_lease_heartbeat = watcher_delivery_lease_heartbeat(
            watcher_lease_acquired,
            watcher_lease_cell.clone(),
            watcher_lease_holder,
            &watcher_lease_key,
        );

        let relay_ok = if session_bound_relay_owns_terminal_delivery {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Delegating terminal response to session-bound StreamRelay sink ({} chars, offset {}, task_notification_kind={})",
                current_response.len(),
                data_start_offset,
                task_notification_kind
                    .map(TaskNotificationKind::as_str)
                    .unwrap_or("none")
            );
            if has_current_response {
                tui_direct_anchor_terminal_body_visible = true;
                last_relayed_offset = Some(turn_data_start_offset);
                last_observed_generation_mtime_ns =
                    Some(read_generation_file_mtime_ns(&tmux_session_name));
                crate::services::observability::watcher_latency::record_first_relay(
                    channel_id.get(),
                );
                // #3558 (codex review follow-up): the old unlocked
                // `load_inflight_state` → mutate → `save_inflight_state(&inflight)`
                // re-wrote the WHOLE stale row (including a possibly-backward
                // `last_offset`/`response_sent_offset`), reintroducing the exact
                // backward-write TOCTOU the #3558 fix closed in the streaming /
                // terminal-commit paths. Route the relay-success watermark through
                // the single-flock RMW helper, which patches ONLY
                // `last_watcher_relayed_*` and preserves the disk watermark.
                // #3041 P1-3 (Part a, B1 — FRAME-CARRIED): the authoritative
                // consumed-terminal END is NOT written here; it rides the
                // RESULT-bearing `StreamFrame` and the sink advances
                // `confirmed_end_offset` identity-gated on its confirmed POST.
                if let Some(identity) = inflight_identity_before_relay.as_ref() {
                    let _ =
                        crate::services::discord::inflight::persist_watcher_relay_watermark_locked(
                            &watcher_provider,
                            channel_id.get(),
                            identity,
                            &tmux_session_name,
                            crate::services::discord::inflight::WatcherRelayWatermarkPatch {
                                last_watcher_relayed_offset: Some(turn_data_start_offset),
                                last_watcher_relayed_generation_mtime_ns:
                                    last_observed_generation_mtime_ns,
                            },
                        );
                }
            }
            clear_provider_overload_retry_state(channel_id);
            true
        } else if matches!(
            watcher_resend_action,
            Some(WatcherTerminalResendAction::SkipAlreadyCommitted)
        ) {
            // #3041 P1-3 (Part b, §3.2): the offset authority already covers this
            // terminal range (`committed >= end`) — the session-bound sink already
            // delivered it (the terminal-commit ACK merely lagged the 10s wait, and
            // Part (a) advanced the authority on the sink's confirmed POST). This is
            // the failure-mode-① case: re-sending would DUPLICATE. Treat it as a
            // completed delegated delivery (mirror the delegation-success arm): do NOT
            // delete the placeholder and do NOT re-send. `relay_ok = true` so the
            // lifecycle finalizes exactly as a delivered turn (the response IS on the
            // channel, posted by the sink); the offset is already at `end`, so the
            // inline advance below is an idempotent no-op.
            if has_current_response {
                tui_direct_anchor_terminal_body_visible = true;
                last_relayed_offset = Some(turn_data_start_offset);
                last_observed_generation_mtime_ns =
                    Some(read_generation_file_mtime_ns(&tmux_session_name));
            }
            clear_provider_overload_retry_state(channel_id);
            true
        } else if matches!(
            watcher_resend_action,
            Some(WatcherTerminalResendAction::WaitInFlight)
        ) {
            // #3151: a sink POST is genuinely IN FLIGHT for this range
            // (`Leased{Sink, fresh}`). Do NOT re-send / finalize / delete the
            // placeholder (the sink is about to post into it). Return `false` so
            // `terminal_output_committed` stays false: the turn is left OPEN and
            // re-entered NEXT pass. BOUNDED by the sink's lease deadline — within one
            // `DELIVERY_LEASE_DEADLINE_MS` the sink commits+releases (→ committed>=end →
            // SkipAlreadyCommitted) or dies (→ deadline lapses → reclaim + SendFull).
            // The sole arm closing the slow-sink-in-flight duplicate (#3151).
            false
        } else if watcher_lease_b2_skip {
            // #3041 P1-1 B2 (single-holder, §5.2): a DIFFERENT watcher instance already
            // holds the delivery lease for this channel/turn/range (mid-send or not yet
            // committed/released/reclaimed). A replacement MUST NOT re-acquire and
            // re-emit — the duplicate-send vector the lease closes. Skip the direct send;
            // `terminal_output_committed` stays false so no offset advance / lifecycle
            // side-effects run; the live holder commit-advances the offset itself.
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %watcher_provider.as_str(),
                channel = channel_id.get(),
                tmux_session = %tmux_session_name,
                data_start_offset = watcher_lease_start,
                lease_end = watcher_lease_end,
                "  [{ts}] 👁 #3041 B2: delivery lease held by another holder — skipped duplicate terminal send for {tmux_session_name} (range {watcher_lease_start}..{watcher_lease_end})"
            );
            false
        } else if watcher_direct_fallback_after_session_bound_ack {
            let formatted = if shared.ui.status_panel_v2_enabled {
                crate::services::discord::formatting::format_for_discord_with_status_panel(
                    direct_terminal_response,
                    &watcher_provider,
                )
            } else {
                crate::services::discord::formatting::format_for_discord_with_provider(
                    direct_terminal_response,
                    &watcher_provider,
                )
            };
            let relay_text = if relay_decision.should_tag_monitor_origin {
                crate::services::discord::prepend_monitor_auto_turn_origin(&formatted)
            } else {
                formatted
            };
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Relaying terminal response to Discord ({} chars, offset {}, task_notification_kind={})",
                relay_text.len(),
                data_start_offset,
                task_notification_kind
                    .map(TaskNotificationKind::as_str)
                    .unwrap_or("none")
            );
            let mut retry_terminal_delivery_from_offset = false;
            let mut relay_ok = true;
            let mut direct_send_delivered = false;
            let mut external_input_lease_consumed_by_relay = false;
            match placeholder_msg_id {
                Some(msg_id) => {
                    if has_direct_terminal_response {
                        if watcher_should_send_ordered_new_chunks_for_terminal_fallback(
                            session_bound_fallback_uses_full_body,
                            &relay_text,
                        ) {
                            if cutover_short_replace {
                                terminal_long_chunks::apply_watcher_long_chunks_controller(
                                    &http,
                                    &shared,
                                    &watcher_provider,
                                    channel_id,
                                    &tmux_session_name,
                                    msg_id,
                                    &relay_text,
                                    &watcher_lease_cell,
                                    watcher_lease_turn,
                                    Some(watcher_lease_key.clone()),
                                    watcher_instance_id,
                                    (watcher_lease_start, watcher_lease_end),
                                    session_bound_fallback_uses_full_body,
                                    &mut watcher_streaming_rollover_frozen_msg_ids,
                                    inflight_before_relay.as_ref(),
                                    terminal_long_chunks::WatcherLongChunksLocals {
                                        relay_ok: &mut relay_ok,
                                        direct_send_delivered: &mut direct_send_delivered,
                                        tui_direct_anchor_terminal_body_visible:
                                            &mut tui_direct_anchor_terminal_body_visible,
                                        external_input_lease_consumed_by_relay:
                                            &mut external_input_lease_consumed_by_relay,
                                        placeholder_msg_id: &mut placeholder_msg_id,
                                        placeholder_from_restored_inflight:
                                            &mut placeholder_from_restored_inflight,
                                        last_edit_text: &mut last_edit_text,
                                    },
                                )
                                .await;
                            } else {
                                terminal_long_chunks::apply_watcher_long_chunks_legacy(
                                    &http,
                                    &shared,
                                    &watcher_provider,
                                    channel_id,
                                    &tmux_session_name,
                                    msg_id,
                                    &relay_text,
                                    session_bound_fallback_uses_full_body,
                                    &mut watcher_streaming_rollover_frozen_msg_ids,
                                    inflight_before_relay.as_ref(),
                                    &mut watcher_long_chunk_anchor_msg_id,
                                    terminal_long_chunks::WatcherLongChunksLocals {
                                        relay_ok: &mut relay_ok,
                                        direct_send_delivered: &mut direct_send_delivered,
                                        tui_direct_anchor_terminal_body_visible:
                                            &mut tui_direct_anchor_terminal_body_visible,
                                        external_input_lease_consumed_by_relay:
                                            &mut external_input_lease_consumed_by_relay,
                                        placeholder_msg_id: &mut placeholder_msg_id,
                                        placeholder_from_restored_inflight:
                                            &mut placeholder_from_restored_inflight,
                                        last_edit_text: &mut last_edit_text,
                                    },
                                )
                                .await;
                            }
                        } else if cutover_short_replace {
                            // #3089 A4: route short-replace through the unified controller
                            // (flag ON) — see `apply_watcher_short_replace_controller`. The
                            // CONTROLLER owns the SINGLE `LeaseHolder::Watcher` lease (the
                            // watcher's own acquire/heartbeat/commit/advance/release were
                            // skipped at the acquire site). #2757 PreserveAlways is honoured;
                            // the rare `SentFallbackAfterEditFailure` sub-case mirrors the
                            // legacy fallback arm (NO footer target, `Failed(edit_error)`
                            // cleanup, original preserved) via the controller-surfaced
                            // `ReplaceDeliveryKind` (#3089 A4 r2, codex r1 [High]).
                            terminal_send::apply_watcher_short_replace_controller(
                                &http,
                                &shared,
                                &watcher_provider,
                                channel_id,
                                &tmux_session_name,
                                msg_id,
                                &relay_text,
                                &watcher_lease_cell,
                                watcher_lease_turn,
                                Some(watcher_lease_key.clone()),
                                watcher_instance_id,
                                (watcher_lease_start, watcher_lease_end),
                                single_message_panel_footer_mode,
                                inflight_before_relay.as_ref(),
                                terminal_send::WatcherShortReplaceLocals {
                                    relay_ok: &mut relay_ok,
                                    direct_send_delivered: &mut direct_send_delivered,
                                    tui_direct_anchor_terminal_body_visible:
                                        &mut tui_direct_anchor_terminal_body_visible,
                                    external_input_lease_consumed_by_relay:
                                        &mut external_input_lease_consumed_by_relay,
                                    placeholder_msg_id: &mut placeholder_msg_id,
                                    placeholder_from_restored_inflight:
                                        &mut placeholder_from_restored_inflight,
                                    last_edit_text: &mut last_edit_text,
                                    completion_footer_terminal_target:
                                        &mut completion_footer_terminal_target,
                                    retry_terminal_delivery_from_offset:
                                        &mut retry_terminal_delivery_from_offset,
                                },
                            )
                            .await;
                        } else {
                            // #3805 P1: capture the tail continuation chunk (id +
                            // its own text) so the completion footer re-anchors onto
                            // it instead of stranding on the edited chunk 0.
                            let mut last_chunk_anchor = None;
                            match replace_long_message_raw_with_outcome(
                                &http,
                                channel_id,
                                msg_id,
                                &relay_text,
                                &shared,
                                &mut last_chunk_anchor,
                            )
                            .await
                            {
                                Ok(ReplaceLongMessageOutcome::EditedOriginal) => {
                                    direct_send_delivered = true;
                                    tui_direct_anchor_terminal_body_visible = true;
                                    external_input_lease_consumed_by_relay =
                                        watcher_inflight_represents_external_input(
                                            inflight_before_relay.as_ref(),
                                        );
                                    // #3805 P1: re-anchor the completion footer to the
                                    // LAST continuation chunk with the tail chunk's OWN
                                    // text (single-chunk ⇒ chunk 0 + full body).
                                    let (footer_target_msg_id, footer_target_text) =
                                        crate::services::discord::formatting::watcher_completion_footer_anchor(
                                            last_chunk_anchor.as_ref(),
                                            msg_id,
                                            &relay_text,
                                        );
                                    remember_watcher_completion_footer_terminal_target(
                                        single_message_panel_footer_mode,
                                        &mut completion_footer_terminal_target,
                                        footer_target_msg_id,
                                        footer_target_text,
                                    );
                                    placeholder_msg_id = None;
                                    placeholder_from_restored_inflight = false;
                                    last_edit_text.clear();
                                    // #3351 r21 mirror: edited into the final response —
                                    // a stale record must not let a drain delete it.
                                    drop_placeholder_orphan_record(
                                        &watcher_provider,
                                        &shared,
                                        channel_id,
                                        msg_id,
                                    );
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!(
                                        "  [{ts}] 👁 ✓ relayed terminal response (edit) channel {} msg {} ({} chars)",
                                        channel_id.get(),
                                        msg_id.get(),
                                        relay_text.len()
                                    );
                                    record_placeholder_cleanup(
                                        &shared,
                                        &watcher_provider,
                                        channel_id,
                                        msg_id,
                                        &tmux_session_name,
                                        PlaceholderCleanupOperation::EditTerminal,
                                        PlaceholderCleanupOutcome::Succeeded,
                                        "watcher_terminal_relay",
                                    );
                                }
                                Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                                    edit_error,
                                }) => {
                                    direct_send_delivered = true;
                                    tui_direct_anchor_terminal_body_visible = true;
                                    external_input_lease_consumed_by_relay =
                                        watcher_inflight_represents_external_input(
                                            inflight_before_relay.as_ref(),
                                        );
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!(
                                        "  [{ts}] 👁 ✓ relayed terminal response (fallback send after edit failure) channel {} msg {} ({} chars, edit_error={edit_error})",
                                        channel_id.get(),
                                        msg_id.get(),
                                        relay_text.len()
                                    );
                                    record_placeholder_cleanup(
                                        &shared,
                                        &watcher_provider,
                                        channel_id,
                                        msg_id,
                                        &tmux_session_name,
                                        PlaceholderCleanupOperation::EditTerminal,
                                        PlaceholderCleanupOutcome::failed(edit_error),
                                        "watcher_terminal_relay",
                                    );
                                    if watcher_fallback_edit_failure_can_delete_original_placeholder(
                                        response_sent_offset,
                                        &last_edit_text,
                                    ) {
                                        let cleanup = delete_terminal_placeholder(
                                            &http,
                                            channel_id,
                                            &shared,
                                            &watcher_provider,
                                            &tmux_session_name,
                                            msg_id,
                                            "watcher_terminal_relay_fallback_cleanup",
                                        )
                                        .await;
                                        match fallback_placeholder_cleanup_decision(&cleanup) {
                                            FallbackPlaceholderCleanupDecision::RelayCommitted => {
                                                placeholder_msg_id = None;
                                                placeholder_from_restored_inflight = false;
                                                last_edit_text.clear();
                                                // #3351 r21 mirror: delete committed.
                                                drop_placeholder_orphan_record(
                                                    &watcher_provider,
                                                    &shared,
                                                    channel_id,
                                                    msg_id,
                                                );
                                            }
                                            FallbackPlaceholderCleanupDecision::PreserveInflightForCleanupRetry => {
                                                relay_ok = false;
                                                tui_direct_anchor_terminal_body_visible = false;
                                                let ts = chrono::Local::now().format("%H:%M:%S");
                                                tracing::warn!(
                                                    "  [{ts}] ⚠ watcher: terminal response was delivered via fallback send, but stale placeholder cleanup did not commit for channel {} msg {}",
                                                    channel_id.get(),
                                                    msg_id.get()
                                                );
                                            }
                                        }
                                    } else {
                                        placeholder_msg_id = None;
                                        placeholder_from_restored_inflight = false;
                                        last_edit_text.clear();
                                        // #3351 (codex r2 #2): message intentionally preserved
                                        // (#2757) — a stale record must not let a drain delete it.
                                        drop_placeholder_orphan_record(
                                            &watcher_provider,
                                            &shared,
                                            channel_id,
                                            msg_id,
                                        );
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::warn!(
                                            "  [{ts}] ⚠ watcher: terminal response delivered via fallback send; preserving original msg {} in channel {} because it may contain streamed response content (#2757)",
                                            msg_id.get(),
                                            channel_id.get()
                                        );
                                    }
                                }
                                Ok(ReplaceLongMessageOutcome::PartialContinuationFailure {
                                    sent_chunks,
                                    total_chunks,
                                    failed_chunk_index,
                                    sent_continuation_message_ids,
                                    cleanup_errors,
                                    error,
                                }) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::warn!(
                                        "  [{ts}] ⚠ watcher: terminal response partially delivered in channel {} msg {} (sent_chunks={}, total_chunks={}, failed_chunk_index={}, cleaned_continuations={}, cleanup_errors={}, error={}); preserving inflight for retry",
                                        channel_id.get(),
                                        msg_id.get(),
                                        sent_chunks,
                                        total_chunks,
                                        failed_chunk_index,
                                        sent_continuation_message_ids.len(),
                                        cleanup_errors.len(),
                                        error
                                    );
                                    record_placeholder_cleanup(
                                        &shared,
                                        &watcher_provider,
                                        channel_id,
                                        msg_id,
                                        &tmux_session_name,
                                        PlaceholderCleanupOperation::EditTerminal,
                                        PlaceholderCleanupOutcome::failed(format!(
                                            "{error}; cleaned_continuations={}; cleanup_errors={}",
                                            sent_continuation_message_ids.len(),
                                            cleanup_errors.len()
                                        )),
                                        "watcher_terminal_relay_partial_continuation_failure",
                                    );
                                    let plan = watcher_partial_continuation_retry_plan();
                                    relay_ok = plan.relay_ok;
                                    retry_terminal_delivery_from_offset = plan.retry_offset;
                                }
                                Err(e) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!("  [{ts}] 👁 Failed to relay: {e}");
                                    relay_ok = false;
                                }
                            }
                        }
                    } else {
                        let outcome = delete_terminal_placeholder(
                            &http,
                            channel_id,
                            &shared,
                            &watcher_provider,
                            &tmux_session_name,
                            msg_id,
                            "watcher_empty_terminal_cleanup",
                        )
                        .await;
                        if !outcome.is_committed() {
                            relay_ok = false;
                        } else {
                            placeholder_msg_id = None;
                            placeholder_from_restored_inflight = false;
                            last_edit_text.clear();
                        }
                    }
                }
                None => {
                    if has_direct_terminal_response {
                        let prompt_anchor =
                            crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
                                watcher_provider.as_str(),
                                &tmux_session_name,
                                channel_id.get(),
                            );
                        let prompt_anchor_reference = prompt_anchor.map(|anchor| {
                            (
                                ChannelId::new(anchor.channel_id),
                                MessageId::new(anchor.message_id),
                            )
                        });
                        match crate::services::discord::formatting::send_long_message_raw_with_reference(
                            &http,
                            channel_id,
                            &relay_text,
                            &shared,
                            prompt_anchor_reference,
                        )
                        .await
                        {
                            Ok(_) => {
                                tui_direct_anchor_or_lease_present_for_lifecycle |=
                                    prompt_anchor.is_some();
                                external_input_lease_consumed_by_relay =
                                    external_input_lease_before_relay || prompt_anchor.is_some();
                                direct_send_delivered = true;
                                tui_direct_anchor_terminal_body_visible = true;
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!(
                                    "  [{ts}] 👁 ✓ relayed terminal response (new message) channel {} ({} chars, prompt_anchor_message_id={:?})",
                                    channel_id.get(),
                                    relay_text.len(),
                                    prompt_anchor_reference.map(|(_, message_id)| message_id.get())
                                );
                            }
                            Err(e) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!("  [{ts}] 👁 Failed to relay: {e}");
                                relay_ok = false;
                            }
                        }
                    }
                }
            }
            if relay_ok {
                if direct_send_delivered || !has_direct_terminal_response {
                    if direct_send_delivered {
                        // #3041 P1-4 codex: clear BY the generation snapshotted before
                        // this awaited delivery, NOT by key. The old unconditional by-key
                        // clear had a stale-snapshot clobber: turn-1 snapshots the lease
                        // present, starts its send; turn-2 records a NEWER same-key lease;
                        // turn-1's send succeeds and the by-key clear removed turn-2's
                        // lease (re-introducing the exact no-clobber race the generation
                        // nonce was added to kill). Generation-scoped clear only removes
                        // the lease this relay actually consumed; sentinel/None (no lease
                        // was present) clears nothing — guarded by the consumed gate too.
                        if let Some(generation) = external_input_lease_generation_before_relay
                            && external_input_lease_consumed_by_relay
                        {
                            crate::services::tui_prompt_dedupe::clear_external_input_relay_lease_if_generation_matches(
                                watcher_provider.as_str(),
                                &tmux_session_name,
                                channel_id.get(),
                                generation,
                            );
                        }
                        if watcher_direct_terminal_should_commit_session_idle(
                            direct_send_delivered,
                            inflight_before_relay.is_some(),
                            external_input_lease_consumed_by_relay,
                            prompt_anchor_present_before_relay,
                            external_input_lease_before_relay,
                            ssh_direct_pending,
                        ) {
                            watcher_direct_terminal_idle_committed =
                                commit_watcher_direct_terminal_session_idle(
                                    &shared,
                                    &watcher_provider,
                                    channel_id,
                                    &tmux_session_name,
                                    terminal_kind,
                                    data_start_offset,
                                    current_offset,
                                )
                                .await;
                        }
                    }
                    last_relayed_offset = Some(turn_data_start_offset);
                    // #1270 codex P2: snapshot the current `.generation` mtime on
                    // every successful relay so the local regression check has a
                    // real baseline. Without this, normal relay paths (which never
                    // enter the reset helper) leave the baseline at None, and a
                    // later regression misclassifies same-wrapper rotation as
                    // fresh-respawn — clearing the offset and re-relaying bytes.
                    last_observed_generation_mtime_ns =
                        Some(read_generation_file_mtime_ns(&tmux_session_name));
                    // #1134: first successful relay for this attach. The
                    // watcher_latency module is idempotent — only the first
                    // call after `record_attach` actually observes a sample,
                    // so the unconditional call here is safe and cheap.
                    crate::services::observability::watcher_latency::record_first_relay(
                        channel_id.get(),
                    );
                    // #3558 (codex review follow-up): same backward-write TOCTOU
                    // as the session-bound-delegation arm — the old unlocked
                    // `load_inflight_state` → mutate → `save_inflight_state` re-wrote
                    // the whole stale row (including `last_offset`/
                    // `response_sent_offset`). Route the relay-success watermark
                    // through the single-flock RMW helper, which patches ONLY
                    // `last_watcher_relayed_*` and preserves the disk watermark.
                    // #1270: persist the matching `.generation` mtime alongside the
                    // offset so a replacement watcher (e.g. after dcserver restart)
                    // can disambiguate same-wrapper rotation (mtime unchanged → pin
                    // to EOF) from cancel→respawn (mtime changed → reset to 0) when
                    // restoring this offset.
                    if let Some(identity) = inflight_identity_before_relay.as_ref() {
                        let _ = crate::services::discord::inflight::persist_watcher_relay_watermark_locked(
                            &watcher_provider,
                            channel_id.get(),
                            identity,
                            &tmux_session_name,
                            crate::services::discord::inflight::WatcherRelayWatermarkPatch {
                                last_watcher_relayed_offset: Some(turn_data_start_offset),
                                last_watcher_relayed_generation_mtime_ns:
                                    last_observed_generation_mtime_ns,
                            },
                        );
                    }
                }
                clear_provider_overload_retry_state(channel_id);
            }
            if retry_terminal_delivery_from_offset {
                // #3041 P1-1: a SAME-holder abandon-without-commit — the partial send
                // failed; reset the offset to retry the SAME range next loop. Leaving the
                // lease `Leased` would make the retry's `try_acquire` lose to our own held
                // lease (B2-skip suppresses the retry until the deadline reclaim), so
                // abandon-release here (Leased→Unleased). The sole non-committing abandon,
                // released on the cell directly (same-holder, no actor serialization);
                // identity-matched no-op when not acquired (#3089 A4 cutover: the
                // controller already released its own lease on the Unknown path).
                if watcher_lease_acquired {
                    watcher_lease_cell.release(
                        watcher_lease_holder,
                        watcher_lease_key.clone(),
                        watcher_lease_start,
                        watcher_lease_end,
                    );
                }
                current_offset = turn_data_start_offset;
                all_data.clear();
                all_data_start_offset = current_offset;
                all_data_fully_mirrored_to_session_relay = true;
                all_data_session_bound_relay_ack = None;
                all_data_first_forwarded_relay_sequence = None;
                // #2840: release before the backoff sleep (timing preserved);
                // the guard's Drop is the safety net for non-explicit exits.
                slot_guard.release();
                sleep_or_jsonl_event(
                    tokio::time::Duration::from_millis(500),
                    &jsonl_notify,
                    &dead_marker_notify,
                )
                .await;
                continue 'watcher_loop;
            }
            relay_ok
        } else if relay_decision.suppressed {
            let monitor_event_count = tool_state.transcript_events.len();
            // #1009: Snapshot the channel's MonitoringStore entry keys ONCE so
            // both the lifecycle notify-outbox row and the suppressed-placeholder
            // edit body share an identical summary (DRY enforcement).
            let monitor_entry_keys: Vec<String> = if matches!(
                task_notification_kind,
                Some(TaskNotificationKind::MonitorAutoTurn)
            ) {
                let store_arc = crate::services::monitoring_store::global_monitoring_store();
                let store = store_arc.lock().await;
                store
                    .list(channel_id.get())
                    .into_iter()
                    .map(|entry| entry.key)
                    .collect()
            } else {
                Vec::new()
            };
            if matches!(
                task_notification_kind,
                Some(TaskNotificationKind::MonitorAutoTurn)
            ) {
                let _ = enqueue_monitor_auto_turn_suppressed_notification(
                    shared.pg_pool.as_ref(),
                    sqlite_runtime_db(shared.as_ref()),
                    channel_id,
                    &tmux_session_name,
                    data_start_offset,
                    monitor_event_count,
                    &monitor_entry_keys,
                );
            }
            let task_notification_detail = format!(
                "{} kind={} offset={}",
                tmux_session_name,
                task_notification_kind
                    .map(TaskNotificationKind::as_str)
                    .unwrap_or("none"),
                data_start_offset,
            );
            let ctx = PlaceholderSuppressContext {
                origin: PlaceholderSuppressOrigin::TaskNotificationTerminal,
                provider: &watcher_provider,
                placeholder_msg_id,
                response_sent_offset,
                last_edit_text: &last_edit_text,
                inflight_state: None,
                has_active_turn: false,
                tmux_session_name: &tmux_session_name,
                task_notification_kind,
                reattach_offset_match: false,
            };
            let mut decision = decide_placeholder_suppression(&ctx);
            // #1009: Monitor auto-turn gets a richer suppressed-placeholder body
            // (event count + current MonitoringStore entry keys) in place of the
            // generic internal-suppression label.
            if matches!(
                task_notification_kind,
                Some(TaskNotificationKind::MonitorAutoTurn)
            ) {
                if let PlaceholderSuppressDecision::Edit(_) = &decision {
                    let body = format_monitor_suppressed_body(
                        &last_edit_text,
                        &watcher_provider,
                        monitor_event_count,
                        &monitor_entry_keys,
                    );
                    decision = PlaceholderSuppressDecision::Edit(body);
                }
            }
            apply_placeholder_suppression(
                &http,
                channel_id,
                &shared,
                &watcher_provider,
                &tmux_session_name,
                placeholder_msg_id,
                ctx.origin,
                decision,
                Some(&task_notification_detail),
            )
            .await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Suppressed task-notification relay for {} (kind={}, offset {})",
                tmux_session_name,
                task_notification_kind
                    .map(TaskNotificationKind::as_str)
                    .unwrap_or("none"),
                data_start_offset
            );
            clear_provider_overload_retry_state(channel_id);
            false
        } else {
            if let Some(msg_id) = placeholder_msg_id {
                // No response text but placeholder exists — clean up
                let _ = delete_terminal_placeholder(
                    &http,
                    channel_id,
                    &shared,
                    &watcher_provider,
                    &tmux_session_name,
                    msg_id,
                    "watcher_no_response_cleanup",
                )
                .await;
            }
            false
        };
        let relay_suppressed = relay_decision.suppressed;
        let terminal_output_committed = relay_ok || relay_suppressed;
        if terminal_output_committed {
            terminal_delivery_observed = true;
        }
        // #3003: the no-response/stopped external-input panel reclaim runs once at
        // the terminal chokepoint near the top of this block (where
        // recent_stop_for_output / inflight_missing_before_relay are computed), so
        // no separate cleanup is needed here.
        let runtime_binding_candidate_offset = terminal_output_committed
            .then(|| terminal_event_consumed_offset(current_offset, &all_data));
        let terminal_delivery_committed = relay_ok
            && has_assistant_response
            && mark_watcher_terminal_delivery_committed(
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                inflight_identity_before_relay.as_ref(),
                &full_response,
                turn_data_start_offset,
                last_observed_generation_mtime_ns,
                runtime_binding_candidate_offset.unwrap_or(current_offset),
            );

        // #3646 OBSERVATION-ONLY (event 1/3 — terminal_body_commit): emit once the
        // body commit decision is in hand. The `if terminal_output_committed` guard
        // only GATES the emit, never the cleanup; `inflight_relay_owner` (snapshot)
        // and the finalizer-side `ledger_relay_owner` JOIN on turn_id to resolve the
        // #3607 None-ledger vs Watcher-finalize confusion. Orchestration lives in
        // relay_owner_observability (non-hot file); this is a thin pass-through.
        if terminal_output_committed {
            crate::services::discord::relay_owner_observability::emit_terminal_body_commit(
                watcher_provider.as_str(),
                channel_id.get(),
                inflight_before_relay
                    .as_ref()
                    .and_then(|s| s.dispatch_id.as_deref()),
                inflight_before_relay
                    .as_ref()
                    .and_then(|s| s.session_key.as_deref()),
                pinned_finalizer_turn_id(
                    inflight_before_relay.as_ref(),
                    &tmux_session_name,
                    current_offset,
                ),
                pinned_finalize_user_msg_id(
                    inflight_before_relay.as_ref(),
                    &tmux_session_name,
                    current_offset,
                ),
                status_panel_msg_id.map(|id| id.get()).unwrap_or(0),
                turn_data_start_offset,
                terminal_event_consumed_offset(current_offset, &all_data),
                inflight_before_relay
                    .as_ref()
                    .map(|state| state.effective_relay_owner_kind().as_str())
                    .unwrap_or("none"),
                terminal_delivery_committed,
            );
        }

        // #2161 TUI completion gate: ClaudeTui can land a `result` JSONL event before the
        // pane is quiescent; without it the user sees `응답 완료` while the pane still
        // streams. On gate timeout (Codex H2) do NOT emit `TurnCompleted` — the sweeper /
        // next-turn intake closes the lingering Active panel. Codex r2 H1: the gate outcome
        // is also threaded into the dispatch finalization so a busy pane does not drain
        // queued turns.
        let watcher_tui_gate_outcome = if terminal_output_committed
            && watcher_terminal_kind_requires_tui_completion_gate(terminal_kind)
        {
            run_tui_completion_gate(
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                task_notification_kind,
            )
            .await
        } else {
            TuiCompletionGateOutcome::NotGated
        };
        if let Some(candidate_offset) = runtime_binding_candidate_offset {
            if watcher_commit_should_advance_runtime_binding(
                terminal_output_committed,
                watcher_tui_gate_outcome,
                terminal_delivery_committed,
            ) {
                // Keep the SSH-direct replay watermark in lockstep with committed bytes
                // (TimedOut gates only keep this a candidate while delivery is unmirrored).
                crate::services::tui_prompt_dedupe::advance_tmux_runtime_binding_offset(
                    &tmux_session_name,
                    &output_path,
                    candidate_offset,
                );
            }
        }
        // #2293 H2 — single boolean threaded through every terminal side effect below.
        // On `TimedOut` before the terminal delivery is durably mirrored, the pane is
        // still busy past the bounded wait, so SKIP: ✅ reaction; transcript/turn-analytics
        // persist (a completion row at this offset is wrong while output flows); history
        // append; confirmed-end advance; `clear_inflight_state` (intake admits the next
        // turn off inflight presence — wiping it races the busy pane);
        // `finish_restored_watcher_active_turn` (mailbox cancel_token, same reason);
        // deferred idle-queue kickoff; terminal-finalize stop. Once delivery is durably
        // mirrored, match the bridge: suppress visible completion on timeout but allow
        // lifecycle cleanup to release inflight/mailbox state and drain follow-ups.
        let lifecycle_stage_paused = watcher_tui_gate_blocks_lifecycle(
            watcher_tui_gate_outcome,
            terminal_delivery_committed,
        );
        if lifecycle_stage_paused {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %watcher_provider.as_str(),
                channel = channel_id.get(),
                tmux_session = %tmux_session_name,
                "[{ts}] ⚠ #2293: watcher lifecycle-stage paused — TUI quiescence gate timed out; submitting GateTimeout to the finalizer's deadline-armed reconciler instead of deferring to a never-firing next pass"
            );
            // #3016 phase 3: the silent SKIP the EPIC targets. The
            // `terminal_output_committed && !lifecycle_stage_paused` blocks below are
            // skipped, so submit a busy gate-timeout: the finalizer arms the short
            // GATE_BACKSTOP and reconciles later. Only fire after committed output.
            if terminal_output_committed {
                // Prefer the persisted finalizer id from inflight so this resolves
                // to the exact Watcher-owned ledger entry (→ DEFERS to backstop).
                let gate_finalizer_turn_id =
                    crate::services::discord::inflight::load_inflight_state(
                        &watcher_provider,
                        channel_id.get(),
                    )
                    .map(|s| s.effective_finalizer_turn_id())
                    .unwrap_or(0);
                let _ = shared
                    .turn_finalizer
                    .submit_terminal(
                        crate::services::discord::turn_finalizer::TurnKey::new(
                            channel_id,
                            gate_finalizer_turn_id,
                            shared.restart.current_generation,
                        ),
                        watcher_provider.clone(),
                        crate::services::discord::turn_finalizer::TerminalEvent::GateTimeout {
                            pane_quiescent: Some(false),
                        },
                        crate::services::discord::turn_finalizer::FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
            }
        }

        if terminal_output_committed && watcher_tui_gate_outcome.should_emit_completion() {
            // #2849: watcher-completed turns never traverse the bridge
            // StatusUpdate path, so the completed panel can lack the Context
            // line even when terminal output carried exact usage. Backfill the
            // exact final context usage onto the panel BEFORE rendering the
            // completed panel. Skip entirely when no exact usage exists or the
            // provider/model has no resolvable window — never fabricate numbers
            // and never reuse stale prior-turn usage. set_context_panel_usage is
            // also internally gated to context_window != 0. #3262: the same
            // turn-idle helper also injects `/compact` when live Claude usage
            // crosses the configured threshold (claude-only, once-per-cycle).
            crate::services::discord::adk_session::backfill_completed_panel_usage_and_maybe_inject_compact(
                &shared, channel_id, &state, &watcher_provider, &tmux_session_name,
            )
            .await;
            // #2427 D wire (Codex round 2 HIGH-1): the watcher loop is not
            // turn-scoped — a new turn may have rewritten on-disk inflight by now, so
            // re-reading user_msg_id and feeding it into `clear_inflight_state_if_matches`
            // becomes self-authentication and *enables* the Pitfall #1 race the guard
            // prevents. Drop the explicit-signal hook on the watcher D wire and rely on
            // the unconditional `clear_inflight_state` at L~2996 (committed-output path).
            // The recovery_engine D wire is preserved (its `state.user_msg_id` is pinned
            // at recovery entry, not re-read at completion).
            // #3142: offset-pin the status-panel completion identity. The old
            // session-only derivation would bind the panel to a NEWER follow-up
            // snapshot (`turn_start_offset >= current_offset`) this range does NOT own,
            // aliasing completion onto the still-running newer turn. Reuse
            // `pinned_finalize_user_msg_id` (the `< current_offset` test) so the
            // identity is None for a newer snapshot, agreeing with the
            // reaction/transcript/analytics + finalize gate. The panel EDIT/finalize
            // below is ALSO gated on `!inflight_before_relay_is_stale_newer_turn` so a
            // stale newer panel is never completed (UI-only aliasing gap). In-range
            // turns are unchanged (gate false; only `expected_user_msg_id` pinned);
            // `!rebind_origin` kept for parity.
            //
            // #3142: same stale-newer predicate as the adopt site (L8328). The panel
            // can be owned by a NEWER turn with `user_msg_id == 0` (external/injected),
            // so the id==0-INCLUSIVE anchor variant is required (the id!=0 sibling would
            // MISS it). The `None` 2nd arg is sound (`is_some_and` → false); an in-range
            // id==0 watcher-direct turn is NOT flagged and STILL completes — the gate
            // keys off the OFFSET test, not `pinned == 0`.
            let inflight_before_relay_is_stale_newer_turn =
                committed_anchor_cleanup_is_stale_for_newer_turn(
                    inflight_before_relay.as_ref(),
                    None,
                    &tmux_session_name,
                    current_offset,
                );
            let pinned_status_panel_user_msg_id = pinned_finalize_user_msg_id(
                inflight_before_relay.as_ref(),
                &tmux_session_name,
                current_offset,
            );
            let status_panel_completion_user_msg_id = inflight_before_relay
                .as_ref()
                .filter(|inflight| !inflight.rebind_origin)
                .and_then(|_| {
                    (pinned_status_panel_user_msg_id != 0)
                        .then_some(pinned_status_panel_user_msg_id)
                });
            // #3055: re-derive this turn's session lifecycle panel line before
            // finalizing. The bridge does this on every status tick via
            // `refresh_session_panel_line_from_lifecycle`; the watcher-direct
            // completion path historically skipped it and so reused a stale
            // per-channel `🆕 새 세션 시작 (최근 대화 N개…)` snapshot from a prior
            // recovery/new-session turn. A watcher-direct TUI turn has
            // `user_msg_id == 0`, keying onto the `discord:<channel>:0` turn id
            // which has no session lifecycle row, so the panel is cleared and
            // the stale line is not rendered.
            let session_panel_lifecycle_user_msg_id = inflight_before_relay
                .as_ref()
                .filter(|inflight| {
                    inflight
                        .tmux_session_name
                        .as_deref()
                        .map(str::trim)
                        .is_some_and(|name| !name.is_empty() && name == tmux_session_name)
                })
                .map(|inflight| inflight.user_msg_id)
                .unwrap_or(0);
            refresh_watcher_session_panel_from_lifecycle(
                &shared,
                channel_id,
                session_panel_lifecycle_user_msg_id,
                &tmux_session_name,
                &watcher_provider, // #3983 item4: one-shot session banner render
            )
            .await;
            // #3142: gate the EDIT/finalize + orphan-store reconciliation on
            // `!inflight_before_relay_is_stale_newer_turn`. When the pre-relay
            // snapshot is a stale NEWER turn the older committed range must NOT touch
            // that newer turn's panel (or its orphan record). The current in-range
            // turn's own panel, if any, is created via the streaming sources and is
            // unaffected (in-range => gate false => completion fires as today).
            if !inflight_before_relay_is_stale_newer_turn {
                let completion_background = matches!(
                    task_notification_kind,
                    Some(TaskNotificationKind::Background | TaskNotificationKind::MonitorAutoTurn)
                );
                // #3969 root invariant: read the CHOKEPOINT-FRESH inflight (this
                // `inflight_before_relay` is re-loaded after the synthetic row exists,
                // unlike the stale `:1017` flag) and suppress the #3089 footer for any
                // non-Managed (TUI-mirror) turn — closing the /loop self-paced leak.
                let turn_is_non_managed_tui_mirror =
                    watcher_inflight_is_non_managed_tui_mirror_for_session(
                        inflight_before_relay.as_ref(),
                        &tmux_session_name,
                    );
                // #3805 P2 (PR-C): skip the status-panel completion edit when a
                // NEWER panel epoch has superseded this stale completion for the
                // SAME owned panel (parity with the sink completion guard). Inert
                // on the default-OFF path (generation stays 0) and at PR-C (no
                // mid-turn re-anchor bumps the epoch); the re-anchor stage (PR-D)
                // makes it live.
                let two_message_status_panel_generation_superseded =
                    watcher_two_message_status_completion_superseded(
                        this_turn_status_panel_generation,
                        status_panel_msg_id,
                        inflight_before_relay.as_ref(),
                    );
                complete_watcher_terminal_footer_or_status_panel(
                    &http,
                    &shared,
                    channel_id,
                    &watcher_provider,
                    status_panel_started_at,
                    single_message_panel_footer_mode,
                    &mut completion_footer_spin_idx,
                    completion_footer_terminal_target.clone(),
                    placeholder_msg_id,
                    &last_edit_text,
                    status_panel_msg_id,
                    &mut last_status_panel_text,
                    completion_background,
                    status_panel_completion_user_msg_id,
                    turn_is_external_input_for_session,
                    turn_is_non_managed_tui_mirror,
                    two_message_status_panel_generation_superseded,
                )
                .await;
            } // #3142: end `if !inflight_before_relay_is_stale_newer_turn` (EDIT/finalize gate)
            // #3003 single-chokepoint reclaim safety: after completion the turn
            // frame ends and the next frame re-seeds `status_panel_msg_id`, so the
            // top-of-interval abandon reclaim never observes this finalized panel's
            // id again — no explicit reset needed here.
        }

        // #3646 OBSERVATION-ONLY (event 2/3 — terminal_ui_transition): label the
        // visible-UI path the watcher took. Reads the same signals the EDIT/finalize
        // block already branched on (`watcher_tui_gate_outcome` + the #3142
        // stale-newer gate) — no new decision, only RECORDS committed /
        // gate_suppressed / stale_identity. The guard gates the EMIT, not the
        // cleanup. Orchestration lives in relay_owner_observability (non-hot file).
        if terminal_output_committed {
            let ui_transition_pane_quiescent = match watcher_tui_gate_outcome {
                TuiCompletionGateOutcome::ConfirmedIdle => Some(true),
                TuiCompletionGateOutcome::TimedOut => Some(false),
                // NotGated / SkippedDead: quiescence was not probed.
                TuiCompletionGateOutcome::NotGated | TuiCompletionGateOutcome::SkippedDead => None,
            };
            crate::services::discord::relay_owner_observability::emit_terminal_ui_transition(
                watcher_provider.as_str(),
                channel_id.get(),
                inflight_before_relay
                    .as_ref()
                    .and_then(|s| s.dispatch_id.as_deref()),
                inflight_before_relay
                    .as_ref()
                    .and_then(|s| s.session_key.as_deref()),
                pinned_finalize_user_msg_id(
                    inflight_before_relay.as_ref(),
                    &tmux_session_name,
                    current_offset,
                ),
                crate::services::discord::relay_owner_observability::TerminalUiOutcome::derive(
                    inflight_before_relay_is_stale_newer_turn,
                    watcher_tui_gate_outcome.should_emit_completion(),
                ),
                &format!("{watcher_tui_gate_outcome:?}"),
                ui_transition_pane_quiescent,
            );
        }

        // Advance the shared confirmed-delivery watermark on any committed
        // direct emission or empty-turn cleanup. CAS loop ensures we only ever move the
        // watermark FORWARD, even if some other instance has raced ahead.
        // #2293 H2 — pinning the watermark while the gate is TimedOut is what
        // keeps the next pass's gate evaluation pointed at the same JSONL
        // slice; advancing here would let `tmux_tail_offset` equal
        // `confirmed_end` on the retry, falsely claiming there's nothing
        // new to relay.
        let terminal_committed_offset = runtime_binding_candidate_offset.unwrap_or(current_offset);
        // #3041 P1-1 (§3, codex R2 Issue-1): the send completed by here. STOP the
        // heartbeat BEFORE the inline commit so the renew loop cannot race the
        // `commit`/`release`. A tick fired before `stop()` only `renew`s our OWN still-
        // `Leased` lease (no-op extension), which the commit flips to `Committed`; after
        // `stop()` no renews occur. `None` on the non-acquired arms (incl. #3089 A4
        // cutover — the controller ran its own heartbeat), so this is a no-op there.
        if let Some(hb) = watcher_lease_heartbeat {
            hb.stop();
        }
        if watcher_lease_acquired {
            // #3041 P1-1 (§5.2): commit the 3-way outcome and, on `Delivered`, advance
            // `confirmed_end_offset` — both INLINE at the pre-P1-1 timing. (#3089 A4: the
            // cut-over short-replace path is `watcher_lease_acquired == false` here — the
            // controller already committed+advanced+released its own lease.)
            //
            // WHY INLINE (not the awaited `CommitDelivery`/`ReleaseDelivery` actor): the
            // actor-commit could queue behind an awaited `Terminal` handler, keeping
            // `confirmed_end_offset` OLD across that await while `session_relay_sink`
            // (dedups on `committed_relay_offset` until P1-2) re-relays the SAME range →
            // the #3143 duplicate. Inline commit+advance keeps that consult current. The
            // cell's `commit` is an atomic CAS, so §5.2 holds without the actor;
            // ledger-coupling (§5.3) deferred (advance is a standalone monotonic CAS).
            //
            // 3-way: `Delivered` (advance to leased `end`), `NotDelivered` (clean send
            // failure), `Unknown` (TUI gate left us lifecycle-paused → ambiguous, do NOT
            // claim delivered). Advance ONLY on `Delivered`, mirroring the old
            // `!lifecycle_stage_paused` gate (leased `end` == `terminal_committed_offset`
            // on the committed path). Then release inline (same-holder) for the next turn.
            let commit_outcome = if lifecycle_stage_paused {
                crate::services::discord::LeaseOutcome::Unknown
            } else if relay_ok {
                crate::services::discord::LeaseOutcome::Delivered
            } else {
                crate::services::discord::LeaseOutcome::NotDelivered
            };
            let committed = watcher_lease_cell.commit(
                watcher_lease_holder,
                watcher_lease_key.clone(),
                watcher_lease_start,
                watcher_lease_end,
                commit_outcome,
            );
            debug_assert!(
                committed,
                "watcher must be able to commit its own freshly-acquired lease"
            );
            if committed && commit_outcome == crate::services::discord::LeaseOutcome::Delivered {
                // INLINE advance — exactly the pre-P1-1 call site/timing.
                advance_watcher_confirmed_end(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &tmux_session_name,
                    watcher_lease_end,
                    "src/services/discord/tmux_watcher.rs:watcher_lease_commit_advance",
                );
                // #3610 PR-1d: record the durable terminal anchor for the legacy
                // long-chunk fallback arm ONLY here — gated on the SAME successful
                // commit+advance (M4) AND `Some` anchor (⇒ the long-chunk arm ran and
                // fully committed, (A)). Same-channel; logic in the sibling.
                if let Some(anchor) = watcher_long_chunk_anchor_msg_id {
                    terminal_send::record_watcher_long_chunk_terminal_delivery(
                        &shared,
                        &watcher_provider,
                        channel_id,
                        (watcher_lease_start, watcher_lease_end),
                        Some(anchor.get()),
                    );
                }
            }
            // Release (Unleased for the next turn). Inline same-holder compare-and-
            // release; idempotent no-op if the identity no longer matches (e.g. a dead
            // holder's lease was reclaimed after the deadline elapsed).
            let _ = watcher_lease_cell.release(
                watcher_lease_holder,
                watcher_lease_key.clone(),
                watcher_lease_start,
                watcher_lease_end,
            );
        } else if terminal_output_committed && !lifecycle_stage_paused {
            // Non-watcher-direct committed paths (relay-suppressed task notifications,
            // empty-turn cleanup, session-bound delegation that consumed the range) keep
            // the inline monotonic-CAS advance — NOT the lease-governed delivery path.
            advance_watcher_confirmed_end(
                &shared,
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                terminal_committed_offset,
                "src/services/discord/tmux.rs:tmux_output_watcher_confirmed_end",
            );
        }
        // #3104: terminal/idle reconciliation. A turn can commit (the channel is
        // about to return to idle) without ever relaying a body onto the live
        // streaming placeholder — e.g. a session-bound/subagent-only turn whose
        // terminal output was delegated elsewhere, so `placeholder_msg_id` keeps
        // the last streaming edit it received. When that last edit still ends in
        // the transient `⠏ 계속 처리 중` footer, the message is left advertising
        // "still processing" forever (the legacy in-body footer counterpart to
        // the status-panel reclaim below). Strip the footer through the shared
        // final-output formatter so the visible message matches the idle runtime.
        //
        // Self-gated: only on genuine commit (not a TimedOut/lifecycle-paused
        // pane), and only when the body still ends with a footer — a
        // genuinely-still-streaming message never reaches this committed-output
        // block, and an already-finalized body is left untouched.
        if terminal_output_committed
            && !lifecycle_stage_paused
            && !single_message_panel_footer_mode
            && let Some(placeholder) = placeholder_msg_id
            && let Some(finalized) = finalize_watcher_streaming_footer(
                single_message_panel_footer_mode,
                &last_edit_text,
                &watcher_provider,
            )
        {
            match crate::services::discord::http::edit_channel_message(
                &http,
                channel_id,
                placeholder,
                &finalized,
            )
            .await
            {
                Ok(_) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 👁 #3104 reconciled stale '계속 처리 중' streaming footer on channel {} msg {} at idle",
                        channel_id.get(),
                        placeholder.get()
                    );
                }
                Err(error) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ #3104 failed to reconcile stale streaming footer on channel {} msg {}: {error}",
                        channel_id.get(),
                        placeholder.get()
                    );
                }
            }
        }
        // Release the emission slot regardless of success. If delivery failed
        // the local `last_relayed_offset` also stayed put, so the same watcher
        // (or its replacement) can retry on the next tick without fighting
        // the slot. #2840: via the RAII guard, so a panic/abort before this
        // point also frees the slot (Drop) instead of wedging the channel.
        slot_guard.release();

        finish_monitor_auto_turn_if_claimed(
            &shared,
            &watcher_provider,
            channel_id,
            &mut monitor_auto_turn_claimed,
            &mut monitor_auto_turn_finished,
            &mut monitor_auto_turn_synthetic_msg_id,
            &mut monitor_auto_turn_ledger_generation,
        )
        .await;

        let provider_kind = watcher_provider.clone();
        let inflight_state = crate::services::discord::inflight::load_inflight_state(
            &provider_kind,
            channel_id.get(),
        );
        let watcher_session_id = state.last_session_id.clone();
        if terminal_output_committed {
            persist_watcher_provider_session_id(
                &shared,
                channel_id,
                &provider_kind,
                &tmux_session_name,
                watcher_session_id.as_deref(),
            )
            .await;
        }
        let result_usage = stream_line_state_token_usage(&state);
        if inflight_state.is_none() {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: inflight state missing for channel {} — using DB dispatch fallback",
                channel_id.get()
            );
        }

        // #3016 (codex R3): the late `inflight_state` re-read above (and the
        // pre-relay snapshot) can already hold a NEWER follow-up turn's id in the
        // R2/R3 offset-aliasing scenario — a follow-up on the SAME tmux session
        // whose `turn_start_offset >= current_offset` (it begins AFTER this
        // committed output range) does NOT make the watcher-yield guard yield, so
        // the watcher still processes this OLD range while inflight on disk
        // belongs to the newer turn. The finalize below is already safe (it uses
        // `pinned_finalize_user_msg_id`, which returns 0 for such a newer turn —
        // the EXACT complement of this gate's offset test), but the SAME block
        // also runs the `⏳ → ✅` reaction + transcript + analytics write and
        // `clear_inflight_state` on that late read. Compute the stale-range gate
        // ONCE here and skip those wrong-turn side-effects (see the two call sites
        // below). For every normal completion (inflight is THIS or an OLDER turn,
        // absent, or rebind_origin/`user_msg_id == 0`) this is FALSE → no-op.
        let completion_is_stale_for_newer_turn = committed_completion_is_stale_for_newer_turn(
            inflight_before_relay.as_ref(),
            inflight_state.as_ref(),
            &tmux_session_name,
            current_offset,
        );

        // #3142: the id==0-inclusive sibling gate for the two anchor-cleanup
        // branches below. The id!=0 `completion_is_stale_for_newer_turn` above
        // deliberately excludes `user_msg_id == 0` newer turns (to protect the
        // finalize/clear id-0 contract), but a newer external-input / injected
        // task-notification turn can have `user_msg_id == 0` while still owning a
        // real anchor (`injected_prompt_message_id` or the shared
        // `prompt_anchor_by_tmux` slot). Computing this once here keeps the late
        // re-read and the pre-relay snapshot both checked for the anchor branches.
        let anchor_cleanup_is_stale_for_newer_turn =
            committed_anchor_cleanup_is_stale_for_newer_turn(
                inflight_before_relay.as_ref(),
                inflight_state.as_ref(),
                &tmux_session_name,
                current_offset,
            );

        if !anchor_cleanup_is_stale_for_newer_turn
            && crate::services::discord::tui_prompt_relay::should_complete_tui_direct_anchor_lifecycle(
            terminal_output_committed,
            tui_direct_anchor_terminal_body_visible,
            tui_direct_anchor_or_lease_present_for_lifecycle,
            lifecycle_stage_paused,
            inflight_state.is_some(),
        ) {
            // #3350 issue-1 + codex r1-2 (lease-gated row-absent commit,
            // tombstone-BEFORE-deliver): resolve the #3303 own-pin markers for
            // the anchor we are ABOUT to ✅ — synchronously, before the Discord
            // await below. The old deliver-then-resolve order let a TTL sweep
            // firing during (or just before) the await claim the row-absent
            // marker uncovered and stack a ⚠ next to the delivered ✅. If the
            // ✅ delivery below then fails, the anchor keeps its ⏳ for retry
            // with the marker already resolved — the same residual state as
            // pre-PR (no marker existed), not a regression.
            if let Some(anchor) = crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
                watcher_provider.as_str(),
                &tmux_session_name,
                channel_id.get(),
            ) {
                crate::services::discord::tui_direct_abort_marker::resolve_own_claim_markers_for_visibly_completed_anchor(
                    watcher_provider.as_str(),
                    &tmux_session_name,
                    channel_id.get(),
                    anchor.message_id,
                );
            }
            let completed = crate::services::discord::tui_prompt_relay::complete_tui_direct_prompt_anchor_lifecycle_if_present(
                &http,
                watcher_provider.as_str(),
                &tmux_session_name,
                channel_id,
                if lifecycle_stage_paused {
                    "watcher_terminal_delivery_visible_completion_suppressed"
                } else {
                    "watcher_terminal_delivery_visible_without_inflight"
                },
            )
            .await;
            // #3174: turn-identity guard on the ⏳ lifecycle vs the lease-gated
            // completion. The gate above can fire on the external-input LEASE
            // alone; a commit inside the sub-second `notify-post + ⏳-add`
            // window finds THIS turn's `record_prompt_anchor` not yet landed —
            // the completion above no-ops (`None`) and the lease clears after
            // delivery, stranding the ⏳. Record a deferred-completion marker
            // keyed to `(provider, tmux, channel)`; the SAME turn's
            // `record_prompt_anchor` (relay) drains it and finishes the swap.
            // Only when the anchor is genuinely still absent — a `None` from a
            // `create_reaction` error keeps the anchor findable and retries.
            // codex P1: stamp the gating lease's `generation`; the relay drains
            // ONLY on a matching generation, so a NEWER same-tmux turn cannot
            // complete the wrong ⏳. Anchor-only firings stay anchor-based.
            if completed.is_none()
                && let Some(turn_lease_generation) = external_input_lease_generation_before_relay
                && crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
                    watcher_provider.as_str(),
                    &tmux_session_name,
                    channel_id.get(),
                )
                .is_none()
            {
                crate::services::tui_prompt_dedupe::record_deferred_anchor_completion(
                    watcher_provider.as_str(),
                    &tmux_session_name,
                    channel_id.get(),
                    turn_lease_generation,
                );
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏳ #3174 watcher: lease-gated completion ran before anchor recorded (channel {}, tmux={}, turn_lease_generation={turn_lease_generation}) — deferred ⏳ completion to record_prompt_anchor",
                    channel_id.get(),
                    tmux_session_name
                );
            }
        } else if terminal_output_committed
            && !lifecycle_stage_paused
            && !anchor_cleanup_is_stale_for_newer_turn
            && inflight_state
                .as_ref()
                .is_some_and(watcher_inflight_needs_anchor_lifecycle_cleanup)
        {
            // #3099: the `⏳ → ✅` block below targets `state.user_msg_id`, but a
            // TUI-injected task-notification turn can complete with an inflight
            // whose `user_msg_id == 0` (no anchored Discord user message) while a
            // real notify-bot message still carries the `⏳`. The
            // `should_complete_tui_direct_anchor_lifecycle` gate above does not
            // fire here because an inflight is still present, so clean the
            // hourglass off the injected message's OWN id.
            //
            // #3099 codex re-review (P2): target THIS turn's pinned
            // `injected_prompt_message_id` rather than re-reading the single shared
            // prompt-anchor slot — under rapid/parallel injection that slot may
            // already belong to a later turn, and reading it would `✅` the wrong
            // (still-running) message.
            let pinned_injected_message_id = inflight_state
                .as_ref()
                .and_then(|state| state.injected_prompt_message_id);
            let _ = crate::services::discord::tui_prompt_relay::complete_tui_direct_anchor_lifecycle_for_inflight(
                &http,
                watcher_provider.as_str(),
                &tmux_session_name,
                channel_id,
                pinned_injected_message_id,
                "watcher_task_notification_anchor_cleanup_user_msg_zero",
            )
            .await;
        }

        // Mark user message as completed: ⏳ → ✅ when inflight metadata is
        // available and terminal output is committed. #897 round-3 Medium:
        // skip the reaction + transcript + analytics block entirely for
        // `rebind_origin` inflights. Their `user_msg_id=0` points at no real
        // message, and persisting a transcript with
        // `turn_id=discord:<channel>:0` poisons session_transcripts /
        // turn_analytics. The notify-bot outbox enqueue above already
        // delivered the recovered response to the user; nothing else on the
        // success path is legitimate here.
        //
        // #2293 H2 — also skip on `lifecycle_stage_paused`. The ✅ reaction +
        // transcript row + analytics row all claim completion at this exact
        // JSONL offset; while the pane is still busy past the gate timeout
        // they would either lie about completion (✅) or write a row that
        // gets contradicted by the next pass (transcript / analytics).
        // Skip rebind_origin (synthetic) and user_msg_id == 0 (e.g. a
        // TUI-direct turn with no anchored Discord user message): there is no
        // message to react against, `discord:<channel>:0` would be a bogus
        // analytics/turn-id key, and `MessageId::new(0)` would panic. The
        // recovered response was already delivered via the notify-bot outbox
        // enqueue above, so skipping the reaction/analytics step is safe.
        //
        // #3016 (codex R3): also skip when `completion_is_stale_for_newer_turn` —
        // the late `inflight_state` belongs to a NEWER follow-up turn that began
        // AFTER this committed range. Marking it `✅` and writing its transcript /
        // analytics here would lie about a still-running turn's completion. The
        // finalize below independently refuses this turn (its
        // `pinned_finalize_user_msg_id` returns 0 via the complementary offset
        // test), so this gate keeps the reaction/transcript/analytics consistent
        // with that decision. No-op for every normal completion.
        if terminal_output_committed
            && !lifecycle_stage_paused
            && !completion_is_stale_for_newer_turn
            && let Some(state) = inflight_state
                .as_ref()
                .filter(|s| watcher_completion_lifecycle_applies(s))
        {
            let user_msg_id = serenity::MessageId::new(state.user_msg_id);
            crate::services::discord::formatting::remove_reaction_raw(
                &http,
                channel_id,
                user_msg_id,
                '⏳',
            )
            .await;
            crate::services::discord::formatting::add_reaction_raw(
                &http,
                channel_id,
                user_msg_id,
                '✅',
            )
            .await;

            if has_assistant_response
                && (None::<&crate::db::Db>.is_some() || shared.pg_pool.is_some())
            {
                let turn_id = format!("discord:{}:{}", channel_id.get(), state.user_msg_id);
                let channel_id_text = channel_id.get().to_string();
                let resolved_did = inflight_state
                    .as_ref()
                    .and_then(|s| s.dispatch_id.clone())
                    .or_else(|| {
                        crate::services::discord::adk_session::parse_dispatch_id(&state.user_text)
                    })
                    .or(
                        crate::services::discord::adk_session::lookup_pending_dispatch_for_thread(
                            shared.api_port,
                            channel_id.get(),
                        )
                        .await,
                    )
                    .or_else(|| {
                        resolve_dispatched_thread_dispatch_from_db(
                            shared.pg_pool.as_ref(),
                            channel_id.get(),
                        )
                    });
                if let Err(e) = crate::db::session_transcripts::persist_turn_db(
                    None::<&crate::db::Db>,
                    shared.pg_pool.as_ref(),
                    crate::db::session_transcripts::PersistSessionTranscript {
                        turn_id: &turn_id,
                        session_key: state.session_key.as_deref(),
                        channel_id: Some(channel_id_text.as_str()),
                        agent_id: resolve_role_binding(channel_id, state.channel_name.as_deref())
                            .as_ref()
                            .map(|binding| binding.role_id.as_str()),
                        provider: Some(provider_kind.as_str()),
                        dispatch_id: resolved_did.as_deref().or(state.dispatch_id.as_deref()),
                        user_message: &state.user_text,
                        assistant_message: &full_response,
                        events: &tool_state.transcript_events,
                        duration_ms: inflight_duration_ms(Some(state.started_at.as_str())),
                    },
                )
                .await
                {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!("  [{ts}] ⚠ watcher: failed to persist session transcript: {e}");
                }

                crate::services::discord::turn_bridge::persist_turn_analytics_row_with_handles(
                    None::<&crate::db::Db>,
                    shared.pg_pool.as_ref(),
                    &provider_kind,
                    channel_id,
                    user_msg_id,
                    resolve_role_binding(channel_id, state.channel_name.as_deref()).as_ref(),
                    resolved_did.as_deref().or(state.dispatch_id.as_deref()),
                    state.session_key.as_deref(),
                    watcher_session_id
                        .as_deref()
                        .or(state.session_id.as_deref()),
                    state,
                    result_usage.unwrap_or_default(),
                    inflight_duration_ms(Some(state.started_at.as_str())).unwrap_or(0),
                );
            }
        }

        let resolved_did = inflight_state
            .as_ref()
            .and_then(|state| state.dispatch_id.clone())
            .or_else(|| {
                inflight_state.as_ref().and_then(|state| {
                    crate::services::discord::adk_session::parse_dispatch_id(&state.user_text)
                })
            })
            .or(
                crate::services::discord::adk_session::lookup_pending_dispatch_for_thread(
                    shared.api_port,
                    channel_id.get(),
                )
                .await,
            )
            .or_else(|| {
                resolve_dispatched_thread_dispatch_from_db(
                    shared.pg_pool.as_ref(),
                    channel_id.get(),
                )
            });

        if resolved_did.is_none() && has_assistant_response {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: no dispatch id resolved for channel {} after terminal success",
                channel_id.get()
            );
        }
        let current_worktree_path = {
            let mut data = shared.core.lock().await;
            data.sessions
                .get_mut(&channel_id)
                .and_then(|session| session.validated_path(channel_id.get()))
        };

        // #2161 (Codex round-2 H1): if the TUI quiescence gate timed out
        // before terminal delivery was durably mirrored, treat the watcher
        // dispatch finalization as "preserved": don't complete the dispatch,
        // don't kick off queued work, and leave inflight alone so the next
        // watcher pass / placeholder sweeper observes the still-busy pane and
        // reconciles. Once delivery is mirrored, match the bridge path and
        // allow cleanup while still suppressing visible completion.
        let dispatch_ok = if lifecycle_stage_paused {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %watcher_provider.as_str(),
                channel = channel_id.get(),
                tmux_session = %tmux_session_name,
                "[{ts}] ⚠ watcher: dispatch finalization deferred — TUI quiescence gate timed out (#2161)"
            );
            false
        } else if let Some(did) = resolved_did
            .as_deref()
            .filter(|_| !completion_is_stale_for_newer_turn)
        {
            // #3142: when stale, the late `inflight_state.dispatch_id` (the first
            // fallback in `resolved_did`) belongs to the NEWER running turn;
            // completing it here with the OLDER `full_response` is wrong-turn
            // corruption. Fall through to the `else => true` no-finalize arm
            // (dispatch_ok stays true; downstream clear/finalize keep their own
            // stale gates) — the newer turn finalizes its own dispatch on its later
            // pass. FALSE in every normal case, so the common finalize is untouched.
            let finalization =
                crate::services::discord::streaming_finalizer::finalize_watcher_streaming_dispatch(
                    crate::services::discord::streaming_finalizer::WatcherStreamingFinalRequest {
                        pg_pool: shared.pg_pool.as_ref(),
                        dispatch_id: did,
                        adk_cwd: current_worktree_path.as_deref(),
                        full_response: &full_response,
                        has_assistant_response,
                    },
                )
                .await;
            if !finalization.completed {
                tracing::debug!(
                    disposition = ?finalization.disposition,
                    dispatch_type = ?finalization.dispatch_type,
                    error = ?finalization.error,
                    "watcher streaming finalizer preserved dispatch state"
                );
            }
            finalization.completed
        } else {
            true
        };

        // #225 P1-2 / #1708 follow-up: clear inflight when the terminal output
        // was either delivered to Discord or intentionally suppressed as an
        // internal task notification. Only genuine delivery failure preserves
        // retry/handoff state for next startup.
        //
        // #2293 H2 — skip the entire block on `lifecycle_stage_paused`. Wiping
        // inflight + releasing the mailbox cancel_token while the pane is
        // still busy is exactly the cascade the issue is filed against: the
        // intake gate would see an empty inflight and a free mailbox and
        // admit a new turn into a non-quiescent pane. The next watcher pass
        // re-evaluates the gate and finishes the cleanup once the pane
        // actually reports idle.
        if terminal_output_committed && !lifecycle_stage_paused {
            // #3142: gate the TUI history push on `!completion_is_stale_for_newer_turn`.
            // When stale, the late `inflight_state.user_text` is the NEWER turn's
            // prompt; pairing it with the OLDER `full_response` would poison the
            // TUI history. The newer turn pushes its own (user_text, response) pair
            // on its own completion pass. Only the push is suppressed —
            // `turn_result_relayed` and the clear/finalize bookkeeping below keep
            // their own (already-shipped) stale gates. FALSE in every normal case.
            // #3142: gate on BOTH the id!=0 stale helper AND the id==0-inclusive
            // `anchor_cleanup_is_stale_for_newer_turn` (computed above) so a NEWER
            // external-input turn with `user_msg_id == 0` (no own dispatch id,
            // `rebind_origin == false`, populated `user_text`) cannot cross-pair
            // its `user_text` with the OLDER `full_response` in the TUI history.
            if has_assistant_response
                && !completion_is_stale_for_newer_turn
                && !anchor_cleanup_is_stale_for_newer_turn
                && let Some(state) = inflight_state.as_ref().filter(|state| !state.rebind_origin)
            {
                let mut data = shared.core.lock().await;
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    if !session.cleared {
                        session.history.push(crate::ui::ai_screen::HistoryItem {
                            item_type: crate::ui::ai_screen::HistoryType::User,
                            content: state.user_text.clone(),
                        });
                        session.history.push(crate::ui::ai_screen::HistoryItem {
                            item_type: crate::ui::ai_screen::HistoryType::Assistant,
                            content: full_response.clone(),
                        });
                    }
                }
                drop(data);
            }
            turn_result_relayed = true;
            // #1670/#1708: always consume the handoff debt and clear inflight
            // when terminal output was committed — the bridge's
            // `bridge_relay_delegated_to_watcher` arm saves inflight and never
            // returns to clear it even if dispatch finalization fails (a stale
            // fallback dispatch_id with `dispatch_ok = false` used to orphan
            // the inflight + cancel_token forever). Decoupling rule: clear +
            // `finish_restored_watcher_active_turn` fire on every committed
            // terminal (idempotent under bridge/watcher concurrency), while
            // dispatch-lifecycle side-effects (queue kickoff, followup,
            // terminal-stop) stay gated on `dispatch_ok` below.
            // #3016 phase-5b2: the legacy `mailbox_finalize_owed` flag is
            // removed — exactly-once is the ledger phase gate's job.
            // #3016 (codex R3): do NOT delete on-disk inflight owned by a
            // NEWER follow-up turn — the same offset decision that zeroes
            // `pinned_finalizer_turn_id` below gates this clear, so a
            // stale-range pass cannot wipe it. Only the on-disk file is gated;
            // the in-memory `inflight_state` and `cleared_by_watcher` keep
            // their semantics.
            // #3296 codex r2: aborted-anchor reconcile, sited BEFORE the row
            // clear — tombstone evidence (committed turn identity) lands first,
            // then the drain covers, then the clear; a sweep claiming a marker
            // mid-commit sees "no live row" only AFTER the tombstone is durable,
            // so its 대조 lands ✅ not ⚠ (r2 finding 1). An ABORT recording its
            // marker after this drain still converges via the tombstone 대조.
            // #3350 issue-1: ALSO tombstone+drain body-INVISIBLE commits of
            // watcher-owned synthetic rows (suppressed task-notification
            // completions) — their `⏳ → ✅` block fires regardless, and skipping
            // here left their own-pin marker to a false TTL `⚠`.
            if !completion_is_stale_for_newer_turn
                && let Some(committed) = inflight_state.as_ref()
                && (tui_direct_anchor_terminal_body_visible
                    || committed_row_requires_marker_tombstone(committed))
            {
                crate::services::discord::tui_direct_abort_marker::record_commit_tombstone(
                    watcher_provider.as_str(),
                    &tmux_session_name,
                    channel_id.get(),
                    committed.user_msg_id,
                    &committed.started_at,
                );
                let _ =
                    crate::services::discord::tui_direct_abort_marker::drain_on_terminal_commit(
                        &shared,
                        watcher_provider.as_str(),
                        &tmux_session_name,
                        channel_id.get(),
                        committed.user_msg_id,
                        &committed.started_at,
                    )
                    .await;
            }
            if !completion_is_stale_for_newer_turn {
                crate::services::discord::inflight::clear_inflight_state(
                    &provider_kind,
                    channel_id.get(),
                );
                let watcher_turn_id = inflight_state
                    .as_ref()
                    .filter(|s| s.user_msg_id != 0)
                    .map(|s| format!("discord:{}:{}", s.channel_id, s.user_msg_id));
                let watcher_session_key_owned =
                    inflight_state.as_ref().and_then(|s| s.session_key.clone());
                let watcher_dispatch_id_owned = resolved_did
                    .clone()
                    .or_else(|| inflight_state.as_ref().and_then(|s| s.dispatch_id.clone()));
                crate::services::observability::emit_inflight_lifecycle_event(
                    provider_kind.as_str(),
                    channel_id.get(),
                    watcher_dispatch_id_owned.as_deref(),
                    watcher_session_key_owned.as_deref(),
                    watcher_turn_id.as_deref(),
                    "cleared_by_watcher",
                    serde_json::json!({
                        "dispatch_ok": dispatch_ok,
                        "has_assistant_response": has_assistant_response,
                        "full_response_len": full_response.len(),
                    }),
                );
                // #3646 OBSERVATION-ONLY (event 3/3 — inflight_clear + invariant
                // signal): the committed-output, non-stale watcher clear — the exact
                // #3607 chokepoint. The clear ABOVE has already run; this only
                // RECORDS the live lifecycle signals and fires a NON-FATAL
                // ERROR-level invariant signal if a committed terminal was cleared
                // with neither a visible UI completion nor a persisted obligation
                // (#3607). Never gates cleanup. `terminal_ui_obligation_persisted` is
                // `false` on the watcher path (obligations are the bridge
                // gate-timeout path; the watcher's TimedOut gate routes through the
                // finalizer GateTimeout submit, which suppresses THIS clear via
                // `lifecycle_stage_paused`). Orchestration + the non-fatal invariant
                // live in relay_owner_observability (non-hot file).
                crate::services::discord::relay_owner_observability::emit_inflight_clear_with_invariant(
                    provider_kind.as_str(),
                    channel_id.get(),
                    watcher_dispatch_id_owned.as_deref(),
                    watcher_session_key_owned.as_deref(),
                    watcher_turn_id.as_deref(),
                    terminal_delivery_committed,
                    terminal_output_committed && watcher_tui_gate_outcome.should_emit_completion(),
                    false,
                );
            }
            // codex P2 (#1670): cleanup (mailbox_finish_turn + cancel_token
            // release) MUST run on every relay-completed terminal even when
            // `dispatch_ok = false` (else organic turns leak forever), but the
            // queue-kickoff side-effect stays gated on `dispatch_ok`. The redundant
            // `should_kickoff_queue` block further below is also `dispatch_ok`-gated
            // as a fallback for paths where the helper short-circuited.
            // #3016/#3645: derive the finalizer id from the TURN-PINNED pre-relay
            // snapshot, with `pinned_finalizer_turn_id` mirroring the output-range
            // guard so newer same-session follow-ups yield 0 and skip destructive
            // completion side effects consistently.
            let restored_finalizer_turn_id = pinned_finalizer_turn_id(
                inflight_before_relay.as_ref(),
                &tmux_session_name,
                current_offset,
            );
            // #3016 (codex B1): SKIP the normal-completion finalize ENTIRELY in the
            // stale-newer-turn case — do NOT call it with a channel-only id 0.
            // A 0-id submit is unsafe, not a no-op: `normal_completion = true`
            // finalizes UNCONDITIONALLY, and a 0-id `TurnKey` reaches
            // `resolve_channel_only` (turn_finalizer.rs:161-181) which collapses onto
            // the single live non-finalized entry. In the stale case the OLD turn
            // (whose trailing output this is) already finalized, so the only live
            // entry is the NEWER still-running turn — a 0-id Complete would
            // wrong-finalize it, releasing its cancel_token/ledger mid-flight. Finalize
            // NOTHING here; the newer turn finalizes itself when ITS output commits in
            // a later loop iteration. `completion_is_stale_for_newer_turn` is the exact
            // complement of the `< current_offset` range test in
            // `pinned_finalizer_turn_id`; a same-session snapshot whose pinned id
            // resolves to 0 is skipped rather than submitted channel-only.
            //
            // Skip-path bookkeeping: `watcher_drove_finalize = false`. #3016
            // phase-5b2: the legacy `mailbox_finalize_owed` flag is removed — the
            // newer turn finalizes via its own real-id path, and the stale-skip is
            // already kickoff-suppressed by `has_active_turn` (the newer live turn).
            let watcher_drove_finalize = if should_submit_restored_watcher_finalize(
                completion_is_stale_for_newer_turn,
                restored_finalizer_turn_id,
            ) {
                finish_restored_watcher_active_turn(
                    &shared,
                    &provider_kind,
                    channel_id,
                    restored_finalizer_turn_id,
                    finish_mailbox_on_completion,
                    // #3016 option A: terminal output was committed above
                    // (`terminal_output_committed && !lifecycle_stage_paused`), the
                    // canonical *normal completion* point. Finalize unconditionally —
                    // independent of `finish_mailbox_on_completion` — so the normal
                    // live bridge→watcher delegation turn no longer depends on the
                    // legacy `mailbox_finalize_owed` flag (removed in #3016
                    // phase-5b2). The finalizer is idempotent (bridge winner →
                    // AlreadyFinalized here), so this cannot over-finalize.
                    true,
                    dispatch_ok,
                    // #3350 codex r1-1: inflight was cleared above — carry the
                    // pre-relay snapshot (the same row `restored_user_msg_id` was
                    // pinned from) for the finalize-time marker ensure.
                    inflight_before_relay.as_ref().map(
                        crate::services::discord::turn_finalizer::SyntheticClaimSnapshot::from_row,
                    ),
                    "restored watcher completed with queued backlog",
                )
                .await
            } else {
                // Stale-newer-turn: finalize skipped (see above). The watcher did
                // not drive any finalize on this pass.
                false
            };
            if !watcher_direct_terminal_idle_committed {
                watcher_direct_terminal_idle_committed =
                    commit_watcher_direct_terminal_session_idle(
                        &shared,
                        &provider_kind,
                        channel_id,
                        &tmux_session_name,
                        terminal_kind,
                        data_start_offset,
                        current_offset,
                    )
                    .await;
            }
            let mailbox = shared.mailbox(channel_id);
            let has_active_turn = mailbox.has_active_turn().await;
            // #3016 (codex R1) / phase-5b2: couple the post-finalize lifecycle to
            // the ACTUAL finalize. `watcher_drove_finalize` is true whenever the
            // helper ran the finalizer (here always, via `normal_completion = true`),
            // so kickoff-suppression and the terminal-stop path below account for it.
            // The legacy `mailbox_finalize_owed`-derived `delegated_finalize_owed`
            // term is dropped: the only false path (stale-newer-turn skip) has a newer
            // live turn, so `has_active_turn` already suppresses kickoff — identical.
            let watcher_handled_mailbox_finish =
                watcher_drove_finalize || finish_mailbox_on_completion;
            let should_kickoff_queue = if watcher_handled_mailbox_finish
                || monitor_auto_turn_finished
                || has_active_turn
            {
                false
            } else {
                mailbox
                    .has_pending_soft_queue(crate::services::discord::queue_persistence_context(
                        &shared,
                        &provider_kind,
                        channel_id,
                    ))
                    .await
                    .has_pending
            };
            if dispatch_ok && should_kickoff_queue {
                crate::services::discord::schedule_deferred_idle_queue_kickoff(
                    shared.clone(),
                    provider_kind.clone(),
                    channel_id,
                    "watcher completed with queued backlog",
                );
            }
            if is_terminal_finalize_stop_candidate(
                terminal_output_committed,
                dispatch_ok,
                watcher_handled_mailbox_finish,
            ) {
                let tmux_alive = probe_tmux_session_liveness(&tmux_session_name).await;
                let confirmed_end = relay_coord.confirmed_end_offset.load(Ordering::Acquire);
                let tmux_tail_offset = std::fs::metadata(&output_path)
                    .map(|meta| meta.len())
                    .unwrap_or(current_offset);
                match watcher_stop_decision_after_terminal_finalize(
                    terminal_output_committed,
                    dispatch_ok,
                    watcher_handled_mailbox_finish,
                    tmux_alive,
                    confirmed_end,
                    tmux_tail_offset,
                    None,
                ) {
                    WatcherStopDecision::Stop => {
                        turn_delivered.store(true, Ordering::Release);
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 👁 watcher: terminal turn finalized; stopping watcher for {} after tmux exit",
                            tmux_session_name
                        );
                        finish_monitor_auto_turn_if_claimed(
                            &shared,
                            &watcher_provider,
                            channel_id,
                            &mut monitor_auto_turn_claimed,
                            &mut monitor_auto_turn_finished,
                            &mut monitor_auto_turn_synthetic_msg_id,
                            &mut monitor_auto_turn_ledger_generation,
                        )
                        .await;
                        break 'watcher_loop;
                    }
                    WatcherStopDecision::Continue
                    | WatcherStopDecision::PostTerminalSuccessContinuation => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 👁 watcher: terminal turn finalized but tmux is still alive for {}; watcher staying attached",
                            tmux_session_name
                        );
                    }
                }
            }
        } else if !relay_suppressed {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ watcher: relay failed — preserving inflight for retry");
        }

        let inflight_missing_for_fallback = missing_inflight_after_session_bound_delivery(
            inflight_state.is_none(),
            session_bound_relay_owns_terminal_delivery,
        );
        let tmux_alive_for_missing_inflight =
            if inflight_missing_for_fallback && resolved_did.is_none() && terminal_output_committed
            {
                probe_tmux_session_liveness(&tmux_session_name).await
            } else {
                true
            };
        let recent_turn_stop =
            recent_turn_stop_for_watcher_range(channel_id, &tmux_session_name, data_start_offset);
        let placeholder_cleanup_committed = placeholder_msg_id.is_some_and(|msg_id| {
            shared.ui.placeholder_cleanup.terminal_cleanup_committed(
                &provider_kind,
                channel_id,
                msg_id,
            )
        });
        let missing_inflight_plan = missing_inflight_fallback_observation(
            inflight_missing_for_fallback,
            resolved_did.is_some(),
            terminal_output_committed,
            recent_turn_stop.is_some(),
            tmux_alive_for_missing_inflight,
        );
        if missing_inflight_plan.suppressed_by_recent_stop {
            if placeholder_cleanup_committed {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ watcher: missing-inflight observation suppressed for channel {} — terminal placeholder cleanup already committed",
                    channel_id.get()
                );
            } else if let Some(stop) = recent_turn_stop {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ↻ watcher: missing-inflight observation suppressed for channel {} — recent turn stop still active ({})",
                    channel_id.get(),
                    stop.reason
                );
            }
        } else if !tmux_alive_for_missing_inflight {
            let _drained_offset = drain_missing_inflight_dead_tmux_tail_to_eof(
                &shared,
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                &output_path,
                current_offset,
            )
            .await;
            handle_tmux_watcher_observed_death(
                channel_id,
                &http,
                &shared,
                &tmux_session_name,
                &output_path,
                &watcher_provider,
                prompt_too_long_killed,
                watcher_lifecycle_terminal_delivery_observed(
                    terminal_delivery_observed,
                    turn_delivered.load(Ordering::Acquire),
                ),
            )
            .await;
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
                &mut monitor_auto_turn_synthetic_msg_id,
                &mut monitor_auto_turn_ledger_generation,
            )
            .await;
            break 'watcher_loop;
        } else if missing_inflight_plan.mark_degraded {
            crate::services::observability::metrics::record_watcher_db_fallback_resolve_failed(
                channel_id.get(),
                provider_kind.as_str(),
            );
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: missing inflight with unresolved dispatch for channel {} while tmux is still alive; keeping watcher attached without synthetic inflight (tmux={})",
                channel_id.get(),
                tmux_session_name
            );
        }

        // Update session tokens from result event and auto-compact if threshold exceeded
        if let Some(tokens) = result_usage.map(|usage| usage.context_occupancy_input_tokens()) {
            let provider = shared.settings.read().await.provider.clone();
            let session_key = crate::services::discord::adk_session::build_adk_session_key(
                &shared, channel_id, &provider,
            )
            .await;
            let channel_name = {
                let data = shared.core.lock().await;
                data.sessions
                    .get(&channel_id)
                    .and_then(|s| s.channel_name.clone())
            };
            let thread_channel_id = channel_name
                .as_deref()
                .and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name);
            let agent_id = resolve_role_binding(channel_id, channel_name.as_deref())
                .map(|binding| binding.role_id);
            crate::services::discord::adk_session::post_adk_session_status(
                session_key.as_deref(),
                channel_name.as_deref(),
                None,
                watcher_terminal_token_update_status(watcher_direct_terminal_idle_committed),
                &provider,
                None,
                Some(tokens),
                None,
                None,
                thread_channel_id,
                Some(channel_id),
                agent_id.as_deref(),
                shared.api_port,
            )
            .await;

            let ctx_cfg =
                crate::services::discord::adk_session::fetch_context_thresholds(shared.api_port)
                    .await;
            let pct = (tokens * 100) / ctx_cfg.context_window.max(1);
            // #227: Re-enabled with 5-min cooldown (matches turn_bridge path).
            // Without cooldown, the compact turn's own result could re-trigger compact.
            let cooldown_key = format!("auto_compact_cooldown:{}", channel_id.get());
            let cooldown_value =
                match crate::services::discord::internal_api::get_kv_value(&cooldown_key) {
                    Ok(value) => value,
                    Err(_) => {
                        if let Some(pg_pool) = shared.pg_pool.as_ref() {
                            sqlx::query_scalar::<_, Option<String>>(
                                "SELECT value
                             FROM kv_meta
                             WHERE key = $1
                               AND (expires_at IS NULL OR expires_at > NOW())
                             LIMIT 1",
                            )
                            .bind(&cooldown_key)
                            .fetch_optional(pg_pool)
                            .await
                            .ok()
                            .flatten()
                            .flatten()
                        } else {
                            None
                        }
                    }
                };
            let compact_cooldown_ok =
                cooldown_value
                    .and_then(|v| v.parse::<i64>().ok())
                    .map_or(true, |ts| {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs() as i64;
                        now - ts > 300 // 5 min cooldown
                    });
            // DISABLED — token counting still unreliable
            if false && pct >= ctx_cfg.compact_pct && !is_prompt_too_long && compact_cooldown_ok {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚡ [watcher] Auto-compact: {} at {pct}% ({tokens} tokens)",
                    tmux_session_name
                );
                let name = tmux_session_name.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    crate::services::platform::tmux::send_keys(&name, &["/compact", "Enter"])
                })
                .await;
                // Set cooldown timestamp
                let cooldown_key = format!("auto_compact_cooldown:{}", channel_id.get());
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let now_text = now.to_string();
                if crate::services::discord::internal_api::set_kv_value(&cooldown_key, &now_text)
                    .is_err()
                {
                    if let Some(pg_pool) = shared.pg_pool.as_ref() {
                        let _ = sqlx::query(
                            "INSERT INTO kv_meta (key, value, expires_at)
                             VALUES ($1, $2, NULL)
                             ON CONFLICT (key) DO UPDATE
                             SET value = EXCLUDED.value,
                                 expires_at = EXCLUDED.expires_at",
                        )
                        .bind(&cooldown_key)
                        .bind(&now_text)
                        .execute(pg_pool)
                        .await;
                    }
                }
                // Notify: auto-compact triggered
                let target = format!("channel:{}", channel_id.get());
                let content = format!("🗜️ 자동 컨텍스트 압축 (사용률: {pct}%)");
                let _ = enqueue_outbox_best_effort(
                    shared.pg_pool.as_ref(),
                    sqlite_runtime_db(shared.as_ref()),
                    OutboxMessage {
                        target: target.as_str(),
                        content: content.as_str(),
                        bot: "notify",
                        source: "system",
                        reason_code: None,
                        session_key: None,
                    },
                )
                .await;
            }
        }
    }

    // Cleanup: only remove from DashMap if we weren't cancelled/replaced.
    // #243: When a watcher is cancelled (replaced by a new watcher or shutdown),
    // the replacement already occupies the slot — removing would delete the new entry.
    if !cancel.load(Ordering::Relaxed) {
        shared.tmux_watchers.remove(&channel_id);
    }

    let api_port = shared.api_port;
    let provider = shared.settings.read().await.provider.clone();
    let session_key = crate::services::discord::adk_session::build_adk_session_key(
        &shared, channel_id, &provider,
    )
    .await;
    let channel_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.channel_name.clone())
    };
    let dispatch_protection =
        crate::services::discord::tmux_lifecycle::resolve_dispatch_tmux_protection(
            None::<&crate::db::Db>,
            shared.pg_pool.as_ref(),
            &shared.token_hash,
            &provider,
            &tmux_session_name,
            channel_name.as_deref(),
        );
    let dispatch_failed_for_dead_session = if let Some(protection) = dispatch_protection.as_ref() {
        crate::services::discord::tmux_lifecycle::fail_active_dispatch_for_dead_tmux_session(
            api_port,
            protection,
            &tmux_session_name,
            "tmux_watcher",
        )
        .await
    } else {
        false
    };
    let cleanup_plan = dead_session_cleanup_plan(
        dispatch_protection.is_some() && !dispatch_failed_for_dead_session,
    );

    if let Some(protection) = dispatch_protection {
        let ts = chrono::Local::now().format("%H:%M:%S");
        if dispatch_failed_for_dead_session {
            tracing::warn!(
                "  [{ts}] tmux watcher: failed active dispatch for dead session {} — {}",
                tmux_session_name,
                protection.log_reason()
            );
        } else {
            tracing::info!(
                "  [{ts}] ♻ tmux watcher: preserving dispatch session {} — {}",
                tmux_session_name,
                protection.log_reason()
            );
        }
    }

    if !cleanup_plan.preserve_tmux_session {
        // #2427 A wire: pane-death explicit inflight cleanup. The
        // tmux pane is gone (or about to be killed below), so any
        // inflight row still pointing at this provider/channel will
        // never receive a normal completion hook. Without this the
        // sweeper has to time-guess (`STALL`/`ABANDON`) before evicting,
        // reproducing the #2415 family of "completion-missing → time
        // heuristic" bugs.
        //
        // We re-check `tmux_session_has_live_pane` on the blocking
        // thread before clearing, matching the same revalidation the
        // kill path uses (#1261 codex P2) so a concurrent
        // `start_claude` respawn of a fresh same-named session does not
        // get its inflight wiped.
        {
            let sess_for_inflight = tmux_session_name.clone();
            let provider_for_inflight = provider.clone();
            let channel_id_inflight = channel_id;
            let watcher_identity_for_inflight = watcher_turn_identity.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let pane_alive = tmux_session_has_live_pane(&sess_for_inflight);
                if pane_alive {
                    // Pane resurrected (e.g. start_claude respawn race) —
                    // do not touch its inflight.
                    return;
                }
                emit_explicit_inflight_cleanup_signal_pane_dead(
                    &provider_for_inflight,
                    channel_id_inflight,
                    &sess_for_inflight,
                    watcher_identity_for_inflight.as_ref(),
                );
            })
            .await;
        }

        // Kill dead tmux session to prevent accumulation (especially for thread sessions
        // which are created per-dispatch and would otherwise linger for 24h).
        // #145: skip kill for unified-thread sessions with active auto-queue runs.
        {
            let sess = tmux_session_name.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if tmux_session_exists(&sess) && !tmux_session_has_live_pane(&sess) {
                    // Check if this is a unified-thread session before killing
                    if let Some((_, ch_name)) =
                        crate::services::provider::parse_provider_and_channel_from_tmux_name(&sess)
                    {
                        if crate::dispatch::is_unified_thread_channel_name_active(&ch_name) {
                            return;
                        }
                    }
                    crate::services::termination_audit::record_termination_for_tmux(
                        &sess,
                        None,
                        "tmux_watcher",
                        "dead_after_turn",
                        Some("watcher cleanup: dead session after turn"),
                        None,
                    );
                    record_tmux_exit_reason(&sess, "watcher cleanup: dead session after turn");

                    // #1261 (Fix B): the wrapper's stderr `[stderr] ...` lines and
                    // synthetic `[fatal startup error]` markers go to the PTY, not
                    // to the structured jsonl that `recent_output_tail` reads. Dump
                    // the current pane buffer to a `death_pane_log` file BEFORE we
                    // kill the session so the wrapper-level death context is still
                    // recoverable post-mortem. Kept out of `cleanup_session_temp_files`
                    // EXTS on purpose — the file persists past the cleanup and is
                    // overwritten on the next death of the same session.
                    if let Some(pane_content) =
                        crate::services::platform::tmux::capture_pane(&sess, -1000)
                    {
                        let stamped = format!(
                            "[{}] post-mortem capture for session={}\n{}",
                            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                            sess,
                            pane_content
                        );
                        let path = crate::services::tmux_common::session_temp_path(
                            &sess,
                            "death_pane_log",
                        );
                        if let Some(parent) = std::path::Path::new(&path).parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = std::fs::write(&path, stamped);
                    }

                    // #1261 (codex P2): the `capture_pane` subprocess above
                    // widens the gap between the outer dead-pane gate and the
                    // kill. In that window a concurrent follow-up could run
                    // claude.rs::start_claude, which kills the stale session
                    // (line 1294), respawns a fresh live session with the
                    // same name (line 1379), and we'd then kill the brand-new
                    // session here. Revalidate the dead-pane condition right
                    // before the kill so we only tear down the same
                    // dead-paned session we capture-paned.
                    if tmux_session_exists(&sess) && !tmux_session_has_live_pane(&sess) {
                        crate::services::platform::tmux::kill_session(
                            &sess,
                            "watcher cleanup: dead session after turn",
                        );
                    }
                    // NOTE: jsonl/FIFO/etc. cleanup intentionally NOT done here.
                    // `claude.rs::start_claude` calls
                    // `cleanup_session_temp_files` at spawn time
                    // (`claude.rs:1304`) before recreating the canonical paths,
                    // which already covers the "next-spawn against stale jsonl"
                    // case. Pairing a watcher-side cleanup with the kill races
                    // with that spawn-side cleanup + recreate (#1261 codex P1):
                    // if the next message lands between our `kill_session` and
                    // our cleanup, claude's spawn already laid down fresh files
                    // and our cleanup deletes them, breaking the new turn.
                    // Keep cleanup as a single-source-of-truth on the spawn
                    // path.
                }
            })
            .await;
        }
    }

    let defer_idle_status_to_bridge =
        crate::services::discord::inflight::load_inflight_state(&provider, channel_id.get())
            .as_ref()
            .is_some_and(|state| {
                state.tmux_session_name.as_deref() == Some(tmux_session_name.as_str())
            });

    if cleanup_plan.report_idle_status && !defer_idle_status_to_bridge {
        // Report idle status to DB so the dashboard doesn't show stale "working" state.
        // Always report idle when the watcher exits, even if dispatch protection
        // keeps the dead tmux session around for the active-dispatch safety path.
        let thread_channel_id = channel_name
            .as_deref()
            .and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name);
        let agent_id = resolve_role_binding(channel_id, channel_name.as_deref())
            .map(|binding| binding.role_id);
        crate::services::discord::adk_session::post_adk_session_status(
            session_key.as_deref(),
            channel_name.as_deref(),
            None, // model
            "idle",
            &provider,
            None, // session_info
            None, // tokens
            None, // cwd
            None, // dispatch_id
            thread_channel_id,
            Some(channel_id),
            agent_id.as_deref(),
            api_port,
        )
        .await;
    } else if cleanup_plan.report_idle_status {
        tracing::debug!(
            provider = %provider.as_str(),
            channel = channel_id.get(),
            tmux_session = %tmux_session_name,
            "watcher deferred idle status because bridge-owned inflight still needs terminal Discord finalization"
        );
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 👁 tmux watcher stopped for #{tmux_session_name} (instance {watcher_instance_id})"
    );
}

#[cfg(test)]
#[path = "tmux_watcher/tests.rs"]
mod tests;
