use super::*;
use crate::services::discord::InflightTurnState;

/// #2441 (H1) — race a fixed sleep against a `notify`-backed wake-up
/// from `JsonlWatcher`. Returns as soon as EITHER the sleep elapses or
/// the watcher fires its `Notify`. This is the primitive used to replace
/// the six fixed-interval `tokio::time::sleep(200ms / 250ms)` polling
/// sites in the watcher loop: a real wrapper write wakes us immediately
/// while the sleep continues to bound the wake-up latency (defense in
/// depth for environments where the notify backend silently drops
/// events).
async fn sleep_or_jsonl_event(
    sleep: std::time::Duration,
    jsonl_notify: &std::sync::Arc<tokio::sync::Notify>,
    dead_marker_notify: &std::sync::Arc<tokio::sync::Notify>,
) {
    tokio::select! {
        _ = tokio::time::sleep(sleep) => {}
        _ = jsonl_notify.notified() => {}
        _ = dead_marker_notify.notified() => {}
    }
}

fn tmux_dead_marker_exists(tmux_session_name: &str) -> bool {
    std::path::Path::new(&crate::services::tmux_common::session_dead_marker_path(
        tmux_session_name,
    ))
    .exists()
}

fn should_probe_tmux_liveness(
    elapsed_since_last_probe: std::time::Duration,
    dead_marker_present: bool,
) -> bool {
    dead_marker_present || elapsed_since_last_probe >= TMUX_LIVENESS_PROBE_INTERVAL
}

fn build_watcher_streaming_edit_text(
    status_panel_v2_enabled: bool,
    current_portion: &str,
    status_block: &str,
    provider: &ProviderKind,
) -> String {
    if status_panel_v2_enabled {
        crate::services::discord::formatting::build_status_panel_streaming_edit_text(
            current_portion,
            status_block,
            provider,
        )
    } else {
        build_streaming_placeholder_text(current_portion, status_block)
    }
}

fn watcher_should_suppress_streaming_after_bridge_delivery(
    bridge_delivered_turn: bool,
    has_assistant_response: bool,
) -> bool {
    bridge_delivered_turn && has_assistant_response
}

pub(super) fn watcher_lifecycle_terminal_delivery_observed(
    terminal_delivery_observed: bool,
    bridge_delivered_turn: bool,
) -> bool {
    terminal_delivery_observed || bridge_delivered_turn
}

#[cfg(test)]
fn watcher_terminal_edit_consumes_placeholder(outcome: &ReplaceLongMessageOutcome) -> bool {
    matches!(outcome, ReplaceLongMessageOutcome::EditedOriginal)
}

fn watcher_should_delete_suppressed_placeholder(placeholder_from_restored_inflight: bool) -> bool {
    !placeholder_from_restored_inflight
}

fn watcher_fallback_edit_failure_can_delete_original_placeholder(
    _response_sent_offset: usize,
    _last_edit_text: &str,
) -> bool {
    // #2757 parity with session_relay_sink: after a terminal fallback send,
    // the original message id may already contain partial assistant content.
    // Without a Discord probe proving it is a pure placeholder, preserve it.
    false
}

fn watcher_should_defer_delegated_fresh_idle(
    delegated_finalize_owed: bool,
    full_response: &str,
) -> bool {
    delegated_finalize_owed && full_response.trim().is_empty()
}

fn watcher_should_clear_stale_terminal_message_ids(
    inflight_present: bool,
    has_assistant_response: bool,
    placeholder_msg_id: Option<serenity::MessageId>,
) -> bool {
    has_assistant_response && !inflight_present && placeholder_msg_id.is_some()
}

fn discard_restored_response_seed_before_no_inflight_terminal_relay(
    full_response: &mut String,
    response_sent_offset: &mut usize,
    last_edit_text: &mut String,
    restored_response_seed: &str,
    inflight_present: bool,
    _fresh_assistant_text_seen: bool,
) -> bool {
    if inflight_present || restored_response_seed.trim().is_empty() {
        return false;
    }
    if !full_response.starts_with(restored_response_seed) {
        return false;
    }
    let seed_len = restored_response_seed.len();
    full_response.replace_range(..seed_len, "");
    *response_sent_offset = response_sent_offset.saturating_sub(seed_len);
    while *response_sent_offset > 0 && !full_response.is_char_boundary(*response_sent_offset) {
        *response_sent_offset -= 1;
    }
    last_edit_text.clear();
    true
}

fn adopt_watcher_terminal_message_ids_from_inflight(
    placeholder_msg_id: &mut Option<serenity::MessageId>,
    placeholder_from_restored_inflight: &mut bool,
    status_panel_msg_id: &mut Option<serenity::MessageId>,
    inflight: &InflightTurnState,
    tmux_session_name: &str,
) {
    if inflight.rebind_origin {
        return;
    }
    let matches_current_watcher_session = inflight
        .tmux_session_name
        .as_deref()
        .map(str::trim)
        .is_some_and(|name| !name.is_empty() && name == tmux_session_name);
    if !matches_current_watcher_session {
        return;
    }
    let placeholderless_discord_turn = inflight.user_msg_id != 0
        && inflight.current_msg_id != 0
        && inflight.current_msg_id == inflight.user_msg_id;
    if placeholderless_discord_turn {
        return;
    }
    if placeholder_msg_id.is_none() && inflight.current_msg_id != 0 {
        *placeholder_msg_id = Some(serenity::MessageId::new(inflight.current_msg_id));
        *placeholder_from_restored_inflight = true;
    }
    if status_panel_msg_id.is_none() {
        *status_panel_msg_id =
            crate::services::discord::turn_bridge::normalize_status_panel_message_id(
                inflight.status_message_id.map(serenity::MessageId::new),
            );
    }
}

fn watcher_inflight_represents_external_input(inflight: Option<&InflightTurnState>) -> bool {
    inflight.is_some_and(|inflight| {
        matches!(
            inflight.turn_source,
            crate::services::discord::inflight::TurnSource::ExternalInput
                | crate::services::discord::inflight::TurnSource::ExternalAdopted
        )
    })
}

fn watcher_direct_terminal_should_commit_session_idle(
    direct_send_delivered: bool,
    inflight_present: bool,
    _external_input_lease_consumed_by_relay: bool,
    _prompt_anchor_present_before_relay: bool,
    _external_input_lease_before_relay: bool,
    _ssh_direct_pending: bool,
) -> bool {
    direct_send_delivered && !inflight_present
}

fn watcher_terminal_token_update_status(
    watcher_direct_terminal_idle_committed: bool,
) -> &'static str {
    if watcher_direct_terminal_idle_committed {
        crate::db::session_status::IDLE
    } else {
        crate::db::session_status::TURN_ACTIVE
    }
}

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

/// #2442 (H3) — fast-path check for the wrapper's `ready_for_input` JSONL
/// sentinel in the tail of the session jsonl. Reads only the last ~4 KiB
/// so it stays O(1) regardless of jsonl size. False negatives just fall
/// back to the existing 2s `READY_FOR_INPUT_IDLE_PROBE_INTERVAL` cadence,
/// so partial-line / rotation edge cases are harmless.
fn jsonl_tail_contains_ready_for_input_sentinel(output_path: &str) -> bool {
    use std::io::{Read, Seek, SeekFrom};

    const TAIL_WINDOW_BYTES: u64 = 4 * 1024;

    let Ok(mut file) = std::fs::File::open(output_path) else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    let len = meta.len();
    if len == 0 {
        return false;
    }
    let start = len.saturating_sub(TAIL_WINDOW_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let mut buf = Vec::with_capacity(TAIL_WINDOW_BYTES as usize);
    if file.read_to_end(&mut buf).is_err() {
        return false;
    }
    let needle = format!(
        "\"type\":\"{}\"",
        crate::services::tmux_common::WRAPPER_READY_FOR_INPUT_EVENT
    );
    String::from_utf8_lossy(&buf).contains(&needle)
}

fn watcher_jsonl_turn_state_ready_for_input(
    provider: &crate::services::provider::ProviderKind,
    runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    output_path: &str,
    current_offset: u64,
) -> Option<bool> {
    let path = std::path::Path::new(output_path);
    crate::services::tui_turn_state::jsonl_ready_for_input(
        provider,
        runtime_kind,
        path,
        Some(current_offset),
    )
    .map(crate::services::tui_turn_state::TuiReadyState::is_ready)
}

fn watcher_session_ready_for_input(
    tmux_session_name: &str,
    provider: &crate::services::provider::ProviderKind,
    output_path: &str,
    current_offset: u64,
) -> bool {
    let runtime_kind =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name)
            .map(|binding| binding.runtime_kind)
            .or_else(|| {
                crate::services::tmux_common::resolve_tmux_runtime_kind_marker(tmux_session_name)
            });
    if let Some(ready) = watcher_jsonl_turn_state_ready_for_input(
        provider,
        runtime_kind,
        output_path,
        current_offset,
    ) {
        return ready;
    }
    if crate::services::tui_turn_state::pane_ready_fallback_allowed(provider, runtime_kind) {
        crate::services::provider::tmux_session_ready_for_input(tmux_session_name, provider)
    } else {
        false
    }
}

fn observe_qwen_user_prompts_in_buffer(
    buffer: &str,
    provider: &crate::services::provider::ProviderKind,
    tmux_session_name: &str,
) {
    if !matches!(provider, crate::services::provider::ProviderKind::Qwen) {
        return;
    }
    for line in buffer.lines() {
        let _ = crate::services::qwen::observe_qwen_user_prompt_line(line, Some(tmux_session_name));
    }
}

fn watcher_batch_contains_relayable_response(data: &[u8]) -> bool {
    let text = String::from_utf8_lossy(data);
    text.contains("\"type\":\"assistant\"")
        || text.contains("\"type\": \"assistant\"")
        || text.contains("\"type\":\"result\"")
        || text.contains("\"type\": \"result\"")
}

fn watcher_batch_contains_assistant_event(data: &[u8]) -> bool {
    let text = String::from_utf8_lossy(data);
    text.contains("\"type\":\"assistant\"") || text.contains("\"type\": \"assistant\"")
}

fn legacy_wrapper_prompt_candidates_from_pane(pane: &str) -> Vec<String> {
    let mut collecting = false;
    let mut current_block: Vec<String> = Vec::new();
    let mut last_submitted_block: Vec<String> = Vec::new();

    for raw_line in pane.lines() {
        let line = raw_line.trim_matches('\r').trim();
        if line.contains("Ready for input") {
            collecting = true;
            current_block.clear();
            continue;
        }
        if line == "[sending...]" {
            if collecting && !current_block.is_empty() {
                last_submitted_block = current_block.clone();
            }
            collecting = false;
            current_block.clear();
            continue;
        }
        if collecting && !line.is_empty() {
            current_block.push(line.to_string());
        }
    }

    if last_submitted_block.is_empty() {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    for candidate in [
        last_submitted_block.join(""),
        last_submitted_block.join(" "),
        last_submitted_block.join("\n"),
    ] {
        let candidate = candidate.trim();
        if candidate.is_empty() {
            continue;
        }
        if !candidates.iter().any(|existing: &String| {
            crate::services::tui_prompt_dedupe::prompts_match(existing, candidate)
        }) {
            candidates.push(candidate.to_string());
        }
    }
    candidates
}

fn observe_legacy_wrapper_direct_prompt_from_pane(
    provider: &crate::services::provider::ProviderKind,
    tmux_session_name: &str,
    channel_id: serenity::ChannelId,
    data_start_offset: u64,
    current_offset: u64,
) -> crate::services::tui_prompt_dedupe::PromptObservation {
    let Some(pane) = crate::services::platform::tmux::capture_pane(tmux_session_name, -160) else {
        return crate::services::tui_prompt_dedupe::PromptObservation::Ignored;
    };
    let candidates = legacy_wrapper_prompt_candidates_from_pane(&pane);
    if candidates.is_empty() {
        return crate::services::tui_prompt_dedupe::PromptObservation::Ignored;
    }
    let observation =
        crate::services::tui_prompt_dedupe::observe_prompt_candidates_by_tmux_for_relay_lease(
            provider.as_str(),
            tmux_session_name,
            &candidates,
        );
    tracing::info!(
        provider = %provider.as_str(),
        channel = channel_id.get(),
        tmux_session = %tmux_session_name,
        data_start_offset,
        current_offset,
        observation = ?observation,
        "watcher: observed legacy wrapper pane prompt before post-terminal suppression"
    );
    observation
}

/// #2427 D/A wires — emit an explicit-signal inflight cleanup attempt.
///
/// Used by the TurnCompleted broadcast and the dead-pane post-mortem
/// path. The on-disk inflight is guarded so that:
///   * stale signals arriving after a new turn has written its own
///     inflight do not delete the new turn's file (Pitfall #1);
///   * planned-restart markers (`restart_mode = Some(_)`) survive across
///     the dcserver restart they were saved for;
///   * `rebind_origin` rows owned by the rebind API are not touched
///     (Pitfall #5).
///
/// All outcomes are logged at trace/info level so the sweeper safety-net
/// strikes are easy to spot when this hook misses.
pub(in crate::services::discord) fn emit_explicit_inflight_cleanup_signal(
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected_user_msg_id: u64,
    reason: &'static str,
) {
    let outcome = crate::services::discord::inflight::clear_inflight_state_if_matches(
        provider,
        channel_id.get(),
        expected_user_msg_id,
    );
    log_explicit_inflight_cleanup_outcome(
        provider,
        channel_id,
        expected_user_msg_id,
        reason,
        outcome,
    );
}

fn log_explicit_inflight_cleanup_outcome(
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected_user_msg_id: u64,
    reason: &'static str,
    outcome: crate::services::discord::inflight::GuardedClearOutcome,
) {
    match outcome {
        crate::services::discord::inflight::GuardedClearOutcome::Cleared => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                provider = %provider.as_str(),
                channel = channel_id.get(),
                user_msg_id = expected_user_msg_id,
                reason = reason,
                "[{ts}] 🧹 inflight cleared via explicit completion signal (#2427)"
            );
        }
        crate::services::discord::inflight::GuardedClearOutcome::Missing => {
            tracing::trace!(
                provider = %provider.as_str(),
                channel = channel_id.get(),
                reason = reason,
                "inflight already absent — explicit signal redundant (#2427)"
            );
        }
        crate::services::discord::inflight::GuardedClearOutcome::UserMsgMismatch => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id.get(),
                expected_user_msg_id = expected_user_msg_id,
                reason = reason,
                "[{ts}] ⚠ inflight user_msg_id mismatch — stale explicit signal ignored (#2427 Pitfall #1)"
            );
        }
        crate::services::discord::inflight::GuardedClearOutcome::PlannedRestartSkipped => {
            tracing::debug!(
                provider = %provider.as_str(),
                channel = channel_id.get(),
                reason = reason,
                "skipping explicit inflight cleanup — planned-restart marker present (#2427)"
            );
        }
        crate::services::discord::inflight::GuardedClearOutcome::RebindOriginSkipped => {
            tracing::debug!(
                provider = %provider.as_str(),
                channel = channel_id.get(),
                reason = reason,
                "skipping explicit inflight cleanup — rebind_origin row (#2427 Pitfall #5)"
            );
        }
        crate::services::discord::inflight::GuardedClearOutcome::IoError => {
            // Surfaces filesystem failures explicitly so the operator can
            // see the sweeper's 1800s safety-net is the only thing
            // catching the failed cleanup. Caller does not clear watcher.
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id.get(),
                reason = reason,
                "explicit inflight cleanup failed with IO error — sweeper safety-net will retry"
            );
        }
    }
}

