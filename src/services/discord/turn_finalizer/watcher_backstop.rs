//! #3479 r9 — watcher far-backstop liveness re-check split out of
//! `turn_finalizer.rs`.
//!
//! PURE MOVE (no logic change): the proven-terminal fast-path tunables
//! (`WATCHER_BACKSTOP_TERMINAL_PROBE_INTERVAL` / `WATCHER_BACKSTOP_TERMINAL_STREAK`)
//! and the reconciler's terminal-or-defer verdict pair
//! (`watcher_backstop_turn_is_terminal` / `watcher_backstop_signal_is_terminal`),
//! plus the pure signal-truth-table unit test. The parent re-imports the
//! consts + fns (`use self::watcher_backstop::{...}`) for the `reconcile` loop,
//! which passes each ledger entry's key.

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
///     (or the natural far-backstop escape fires, or O owns the channel's TUI body);
///     `PausedLive` defers; `Unknown`
///     (non-JSONL runtime) consults the pane-ready fallback ONLY at the natural
///     deadline.
pub(super) fn watcher_backstop_turn_is_terminal(
    shared: &Arc<SharedData>,
    turn: impl Into<BackstopTurn>,
    provider: &ProviderKind,
    at_deadline: bool,
) -> bool {
    let BackstopTurn { channel_id, ledger } = turn.into();
    let inflight_state =
        crate::services::discord::inflight::load_inflight_state(provider, channel_id.get());
    let inflight_tmux = inflight_state
        .as_ref()
        .and_then(|state| state.tmux_session_name.as_deref());
    // A Herdr turn ends only on its own transcript's terminal: without a live watcher that
    // transcript is read instead of conceding at the deadline, and no pane probe runs.
    let herdr_hold = herdr_hold_source(inflight_state.as_ref());
    let (tmux_session_name, output_path, paused) = 'source: {
        let handle = match inflight_tmux {
            Some(tmux) => shared.tmux_watchers.by_tmux_session.get(tmux),
            None => shared.tmux_watchers.get(&channel_id),
        };
        let Some(handle) = handle else {
            match herdr_hold.clone() {
                Some(source) => break 'source source,
                None => return at_deadline,
            }
        };
        if handle.cancel.load(std::sync::atomic::Ordering::Relaxed) || handle.heartbeat_stale() {
            match herdr_hold.clone() {
                Some(source) => break 'source source,
                None => return at_deadline,
            }
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
    let herdr = herdr_hold.is_some();
    #[cfg(test)]
    let herdr = policy_mutant(herdr);
    let transcript = std::path::Path::new(&output_path);
    let signal = match herdr {
        true => {
            // Only the held turn's own start, in the hold's transcript, can prove its abort.
            let held = held_turn(
                inflight_state.as_ref(),
                herdr_hold.as_ref(),
                &output_path,
                ledger.as_ref(),
            );
            super::completion_signal::herdr_completion_signal_from_transcript(
                provider,
                runtime_kind,
                transcript,
                held.as_deref(),
            )
        }
        false => completion_signal_from_transcript(provider, runtime_kind, transcript),
    };
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
    // O posts this channel's TUI body from the transcript itself, so a Done turn needs no
    // Legacy delivery confirmation; the live/paused guards above still apply. A held identity
    // keeps the Legacy confirmation requirement. Sending no body, it leaves an adoption pending.
    let o_owns_body = crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel(
        channel_id.get(),
        runtime_kind,
    ) == Ok(true);
    let delivery_confirmed_or_natural_deadline_escape =
        o_owns_body || delivery_confirmed || at_deadline;
    if matches!(signal, CompletionSignal::Done) && !delivery_confirmed && !o_owns_body {
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
        at_deadline && herdr_hold.is_none(),
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

/// The session and transcript a Herdr turn's backstop reads when no live watcher does. Settlement
/// is checked first, so nothing else is read while it is unavailable.
#[cfg(not(unix))]
fn herdr_hold_source(
    _: Option<&crate::services::discord::inflight::InflightTurnState>,
) -> Option<(String, String, bool)> {
    None
}

#[cfg(unix)]
fn herdr_hold_source(
    inflight_state: Option<&crate::services::discord::inflight::InflightTurnState>,
) -> Option<(String, String, bool)> {
    if !crate::services::provider::cancel_token_claude_interrupt::herdr_stop_settlement_available()
    {
        return None;
    }
    let state = inflight_state?;
    let name = state.tmux_session_name.clone()?;
    if !crate::services::discord::turn_bridge::herdr_marked(&name) {
        return None;
    }
    // No recorded transcript proves no end: the turn stays held.
    let path = state.output_path.clone().unwrap_or_default();
    Some((name, path, false))
}

/// The turn a backstop check is for: its channel and the finalize ledger's key for it.
pub(super) struct BackstopTurn {
    channel_id: ChannelId,
    ledger: Option<TurnKey>,
}

impl From<TurnKey> for BackstopTurn {
    fn from(key: TurnKey) -> Self {
        Self {
            channel_id: key.channel_id,
            ledger: Some(key),
        }
    }
}

/// A channel-only check names no ledger turn, so no Herdr abort can end it.
#[cfg(test)]
impl From<ChannelId> for BackstopTurn {
    fn from(channel_id: ChannelId) -> Self {
        Self {
            channel_id,
            ledger: None,
        }
    }
}

/// The live interrupt state of the turn a Herdr hold names, only while the watcher reads the
/// transcript the hold names and the ledger key is that row's own turn.
fn held_turn(
    inflight_state: Option<&crate::services::discord::inflight::InflightTurnState>,
    hold: Option<&(String, String, bool)>,
    output_path: &str,
    ledger: Option<&TurnKey>,
) -> Option<Arc<crate::services::provider::cancel_token_claude_interrupt::HerdrInterruptState>> {
    let state = inflight_state?;
    #[cfg(test)]
    let unheld;
    #[cfg(test)]
    let hold = match hold {
        None if backstop_mutant("backstop_off_changed") => {
            unheld = (
                state.tmux_session_name.clone()?,
                output_path.to_owned(),
                false,
            );
            Some(&unheld)
        }
        hold => hold,
    };
    let (logical, hold_path, _) = hold?;
    let nonce = state
        .turn_nonce
        .as_deref()
        .filter(|nonce| !nonce.is_empty())?;
    let same_transcript = hold_path == output_path;
    #[cfg(test)]
    let same_transcript = same_transcript || backstop_mutant("backstop_identity_skipped");
    // A key without an episode, or of another episode or turn, proves nothing of this row's turn.
    let own_ledger_turn = ledger.is_some_and(|key| {
        key.episode.is_some()
            && key.matches_episode_nonce(Some(nonce))
            && key.user_msg_id != 0
            && key.user_msg_id == state.finalizer_turn_id
    });
    #[cfg(test)]
    let own_ledger_turn = own_ledger_turn || backstop_mutant("backstop_ledger_key_skipped");
    if !(same_transcript && own_ledger_turn) {
        return None;
    }
    crate::services::provider::cancel_token_claude_interrupt::herdr_turn(logical, nonce)
}

/// Test-only effect mutations of the backstop's Herdr policy.
#[cfg(test)]
pub(super) fn backstop_mutant(name: &str) -> bool {
    crate::services::provider::cancel_token_claude_interrupt::herdr_interrupt_mutant(name)
}

#[cfg(test)]
fn policy_mutant(herdr: bool) -> bool {
    match herdr {
        true => !backstop_mutant("backstop_policy_ignored"),
        false => backstop_mutant("backstop_off_changed"),
    }
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

    /// With no live watcher a Herdr turn is terminal only on its transcript's terminal, at the
    /// natural deadline too; without settlement or a Herdr marker the deadline still concedes.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn herdr_turn_without_watcher_ends_only_on_its_transcript() {
        use crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE;
        super::super::tests::with_isolated_runtime_root(|| async move {
            let shared = Arc::new(crate::services::discord::make_shared_data_for_tests());
            let entropy = chrono::Utc::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
                .unsigned_abs();
            let channel = ChannelId::new(50_340_025u64.saturating_add(entropy % 1_000_000));
            let session = format!("AgentDesk-claude-backstop-herdr-{}", entropy % 1_000_000);
            let transcript = std::env::temp_dir().join(format!("{session}.jsonl"));
            let busy = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"go\"}}\n";
            let done = "{\"type\":\"result\",\"result\":\"done\",\"session_id\":\"s\"}\n";
            let path = transcript.to_str().unwrap().to_string();
            let state = crate::services::discord::inflight::InflightTurnState::new(
                ProviderKind::Claude,
                channel.get(),
                None,
                7,
                310,
                311,
                "herdr hold".to_string(),
                None,
                Some(session.clone()),
                Some(path),
                None,
                0,
            );
            crate::services::discord::inflight::save_inflight_state(&state).unwrap();
            let marker = crate::services::tmux_common::session_temp_path(&session, "host_kind");
            std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
            let terminal = |at_deadline| {
                watcher_backstop_turn_is_terminal(
                    &shared,
                    channel,
                    &ProviderKind::Claude,
                    at_deadline,
                )
            };

            std::fs::write(&transcript, busy).unwrap();
            assert!(
                terminal(true),
                "a tmux turn without a watcher concedes at the deadline"
            );
            std::fs::write(&marker, "herdr").unwrap();
            assert!(
                !terminal(true),
                "a running Herdr turn is not ended by the deadline"
            );
            assert!(!terminal(false));
            HERDR_SETTLEMENT_OVERRIDE.set(false);
            let unsettled = terminal(true);
            HERDR_SETTLEMENT_OVERRIDE.set(true);
            assert!(unsettled, "without settlement the deadline still concedes");
            std::fs::write(&transcript, done).unwrap();
            assert!(
                terminal(true),
                "the transcript's own terminal ends the Herdr turn"
            );

            let _ = std::fs::remove_file(&marker);
            let _ = std::fs::remove_file(&transcript);
        })
        .await;
    }

    fn codex(kind: &str, turn: &str) -> String {
        let record =
            serde_json::json!({"type": "event_msg", "payload": {"type": kind, "turn_id": turn}});
        format!("{record}\n")
    }

    /// `n` KiB of a running Codex turn's reasoning records.
    fn reasoning(n: usize) -> String {
        let text = "x".repeat(1000);
        let payload = serde_json::json!({"type": "agent_reasoning", "text": text});
        let record = serde_json::json!({"type": "event_msg", "payload": payload});
        format!("{record}\n").repeat(n)
    }

    /// A held Herdr Codex turn: its row names `held`, a live watcher reads `watched`, and its
    /// token's own input began at `offset` of `held`.
    #[cfg(unix)]
    struct HeldTurn {
        shared: Arc<SharedData>,
        channel: ChannelId,
        marker: String,
        /// The ledger key of the row's own turn.
        key: TurnKey,
        herdr: Arc<crate::services::provider::cancel_token_claude_interrupt::HerdrInterruptState>,
        token: Arc<crate::services::provider::CancelToken>,
    }

    #[cfg(unix)]
    impl HeldTurn {
        fn new(held: &std::path::Path, offset: u64, watched: &std::path::Path) -> Self {
            use crate::services::provider::cancel_token_claude_interrupt::HerdrTurnStart;
            use std::os::unix::fs::MetadataExt;
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let shared = crate::services::discord::make_shared_data_for_tests();
            let channel = ChannelId::new(50_340_027_000 + u64::from(std::process::id()) * 100 + n);
            let session = format!("AgentDesk-codex-backstop-held-{}-{n}", std::process::id());
            let token = Arc::new(crate::services::provider::CancelToken::new());
            let mut state = crate::services::discord::inflight::InflightTurnState::new(
                ProviderKind::Codex,
                channel.get(),
                None,
                7,
                310,
                311,
                "herdr hold".to_string(),
                None,
                Some(session.clone()),
                Some(held.display().to_string()),
                None,
                1,
            );
            state.turn_nonce = token.turn_nonce().map(str::to_owned);
            state.ensure_finalizer_turn_id();
            let key = TurnKey::new(channel, state.finalizer_turn_id, 0)
                .with_episode_nonce(token.turn_nonce());
            crate::services::discord::inflight::save_inflight_state(&state).unwrap();
            shared.tmux_watchers.insert(
                channel,
                crate::services::discord::TmuxWatcherHandle {
                    tmux_session_name: session.clone(),
                    output_path: watched.display().to_string(),
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
            let marker = crate::services::tmux_common::session_temp_path(&session, "host_kind");
            std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
            std::fs::write(&marker, "herdr").unwrap();
            let owner = crate::db::dispatched_sessions::hosted_execution::HostedOwner {
                provider: "codex".into(),
                discord_token_hash: shared.token_hash.clone(),
                channel_id: channel.to_string(),
                logical_key: session,
                owner_node: "node".into(),
                runtime_root: "/tmp".into(),
            };
            let herdr = token.prepare_herdr_interrupt(ProviderKind::Codex, &owner);
            let meta = std::fs::metadata(held).unwrap();
            assert!(herdr.record_turn_start(HerdrTurnStart {
                execution_nonce: "backstop".into(),
                source: held.to_owned(),
                file: Some((meta.dev(), meta.ino())),
                offset,
                submitted_at: None,
            }));
            Self {
                shared,
                channel,
                marker,
                key,
                herdr,
                token,
            }
        }

        fn terminal(&self) -> bool {
            watcher_backstop_turn_is_terminal(&self.shared, self.key, &ProviderKind::Codex, true)
        }

        /// Bytes the last backstop poll read of this turn.
        fn polled(&self) -> u64 {
            self.herdr.own_turn.lock().unwrap().polled
        }
    }

    #[cfg(unix)]
    impl Drop for HeldTurn {
        fn drop(&mut self) {
            self.shared.tmux_watchers.remove(&self.channel);
            let _ = std::fs::remove_file(&self.marker);
        }
    }

    /// Under settlement a held Herdr Codex turn ends only on its own abort read from its own start
    /// in the hold's transcript; outside Herdr the abort keeps the existing signal.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn a_held_herdr_turn_ends_only_on_its_own_abort() {
        use crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE;
        super::super::tests::with_isolated_runtime_root(|| async move {
            let dir = tempfile::tempdir().unwrap();
            let file = |name: &str, body: String| {
                let path = dir.path().join(name);
                std::fs::write(&path, body).unwrap();
                path
            };
            let earlier = codex("task_started", "a") + &codex("turn_aborted", "a");
            let own = codex("task_started", "b") + &codex("turn_aborted", "b");

            let mine = file("own.jsonl", own.clone());
            let held = HeldTurn::new(&mine, 0, &mine);
            let existing = completion_signal_from_transcript(&ProviderKind::Codex, None, &mine);
            assert_eq!(existing, CompletionSignal::PausedLive);
            assert!(held.terminal(), "its own abort from its own start ends it");
            HERDR_SETTLEMENT_OVERRIDE.set(false);
            let unsettled = held.terminal();
            HERDR_SETTLEMENT_OVERRIDE.set(true);
            assert!(!unsettled, "without settlement the existing signal holds");
            std::fs::remove_file(&held.marker).unwrap();
            assert!(!held.terminal(), "outside Herdr the abort is no terminal");

            let after = file("after.jsonl", earlier.clone());
            let end = std::fs::metadata(&after).unwrap().len();
            let next = HeldTurn::new(&after, end, &after);
            assert!(
                !next.terminal(),
                "an earlier turn's abort ends no later held turn"
            );

            let other = file("other.jsonl", earlier);
            let held_b = file("held.jsonl", codex("task_started", "b"));
            let elsewhere = HeldTurn::new(&held_b, 0, &other);
            assert!(
                !elsewhere.terminal(),
                "another transcript's abort ends nothing"
            );

            let unstarted = file("unstarted.jsonl", codex("turn_aborted", "b"));
            assert!(!HeldTurn::new(&unstarted, 0, &unstarted).terminal());
            let foreign = file(
                "foreign.jsonl",
                codex("task_started", "b") + &codex("turn_aborted", "z"),
            );
            assert!(!HeldTurn::new(&foreign, 0, &foreign).terminal());
        })
        .await;
    }

    /// A held Herdr turn longer than the transcript tails ends on its own abort, each poll reading
    /// at most the poll budget and the next poll reading on from there.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn a_long_held_herdr_turn_ends_on_its_own_abort() {
        use crate::services::provider::cancel_token_claude_interrupt::OWN_TURN_POLL_BUDGET;
        super::super::tests::with_isolated_runtime_root(|| async move {
            let dir = tempfile::tempdir().unwrap();
            for kib in [70, 1100] {
                let path = dir.path().join(format!("long-{kib}.jsonl"));
                let body =
                    codex("task_started", "b") + &reasoning(kib) + &codex("turn_aborted", "b");
                std::fs::write(&path, body).unwrap();
                let held = HeldTurn::new(&path, 0, &path);
                let mut polls = 1;
                while !held.terminal() {
                    assert!(
                        held.polled() <= OWN_TURN_POLL_BUDGET,
                        "{kib} KiB poll {polls}"
                    );
                    assert!(polls < 10, "{kib} KiB never reached its own abort");
                    polls += 1;
                }
                assert!(held.polled() <= OWN_TURN_POLL_BUDGET, "{kib} KiB last poll");
                assert!(kib < 1024 || polls > 4, "1100 KiB read over {polls} polls");
            }
        })
        .await;
    }

    /// A partial line, a transcript cut short or another file at the path is no abort on that
    /// poll; the read goes on at the next poll.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn a_held_turns_unfinished_or_changed_transcript_is_no_abort() {
        super::super::tests::with_isolated_runtime_root(|| async move {
            use std::io::Write;
            let dir = tempfile::tempdir().unwrap();
            let own = codex("task_started", "b") + &codex("turn_aborted", "b");

            let partial = dir.path().join("partial.jsonl");
            std::fs::write(&partial, own.trim_end()).unwrap();
            let held = HeldTurn::new(&partial, 0, &partial);
            assert!(!held.terminal() && !held.terminal(), "a partial abort line");
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&partial)
                .unwrap();
            file.write_all(b"\n").unwrap();
            assert!(held.terminal(), "the completed line is read on");

            let cut = dir.path().join("cut.jsonl");
            std::fs::write(&cut, codex("task_started", "b") + &reasoning(4)).unwrap();
            let held = HeldTurn::new(&cut, 0, &cut);
            assert!(!held.terminal());
            std::fs::write(&cut, &own).unwrap();
            assert!(
                !held.terminal(),
                "a transcript cut below the read is no abort"
            );
            assert!(held.terminal(), "the next poll reads again from the start");

            let swapped = dir.path().join("swapped.jsonl");
            std::fs::write(&swapped, codex("task_started", "b")).unwrap();
            let held = HeldTurn::new(&swapped, 0, &swapped);
            assert!(!held.terminal());
            let other = dir.path().join("other.jsonl");
            std::fs::write(&other, &own).unwrap();
            std::fs::rename(&other, &swapped).unwrap();
            assert!(
                !held.terminal() && !held.terminal(),
                "another file at the path"
            );
        })
        .await;
    }

    /// Once the reader saw the held turn's start as `a`, only `a`'s abort ends it, even after the
    /// transcript is rewritten in place with another turn at that start.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn a_held_turn_ends_only_on_the_turn_its_reader_saw() {
        use std::os::unix::fs::MetadataExt;
        super::super::tests::with_isolated_runtime_root(|| async move {
            let dir = tempfile::tempdir().unwrap();
            let seen_start = codex("task_started", "a");
            for (name, rewrite, ends) in [("other", "b", false), ("own", "a", true)] {
                let path = dir.path().join(format!("{name}.jsonl"));
                std::fs::write(&path, &seen_start).unwrap();
                let held = HeldTurn::new(&path, 0, &path);
                held.herdr.own_start_observed(seen_start.len() as u64, "a");
                let inode = std::fs::metadata(&path).unwrap().ino();
                let body = codex("task_started", rewrite) + &codex("turn_aborted", rewrite);
                std::fs::write(&path, body).unwrap();
                assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
                assert_eq!(held.terminal(), ends, "seen a, rewritten {rewrite}");
            }
        })
        .await;
    }

    /// The reconciler ends a ledger turn on a held Herdr abort only when its key is the row's own
    /// turn: another episode, turn or an uncaptured episode keeps its entry and B's mailbox.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn a_held_abort_advances_only_its_own_ledger_turn() {
        use serenity::model::id::{MessageId, UserId};
        super::super::tests::with_isolated_runtime_root(|| async move {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("b.jsonl");
            std::fs::write(
                &path,
                codex("task_started", "b") + &codex("turn_aborted", "b"),
            )
            .unwrap();
            let held = HeldTurn::new(&path, 0, &path);
            let (shared, own) = (held.shared.clone(), held.key);
            shared
                .tmux_relay_coord(held.channel)
                .confirmed_end_offset
                .store(1, std::sync::atomic::Ordering::Release);
            let message = MessageId::new(own.user_msg_id);
            let mailbox = shared.mailbox(held.channel);
            mailbox
                .restore_active_turn(held.token.clone(), UserId::new(7), message)
                .await;
            let entry = |turn_key: TurnKey, deadline: Instant| LedgerEntry {
                recovery_lease: None,
                phase: Phase::Pending,
                relay_owner: RelayOwnerKind::Watcher,
                provider: ProviderKind::Codex,
                turn_key,
                terminal_deadline: None,
                watcher_backstop_deadline: Some(deadline),
                watcher_backstop_probe_at: None,
                watcher_backstop_terminal_streak: 0,
                watcher_backstop_deadline_pulled: false,
                completion_admission: CompletionAdmission::new(CompletionAdmissionPlan::Immediate),
                finalized_at: None,
            };
            let (now, far) = (Instant::now(), Instant::now() + WATCHER_REGISTER_BACKSTOP);
            let others = [
                TurnKey::new(held.channel, own.user_msg_id, 0).with_episode_nonce(Some("a")),
                TurnKey {
                    user_msg_id: own.user_msg_id + 1,
                    ..own
                },
                TurnKey::new(held.channel, own.user_msg_id, 0),
            ];
            let mut ledger = HashMap::new();
            for (n, key) in (1u64..).zip(others) {
                for (generation, deadline) in [(2 * n, now), (2 * n + 1, far)] {
                    let key = TurnKey { generation, ..key };
                    ledger.insert(key.exact_key(), entry(key, deadline));
                }
            }
            ledger.insert(own.exact_key(), entry(own, far));

            reconcile::reconcile(&mut ledger, &mut HashMap::new(), &shared).await;
            let theirs = |entry: &&LedgerEntry| entry.turn_key.exact_key() != own.exact_key();
            for entry in ledger.values().filter(theirs) {
                let key = entry.turn_key;
                assert!(
                    entry.phase == Phase::Pending,
                    "{key:?} advanced on B's abort"
                );
                assert_eq!(entry.watcher_backstop_terminal_streak, 0, "{key:?}");
            }
            assert_eq!(ledger[&own.exact_key()].watcher_backstop_terminal_streak, 1);
            assert!(
                mailbox.has_active_turn().await.unwrap(),
                "B's mailbox is kept"
            );
            assert!(
                !held
                    .token
                    .cancelled
                    .load(std::sync::atomic::Ordering::Relaxed)
            );
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

    /// A Done turn whose TUI body O posts may finalize on the fast path with no Legacy delivery
    /// proof; busy transcripts, paused handles and a flag-off build keep deferring.
    #[tokio::test(flavor = "current_thread")]
    async fn o_delegated_done_turn_needs_no_legacy_delivery_confirmation() {
        super::super::tests::with_isolated_runtime_root(|| async move {
            let shared = Arc::new(crate::services::discord::make_shared_data_for_tests());
            let entropy = chrono::Utc::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
                .unsigned_abs();
            let channel = ChannelId::new(50_220_025u64.saturating_add(entropy % 1_000_000));
            let session = format!("backstop-o-delegated-{}", std::process::id());
            let transcript = std::env::temp_dir().join(format!("{session}.jsonl"));
            let transcript_str = transcript.to_str().unwrap().to_string();
            let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
            shared.tmux_watchers.insert(
                channel,
                crate::services::discord::TmuxWatcherHandle {
                    tmux_session_name: session.clone(),
                    output_path: transcript_str.clone(),
                    paused: paused.clone(),
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
                212,
                213,
                "O-delegated done turn".to_string(),
                None,
                Some(session.clone()),
                Some(transcript_str.clone()),
                None,
                0,
            );
            state.turn_start_offset = Some(0);
            crate::services::discord::inflight::save_inflight_state(&state).unwrap();
            let _tui = crate::services::tui_o::cutover::test_override::bind_claude_tui_session(
                &session,
                &transcript_str,
            );
            let done = "{\"type\":\"result\",\"result\":\"done\",\"session_id\":\"s\"}\n";
            let busy = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"still going\"}]}}\n";
            let terminal = || {
                watcher_backstop_turn_is_terminal(&shared, channel, &ProviderKind::Claude, false)
            };

            std::fs::write(&transcript, done).unwrap();
            assert!(!terminal(), "flag off: a TUI channel still waits for Legacy delivery proof");
            {
                let _on = crate::services::tui_o::cutover::test_override::force_channels(&[(
                    channel.get() + 1,
                    crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
                )]);
                assert!(!terminal(), "an unlisted channel still waits for Legacy delivery proof");
            }
            {
                let _on = crate::services::tui_o::cutover::test_override::force_channels(&[(
                    channel.get(),
                    crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
                )]);
                assert!(
                    terminal(),
                    "O owns the body: Done with no produced frontier or lease still finalizes"
                );
                paused.store(true, std::sync::atomic::Ordering::Release);
                assert!(!terminal(), "a paused handle defers even when O owns the body");
                paused.store(false, std::sync::atomic::Ordering::Release);
                std::fs::write(&transcript, busy).unwrap();
                assert!(!terminal(), "an unterminated transcript never finalizes");
            }
            {
                let _pending = crate::services::tui_o::cutover::test_override::force_candidates(&[(
                    channel.get(),
                    crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
                )]);
                std::fs::write(&transcript, done).unwrap();
                assert!(!terminal(), "a pending adoption still waits for Legacy delivery proof");
                let adoption = crate::services::tui_o::cutover::test_override::with_channels(|b| {
                    b.unwrap().candidate(channel.get()).unwrap().peek()
                });
                let pending = crate::services::tui_o::channel_policy::Adoption::Pending;
                assert_eq!(adoption, pending, "the backstop sends no body, so it decides nothing");
            }
            let _ = std::fs::remove_file(transcript);
        })
        .await;
    }
}
