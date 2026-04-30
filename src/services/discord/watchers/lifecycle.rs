use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DeadSessionCleanupPlan {
    pub(super) preserve_tmux_session: bool,
    pub(super) report_idle_status: bool,
}

pub(super) fn dead_session_cleanup_plan(dispatch_protected: bool) -> DeadSessionCleanupPlan {
    DeadSessionCleanupPlan {
        preserve_tmux_session: dispatch_protected,
        report_idle_status: true,
    }
}

/// Default idle window the post-terminal-success watcher uses to classify
/// continuation state while tmux is still alive and confirmed_end has caught up
/// to the tail offset.
/// See issue #1137: codex agents (G2/G3/G4) were observed emitting additional
/// output for several seconds AFTER the terminal-success log. Issue #1171
/// makes tmux liveness, not post-result idleness, the normal watcher shutdown
/// authority.
pub(crate) const WATCHER_POST_TERMINAL_IDLE_WINDOW: std::time::Duration =
    std::time::Duration::from_secs(5);

/// Input snapshot for [`watcher_stop_decision_after_terminal_success`].
/// Kept as a plain copyable struct so the helper is trivially unit-testable
/// without mocking tokio time or tmux. See issue #1137 for the watcher
/// strictness contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WatcherStopInput {
    /// True once the watcher has relayed a terminal-success result to Discord
    /// for the current dispatch (i.e. `turn_result_relayed`).
    pub(crate) terminal_success_seen: bool,
    /// Tmux pane liveness — `crate::services::platform::tmux::has_session`
    /// (or the watcher's wrapper [`tmux_session_has_live_pane`]).
    pub(crate) tmux_alive: bool,
    /// Shared `confirmed_end_offset` watermark across all watcher replicas
    /// for this channel.
    pub(crate) confirmed_end: u64,
    /// Current tmux jsonl tail offset (`std::fs::metadata(output).len()`).
    pub(crate) tmux_tail_offset: u64,
    /// Time since the last new-output observation. `None` means we have not
    /// observed any output yet during this watcher iteration.
    pub(crate) idle_duration: Option<std::time::Duration>,
    /// Idle window used to classify post-terminal-success continuation state.
    pub(crate) idle_threshold: std::time::Duration,
}

/// Outcome of the watcher-stop strictness check (#1137).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatcherStopDecision {
    /// Watcher should keep running. Either the dispatch hasn't reached
    /// terminal-success yet, or new output is still arriving / the
    /// confirmed-end watermark hasn't caught up.
    Continue,
    /// Terminal success was relayed but additional tmux output is still
    /// being produced (or the idle window hasn't elapsed). The caller should
    /// log "post-terminal-success continuation" exactly once when this
    /// transitions in, then keep the watcher alive.
    PostTerminalSuccessContinuation,
    /// Watcher may stop quietly because the tmux pane died. Normal completion
    /// must route through tmux death detection rather than post-result idleness.
    Stop,
}

/// Decide whether the tmux output watcher may stop after a terminal-success
/// event. Issue #1137 widened the legacy "exit on terminal success" rule, and
/// issue #1171 makes tmux liveness the only normal watcher-stop authority:
///
/// - dead tmux pane                                      -> Stop
/// - terminal success seen + tmux still alive + (tail
///   has advanced past confirmed_end OR idle window has
///   not elapsed yet)                                    -> PostTerminalSuccessContinuation
/// - otherwise, including terminal success with an alive
///   idle tmux pane                                      -> Continue
pub(crate) fn watcher_stop_decision_after_terminal_success(
    input: WatcherStopInput,
) -> WatcherStopDecision {
    // Tmux death always wins — there's no further output possible, so the
    // watcher must stop regardless of the confirmed-end / idle bookkeeping.
    if !input.tmux_alive {
        return WatcherStopDecision::Stop;
    }

    // Pre-terminal-success: keep the legacy semantics — the loop must
    // continue reading output until it sees a result event.
    if !input.terminal_success_seen {
        return WatcherStopDecision::Continue;
    }

    // Strictness invariant (1): the confirmed-end watermark must reach the
    // current tmux tail. If new bytes have landed past confirmed_end, the
    // watcher cannot stop yet — those bytes still need to be relayed.
    let confirmed_caught_up = input.confirmed_end >= input.tmux_tail_offset;
    if !confirmed_caught_up {
        return WatcherStopDecision::PostTerminalSuccessContinuation;
    }

    // Alive tmux owns the watcher until the tmux liveness monitor observes
    // death. The idle window is now only a continuation classifier.
    match input.idle_duration {
        Some(idle) if idle >= input.idle_threshold => WatcherStopDecision::Continue,
        _ => WatcherStopDecision::PostTerminalSuccessContinuation,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TmuxLivenessDecision {
    Continue,
    QuietStop,
    TmuxDied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WatcherOutputPollDecision {
    DrainOutput,
    Continue,
    QuietStop,
    TmuxDied,
}

pub(super) fn tmux_liveness_decision(
    cancelled: bool,
    shutting_down: bool,
    tmux_alive: bool,
) -> TmuxLivenessDecision {
    if cancelled || shutting_down {
        TmuxLivenessDecision::QuietStop
    } else if tmux_alive {
        TmuxLivenessDecision::Continue
    } else {
        TmuxLivenessDecision::TmuxDied
    }
}

pub(super) fn watcher_output_poll_decision(
    bytes_read: usize,
    liveness_after_empty_read: Option<TmuxLivenessDecision>,
) -> WatcherOutputPollDecision {
    if bytes_read > 0 {
        return WatcherOutputPollDecision::DrainOutput;
    }

    match liveness_after_empty_read.expect("empty watcher read must probe tmux liveness") {
        TmuxLivenessDecision::Continue => WatcherOutputPollDecision::Continue,
        TmuxLivenessDecision::QuietStop => WatcherOutputPollDecision::QuietStop,
        TmuxLivenessDecision::TmuxDied => WatcherOutputPollDecision::TmuxDied,
    }
}

pub(super) async fn probe_tmux_session_liveness(tmux_session_name: &str) -> bool {
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking({
            let name = tmux_session_name.to_string();
            move || tmux_session_has_live_pane(&name)
        }),
    )
    .await
    .unwrap_or(Ok(false))
    .unwrap_or(false)
}

pub(super) async fn handle_tmux_watcher_observed_death(
    channel_id: ChannelId,
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    output_path: &str,
    watcher_provider: &ProviderKind,
    prompt_too_long_killed: bool,
    turn_result_relayed: bool,
) {
    let ts = chrono::Local::now().format("%H:%M:%S");
    let diagnostic = build_tmux_death_diagnostic(tmux_session_name, Some(output_path));
    if let Some(diag) = diagnostic.as_deref() {
        tracing::info!(
            "  [{ts}] 👁 tmux session {tmux_session_name} ended, watcher stopping ({diag})"
        );
    } else {
        tracing::info!("  [{ts}] 👁 tmux session {tmux_session_name} ended, watcher stopping");
    }
    let reason_short = read_tmux_exit_reason(tmux_session_name);
    let is_normal_completion =
        tmux_death_is_normal_completion(reason_short.as_deref(), diagnostic.as_deref());
    // The watcher cleanup path that follows an explicit cancel (user removed
    // the activity reaction or invoked /stop) writes
    // `record_tmux_exit_reason("watcher cleanup: dead session after turn")`
    // and tears the session down. Without this gate that synthetic reason
    // surfaces as a 🔴 lifecycle notification AND as the "대화를 이어붙이지
    // 못했습니다" handoff — both of which are noise for a user who just
    // canceled the turn themselves. The same suppression applies to the
    // immediate-respawn watcher death that can fire seconds later when the
    // next message arrives, since both are direct consequences of the cancel.
    let cancel_induced = cancel_induced_watcher_death_async(
        channel_id,
        tmux_session_name,
        tmux_output_offset(tmux_session_name),
        shared.pg_pool.as_ref(),
    )
    .await;
    // Notify: tmux session termination with reason
    if cancel_induced {
        tracing::info!(
            "  [{ts}] 👁 tmux session {tmux_session_name} ended after recent cancel/turn-stop, skipping lifecycle notification + restart handoff"
        );
        let outcome = trigger_missing_inflight_reattach(
            http,
            shared,
            watcher_provider,
            channel_id,
            tmux_session_name,
        );
        log_missing_inflight_reattach_outcome(
            channel_id,
            tmux_session_name,
            outcome,
            "cancel_induced_watcher_death",
        );
    } else if !is_normal_completion {
        if let Some(reason_text) = tmux_death_lifecycle_notification_reason(reason_short.as_deref())
        {
            let reason_truncated: String = reason_text.chars().take(100).collect();
            let session_key = super::super::adk_session::build_adk_session_key(
                shared,
                channel_id,
                watcher_provider,
            )
            .await
            .unwrap_or_else(|| {
                format!(
                    "{}:{}",
                    crate::services::platform::hostname_short(),
                    tmux_session_name
                )
            });
            enqueue_lifecycle_notification_best_effort(
                sqlite_runtime_db(shared.as_ref()),
                shared.pg_pool.as_ref(),
                &format!("channel:{}", channel_id.get()),
                Some(session_key.as_str()),
                lifecycle_reason_code_for_tmux_exit(reason_text),
                &format!("🔴 세션 종료: {reason_truncated}"),
            );
        } else {
            tracing::info!(
                "  [{ts}] 👁 tmux session {tmux_session_name} ended without an actionable lifecycle reason, skipping lifecycle notification"
            );
        }
    } else {
        tracing::info!(
            "  [{ts}] 👁 tmux session {tmux_session_name} ended after normal completion, skipping lifecycle notification"
        );
    }
    if !cancel_induced && !prompt_too_long_killed && !turn_result_relayed {
        // Suppress warning for normal dispatch completion — not an error.
        let suppress_restart = is_normal_completion
            || reason_short
                .as_deref()
                .is_some_and(tmux_exit_reason_is_normal_completion);
        if !suppress_restart {
            let _ = resume_aborted_restart_turn(
                channel_id,
                http,
                shared,
                tmux_session_name,
                output_path,
            )
            .await;
        }
    }
}

pub(super) fn extract_result_error_text(value: &serde_json::Value) -> String {
    let errors = value
        .get("errors")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::trim))
                .filter(|item| !item.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    if !errors.trim().is_empty() {
        errors
    } else {
        value
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    }
}

pub(super) fn load_restored_session_cwd(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    session_keys: &[String],
) -> Option<String> {
    if let Some(pg_pool) = pg_pool {
        let session_keys = session_keys.to_vec();
        return crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |pool| async move {
                for session_key in session_keys {
                    let path = sqlx::query_scalar::<_, String>(
                        "SELECT cwd FROM sessions WHERE session_key = $1 LIMIT 1",
                    )
                    .bind(&session_key)
                    .fetch_optional(&pool)
                    .await
                    .map_err(|error| format!("load tmux restore cwd {session_key}: {error}"))?;
                    if let Some(path) =
                        path.filter(|path| !path.is_empty() && std::path::Path::new(path).is_dir())
                    {
                        return Ok(Some(path));
                    }
                }
                Ok(None)
            },
            |message| message,
        )
        .ok()
        .flatten();
    }

    let _ = (db, session_keys);
    None
}