/// #2427 A wire — synchronous variant for the dead-pane post-mortem,
/// which runs on a `spawn_blocking` thread.
///
/// Codex round-2 HIGH-1: a naïve "load → re-feed user_msg_id" guard is
/// self-authenticating (a new turn's inflight matches itself). To make
/// the guard meaningful for the pane-death path, we also require the
/// loaded inflight to point at the *same dead tmux session* the caller
/// witnessed. If a fresh `start_claude` respawn already replaced the
/// inflight with one tied to a new (live) tmux name, we leave it alone
/// — the new turn's pane is alive, and its inflight does not belong to
/// us to clear.
pub(in crate::services::discord) fn emit_explicit_inflight_cleanup_signal_pane_dead(
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected_tmux_session_name: &str,
    expected_identity: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
) {
    let Some(state) =
        crate::services::discord::inflight::load_inflight_state(provider, channel_id.get())
    else {
        return;
    };
    if state.tmux_session_name.as_deref() != Some(expected_tmux_session_name) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::debug!(
            provider = %provider.as_str(),
            channel = channel_id.get(),
            on_disk = ?state.tmux_session_name,
            expected = expected_tmux_session_name,
            "[{ts}] skipping pane-dead explicit cleanup — inflight points at a different tmux session (#2427 A self-auth guard)"
        );
        return;
    }
    let Some(identity) = expected_identity else {
        tracing::warn!(
            provider = %provider.as_str(),
            channel = channel_id.get(),
            expected_tmux_session_name,
            "pane-dead inflight cleanup skipped because watcher attach identity is unavailable (#2450)"
        );
        return;
    };
    let outcome = crate::services::discord::inflight::clear_inflight_state_if_matches_identity(
        provider,
        channel_id.get(),
        identity,
    );
    log_explicit_inflight_cleanup_outcome(
        provider,
        channel_id,
        state.user_msg_id,
        "pane_dead",
        outcome,
    );
}

fn matching_watcher_turn_identity(
    state: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
) -> Option<crate::services::discord::inflight::InflightTurnIdentity> {
    state
        .filter(|state| state.tmux_session_name.as_deref() == Some(tmux_session_name))
        .map(crate::services::discord::inflight::InflightTurnIdentity::from_state)
}

fn refresh_watcher_turn_identity(
    current: &mut Option<crate::services::discord::inflight::InflightTurnIdentity>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
) {
    let inflight =
        crate::services::discord::inflight::load_inflight_state(provider, channel_id.get());
    *current = matching_watcher_turn_identity(inflight.as_ref(), tmux_session_name);
}

#[cfg(test)]
mod pane_dead_identity_tests {
    use super::*;
    use crate::services::discord::inflight::InflightTurnState;

    fn state_for_turn(user_msg_id: u64, tmux_session_name: &str) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            7,
            user_msg_id,
            user_msg_id + 1,
            "prompt".to_string(),
            None,
            Some(tmux_session_name.to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        )
    }

    #[test]
    fn watcher_identity_refreshes_for_next_turn_on_same_long_lived_session() {
        let first = state_for_turn(100, "AgentDesk-codex-adk-cdx");
        let second = state_for_turn(200, "AgentDesk-codex-adk-cdx");
        let mut identity = matching_watcher_turn_identity(Some(&first), "AgentDesk-codex-adk-cdx");
        assert_eq!(identity.as_ref().unwrap().user_msg_id, 100);

        identity = matching_watcher_turn_identity(Some(&second), "AgentDesk-codex-adk-cdx");

        assert_eq!(identity.unwrap().user_msg_id, 200);
    }

    #[test]
    fn watcher_identity_does_not_adopt_different_session_name() {
        let first = state_for_turn(100, "AgentDesk-codex-adk-cdx");
        let second = state_for_turn(200, "AgentDesk-codex-adk-cdx-fresh");
        let mut identity = matching_watcher_turn_identity(Some(&first), "AgentDesk-codex-adk-cdx");
        assert_eq!(identity.as_ref().unwrap().user_msg_id, 100);

        identity = matching_watcher_turn_identity(Some(&second), "AgentDesk-codex-adk-cdx");

        assert!(identity.is_none());
    }
}

/// E5 (#2412): forward a freshly-read tmux output chunk into the
/// supervisor-owned [`StreamRelay`] (if one exists for the session). The
/// supervisor's [`RelayProducerRegistry`] is the bridge — it hands the
/// production tmux watcher a clonable
/// [`crate::services::cluster::stream_relay::RelayProducer`] keyed by
/// `tmux_session_name`. The producer's MPSC absorbs the chunk; the
/// relay task drains it into the configured [`RelaySink`]. In production
/// that sink parses provider JSONL and performs Discord terminal delivery
/// for eligible session-bound inflight shapes; metrics-only fallback
/// runtimes still count frames via
/// [`crate::services::cluster::registry_adapter_sink::RegistryAdapterSink`].
///
/// `cached_producer` caches a single producer clone to avoid taking the
/// registry RwLock on every chunk read; it is refreshed from the registry
/// when the cache is empty or when an attempted send observed a torn-down
/// relay (`try_send_frame` returned `false`). When the registry has no
/// producer for this session (flag off, supervisor not running, or this
/// session simply not in the registry's matched set) the function is a
/// total no-op and adds no measurable overhead vs the pre-E5 hot path.
#[derive(Clone)]
struct SessionBoundRelayAckTarget {
    metrics: std::sync::Arc<crate::services::cluster::stream_relay::RelayMetrics>,
    sequence: u64,
}

#[derive(Clone)]
struct SupervisorRelayForward {
    mirrored: bool,
    ack_target: Option<SessionBoundRelayAckTarget>,
}

impl SupervisorRelayForward {
    fn mirrored_without_ack() -> Self {
        Self {
            mirrored: true,
            ack_target: None,
        }
    }

    fn not_mirrored() -> Self {
        Self {
            mirrored: false,
            ack_target: None,
        }
    }
}

fn discard_watcher_pending_buffer_after_suppressed_turn(
    all_data: &mut String,
    all_data_start_offset: &mut u64,
    all_data_fully_mirrored_to_session_relay: &mut bool,
    all_data_session_bound_relay_ack: &mut Option<SessionBoundRelayAckTarget>,
    current_offset: u64,
) {
    all_data.clear();
    *all_data_start_offset = current_offset;
    *all_data_fully_mirrored_to_session_relay = true;
    *all_data_session_bound_relay_ack = None;
}

#[derive(Debug, Default)]
struct Utf8ChunkDecoder {
    pending: Vec<u8>,
    pending_start_offset: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
struct DecodedUtf8Chunk {
    start_offset: Option<u64>,
    text: String,
}

impl Utf8ChunkDecoder {
    fn decode(&mut self, chunk: &[u8], chunk_start_offset: u64) -> DecodedUtf8Chunk {
        if chunk.is_empty() {
            return DecodedUtf8Chunk {
                start_offset: None,
                text: String::new(),
            };
        }
        if self.pending.is_empty() {
            self.pending_start_offset = Some(chunk_start_offset);
        }
        self.pending.extend_from_slice(chunk);

        let start_offset = self.pending_start_offset.unwrap_or(chunk_start_offset);
        match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                let text = text.to_string();
                self.pending.clear();
                self.pending_start_offset = None;
                DecodedUtf8Chunk {
                    start_offset: Some(start_offset),
                    text,
                }
            }
            Err(err) if err.error_len().is_none() => {
                let valid_up_to = err.valid_up_to();
                if valid_up_to == 0 {
                    return DecodedUtf8Chunk {
                        start_offset: None,
                        text: String::new(),
                    };
                }
                let text = std::str::from_utf8(&self.pending[..valid_up_to])
                    .expect("valid UTF-8 prefix")
                    .to_string();
                self.pending.drain(..valid_up_to);
                self.pending_start_offset = Some(start_offset.saturating_add(valid_up_to as u64));
                DecodedUtf8Chunk {
                    start_offset: Some(start_offset),
                    text,
                }
            }
            Err(_) => {
                let text = String::from_utf8_lossy(&self.pending).into_owned();
                self.pending.clear();
                self.pending_start_offset = None;
                DecodedUtf8Chunk {
                    start_offset: Some(start_offset),
                    text,
                }
            }
        }
    }

    fn clear_pending(&mut self) {
        self.pending.clear();
        self.pending_start_offset = None;
    }
}

fn forward_chunk_to_supervisor_relay(
    tmux_session_name: &str,
    chunk: &str,
    registry: &std::sync::Arc<
        crate::services::cluster::relay_producer_registry::RelayProducerRegistry,
    >,
    cached_producer: &mut Option<crate::services::cluster::stream_relay::RelayProducer>,
) -> SupervisorRelayForward {
    if chunk.is_empty() {
        return SupervisorRelayForward::mirrored_without_ack();
    }
    if cached_producer.is_none() {
        *cached_producer = registry.get_producer(tmux_session_name);
    }
    let Some(producer) = cached_producer.as_ref() else {
        return SupervisorRelayForward::not_mirrored();
    };
    // The relay treats each `try_send_frame` call as one frame. The caller
    // decodes only complete UTF-8 prefixes, so a multibyte scalar split across
    // file reads is forwarded after the next read completes it instead of being
    // replaced with U+FFFD.
    let payload = chunk.to_string();
    let outcome = producer.try_send_frame_with_sequence(payload);
    if !outcome.is_alive() {
        // Relay was torn down between our registry read and the send —
        // drop the cache so the next chunk re-resolves. If the supervisor
        // republishes for the same session name (Updated event), the
        // next call will hit the new producer.
        *cached_producer = None;
        return SupervisorRelayForward::not_mirrored();
    }
    SupervisorRelayForward {
        mirrored: true,
        ack_target: outcome.sequence.map(|sequence| SessionBoundRelayAckTarget {
            metrics: producer.metrics().clone(),
            sequence,
        }),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionBoundRelayAckOutcome {
    Delivered,
    TerminalSkipped,
    Dropped,
    SinkError,
    TimedOut,
    MissingTarget,
}

fn sequence_reached(latest: Option<u64>, target: u64) -> bool {
    latest.is_some_and(|sequence| sequence >= target)
}

fn session_bound_relay_ack_snapshot_outcome(
    target: Option<&SessionBoundRelayAckTarget>,
) -> Option<SessionBoundRelayAckOutcome> {
    let target = target?;
    let snapshot = target.metrics.snapshot();
    if sequence_reached(snapshot.last_terminal_committed_sequence, target.sequence) {
        return Some(SessionBoundRelayAckOutcome::Delivered);
    }
    if sequence_reached(snapshot.last_terminal_skipped_sequence, target.sequence) {
        return Some(SessionBoundRelayAckOutcome::TerminalSkipped);
    }
    if sequence_reached(snapshot.last_sink_error_sequence, target.sequence) {
        return Some(SessionBoundRelayAckOutcome::SinkError);
    }
    if sequence_reached(snapshot.last_dropped_sequence, target.sequence) {
        return Some(SessionBoundRelayAckOutcome::Dropped);
    }
    None
}

fn session_bound_relay_frame_ack_reached(target: Option<&SessionBoundRelayAckTarget>) -> bool {
    let Some(target) = target else {
        return false;
    };
    let snapshot = target.metrics.snapshot();
    sequence_reached(snapshot.last_delivered_sequence, target.sequence)
}

fn watcher_should_direct_send_after_session_bound_ack(
    should_direct_send: bool,
    ack_outcome: SessionBoundRelayAckOutcome,
) -> bool {
    should_direct_send && !matches!(ack_outcome, SessionBoundRelayAckOutcome::Delivered)
}

fn watcher_terminal_response_for_direct_send<'a>(
    full_response: &'a str,
    response_sent_offset: usize,
    session_bound_fallback_uses_full_body: bool,
) -> &'a str {
    if session_bound_fallback_uses_full_body {
        return full_response;
    }
    full_response.get(response_sent_offset..).unwrap_or("")
}

fn watcher_should_send_ordered_new_chunks_for_terminal_fallback(
    session_bound_fallback_uses_full_body: bool,
    relay_text: &str,
) -> bool {
    session_bound_fallback_uses_full_body
        && relay_text.len() > crate::services::discord::DISCORD_MSG_LIMIT
}

/// #2840 (relay-stability P1): RAII guard for the cross-watcher emission slot
/// (`relay_coord.relay_slot`, an `Arc<AtomicU64>`: 0 = free, non-zero = a
/// watcher is mid-emission with that start offset). The slot is shared across
/// every watcher instance for a channel/session, so if the holding watcher
/// early-returns, hits a `?`, panics, or is task-aborted between CAS-acquire
/// and the manual `store(0)`, the slot stays non-zero forever and every
/// replacement watcher's relay is skipped — a permanent channel wedge until
/// process restart.
///
/// The guard releases the slot on Drop so ANY exit path frees it. The two
/// intended in-loop release points still call `release()` explicitly to
/// preserve their exact timing (site 1 releases *before* a 500ms backoff sleep,
/// so scope-end Drop alone would hold the slot across that sleep); the
/// idempotent `released` flag makes the trailing Drop a no-op after an explicit
/// release.
struct RelaySlotGuard {
    slot: std::sync::Arc<std::sync::atomic::AtomicU64>,
    released: bool,
}

impl RelaySlotGuard {
    fn new(slot: std::sync::Arc<std::sync::atomic::AtomicU64>) -> Self {
        Self {
            slot,
            released: false,
        }
    }

    fn release(&mut self) {
        if !self.released {
            self.slot.store(0, std::sync::atomic::Ordering::Release);
            self.released = true;
        }
    }
}

impl Drop for RelaySlotGuard {
    fn drop(&mut self) {
        if !self.released {
            // #2841 (codex review): reaching Drop without a prior explicit
            // release() means an abnormal exit (panic / `?` / task
            // cancellation) BEFORE the turn recorded its relayed offset /
            // advanced confirmed-end — so the delivery outcome of any in-flight
            // Discord send is UNKNOWN. Freeing the slot prevents a permanent
            // channel wedge, but a replacement watcher MAY then re-emit the same
            // range (a bounded duplicate window). This is strictly better than a
            // permanent wedge; the (channel, turn, byte-range) delivery lease
            // (P1) closes the window by recording delivery BEFORE the slot
            // frees. Surface it so the window is measurable until the lease lands.
            tracing::warn!(
                target: "agentdesk::relay_flight_recorder",
                "relay emission slot freed via Drop on abnormal exit (in-flight send outcome unknown); a replacement watcher may re-emit the same range — resolved by the delivery lease"
            );
        }
        self.release();
    }
}

async fn wait_for_session_bound_relay_delivery_ack(
    target: Option<&SessionBoundRelayAckTarget>,
    timeout: std::time::Duration,
) -> SessionBoundRelayAckOutcome {
    if target.is_none() {
        return SessionBoundRelayAckOutcome::MissingTarget;
    }
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(outcome) = session_bound_relay_ack_snapshot_outcome(target) {
            return outcome;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return SessionBoundRelayAckOutcome::TimedOut;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25).min(deadline - now)).await;
    }
}

fn terminal_event_consumed_offset(current_offset: u64, unprocessed_tail: &str) -> u64 {
    current_offset.saturating_sub(unprocessed_tail.len() as u64)
}

async fn persist_watcher_provider_session_id(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    provider: &ProviderKind,
    tmux_session_name: &str,
    session_id: Option<&str>,
) {
    let Some(session_id) = session_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };

    {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id)
            && !session.cleared
        {
            session.restore_provider_session(Some(session_id.to_string()));
        }
    }

    let session_key = crate::services::discord::adk_session::build_namespaced_session_key(
        &shared.token_hash,
        provider,
        tmux_session_name,
    );
    crate::services::discord::adk_session::save_provider_session_id(
        &session_key,
        session_id,
        Some(session_id),
        provider,
        channel_id,
        shared.api_port,
    )
    .await;

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 👁 watcher persisted provider session selector for {} channel {}",
        tmux_session_name,
        channel_id.get()
    );
}

