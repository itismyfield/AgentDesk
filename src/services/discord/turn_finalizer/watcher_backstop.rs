//! #3479 r9 — watcher far-backstop liveness re-check split out of
//! `turn_finalizer.rs`.
//!
//! PURE MOVE (no logic change): the proven-terminal fast-path tunables
//! (`WATCHER_BACKSTOP_TERMINAL_PROBE_INTERVAL` / `WATCHER_BACKSTOP_TERMINAL_STREAK`)
//! and the reconciler's terminal-or-defer verdict pair
//! (`watcher_backstop_turn_is_terminal` / `watcher_backstop_signal_is_terminal`),
//! plus the pure signal-truth-table unit test. The parent re-imports the
//! consts + fns (`use self::watcher_backstop::{...}`) so the `reconcile` loop
//! call sites stay byte-identical.

use super::*;

/// #3277 (Defect C) — proven-terminal FAST path for the watcher far-backstop.
/// In the #3277 incident the handed-off turn was already PROVABLY complete
/// (JSONL terminator on disk) while its watcher owner sat parked at transcript
/// EOF, so no data-driven finalize ever fired and the channel stayed stranded
/// for the full 1800s. The reconciler therefore PROBES watcher-owned Pending
/// entries with the STRICT (`at_deadline = false`) form of
/// `watcher_backstop_turn_is_terminal`: after
/// `WATCHER_BACKSTOP_TERMINAL_STREAK` terminal probes this interval apart, the
/// far deadline is pulled in to `GATE_BACKSTOP` for a third (still strict)
/// confirmation before finalizing. A single non-terminal probe resets the
/// streak (paused / paused-live / flapping turns keep the generous horizon).
pub(super) const WATCHER_BACKSTOP_TERMINAL_PROBE_INTERVAL: Duration = Duration::from_secs(15);

/// Consecutive terminal probes required before the fast path pulls the
/// watcher far-backstop deadline in (see above).
pub(super) const WATCHER_BACKSTOP_TERMINAL_STREAK: u8 = 2;