pub(super) fn push_transcript_event(
    events: &mut Vec<SessionTranscriptEvent>,
    event: SessionTranscriptEvent,
) {
    let has_payload = !event.content.trim().is_empty()
        || event
            .summary
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        || event
            .tool_name
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
    if has_payload
        || matches!(
            event.kind,
            SessionTranscriptEventKind::Thinking
                | SessionTranscriptEventKind::Result
                | SessionTranscriptEventKind::Error
                | SessionTranscriptEventKind::Task
                | SessionTranscriptEventKind::System
        )
    {
        events.push(event);
    }
}

pub(super) const REDACTED_THINKING_STATUS_LINE: &str = "💭 Thinking...";

pub(super) fn redacted_thinking_transcript_event() -> SessionTranscriptEvent {
    SessionTranscriptEvent {
        kind: SessionTranscriptEventKind::Thinking,
        tool_name: None,
        summary: None,
        content: String::new(),
        status: Some("info".to_string()),
        is_error: false,
    }
}

pub(super) fn inflight_duration_ms(started_at: Option<&str>) -> Option<i64> {
    let started_at = started_at?.trim();
    if started_at.is_empty() {
        return None;
    }
    let parsed = chrono::NaiveDateTime::parse_from_str(started_at, "%Y-%m-%d %H:%M:%S").ok()?;
    let elapsed = chrono::Local::now().naive_local() - parsed;
    Some(elapsed.num_milliseconds().max(0))
}

pub(super) fn load_restored_provider_session_id(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    channel_name: &str,
) -> Option<String> {
    let tmux_name = provider.build_tmux_session_name(channel_name);
    let session_keys =
        super::super::adk_session::build_session_key_candidates(token_hash, provider, &tmux_name);

    if let Some(pg_pool) = pg_pool {
        let session_keys = session_keys.clone();
        let provider_name = provider.as_str().to_string();
        return crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |pool| async move {
                for session_key in session_keys {
                    let session_id = sqlx::query_scalar::<_, Option<String>>(
                        "SELECT claude_session_id
                         FROM sessions
                         WHERE session_key = $1 AND provider = $2
                         LIMIT 1",
                    )
                    .bind(&session_key)
                    .bind(&provider_name)
                    .fetch_optional(&pool)
                    .await
                    .map_err(|error| format!("load tmux provider session {session_key}: {error}"))?
                    .flatten();
                    if let Some(session_id) = session_id.filter(|session_id| !session_id.is_empty())
                    {
                        return Ok(Some(session_id));
                    }
                }
                Ok(None)
            },
            |message| message,
        )
        .ok()
        .flatten();
    }

    let _ = (db, session_keys);
    None
}

pub(super) fn recovery_handled_channel_key(channel_id: u64) -> String {
    format!("recovery_handled_channel:{channel_id}")
}

pub(super) fn sqlite_runtime_db(shared: &SharedData) -> Option<&crate::db::Db> {
    if shared.pg_pool.is_some() {
        None
    } else {
        None::<&crate::db::Db>
    }
}

pub(super) fn watcher_has_post_work_ready_evidence(
    full_response: &str,
    tool_state: &WatcherToolState,
    task_notification_kind: Option<TaskNotificationKind>,
) -> bool {
    !full_response.trim().is_empty() || tool_state.any_tool_used || task_notification_kind.is_some()
}

pub(super) fn normalize_human_alert_target(channel: &str) -> Option<String> {
    let channel = channel.trim();
    if channel.is_empty() {
        return None;
    }
    Some(if channel.starts_with("channel:") {
        channel.to_string()
    } else {
        format!("channel:{channel}")
    })
}

pub(super) fn load_human_alert_target(shared: &SharedData) -> Option<String> {
    if let Some(pool) = shared.pg_pool.as_ref() {
        return crate::utils::async_bridge::block_on_pg_result(
            pool,
            |pool| async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT value FROM kv_meta WHERE key = 'kanban_human_alert_channel_id'",
                )
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load postgres human alert target: {error}"))
            },
            |message| message,
        )
        .ok()
        .flatten()
        .and_then(|channel| normalize_human_alert_target(&channel));
    }

    let _ = shared;
    None
}

pub(super) fn merge_card_label_metadata(existing_metadata: Option<&str>, label: &str) -> String {
    let mut metadata = existing_metadata
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();

    let mut labels = metadata
        .get("labels")
        .and_then(|value| value.as_str())
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !labels.iter().any(|existing| existing == label) {
        labels.push(label.to_string());
    }
    metadata.insert(
        "labels".to_string(),
        serde_json::Value::String(labels.join(",")),
    );

    serde_json::Value::Object(metadata).to_string()
}