async fn complete_watcher_status_panel_v2(
    http: &serenity::Http,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    status_panel_msg_id: Option<serenity::MessageId>,
    provider: &ProviderKind,
    started_at_unix: i64,
    last_status_panel_text: &mut String,
    background: bool,
    expected_user_msg_id: Option<u64>,
) {
    // #2427 D wire (Codex round 2 HIGH-1): explicit-signal inflight cleanup
    // is intentionally NOT emitted from the watcher path. The watcher is
    // not turn-scoped, so any user_msg_id read here would be the *current*
    // on-disk value (possibly the next turn's). The committed-output path
    // at L~2996 already performs the unconditional `clear_inflight_state`
    // for the turn the watcher actually finished. Recovery-driven
    // TurnCompleted still emits the guarded signal (see recovery_engine.rs)
    // because its state snapshot is pinned at recovery entry.
    if !shared.status_panel_v2_enabled {
        return;
    }
    let _committed = crate::services::discord::turn_bridge::complete_status_panel_v2_with_http(
        shared,
        http,
        channel_id,
        status_panel_msg_id,
        provider,
        started_at_unix,
        last_status_panel_text,
        background,
        "tmux_watcher",
        expected_user_msg_id,
    )
    .await;
}

/// #2161 — TUI completion gate. Callers ask `run_tui_completion_gate` to
/// confirm the underlying tmux pane has reached a `Ready for input`
/// quiescent state before pushing `StatusEvent::TurnCompleted` to the live
/// status panel.
///
/// Only `RuntimeHandoffKind::ClaudeTui` turns are gated; other runtime kinds
/// return `NotGated` (= emit immediately) so existing completion contracts
/// stay unchanged (see `should_gate_completion_for_tui_quiescence` in
/// `tmux.rs` for the full matrix).
///
/// The wait is bounded by `TUI_COMPLETION_QUIESCENCE_TIMEOUT`. On `TimedOut`
/// the caller MUST suppress the `TurnCompleted` emit — promoting the panel
/// to `✅ 응답 완료` on a still-busy pane reproduces the bug this gate
/// exists to prevent (Codex review #2161 H2). If terminal delivery is not
/// yet durably mirrored, the placeholder sweeper and next-turn intake
/// reconcile the lingering Active panel; already-committed delivery may still
/// proceed with non-visual lifecycle cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum TuiCompletionGateOutcome {
    NotGated,
    ConfirmedIdle,
    SkippedDead,
    TimedOut,
}

impl TuiCompletionGateOutcome {
    /// `true` when callers should proceed with emitting the user-visible
    /// `TurnCompleted` status event. `false` only on `TimedOut`, where
    /// the pane is still busy past the bounded wait and emitting would
    /// reproduce the #2161 premature-completion bug. The placeholder
    /// sweeper / next-turn intake reconciles the still-Active panel later.
    pub(in crate::services::discord) fn should_emit_completion(self) -> bool {
        match self {
            Self::NotGated | Self::ConfirmedIdle | Self::SkippedDead => true,
            Self::TimedOut => false,
        }
    }
}

/// Source-agnostic terminal probe for a matched session's provider JSONL.
/// `InflightTurnState::turn_source` is audit metadata only (#2346/#2285).
fn matched_session_jsonl_turn_state(
    provider: &ProviderKind,
    inflight: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
) -> Option<crate::services::tui_turn_state::TuiTurnState> {
    let state = inflight?;
    if state.tmux_session_name.as_deref() != Some(tmux_session_name) {
        return None;
    }
    let output_path = state
        .output_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())?;
    let path = std::path::Path::new(output_path);
    let Ok(metadata) = std::fs::metadata(path) else {
        return Some(crate::services::tui_turn_state::TuiTurnState::Unknown);
    };
    if !metadata.is_file() || metadata.len() == 0 {
        return Some(crate::services::tui_turn_state::TuiTurnState::Unknown);
    }
    Some(crate::services::tui_turn_state::observe_provider_jsonl_turn_state(provider, path))
}

fn matched_session_structured_ready_for_input(
    provider: &ProviderKind,
    inflight: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
) -> Option<crate::services::tui_turn_state::TuiReadyState> {
    let state = inflight?;
    if state.tmux_session_name.as_deref() != Some(tmux_session_name) {
        return None;
    }
    let output_path = state
        .output_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())?;
    crate::services::tui_turn_state::jsonl_ready_for_input(
        provider,
        state.runtime_kind,
        std::path::Path::new(output_path),
        None,
    )
}

fn jsonl_terminal_can_confirm_completion(
    inflight: Option<&crate::services::discord::inflight::InflightTurnState>,
) -> bool {
    inflight.is_some_and(|state| {
        let has_session_binding = state
            .tmux_session_name
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
            && state
                .output_path
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty());
        let placeholderless_discord_turn =
            state.user_msg_id != 0 && state.current_msg_id == state.user_msg_id;
        let adopted_session_turn =
            state.rebind_origin && state.user_msg_id == 0 && state.current_msg_id == 0;
        let watcher_owned_session_bound_turn = matches!(
            state.effective_relay_owner_kind(),
            crate::services::discord::inflight::RelayOwnerKind::Watcher
        ) && !state.rebind_origin;
        let managed_terminal_runtime_turn = matches!(
            state.runtime_kind,
            Some(
                crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
                    | crate::services::agent_protocol::RuntimeHandoffKind::ProcessBackend,
            )
        ) && !state.rebind_origin
            && state.user_msg_id != 0
            && state.current_msg_id != 0
            && state
                .turn_start_offset
                .map(|start| state.last_offset > start)
                .unwrap_or(false);
        let legacy_terminal_shortcut = if state.rebind_origin {
            adopted_session_turn
        } else {
            placeholderless_discord_turn
        };

        has_session_binding
            && ((state.status_message_id.is_none() && legacy_terminal_shortcut)
                || watcher_owned_session_bound_turn
                || managed_terminal_runtime_turn)
    })
}

fn session_bound_relay_should_own_terminal_delivery(
    should_direct_send: bool,
    session_bound_discord_delivery_enabled: bool,
    session_bound_relay_turn_fully_mirrored: bool,
    relay_producer_session_name: Option<&str>,
    inflight: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
) -> bool {
    should_direct_send
        && session_bound_discord_delivery_enabled
        && session_bound_relay_turn_fully_mirrored
        && relay_producer_session_name == Some(tmux_session_name)
        && crate::services::discord::session_relay_sink::session_bound_discord_relay_can_own_terminal_delivery(
            inflight,
            tmux_session_name,
        )
}

fn post_terminal_jsonl_payload_contains_init_without_user_event(payload: &[u8]) -> bool {
    let mut contains_init = false;
    for line in String::from_utf8_lossy(payload).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("user") => return false,
            Some("system")
                if value.get("subtype").and_then(serde_json::Value::as_str) == Some("init") =>
            {
                contains_init = true;
            }
            _ => {}
        }
    }
    contains_init
}

#[cfg(test)]
mod matched_session_jsonl_gate_tests {
    use super::*;