/// #3016 phase-5a — the reconciler's terminal-or-defer verdict for a
/// watcher-owned `register_start` Pending. `at_deadline == true` is the
/// NATURAL 1800s far-backstop expiry; `false` (the #3277 fast-path probe AND
/// the re-check of a fast-path-PULLED deadline, codex r1) stays STRICTLY
/// transcript-proven. Never finalizes a legitimately long paused-live turn:
///   * NO LIVE handle — absent (also under the inflight `tmux_session_name`
///     re-key below: #3277 verify-1, a `claim_or_reuse_watcher` ReuseExisting
///     dispatch registers under the OWNER channel only), `cancel` set, or
///     `heartbeat_stale()` (#3268) → terminal ONLY at the natural deadline
///     (nothing is left to drive the pane). The strict mode DEFERS: a watcher
///     replace/reuse leaves the registry transiently absent/stale while the
///     transcript still says busy — absence proves nothing about the TURN;
///     dead/absent authority stays with the far horizon, never the fast path.
///   * live-but-`paused` (a Discord turn took the session over) → defer.
///   * else `watcher_backstop_signal_is_terminal` on the transcript: `Done`
///     terminal only once the relay-space produced frontier is delivery-confirmed
///     (or the natural far-backstop escape fires); `PausedLive` defers; `Unknown`
///     (non-JSONL runtime) consults the pane-ready fallback ONLY at the natural
///     deadline.
pub(super) fn watcher_backstop_turn_is_terminal(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    provider: &ProviderKind,
    at_deadline: bool,
) -> bool {
    let inflight_state =
        crate::services::discord::inflight::load_inflight_state(provider, channel_id.get());
    let inflight_tmux = inflight_state
        .as_ref()
        .and_then(|state| state.tmux_session_name.as_deref());
    let (tmux_session_name, output_path, paused) = {
        let handle = match inflight_tmux {
            Some(tmux) => shared.tmux_watchers.by_tmux_session.get(tmux),
            None => shared.tmux_watchers.get(&channel_id),
        };
        let Some(handle) = handle else {
            return at_deadline;
        };
        if handle.cancel.load(std::sync::atomic::Ordering::Relaxed) || handle.heartbeat_stale() {
            return at_deadline;
        }
        (
            handle.tmux_session_name.clone(),
            handle.output_path.clone(),
            handle.paused.load(std::sync::atomic::Ordering::Acquire),
        )
        // dashmap `Ref` dropped here, BEFORE the (blocking) pane capture below.
    };
    if paused {
        return false;
    }
    let runtime_binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&tmux_session_name);
    let runtime_kind = runtime_binding
        .as_ref()
        .map(|binding| binding.runtime_kind)
        .or_else(|| {
            crate::services::tmux_common::resolve_tmux_runtime_kind_marker(&tmux_session_name)
        });
    let signal = completion_signal_from_transcript(
        provider,
        runtime_kind,
        std::path::Path::new(&output_path),
    );
    let confirmed_end_publication = Some(
        shared
            .tmux_relay_coord(channel_id)
            .confirmed_end_offset
            .load(std::sync::atomic::Ordering::Acquire),
    );
    let produced_terminal_end = watcher_backstop_produced_terminal_end(
        shared,
        channel_id,
        inflight_state.as_ref(),
        &tmux_session_name,
        &output_path,
    );
    // Both publication identity and a produced terminal end are required.
    // Missing either fact is unconfirmed; never manufacture authority by
    // comparing a frontier to itself.
    let delivery_confirmed =
        delivery_confirmed_for_produced_end(confirmed_end_publication, produced_terminal_end);
    // Bounded escape hatch: fast-path probes and pulled deadlines stay strict,
    // but the natural far-backstop deadline may finalize a structurally Done
    // turn even if the relay watermark never reaches the produced frontier. This
    // bounds a dead relay to one full WATCHER_REGISTER_BACKSTOP horizon while
    // still preventing the seconds-long fast path from clearing an undelivered
    // produced tail.
    let delivery_confirmed_or_natural_deadline_escape = delivery_confirmed || at_deadline;
    if matches!(signal, CompletionSignal::Done) && !delivery_confirmed {
        tracing::warn!(
            channel_id = channel_id.get(),
            provider = %provider.as_str(),
            tmux_session = %tmux_session_name,
            ?confirmed_end_publication,
            ?produced_terminal_end,
            at_deadline,
            natural_deadline_escape = delivery_confirmed_or_natural_deadline_escape,
            "watcher backstop observed Done before delivery confirmation"
        );
    }
    watcher_backstop_signal_is_terminal(
        signal,
        at_deadline,
        delivery_confirmed_or_natural_deadline_escape,
        || {
            crate::services::provider::tmux_session_fallback_ready_for_input(
                &tmux_session_name,
                provider,
                runtime_kind,
            )
            .is_some_and(crate::services::pane_readiness::FallbackPaneReadiness::is_ready)
        },
    )
}

fn delivery_confirmed_for_produced_end(
    confirmed_end_publication: Option<u64>,
    produced_terminal_end: Option<u64>,
) -> bool {
    confirmed_end_publication
        .zip(produced_terminal_end)
        .is_some_and(|(confirmed, produced)| confirmed >= produced)
}

fn watcher_backstop_produced_terminal_end(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    inflight_state: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
    output_path: &str,
) -> Option<u64> {
    let mut end = inflight_state
        .filter(|state| {
            state.tmux_session_name.as_deref() == Some(tmux_session_name)
                && state.output_path.as_deref() == Some(output_path)
        })
        .map(|state| state.last_offset)
        // `0` is the inflight sentinel for "no relay output frontier observed",
        // not a confirmed zero-byte production record. Treating it as a produced
        // end would let the initial confirmed watermark (`0 >= 0`) fabricate
        // strict-finalization authority for an empty/silent Done turn.
        .filter(|offset| *offset > 0);

    if let Some(state) = inflight_state {
        let expected_key = DeliveryLeaseKey::from_inflight_state_for_site(
            channel_id,
            shared.restart.current_generation,
            state,
            "watcher_backstop_produced_terminal_end",
        );
        // #5071 relay-tail S4: the `Leased | Committed` + `key == expected_key`
        // discrimination this used to spell inline is now
        // `LeaseSnapshot::identity_matched`, shared verbatim with the
        // `TerminalDeliveryFence` conjunct in `tmux_watcher_registry`. This
        // caller wants the produced END from EITHER state and ignores the
        // deadline; the fence wants the deadline and only from `Leased`.
        let lease_end = shared
            .delivery_lease(channel_id)
            .read()
            .identity_matched(&expected_key)
            .map(|matched| matched.end);
        if let Some(lease_end) = lease_end {
            end = Some(end.unwrap_or(0).max(lease_end));
        }
    }

    // REVIEW-ME (#4174): there is no durable "produced terminal end" field on
    // the finalizer ledger. The defensible relay-space sources in this caller
    // are the matching inflight row's `last_offset` (the same offset family used
    // as delivery-lease target/end by bridge handoff paths) and any currently
    // identity-matched delivery-lease range. We intentionally do NOT fall back
    // to `output_path` metadata: for CodexTui the watcher path can be the
    // provider rollout transcript, not the relay cursor, and empty/silent turns
    // would over-defer because transcript terminator bytes are not delivered
    // output. If neither source is present, the caller keeps delivery unconfirmed;
    // an unknown produced end cannot confer destructive finalization authority.
    end
}