pub(super) async fn update_card_ready_failure_marker_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
    reason: &str,
) -> Result<bool, String> {
    let existing_metadata = sqlx::query_scalar::<_, Option<String>>(
        "SELECT metadata::text FROM kanban_cards WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres card metadata for {card_id}: {error}"))?
    .flatten();
    let metadata_json =
        merge_card_label_metadata(existing_metadata.as_deref(), READY_FOR_INPUT_STUCK_LABEL);
    let updated = sqlx::query(
        "UPDATE kanban_cards
         SET metadata = $1::jsonb,
             blocked_reason = $2,
             updated_at = NOW()
         WHERE id = $3",
    )
    .bind(metadata_json)
    .bind(reason)
    .bind(card_id)
    .execute(pool)
    .await
    .map_err(|error| format!("update postgres ready marker for {card_id}: {error}"))?
    .rows_affected();
    Ok(updated > 0)
}

pub(super) fn load_dispatch_card_id(shared: &SharedData, dispatch_id: &str) -> Option<String> {
    if let Some(pool) = shared.pg_pool.as_ref() {
        let dispatch_id = dispatch_id.to_string();
        return crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |pool| async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT kanban_card_id FROM task_dispatches WHERE id = $1",
                )
                .bind(dispatch_id)
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load postgres dispatch card id: {error}"))
            },
            |message| message,
        )
        .ok()
        .flatten();
    }

    let _ = (shared, dispatch_id);
    None
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct ReadyForInputFailureResult {
    pub dispatch_failed: bool,
    pub card_id: Option<String>,
    pub card_marked: bool,
    pub human_alert_sent: bool,
}

pub(in crate::services::discord) async fn fail_dispatch_for_ready_for_input_stall(
    shared: &Arc<SharedData>,
    dispatch_id: &str,
    tmux_session_name: &str,
) -> Result<ReadyForInputFailureResult, String> {
    let payload = serde_json::json!({
        "reason": READY_FOR_INPUT_STUCK_REASON,
        "failure_kind": READY_FOR_INPUT_STUCK_LABEL,
        "tmux_session_name": tmux_session_name,
    });
    let changed = crate::dispatch::set_dispatch_status_with_backends(
        sqlite_runtime_db(shared.as_ref()),
        shared.pg_pool.as_ref(),
        dispatch_id,
        "failed",
        Some(&payload),
        "tmux_ready_for_input_stuck",
        Some(&["pending", "dispatched"]),
        false,
    )
    .map_err(|error| format!("mark dispatch {dispatch_id} failed for ready stall: {error}"))?;

    let card_id = load_dispatch_card_id(shared.as_ref(), dispatch_id);
    let mut card_marked = false;
    if let Some(card_id_ref) = card_id.as_deref() {
        card_marked = if let Some(pool) = shared.pg_pool.as_ref() {
            update_card_ready_failure_marker_pg(pool, card_id_ref, READY_FOR_INPUT_STUCK_REASON)
                .await?
        } else {
            false
        };
    }

    let human_alert_sent = if changed > 0 {
        load_human_alert_target(shared.as_ref()).is_some_and(|target| {
            let card_label = card_id.as_deref().unwrap_or("-");
            let content = format!(
                "자동큐 safety-net 발동: dispatch {dispatch_id} / card {card_label} / session {tmux_session_name} / {READY_FOR_INPUT_STUCK_REASON}"
            );
            enqueue_lifecycle_notification_best_effort(
                sqlite_runtime_db(shared.as_ref()),
                shared.pg_pool.as_ref(),
                &target,
                Some(dispatch_id),
                "dispatch.stuck_at_ready",
                &content,
            )
        })
    } else {
        false
    };

    Ok(ReadyForInputFailureResult {
        dispatch_failed: changed > 0,
        card_id,
        card_marked,
        human_alert_sent,
    })
}

pub(in crate::services::discord) fn recovery_handled_channel_exists(
    shared: &SharedData,
    channel_id: u64,
) -> bool {
    let key = recovery_handled_channel_key(channel_id);

    if let Ok(value) = super::super::internal_api::get_kv_value(&key) {
        return value.is_some();
    }

    if let Some(pg_pool) = shared.pg_pool.as_ref() {
        return crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |pool| async move {
                sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS(
                         SELECT 1
                         FROM kv_meta
                         WHERE key = $1
                           AND (expires_at IS NULL OR expires_at > NOW())
                     )",
                )
                .bind(&key)
                .fetch_one(&pool)
                .await
                .map_err(|error| format!("load recovery handled marker {key}: {error}"))
            },
            |message| message,
        )
        .unwrap_or(false);
    }

    let _ = (shared, key);
    false
}

pub(in crate::services::discord) async fn store_recovery_handled_channels(
    shared: &SharedData,
    channel_ids: &[u64],
) {
    if channel_ids.is_empty() {
        return;
    }

    let marker_value = chrono::Utc::now().timestamp().to_string();
    let mut stored_via_internal_api = true;
    for channel_id in channel_ids {
        let key = recovery_handled_channel_key(*channel_id);
        if let Err(error) = super::super::internal_api::set_kv_value(&key, &marker_value) {
            tracing::debug!(
                "recovery handled marker fallback for {key}: direct runtime API unavailable: {error}"
            );
            stored_via_internal_api = false;
            break;
        }
    }
    if stored_via_internal_api {
        return;
    }

    if let Some(pg_pool) = shared.pg_pool.as_ref() {
        match pg_pool.begin().await {
            Ok(mut tx) => {
                for channel_id in channel_ids {
                    let key = recovery_handled_channel_key(*channel_id);
                    if let Err(error) = sqlx::query(
                        "INSERT INTO kv_meta (key, value, expires_at)
                         VALUES ($1, $2, NULL)
                         ON CONFLICT (key) DO UPDATE
                         SET value = EXCLUDED.value,
                             expires_at = EXCLUDED.expires_at",
                    )
                    .bind(&key)
                    .bind(&marker_value)
                    .execute(&mut *tx)
                    .await
                    {
                        tracing::warn!(
                            "failed to persist recovery handled marker {key} in postgres: {error}"
                        );
                        return;
                    }
                }
                if let Err(error) = tx.commit().await {
                    tracing::warn!("failed to commit recovery handled marker tx: {error}");
                }
            }
            Err(error) => {
                tracing::warn!("failed to begin recovery handled marker tx: {error}");
            }
        }
        return;
    }

    let _ = shared;
}

pub(in crate::services::discord) async fn clear_recovery_handled_channels(shared: &SharedData) {
    if let Err(error) = super::super::internal_api::clear_kv_prefix("recovery_handled_channel:") {
        tracing::debug!(
            "recovery handled marker clear fallback: direct runtime API unavailable: {error}"
        );
    } else {
        return;
    }

    if let Some(pg_pool) = shared.pg_pool.as_ref() {
        if let Err(error) =
            sqlx::query("DELETE FROM kv_meta WHERE key LIKE 'recovery_handled_channel:%'")
                .execute(pg_pool)
                .await
        {
            tracing::warn!("failed to clear recovery handled markers in postgres: {error}");
        }
        return;
    }

    let _ = shared;
}

