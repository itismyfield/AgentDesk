use std::fs;
use std::sync::Arc;
use std::time::Instant;

use poise::serenity_prelude as serenity;
use serenity::ChannelId;

use super::super::formatting::{send_long_message_ctx, truncate_str};
use super::super::inflight::load_inflight_states;
use super::super::metrics;
use super::super::runtime_store;
use super::super::{Context, CoreState, Error, PendingQueueItem, SharedData, check_auth};
use crate::services::claude;
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

pub(in crate::services::discord) async fn build_health_report(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> String {
    let (
        session_path,
        session_id,
        session_channel_name,
        pending_uploads,
        active_request,
        queued_count,
        session_count,
        active_request_count,
        queued_channel_count,
        queued_total,
    ) = {
        let data = shared.core.lock().await;
        let session = data.sessions.get(&channel_id);
        let queued_count = data
            .intervention_queue
            .get(&channel_id)
            .map(|q| q.len())
            .unwrap_or(0);
        let queued_channel_count = data
            .intervention_queue
            .values()
            .filter(|q| !q.is_empty())
            .count();
        let queued_total: usize = data.intervention_queue.values().map(|q| q.len()).sum();
        (
            session.and_then(|s| s.current_path.clone()),
            session.and_then(|s| s.session_id.clone()),
            session.and_then(|s| s.channel_name.clone()),
            session.map(|s| s.pending_uploads.len()).unwrap_or(0),
            data.cancel_tokens.contains_key(&channel_id),
            queued_count,
            data.sessions.len(),
            data.cancel_tokens.len(),
            queued_channel_count,
            queued_total,
        )
    };

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
    let recovering_count = shared.recovering_channels.len();
    let watchers = shared.tmux_watchers.len();
    let channel_watcher = shared.tmux_watchers.contains_key(&channel_id);
    let channel_recovering = shared.recovering_channels.contains_key(&channel_id);
    let current_path_text =
        compact_path(session_path.unwrap_or_else(|| "(no session)".to_string()));
    let session_id_text = session_id.unwrap_or_else(|| "(none)".to_string());
    let session_id_short = if session_id_text.len() > 24 {
        format!("{}...", &session_id_text[..24])
    } else {
        session_id_text.clone()
    };
    let tmux_session_name =
        session_channel_name.map(|name| provider.build_tmux_session_name(&name));
    let queue_namespace = format!("{}/{}", provider.as_str(), shared.token_hash);
    let session_key_text = tmux_session_name
        .as_ref()
        .map(|session_name| {
            super::super::adk_session::build_namespaced_session_key(
                &shared.token_hash,
                provider,
                session_name,
            )
        })
        .unwrap_or_else(|| "(none)".to_string());
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
        .map(|state| format!("yes (offset {})", state.last_offset))
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
    let (
        session_path,
        session_id,
        remote_name,
        pending_uploads,
        history_len,
        cleared,
        active_request,
        active_owner,
        queued_count,
        session_channel_name,
    ) = {
        let data = shared.core.lock().await;
        let session = data.sessions.get(&channel_id);
        (
            session.and_then(|s| s.current_path.clone()),
            session.and_then(|s| s.session_id.clone()),
            session.and_then(|s| s.remote_profile_name.clone()),
            session.map(|s| s.pending_uploads.len()).unwrap_or(0),
            session.map(|s| s.history.len()).unwrap_or(0),
            session.map(|s| s.cleared).unwrap_or(false),
            data.cancel_tokens.contains_key(&channel_id),
            data.active_request_owner.get(&channel_id).copied(),
            data.intervention_queue
                .get(&channel_id)
                .map(|q| q.len())
                .unwrap_or(0),
            session.and_then(|s| s.channel_name.clone()),
        )
    };

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
    let session_id_short = if session_id_text.len() > 24 {
        format!("{}...", &session_id_text[..24])
    } else {
        session_id_text
    };
    let tmux_session_name =
        session_channel_name.map(|name| provider.build_tmux_session_name(&name));
    let queue_namespace = format!("{}/{}", provider.as_str(), shared.token_hash);
    let session_key_text = tmux_session_name
        .as_ref()
        .map(|session_name| {
            super::super::adk_session::build_namespaced_session_key(
                &shared.token_hash,
                provider,
                session_name,
            )
        })
        .unwrap_or_else(|| "(none)".to_string());
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
    let channel_recovering = shared.recovering_channels.contains_key(&channel_id);
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
    let remote_text = remote_name.unwrap_or_else(|| "local".to_string());

    format!(
        "\
**Channel Status**
- provider: `{}`
- namespace: `{}`
- state: `{}`
- path: `{}`
- session_id: `{}`
- session_key: `{}`
- remote: `{}`
- tmux: `{}`
- owner: {}
- queue: interventions `{}`, uploads `{}`
- history: items `{}`, cleared `{}`
",
        provider.as_str(),
        queue_namespace,
        channel_state,
        path_text,
        session_id_short,
        truncate_str(&session_key_text, 96),
        remote_text,
        tmux_alive,
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

    let recovering_count = shared.recovering_channels.len();
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
- session_id: `{}`
- tmux: `{}`
- placeholder_msg: `{}`
- user_text: `{}`
",
            state.started_at,
            state.updated_at,
            state.last_offset,
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
                    "- `{}` (`{}`) offset `{}` updated `{}`",
                    state.channel_name.as_deref().unwrap_or("unknown"),
                    state.channel_id,
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

fn build_queue_report_sync(
    data: &CoreState,
    provider: &ProviderKind,
    token_hash: &str,
    current_channel: ChannelId,
    show_all: bool,
) -> String {
    let now = Instant::now();
    let mut lines = Vec::new();

    // In-memory queues
    let channels: Vec<_> = if show_all {
        data.intervention_queue.iter().collect()
    } else {
        data.intervention_queue
            .get(&current_channel)
            .map(|q| vec![(&current_channel, q)])
            .unwrap_or_default()
    };

    let total_in_memory: usize = if show_all {
        data.intervention_queue.values().map(|q| q.len()).sum()
    } else {
        channels.iter().map(|(_, q)| q.len()).sum()
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
        let legacy_dir = root.join(provider.as_str());
        if legacy_dir.is_dir() {
            let legacy_files: Vec<_> = std::fs::read_dir(&legacy_dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|entry| {
                    let path = entry.path();
                    path.is_file() && path.extension().map(|ext| ext == "json").unwrap_or(false)
                })
                .collect();
            if !legacy_files.is_empty() {
                lines.push(format!(
                    "  ⚠ **Legacy queue files** ({} file(s)) found at `{}/` — predates bot-identity namespacing, will NOT be restored.",
                    legacy_files.len(),
                    provider.as_str()
                ));
            }
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
    let data = shared.core.lock().await;
    build_queue_report_sync(
        &data,
        provider,
        &shared.token_hash,
        current_channel,
        show_all,
    )
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
    println!("  [{ts}] ◀ [{user_name}] /metrics");

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
    println!("  [{ts}] ◀ [{user_name}] /health");

    let text =
        build_health_report(&ctx.data().shared, &ctx.data().provider, ctx.channel_id()).await;
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
    println!("  [{ts}] ◀ [{user_name}] /status");

    let text =
        build_status_report(&ctx.data().shared, &ctx.data().provider, ctx.channel_id()).await;
    send_long_message_ctx(ctx, &text).await?;
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
    println!("  [{ts}] ◀ [{user_name}] /inflight");

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
    println!("  [{ts}] ◀ [{user_name}] /queue");

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

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{user_name}] /debug");

    let new_state = claude::toggle_debug();
    let status = if new_state { "ON" } else { "OFF" };
    ctx.say(format!("Debug logging: **{}**", status)).await?;
    println!("  [{ts}] ▶ Debug logging toggled to {status}");
    Ok(())
}
