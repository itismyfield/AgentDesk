use std::sync::Arc;

use super::super::formatting::{send_long_message_ctx, truncate_str};
use super::super::metrics;
use super::super::{Context, Error, SharedData, check_auth};
use super::config::current_working_dir;
use crate::services::claude;
use crate::services::gemini;
use crate::services::provider::ProviderKind;

mod reports;

pub(in crate::services::discord) use reports::{
    build_health_report, build_inflight_report, build_queue_report, build_status_report,
};

async fn run_blocking_gemini_command<T, F>(label: &'static str, task: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|error| format!("{label} task failed: {error}"))?
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

/// /metrics — Show local turn metrics summary
#[poise::command(slash_command, rename = "metrics")]
pub(in crate::services::discord) async fn cmd_metrics(
    ctx: Context<'_>,
    #[description = "Date (YYYY-MM-DD), default today, for local turn metrics"] date: Option<
        String,
    >,
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

/// /adk-phase — Show phase-gate violations + run pointers (issue #2657).
///
/// Read-only snapshot via the in-process Postgres pool. No HTTP hop, so the
/// command remains responsive even when the auth-token guarded `/api/queue/*`
/// path is restricted to localhost. Defaults to a one-screen summary; the
/// optional `details` boolean expands each violation with run/dispatch ids.
#[poise::command(slash_command, rename = "adk-phase")]
pub(in crate::services::discord) async fn cmd_adk_phase(
    ctx: Context<'_>,
    #[description = "Show full per-entry details (default: compact)"] details: Option<bool>,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /adk-phase");

    let detailed = details.unwrap_or(false);
    let text = build_adk_phase_report(&ctx.data().shared, detailed).await;
    send_long_message_ctx(ctx, &text).await?;
    Ok(())
}

async fn build_adk_phase_report(shared: &Arc<SharedData>, detailed: bool) -> String {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return "phase-gate snapshot unavailable: postgres pool not configured".to_string();
    };
    // Direct scanner call — bypasses the AppState route wrapper because
    // slash commands run inside the Discord runtime (no axum extractor).
    let snapshot =
        match crate::services::auto_queue::route::phase_gate_violations::scan_violations_pg(pool)
            .await
        {
            Ok(s) => s,
            Err(e) => return format!("phase-gate scan failed: {e}"),
        };

    if snapshot.violations.is_empty() {
        return format!(
            "phase-gate: clean (runs scanned: {}, complete: {})",
            snapshot.runs_scanned, snapshot.complete
        );
    }

    let mut lines = vec![format!(
        "phase-gate violations: {} (runs scanned: {})",
        snapshot.violations.len(),
        snapshot.runs_scanned
    )];
    for v in &snapshot.violations {
        if detailed {
            lines.push(format!(
                "- run={} entry={} card={} phase={}>current={} dispatch={}",
                v.run_id,
                v.entry_id,
                v.kanban_card_id.as_deref().unwrap_or("-"),
                v.entry_batch_phase,
                v.current_batch_phase,
                v.dispatch_id.as_deref().unwrap_or("-"),
            ));
        } else {
            lines.push(format!("- {}", v.summary));
        }
    }
    lines.join("\n")
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