// Tmux watcher output is activity, but reusing hook_session here would also
// overwrite status/tokens defaults. Touch only last_heartbeat instead.
pub(in crate::services::discord) fn refresh_session_heartbeat_from_tmux_output(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    tmux_session_name: &str,
    thread_channel_id: Option<u64>,
) -> bool {
    let session_keys = super::super::adk_session::build_session_key_candidates(
        token_hash,
        provider,
        tmux_session_name,
    );

    if let Some(pg_pool) = pg_pool {
        let provider_name = provider.as_str().to_string();
        let thread_channel_id = thread_channel_id.map(|value| value.to_string());
        return crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |pool| async move {
                let updated = sqlx::query(
                    "UPDATE sessions
                     SET last_heartbeat = NOW()
                     WHERE session_key = $1 OR session_key = $2",
                )
                .bind(&session_keys[0])
                .bind(&session_keys[1])
                .execute(&pool)
                .await
                .map_err(|error| format!("refresh pg watcher heartbeat by session key: {error}"))?
                .rows_affected();
                if updated > 0 {
                    return Ok(true);
                }

                let Some(thread_channel_id) = thread_channel_id else {
                    return Ok(false);
                };
                let updated = sqlx::query(
                    "UPDATE sessions
                     SET last_heartbeat = NOW()
                     WHERE provider = $1
                       AND thread_channel_id = $2
                       AND status IN ('idle', 'working')",
                )
                .bind(&provider_name)
                .bind(&thread_channel_id)
                .execute(&pool)
                .await
                .map_err(|error| {
                    format!("refresh pg watcher heartbeat by thread channel: {error}")
                })?
                .rows_affected();
                Ok(updated > 0)
            },
            |message| message,
        )
        .unwrap_or(false);
    }

    let _ = (db, provider, thread_channel_id, session_keys);
    false
}

pub(super) fn maybe_refresh_watcher_activity_heartbeat(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    tmux_session_name: &str,
    thread_channel_id: Option<u64>,
    last_heartbeat_at: &mut Option<std::time::Instant>,
) {
    let now = std::time::Instant::now();
    if last_heartbeat_at
        .is_some_and(|last| now.duration_since(last) < WATCHER_ACTIVITY_HEARTBEAT_INTERVAL)
    {
        return;
    }

    if refresh_session_heartbeat_from_tmux_output(
        db,
        pg_pool,
        token_hash,
        provider,
        tmux_session_name,
        thread_channel_id,
    ) {
        *last_heartbeat_at = Some(now);
    }
}

pub(super) async fn clear_provider_session_for_retry(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    tmux_session_name: &str,
    fallback_session_id: Option<&str>,
) {
    let stale_sid = {
        let mut data = shared.core.lock().await;
        let old = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.session_id.clone())
            .or_else(|| fallback_session_id.map(ToString::to_string));
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.clear_provider_session();
        }
        old
    };

    let session_key = format!(
        "{}:{}",
        crate::services::platform::hostname_short(),
        tmux_session_name
    );
    super::super::adk_session::clear_provider_session_id(&session_key, shared.api_port).await;

    if let Some(sid) = stale_sid {
        let _ = super::super::internal_api::clear_stale_session_id(&sid).await;
    }
}

