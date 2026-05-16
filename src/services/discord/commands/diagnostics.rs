use std::fs;
use std::sync::Arc;
use std::time::Instant;

use poise::serenity_prelude as serenity;
use serenity::ChannelId;

use super::super::formatting::{send_long_message_ctx, truncate_str};
use super::super::inflight::{InflightTurnState, load_inflight_states};
use super::super::metrics;
use super::super::runtime_store;
use super::super::{
    Context, Error, Intervention, PendingQueueItem, SharedData, check_auth,
    mailbox_queue_snapshots, mailbox_snapshot,
};
use super::config::current_working_dir;
use crate::services::claude;
use crate::services::gemini;
use crate::services::provider::ProviderKind;
#[cfg(unix)]
use crate::services::tmux_diagnostics::{tmux_session_exists, tmux_session_has_live_pane};

#[cfg(not(unix))]
fn tmux_session_has_live_pane(_name: &str) -> bool {
    false
}
#[cfg(not(unix))]
fn tmux_session_exists(_name: &str) -> bool {
    false
}

fn shorten_session_identifier(value: &str) -> String {
    if value.len() > 24 {
        format!("{}...", &value[..24])
    } else {
        value.to_string()
    }
}

fn inflight_runtime_label(state: &InflightTurnState) -> &'static str {
    state.runtime_kind_for_recovery().label()
}

async fn run_blocking_gemini_command<T, F>(label: &'static str, task: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|error| format!("{label} task failed: {error}"))?
}