/// #3277 verify-3 — the verdict over the transcript completion signal. The
/// strict mode (`allow_pane_probe == false`: fast-path probe and pulled
/// re-check) treats `Unknown` (non-JSONL Gemini / OpenCode / Qwen / legacy
/// wrapper: no provable terminator) as NON-terminal: the synchronous
/// pane-capture fallback can misread a dialog or a long silent stretch as
/// idle, and probing it every 15s would amplify the old once-per-1800s
/// exposure ~120× (and block the actor task). Only the NATURAL at-deadline
/// re-check (`true`) consults `pane_ready` — lazily, only on `Unknown`.
pub(super) fn watcher_backstop_signal_is_terminal(
    signal: CompletionSignal,
    allow_pane_probe: bool,
    delivery_confirmed: bool,
    pane_ready: impl FnOnce() -> bool,
) -> bool {
    match signal {
        CompletionSignal::PausedLive => false,
        CompletionSignal::Done => delivery_confirmed,
        CompletionSignal::Unknown => allow_pane_probe && pane_ready(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #3277 verify-3 (MINOR) truth table: the fast-path probe
    /// (`allow_pane_probe == false`) must NEVER report `Unknown` (non-JSONL
    /// runtime) as terminal — and must not even RUN the pane capture — while
    /// the at-deadline re-check keeps the pane-ready fallback. `Done` /
    /// `PausedLive` verdicts are identical in both modes.
    #[tokio::test(flavor = "current_thread")]
    async fn done_without_inflight_or_lease_defers_strict_finalize() {
        super::super::tests::with_isolated_runtime_root(|| async move {
            let shared = Arc::new(crate::services::discord::make_shared_data_for_tests());
            let entropy = chrono::Utc::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
                .unsigned_abs();
            let channel = ChannelId::new(50_220_023u64.saturating_add(entropy % 1_000_000));
            crate::services::discord::inflight::clear_inflight_state(
                &ProviderKind::Claude,
                channel.get(),
            );
            shared.tmux_watchers.remove(&channel);
            shared.tmux_relay_coords.remove(&channel);
            shared.tmux_relay_coord(channel); /* create a fresh coordinate */
            shared.dispatch.thread_parents.remove(&channel);
            shared.restart.recovering_channels.remove(&channel);
            shared.turn_start_times.remove(&channel); /* isolate stale process state */
            shared.ui.placeholder_live_events.clear_channel(channel); /* isolate stale process state */
            let channel = ChannelId::new(channel.get()); /* retain isolated identity */

            let session = format!("backstop-no-inflight-{}", std::process::id());
            let transcript = std::env::temp_dir().join(format!("{session}.jsonl"));
            std::fs::write(
                &transcript,
                "{\"type\":\"result\",\"result\":\"done\",\"session_id\":\"s\"}\n",
            )
            .unwrap();
            shared.tmux_watchers.insert(
                channel,
                crate::services::discord::TmuxWatcherHandle {
                    tmux_session_name: session.clone(),
                    output_path: transcript.to_str().unwrap().to_string(),
                    paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    resume_offset: Arc::new(std::sync::Mutex::new(None)),
                    cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                        crate::services::discord::tmux_watcher_now_ms(),
                    )),
                },
            );

            assert!(!watcher_backstop_turn_is_terminal(
                &shared,
                channel,
                &ProviderKind::Claude,
                false,
            ));
            let _ = std::fs::remove_file(transcript);
        })
        .await;
    }

    #[test]
    fn missing_delivery_proof_never_confirms_terminal_end() {
        assert!(!delivery_confirmed_for_produced_end(Some(0), None));
        assert!(!delivery_confirmed_for_produced_end(None, Some(64)));
        assert!(delivery_confirmed_for_produced_end(Some(64), Some(64)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn done_with_zero_inflight_offset_defers_strict_finalize() {
        super::super::tests::with_isolated_runtime_root(|| async move {
            let shared = Arc::new(crate::services::discord::make_shared_data_for_tests());
            let entropy = chrono::Utc::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
                .unsigned_abs();
            let channel = ChannelId::new(50_220_024u64.saturating_add(entropy % 1_000_000));
            let session = format!("backstop-zero-offset-{}", std::process::id());
            let transcript = std::env::temp_dir().join(format!("{session}.jsonl"));
            std::fs::write(
                &transcript,
                "{\"type\":\"result\",\"result\":\"done\",\"session_id\":\"s\"}\n",
            )
            .unwrap();
            let transcript_str = transcript.to_str().unwrap().to_string();
            shared.tmux_watchers.insert(
                channel,
                crate::services::discord::TmuxWatcherHandle {
                    tmux_session_name: session.clone(),
                    output_path: transcript_str.clone(),
                    paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    resume_offset: Arc::new(std::sync::Mutex::new(None)),
                    cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                        crate::services::discord::tmux_watcher_now_ms(),
                    )),
                },
            );
            let mut state = crate::services::discord::inflight::InflightTurnState::new(
                ProviderKind::Claude,
                channel.get(),
                None,
                7,
                210,
                211,
                "done with zero sentinel offset".to_string(),
                None,
                Some(session),
                Some(transcript_str),
                None,
                0,
            );
            state.turn_start_offset = Some(0);
            crate::services::discord::inflight::save_inflight_state(&state).unwrap();
            shared
                .tmux_relay_coord(channel)
                .confirmed_end_offset
                .store(0, std::sync::atomic::Ordering::Release);

            assert!(
                !watcher_backstop_turn_is_terminal(&shared, channel, &ProviderKind::Claude, false,),
                "zero is an unknown produced frontier sentinel, not delivery proof"
            );
            assert!(
                watcher_backstop_turn_is_terminal(&shared, channel, &ProviderKind::Claude, true,),
                "the natural far-backstop remains the bounded escape"
            );
            let _ = std::fs::remove_file(transcript);
        })
        .await;
    }

    #[test]
    fn non_jsonl_signal_never_terminal_on_fast_path_probe() {
        use std::cell::Cell;
        // Unknown + fast path: non-terminal AND the pane capture must not run.
        let captured = Cell::new(false);
        assert!(!watcher_backstop_signal_is_terminal(
            CompletionSignal::Unknown,
            false,
            false,
            || {
                captured.set(true);
                true
            }
        ));
        assert!(
            !captured.get(),
            "the 15s fast-path probe must never run a blocking pane capture"
        );
        // Unknown + at-deadline: pane fallback decides (both directions).
        assert!(watcher_backstop_signal_is_terminal(
            CompletionSignal::Unknown,
            true,
            false,
            || true
        ));
        assert!(!watcher_backstop_signal_is_terminal(
            CompletionSignal::Unknown,
            true,
            false,
            || false
        ));
        // Done: terminal only after delivery confirmation, in both probe modes.
        for probe in [false, true] {
            assert!(watcher_backstop_signal_is_terminal(
                CompletionSignal::Done,
                probe,
                true,
                || unreachable!("Done must not consult the pane")
            ));
            assert!(!watcher_backstop_signal_is_terminal(
                CompletionSignal::Done,
                probe,
                false,
                || unreachable!("Done must not consult the pane")
            ));
            assert!(!watcher_backstop_signal_is_terminal(
                CompletionSignal::PausedLive,
                probe,
                true,
                || unreachable!("PausedLive must not consult the pane")
            ));
        }
    }
}