pub(super) async fn resolve_watcher_dispatch_id(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    inflight_state: Option<&super::super::inflight::InflightTurnState>,
) -> Option<String> {
    inflight_state
        .and_then(|state| state.dispatch_id.clone())
        .or_else(|| {
            inflight_state
                .and_then(|state| super::super::adk_session::parse_dispatch_id(&state.user_text))
        })
        .or(
            super::super::adk_session::lookup_pending_dispatch_for_thread(
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
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MissingInflightFallbackPlan {
    pub(super) warn: bool,
    pub(super) trigger_reattach: bool,
    pub(super) suppressed_by_recent_stop: bool,
}

pub(super) fn missing_inflight_fallback_plan(
    inflight_missing: bool,
    dispatch_resolved: bool,
    terminal_output_committed: bool,
    recent_turn_stop: bool,
    _placeholder_cleanup_committed: bool,
    tmux_alive: bool,
) -> MissingInflightFallbackPlan {
    let would_trigger =
        inflight_missing && !dispatch_resolved && terminal_output_committed && tmux_alive;
    let suppressed = recent_turn_stop;
    MissingInflightFallbackPlan {
        warn: inflight_missing,
        trigger_reattach: would_trigger && !suppressed,
        suppressed_by_recent_stop: would_trigger && suppressed,
    }
}

pub(super) fn should_flush_post_terminal_success_continuation(
    terminal_success_seen: bool,
    found_result: bool,
    full_response: &str,
) -> bool {
    terminal_success_seen && !found_result && !full_response.trim().is_empty()
}

pub(super) fn log_missing_inflight_reattach_outcome(
    channel_id: ChannelId,
    tmux_session_name: &str,
    outcome: MissingInflightReattachOutcome,
    origin: &str,
) {
    let ts = chrono::Local::now().format("%H:%M:%S");
    match outcome {
        MissingInflightReattachOutcome::Spawned { replaced_existing } => {
            tracing::info!(
                "  [{ts}] ↻ watcher: 재부착 성공 for channel {} (tmux={}, replaced={}, origin={})",
                channel_id.get(),
                tmux_session_name,
                replaced_existing,
                origin
            );
        }
        MissingInflightReattachOutcome::ReusedExisting { owner_channel_id } => {
            tracing::info!(
                "  [{ts}] ↻ watcher: 재부착 생략 for channel {} — live tmux watcher already owned by channel {} (tmux={}, origin={})",
                channel_id.get(),
                owner_channel_id.get(),
                tmux_session_name,
                origin
            );
        }
        MissingInflightReattachOutcome::SessionDead => {
            tracing::warn!(
                "  [{ts}] ↻ watcher: 재부착 실패 — 추가 진단 필요 for channel {} (tmux={} — session not live, origin={})",
                channel_id.get(),
                tmux_session_name,
                origin
            );
        }
        MissingInflightReattachOutcome::InflightAlreadyExists => {
            tracing::info!(
                "  [{ts}] ↻ watcher: 재부착 생략 for channel {} — concurrent inflight already present (origin={})",
                channel_id.get(),
                origin
            );
        }
        MissingInflightReattachOutcome::SaveFailed => {
            tracing::error!(
                "  [{ts}] ↻ watcher: 재부착 실패 — 추가 진단 필요 for channel {} (tmux={} — inflight save failed, origin={})",
                channel_id.get(),
                tmux_session_name,
                origin
            );
        }
    }
}

pub(super) fn should_suppress_terminal_output_after_recent_stop(
    has_assistant_response: bool,
    inflight_missing: bool,
    recent_turn_stop: bool,
) -> bool {
    should_suppress_streaming_placeholder_after_recent_stop(
        has_assistant_response,
        inflight_missing,
        recent_turn_stop,
    )
}

pub(super) fn should_suppress_streaming_placeholder_after_recent_stop(
    has_assistant_response: bool,
    inflight_missing: bool,
    recent_turn_stop: bool,
) -> bool {
    has_assistant_response && inflight_missing && recent_turn_stop
}

pub(super) async fn wait_for_reacquired_turn_bridge_inflight_state(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    attempts: usize,
    delay: tokio::time::Duration,
) -> bool {
    for attempt in 0..=attempts {
        if super::super::inflight::load_inflight_state(provider, channel_id.get()).is_some_and(
            |state| {
                !state.rebind_origin
                    && state.tmux_session_name.as_deref() == Some(tmux_session_name)
            },
        ) {
            return true;
        }

        if attempt < attempts {
            tokio::time::sleep(delay).await;
        }
    }

    false
}

/// #1136: outcome of an explicit watcher re-attach attempt that the runtime
/// fires when the legacy "inflight missing → DB dispatch fallback" path would
/// have silently dropped the watcher. Returned by
/// [`trigger_missing_inflight_reattach`] so the caller can record the result
/// in logs / counters and surface it to operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MissingInflightReattachOutcome {
    /// A fresh watcher task was spawned for `channel_id`. `replaced_existing`
    /// is `true` if a stale handle was kicked out before the new one took
    /// over. Tagged with `rebind_origin = true` on the inflight state so that
    /// the new attach does NOT itself trigger another DB-fallback reattach.
    Spawned { replaced_existing: bool },
    /// A live watcher already owns the tmux session. The synthetic inflight
    /// metadata was restored, but watcher lifetime remains bound to the
    /// incumbent tmux session rather than forced through a replacement.
    ReusedExisting { owner_channel_id: ChannelId },
    /// The tmux pane is not live — re-attach was skipped. Operators should
    /// investigate whether the session needs to be respawned manually.
    SessionDead,
    /// Another path raced ahead and created an inflight state for the channel
    /// before we could persist ours. The competing inflight wins and we leave
    /// it untouched.
    InflightAlreadyExists,
    /// Persisting the synthetic inflight state failed. Watcher remains
    /// unattached; this is the "재부착 실패 — 추가 진단 필요" branch.
    SaveFailed,
}

pub(super) fn trigger_missing_inflight_reattach(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
) -> MissingInflightReattachOutcome {
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ⚠ watcher: DB dispatch fallback unresolved for channel {} — 재부착 시도 중 (tmux={})",
        channel_id.get(),
        tmux_session_name
    );

    if !tmux_session_has_live_pane(tmux_session_name) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ watcher: 재부착 실패 — 추가 진단 필요 for channel {} — tmux session is not live ({})",
            channel_id.get(),
            tmux_session_name
        );
        return MissingInflightReattachOutcome::SessionDead;
    }

    let (output_path, input_fifo_path) =
        super::super::turn_bridge::tmux_runtime_paths(tmux_session_name);
    let initial_offset = std::fs::metadata(&output_path)
        .map(|meta| meta.len())
        .unwrap_or(0);
    let channel_name =
        parse_provider_and_channel_from_tmux_name(tmux_session_name).map(|(_, name)| name);

    let mut state = super::super::inflight::InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        channel_name.clone(),
        0,
        0,
        0,
        "watcher missing-inflight reattach".to_string(),
        None,
        Some(tmux_session_name.to_string()),
        Some(output_path.clone()),
        Some(input_fifo_path),
        initial_offset,
    );
    state.rebind_origin = true;

    match super::super::inflight::save_inflight_state_create_new(&state) {
        Ok(()) => {}
        Err(super::super::inflight::CreateNewInflightError::AlreadyExists) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: 재부착 실패 — 추가 진단 필요 for channel {} — inflight state already exists",
                channel_id.get()
            );
            return MissingInflightReattachOutcome::InflightAlreadyExists;
        }
        Err(super::super::inflight::CreateNewInflightError::Internal(error)) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::error!(
                "  [{ts}] ❌ watcher: 재부착 실패 — 추가 진단 필요 for channel {} / {} after DB fallback miss: {}",
                channel_id.get(),
                tmux_session_name,
                error
            );
            return MissingInflightReattachOutcome::SaveFailed;
        }
    }

    if let Ok(mut data) = shared.core.try_lock() {
        let session =
            data.sessions
                .entry(channel_id)
                .or_insert_with(|| super::super::DiscordSession {
                    session_id: None,
                    memento_context_loaded: false,
                    memento_reflected: false,
                    current_path: None,
                    history: Vec::new(),
                    pending_uploads: Vec::new(),
                    cleared: false,
                    remote_profile_name: None,
                    channel_id: Some(channel_id.get()),
                    channel_name: channel_name.clone(),
                    category_name: None,
                    last_active: tokio::time::Instant::now(),
                    worktree: None,
                    born_generation: super::super::runtime_store::load_generation(),
                    assistant_turns: 0,
                });
        session.channel_id = Some(channel_id.get());
        session.last_active = tokio::time::Instant::now();
        if session.channel_name.is_none() {
            session.channel_name = channel_name.clone();
        }
    } else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ watcher: explicit reattach could not refresh in-memory session for channel {} because core state was busy",
            channel_id.get()
        );
    }

    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let resume_offset = Arc::new(std::sync::Mutex::new(None::<u64>));
    let pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let turn_delivered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let last_heartbeat_ts_ms = Arc::new(std::sync::atomic::AtomicI64::new(
        super::super::tmux_watcher_now_ms(),
    ));
    let mailbox_finalize_owed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = TmuxWatcherHandle {
        tmux_session_name: tmux_session_name.to_string(),
        paused: paused.clone(),
        resume_offset: resume_offset.clone(),
        cancel: cancel.clone(),
        pause_epoch: pause_epoch.clone(),
        turn_delivered: turn_delivered.clone(),
        last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
        mailbox_finalize_owed: mailbox_finalize_owed.clone(),
    };
    let claim = claim_or_reuse_watcher(
        &shared.tmux_watchers,
        channel_id,
        handle,
        provider,
        "watcher_missing_inflight_fallback",
    );
    if claim.should_spawn() {
        record_recent_watcher_reattach_offset(channel_id, tmux_session_name, initial_offset);
        shared.record_tmux_watcher_reconnect(channel_id);
        tokio::spawn(tmux_output_watcher(
            channel_id,
            http.clone(),
            shared.clone(),
            output_path,
            tmux_session_name.to_string(),
            initial_offset,
            cancel,
            paused,
            resume_offset,
            pause_epoch,
            turn_delivered,
            last_heartbeat_ts_ms,
            mailbox_finalize_owed,
        ));
    } else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ watcher: live tmux watcher reused for channel {} (owner={}, tmux={}, outcome={})",
            channel_id.get(),
            claim.owner_channel_id().get(),
            tmux_session_name,
            claim.as_str()
        );
        return MissingInflightReattachOutcome::ReusedExisting {
            owner_channel_id: claim.owner_channel_id(),
        };
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ↻ watcher: reattach triggered for channel {} (tmux={}, offset={}, outcome={})",
        channel_id.get(),
        tmux_session_name,
        initial_offset,
        claim.as_str()
    );
    MissingInflightReattachOutcome::Spawned {
        replaced_existing: claim.replaced_existing(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatcherClaimAction {
    SpawnFresh,
    SpawnReplacedStale,
    SpawnReplacedDifferentSession,
    ReuseExisting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WatcherClaimOutcome {
    action: WatcherClaimAction,
    owner_channel_id: ChannelId,
}

impl WatcherClaimOutcome {
    fn new(action: WatcherClaimAction, owner_channel_id: ChannelId) -> Self {
        Self {
            action,
            owner_channel_id,
        }
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    pub(crate) fn action(self) -> WatcherClaimAction {
        self.action
    }

    pub(crate) fn owner_channel_id(self) -> ChannelId {
        self.owner_channel_id
    }

    pub(crate) fn should_spawn(self) -> bool {
        matches!(
            self.action,
            WatcherClaimAction::SpawnFresh
                | WatcherClaimAction::SpawnReplacedStale
                | WatcherClaimAction::SpawnReplacedDifferentSession
        )
    }

    pub(crate) fn replaced_existing(self) -> bool {
        matches!(
            self.action,
            WatcherClaimAction::SpawnReplacedStale
                | WatcherClaimAction::SpawnReplacedDifferentSession
        )
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self.action {
            WatcherClaimAction::SpawnFresh => "spawn_fresh",
            WatcherClaimAction::SpawnReplacedStale => "spawn_replaced_stale",
            WatcherClaimAction::SpawnReplacedDifferentSession => "spawn_replaced_different_session",
            WatcherClaimAction::ReuseExisting => "reuse_existing",
        }
    }
}

pub(super) fn find_watcher_by_tmux_session(
    watchers: &TmuxWatcherRegistry,
    tmux_session_name: &str,
) -> Option<(ChannelId, bool)> {
    watchers
        .owner_channel_for_tmux_session(tmux_session_name)
        .zip(watchers.tmux_session_is_stale(tmux_session_name))
}

/// #226/#1170: Atomically claim a tmux session for watcher creation.
/// Returns true if the claim succeeded (caller should spawn the watcher).
/// Returns false if a watcher already exists (caller should skip).
pub(in crate::services::discord) fn try_claim_watcher(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
) -> bool {
    let guard = lock_tmux_watcher_registry();
    let requested_tmux = handle.tmux_session_name.clone();
    if let Some(existing) = find_watcher_by_tmux_session(watchers, &requested_tmux) {
        if existing.1 {
            watchers.remove_tmux_session_locked(&guard, &requested_tmux);
        } else {
            record_watcher_invariant(
                true,
                None,
                channel_id,
                "watcher_one_per_tmux_session",
                "src/services/discord/tmux.rs:try_claim_watcher",
                "same tmux session must reuse the live watcher slot",
                serde_json::json!({
                    "existing_channel_id": existing.0.get(),
                    "tmux_session_name": requested_tmux,
                    "watcher_slots": watchers.len(),
                }),
            );
            return false;
        }
    }
    let claimed = if watchers.contains_key(&channel_id) {
        false
    } else {
        watchers.insert_locked(&guard, channel_id, handle);
        true
    };
    let slot_present = watchers.contains_key(&channel_id);
    record_watcher_invariant(
        slot_present,
        None,
        channel_id,
        "watcher_one_per_channel",
        "src/services/discord/tmux.rs:try_claim_watcher",
        "watcher claim must leave a single channel-owned watcher slot",
        serde_json::json!({
            "claimed": claimed,
            "watcher_slots": watchers.len(),
        }),
    );
    debug_assert!(
        slot_present,
        "watcher claim must leave a channel-owned watcher slot"
    );
    claimed
}

/// Claim a channel for watcher creation with the #1135 single-watcher policy.
///
/// Same tmux session:
/// - live incumbent: reuse it and do not spawn another watcher;
/// - cancelled incumbent: remove it and spawn the requested watcher.
///
/// Same channel but a different tmux session still replaces the incumbent. That
/// preserves the existing new-turn recovery behavior without allowing two
/// owners for one tmux session.
pub(in crate::services::discord) fn claim_or_reuse_watcher(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
    provider: &ProviderKind,
    source: &str,
) -> WatcherClaimOutcome {
    claim_watcher(watchers, channel_id, handle, provider, source)
}

pub(super) fn claim_watcher(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
    provider: &ProviderKind,
    source: &str,
) -> WatcherClaimOutcome {
    let guard = lock_tmux_watcher_registry();
    let requested_tmux = handle.tmux_session_name.clone();
    let mut removed_stale_same_tmux = false;

    if let Some((existing_channel_id, existing_cancelled)) =
        find_watcher_by_tmux_session(watchers, &requested_tmux)
    {
        if existing_cancelled {
            if let Some((_, existing_handle)) =
                watchers.remove_tmux_session_locked(&guard, &requested_tmux)
            {
                existing_handle
                    .cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            removed_stale_same_tmux = true;
        } else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher reuse for channel {} — tmux {} is already watched by channel {}",
                channel_id,
                requested_tmux,
                existing_channel_id
            );
            record_watcher_invariant(
                true,
                Some(provider),
                channel_id,
                "watcher_one_per_tmux_session",
                "src/services/discord/tmux.rs:claim_or_reuse_watcher",
                "same tmux session must reuse the live watcher slot",
                serde_json::json!({
                    "source": source,
                    "existing_channel_id": existing_channel_id.get(),
                    "tmux_session_name": requested_tmux,
                    "watcher_slots": watchers.len(),
                }),
            );
            return WatcherClaimOutcome::new(
                WatcherClaimAction::ReuseExisting,
                existing_channel_id,
            );
        }
    }

    let outcome = if let Some(entry) = watchers.get(&channel_id) {
        let previous_tmux = entry.tmux_session_name.clone();
        let same_tmux = previous_tmux == requested_tmux;
        entry
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let stale_cancelled = entry.cancel.load(std::sync::atomic::Ordering::Relaxed);
        record_watcher_invariant(
            stale_cancelled,
            Some(provider),
            channel_id,
            "watcher_replacement_cancels_stale",
            "src/services/discord/tmux.rs:claim_or_reuse_watcher",
            "replacing a watcher must cancel the stale watcher before installing the new handle",
            serde_json::json!({
                "source": source,
                "same_tmux": same_tmux,
                "previous_tmux_session_name": previous_tmux,
                "tmux_session_name": requested_tmux.as_str(),
            }),
        );
        debug_assert!(
            stale_cancelled,
            "stale watcher must be cancelled before replacement"
        );
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ♻ watcher replaced for channel {} — cancelled stale watcher",
            channel_id
        );
        drop(entry);
        watchers.insert_locked(&guard, channel_id, handle);
        crate::services::observability::emit_watcher_replaced(
            provider.as_str(),
            channel_id.get(),
            source,
        );
        if same_tmux {
            WatcherClaimOutcome::new(WatcherClaimAction::SpawnReplacedStale, channel_id)
        } else {
            WatcherClaimOutcome::new(
                WatcherClaimAction::SpawnReplacedDifferentSession,
                channel_id,
            )
        }
    } else {
        watchers.insert_locked(&guard, channel_id, handle);
        if removed_stale_same_tmux {
            WatcherClaimOutcome::new(WatcherClaimAction::SpawnReplacedStale, channel_id)
        } else {
            WatcherClaimOutcome::new(WatcherClaimAction::SpawnFresh, channel_id)
        }
    };
    let slot_present = watchers.contains_key(&channel_id);
    record_watcher_invariant(
        slot_present,
        Some(provider),
        channel_id,
        "watcher_one_per_channel",
        "src/services/discord/tmux.rs:claim_or_reuse_watcher",
        "watcher replacement must leave exactly one channel-owned watcher slot",
        serde_json::json!({
            "outcome": outcome.as_str(),
            "source": source,
            "watcher_slots": watchers.len(),
        }),
    );
    debug_assert!(
        slot_present,
        "watcher replacement must leave a channel-owned watcher slot"
    );
    outcome
}

use crate::services::tmux_common::{current_tmux_owner_marker, tmux_owner_path};

pub(in crate::services::discord) fn session_belongs_to_current_runtime(
    session_name: &str,
    current_owner_marker: &str,
) -> bool {
    std::fs::read_to_string(tmux_owner_path(session_name))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| value == current_owner_marker)
        .unwrap_or(false)
}

/// On startup, scan for surviving tmux sessions (AgentDesk-*) and restore watchers.
/// This handles the case where AgentDesk was restarted but tmux sessions are still alive.
pub(in crate::services::discord) async fn restore_tmux_watchers(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
) {
    let settings_snapshot = { shared.settings.read().await.clone() };
    let provider = settings_snapshot.provider.clone();

    // List tmux sessions matching our naming convention
    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(crate::services::platform::tmux::list_session_names),
    )
    .await
    {
        Ok(Ok(Ok(names))) => names,
        _ => return, // No tmux, timeout, or no sessions
    };

    let agent_sessions: Vec<&str> = output
        .iter()
        .map(|l| l.trim())
        .filter(|l| {
            parse_provider_and_channel_from_tmux_name(l)
                .map(|(session_provider, _)| session_provider == provider)
                .unwrap_or(false)
        })
        .collect();

    if agent_sessions.is_empty() {
        return;
    }

    // Build channel name → ChannelId map from Discord API (sessions map may be empty after restart)
    let mut name_to_channel: std::collections::HashMap<String, (ChannelId, String)> =
        std::collections::HashMap::new();

    // Try from in-memory sessions first
    {
        let data = shared.core.lock().await;
        for (&ch_id, session) in &data.sessions {
            if let Some(ref ch_name) = session.channel_name {
                let tmux_name = provider.build_tmux_session_name(ch_name);
                name_to_channel.insert(tmux_name, (ch_id, ch_name.clone()));
            }
        }
    }

    // If in-memory sessions don't cover all tmux sessions, fetch from Discord API
    let unresolved: Vec<&&str> = agent_sessions
        .iter()
        .filter(|s| !name_to_channel.contains_key(**s))
        .collect();

    if !unresolved.is_empty() {
        // Fetch guild channels via Discord API
        if let Ok(guilds) = http.get_guilds(None, None).await {
            for guild_info in &guilds {
                if let Ok(channels) = guild_info.id.channels(http).await {
                    for (ch_id, channel) in &channels {
                        let role_binding = resolve_role_binding(*ch_id, Some(&channel.name));
                        if !channel_supports_provider(
                            &provider,
                            Some(&channel.name),
                            false,
                            role_binding.as_ref(),
                        ) {
                            continue;
                        }
                        let tmux_name = provider.build_tmux_session_name(&channel.name);
                        name_to_channel
                            .entry(tmux_name)
                            .or_insert((*ch_id, channel.name.clone()));
                    }
                }
            }
        }

        // Fallback for thread sessions: guild.channels() doesn't return threads.
        // Extract thread_id from the channel name suffix (-t{id}) and use it
        // as the channel_id directly, since Discord thread IDs are channel IDs.
        let still_unresolved: Vec<&&str> = agent_sessions
            .iter()
            .filter(|s| !name_to_channel.contains_key(**s))
            .collect();
        for session_name in &still_unresolved {
            if let Some((_, ch_name)) = parse_provider_and_channel_from_tmux_name(session_name) {
                if let Some(pos) = ch_name.rfind("-t") {
                    let suffix = &ch_name[pos + 2..];
                    if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
                        if let Ok(thread_id) = suffix.parse::<u64>() {
                            let channel_id = ChannelId::new(thread_id);
                            name_to_channel
                                .entry(session_name.to_string())
                                .or_insert((channel_id, ch_name.clone()));
                        }
                    }
                }
            }
        }
    }

    // Collect sessions to restore
    struct PendingWatcher {
        channel_id: ChannelId,
        output_path: String,
        session_name: String,
        initial_offset: u64,
        restored_turn: Option<RestoredWatcherTurn>,
    }

    // Dead sessions that need DB cleanup (idle status report + tmux kill)
    struct DeadSessionCleanup {
        channel_id: u64,
        channel_name: String,
        session_name: String,
    }

    let mut pending: Vec<PendingWatcher> = Vec::new();
    let mut dead_cleanups: Vec<DeadSessionCleanup> = Vec::new();
    let mut owned_sessions: std::collections::HashMap<ChannelId, String> =
        std::collections::HashMap::new();

    for session_name in &agent_sessions {
        let Some((channel_id, channel_name)) = name_to_channel.get(*session_name) else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — channel mapping not found",
                session_name
            );
            continue;
        };

        // #148: Do NOT register in owned_sessions yet — QUARANTINE check below may
        // skip this session. Registering early blocks new session creation for the channel.
        let is_dm = matches!(
            channel_id.to_channel(http.as_ref()).await,
            Ok(serenity::model::channel::Channel::Private(_))
        );
        // Resolve thread parent so validation uses the same semantics
        // as normal message routing (router.rs).
        let (allowlist_channel_id, provider_channel_name) = if let Some((pid, pname)) =
            super::super::resolve_thread_parent(http, *channel_id).await
        {
            (pid, pname.unwrap_or_else(|| channel_name.clone()))
        } else {
            (*channel_id, channel_name.clone())
        };
        if let Err(reason) = validate_bot_channel_routing_with_provider_channel(
            &settings_snapshot,
            &provider,
            allowlist_channel_id,
            Some(&channel_name),
            Some(&provider_channel_name),
            is_dm,
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — {reason} for channel {}",
                session_name,
                channel_id
            );
            continue;
        }

        if let Some(started) = super::super::mailbox_snapshot(&shared, *channel_id)
            .await
            .recovery_started_at
        {
            if started.elapsed() < std::time::Duration::from_secs(60) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏳ watcher skip for {} — recovery in progress ({:.0}s ago)",
                    session_name,
                    started.elapsed().as_secs_f64()
                );
                continue;
            }
            // Stale recovery — remove marker and proceed with watcher
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ clearing stale recovery marker for {} ({:.0}s elapsed)",
                session_name,
                started.elapsed().as_secs_f64()
            );
            super::super::mailbox_clear_recovery_marker(&shared, *channel_id).await;
        }

        if let Some((owner_channel_id, cancelled)) =
            find_watcher_by_tmux_session(&shared.tmux_watchers, session_name)
        {
            if !cancelled {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — tmux session already watched by channel {}",
                    session_name,
                    owner_channel_id
                );
                continue;
            }
        }

        // Accept either the new persistent location or the legacy /tmp
        // location — older wrappers still write to /tmp, and a dcserver
        // restart that lost /tmp files should not falsely flag a live
        // session as "no output file". See issue #892.
        let Some(output_path) =
            crate::services::tmux_common::resolve_session_temp_path(session_name, "jsonl")
        else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — no output file",
                session_name
            );
            continue;
        };

        // Old-gen sessions: adopt instead of killing.
        // The tmux session and Claude CLI process are still alive from the
        // previous dcserver — just update the generation marker and re-attach
        // a watcher. Auto-retry handles stale Claude session IDs if needed.
        let gen_marker_path =
            crate::services::tmux_common::session_temp_path(session_name, "generation");
        let session_gen = std::fs::read_to_string(&gen_marker_path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let current_gen = super::super::runtime_store::load_generation();
        if session_gen < current_gen && current_gen > 0 {
            // Skip sessions belonging to other runtimes
            let current_owner_marker = current_tmux_owner_marker();
            if !session_belongs_to_current_runtime(session_name, &current_owner_marker) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — owned by other runtime",
                    session_name
                );
                continue;
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Adopting old-gen session {} (gen {} → {})",
                session_name,
                session_gen,
                current_gen
            );
            // Update generation marker to current gen, preserving the
            // existing mtime.
            //
            // #1275 P2 #1: the `.generation` mtime is the wrapper-identity
            // signal used by `watermark_after_output_regression`. Adoption
            // does NOT respawn the wrapper (the tmux session and Claude CLI
            // process are still alive from the previous dcserver), so the
            // mtime must stay pinned to its original value. Otherwise a
            // restored watcher with `last_watcher_relayed_generation_mtime_ns`
            // captured before the dcserver restart will mismatch the freshly
            // touched `.generation` mtime, the regression check classifies
            // as fresh wrapper, clears `last_relayed_offset`, and a rotated
            // jsonl re-relays surviving content.
            preserve_mtime_after_write(
                &gen_marker_path,
                current_gen.to_string().as_bytes(),
                "adoption_marker_rewrite",
            );
        }

        if !tmux_session_has_live_pane(session_name) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            if let Some(diag) = build_tmux_death_diagnostic(session_name, Some(&output_path)) {
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — tmux pane dead ({diag})",
                    session_name
                );
            } else {
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — tmux pane dead",
                    session_name
                );
            }
            // Schedule DB cleanup + tmux kill for this dead session
            dead_cleanups.push(DeadSessionCleanup {
                channel_id: channel_id.get(),
                channel_name: channel_name.clone(),
                session_name: session_name.to_string(),
            });
            continue;
        }

        // #148: Only register in owned_sessions after passing QUARANTINE + live-pane checks.
        // Earlier registration blocked new session creation for quarantined/dead channels.
        owned_sessions
            .entry(*channel_id)
            .or_insert_with(|| channel_name.clone());

        let mut restored_turn = None;
        let initial_offset = if let Some(state) =
            super::super::inflight::load_inflight_state(&provider, channel_id.get())
        {
            if let Some(restored_tmux) =
                restored_watcher_turn_from_inflight(&state, session_name, false)
            {
                let finish_mailbox_on_completion =
                    super::super::recovery::reregister_active_turn_from_inflight(&shared, &state)
                        .await;
                restored_turn = Some(RestoredWatcherTurn {
                    finish_mailbox_on_completion,
                    ..restored_tmux
                });
                let file_len = std::fs::metadata(&output_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
                if file_len >= state.last_offset {
                    state.last_offset
                } else {
                    0
                }
            } else {
                std::fs::metadata(&output_path)
                    .map(|m| m.len())
                    .unwrap_or(0)
            }
        } else {
            std::fs::metadata(&output_path)
                .map(|m| m.len())
                .unwrap_or(0)
        };

        pending.push(PendingWatcher {
            channel_id: *channel_id,
            output_path,
            session_name: session_name.to_string(),
            initial_offset,
            restored_turn,
        });
    }

    // Register sessions in CoreState so cleanup_orphan_tmux_sessions recognizes them
    // and message handlers find an active session with current_path
    if !owned_sessions.is_empty() {
        let mut data = shared.core.lock().await;
        let sqlite_settings_db = if shared.pg_pool.is_some() {
            None
        } else {
            None::<&crate::db::Db>
        };
        for (channel_id, channel_name) in &owned_sessions {
            let persisted_path = load_last_session_path(
                sqlite_settings_db,
                shared.pg_pool.as_ref(),
                &shared.token_hash,
                channel_id.get(),
            );
            let remote_profile = load_last_remote_profile(
                sqlite_settings_db,
                shared.pg_pool.as_ref(),
                &shared.token_hash,
                channel_id.get(),
            );
            let persisted_session_id = load_restored_provider_session_id(
                sqlite_settings_db,
                shared.pg_pool.as_ref(),
                &shared.token_hash,
                &provider,
                channel_name,
            );
            let configured_path =
                super::super::settings::resolve_workspace(*channel_id, Some(channel_name.as_str()));
            let tmux_name = provider.build_tmux_session_name(channel_name);
            let session_keys = super::super::adk_session::build_session_key_candidates(
                &shared.token_hash,
                &provider,
                &tmux_name,
            );
            let db_cwd = load_restored_session_cwd(
                None::<&crate::db::Db>,
                shared.pg_pool.as_ref(),
                &session_keys,
            );

            let session =
                data.sessions
                    .entry(*channel_id)
                    .or_insert_with(|| super::super::DiscordSession {
                        session_id: persisted_session_id.clone(),
                        memento_context_loaded:
                            super::super::session_runtime::restored_memento_context_loaded(
                                false,
                                None,
                                persisted_session_id.as_deref(),
                            ),
                        memento_reflected: false,
                        current_path: None,
                        history: Vec::new(),
                        pending_uploads: Vec::new(),
                        cleared: false,
                        channel_name: Some(channel_name.clone()),
                        category_name: None,
                        remote_profile_name: remote_profile.clone(),
                        channel_id: Some(channel_id.get()),

                        last_active: tokio::time::Instant::now(),
                        worktree: None,

                        born_generation: super::super::runtime_store::load_generation(),
                        assistant_turns: 0,
                    });

            if session.session_id.is_none() && persisted_session_id.is_some() {
                session.restore_provider_session(persisted_session_id.clone());
            }

            // Restore current_path: DB cwd (worktree-aware) > last_sessions (yaml, main workspace)
            if session.current_path.is_none() {
                if let (Some(configured), Some(restored)) =
                    (configured_path.as_ref(), db_cwd.as_ref())
                {
                    if configured != restored {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ⚠ Ignoring restored DB cwd for channel {}: {} (configured workspace: {})",
                            channel_id,
                            restored,
                            configured
                        );
                    }
                }
                let effective_path = super::super::select_restored_session_path(
                    configured_path,
                    db_cwd,
                    persisted_path,
                    remote_profile.as_deref(),
                );
                if let Some(path) = effective_path {
                    session.current_path = Some(path);
                }
            }
        }
    }

    // Spawn watchers
    // #226: Use try_claim_watcher for atomic check-and-insert. The pending list
    // was built during the scan phase, which includes async Discord API calls.
    // A normal turn may have created a watcher in the meantime.
    for pw in pending {
        // #226: Skip channels that recovery already handled — their watchers may have
        // ended quickly (session died), removing themselves from the DashMap, but we
        // should not create a second watcher because recovery already processed the turn.
        let recovery_handled =
            recovery_handled_channel_exists(shared.as_ref(), pw.channel_id.get());
        if recovery_handled {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — recovery already handled this channel",
                pw.session_name
            );
            continue;
        }

        if pw.restored_turn.is_none() {
            reconcile_orphan_suppressed_placeholder_for_restored_watcher(
                http,
                shared,
                &provider,
                pw.channel_id,
                &pw.session_name,
            )
            .await;
        }

        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let resume_offset = Arc::new(std::sync::Mutex::new(None::<u64>));
        let pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let turn_delivered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let last_heartbeat_ts_ms = Arc::new(std::sync::atomic::AtomicI64::new(
            super::super::tmux_watcher_now_ms(),
        ));
        let mailbox_finalize_owed = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let handle = TmuxWatcherHandle {
            tmux_session_name: pw.session_name.clone(),
            paused: paused.clone(),
            resume_offset: resume_offset.clone(),
            cancel: cancel.clone(),
            pause_epoch: pause_epoch.clone(),
            turn_delivered: turn_delivered.clone(),
            last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
            mailbox_finalize_owed: mailbox_finalize_owed.clone(),
        };
        if !try_claim_watcher(&shared.tmux_watchers, pw.channel_id, handle) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — already watching (created during scan)",
                pw.session_name
            );
            continue;
        }

        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ Restoring tmux watcher for {} (offset {})",
            pw.session_name,
            pw.initial_offset
        );

        shared.record_tmux_watcher_reconnect(pw.channel_id);
        tokio::spawn(tmux_output_watcher_with_restore(
            pw.channel_id,
            http.clone(),
            shared.clone(),
            pw.output_path,
            pw.session_name,
            pw.initial_offset,
            cancel,
            paused,
            resume_offset,
            pause_epoch,
            turn_delivered,
            last_heartbeat_ts_ms,
            mailbox_finalize_owed,
            pw.restored_turn,
        ));
    }

    // Clean up dead sessions: report idle to DB and kill tmux sessions
    if !dead_cleanups.is_empty() {
        let api_port = shared.api_port;
        let provider = shared.settings.read().await.provider.clone();

        let mut cleaned_dead_sessions = 0usize;
        for dc in &dead_cleanups {
            let dispatch_protection =
                super::super::tmux_lifecycle::resolve_dispatch_tmux_protection(
                    None::<&crate::db::Db>,
                    shared.pg_pool.as_ref(),
                    &shared.token_hash,
                    &provider,
                    &dc.session_name,
                    Some(&dc.channel_name),
                );
            let cleanup_plan = dead_session_cleanup_plan(dispatch_protection.is_some());

            if let Some(protection) = dispatch_protection {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ♻ tmux startup: preserving dispatch session {} — {}",
                    dc.session_name,
                    protection.log_reason()
                );
            }

            let tmux_name = provider.build_tmux_session_name(&dc.channel_name);
            let thread_channel_id =
                super::super::adk_session::parse_thread_channel_id_from_name(&dc.channel_name);
            let session_key = super::super::adk_session::build_namespaced_session_key(
                &shared.token_hash,
                &provider,
                &tmux_name,
            );
            let agent_id =
                resolve_role_binding(ChannelId::new(dc.channel_id), Some(&dc.channel_name))
                    .map(|binding| binding.role_id);

            if cleanup_plan.report_idle_status {
                super::super::adk_session::post_adk_session_status(
                    Some(&session_key),
                    Some(&dc.channel_name),
                    None,
                    "idle",
                    &provider,
                    None,
                    None,
                    None,
                    None,
                    thread_channel_id,
                    agent_id.as_deref(),
                    api_port,
                )
                .await;
            }

            if cleanup_plan.preserve_tmux_session {
                continue;
            }

            // Kill the dead tmux session
            let sess = dc.session_name.clone();
            let _ = tokio::task::spawn_blocking(move || {
                crate::services::termination_audit::record_termination_for_tmux(
                    &sess,
                    None,
                    "tmux_startup",
                    "startup_dead_session",
                    Some("startup cleanup: dead session"),
                    None,
                );
                record_tmux_exit_reason(&sess, "startup cleanup: dead session");
                crate::services::platform::tmux::kill_session_with_reason(
                    &sess,
                    "startup cleanup: dead session",
                );
            })
            .await;
            cleaned_dead_sessions += 1;
        }

        if cleaned_dead_sessions > 0 {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🧹 Cleaned {} dead tmux session(s) on startup",
                cleaned_dead_sessions
            );
        }

        // Sweep orphan session temp files (no matching tmux session AND
        // owner marker older than the threshold). Conservative: skip the
        // legacy /tmp directory (those files may still be held open by
        // pre-migration wrappers) — we only clean the new persistent
        // directory. See issue #892.
        sweep_orphan_session_files().await;
    }
}