async fn fetch_raw_provider_session_id(
    session_key: Option<&str>,
    provider: &ProviderKind,
) -> Option<String> {
    let session_key = session_key?;
    super::super::internal_api::get_provider_session_id(session_key, Some(provider.as_str()))
        .await
        .ok()
        .and_then(|json| {
            json.get("raw_provider_session_id")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
}

fn normalized_pending_queue(queue: Vec<Intervention>) -> Vec<Intervention> {
    normalized_pending_queue_at(queue, Instant::now())
}

fn normalized_pending_queue_at(mut queue: Vec<Intervention>, now: Instant) -> Vec<Intervention> {
    if crate::services::discord::has_soft_intervention_at(&mut queue, now) {
        queue
    } else {
        Vec::new()
    }
}

fn pending_queue_len(queue: &[Intervention]) -> usize {
    normalized_pending_queue(queue.to_vec()).len()
}

fn pending_queue_len_at(queue: &[Intervention], now: Instant) -> usize {
    normalized_pending_queue_at(queue.to_vec(), now).len()
}

fn normalize_pending_queues(
    queues: std::collections::HashMap<ChannelId, Vec<Intervention>>,
) -> std::collections::HashMap<ChannelId, Vec<Intervention>> {
    normalize_pending_queues_at(queues, Instant::now())
}

fn normalize_pending_queues_at(
    queues: std::collections::HashMap<ChannelId, Vec<Intervention>>,
    now: Instant,
) -> std::collections::HashMap<ChannelId, Vec<Intervention>> {
    queues
        .into_iter()
        .filter_map(|(channel_id, queue)| {
            let queue = normalized_pending_queue_at(queue, now);
            if queue.is_empty() {
                None
            } else {
                Some((channel_id, queue))
            }
        })
        .collect()
}

pub(in crate::services::discord) async fn build_health_report(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> String {
    let mailbox_snapshots = shared.mailboxes.snapshot_all().await;
    let channel_snapshot = mailbox_snapshots
        .get(&channel_id)
        .cloned()
        .unwrap_or_default();
    let active_request_count = mailbox_snapshots
        .values()
        .filter(|snapshot| snapshot.cancel_token.is_some())
        .count();
    let queued_channel_count = mailbox_snapshots
        .values()
        .filter(|snapshot| pending_queue_len(&snapshot.intervention_queue) > 0)
        .count();
    let queued_total: usize = mailbox_snapshots
        .values()
        .map(|snapshot| pending_queue_len(&snapshot.intervention_queue))
        .sum();

    let (session_path, session_id, session_channel_name, pending_uploads, session_count) = {
        let data = shared.core.lock().await;
        let session = data.sessions.get(&channel_id);
        (
            session.and_then(|s| s.current_path.clone()),
            session.and_then(|s| s.session_id.clone()),
            session.and_then(|s| s.channel_name.clone()),
            session.map(|s| s.pending_uploads.len()).unwrap_or(0),
            data.sessions.len(),
        )
    };
    let active_request = channel_snapshot.cancel_token.is_some();
    let queued_count = pending_queue_len(&channel_snapshot.intervention_queue);

    let runtime_root = crate::cli::dcserver::agentdesk_runtime_root();
    let current_release = runtime_root
        .as_ref()
        .map(|r| r.join("releases").join("current"))
        .and_then(|p| fs::read_link(p).ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(unknown)".to_string());
    let previous_release = runtime_root
        .as_ref()
        .map(|r| r.join("releases").join("previous"))
        .and_then(|p| fs::read_link(p).ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(none)".to_string());
    let release_label = |value: &str| value.rsplit('/').next().unwrap_or(value).to_string();
    let home_prefix = dirs::home_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let compact_path = |value: String| {
        if value.starts_with(&home_prefix) {
            value.replacen(&home_prefix, "~", 1)
        } else {
            value
        }
    };
    let inflight_states = load_inflight_states(&provider);
    let inflight_count = inflight_states.len();
    let channel_inflight = inflight_states
        .iter()
        .find(|s| s.channel_id == channel_id.get());
    let recovering_count = mailbox_snapshots
        .values()
        .filter(|snapshot| snapshot.recovery_started_at.is_some())
        .count();
    let watchers = shared.tmux_watchers.len();
    let channel_watcher = shared.tmux_watchers.contains_key(&channel_id);
    let channel_recovering = channel_snapshot.recovery_started_at.is_some();
    let current_path_text =
        compact_path(session_path.unwrap_or_else(|| "(no session)".to_string()));
    let session_id_text = session_id.unwrap_or_else(|| "(none)".to_string());
    let session_id_short = shorten_session_identifier(&session_id_text);
    let tmux_session_name =
        session_channel_name.map(|name| provider.build_tmux_session_name(&name));
    let queue_namespace = format!("{}/{}", provider.as_str(), shared.token_hash);
    let session_key = tmux_session_name.as_ref().map(|session_name| {
        super::super::adk_session::build_namespaced_session_key(
            &shared.token_hash,
            provider,
            session_name,
        )
    });
    let session_key_text = session_key.clone().unwrap_or_else(|| "(none)".to_string());
    let raw_provider_session_id =
        fetch_raw_provider_session_id(session_key.as_deref(), provider).await;
    let raw_provider_session_id_text =
        raw_provider_session_id.unwrap_or_else(|| "(none)".to_string());
    let raw_provider_session_id_short = shorten_session_identifier(&raw_provider_session_id_text);
    let tmux_alive = if let Some(ref session_name) = tmux_session_name {
        if tmux_session_has_live_pane(session_name) {
            "alive"
        } else if tmux_session_exists(session_name) {
            "dead-pane"
        } else {
            "missing"
        }
    } else {
        "unknown"
    };
    let channel_state = if channel_recovering {
        "recovering"
    } else if active_request {
        "working"
    } else if channel_watcher {
        "watching"
    } else {
        "idle"
    };
    let inflight_text = channel_inflight
        .map(|state| {
            format!(
                "yes (offset {}, runtime {})",
                state.last_offset,
                inflight_runtime_label(state)
            )
        })
        .unwrap_or_else(|| "no".to_string());

    format!(
        "\
**AgentDesk Health**
- provider: `{}`
- dcserver pid: `{}`
- release: current `{}`, previous `{}`
- runtime: sessions `{}`, active `{}`, queued `{}/{}`
- bridge: watchers `{}`, recovering `{}`, inflight saved `{}`

**This Channel**
- state: `{}`
- namespace: `{}`
- path: `{}`
- session_id: `{}`
- raw_provider_session_id: `{}`
- session_key: `{}`
- tmux: `{}`
- bridge: active `{}`, watcher `{}`, inflight `{}`
- queue: interventions `{}`, uploads `{}`
",
        provider.as_str(),
        std::process::id(),
        release_label(&current_release),
        release_label(&previous_release),
        session_count,
        active_request_count,
        queued_channel_count,
        queued_total,
        watchers,
        recovering_count,
        inflight_count,
        channel_state,
        queue_namespace,
        current_path_text,
        session_id_short,
        raw_provider_session_id_short,
        truncate_str(&session_key_text, 96),
        tmux_alive,
        if active_request { "yes" } else { "no" },
        if channel_watcher { "yes" } else { "no" },
        inflight_text,
        queued_count,
        pending_uploads
    )
}

pub(in crate::services::discord) async fn build_status_report(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> String {
    let channel_snapshot = mailbox_snapshot(shared, channel_id).await;
    let (session_path, session_id, pending_uploads, history_len, cleared, session_channel_name) = {
        let data = shared.core.lock().await;
        let session = data.sessions.get(&channel_id);
        (
            session.and_then(|s| s.current_path.clone()),
            session.and_then(|s| s.session_id.clone()),
            session.map(|s| s.pending_uploads.len()).unwrap_or(0),
            session.map(|s| s.history.len()).unwrap_or(0),
            session.map(|s| s.cleared).unwrap_or(false),
            session.and_then(|s| s.channel_name.clone()),
        )
    };
    let active_request = channel_snapshot.cancel_token.is_some();
    let active_owner = channel_snapshot.active_request_owner;
    let queued_count = pending_queue_len(&channel_snapshot.intervention_queue);

    let home_prefix = dirs::home_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let compact_path = |value: String| {
        if value.starts_with(&home_prefix) {
            value.replacen(&home_prefix, "~", 1)
        } else {
            value
        }
    };
    let session_id_text = session_id.unwrap_or_else(|| "(none)".to_string());
    let session_id_short = shorten_session_identifier(&session_id_text);
    let tmux_session_name =
        session_channel_name.map(|name| provider.build_tmux_session_name(&name));
    let queue_namespace = format!("{}/{}", provider.as_str(), shared.token_hash);
    let session_key = tmux_session_name.as_ref().map(|session_name| {
        super::super::adk_session::build_namespaced_session_key(
            &shared.token_hash,
            provider,
            session_name,
        )
    });
    let session_key_text = session_key.clone().unwrap_or_else(|| "(none)".to_string());
    let raw_provider_session_id =
        fetch_raw_provider_session_id(session_key.as_deref(), provider).await;
    let raw_provider_session_id_text =
        raw_provider_session_id.unwrap_or_else(|| "(none)".to_string());
    let raw_provider_session_id_short = shorten_session_identifier(&raw_provider_session_id_text);
    let tmux_alive = if let Some(ref session_name) = tmux_session_name {
        if tmux_session_has_live_pane(session_name) {
            "alive"
        } else if tmux_session_exists(session_name) {
            "dead-pane"
        } else {
            "missing"
        }
    } else {
        "unknown"
    };
    let channel_watcher = shared.tmux_watchers.contains_key(&channel_id);
    let channel_recovering = channel_snapshot.recovery_started_at.is_some();
    let channel_state = if channel_recovering {
        "recovering"
    } else if active_request {
        "working"
    } else if channel_watcher {
        "watching"
    } else {
        "idle"
    };
    let owner_text = active_owner
        .map(|id| format!("<@{}>", id.get()))
        .unwrap_or_else(|| "(none)".to_string());
    let path_text = compact_path(session_path.unwrap_or_else(|| "(no session)".to_string()));
    let inflight_runtime_text = load_inflight_states(provider)
        .iter()
        .find(|state| state.channel_id == channel_id.get())
        .map(inflight_runtime_label)
        .unwrap_or("none");

    format!(
        "\
**Channel Status**
- provider: `{}`
- namespace: `{}`
- state: `{}`
- path: `{}`
- session_id: `{}`
- raw_provider_session_id: `{}`
- session_key: `{}`
- tmux: `{}`
- runtime: `{}`
- owner: {}
- queue: interventions `{}`, uploads `{}`
- history: items `{}`, cleared `{}`
",
        provider.as_str(),
        queue_namespace,
        channel_state,
        path_text,
        session_id_short,
        raw_provider_session_id_short,
        truncate_str(&session_key_text, 96),
        tmux_alive,
        inflight_runtime_text,
        owner_text,
        queued_count,
        pending_uploads,
        history_len,
        if cleared { "yes" } else { "no" }
    )
}

pub(in crate::services::discord) async fn build_inflight_report(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> String {
    let mut inflight_states = load_inflight_states(provider);
    inflight_states.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    let recovering_count = shared
        .mailboxes
        .snapshot_all()
        .await
        .values()
        .filter(|snapshot| snapshot.recovery_started_at.is_some())
        .count();
    let channel_inflight = inflight_states
        .iter()
        .find(|state| state.channel_id == channel_id.get());

    let channel_status = channel_inflight.map(|_| "saved").unwrap_or("none");

    let current_section = if let Some(state) = channel_inflight {
        let session_id = state
            .session_id
            .clone()
            .unwrap_or_else(|| "(none)".to_string());
        let session_id_short = if session_id.len() > 24 {
            format!("{}...", &session_id[..24])
        } else {
            session_id
        };
        let tmux_name = state
            .tmux_session_name
            .clone()
            .unwrap_or_else(|| "(none)".to_string());
        format!(
            "\
**This Channel**
- started: `{}`
- updated: `{}`
- offset: `{}`
- runtime: `{}`
- session_id: `{}`
- tmux: `{}`
- placeholder_msg: `{}`
- user_text: `{}`
",
            state.started_at,
            state.updated_at,
            state.last_offset,
            inflight_runtime_label(state),
            session_id_short,
            tmux_name,
            state.current_msg_id,
            truncate_str(&state.user_text, 80)
        )
    } else {
        "\
**This Channel**
- status: `none`
"
        .to_string()
    };

    let saved_channels = if inflight_states.is_empty() {
        "- (none)".to_string()
    } else {
        inflight_states
            .iter()
            .take(6)
            .map(|state| {
                format!(
                    "- `{}` (`{}`) runtime `{}` offset `{}` updated `{}`",
                    state.channel_name.as_deref().unwrap_or("unknown"),
                    state.channel_id,
                    inflight_runtime_label(state),
                    state.last_offset,
                    state.updated_at
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "\
**Inflight**
- provider: `{}`
- saved turns: `{}`
- recovering channels: `{}`
- this channel: `{}`

{}
**Saved Channels**
{}
",
        provider.as_str(),
        inflight_states.len(),
        recovering_count,
        channel_status,
        current_section,
        saved_channels
    )
}

#[cfg(test)]
mod runtime_label_tests {
    use super::*;
    use crate::services::agent_protocol::RuntimeHandoffKind;

    #[test]
    fn inflight_runtime_label_reports_direct_tui_kind() {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            42,
            Some("adk-claude".to_string()),
            1,
            2,
            3,
            "prompt".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-claude-adk-claude".to_string()),
            Some("/tmp/claude-transcript.jsonl".to_string()),
            None,
            0,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);

        assert_eq!(inflight_runtime_label(&state), "Claude TUI");
    }
}

fn build_queue_report_sync(
    queues: &std::collections::HashMap<ChannelId, Vec<Intervention>>,
    provider: &ProviderKind,
    token_hash: &str,
    current_channel: ChannelId,
    show_all: bool,
) -> String {
    let now = Instant::now();
    let mut lines = Vec::new();

    // In-memory queues
    let channels: Vec<(ChannelId, &Vec<Intervention>)> = if show_all {
        queues
            .iter()
            .map(|(channel_id, queue)| (*channel_id, queue))
            .collect()
    } else {
        queues
            .get(&current_channel)
            .map(|queue| vec![(current_channel, queue)])
            .unwrap_or_default()
    };

    let total_in_memory: usize = if show_all {
        queues.values().map(|queue| queue.len()).sum()
    } else {
        channels.iter().map(|(_, queue)| queue.len()).sum()
    };

    lines.push(format!(
        "**📋 Pending Queue{}**",
        if show_all { " (all channels)" } else { "" }
    ));

    if channels.is_empty() || total_in_memory == 0 {
        lines.push("  In-memory: (empty)".to_string());
    } else {
        lines.push(format!("  In-memory: {} item(s)", total_in_memory));
        for (ch_id, queue) in &channels {
            if queue.is_empty() {
                continue;
            }
            lines.push(format!("  **#{}** — {} queued", ch_id, queue.len()));
            for (i, item) in queue.iter().enumerate().take(5) {
                let age = now.duration_since(item.created_at).as_secs();
                let preview = truncate_str(&item.text, 60);
                lines.push(format!(
                    "    {}. `<@{}>` {}s ago: {}",
                    i + 1,
                    item.author_id,
                    age,
                    preview
                ));
            }
            if queue.len() > 5 {
                lines.push(format!("    ... +{} more", queue.len() - 5));
            }
        }
    }

    // Disk-persisted queues — read from token_hash namespace (P1-3)
    if let Some(root) = runtime_store::discord_pending_queue_root() {
        let ns_dir = root.join(provider.as_str()).join(token_hash);
        lines.push(format!(
            "  Disk namespace: `{provider}/{token_hash}`",
            provider = provider.as_str()
        ));
        if ns_dir.is_dir() {
            let mut disk_count = 0usize;
            let target_file = if show_all {
                None
            } else {
                Some(ns_dir.join(format!("{}.json", current_channel)))
            };
            let paths: Vec<std::path::PathBuf> = if let Some(ref tf) = target_file {
                if tf.is_file() {
                    vec![tf.clone()]
                } else {
                    vec![]
                }
            } else if let Ok(entries) = std::fs::read_dir(&ns_dir) {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
                    .collect()
            } else {
                vec![]
            };
            for path in &paths {
                if let Ok(contents) = std::fs::read_to_string(path) {
                    if let Ok(items) = serde_json::from_str::<Vec<PendingQueueItem>>(&contents) {
                        if !items.is_empty() {
                            let ch_name = path
                                .file_stem()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_default();
                            // Use file mtime as approximate queue age
                            let age_str = std::fs::metadata(path)
                                .and_then(|m| m.modified())
                                .ok()
                                .and_then(|mt| std::time::SystemTime::now().duration_since(mt).ok())
                                .map(|d| format!(" (saved ~{}s ago)", d.as_secs()))
                                .unwrap_or_default();
                            let has_routing = items.iter().any(|i| i.channel_id.is_some());
                            let has_override =
                                items.iter().any(|i| i.override_channel_id.is_some());
                            let routing_tag = match (has_routing, has_override) {
                                (true, true) => " [routing+override]",
                                (true, false) => " [routing]",
                                _ => " [no routing snapshot]",
                            };
                            lines.push(format!(
                                "  **Disk** #{} — {} item(s){}{}",
                                ch_name,
                                items.len(),
                                age_str,
                                routing_tag
                            ));
                            for (i, item) in items.iter().enumerate().take(3) {
                                let preview = truncate_str(&item.text, 60);
                                lines.push(format!(
                                    "    {}. `<@{}>`: {}",
                                    i + 1,
                                    item.author_id,
                                    preview
                                ));
                            }
                            disk_count += items.len();
                        }
                    }
                }
            }
            if disk_count > 0 {
                lines.push(format!("  Disk total: {} item(s)", disk_count));
            } else {
                lines.push("  Disk: (empty)".to_string());
            }
        } else {
            lines.push("  Disk: (no directory)".to_string());
        }
    }

    lines.join("\n")
}

pub(in crate::services::discord) async fn build_queue_report(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    current_channel: ChannelId,
    show_all: bool,
) -> String {
    let queues = normalize_pending_queues(mailbox_queue_snapshots(shared).await);
    build_queue_report_sync(
        &queues,
        provider,
        &shared.token_hash,
        current_channel,
        show_all,
    )
}

fn format_gemini_session_report(
    working_dir: &str,
    sessions: &[gemini::GeminiProjectSession],
) -> String {
    if sessions.is_empty() {
        return format!(
            "Gemini sessions for `{}`: none",
            truncate_str(working_dir, 96)
        );
    }

    let mut lines = vec![format!(
        "Gemini sessions for `{}`:",
        truncate_str(working_dir, 96)
    )];
    for session in sessions {
        let current = if session.is_current_session {
            " (current)"
        } else {
            ""
        };
        lines.push(format!(
            "- `{}` {}{} — `{}` — `{}`",
            session.index, session.title, current, session.relative_time, session.session_id
        ));
    }
    lines.join("\n")
}

/// /metrics — Show turn metrics summary
#[poise::command(slash_command, rename = "metrics")]
pub(in crate::services::discord) async fn cmd_metrics(
    ctx: Context<'_>,
    #[description = "Date (YYYY-MM-DD), default today"] date: Option<String>,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /metrics");

    let data = match &date {
        Some(d) => metrics::load_date(d),
        None => metrics::load_today(),
    };
    let label_owned = date.as_deref().unwrap_or("today");
    let text = metrics::build_metrics_report(&data, label_owned);
    send_long_message_ctx(ctx, &text).await?;
    Ok(())
}

/// /health — Show runtime health summary
#[poise::command(slash_command, rename = "health")]
pub(in crate::services::discord) async fn cmd_health(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /health");

    let text =
        build_health_report(&ctx.data().shared, &ctx.data().provider, ctx.channel_id()).await;
    send_long_message_ctx(ctx, &text).await?;
    Ok(())
}

/// /sessions — Show Gemini project sessions for the current working directory
#[poise::command(slash_command, rename = "sessions")]
pub(in crate::services::discord) async fn cmd_sessions(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /sessions");

    if ctx.data().provider != ProviderKind::Gemini {
        ctx.say("`/sessions` is currently supported only when the active provider is Gemini.")
            .await?;
        return Ok(());
    }

    let Some(working_dir) = current_working_dir(&ctx.data().shared, ctx.channel_id()).await else {
        ctx.say("No active working directory for this channel. Start or restore a session first.")
            .await?;
        return Ok(());
    };

    ctx.defer().await?;

    let working_dir_for_list = working_dir.clone();
    let sessions = match run_blocking_gemini_command("Gemini session list", move || {
        gemini::list_project_sessions(&working_dir_for_list)
    })
    .await
    {
        Ok(sessions) => sessions,
        Err(error) => {
            ctx.say(format!("Gemini session list failed: `{}`", error))
                .await?;
            return Ok(());
        }
    };

    let text = format_gemini_session_report(&working_dir, &sessions);
    send_long_message_ctx(ctx, &text).await?;
    Ok(())
}

/// /status — Show concise per-channel runtime state
#[poise::command(slash_command, rename = "status")]
pub(in crate::services::discord) async fn cmd_status(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /status");

    let text =
        build_status_report(&ctx.data().shared, &ctx.data().provider, ctx.channel_id()).await;
    send_long_message_ctx(ctx, &text).await?;
    Ok(())
}

/// /deletesession [identifier] — Delete a Gemini project session by index or UUID
#[poise::command(slash_command, rename = "deletesession")]
pub(in crate::services::discord) async fn cmd_deletesession(
    ctx: Context<'_>,
    #[description = "Gemini session index or UUID from /sessions"] identifier: String,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }
    // Issue #1005: runtime-control tier — owner-only. Deleting a Gemini
    // session is destructive and may interrupt live work.
    if !super::enforce_slash_command_policy(&ctx, "/deletesession").await? {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] ◀ [{user_name}] /deletesession identifier={}",
        identifier
    );

    if ctx.data().provider != ProviderKind::Gemini {
        ctx.say("`/deletesession` is currently supported only when the active provider is Gemini.")
            .await?;
        return Ok(());
    }

    let Some(working_dir) = current_working_dir(&ctx.data().shared, ctx.channel_id()).await else {
        ctx.say("No active working directory for this channel. Start or restore a session first.")
            .await?;
        return Ok(());
    };

    ctx.defer().await?;

    let working_dir_for_list = working_dir.clone();
    let sessions = match run_blocking_gemini_command("Gemini session list", move || {
        gemini::list_project_sessions(&working_dir_for_list)
    })
    .await
    {
        Ok(sessions) => sessions,
        Err(error) => {
            ctx.say(format!("Gemini session list failed: `{}`", error))
                .await?;
            return Ok(());
        }
    };

    let trimmed_identifier = identifier.trim();
    let deleted_session_id = if trimmed_identifier.chars().all(|ch| ch.is_ascii_digit()) {
        trimmed_identifier
            .parse::<usize>()
            .ok()
            .and_then(|index| sessions.iter().find(|session| session.index == index))
            .map(|session| session.session_id.clone())
    } else {
        sessions
            .iter()
            .find(|session| session.session_id == trimmed_identifier)
            .map(|session| session.session_id.clone())
    };

    let working_dir_for_delete = working_dir.clone();
    let identifier_for_delete = trimmed_identifier.to_string();
    let result = match run_blocking_gemini_command("Gemini session delete", move || {
        gemini::delete_project_session(&working_dir_for_delete, &identifier_for_delete)
    })
    .await
    {
        Ok(result) => result,
        Err(error) => {
            ctx.say(format!("Gemini session delete failed: `{}`", error))
                .await?;
            return Ok(());
        }
    };

    if let Some(session_id) = deleted_session_id.as_deref() {
        let _ = super::super::internal_api::clear_stale_session_id(session_id).await;
    }

    ctx.say(result).await?;
    Ok(())
}

/// /inflight — Show saved inflight turn state
#[poise::command(slash_command, rename = "inflight")]
pub(in crate::services::discord) async fn cmd_inflight(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /inflight");

    let text =
        build_inflight_report(&ctx.data().shared, &ctx.data().provider, ctx.channel_id()).await;
    send_long_message_ctx(ctx, &text).await?;
    Ok(())
}

/// /queue — Show pending intervention queue state
#[poise::command(slash_command, rename = "queue")]
pub(in crate::services::discord) async fn cmd_queue(
    ctx: Context<'_>,
    #[description = "Show all channels (omit for current channel only)"] all: Option<bool>,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /queue");

    let show_all = all.unwrap_or(false);
    let text = build_queue_report(
        &ctx.data().shared,
        &ctx.data().provider,
        ctx.channel_id(),
        show_all,
    )
    .await;
    send_long_message_ctx(ctx, &text).await?;
    Ok(())
}

/// /debug — Toggle debug logging at runtime
#[poise::command(slash_command, rename = "debug")]
pub(in crate::services::discord) async fn cmd_debug(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }
    // Issue #1005: runtime-control tier — owner-only. Toggling global debug
    // logging changes runtime behavior and is owner-managed.
    if !super::enforce_slash_command_policy(&ctx, "/debug").await? {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /debug");

    let new_state = claude::toggle_debug();
    let status = if new_state { "ON" } else { "OFF" };
    ctx.say(format!("Debug logging: **{}**", status)).await?;
    tracing::info!("  [{ts}] ▶ Debug logging toggled to {status}");
    Ok(())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use crate::services::discord::InterventionMode;
    use poise::serenity_prelude::{MessageId, UserId};

    fn intervention(
        snapshot_now: Instant,
        age_secs: u64,
        message_id: u64,
        text: &str,
    ) -> Intervention {
        Intervention {
            author_id: UserId::new(1),
            message_id: MessageId::new(message_id),
            source_message_ids: vec![MessageId::new(message_id)],
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at: snapshot_now
                .checked_sub(Duration::from_secs(age_secs))
                .expect("snapshot instant should be offset far enough for the requested age"),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
        }
    }

    #[test]
    fn pending_queue_len_prunes_expired_items() {
        let snapshot_now = Instant::now() + Duration::from_secs(10 * 60 + 5);
        let queue = vec![
            intervention(snapshot_now, 10 * 60 + 5, 1, "expired"),
            intervention(snapshot_now, 30, 2, "fresh"),
        ];

        assert_eq!(pending_queue_len_at(&queue, snapshot_now), 1);
    }

    #[test]
    fn normalize_pending_queues_drops_channels_without_live_items() {
        let snapshot_now = Instant::now() + Duration::from_secs(10 * 60 + 1);
        let expired_channel = ChannelId::new(1);
        let fresh_channel = ChannelId::new(2);
        let queues = HashMap::from([
            (
                expired_channel,
                vec![intervention(snapshot_now, 10 * 60 + 1, 1, "expired-only")],
            ),
            (
                fresh_channel,
                vec![intervention(snapshot_now, 15, 2, "fresh")],
            ),
        ]);

        let normalized = normalize_pending_queues_at(queues, snapshot_now);

        assert!(!normalized.contains_key(&expired_channel));
        assert_eq!(
            normalized.get(&fresh_channel).map(std::vec::Vec::len),
            Some(1)
        );
    }

    #[test]
    fn format_gemini_session_report_shows_full_session_id_for_copy_paste() {
        let sessions = vec![gemini::GeminiProjectSession {
            index: 1,
            title: "Reply with exactly OK.".to_string(),
            relative_time: "Just now".to_string(),
            is_current_session: false,
            session_id: "242215ad-7e7b-4008-a0a5-7ccba0bcd4a5".to_string(),
        }];

        let report = format_gemini_session_report("/tmp/project", &sessions);

        assert!(report.contains("242215ad-7e7b-4008-a0a5-7ccba0bcd4a5"));
        assert!(!report.contains("242215ad-7e7b-4008-a0a5..."));
    }
}
