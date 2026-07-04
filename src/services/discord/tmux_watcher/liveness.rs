//! #3038 S1 tmux watcher liveness and stream decision helpers.

use super::*;

#[cfg(unix)]
pub(super) async fn commit_watcher_direct_terminal_session_idle(
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

/// #3041 P1-1: process-global monotonic counter that mints a unique
/// `instance_id` for each watcher spawn. It distinguishes an outgoing watcher
/// from its replacement across a reattach so the delivery-lease holder
/// (`LeaseHolder::Watcher { instance_id }`) of a still-running send cannot be
/// confused with — or released/committed by — a successor watcher that picks
/// up the same channel/session (the §5.2 B2 single-holder invariant).
static WATCHER_INSTANCE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub(super) fn next_watcher_instance_id() -> u64 {
    WATCHER_INSTANCE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// #3041 P1-1 (B3, codex R2 Issue-1): delivery-lease acquire deadline for the
/// watcher terminal send. The deadline is a HOLDER-LIVENESS signal, NOT a hard
/// cap on delivery duration — while the send future is in flight the watcher
/// keeps the lease alive with a background HEARTBEAT that `renew()`s the
/// deadline every heartbeat interval. Because a LIVE holder always re-extends
/// within one interval, a long multi-chunk send (which can exceed any FIXED
/// deadline — an unbounded response splits into 2000-char chunks paced ~500ms
/// apart plus a 1s rate limiter, so 60+ chunks can run past 90s) is NEVER
/// reclaimed mid-flight. Conversely, a genuinely DEAD holder (its watcher
/// task/process gone) stops renewing, so the lease expires and a replacement
/// reclaims it within ~one deadline.
///
/// #3041 P1-2: this is now an ALIAS for the shared
/// [`crate::services::discord::DELIVERY_LEASE_DEADLINE_MS`] so the watcher and
/// the bridge use the SAME deadline against the SAME per-channel cell. Kept as a
/// named alias to minimize churn at the watcher call/test sites.
pub(super) const WATCHER_DELIVERY_LEASE_DEADLINE_MS: u64 =
    crate::services::discord::DELIVERY_LEASE_DEADLINE_MS;

/// #3041 P1-1 (§3, codex R2 Issue-1): how often the in-flight watcher send
/// renews its delivery lease. Alias for the shared
/// [`crate::services::discord::DELIVERY_LEASE_HEARTBEAT_MS`] (P1-2).
#[allow(dead_code)] // #3034: test-only alias; live path reads the shared const.
pub(super) const WATCHER_DELIVERY_LEASE_HEARTBEAT_MS: u64 =
    crate::services::discord::DELIVERY_LEASE_HEARTBEAT_MS;

/// #3041 P1-2: the heartbeat RAII handle now lives in the shared `discord` module
/// (`super::DeliveryLeaseHeartbeat`) so the watcher and the bridge reuse one
/// implementation. Re-exported here under the watcher-local name to keep the
/// existing watcher call sites and tests unchanged.
pub(super) use crate::services::discord::DeliveryLeaseHeartbeat;

/// #2441 (H1) — race a fixed sleep against a `notify`-backed wake-up
/// from `JsonlWatcher`. Returns as soon as EITHER the sleep elapses or
/// the watcher fires its `Notify`. This is the primitive used to replace
/// the six fixed-interval `tokio::time::sleep(200ms / 250ms)` polling
/// sites in the watcher loop: a real wrapper write wakes us immediately
/// while the sleep continues to bound the wake-up latency (defense in
/// depth for environments where the notify backend silently drops
/// events).
pub(super) async fn sleep_or_jsonl_event(
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

pub(super) fn tmux_dead_marker_exists(tmux_session_name: &str) -> bool {
    std::path::Path::new(&crate::services::tmux_common::session_dead_marker_path(
        tmux_session_name,
    ))
    .exists()
}

pub(super) fn should_probe_tmux_liveness(
    elapsed_since_last_probe: std::time::Duration,
    dead_marker_present: bool,
) -> bool {
    dead_marker_present || elapsed_since_last_probe >= TMUX_LIVENESS_PROBE_INTERVAL
}

pub(super) fn build_watcher_streaming_edit_text(
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
        let formatted = crate::services::discord::formatting::format_for_discord_with_status_panel(
            current_portion,
            provider,
        );
        build_streaming_placeholder_text(&formatted, status_block)
    }
}

pub(super) fn watcher_streaming_rollover_should_skip(current_portion: &str) -> bool {
    crate::services::discord::response_sanitizer::subagent_notification_card::streaming_rollover_should_skip(
        current_portion,
    )
}

pub(super) fn watcher_should_suppress_streaming_after_bridge_delivery(
    bridge_delivered_turn: bool,
    has_assistant_response: bool,
) -> bool {
    bridge_delivered_turn && has_assistant_response
}

pub(in crate::services::discord::tmux) fn watcher_lifecycle_terminal_delivery_observed(
    terminal_delivery_observed: bool,
    bridge_delivered_turn: bool,
) -> bool {
    terminal_delivery_observed || bridge_delivered_turn
}

#[cfg(test)]
pub(super) fn watcher_terminal_edit_consumes_placeholder(
    outcome: &ReplaceLongMessageOutcome,
) -> bool {
    matches!(outcome, ReplaceLongMessageOutcome::EditedOriginal)
}

pub(super) fn watcher_should_delete_suppressed_placeholder(
    placeholder_from_restored_inflight: bool,
) -> bool {
    !placeholder_from_restored_inflight
}

pub(super) fn watcher_fallback_edit_failure_can_delete_original_placeholder(
    _response_sent_offset: usize,
    _last_edit_text: &str,
) -> bool {
    // #2757 parity with session_relay_sink: after a terminal fallback send,
    // the original message id may already contain partial assistant content.
    // Without a Discord probe proving it is a pure placeholder, preserve it.
    false
}

#[cfg(test)]
mod tests {
    use super::{build_watcher_streaming_edit_text, watcher_streaming_rollover_should_skip};
    use crate::services::provider::ProviderKind;

    #[test]
    fn rollover_skips_subagent_notification_then_sanitizes_edit_3818() {
        let current_portion = format!(
            r#"<subagent_notification>
{{"agent_path":"/tmp/adk-issue-3818-subagent-xml/private-agent","status":{{"completed":"{}"}}}}
</subagent_notification>"#,
            "Review complete. ".repeat(220),
        );
        let status_block = "⠙ 계속 처리 중";
        let raw_plan = crate::services::discord::formatting::plan_streaming_rollover(
            &current_portion,
            status_block,
        )
        .expect("raw notification would otherwise roll over");

        assert!(raw_plan.frozen_chunk.contains("<subagent_notification>"));
        assert!(watcher_streaming_rollover_should_skip(&current_portion));

        let rendered = build_watcher_streaming_edit_text(
            false,
            &current_portion,
            status_block,
            &ProviderKind::Codex,
        );
        assert!(rendered.contains("Subagent completed"));
        assert!(!rendered.contains("<subagent_notification>"));
        assert!(!rendered.contains("</subagent_notification>"));
        assert!(!rendered.contains("agent_path"));
        assert!(!rendered.contains("/tmp/adk-issue-3818-subagent-xml"));
    }

    #[test]
    fn rollover_skips_tui_chrome_prefixed_subagent_notification_3818() {
        let current_portion = format!(
            "No response requested.\n<subagent_notification>\n{{\"agent_path\":\"/tmp/adk-issue-3818-subagent-xml/private-agent\",\"status\":{{\"completed\":\"{}\"}}}}\n</subagent_notification>",
            "Implementation worker complete. ".repeat(180),
        );

        assert!(watcher_streaming_rollover_should_skip(&current_portion));

        let rendered = build_watcher_streaming_edit_text(
            false,
            &current_portion,
            "⠙ 계속 처리 중",
            &ProviderKind::Codex,
        );
        assert!(rendered.contains("Subagent completed"));
        assert!(!rendered.contains("No response requested."));
        assert!(!rendered.contains("<subagent_notification>"));
        assert!(!rendered.contains("</subagent_notification>"));
        assert!(!rendered.contains("agent_path"));
        assert!(!rendered.contains("/tmp/adk-issue-3818-subagent-xml"));
    }

    #[test]
    fn legacy_streaming_edit_sanitizes_subagent_notification_3818() {
        let current_portion = r#"<subagent_notification>
{"agent_path":"/tmp/adk-issue-3818-subagent-xml/review-agent","status":{"completed":"Read-only review complete.\n\nVERDICT: CLEAN"}}
</subagent_notification>"#;

        let rendered = build_watcher_streaming_edit_text(
            false,
            current_portion,
            "⠙ 계속 처리 중",
            &ProviderKind::Codex,
        );

        assert!(rendered.contains("Subagent completed"));
        assert!(rendered.contains("Read-only review complete."));
        assert!(rendered.contains("VERDICT: CLEAN"));
        assert!(rendered.ends_with("⠙ 계속 처리 중"));
        assert!(!rendered.contains("<subagent_notification>"));
        assert!(!rendered.contains("</subagent_notification>"));
        assert!(!rendered.contains("agent_path"));
        assert!(!rendered.contains("/tmp/adk-issue-3818-subagent-xml"));
    }
}

/// #3107: inflight-INDEPENDENT probe — capture the pane right now and classify
/// it as "actively producing assistant output" (busy/streaming) vs.
/// "finished/idle". Used to tell a live agentic TUI turn that merely lost its
/// inflight mid-turn apart from genuine post-finish ghost noise.
///
/// THROTTLED hot-path cost: this spawns a `tmux capture-pane` subprocess, so the
/// callers only invoke it lazily — when they are *already about to suppress*
/// (the cheap `inflight_missing` prefix is true) — mirroring the existing lazy
/// SSH-direct / external-lease computations in the post-terminal guard, which
/// are themselves gated on `turn_result_relayed && post_terminal_inflight_missing`.
pub(super) fn watcher_pane_actively_streaming(tmux_session_name: &str) -> bool {
    let Some(pane) = crate::services::platform::tmux::capture_pane(tmux_session_name, -160) else {
        // Capture failed (pane gone / tmux error): not a positive streaming
        // signal — fall back to the existing suppression behavior.
        return false;
    };
    crate::services::tmux_common::tmux_capture_indicates_claude_tui_actively_streaming(&pane)
}

/// #3107 codex re-review (P2#3): abandonment-side progress check. A single
/// static busy frame (e.g. a frozen spinner left on screen by a turn that has
/// actually stopped) must NOT pin the status panel forever. So the abandonment
/// `None` arm requires BOTH a positive busy pane AND *evidence of progress* —
/// the session JSONL was written within `LIVE_TURN_PROGRESS_WINDOW`. A truly
/// live turn keeps appending output, so its file mtime stays fresh; a frozen
/// pane's output file goes stale and the turn is correctly declared abandoned
/// and reclaimed.
///
/// This is intentionally distinct from `watcher_pane_actively_streaming` (used
/// by the self-heal re-acquire): re-acquiring a live-but-inflight-lost turn is
/// safe even on a single busy frame, but BLOCKING a reclaim is the dangerous
/// direction (panel orphan leak), so the reclaim path adds the freshness gate.
pub(super) fn watcher_pane_live_turn_in_progress(
    tmux_session_name: &str,
    output_path: &str,
) -> bool {
    watcher_pane_actively_streaming(tmux_session_name)
        && watcher_output_progressed_recently(output_path)
}

/// Maximum age of the session JSONL's last write for the turn to count as
/// "still making progress". A live agentic Claude turn appends stream-json
/// frames continuously (token deltas, tool events); even a slow tool keeps the
/// wrapper writing well inside this window. Generous enough not to false-reclaim
/// a momentarily-quiet live turn, tight enough that a frozen/stopped pane's
/// stale file trips abandonment within one sweep.
const LIVE_TURN_PROGRESS_WINDOW: std::time::Duration = std::time::Duration::from_secs(20);

pub(super) fn watcher_output_progressed_recently(output_path: &str) -> bool {
    let Ok(meta) = std::fs::metadata(output_path) else {
        // No readable output file: cannot prove progress → not "in progress".
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    modified
        .elapsed()
        .map(|age| age <= LIVE_TURN_PROGRESS_WINDOW)
        // #3107 codex re-review (P2, F4): `elapsed()` returns Err when the file
        // mtime is in the FUTURE (clock drift / NTP jump / an external write with
        // a skewed clock). Bias the ambiguous case toward "in progress" (true):
        // the SAFE direction here is to PRESERVE a live turn's panel, not to
        // reclaim it. A false "not in progress" would delete a genuinely live
        // turn's panel; a false "in progress" merely defers reclaim by one sweep.
        // (Distinct from the mtime-MISSING case above, which returns false for the
        // abandonment path — that's a different, stale-file signal.)
        .unwrap_or(true)
}

/// #3107 self-heal (CHANGE 2): when the pane is actively streaming but the
/// dcserver has no inflight for this channel, re-establish a minimal
/// watcher-owned `InflightTurnState` so subsequent streaming edits relay and the
/// terminal ack has a target (kills `frame_ack_outcome=MissingTarget`). Mirrors
/// the watcher-commit / `build_tui_direct_synthetic_inflight_state` construction
/// (`relay_owner_kind = Watcher`, ExternalInput turn source, session-bound paths)
/// and reuses the still-present status-panel / placeholder message ids as the
/// streaming target.
///
/// Returns true when a fresh inflight was written (so the caller emits the
/// one-shot incident log).
#[allow(clippy::too_many_arguments)]
pub(super) fn reacquire_watcher_inflight_for_active_stream(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    output_path: &str,
    start_offset: u64,
    status_panel_msg_id: Option<serenity::MessageId>,
    placeholder_msg_id: Option<serenity::MessageId>,
    // #3107 codex re-review (P2#3): the #3099 hourglass anchor from the
    // just-cleared inflight, when a source still has it. The watcher-owned
    // re-acquire mints a `user_msg_id == 0` synthetic row; per #3099/#3100 the
    // watcher path does NOT add a `⏳` to a real Discord *user* message for
    // such turns, so leaving this `None` is safe for the common case. But if
    // the cleared row HAD pinned an injected message id (e.g. a
    // task-notification auto-turn that lost its inflight mid-flight), preserving
    // it here keeps the `⏳ → ✅` completion cleanup able to find its own
    // message instead of orphaning the hourglass.
    injected_prompt_message_id: Option<u64>,
) -> bool {
    // The streaming-edit target is the placeholder/status-panel message still
    // owned by this watcher; pin it as `current_msg_id` so edits + the terminal
    // ack resolve a target instead of MissingTarget.
    let current_msg_id = placeholder_msg_id
        .or(status_panel_msg_id)
        .map(serenity::MessageId::get)
        .unwrap_or(0);
    let mut state = crate::services::discord::inflight::InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        None,
        // Headless watcher-owned re-acquire: no request owner, no user message.
        // `user_msg_id == 0` is the established headless/synthetic-turn signal.
        0,
        0,
        current_msg_id,
        String::new(),
        None,
        Some(tmux_session_name.to_string()),
        Some(output_path.to_string()),
        None,
        start_offset,
    );
    state.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
    state.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);
    if let Some(panel) = status_panel_msg_id {
        state.status_message_id = Some(panel.get());
    }
    state.injected_prompt_message_id = injected_prompt_message_id;
    // #3107 codex re-review (P1): atomic compare-and-set. The previous
    // implementation did a non-atomic `load_inflight_state(...).is_some()`
    // preflight here and then an unconditional `save_inflight_state`, leaving a
    // window in which the Discord intake path could create a REAL inflight for a
    // new user turn on this `(provider, channel_id)` that the synthetic save
    // would then clobber. `save_inflight_state_if_absent` performs the
    // existence check and the write under one sidecar flock (shared with
    // intake's `save_inflight_state`), so a concurrent intake inflight always
    // wins and the re-acquire degrades to a no-op.
    crate::services::discord::inflight::save_inflight_state_if_absent(&state).unwrap_or(false)
}

pub(super) fn watcher_handle_no_dispatch_post_work_idle_body(
    full_response: &mut String,
    terminal_kind: &mut Option<WatcherTerminalKind>,
    stall_inflight_snapshot: Option<&InflightTurnState>,
    dispatch_id_present: bool,
    tmux_session_name: &str,
) -> bool {
    if !dispatch_id_present
        && !full_response.trim().is_empty()
        && crate::services::discord::tui_prompt_relay::tui_direct_watcher_synthetic_inflight_matches(
            stall_inflight_snapshot,
            tmux_session_name,
        )
    {
        terminal_kind.get_or_insert(WatcherTerminalKind::SoftStopHookSummary);
        return true;
    }
    full_response.clear();
    false
}

pub(super) fn discard_restored_response_seed_before_no_inflight_terminal_relay(
    full_response: &mut String,
    response_sent_offset: &mut usize,
    last_edit_text: &mut String,
    restored_response_seed: &str,
    inflight_present: bool,
    fresh_assistant_text_seen: bool,
    force_discard_restored_seed: bool,
    restored_seed_delivery_confirmed: bool,
) -> bool {
    if inflight_present || restored_response_seed.trim().is_empty() {
        return false;
    }
    if !full_response.starts_with(restored_response_seed) {
        return false;
    }
    let restored_seed_has_undelivered_body = restored_response_seed
        .get(*response_sent_offset..)
        .is_some_and(|body| !body.trim().is_empty());
    // Preserve the restored seed for the quiescence handoff shape unless the
    // force-discard signal is backed by authoritative delivery evidence for the
    // seed body. `local_cmd_no_output` describes the current turn shape; the
    // delivered-content ring is the proof that stripping this body cannot lose it.
    if restored_seed_has_undelivered_body
        && !fresh_assistant_text_seen
        && !(force_discard_restored_seed && restored_seed_delivery_confirmed)
    {
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

pub(super) fn local_cmd_no_output(
    unprocessed_tail: &str,
    terminal_kind: Option<WatcherTerminalKind>,
    fresh_assistant_text_seen: bool,
    tool_state: &WatcherToolState,
) -> bool {
    matches!(terminal_kind, Some(WatcherTerminalKind::SoftUserBoundary))
        && !fresh_assistant_text_seen
        && !tool_state.any_tool_used
        && first_user_prompt_text(unprocessed_tail).is_some_and(|prompt| {
        !crate::services::discord::tui_prompt_relay::observed_prompt_starts_external_turn_lifecycle(
            &prompt,
        )
    })
}

fn first_user_prompt_text(unprocessed_tail: &str) -> Option<String> {
    for line in unprocessed_tail
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
        if value.get("type").and_then(serde_json::Value::as_str) != Some("user") {
            return None;
        }
        return user_message_prompt_text(value.get("message")?);
    }
    None
}

fn user_message_prompt_text(message: &serde_json::Value) -> Option<String> {
    if message
        .get("role")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|role| role != "user")
    {
        return None;
    }
    match message.get("content")? {
        serde_json::Value::String(text) => non_empty_prompt_text(text),
        serde_json::Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(|item| {
                    item.get("text")
                        .or_else(|| item.get("input_text"))
                        .and_then(serde_json::Value::as_str)
                })
                .collect::<Vec<_>>()
                .join("\n");
            non_empty_prompt_text(&text)
        }
        _ => None,
    }
}

fn non_empty_prompt_text(text: &str) -> Option<String> {
    (!text.trim().is_empty()).then(|| text.to_string())
}
