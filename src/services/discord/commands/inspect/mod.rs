use anyhow::Result;
use sqlx::PgPool;

mod formatting;
mod model;
mod query;
mod render_context;
mod render_last;
mod render_prompt;
mod render_recovery;
mod render_session;
#[cfg(test)]
mod tests;

use self::formatting::{fenced_report, no_data_report};
use self::model::InspectContextConfig;
use self::query::{
    load_latest_compaction_event, load_latest_prompt_manifest, load_latest_session_event,
    load_latest_turn, load_lifecycle_events,
};
use self::render_context::render_context_report;
use self::render_last::render_last_report;
use self::render_prompt::render_prompt_manifest_report;
use self::render_recovery::render_recovery_report;
use self::render_session::render_session_report;
use super::super::formatting::send_long_message_ctx;
use super::super::{Context, Error};
use crate::db::prompt_manifests::fetch_prompt_manifest;
use crate::services::observability::recovery_audit::fetch_recovery_audit;
use crate::services::provider::ProviderKind;

#[derive(Debug, Clone, Copy, poise::ChoiceParameter)]
enum InspectView {
    #[name = "last"]
    Last,
    #[name = "session"]
    Session,
    #[name = "prompt"]
    Prompt,
    #[name = "context"]
    Context,
    #[name = "recovery"]
    Recovery,
}

impl InspectView {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Last => "last",
            Self::Session => "session",
            Self::Prompt => "prompt",
            Self::Context => "context",
            Self::Recovery => "recovery",
        }
    }
}

/// /adk — AgentDesk diagnostics namespace.
#[poise::command(slash_command, rename = "adk", subcommands("cmd_adk_inspect"))]
pub(in crate::services::discord) async fn cmd_adk(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// /adk inspect <view> — Inspect recent turn diagnostics.
#[poise::command(slash_command, rename = "inspect")]
async fn cmd_adk_inspect(
    ctx: Context<'_>,
    #[description = "View: last / session / prompt / context / recovery"] view: InspectView,
) -> Result<(), Error> {
    let user_name = &ctx.author().name;
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] ◀ [{user_name}] /adk inspect {}", view.as_str());

    ctx.defer().await?;

    let channel_id = ctx.channel_id().get().to_string();
    let Some(pool) = ctx.data().shared.pg_pool.as_ref() else {
        send_long_message_ctx(ctx, &no_data_report()).await?;
        return Ok(());
    };

    let report = match build_inspect_report(&ctx, pool, &channel_id, view).await {
        Ok(report) => report,
        Err(error) => fenced_report(format!("Inspect\nerror: {error}")),
    };
    send_long_message_ctx(ctx, &report).await?;
    Ok(())
}

async fn build_inspect_report(
    ctx: &Context<'_>,
    pool: &PgPool,
    channel_id: &str,
    view: InspectView,
) -> Result<String> {
    match view {
        InspectView::Last => {
            let Some(turn) = load_latest_turn(pool, channel_id).await? else {
                return Ok(no_data_report());
            };
            let session_event =
                load_latest_session_event(pool, channel_id, Some(turn.turn_id.as_str())).await?;
            let automation_events =
                load_lifecycle_events(pool, channel_id, &turn.turn_id, 5).await?;
            let prompt = fetch_prompt_manifest(Some(pool), &turn.turn_id).await?;
            let context = load_context_config(ctx, turn.provider.as_deref()).await;
            Ok(render_last_report(
                &turn,
                session_event.as_ref(),
                prompt.as_ref(),
                &automation_events,
                &context,
            ))
        }
        InspectView::Session => {
            let Some(turn) = load_latest_turn(pool, channel_id).await? else {
                return Ok(no_data_report());
            };
            let session_event = load_latest_session_event(pool, channel_id, None).await?;
            Ok(render_session_report(&turn, session_event.as_ref()))
        }
        InspectView::Prompt => {
            let Some(manifest) = load_latest_prompt_manifest(pool, channel_id).await? else {
                return Ok(no_data_report());
            };
            Ok(render_prompt_manifest_report(&manifest))
        }
        InspectView::Context => {
            let Some(turn) = load_latest_turn(pool, channel_id).await? else {
                return Ok(no_data_report());
            };
            let manifest = fetch_prompt_manifest(Some(pool), &turn.turn_id).await?;
            let compaction = load_latest_compaction_event(pool, channel_id).await?;
            let context = load_context_config(ctx, turn.provider.as_deref()).await;
            Ok(render_context_report(
                &turn,
                manifest.as_ref(),
                compaction.as_ref(),
                &context,
            ))
        }
        InspectView::Recovery => {
            let records = fetch_recovery_audit(pool, channel_id, 1).await?;
            let Some(record) = records.first() else {
                return Ok(no_data_report());
            };
            Ok(render_recovery_report(record))
        }
    }
}

async fn load_context_config(
    ctx: &Context<'_>,
    turn_provider: Option<&str>,
) -> InspectContextConfig {
    let provider = turn_provider
        .and_then(ProviderKind::from_str)
        .unwrap_or_else(|| ctx.data().provider.clone());
    let thresholds =
        super::super::adk_session::fetch_context_thresholds(ctx.data().shared.api_port).await;
    let model =
        super::resolve_model_for_turn(&ctx.data().shared, ctx.channel_id(), &provider).await;
    let context_window_tokens = provider.resolve_context_window(model.as_deref());
    let compact_percent = thresholds.compact_pct_for(&provider);
    InspectContextConfig {
        provider,
        model,
        context_window_tokens,
        compact_percent,
    }
}
