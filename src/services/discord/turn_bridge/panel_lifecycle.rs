//! #3479 task/session-panel line rendering + active-placeholder-card helpers,
//! moved verbatim out of turn_bridge/mod.rs (Option A giant-file reduction;
//! behavior-preserving — only visibility prefixes and `super::` depth adjusted).

use super::*;

pub(in crate::services::discord) fn record_placeholder_live_event(
    shared: &SharedData,
    channel_id: ChannelId,
    event: Option<super::super::placeholder_live_events::RecentPlaceholderEvent>,
) {
    if (shared.ui.placeholder_live_events_enabled || shared.ui.status_panel_v2_enabled)
        && let Some(event) = event
    {
        shared
            .ui
            .placeholder_live_events
            .push_event(channel_id, event);
    }
}

pub(super) fn first_request_line(user_text: &str) -> Option<String> {
    user_text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

pub(super) async fn child_progress_line(
    pg_pool: Option<&sqlx::PgPool>,
    parent_session_key: Option<&str>,
) -> Option<String> {
    let (Some(pg_pool), Some(parent_session_key)) = (pg_pool, parent_session_key) else {
        return None;
    };
    match load_child_inventory_by_parent_key_pg(pg_pool, parent_session_key).await {
        Ok(summary) => format_child_inventory_progress(&summary, chrono::Utc::now()),
        Err(error) => {
            tracing::warn!(
                "Failed to load background child inventory for {}: {}",
                parent_session_key,
                error
            );
            None
        }
    }
}

pub(super) async fn refresh_session_panel_line_from_lifecycle(
    shared: &SharedData,
    channel_id: ChannelId,
    turn_id: &str,
    tmux_session_name: Option<&str>,
    // #3983 item4: provider for the one-shot session banner render (provider
    // session id label). Threaded from the bridge call site.
    provider: &crate::services::provider::ProviderKind,
) -> bool {
    let Some(pg_pool) = shared.pg_pool.as_ref() else {
        return false;
    };
    // `session_panel_instance_key` lives in the unix-only `tmux` module; on
    // non-unix targets there is no tmux session, so the instance key is None.
    #[cfg(unix)]
    let session_instance_key = tmux_session_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .and_then(super::super::tmux::session_panel_instance_key);
    #[cfg(not(unix))]
    let session_instance_key = {
        let _ = tmux_session_name;
        Option::<String>::None
    };
    let channel_id_text = channel_id.get().to_string();
    let dirty =
        match crate::services::observability::turn_lifecycle::load_latest_session_lifecycle_event(
            pg_pool,
            &channel_id_text,
            turn_id,
        )
        .await
        {
            Ok(Some(event)) => shared
                .ui
                .placeholder_live_events
                .set_session_panel_lifecycle_event(
                    channel_id,
                    session_instance_key.as_deref(),
                    &event.kind,
                    &event.details_json,
                ),
            Ok(None) => shared
                .ui
                .placeholder_live_events
                .clear_session_panel(channel_id),
            Err(error) => {
                tracing::debug!(
                    "[turn_bridge] failed to load session lifecycle line for turn {} in channel {}: {}",
                    turn_id,
                    channel_id,
                    error
                );
                false
            }
        };
    // #3983 item4: after the snapshot is (re)set, emit the one-shot top session
    // banner. The claim is deduped per session across this sink path and the
    // tmux-watcher path, so calling it on every status tick posts at most once
    // per session (and re-arms on a genuine new-session boundary).
    crate::services::discord::session_banner::emit_session_banner_if_new(
        shared, channel_id, provider,
    )
    .await;
    dirty
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskPanelDispatchMetadata {
    card_id: Option<String>,
    dispatch_type: Option<String>,
    claim_owner: Option<String>,
    card_title: Option<String>,
    dispatch_title: Option<String>,
    github_issue_number: Option<i64>,
}

async fn load_task_panel_dispatch_metadata(
    pg_pool: Option<&sqlx::PgPool>,
    dispatch_id: &str,
) -> Result<Option<TaskPanelDispatchMetadata>, String> {
    let Some(pg_pool) = pg_pool else {
        return Ok(None);
    };
    let row = sqlx::query(
        "SELECT td.kanban_card_id,
                td.dispatch_type,
                td.title AS dispatch_title,
                kc.title AS card_title,
                kc.github_issue_number,
                dox.claim_owner
         FROM task_dispatches td
         LEFT JOIN kanban_cards kc ON kc.id = td.kanban_card_id
         LEFT JOIN dispatch_outbox dox ON dox.dispatch_id = td.id
         WHERE td.id = $1
         ORDER BY dox.created_at DESC NULLS LAST
         LIMIT 1",
    )
    .bind(dispatch_id)
    .fetch_optional(pg_pool)
    .await
    .map_err(|error| format!("load task panel dispatch metadata for {dispatch_id}: {error}"))?;

    row.map(|row| {
        Ok::<TaskPanelDispatchMetadata, String>(TaskPanelDispatchMetadata {
            card_id: row
                .try_get("kanban_card_id")
                .map_err(|error| format!("decode task panel card_id for {dispatch_id}: {error}"))?,
            dispatch_type: row.try_get("dispatch_type").map_err(|error| {
                format!("decode task panel dispatch_type for {dispatch_id}: {error}")
            })?,
            claim_owner: row.try_get("claim_owner").map_err(|error| {
                format!("decode task panel claim_owner for {dispatch_id}: {error}")
            })?,
            card_title: row.try_get("card_title").map_err(|error| {
                format!("decode task panel card_title for {dispatch_id}: {error}")
            })?,
            dispatch_title: row.try_get("dispatch_title").map_err(|error| {
                format!("decode task panel dispatch_title for {dispatch_id}: {error}")
            })?,
            github_issue_number: row.try_get("github_issue_number").map_err(|error| {
                format!("decode task panel github_issue_number for {dispatch_id}: {error}")
            })?,
        })
    })
    .transpose()
}

pub(super) async fn refresh_task_panel_line_from_dispatch(
    shared: &SharedData,
    channel_id: ChannelId,
    dispatch_id: &str,
) -> bool {
    let dispatch_id = dispatch_id.trim();
    if dispatch_id.is_empty() {
        return false;
    }
    let metadata =
        match load_task_panel_dispatch_metadata(shared.pg_pool.as_ref(), dispatch_id).await {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::debug!(
                    "[turn_bridge] failed to load task line for dispatch {} in channel {}: {}",
                    dispatch_id,
                    channel_id,
                    error
                );
                None
            }
        };
    shared.ui.placeholder_live_events.set_task_panel_info(
        channel_id,
        crate::services::discord::placeholder_live_events::TaskPanelInfo {
            dispatch_id,
            card_id: metadata.as_ref().and_then(|value| value.card_id.as_deref()),
            dispatch_type: metadata
                .as_ref()
                .and_then(|value| value.dispatch_type.as_deref()),
            owner_instance_id: metadata
                .as_ref()
                .and_then(|value| value.claim_owner.as_deref()),
            card_title: metadata
                .as_ref()
                .and_then(|value| value.card_title.as_deref()),
            dispatch_title: metadata
                .as_ref()
                .and_then(|value| value.dispatch_title.as_deref()),
            github_issue_number: metadata
                .as_ref()
                .and_then(|value| value.github_issue_number),
        },
    )
}

pub(super) async fn ensure_active_placeholder_card<G: TurnGateway + ?Sized>(
    shared: &SharedData,
    gateway: &G,
    key: super::super::placeholder_controller::PlaceholderKey,
    input: super::super::placeholder_controller::PlaceholderActiveInput,
) -> super::super::placeholder_controller::PlaceholderControllerOutcome {
    if shared.ui.placeholder_live_events_enabled
        && let Some(block) = shared
            .ui
            .placeholder_live_events
            .render_block(key.channel_id)
    {
        return shared
            .ui
            .placeholder_controller
            .ensure_active_with_live_events(gateway, key, input, block)
            .await;
    }
    shared
        .ui
        .placeholder_controller
        .ensure_active(gateway, key, input)
        .await
}