    fn state_for_matched_session(
        provider: ProviderKind,
        tmux_session_name: &str,
        output_path: &str,
    ) -> crate::services::discord::inflight::InflightTurnState {
        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            provider,
            42,
            Some("relay-test".to_string()),
            7,
            9001,
            9002,
            "typed over ssh".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some(output_path.to_string()),
            Some("/tmp/input.fifo".to_string()),
            0,
        );
        state.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
        state
    }

    #[test]
    fn matched_session_terminal_jsonl_confirms_idle_without_turn_source_branch() {
        let file = tempfile::NamedTempFile::new().expect("temp jsonl");
        std::fs::write(
            file.path(),
            r#"{"type":"result","result":"done","session_id":"s"}"#,
        )
        .expect("write jsonl");
        let tmux_session_name = "AgentDesk-claude-relay-test";
        let state = state_for_matched_session(
            ProviderKind::Claude,
            tmux_session_name,
            &file.path().display().to_string(),
        );

        assert_eq!(
            matched_session_jsonl_turn_state(
                &ProviderKind::Claude,
                Some(&state),
                tmux_session_name
            ),
            Some(crate::services::tui_turn_state::TuiTurnState::Idle)
        );
    }

    #[test]
    fn turn_source_does_not_affect_jsonl_completion_probe() {
        let file = tempfile::NamedTempFile::new().expect("temp jsonl");
        std::fs::write(
            file.path(),
            r#"{"type":"result","result":"done","session_id":"s"}"#,
        )
        .expect("write jsonl");
        let tmux_session_name = "AgentDesk-claude-relay-test";
        let mut state = state_for_matched_session(
            ProviderKind::Claude,
            tmux_session_name,
            &file.path().display().to_string(),
        );
        state.turn_source = crate::services::discord::inflight::TurnSource::Managed;

        assert_eq!(
            matched_session_jsonl_turn_state(
                &ProviderKind::Claude,
                Some(&state),
                tmux_session_name
            ),
            Some(crate::services::tui_turn_state::TuiTurnState::Idle)
        );
    }

    #[test]
    fn jsonl_terminal_completion_shortcut_uses_turn_shape_not_turn_source() {
        let mut state = state_for_matched_session(
            ProviderKind::Claude,
            "AgentDesk-claude-relay-test",
            "/tmp/unused.jsonl",
        );
        state.turn_source = crate::services::discord::inflight::TurnSource::Managed;
        state.current_msg_id = state.user_msg_id;
        state.status_message_id = None;
        assert!(jsonl_terminal_can_confirm_completion(Some(&state)));

        state.current_msg_id = state.user_msg_id + 1;
        assert!(!jsonl_terminal_can_confirm_completion(Some(&state)));

        state.current_msg_id = state.user_msg_id;
        state.rebind_origin = true;
        assert!(!jsonl_terminal_can_confirm_completion(Some(&state)));

        state.rebind_origin = false;
        state.tmux_session_name = None;
        assert!(!jsonl_terminal_can_confirm_completion(Some(&state)));
    }

    #[test]
    fn jsonl_terminal_completion_accepts_session_bound_watcher_owned_placeholder() {
        let mut state = state_for_matched_session(
            ProviderKind::Claude,
            "AgentDesk-claude-watcher-owned",
            "/tmp/watcher-owned.jsonl",
        );
        state.current_msg_id = state.user_msg_id + 1;
        state.status_message_id = Some(state.current_msg_id + 1);
        state.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);

        assert!(
            jsonl_terminal_can_confirm_completion(Some(&state)),
            "session-bound watcher-owned terminal envelopes should finish cleanup even with a placeholder/status panel"
        );
    }

    #[test]
    fn jsonl_terminal_completion_accepts_watcher_owned_external_zero_message_claim() {
        let mut state = state_for_matched_session(
            ProviderKind::Claude,
            "AgentDesk-claude-watcher-external",
            "/tmp/watcher-external.jsonl",
        );
        state.user_msg_id = 0;
        state.current_msg_id = 0;
        state.rebind_origin = false;
        state.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);

        assert!(
            jsonl_terminal_can_confirm_completion(Some(&state)),
            "watcher-owned external pane claims should not need rebind_origin to finish cleanup"
        );
    }

    #[test]
    fn jsonl_terminal_completion_accepts_managed_claude_tui_bridge_owned_placeholder() {
        let mut state = state_for_matched_session(
            ProviderKind::Claude,
            "AgentDesk-claude-bridge-owned",
            "/tmp/bridge-owned.jsonl",
        );
        state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
        state.current_msg_id = state.user_msg_id + 1;
        state.status_message_id = Some(state.current_msg_id + 1);
        state.turn_start_offset = Some(10);
        state.last_offset = 42;

        assert!(
            jsonl_terminal_can_confirm_completion(Some(&state)),
            "matched ClaudeTui terminal JSONL should release bridge-owned placeholders instead of waiting forever on pane prompt detection"
        );

        state
            .set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::StandbyRelay);
        assert!(
            jsonl_terminal_can_confirm_completion(Some(&state)),
            "managed ClaudeTui terminal JSONL remains authoritative even if a relay-owner label is stale"
        );

        state.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::None);
        state.turn_start_offset = Some(42);
        state.last_offset = 42;
        assert!(
            !jsonl_terminal_can_confirm_completion(Some(&state)),
            "a stale prior terminal envelope must not unlock a fresh turn that has not advanced the output offset"
        );

        state.turn_start_offset = None;
        state.last_offset = 99;
        state.full_response = "prior response".to_string();
        assert!(
            !jsonl_terminal_can_confirm_completion(Some(&state)),
            "without a current turn_start_offset anchor, non-empty full_response is not enough to unlock cleanup"
        );
    }

    #[test]
    fn jsonl_terminal_completion_accepts_managed_process_backend_bridge_owned_placeholder() {
        let mut state = state_for_matched_session(
            ProviderKind::Codex,
            "AgentDesk-codex-process-backend",
            "/tmp/process-backend.jsonl",
        );
        state.runtime_kind =
            Some(crate::services::agent_protocol::RuntimeHandoffKind::ProcessBackend);
        state.current_msg_id = state.user_msg_id + 1;
        state.status_message_id = Some(state.current_msg_id + 1);
        state.turn_start_offset = Some(100);
        state.last_offset = 150;

        assert!(
            jsonl_terminal_can_confirm_completion(Some(&state)),
            "process backend terminal JSONL should also release bridge-owned live placeholders"
        );
    }

    #[test]
    fn jsonl_terminal_completion_rejects_unanchored_managed_runtime_shapes() {
        let mut state = state_for_matched_session(
            ProviderKind::Claude,
            "AgentDesk-claude-guarded",
            "/tmp/guarded.jsonl",
        );
        state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
        state.current_msg_id = state.user_msg_id + 1;
        state.turn_start_offset = Some(1);
        state.last_offset = 2;
        assert!(jsonl_terminal_can_confirm_completion(Some(&state)));

        state.runtime_kind = None;
        assert!(!jsonl_terminal_can_confirm_completion(Some(&state)));

        state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
        state.user_msg_id = 0;
        assert!(!jsonl_terminal_can_confirm_completion(Some(&state)));

        state.user_msg_id = 9001;
        state.current_msg_id = 0;
        assert!(!jsonl_terminal_can_confirm_completion(Some(&state)));

        state.current_msg_id = 9002;
        state.rebind_origin = true;
        assert!(!jsonl_terminal_can_confirm_completion(Some(&state)));
    }

    #[test]
    fn jsonl_terminal_completion_accepts_monitor_auto_turn_shape() {
        let mut state = state_for_matched_session(
            ProviderKind::Claude,
            "AgentDesk-claude-monitor-relay",
            "/tmp/monitor-auto-turn.jsonl",
        );
        state.turn_source = crate::services::discord::inflight::TurnSource::MonitorTriggered;
        state.rebind_origin = true;
        state.user_msg_id = 0;
        state.current_msg_id = 0;
        state.status_message_id = None;

        assert!(jsonl_terminal_can_confirm_completion(Some(&state)));
    }

    #[test]
    fn jsonl_terminal_completion_accepts_external_adopted_shape_without_turn_source_branch() {
        let mut state = state_for_matched_session(
            ProviderKind::Claude,
            "AgentDesk-claude-external-adopted",
            "/tmp/external-adopted.jsonl",
        );
        state.turn_source = crate::services::discord::inflight::TurnSource::ExternalAdopted;
        state.rebind_origin = true;
        state.user_msg_id = 0;
        state.current_msg_id = 0;
        state.status_message_id = None;
        assert!(jsonl_terminal_can_confirm_completion(Some(&state)));

        state.turn_source = crate::services::discord::inflight::TurnSource::Managed;
        assert!(
            jsonl_terminal_can_confirm_completion(Some(&state)),
            "completion eligibility is defined by the session-bound inflight shape, not turn_source"
        );
    }

    #[test]
    fn session_bound_terminal_delivery_delegation_uses_inflight_shape() {
        let tmux_session_name = "AgentDesk-claude-session-bound";
        let mut state =
            state_for_matched_session(ProviderKind::Claude, tmux_session_name, "/tmp/out.jsonl");
        state.rebind_origin = true;
        state.user_msg_id = 0;
        state.current_msg_id = 0;

        assert!(session_bound_relay_should_own_terminal_delivery(
            true,
            true,
            true,
            Some(tmux_session_name),
            Some(&state),
            tmux_session_name,
        ));
        assert!(!session_bound_relay_should_own_terminal_delivery(
            false,
            true,
            true,
            Some(tmux_session_name),
            Some(&state),
            tmux_session_name,
        ));
        assert!(!session_bound_relay_should_own_terminal_delivery(
            true,
            false,
            true,
            Some(tmux_session_name),
            Some(&state),
            tmux_session_name,
        ));
        assert!(!session_bound_relay_should_own_terminal_delivery(
            true,
            true,
            false,
            Some(tmux_session_name),
            Some(&state),
            tmux_session_name,
        ));
        assert!(!session_bound_relay_should_own_terminal_delivery(
            true,
            true,
            true,
            Some("AgentDesk-claude-other"),
            Some(&state),
            tmux_session_name,
        ));
        assert!(
            session_bound_relay_should_own_terminal_delivery(
                true,
                true,
                true,
                Some(tmux_session_name),
                None,
                tmux_session_name,
            ),
            "matched session binding is enough for session relay ownership; inflight only selects edit metadata"
        );

        state.rebind_origin = false;
        state.user_msg_id = 9001;
        state.current_msg_id = 9001;
        assert!(
            !session_bound_relay_should_own_terminal_delivery(
                true,
                true,
                true,
                Some(tmux_session_name),
                Some(&state),
                tmux_session_name,
            ),
            "bridge-owned inflight remains on legacy/bridge delivery instead of the session relay sink"
        );
    }

    #[test]
    fn post_terminal_jsonl_payload_allows_external_init_without_user_event() {
        let payload = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\",\"tools\":[\"ScheduleWakeup\"]}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"[E2E:E13:WAKE]\"}]}}\n",
            "{\"type\":\"result\",\"result\":\"[E2E:E13:WAKE]\"}\n"
        );
        assert!(post_terminal_jsonl_payload_contains_init_without_user_event(payload.as_bytes()));
    }

    #[test]
    fn post_terminal_jsonl_payload_rejects_active_tool_result() {
        let payload = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\",\"tools\":[\"ScheduleWakeup\"]}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"ScheduleWakeup\"}]}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"scheduled\"}]}}\n",
            "{\"type\":\"result\",\"result\":\"setup complete\"}\n"
        );
        assert!(!post_terminal_jsonl_payload_contains_init_without_user_event(payload.as_bytes()));
    }

    #[tokio::test]
    async fn session_bound_relay_ack_success_commits_and_failure_outcomes_do_not() {
        let metrics =
            std::sync::Arc::new(crate::services::cluster::stream_relay::RelayMetrics::default());
        let target = SessionBoundRelayAckTarget {
            metrics: metrics.clone(),
            sequence: 7,
        };
        assert_eq!(
            wait_for_session_bound_relay_delivery_ack(
                Some(&target),
                std::time::Duration::from_millis(1),
            )
            .await,
            SessionBoundRelayAckOutcome::TimedOut
        );

        metrics.record_sink_error_sequence_for_test(7);
        assert_eq!(
            session_bound_relay_ack_snapshot_outcome(Some(&target)),
            Some(SessionBoundRelayAckOutcome::SinkError)
        );

        let dropped_metrics =
            std::sync::Arc::new(crate::services::cluster::stream_relay::RelayMetrics::default());
        let dropped_target = SessionBoundRelayAckTarget {
            metrics: dropped_metrics.clone(),
            sequence: 9,
        };
        dropped_metrics.record_dropped_sequence_for_test(9);
        assert_eq!(
            session_bound_relay_ack_snapshot_outcome(Some(&dropped_target)),
            Some(SessionBoundRelayAckOutcome::Dropped)
        );

        let skipped_metrics =
            std::sync::Arc::new(crate::services::cluster::stream_relay::RelayMetrics::default());
        let skipped_target = SessionBoundRelayAckTarget {
            metrics: skipped_metrics.clone(),
            sequence: 11,
        };
        skipped_metrics.record_terminal_skipped_sequence_for_test(11);
        assert_eq!(
            session_bound_relay_ack_snapshot_outcome(Some(&skipped_target)),
            Some(SessionBoundRelayAckOutcome::TerminalSkipped)
        );

        let delivered_metrics =
            std::sync::Arc::new(crate::services::cluster::stream_relay::RelayMetrics::default());
        let delivered_target = SessionBoundRelayAckTarget {
            metrics: delivered_metrics.clone(),
            sequence: 3,
        };
        delivered_metrics.record_delivered_sequence_for_test(3);
        assert_eq!(
            wait_for_session_bound_relay_delivery_ack(
                Some(&delivered_target),
                std::time::Duration::from_millis(1),
            )
            .await,
            SessionBoundRelayAckOutcome::TimedOut,
            "frame delivery ack alone must not count as terminal Discord commit"
        );
        delivered_metrics.record_terminal_committed_sequence_for_test(3);
        delivered_metrics.record_sink_error_sequence_for_test(4);
        assert_eq!(
            wait_for_session_bound_relay_delivery_ack(
                Some(&delivered_target),
                std::time::Duration::from_millis(1),
            )
            .await,
            SessionBoundRelayAckOutcome::Delivered
        );
        assert_eq!(
            wait_for_session_bound_relay_delivery_ack(None, std::time::Duration::from_millis(1))
                .await,
            SessionBoundRelayAckOutcome::MissingTarget
        );
    }

    #[test]
    fn session_bound_direct_fallback_selects_full_provider_body_over_tail_suffix() {
        let body = format!(
            "[E2E:E15:BEGIN]\n{}\n[E2E:E15:MID]\n{}\n[E2E:E15:END]",
            (1..=149)
                .map(|n| format!("E15-LINE-{n:03}\n"))
                .collect::<String>(),
            (150..=160)
                .map(|n| format!("E15-LINE-{n:03}\n"))
                .collect::<String>()
        );
        let tail_offset = body.find("E15-LINE-150").expect("tail marker");

        let ordinary_watcher_suffix =
            watcher_terminal_response_for_direct_send(&body, tail_offset, false);
        assert!(!ordinary_watcher_suffix.contains("[E2E:E15:BEGIN]"));
        assert!(ordinary_watcher_suffix.contains("E15-LINE-150"));
        assert!(ordinary_watcher_suffix.contains("[E2E:E15:END]"));

        let session_bound_fallback =
            watcher_terminal_response_for_direct_send(&body, tail_offset, true);
        assert!(session_bound_fallback.contains("[E2E:E15:BEGIN]"));
        assert!(session_bound_fallback.contains("[E2E:E15:MID]"));
        assert!(session_bound_fallback.contains("E15-LINE-150"));
        assert!(session_bound_fallback.contains("[E2E:E15:END]"));
        assert_eq!(session_bound_fallback, body);
    }

    #[test]
    fn session_bound_full_body_fallback_uses_ordered_chunks_for_long_placeholder_response() {
        let body = format!(
            "[E2E:E15:BEGIN]\n{}\n[E2E:E15:MID]\n{}\n[E2E:E15:END]",
            "E15-LINE-010\n".repeat(90),
            "E15-LINE-150\n".repeat(90)
        );

        assert!(body.len() > crate::services::discord::DISCORD_MSG_LIMIT);
        assert!(watcher_should_send_ordered_new_chunks_for_terminal_fallback(true, &body));
        assert!(!watcher_should_send_ordered_new_chunks_for_terminal_fallback(false, &body));
        assert!(
            !watcher_should_send_ordered_new_chunks_for_terminal_fallback(
                true,
                "E15-LINE-150\n[E2E:E15:END]"
            )
        );
    }

    #[test]
    fn frame_accepted_without_terminal_commit_uses_watcher_direct_fallback() {
        assert!(watcher_should_direct_send_after_session_bound_ack(
            true,
            SessionBoundRelayAckOutcome::TimedOut
        ));
        assert!(watcher_should_direct_send_after_session_bound_ack(
            true,
            SessionBoundRelayAckOutcome::TerminalSkipped
        ));
        assert!(watcher_should_direct_send_after_session_bound_ack(
            true,
            SessionBoundRelayAckOutcome::MissingTarget
        ));
        assert!(!watcher_should_direct_send_after_session_bound_ack(
            true,
            SessionBoundRelayAckOutcome::Delivered
        ));
        assert!(!watcher_should_direct_send_after_session_bound_ack(
            false,
            SessionBoundRelayAckOutcome::TimedOut
        ));
    }

    #[test]
    fn session_sink_route_skip_uses_watcher_direct_fallback() {
        assert!(watcher_should_direct_send_after_session_bound_ack(
            true,
            SessionBoundRelayAckOutcome::SinkError
        ));
        assert!(watcher_should_direct_send_after_session_bound_ack(
            true,
            SessionBoundRelayAckOutcome::TerminalSkipped
        ));
        assert!(watcher_should_direct_send_after_session_bound_ack(
            true,
            SessionBoundRelayAckOutcome::Dropped
        ));
        assert!(!watcher_should_direct_send_after_session_bound_ack(
            true,
            SessionBoundRelayAckOutcome::Delivered
        ));
    }

    #[test]
    fn missing_matched_session_jsonl_is_unknown_for_existing_inflight() {
        let missing_path = std::env::temp_dir().join(format!(
            "agentdesk-missing-external-jsonl-{}-{}.jsonl",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_file(&missing_path);
        let tmux_session_name = "AgentDesk-claude-relay-test";
        let state = state_for_matched_session(
            ProviderKind::Claude,
            tmux_session_name,
            &missing_path.display().to_string(),
        );

        assert_eq!(
            matched_session_jsonl_turn_state(
                &ProviderKind::Claude,
                Some(&state),
                tmux_session_name
            ),
            Some(crate::services::tui_turn_state::TuiTurnState::Unknown)
        );
    }
}

fn watcher_tui_gate_blocks_lifecycle(
    gate_outcome: TuiCompletionGateOutcome,
    terminal_delivery_committed: bool,
) -> bool {
    matches!(gate_outcome, TuiCompletionGateOutcome::TimedOut) && !terminal_delivery_committed
}

fn watcher_commit_should_advance_runtime_binding(
    terminal_output_committed: bool,
    gate_outcome: TuiCompletionGateOutcome,
    terminal_delivery_committed: bool,
) -> bool {
    terminal_output_committed
        && !watcher_tui_gate_blocks_lifecycle(gate_outcome, terminal_delivery_committed)
}

fn mark_watcher_terminal_delivery_committed(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    expected_identity: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
    full_response: &str,
    turn_data_start_offset: u64,
    generation_mtime_ns: Option<i64>,
    last_offset: u64,
) -> bool {
    let Some(expected_identity) = expected_identity else {
        return false;
    };
    if expected_identity.user_msg_id == 0 || full_response.trim().is_empty() {
        return false;
    }
    let Some(mut inflight) =
        crate::services::discord::inflight::load_inflight_state(provider, channel_id.get())
    else {
        return false;
    };
    if inflight.restart_mode.is_some() || inflight.rebind_origin {
        return false;
    }
    if inflight.user_msg_id != expected_identity.user_msg_id
        || inflight.started_at.as_str() != expected_identity.started_at.as_str()
        || inflight.tmux_session_name.as_deref() != expected_identity.tmux_session_name.as_deref()
        || inflight.tmux_session_name.as_deref() != Some(tmux_session_name)
    {
        return false;
    }

    inflight.terminal_delivery_committed = true;
    inflight.full_response = full_response.to_string();
    inflight.response_sent_offset = full_response.len();
    inflight.last_offset = last_offset;
    inflight.last_watcher_relayed_offset = Some(turn_data_start_offset);
    inflight.last_watcher_relayed_generation_mtime_ns = generation_mtime_ns;

    match crate::services::discord::inflight::save_inflight_state(&inflight) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id.get(),
                tmux_session = %tmux_session_name,
                error = %error,
                "watcher failed to mirror committed terminal delivery into inflight state"
            );
            false
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WatcherTerminalCommitSideEffects {
    advance_runtime_binding: bool,
    advance_confirmed_end: bool,
    clear_inflight: bool,
    finish_restored_turn: bool,
    late_output_retry_possible: bool,
}

#[cfg(test)]
fn watcher_terminal_commit_side_effects_for_test(
    terminal_output_committed: bool,
    gate_outcome: TuiCompletionGateOutcome,
    terminal_delivery_committed: bool,
) -> WatcherTerminalCommitSideEffects {
    let lifecycle_allowed = terminal_output_committed
        && !watcher_tui_gate_blocks_lifecycle(gate_outcome, terminal_delivery_committed);
    WatcherTerminalCommitSideEffects {
        advance_runtime_binding: watcher_commit_should_advance_runtime_binding(
            terminal_output_committed,
            gate_outcome,
            terminal_delivery_committed,
        ),
        advance_confirmed_end: lifecycle_allowed,
        clear_inflight: lifecycle_allowed,
        finish_restored_turn: lifecycle_allowed,
        late_output_retry_possible: terminal_output_committed && !lifecycle_allowed,
    }
}

fn watcher_terminal_kind_requires_tui_completion_gate(
    terminal_kind: Option<WatcherTerminalKind>,
) -> bool {
    !matches!(terminal_kind, Some(WatcherTerminalKind::SoftUserBoundary))
}

fn missing_inflight_after_session_bound_delivery(
    inflight_missing: bool,
    session_bound_relay_delivered: bool,
) -> bool {
    inflight_missing && !session_bound_relay_delivered
}

#[cfg(test)]
mod runtime_binding_offset_tests {
    use super::*;

    #[test]
    fn committed_watcher_output_advances_runtime_binding_even_without_inflight() {
        assert!(watcher_commit_should_advance_runtime_binding(
            true,
            TuiCompletionGateOutcome::ConfirmedIdle,
            false,
        ));
    }

    #[test]
    fn uncommitted_watcher_output_does_not_advance_runtime_binding() {
        assert!(!watcher_commit_should_advance_runtime_binding(
            false,
            TuiCompletionGateOutcome::ConfirmedIdle,
            false,
        ));
    }

    #[test]
    fn tui_timeout_without_delivery_keeps_previous_runtime_binding() {
        assert!(!watcher_commit_should_advance_runtime_binding(
            true,
            TuiCompletionGateOutcome::TimedOut,
            false,
        ));
    }

    #[test]
    fn tui_completion_gate_timeout_without_terminal_delivery_preserves_cleanup_for_retry() {
        let side_effects = watcher_terminal_commit_side_effects_for_test(
            true,
            TuiCompletionGateOutcome::TimedOut,
            false,
        );

        assert!(!side_effects.advance_runtime_binding);
        assert!(!side_effects.advance_confirmed_end);
        assert!(!side_effects.clear_inflight);
        assert!(!side_effects.finish_restored_turn);
        assert!(side_effects.late_output_retry_possible);

        let confirmed = watcher_terminal_commit_side_effects_for_test(
            true,
            TuiCompletionGateOutcome::ConfirmedIdle,
            false,
        );
        assert!(confirmed.advance_runtime_binding);
        assert!(confirmed.advance_confirmed_end);
        assert!(confirmed.clear_inflight);
        assert!(confirmed.finish_restored_turn);
        assert!(!confirmed.late_output_retry_possible);
    }

    #[test]
    fn tui_completion_gate_timeout_after_terminal_delivery_allows_lifecycle_cleanup() {
        let side_effects = watcher_terminal_commit_side_effects_for_test(
            true,
            TuiCompletionGateOutcome::TimedOut,
            true,
        );

        assert!(side_effects.advance_runtime_binding);
        assert!(side_effects.advance_confirmed_end);
        assert!(side_effects.clear_inflight);
        assert!(side_effects.finish_restored_turn);
        assert!(!side_effects.late_output_retry_possible);
    }

    #[test]
    fn soft_user_boundary_terminal_skips_tui_completion_gate() {
        assert!(!watcher_terminal_kind_requires_tui_completion_gate(Some(
            WatcherTerminalKind::SoftUserBoundary
        )));
        assert!(watcher_terminal_kind_requires_tui_completion_gate(Some(
            WatcherTerminalKind::SoftStopHookSummary
        )));
        assert!(watcher_terminal_kind_requires_tui_completion_gate(Some(
            WatcherTerminalKind::HardResult
        )));
        assert!(watcher_terminal_kind_requires_tui_completion_gate(None));
    }

    #[test]
    fn acknowledged_session_bound_delivery_is_not_missing_inflight_fallback() {
        assert!(!missing_inflight_after_session_bound_delivery(true, true));
        assert!(missing_inflight_after_session_bound_delivery(true, false));
        assert!(!missing_inflight_after_session_bound_delivery(false, false));
    }
}

pub(in crate::services::discord) async fn run_tui_completion_gate(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    task_notification_kind: Option<crate::services::agent_protocol::TaskNotificationKind>,
) -> TuiCompletionGateOutcome {
    let inflight =
        crate::services::discord::inflight::load_inflight_state(provider, channel_id.get());
    if jsonl_terminal_can_confirm_completion(inflight.as_ref())
        && matched_session_jsonl_turn_state(provider, inflight.as_ref(), tmux_session_name)
            == Some(crate::services::tui_turn_state::TuiTurnState::Idle)
    {
        tracing::info!(
            provider = %provider.as_str(),
            channel = channel_id.get(),
            tmux_session = %tmux_session_name,
            "confirmed matched session completion from provider JSONL terminal envelope"
        );
        return TuiCompletionGateOutcome::ConfirmedIdle;
    }
    let runtime_kind = inflight.as_ref().and_then(|state| state.runtime_kind);
    let rebind_origin = inflight
        .as_ref()
        .map(|state| state.rebind_origin)
        .unwrap_or(false);

    if !crate::services::discord::tmux::should_gate_completion_for_tui_quiescence(
        runtime_kind,
        rebind_origin,
        task_notification_kind,
    ) {
        return TuiCompletionGateOutcome::NotGated;
    }
    let tmux_session_for_liveness = tmux_session_name.to_string();
    let pane_alive = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            crate::services::tmux_diagnostics::tmux_session_has_live_pane(
                &tmux_session_for_liveness,
            )
        }),
    )
    .await
    .unwrap_or(Ok(false))
    .unwrap_or(false);
    if !pane_alive {
        tracing::info!(
            provider = %provider.as_str(),
            channel = channel_id.get(),
            tmux_session = %tmux_session_name,
            "TUI completion gate skipped because tmux pane is no longer live"
        );
        return TuiCompletionGateOutcome::SkippedDead;
    }

    let started_at = tokio::time::Instant::now();
    loop {
        let ready = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::task::spawn_blocking({
                let provider = provider.clone();
                let tmux_session_name = tmux_session_name.to_string();
                let inflight = inflight.clone();
                move || {
                    matched_session_structured_ready_for_input(
                        &provider,
                        inflight.as_ref(),
                        &tmux_session_name,
                    )
                    .is_some_and(crate::services::tui_turn_state::TuiReadyState::is_ready)
                }
            }),
        )
        .await
        .unwrap_or(Ok(false))
        .unwrap_or(false);

        if ready {
            return TuiCompletionGateOutcome::ConfirmedIdle;
        }
        if started_at.elapsed() >= crate::services::discord::tmux::TUI_COMPLETION_QUIESCENCE_TIMEOUT
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id.get(),
                tmux_session = %tmux_session_name,
                gate = "tui_completion_quiescence",
                "[{ts}] \u{26a0} TUI structured turn state was not idle after {:?} — suppressing turn-complete status to avoid premature completion (#2161); placeholder sweeper / next-turn intake will reconcile",
                crate::services::discord::tmux::TUI_COMPLETION_QUIESCENCE_TIMEOUT,
            );
            return TuiCompletionGateOutcome::TimedOut;
        }
        tokio::time::sleep(crate::services::discord::tmux::TUI_COMPLETION_QUIESCENCE_POLL_INTERVAL)
            .await;
    }
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
    mailbox_finalize_owed: Arc<std::sync::atomic::AtomicBool>,
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
        mailbox_finalize_owed,
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
    mailbox_finalize_owed: Arc<std::sync::atomic::AtomicBool>,
    restored_turn: Option<RestoredWatcherTurn>,
) {
    use std::io::{Read, Seek, SeekFrom};

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 👁 tmux watcher started for #{tmux_session_name} at offset {initial_offset}"
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
    let mut restored_turn = restored_turn;
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

        // Check cancel or global shutdown (both exit quietly, no "session ended" message)
        if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
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
                shared.shutting_down.load(Ordering::Relaxed),
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
                    shared.shutting_down.load(Ordering::Relaxed),
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
                    shared.shutting_down.load(Ordering::Relaxed),
                    probe_tmux_session_liveness(&tmux_session_name).await,
                )),
            )
        } else {
            watcher_output_poll_decision(bytes_available, None)
        };
        match poll_decision {
            WatcherOutputPollDecision::DrainOutput => {}
            WatcherOutputPollDecision::Continue => {
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
        let post_terminal_no_inflight_should_suppress =
            should_suppress_post_terminal_output_without_inflight(
                turn_result_relayed,
                post_terminal_inflight_missing,
                ssh_direct_prompt_pending,
                external_input_lease_present,
                watcher_batch_contains_assistant_event(&data),
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
        let data_mirrored_to_session_relay = if decoded_data.text.is_empty() {
            SupervisorRelayForward::mirrored_without_ack()
        } else {
            // E5 (#2412): mirror the freshly-read chunk into the
            // supervisor-owned StreamRelay if one exists for this session.
            // This is the *producer* side of the supervisor pipeline —
            // without this call, `try_send_frame` is never invoked in
            // production. The Discord sink consumes these frames directly for
            // eligible session-bound inflight shapes; this watcher remains the
            // fallback for bridge-owned/no-inflight envelopes.
            forward_chunk_to_supervisor_relay(
                &tmux_session_name,
                &decoded_data.text,
                &producer_registry,
                &mut cached_relay_producer,
            )
        };
        if let Some(ack_target) = data_mirrored_to_session_relay.ack_target.clone() {
            all_data_session_bound_relay_ack = Some(ack_target);
        }
        if all_data.is_empty() {
            all_data_start_offset = decoded_data.start_offset.unwrap_or(data_start_offset);
            all_data_fully_mirrored_to_session_relay = data_mirrored_to_session_relay.mirrored;
        } else {
            all_data_fully_mirrored_to_session_relay &= data_mirrored_to_session_relay.mirrored;
        }
        if decoded_data.text.is_empty() && all_data.is_empty() {
            continue;
        }
        all_data.push_str(&decoded_data.text);
        let turn_data_start_offset = all_data_start_offset;
        let mut session_bound_relay_turn_fully_mirrored = all_data_fully_mirrored_to_session_relay;
        let mut state = StreamLineState::new();
        let restored_turn_seed = restored_turn.take();
        let discard_restored_seed = should_discard_restored_seed_for_idle_direct_prompt(
            restored_turn_seed.is_some(),
            crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
                watcher_provider.as_str(),
                &tmux_session_name,
                channel_id.get(),
            )
            .is_some(),
        );
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
        if restored_assistant_text_seen {
            // The restored response prefix came from watcher state, not from
            // chunks mirrored into the session-bound StreamRelay parser. Keep
            // the legacy watcher delivery owner for this terminal envelope so
            // we do not delegate a partial response.
            session_bound_relay_turn_fully_mirrored = false;
        }
        let mut full_response = stream_seed.full_response;
        let mut tool_state = WatcherToolState::new();

        // Create a placeholder message for real-time status display
        const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let mut spin_idx: usize = 0;
        let mut placeholder_msg_id: Option<serenity::MessageId> = stream_seed.placeholder_msg_id;
        let mut placeholder_from_restored_inflight = placeholder_msg_id.is_some();
        let mut status_panel_msg_id: Option<serenity::MessageId> = stream_seed.status_panel_msg_id;
        let mut last_status_panel_text = String::new();
        let status_panel_started_at = chrono::Utc::now().timestamp();
        let mut last_edit_text = stream_seed.last_edit_text;
        let mut response_sent_offset = stream_seed.response_sent_offset;
        let finish_mailbox_on_completion = stream_seed.finish_mailbox_on_completion;
        let mut monitor_auto_turn_claimed = false;
        let mut monitor_auto_turn_deferred = false;
        let mut monitor_auto_turn_finished = false;
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
            if !start.acquired {
                all_data.clear();
                all_data_start_offset = current_offset;
                all_data_fully_mirrored_to_session_relay = true;
                all_data_session_bound_relay_ack = None;
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
            continue;
        }
        if !found_result {
            let turn_start = tokio::time::Instant::now();
            let turn_timeout = crate::services::discord::turn_watchdog_timeout();
            let mut last_status_update = tokio::time::Instant::now();
            let mut last_output_at = tokio::time::Instant::now();
            if live_events_dirty {
                force_next_watcher_status_update(&mut last_status_update);
            }
            let mut ready_for_input_tracker =
                crate::services::provider::ReadyForInputIdleTracker::default();
            let mut last_ready_probe_at: Option<std::time::Instant> = None;
            let mut last_liveness_probe_at = tokio::time::Instant::now();
            let mut tmux_death_observed = false;
            let mut ready_for_input_failure_notice: Option<String> = None;
            let mut ready_for_input_stall_dispatch_id: Option<String> = None;
            let mut streaming_suppressed_by_recent_stop = false;
            let mut streaming_suppressed_by_missing_inflight = false;
            let mut fresh_ready_for_input_idle = false;

            while !found_result && turn_start.elapsed() < turn_timeout {
                // The inner loop can wait for minutes while a long tool/test
                // produces no provider JSONL result. Keep the registry
                // heartbeat fresh so the heartbeat sweeper does not mistake a
                // healthy streaming watcher for a dead task and cancel relay.
                last_heartbeat_ts_ms.store(
                    crate::services::discord::tmux_watcher_now_ms(),
                    std::sync::atomic::Ordering::Release,
                );
                if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
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
                        let chunk_forwarded_to_session_relay = if decoded_chunk.text.is_empty() {
                            SupervisorRelayForward::mirrored_without_ack()
                        } else {
                            // E5 (#2412): producer-side wiring for the
                            // supervisor-owned StreamRelay. Same rationale as
                            // the outer read site in this fn — every decoded
                            // chunk read off the tmux output file is also
                            // pushed into the relay's MPSC so the
                            // session-bound Discord sink receives frames in
                            // production.
                            forward_chunk_to_supervisor_relay(
                                &tmux_session_name,
                                &decoded_chunk.text,
                                &producer_registry,
                                &mut cached_relay_producer,
                            )
                        };
                        if let Some(ack_target) = chunk_forwarded_to_session_relay.ack_target {
                            all_data_session_bound_relay_ack = Some(ack_target);
                        }
                        let chunk_mirrored_to_session_relay =
                            chunk_forwarded_to_session_relay.mirrored;
                        session_bound_relay_turn_fully_mirrored &= chunk_mirrored_to_session_relay;
                        if all_data.is_empty() {
                            all_data_start_offset =
                                decoded_chunk.start_offset.unwrap_or(chunk_start_offset);
                            all_data_fully_mirrored_to_session_relay =
                                chunk_mirrored_to_session_relay;
                        } else {
                            all_data_fully_mirrored_to_session_relay &=
                                chunk_mirrored_to_session_relay;
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
                        last_output_at = tokio::time::Instant::now();
                        all_data_start_offset = advance_buffer_start_offset(
                            chunk_buffer_start_offset,
                            chunk_buffer_len,
                            all_data.len(),
                        );
                        if flush_placeholder_live_events(&shared, channel_id, &mut tool_state) {
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
                                    shared.shutting_down.load(Ordering::Relaxed),
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
                        // #2442 (H3) — wrapper emits a `ready_for_input`
                        // JSONL sentinel as soon as it transitions back to
                        // accepting stdin. If we see the sentinel in the
                        // tail bytes, treat it as a free readiness signal
                        // and short-circuit the 2s probe cadence. The
                        // legacy `should_probe_ready` cadence stays as a
                        // fallback for the SIGKILL / sentinel-lost case.
                        //
                        // Claude TUI is transcript-backed: its visible
                        // composer can stay on-screen during active work, so
                        // watcher completion must use the JSONL turn state,
                        // not pane chrome.
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
                                    let dispatch_id = resolve_dispatched_thread_dispatch_from_db(
                                        None::<&crate::db::Db>,
                                        shared.pg_pool.as_ref(),
                                        watcher_thread_channel_id.unwrap_or_else(|| channel_id.get()),
                                    )
                                    .or_else(|| {
                                        crate::services::discord::inflight::load_inflight_state(
                                            &watcher_provider,
                                            channel_id.get(),
                                        )
                                        .and_then(|state| state.dispatch_id)
                                    });
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

                    if shared.status_panel_v2_enabled
                        && let Some(status_msg_id) = status_panel_msg_id
                    {
                        let panel_text = shared.placeholder_live_events.render_status_panel(
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
                    if should_skip_streaming_placeholder_without_inflight(
                        inflight_missing_for_streaming,
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
                        continue;
                    }

                    loop {
                        let current_portion =
                            full_response.get(response_sent_offset..).unwrap_or("");
                        if current_portion.is_empty() {
                            break;
                        }

                        let status_block = build_watcher_placeholder_status_block(
                            &shared,
                            channel_id,
                            indicator,
                            tool_state.prev_tool_status.as_deref(),
                            tool_state.current_tool_line.as_deref(),
                            &full_response,
                            status_panel_msg_id,
                        );
                        let Some(msg_id) = placeholder_msg_id else {
                            break;
                        };
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
                                        placeholder_msg_id = Some(message.id);
                                        placeholder_from_restored_inflight = false;
                                        response_sent_offset += plan.split_at;
                                        last_edit_text = status_block;
                                        persist_watcher_stream_progress(
                                            &watcher_provider,
                                            channel_id,
                                            &tmux_session_name,
                                            placeholder_msg_id,
                                            &full_response,
                                            response_sent_offset,
                                            tool_state.current_tool_line.as_deref(),
                                            tool_state.prev_tool_status.as_deref(),
                                            task_notification_kind,
                                            tool_state.any_tool_used,
                                            tool_state.has_post_tool_text,
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

                    let status_block = build_watcher_placeholder_status_block(
                        &shared,
                        channel_id,
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
                        shared.status_panel_v2_enabled,
                        current_portion,
                        &status_block,
                        &watcher_provider,
                    );

                    if display_text != last_edit_text {
                        match placeholder_msg_id {
                            Some(msg_id) => {
                                // Edit existing placeholder
                                rate_limit_wait(&shared, channel_id).await;
                                let _ = crate::services::discord::http::edit_channel_message(
                                    &http,
                                    channel_id,
                                    msg_id,
                                    &display_text,
                                )
                                .await;
                            }
                            None => {
                                // Create new placeholder
                                if let Ok(msg) =
                                    crate::services::discord::http::send_channel_message(
                                        &http,
                                        channel_id,
                                        &display_text,
                                    )
                                    .await
                                {
                                    placeholder_msg_id = Some(msg.id);
                                    placeholder_from_restored_inflight = false;
                                }
                            }
                        }
                        last_edit_text = display_text;
                        persist_watcher_stream_progress(
                            &watcher_provider,
                            channel_id,
                            &tmux_session_name,
                            placeholder_msg_id,
                            &full_response,
                            response_sent_offset,
                            tool_state.current_tool_line.as_deref(),
                            tool_state.prev_tool_status.as_deref(),
                            task_notification_kind,
                            tool_state.any_tool_used,
                            tool_state.has_post_tool_text,
                        );
                    }
                }
            }

            if fresh_ready_for_input_idle {
                let delegated_finalize_owed_pending =
                    mailbox_finalize_owed.load(std::sync::atomic::Ordering::Acquire);
                if watcher_should_defer_delegated_fresh_idle(
                    delegated_finalize_owed_pending,
                    &full_response,
                ) {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 👁 watcher observed fresh ready-for-input idle for {tmux_session_name} at offset {current_offset}, but bridge-delegated turn has no terminal assistant text yet; preserving inflight and waiting for terminal commit"
                    );
                    all_data.clear();
                    all_data_start_offset = current_offset;
                    all_data_fully_mirrored_to_session_relay = true;
                    all_data_session_bound_relay_ack = None;
                    last_observed_generation_mtime_ns =
                        Some(read_generation_file_mtime_ns(&tmux_session_name));
                    finish_monitor_auto_turn_if_claimed(
                        &shared,
                        &watcher_provider,
                        channel_id,
                        &mut monitor_auto_turn_claimed,
                        &mut monitor_auto_turn_finished,
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
                let owed = mailbox_finalize_owed.swap(false, std::sync::atomic::Ordering::AcqRel);
                let should_finish_mailbox = finish_mailbox_on_completion || owed;
                if should_finish_mailbox {
                    crate::services::discord::inflight::clear_inflight_state(
                        &watcher_provider,
                        channel_id.get(),
                    );
                    crate::services::observability::emit_inflight_lifecycle_event(
                        watcher_provider.as_str(),
                        channel_id.get(),
                        None,
                        None,
                        None,
                        "cleared_by_watcher_fresh_idle",
                        serde_json::json!({
                            "owed_finalize": owed,
                            "finish_mailbox_on_completion": finish_mailbox_on_completion,
                            "tmux_session": tmux_session_name.as_str(),
                            "offset": current_offset,
                        }),
                    );
                    finish_restored_watcher_active_turn(
                        &shared,
                        &watcher_provider,
                        channel_id,
                        finish_mailbox_on_completion,
                        owed,
                        true,
                        "watcher fresh ready-for-input idle with queued backlog",
                    )
                    .await;
                }
                all_data.clear();
                all_data_start_offset = current_offset;
                all_data_fully_mirrored_to_session_relay = true;
                all_data_session_bound_relay_ack = None;
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
                break 'watcher_loop;
            }

            if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
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
                            if let Some(state) =
                                crate::services::discord::inflight::load_inflight_state(
                                    &watcher_provider,
                                    channel_id.get(),
                                )
                                .filter(|state| !state.rebind_origin)
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
                            crate::services::discord::inflight::clear_inflight_state(
                                &watcher_provider,
                                channel_id.get(),
                            );
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
                )
                .await;
                continue;
            }
        }

        // If paused was set while we were reading (even if already unpaused), discard partial data.
        // Also check epoch: if it changed, a Discord turn claimed this data even if paused is now false.
        let paused_now = paused.load(Ordering::Relaxed);
        let epoch_changed_now = pause_epoch.load(Ordering::Relaxed) != epoch_snapshot;
        let deferred_monitor_ready =
            monitor_auto_turn_claimed && monitor_auto_turn_deferred && !paused_now;
        if (was_paused || paused_now || epoch_changed_now) && !deferred_monitor_ready {
            // Clean up placeholder if we created one
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
            )
            .await;
            all_data.clear();
            all_data_start_offset = current_offset;
            all_data_fully_mirrored_to_session_relay = true;
            all_data_session_bound_relay_ack = None;
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
                )
                .await;
                continue;
            }
            // #897 round-3 Medium: skip reaction work for `rebind_origin`
            // inflights — their `user_msg_id=0` identifies no real Discord
            // message so issuing reactions against it just produces API
            // errors. The synthetic state was created by
            // `/api/inflight/rebind` to adopt a live tmux session.
            if let Some(state) = inflight_state.as_ref().filter(|s| !s.rebind_origin) {
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
            crate::services::discord::inflight::clear_inflight_state(
                &watcher_provider,
                channel_id.get(),
            );
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
                )
                .await;
                continue;
            }

            // #897 round-3 Medium: skip reaction + retry scheduling for
            // `rebind_origin` inflights — they have no real user message
            // to react against and no real user text to re-prompt.
            if let Some(state) = inflight_state.as_ref().filter(|s| !s.rebind_origin) {
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
            crate::services::discord::inflight::clear_inflight_state(
                &watcher_provider,
                channel_id.get(),
            );

            match decision {
                ProviderOverloadDecision::Retry {
                    attempt,
                    delay,
                    fingerprint,
                } => {
                    if let Some(retry_text) = retry_text {
                        if let Some(state) = inflight_state.as_ref().filter(|s| !s.rebind_origin) {
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
            )
            .await;
            discard_watcher_pending_buffer_after_suppressed_turn(
                &mut all_data,
                &mut all_data_start_offset,
                &mut all_data_fully_mirrored_to_session_relay,
                &mut all_data_session_bound_relay_ack,
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
            )
            .await;
            discard_watcher_pending_buffer_after_suppressed_turn(
                &mut all_data,
                &mut all_data_start_offset,
                &mut all_data_fully_mirrored_to_session_relay,
                &mut all_data_session_bound_relay_ack,
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
                )
                .await;
                discard_watcher_pending_buffer_after_suppressed_turn(
                    &mut all_data,
                    &mut all_data_start_offset,
                    &mut all_data_fully_mirrored_to_session_relay,
                    &mut all_data_session_bound_relay_ack,
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
                Some(state) if state.rebind_origin => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ Watcher auto-retry skipped for channel {} — rebind_origin inflight has no user message to retry",
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
        let external_input_lease_before_relay =
            crate::services::tui_prompt_dedupe::external_input_relay_lease_present(
                watcher_provider.as_str(),
                &tmux_session_name,
                channel_id.get(),
            );
        let inflight_before_relay = crate::services::discord::inflight::load_inflight_state(
            &watcher_provider,
            channel_id.get(),
        );
        let inflight_identity_before_relay =
            matching_watcher_turn_identity(inflight_before_relay.as_ref(), &tmux_session_name);
        let should_adopt_inflight_terminal_message_ids = !external_input_lease_before_relay
            || watcher_inflight_represents_external_input(inflight_before_relay.as_ref());
        if should_adopt_inflight_terminal_message_ids
            && let Some(inflight) = inflight_before_relay.as_ref()
        {
            adopt_watcher_terminal_message_ids_from_inflight(
                &mut placeholder_msg_id,
                &mut placeholder_from_restored_inflight,
                &mut status_panel_msg_id,
                inflight,
                &tmux_session_name,
            );
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

        let recent_stop_for_output =
            recent_turn_stop_for_watcher_range(channel_id, &tmux_session_name, data_start_offset);
        let inflight_missing_before_relay = inflight_before_relay.is_none();
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
            status_panel_msg_id = None;
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
            )
            .await;
            continue;
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
        let mut session_bound_ack_outcome = SessionBoundRelayAckOutcome::MissingTarget;
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
                session_bound_ack_outcome = ack_outcome;
                let delivered = matches!(ack_outcome, SessionBoundRelayAckOutcome::Delivered);
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
        let watcher_direct_fallback_after_session_bound_ack =
            watcher_should_direct_send_after_session_bound_ack(
                relay_decision.should_direct_send,
                session_bound_ack_outcome,
            );
        let session_bound_fallback_uses_full_body = session_bound_terminal_delivery_attempted
            && watcher_direct_fallback_after_session_bound_ack;
        let direct_terminal_response = watcher_terminal_response_for_direct_send(
            &full_response,
            response_sent_offset,
            session_bound_fallback_uses_full_body,
        );
        let has_direct_terminal_response = !direct_terminal_response.trim().is_empty();
        // #2838 (relay-stability P0-1): count the primary duplicate-emit vector.
        // The 10s session-bound terminal ACK timed out yet the watcher proceeds
        // to direct-send, so the StreamRelay sink may have actually posted (just
        // lagged the committed-sequence metric) and this re-sends the same
        // answer. Rising counts here are the signal that the dual-authority
        // terminal-delivery lease (P1) is overdue.
        if watcher_direct_fallback_after_session_bound_ack
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
            relay_owner_kind = inflight_before_relay
                .as_ref()
                .map(|state| state.effective_relay_owner_kind().as_str())
                .unwrap_or("none"),
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
        let mut watcher_direct_terminal_idle_committed = false;
        let mut tui_direct_anchor_terminal_body_visible = false;
        let mut tui_direct_anchor_or_lease_present_for_lifecycle =
            prompt_anchor_present_before_relay || external_input_lease_before_relay;
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
                if let Some((pk, _)) = parse_provider_and_channel_from_tmux_name(&tmux_session_name)
                {
                    if let Some(mut inflight) =
                        crate::services::discord::inflight::load_inflight_state(
                            &pk,
                            channel_id.get(),
                        )
                    {
                        inflight.last_watcher_relayed_offset = Some(turn_data_start_offset);
                        inflight.last_watcher_relayed_generation_mtime_ns =
                            last_observed_generation_mtime_ns;
                        let _ = crate::services::discord::inflight::save_inflight_state(&inflight);
                    }
                }
            }
            clear_provider_overload_retry_state(channel_id);
            true
        } else if watcher_direct_fallback_after_session_bound_ack {
            let formatted = if shared.status_panel_v2_enabled {
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
                            match crate::services::discord::formatting::send_long_message_raw_with_rollback(
                                &http,
                                channel_id,
                                msg_id,
                                &relay_text,
                                &shared,
                            )
                            .await
                            {
                                Ok(_) => {
                                    direct_send_delivered = true;
                                    tui_direct_anchor_terminal_body_visible = true;
                                    external_input_lease_consumed_by_relay =
                                        watcher_inflight_represents_external_input(
                                            inflight_before_relay.as_ref(),
                                        );
                                    let cleanup = delete_terminal_placeholder(
                                        &http,
                                        channel_id,
                                        &shared,
                                        &watcher_provider,
                                        &tmux_session_name,
                                        msg_id,
                                        "watcher_terminal_relay_full_body_fallback_cleanup",
                                    )
                                    .await;
                                    if cleanup.is_committed() {
                                        placeholder_msg_id = None;
                                        placeholder_from_restored_inflight = false;
                                        last_edit_text.clear();
                                    }
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!(
                                        "  [{ts}] 👁 ✓ relayed full terminal response after session-bound fallback (ordered chunks) channel {} msg {} ({} chars)",
                                        channel_id.get(),
                                        msg_id.get(),
                                        relay_text.len()
                                    );
                                }
                                Err(e) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!(
                                        "  [{ts}] 👁 Failed to relay ordered terminal chunks: {e}"
                                    );
                                    relay_ok = false;
                                }
                            }
                        } else {
                            match replace_long_message_raw_with_outcome(
                                &http,
                                channel_id,
                                msg_id,
                                &relay_text,
                                &shared,
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
                                    placeholder_msg_id = None;
                                    placeholder_from_restored_inflight = false;
                                    last_edit_text.clear();
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
                                    relay_ok = false;
                                    retry_terminal_delivery_from_offset = true;
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
                        if external_input_lease_consumed_by_relay {
                            crate::services::tui_prompt_dedupe::clear_external_input_relay_lease(
                                watcher_provider.as_str(),
                                &tmux_session_name,
                                channel_id.get(),
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
                    // #1270 codex P2: snapshot the current `.generation` mtime
                    // on every successful relay so the local regression check
                    // has a real baseline. Without this, normal relay paths
                    // (which never enter the reset helper) leave the baseline
                    // at None, and any later regression misclassifies
                    // same-wrapper rotation as fresh-respawn — clearing the
                    // local offset and re-relaying surviving bytes.
                    last_observed_generation_mtime_ns =
                        Some(read_generation_file_mtime_ns(&tmux_session_name));
                    // #1134: first successful relay for this attach. The
                    // watcher_latency module is idempotent — only the first
                    // call after `record_attach` actually observes a sample,
                    // so the unconditional call here is safe and cheap.
                    crate::services::observability::watcher_latency::record_first_relay(
                        channel_id.get(),
                    );
                    if let Some((pk, _)) =
                        parse_provider_and_channel_from_tmux_name(&tmux_session_name)
                    {
                        if let Some(mut inflight) =
                            crate::services::discord::inflight::load_inflight_state(
                                &pk,
                                channel_id.get(),
                            )
                        {
                            inflight.last_watcher_relayed_offset = Some(turn_data_start_offset);
                            // #1270: persist the matching `.generation` mtime
                            // alongside the offset so a replacement watcher
                            // (e.g. after dcserver restart) can disambiguate
                            // same-wrapper rotation (mtime unchanged → pin to
                            // EOF) from cancel→respawn (mtime changed → reset
                            // to 0) when restoring this offset.
                            inflight.last_watcher_relayed_generation_mtime_ns =
                                last_observed_generation_mtime_ns;
                            let _ =
                                crate::services::discord::inflight::save_inflight_state(&inflight);
                        }
                    }
                }
                clear_provider_overload_retry_state(channel_id);
            }
            if retry_terminal_delivery_from_offset {
                current_offset = turn_data_start_offset;
                all_data.clear();
                all_data_start_offset = current_offset;
                all_data_fully_mirrored_to_session_relay = true;
                all_data_session_bound_relay_ack = None;
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
                let store_arc = crate::server::routes::state::global_monitoring_store();
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

        // #2161 TUI completion gate: ClaudeTui sessions can land a
        // `result` JSONL event before the interactive pane is actually
        // quiescent. Without this gate the user sees `응답 완료` on
        // Discord while the tmux pane still shows `almost done thinking`
        // and subsequent relay messages continue past the completion
        // marker.
        //
        // On gate timeout (Codex H2) we deliberately do NOT emit
        // `TurnCompleted` — the placeholder sweeper / next-turn intake
        // will close the lingering Active panel rather than mark a hung
        // pane as completed.
        //
        // Codex round-2 H1: the gate outcome is now also threaded into the
        // dispatch finalization step below so a still-busy ClaudeTui pane
        // does not drain queued turns into a busy-followup notice.
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
                // Keep the SSH-direct replay watermark in lockstep with bytes the
                // watcher actually committed. TimedOut gates only keep this as
                // a candidate when the terminal delivery has not been mirrored.
                crate::services::tui_prompt_dedupe::advance_tmux_runtime_binding_offset(
                    &tmux_session_name,
                    &output_path,
                    candidate_offset,
                );
            }
        }
        // #2293 H2 — single boolean threaded through every terminal side
        // effect below. On `TimedOut` before the terminal delivery is durably
        // mirrored, the pane is still busy past the bounded wait, so we must SKIP:
        //   * ✅ reaction on the user message
        //   * session transcript / turn-analytics persist (writes a row that
        //     claims completion at this exact JSONL offset, which is wrong
        //     while output is still being produced)
        //   * history append into the in-memory session
        //   * confirmed-end watermark advance (turn isn't actually done)
        //   * `clear_inflight_state` (intake gate uses inflight presence to
        //     decide whether to admit a new turn — wiping it lets the next
        //     turn race the still-busy pane)
        //   * `finish_restored_watcher_active_turn` (mailbox cancel_token
        //     release for the same reason)
        //   * deferred idle queue kickoff (would push backlog into the busy
        //     pane)
        //   * terminal-finalize stop decision (would stop the watcher while
        //     output is still flowing)
        // Once watcher delivery is durably mirrored, match the bridge path:
        // suppress visible completion on timeout, but allow lifecycle cleanup
        // to release inflight/mailbox state and drain queued follow-ups.
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
                "[{ts}] ⚠ #2293: watcher lifecycle-stage paused — TUI quiescence gate timed out; skipping reaction / transcript / inflight-clear / mailbox-finish side effects until the next pass observes idle"
            );
        }

        if terminal_output_committed && watcher_tui_gate_outcome.should_emit_completion() {
            // #2849: watcher-completed turns never traverse the bridge
            // StatusUpdate path, so the completed panel can lack the Context
            // line even when terminal output carried exact usage. Backfill the
            // exact final context usage onto the panel BEFORE rendering the
            // completed panel. Skip entirely when no exact usage exists or the
            // provider/model has no resolvable window — never fabricate numbers
            // and never reuse stale prior-turn usage. set_context_panel_usage is
            // also internally gated to context_window != 0.
            if shared.status_panel_v2_enabled
                && let Some(usage) = stream_line_state_token_usage(&state)
                    .filter(|usage| usage.context_occupancy_input_tokens() > 0)
            {
                let context_window =
                    watcher_provider.resolve_context_window(state.last_model.as_deref());
                if context_window > 0 {
                    let ctx_cfg = crate::services::discord::adk_session::fetch_context_thresholds(
                        shared.api_port,
                    )
                    .await;
                    shared.placeholder_live_events.set_context_panel_usage(
                        channel_id,
                        state.last_session_id.as_deref(),
                        usage.input_tokens,
                        usage.cache_create_tokens,
                        usage.cache_read_tokens,
                        context_window,
                        ctx_cfg.compact_pct_for(&watcher_provider),
                    );
                }
            }
            // #2427 D wire (Codex round 2 HIGH-1): the watcher loop is not
            // turn-scoped — by the time we reach here a new turn may have
            // rewritten the inflight on disk. Reading user_msg_id from that
            // same file and feeding it back into
            // `clear_inflight_state_if_matches` becomes self-authentication
            // and *enables* the very Pitfall #1 race the guard was meant
            // to prevent. We therefore drop the explicit-signal hook on
            // the watcher D wire and rely exclusively on the unconditional
            // `clear_inflight_state` call at L~2996 (committed-output
            // path). The recovery_engine D wire is preserved because its
            // `state.user_msg_id` is captured from the inflight snapshot
            // pinned at recovery entry, not re-read at completion time.
            let status_panel_completion_user_msg_id =
                inflight_before_relay.as_ref().and_then(|inflight| {
                    let matches_current_watcher_session = inflight
                        .tmux_session_name
                        .as_deref()
                        .map(str::trim)
                        .is_some_and(|name| !name.is_empty() && name == tmux_session_name);
                    if !inflight.rebind_origin
                        && inflight.user_msg_id != 0
                        && matches_current_watcher_session
                    {
                        Some(inflight.user_msg_id)
                    } else {
                        None
                    }
                });
            complete_watcher_status_panel_v2(
                &http,
                &shared,
                channel_id,
                status_panel_msg_id,
                &watcher_provider,
                status_panel_started_at,
                &mut last_status_panel_text,
                matches!(
                    task_notification_kind,
                    Some(TaskNotificationKind::Background | TaskNotificationKind::MonitorAutoTurn)
                ),
                status_panel_completion_user_msg_id,
            )
            .await;
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
        if terminal_output_committed && !lifecycle_stage_paused {
            advance_watcher_confirmed_end(
                &shared,
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                terminal_committed_offset,
                "src/services/discord/tmux.rs:tmux_output_watcher_confirmed_end",
            );
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

        if crate::services::discord::tui_prompt_relay::should_complete_tui_direct_anchor_lifecycle(
            terminal_output_committed,
            tui_direct_anchor_terminal_body_visible,
            tui_direct_anchor_or_lease_present_for_lifecycle,
            lifecycle_stage_paused,
            inflight_state.is_some(),
        ) {
            let _ = crate::services::discord::tui_prompt_relay::complete_tui_direct_prompt_anchor_lifecycle_if_present(
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
        if terminal_output_committed
            && !lifecycle_stage_paused
            && let Some(state) = inflight_state.as_ref().filter(|s| !s.rebind_origin)
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
                            None::<&crate::db::Db>,
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
                    None::<&crate::db::Db>,
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
        } else if let Some(did) = resolved_did.as_deref() {
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
            if has_assistant_response
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
            // #1670/#1708: Always consume the handoff debt and clear inflight
            // when terminal output was committed — the bridge's
            // `bridge_relay_delegated_to_watcher`
            // arm in `turn_bridge/mod.rs` (the `else if` at ~line 4071) saves
            // inflight and immediately returns, so the bridge will NOT come back
            // to revoke the debt or clear the inflight even if dispatch
            // finalization fails. Organic user turns (`dispatch_id = null`)
            // surfaced this regression: when the streaming finalizer fell
            // through to a stale fallback dispatch_id and reported
            // `dispatch_ok = false`, the watcher used to leave the inflight and
            // the channel mailbox cancel_token in place, orphaning them
            // forever. The decoupling rule is:
            //
            //   * `clear_inflight_state` + `finish_restored_watcher_active_turn`
            //     fire whenever the watcher committed terminal output
            //     (delivered or intentionally suppressed) — both bridge and
            //     watcher are now safe to call them concurrently because
            //     `mailbox_finish_turn` is idempotent (the second caller
            //     observes an empty active slot).
            //   * Anything that genuinely depends on the dispatch lifecycle
            //     having completed (queue kickoff, dispatch followup,
            //     terminal-stop decision) remains gated on `dispatch_ok` further
            //     below.
            //
            // The `mailbox_finalize_owed.swap(false, AcqRel)` ordering still
            // matters:
            //   * Acquire — observes the bridge's prior `Release` store of
            //     `true` (and any inflight writes that preceded it) before
            //     we call `mailbox_finish_turn`.
            //   * Release — publishes our reset back to `false`, so a watcher
            //     that survives into the next turn will not accidentally clear
            //     that turn's freshly registered cancel_token.
            let owed = mailbox_finalize_owed.swap(false, std::sync::atomic::Ordering::AcqRel);
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
                    "owed_finalize": owed,
                    "dispatch_ok": dispatch_ok,
                    "has_assistant_response": has_assistant_response,
                    "full_response_len": full_response.len(),
                }),
            );
            // codex P2 (#1670): cleanup (mailbox_finish_turn + cancel_token
            // release) MUST run on every relay-completed terminal even when
            // `dispatch_ok = false`, otherwise organic turns leak forever.
            // But the queue-kickoff side-effect — auto-dispatching the next
            // queued turn — must stay gated on `dispatch_ok`. Without this
            // split a failed dispatch silently kicks off the next backlog
            // entry. The redundant `should_kickoff_queue` block further
            // below is also `dispatch_ok`-gated and remains as a fallback
            // for paths where the helper short-circuited.
            finish_restored_watcher_active_turn(
                &shared,
                &provider_kind,
                channel_id,
                finish_mailbox_on_completion,
                owed,
                dispatch_ok,
                "restored watcher completed with queued backlog",
            )
            .await;
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
            let delegated_finalize_owed = owed;
            let mailbox = shared.mailbox(channel_id);
            let has_active_turn = mailbox.has_active_turn().await;
            let watcher_handled_mailbox_finish =
                finish_mailbox_on_completion || delegated_finalize_owed;
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
            shared.placeholder_cleanup.terminal_cleanup_committed(
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
    tracing::info!("  [{ts}] 👁 tmux watcher stopped for #{tmux_session_name}");
}

#[cfg(test)]
mod tests {
    use super::{
        RelaySlotGuard, TuiCompletionGateOutcome, Utf8ChunkDecoder,
        adopt_watcher_terminal_message_ids_from_inflight, build_watcher_streaming_edit_text,
        discard_restored_response_seed_before_no_inflight_terminal_relay,
        discard_watcher_pending_buffer_after_suppressed_turn,
        legacy_wrapper_prompt_candidates_from_pane, mark_watcher_terminal_delivery_committed,
        should_probe_tmux_liveness, terminal_event_consumed_offset,
        watcher_batch_contains_assistant_event, watcher_batch_contains_relayable_response,
        watcher_direct_terminal_should_commit_session_idle,
        watcher_fallback_edit_failure_can_delete_original_placeholder,
        watcher_inflight_represents_external_input, watcher_jsonl_turn_state_ready_for_input,
        watcher_should_clear_stale_terminal_message_ids, watcher_should_defer_delegated_fresh_idle,
        watcher_should_delete_suppressed_placeholder,
        watcher_should_suppress_streaming_after_bridge_delivery,
        watcher_terminal_commit_side_effects_for_test, watcher_terminal_edit_consumes_placeholder,
        watcher_terminal_token_update_status,
    };
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::discord::InflightTurnState;
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use crate::services::discord::{
        mailbox_enqueue_intervention, mailbox_snapshot, mailbox_take_next_soft_intervention,
        mailbox_try_start_turn,
    };
    use crate::services::provider::{CancelToken, ProviderKind};
    use crate::services::turn_orchestrator::{Intervention, InterventionMode};
    use serenity::all::{ChannelId, MessageId, UserId};
    use std::sync::{LazyLock, Mutex};

    static TEST_ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct AgentdeskRootGuard(Option<std::ffi::OsString>);

    impl AgentdeskRootGuard {
        fn set(path: &std::path::Path) -> Self {
            let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
            Self(previous)
        }
    }

    impl Drop for AgentdeskRootGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    #[test]
    fn terminal_event_consumed_offset_excludes_buffered_tail() {
        assert_eq!(terminal_event_consumed_offset(128, "next-turn\n"), 118);
        assert_eq!(terminal_event_consumed_offset(8, "longer-than-offset"), 0);
    }

    #[test]
    fn relay_slot_guard_releases_on_drop() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        // Simulate a watcher acquiring the slot (CAS 0 -> non-zero token).
        let slot = Arc::new(AtomicU64::new(0));
        slot.store(42, Ordering::Release);
        {
            let _guard = RelaySlotGuard::new(slot.clone());
            assert_eq!(slot.load(Ordering::Acquire), 42, "slot held inside scope");
        }
        // #2840: dropping without an explicit release (panic / `?` / abort) must
        // still free the slot so a replacement watcher is not wedged.
        assert_eq!(slot.load(Ordering::Acquire), 0, "Drop released the slot");
    }

    #[test]
    fn watcher_terminal_delivery_commit_mirrors_bridge_inflight_fields() {
        let _lock = match TEST_ENV_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root_guard = AgentdeskRootGuard::set(tmp.path());

        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(987_2999);
        let tmux_session_name = "AgentDesk-claude-adk-cc";
        let mut state = InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            Some("adk-cc".to_string()),
            42,
            1001,
            1002,
            "prompt".to_string(),
            Some("session-2999".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/agentdesk-2999-output.jsonl".to_string()),
            None,
            64,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        state.turn_start_offset = Some(64);
        crate::services::discord::inflight::save_inflight_state(&state).expect("save inflight");
        let identity = crate::services::discord::inflight::InflightTurnIdentity::from_state(&state);

        assert!(mark_watcher_terminal_delivery_committed(
            &provider,
            channel_id,
            tmux_session_name,
            Some(&identity),
            "delivered response",
            64,
            Some(7),
            128,
        ));

        let persisted =
            crate::services::discord::inflight::load_inflight_state(&provider, channel_id.get())
                .expect("load inflight");
        assert!(persisted.terminal_delivery_committed);
        assert_eq!(persisted.full_response, "delivered response");
        assert_eq!(persisted.response_sent_offset, "delivered response".len());
        assert_eq!(persisted.last_offset, 128);
        assert_eq!(persisted.last_watcher_relayed_offset, Some(64));
        assert_eq!(persisted.last_watcher_relayed_generation_mtime_ns, Some(7));
    }

    #[tokio::test]
    async fn terminal_delivery_timeout_cleanup_releases_mailbox_and_preserves_followup_queue() {
        let _lock = match TEST_ENV_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root_guard = AgentdeskRootGuard::set(tmp.path());

        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(987_3000);
        let tmux_session_name = "AgentDesk-claude-adk-cc";
        assert!(
            mailbox_try_start_turn(
                &shared,
                channel_id,
                std::sync::Arc::new(CancelToken::new()),
                UserId::new(42),
                MessageId::new(1001),
            )
            .await
        );

        let enqueue = mailbox_enqueue_intervention(
            &shared,
            &provider,
            channel_id,
            Intervention {
                author_id: UserId::new(99),
                author_is_bot: false,
                message_id: MessageId::new(2001),
                source_message_ids: vec![MessageId::new(2001)],
                text: "queued follow-up".to_string(),
                mode: InterventionMode::Soft,
                created_at: std::time::Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
                pending_uploads: Vec::new(),
                voice_announcement: None,
            },
        )
        .await;
        assert!(enqueue.enqueued);

        let mut state = InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            Some("adk-cc".to_string()),
            42,
            1001,
            1002,
            "prompt".to_string(),
            Some("session-2999".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/agentdesk-2999-output.jsonl".to_string()),
            None,
            64,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        state.turn_start_offset = Some(64);
        crate::services::discord::inflight::save_inflight_state(&state).expect("save inflight");
        let identity = crate::services::discord::inflight::InflightTurnIdentity::from_state(&state);
        assert!(mark_watcher_terminal_delivery_committed(
            &provider,
            channel_id,
            tmux_session_name,
            Some(&identity),
            "delivered response",
            64,
            Some(7),
            128,
        ));

        let side_effects = watcher_terminal_commit_side_effects_for_test(
            true,
            TuiCompletionGateOutcome::TimedOut,
            true,
        );
        assert!(side_effects.clear_inflight);
        assert!(side_effects.finish_restored_turn);
        crate::services::discord::inflight::clear_inflight_state(&provider, channel_id.get());
        super::finish_restored_watcher_active_turn(
            &shared,
            &provider,
            channel_id,
            true,
            false,
            false,
            "terminal_delivery_timeout_cleanup_test",
        )
        .await;

        assert!(
            crate::services::discord::inflight::load_inflight_state(&provider, channel_id.get())
                .is_none()
        );
        let snapshot = mailbox_snapshot(&shared, channel_id).await;
        assert!(snapshot.cancel_token.is_none());
        assert_eq!(snapshot.intervention_queue.len(), 1);
        let next = mailbox_take_next_soft_intervention(&shared, &provider, channel_id)
            .await
            .into_intervention()
            .map(|(intervention, _)| intervention.text);
        assert_eq!(next.as_deref(), Some("queued follow-up"));
    }

    #[test]
    fn relay_slot_guard_release_is_idempotent_and_does_not_clobber_reacquire() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        let slot = Arc::new(AtomicU64::new(7));
        let mut guard = RelaySlotGuard::new(slot.clone());
        guard.release();
        assert_eq!(
            slot.load(Ordering::Acquire),
            0,
            "explicit release frees slot"
        );

        // After the explicit release, another watcher may legitimately acquire
        // the slot. The first guard's trailing Drop must NOT reset that token to
        // 0 — the idempotent `released` flag guarantees it.
        slot.store(99, Ordering::Release);
        drop(guard);
        assert_eq!(
            slot.load(Ordering::Acquire),
            99,
            "Drop after explicit release must not clobber a re-acquired slot"
        );
    }

    #[test]
    fn bridge_suppressed_turn_discards_pending_buffer_before_direct_input() {
        let mut all_data = "{\"type\":\"assistant\",\"message\":\"old\"}\n".to_string();
        let mut all_data_start_offset = 10;
        let mut all_data_fully_mirrored_to_session_relay = false;
        let mut all_data_session_bound_relay_ack = None;

        discard_watcher_pending_buffer_after_suppressed_turn(
            &mut all_data,
            &mut all_data_start_offset,
            &mut all_data_fully_mirrored_to_session_relay,
            &mut all_data_session_bound_relay_ack,
            42,
        );

        assert!(all_data.is_empty());
        assert_eq!(all_data_start_offset, 42);
        assert!(all_data_fully_mirrored_to_session_relay);
        assert!(all_data_session_bound_relay_ack.is_none());
    }

    #[test]
    fn delegated_fresh_idle_without_response_is_not_terminal_commit() {
        assert!(watcher_should_defer_delegated_fresh_idle(true, ""));
        assert!(!watcher_should_defer_delegated_fresh_idle(false, "   "));
        assert!(!watcher_should_defer_delegated_fresh_idle(
            true,
            "assistant text"
        ));
    }

    #[test]
    fn terminal_relay_adopts_late_saved_inflight_message_ids() {
        let mut inflight = InflightTurnState::new(
            ProviderKind::Claude,
            123,
            Some("adk-cc".to_string()),
            42,
            1001,
            2002,
            "prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        );
        inflight.status_message_id = Some(3003);

        let mut placeholder_msg_id = None;
        let mut placeholder_from_restored_inflight = false;
        let mut status_panel_msg_id = None;

        adopt_watcher_terminal_message_ids_from_inflight(
            &mut placeholder_msg_id,
            &mut placeholder_from_restored_inflight,
            &mut status_panel_msg_id,
            &inflight,
            "AgentDesk-claude-adk-cc",
        );

        assert_eq!(placeholder_msg_id, Some(MessageId::new(2002)));
        assert!(placeholder_from_restored_inflight);
        assert_eq!(status_panel_msg_id, Some(MessageId::new(3003)));
    }

    #[test]
    fn terminal_relay_does_not_adopt_synthetic_status_panel_message_id() {
        let mut inflight = InflightTurnState::new(
            ProviderKind::Claude,
            123,
            Some("adk-cc".to_string()),
            42,
            1001,
            2002,
            "prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        );
        inflight.status_message_id = Some(9_100_000_000_000_000_123);

        let mut placeholder_msg_id = None;
        let mut placeholder_from_restored_inflight = false;
        let mut status_panel_msg_id = None;

        adopt_watcher_terminal_message_ids_from_inflight(
            &mut placeholder_msg_id,
            &mut placeholder_from_restored_inflight,
            &mut status_panel_msg_id,
            &inflight,
            "AgentDesk-claude-adk-cc",
        );

        assert_eq!(placeholder_msg_id, Some(MessageId::new(2002)));
        assert!(placeholder_from_restored_inflight);
        assert_eq!(status_panel_msg_id, None);
    }

    #[test]
    fn terminal_relay_does_not_adopt_inflight_for_other_tmux_session() {
        let mut inflight = InflightTurnState::new(
            ProviderKind::Claude,
            123,
            Some("adk-cc".to_string()),
            42,
            1001,
            2002,
            "prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-claude-other".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        );
        inflight.status_message_id = Some(3003);

        let mut placeholder_msg_id = None;
        let mut placeholder_from_restored_inflight = false;
        let mut status_panel_msg_id = None;

        adopt_watcher_terminal_message_ids_from_inflight(
            &mut placeholder_msg_id,
            &mut placeholder_from_restored_inflight,
            &mut status_panel_msg_id,
            &inflight,
            "AgentDesk-claude-adk-cc",
        );

        assert_eq!(placeholder_msg_id, None);
        assert!(!placeholder_from_restored_inflight);
        assert_eq!(status_panel_msg_id, None);
    }

    #[test]
    fn terminal_relay_does_not_adopt_placeholderless_user_message() {
        let inflight = InflightTurnState::new(
            ProviderKind::Claude,
            123,
            Some("adk-cc".to_string()),
            42,
            1001,
            1001,
            "prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        );

        let mut placeholder_msg_id = None;
        let mut placeholder_from_restored_inflight = false;
        let mut status_panel_msg_id = None;

        adopt_watcher_terminal_message_ids_from_inflight(
            &mut placeholder_msg_id,
            &mut placeholder_from_restored_inflight,
            &mut status_panel_msg_id,
            &inflight,
            "AgentDesk-claude-adk-cc",
        );

        assert_eq!(placeholder_msg_id, None);
        assert!(!placeholder_from_restored_inflight);
        assert_eq!(status_panel_msg_id, None);
    }

    #[test]
    fn external_input_lease_is_consumed_only_by_external_input_inflight() {
        let mut managed = InflightTurnState::new(
            ProviderKind::Claude,
            123,
            Some("adk-cc".to_string()),
            42,
            1001,
            2002,
            "prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        );
        assert!(!watcher_inflight_represents_external_input(Some(&managed)));

        managed.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
        assert!(watcher_inflight_represents_external_input(Some(&managed)));

        managed.turn_source = crate::services::discord::inflight::TurnSource::ExternalAdopted;
        assert!(watcher_inflight_represents_external_input(Some(&managed)));
    }

    #[test]
    fn watcher_direct_terminal_idle_commit_requires_delivery_without_inflight() {
        assert!(watcher_direct_terminal_should_commit_session_idle(
            true, false, true, false, false, false
        ));
        assert!(watcher_direct_terminal_should_commit_session_idle(
            true, false, false, true, false, false
        ));
        assert!(watcher_direct_terminal_should_commit_session_idle(
            true, false, false, false, true, false
        ));
        assert!(watcher_direct_terminal_should_commit_session_idle(
            true, false, false, false, false, true
        ));
        assert!(!watcher_direct_terminal_should_commit_session_idle(
            false, false, true, true, true, true
        ));
        assert!(!watcher_direct_terminal_should_commit_session_idle(
            true, true, true, true, true, true
        ));
        assert!(watcher_direct_terminal_should_commit_session_idle(
            true, false, false, false, false, false
        ));
    }

    #[test]
    fn watcher_direct_terminal_idle_commit_keeps_later_token_update_idle() {
        assert_eq!(watcher_terminal_token_update_status(true), "idle");
        assert_eq!(
            watcher_terminal_token_update_status(false),
            crate::db::session_status::TURN_ACTIVE
        );
    }

    #[test]
    fn legacy_wrapper_pane_prompt_candidates_reconstruct_wrapped_direct_input() {
        let pane = "\
▶ Ready for input (type message + Enter)
TUI-E2E-marker 한 줄로 marker를 그대로 응답하고, 'ssh
-direct' 단어도 포함해줘.
[sending...]
[session: abc]
TUI-E2E-marker ssh-direct

▶ Ready for input (type message + Enter)
";

        let candidates = legacy_wrapper_prompt_candidates_from_pane(pane);

        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.contains("'ssh-direct'")),
            "wrapped terminal prompt should have a compact candidate for pending-prompt matching"
        );
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.contains("'ssh -direct'")),
            "wrapped terminal prompt should keep a spaced candidate for readable direct observation"
        );
    }

    #[test]
    fn legacy_wrapper_prompt_observation_requires_response_batch() {
        assert!(!watcher_batch_contains_relayable_response(
            br#"{"provider":"codex","type":"ready_for_input"}"#
        ));
        assert!(watcher_batch_contains_relayable_response(
            br#"{"type":"assistant","message":{"content":[{"text":"ok"}]}}"#
        ));
        assert!(watcher_batch_contains_relayable_response(
            br#"{"type":"result","result":"ok"}"#
        ));
    }

    #[test]
    fn post_terminal_continuation_probe_ignores_result_only_batches() {
        assert!(!watcher_batch_contains_assistant_event(
            br#"{"provider":"codex","type":"ready_for_input"}"#
        ));
        assert!(watcher_batch_contains_assistant_event(
            br#"{"type":"assistant","message":{"content":[{"type":"tool_use"}]}}"#
        ));
        assert!(!watcher_batch_contains_assistant_event(
            br#"{"type":"result","result":"duplicate terminal text"}"#
        ));
    }

    #[test]
    fn claude_watcher_ready_uses_transcript_turn_state_not_pane_prompt() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            concat!(
                r#"{"type":"user","message":{"content":"review"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
                "\n"
            ),
        )
        .unwrap();
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            watcher_jsonl_turn_state_ready_for_input(
                &crate::services::provider::ProviderKind::Claude,
                Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui),
                file.path().to_str().unwrap(),
                len,
            ),
            Some(false)
        );

        std::fs::write(
            file.path(),
            concat!(
                r#"{"type":"user","message":{"content":"review"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#,
                "\n",
                r#"{"type":"system","subtype":"turn_duration","sessionId":"s"}"#,
                "\n"
            ),
        )
        .unwrap();
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            watcher_jsonl_turn_state_ready_for_input(
                &crate::services::provider::ProviderKind::Claude,
                Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui),
                file.path().to_str().unwrap(),
                len,
            ),
            Some(true)
        );
    }

    // The transcript holds a fully written terminator envelope
    // (`system/turn_duration`) and the watcher's `current_offset` lags the
    // file size by one byte. Pre-fix the watcher would return Busy and the
    // idle-queue drain would loop indefinitely (the production 9× recurrence
    // observed on 2026-05-26: `hosted TUI structured turn state is busy`
    // every 2s after #2789 froze the binding offset across quick-exit
    // restarts). The strict-terminator override in `jsonl_ready_for_input`
    // now classifies a fully-parsed terminator envelope as Ready regardless
    // of the relay's last_offset; partial trailing fragments are still
    // refused, so this is safe.
    #[test]
    fn claude_watcher_ready_treats_complete_terminator_envelope_as_ready() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            r#"{"type":"system","subtype":"turn_duration","sessionId":"s"}"#,
        )
        .unwrap();
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            watcher_jsonl_turn_state_ready_for_input(
                &crate::services::provider::ProviderKind::Claude,
                Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui),
                file.path().to_str().unwrap(),
                len.saturating_sub(1),
            ),
            Some(true)
        );
    }

    // Race guard at the watcher boundary: a complete terminator envelope is
    // followed by a partial `{"ty` fragment of the next turn's user line and
    // the watcher's offset still lags. The strict-terminator predicate must
    // refuse to fall through the partial line, keeping the watcher non-ready
    // so we do not race a new turn that has just begun.
    #[test]
    fn claude_watcher_ready_keeps_busy_when_partial_user_follows_terminator() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            concat!(
                r#"{"type":"system","subtype":"turn_duration","sessionId":"s"}"#,
                "\n",
                r#"{"ty"#,
            ),
        )
        .unwrap();
        let len = std::fs::metadata(file.path()).unwrap().len();

        assert_eq!(
            watcher_jsonl_turn_state_ready_for_input(
                &crate::services::provider::ProviderKind::Claude,
                Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui),
                file.path().to_str().unwrap(),
                len.saturating_sub(5),
            ),
            Some(false)
        );
    }

    #[test]
    fn no_inflight_terminal_response_does_not_reuse_previous_placeholder() {
        assert!(watcher_should_clear_stale_terminal_message_ids(
            false,
            true,
            Some(MessageId::new(42))
        ));
        assert!(!watcher_should_clear_stale_terminal_message_ids(
            true,
            true,
            Some(MessageId::new(42))
        ));
        assert!(!watcher_should_clear_stale_terminal_message_ids(
            false,
            false,
            Some(MessageId::new(42))
        ));
        assert!(!watcher_should_clear_stale_terminal_message_ids(
            false, true, None
        ));
    }

    #[test]
    fn no_inflight_terminal_response_drops_restored_response_seed() {
        let restored = "previous turn";
        let mut full_response = "previous turnfresh turn".to_string();
        let mut response_sent_offset = 0;
        let mut last_edit_text = "previous turn".to_string();

        assert!(
            discard_restored_response_seed_before_no_inflight_terminal_relay(
                &mut full_response,
                &mut response_sent_offset,
                &mut last_edit_text,
                restored,
                false,
                true,
            )
        );
        assert_eq!(full_response, "fresh turn");
        assert_eq!(response_sent_offset, 0);
        assert!(last_edit_text.is_empty());
    }

    #[test]
    fn restored_response_seed_is_kept_for_managed_inflight() {
        let restored = "previous turn";
        let mut full_response = "previous turnfresh turn".to_string();
        let mut response_sent_offset = restored.len();
        let mut last_edit_text = "previous turn".to_string();

        assert!(
            !discard_restored_response_seed_before_no_inflight_terminal_relay(
                &mut full_response,
                &mut response_sent_offset,
                &mut last_edit_text,
                restored,
                true,
                true,
            )
        );
        assert_eq!(full_response, "previous turnfresh turn");
        assert_eq!(response_sent_offset, restored.len());
    }

    #[test]
    fn no_inflight_user_boundary_without_fresh_text_drops_restored_response_seed() {
        let restored = "previous turn";
        let mut full_response = "previous turn".to_string();
        let mut response_sent_offset = restored.len();
        let mut last_edit_text = "previous turn".to_string();

        assert!(
            discard_restored_response_seed_before_no_inflight_terminal_relay(
                &mut full_response,
                &mut response_sent_offset,
                &mut last_edit_text,
                restored,
                false,
                false,
            )
        );
        assert_eq!(full_response, "");
        assert_eq!(response_sent_offset, 0);
        assert!(last_edit_text.is_empty());
    }

    #[test]
    fn tmux_dead_marker_short_circuits_liveness_interval() {
        assert!(should_probe_tmux_liveness(
            std::time::Duration::from_millis(1),
            true,
        ));
        assert!(!should_probe_tmux_liveness(
            std::time::Duration::from_millis(1),
            false,
        ));
    }

    #[test]
    fn status_panel_v2_watcher_streaming_edit_moves_processing_footer_to_response_message() {
        let rendered = build_watcher_streaming_edit_text(
            true,
            "PIPE-E2E-CODEX OK",
            "⠙ 계속 처리 중",
            &ProviderKind::Codex,
        );

        assert_eq!(rendered, "PIPE-E2E-CODEX OK\n\n⠙ 계속 처리 중");
    }

    #[test]
    fn legacy_watcher_streaming_edit_keeps_processing_footer() {
        let rendered = build_watcher_streaming_edit_text(
            false,
            "Partial answer",
            "⠙ 계속 처리 중",
            &ProviderKind::Codex,
        );

        assert_eq!(rendered, "Partial answer\n\n⠙ 계속 처리 중");
    }

    #[test]
    fn watcher_streaming_suppresses_after_bridge_delivery_only_for_response() {
        assert!(watcher_should_suppress_streaming_after_bridge_delivery(
            true, true
        ));
        assert!(!watcher_should_suppress_streaming_after_bridge_delivery(
            true, false
        ));
        assert!(!watcher_should_suppress_streaming_after_bridge_delivery(
            false, true
        ));
    }

    #[test]
    fn watcher_terminal_edit_detaches_placeholder_from_later_cleanup() {
        assert!(watcher_terminal_edit_consumes_placeholder(
            &ReplaceLongMessageOutcome::EditedOriginal
        ));
        assert!(!watcher_terminal_edit_consumes_placeholder(
            &ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                edit_error: "edit failed".to_string()
            }
        ));
    }

    #[test]
    fn watcher_bridge_delivery_preserves_restored_inflight_placeholder() {
        assert!(!watcher_should_delete_suppressed_placeholder(true));
        assert!(watcher_should_delete_suppressed_placeholder(false));
    }

    #[test]
    fn fallback_edit_failure_never_deletes_original_without_placeholder_probe() {
        assert!(
            !watcher_fallback_edit_failure_can_delete_original_placeholder(12, "partial answer")
        );
        assert!(
            !watcher_fallback_edit_failure_can_delete_original_placeholder(0, "partial answer")
        );
        assert!(
            !watcher_fallback_edit_failure_can_delete_original_placeholder(0, "⠙ Processing...")
        );
    }

    #[test]
    fn utf8_decoder_buffers_split_multibyte_scalar_at_chunk_start() {
        let mut decoder = Utf8ChunkDecoder::default();
        let payload = "안녕\n";
        let bytes = payload.as_bytes();

        let first = decoder.decode(&bytes[..1], 20);
        assert_eq!(first.start_offset, None);
        assert!(first.text.is_empty());

        let second = decoder.decode(&bytes[1..], 21);
        assert_eq!(second.start_offset, Some(20));
        assert_eq!(second.text, payload);
        assert!(!second.text.contains('\u{FFFD}'));
    }

    #[test]
    fn utf8_decoder_preserves_jsonl_when_multibyte_scalar_splits_after_prefix() {
        let mut decoder = Utf8ChunkDecoder::default();
        let payload = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"안녕하세요 😀\"}]}}\n";
        let korean_start = payload.find('안').expect("fixture contains korean text");
        let split = korean_start + 1;
        let bytes = payload.as_bytes();

        let first = decoder.decode(&bytes[..split], 100);
        let second = decoder.decode(&bytes[split..], 100 + split as u64);

        assert_eq!(first.start_offset, Some(100));
        assert_eq!(second.start_offset, Some(100 + korean_start as u64));
        assert_eq!(format!("{}{}", first.text, second.text), payload);
        assert!(!first.text.contains('\u{FFFD}'));
        assert!(!second.text.contains('\u{FFFD}'));
    }
}
