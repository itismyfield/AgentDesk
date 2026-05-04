use anyhow::{Context as AnyhowContext, Result};
use chrono::{DateTime, FixedOffset, Utc};
use serde_json::Value;
use sqlx::{PgPool, Row};

use super::super::formatting::{escape_for_code_fence, send_long_message_ctx};
use super::super::{Context, Error};
use crate::db::prompt_manifests::{
    PromptContentVisibility, PromptManifest, PromptManifestLayer, fetch_prompt_manifest,
};
use crate::db::turns::TurnTokenUsage;
use crate::services::observability::recovery_audit::{RecoveryAuditRecord, fetch_recovery_audit};
use crate::services::provider::ProviderKind;

const NO_RECENT_TURN_DATA: &str = "최근 턴 데이터 없음";
const REPORT_LINE_MAX: usize = 100;
const ID_MAX_CHARS: usize = 28;
const SOURCE_MAX_CHARS: usize = 30;
const PROMPT_LAYER_CONTENT_MAX_CHARS: usize = 360;
const PROMPT_LAYER_CONTENT_MAX_LAYERS: usize = 6;
const RECOVERY_PREVIEW_MAX_LINES: usize = 8;
const RECOVERY_PREVIEW_LINE_MAX_CHARS: usize = 92;

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

#[derive(Debug, Clone)]
struct InspectContextConfig {
    provider: ProviderKind,
    model: Option<String>,
    context_window_tokens: u64,
    compact_percent: u64,
}

#[derive(Debug, Clone)]
struct LatestTurn {
    turn_id: String,
    channel_id: String,
    provider: Option<String>,
    session_key: Option<String>,
    session_id: Option<String>,
    dispatch_id: Option<String>,
    finished_at: DateTime<Utc>,
    duration_ms: Option<i64>,
    input_tokens: u64,
    cache_create_tokens: u64,
    cache_read_tokens: u64,
}

impl LatestTurn {
    fn token_usage(&self) -> TurnTokenUsage {
        TurnTokenUsage {
            input_tokens: self.input_tokens,
            cache_create_tokens: self.cache_create_tokens,
            cache_read_tokens: self.cache_read_tokens,
            output_tokens: 0,
        }
    }

    fn context_occupancy_input_tokens(&self) -> u64 {
        self.token_usage().context_occupancy_input_tokens()
    }
}

#[derive(Debug, Clone)]
struct LifecycleEventRow {
    kind: String,
    severity: String,
    summary: String,
    details_json: Value,
    created_at: DateTime<Utc>,
}

async fn load_latest_turn(pool: &PgPool, channel_id: &str) -> Result<Option<LatestTurn>> {
    let row = sqlx::query(
        "SELECT turn_id,
                channel_id,
                provider,
                session_key,
                session_id,
                dispatch_id,
                finished_at,
                duration_ms::BIGINT AS duration_ms,
                input_tokens::BIGINT AS input_tokens,
                cache_create_tokens::BIGINT AS cache_create_tokens,
                cache_read_tokens::BIGINT AS cache_read_tokens
         FROM turns
         WHERE channel_id = $1
         ORDER BY finished_at DESC, started_at DESC, created_at DESC
         LIMIT 1",
    )
    .bind(channel_id)
    .fetch_optional(pool)
    .await
    .context("load latest turn")?;

    row.map(|row| {
        Ok(LatestTurn {
            turn_id: row.try_get("turn_id")?,
            channel_id: row.try_get("channel_id")?,
            provider: row.try_get("provider")?,
            session_key: row.try_get("session_key")?,
            session_id: row.try_get("session_id")?,
            dispatch_id: row.try_get("dispatch_id")?,
            finished_at: row.try_get("finished_at")?,
            duration_ms: row.try_get("duration_ms")?,
            input_tokens: non_negative_u64(row.try_get::<i64, _>("input_tokens")?),
            cache_create_tokens: non_negative_u64(row.try_get::<i64, _>("cache_create_tokens")?),
            cache_read_tokens: non_negative_u64(row.try_get::<i64, _>("cache_read_tokens")?),
        })
    })
    .transpose()
}

async fn load_latest_prompt_manifest(
    pool: &PgPool,
    channel_id: &str,
) -> Result<Option<PromptManifest>> {
    let turn_id = sqlx::query_scalar::<_, String>(
        "SELECT turn_id
         FROM prompt_manifests
         WHERE channel_id = $1
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
    )
    .bind(channel_id)
    .fetch_optional(pool)
    .await
    .context("load latest prompt manifest turn_id")?;

    let Some(turn_id) = turn_id else {
        return Ok(None);
    };
    fetch_prompt_manifest(Some(pool), &turn_id).await
}

async fn load_latest_session_event(
    pool: &PgPool,
    channel_id: &str,
    turn_id: Option<&str>,
) -> Result<Option<LifecycleEventRow>> {
    let row = sqlx::query(
        "SELECT kind, severity, summary, details_json, created_at
         FROM turn_lifecycle_events
         WHERE channel_id = $1
           AND ($2::TEXT IS NULL OR turn_id = $2)
           AND kind IN (
               'session_fresh',
               'session_resumed',
               'session_resume_failed_with_recovery'
           )
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
    )
    .bind(channel_id)
    .bind(turn_id)
    .fetch_optional(pool)
    .await
    .context("load latest session lifecycle event")?;

    row.map(decode_lifecycle_event).transpose()
}

async fn load_latest_compaction_event(
    pool: &PgPool,
    channel_id: &str,
) -> Result<Option<LifecycleEventRow>> {
    let row = sqlx::query(
        "SELECT kind, severity, summary, details_json, created_at
         FROM turn_lifecycle_events
         WHERE channel_id = $1
           AND kind = 'context_compacted'
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
    )
    .bind(channel_id)
    .fetch_optional(pool)
    .await
    .context("load latest context compaction event")?;

    row.map(decode_lifecycle_event).transpose()
}

async fn load_lifecycle_events(
    pool: &PgPool,
    channel_id: &str,
    turn_id: &str,
    limit: i64,
) -> Result<Vec<LifecycleEventRow>> {
    let rows = sqlx::query(
        "SELECT kind, severity, summary, details_json, created_at
         FROM turn_lifecycle_events
         WHERE channel_id = $1
           AND turn_id = $2
         ORDER BY created_at DESC, id DESC
         LIMIT $3",
    )
    .bind(channel_id)
    .bind(turn_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("load lifecycle events")?;

    rows.into_iter().map(decode_lifecycle_event).collect()
}

fn decode_lifecycle_event(row: sqlx::postgres::PgRow) -> Result<LifecycleEventRow> {
    Ok(LifecycleEventRow {
        kind: row.try_get("kind")?,
        severity: row.try_get("severity")?,
        summary: row.try_get("summary")?,
        details_json: row.try_get("details_json")?,
        created_at: row.try_get("created_at")?,
    })
}

fn render_last_report(
    turn: &LatestTurn,
    session_event: Option<&LifecycleEventRow>,
    manifest: Option<&PromptManifest>,
    automation_events: &[LifecycleEventRow],
    context: &InspectContextConfig,
) -> String {
    let mut out = String::new();
    push_line(&mut out, "Last Turn");
    push_kv(&mut out, "turn_id", &turn.turn_id);
    push_kv(&mut out, "channel", &turn.channel_id);
    push_kv(&mut out, "provider", opt_or_none(turn.provider.as_deref()));
    push_kv(&mut out, "finished", &format_kst(turn.finished_at));
    push_kv(&mut out, "duration", &format_duration(turn.duration_ms));
    push_kv(
        &mut out,
        "session",
        session_event
            .map(session_status_label)
            .unwrap_or("(lifecycle 없음)"),
    );
    push_kv(&mut out, "context", &format_context_usage(turn, context));
    push_kv(&mut out, "prompt", &format_prompt_summary(manifest));
    push_kv(
        &mut out,
        "dispatch",
        opt_or_none(turn.dispatch_id.as_deref()),
    );
    push_line(&mut out, "");
    push_line(&mut out, "last automation:");
    if automation_events.is_empty() {
        push_line(&mut out, "- (없음)");
    } else {
        for event in automation_events.iter().take(5) {
            push_line(
                &mut out,
                &format!(
                    "- {} [{}] {}",
                    event.kind,
                    event.severity,
                    truncate_chars(&event.summary, 62)
                ),
            );
        }
    }
    fenced_report(out)
}

fn render_session_report(turn: &LatestTurn, event: Option<&LifecycleEventRow>) -> String {
    let mut out = String::new();
    push_line(&mut out, "Session");
    push_kv(
        &mut out,
        "status",
        event
            .map(session_status_label)
            .unwrap_or("(lifecycle 없음)"),
    );
    push_kv(&mut out, "provider", opt_or_none(turn.provider.as_deref()));
    let provider_session = session_id_from_event(event)
        .map(str::to_string)
        .unwrap_or_else(|| opt_or_none(turn.session_id.as_deref()));
    push_kv(&mut out, "provider session", provider_session);
    let adk_session = adk_session_from_event(event)
        .map(str::to_string)
        .unwrap_or_else(|| opt_or_none(turn.session_key.as_deref()));
    push_kv(&mut out, "adk session", adk_session);
    push_kv(&mut out, "backend", "tmux");
    push_kv(&mut out, "last clear reason", "(없음)");
    push_kv(&mut out, "last tmux action", &tmux_action_label(event));
    if let Some(event) = event {
        push_kv(&mut out, "event at", &format_kst(event.created_at));
        push_kv(&mut out, "event summary", &event.summary);
    }
    fenced_report(out)
}

fn render_prompt_manifest_report(manifest: &PromptManifest) -> String {
    let mut out = String::new();
    push_line(
        &mut out,
        &format!(
            "Prompt Manifest {}",
            manifest
                .id
                .map(|id| format!("pm_{id}"))
                .unwrap_or_else(|| "(unsaved)".to_string())
        ),
    );
    push_kv(&mut out, "turn_id", &manifest.turn_id);
    push_kv(
        &mut out,
        "profile",
        opt_or_none(manifest.profile.as_deref()),
    );
    push_kv(
        &mut out,
        "total input estimate",
        &format!("{} tokens", format_tokens(manifest.total_input_tokens_est)),
    );
    push_kv(
        &mut out,
        "layers",
        &format!(
            "{}/{} enabled",
            manifest.layers.iter().filter(|layer| layer.enabled).count(),
            manifest.layers.len()
        ),
    );
    push_line(&mut out, "");
    push_line(&mut out, "Layers");
    for layer in &manifest.layers {
        push_line(&mut out, &format_prompt_layer_summary(layer));
    }

    push_line(&mut out, "");
    push_line(
        &mut out,
        "Layer content (ADK full source, user redacted preview)",
    );
    for layer in manifest
        .layers
        .iter()
        .filter(|layer| layer.enabled)
        .take(PROMPT_LAYER_CONTENT_MAX_LAYERS)
    {
        push_line(
            &mut out,
            &format!(
                "{} [{}]",
                truncate_chars(&layer.layer_name, 38),
                visibility_label(layer.content_visibility)
            ),
        );
        for line in layer_display_body(layer)
            .lines()
            .flat_map(|line| wrap_line(line, REPORT_LINE_MAX - 2))
            .take(6)
        {
            push_line(&mut out, &format!("  {line}"));
        }
    }
    fenced_report(out)
}

fn render_context_report(
    turn: &LatestTurn,
    manifest: Option<&PromptManifest>,
    compaction: Option<&LifecycleEventRow>,
    context: &InspectContextConfig,
) -> String {
    let mut out = String::new();
    push_line(&mut out, "Context Window");
    push_kv(&mut out, "turn_id", &turn.turn_id);
    push_kv(&mut out, "usage", &format_context_usage(turn, context));
    push_kv(
        &mut out,
        "auto-compact threshold",
        &format!("{}%", context.compact_percent),
    );
    push_kv(&mut out, "provider", context.provider.as_str());
    push_kv(&mut out, "model", opt_or_none(context.model.as_deref()));
    match manifest {
        Some(manifest) => {
            push_kv(
                &mut out,
                "prompt estimate",
                &format!("{} tokens", format_tokens(manifest.total_input_tokens_est)),
            );
        }
        None => {
            push_kv(
                &mut out,
                "prompt estimate",
                "(manifest pending for this turn)",
            );
        }
    }
    push_kv(&mut out, "last compact", &format_compaction(compaction));
    push_line(&mut out, "");
    match manifest {
        Some(manifest) => {
            let mut layers = manifest.layers.clone();
            layers.sort_by(|a, b| b.tokens_est.cmp(&a.tokens_est));
            push_line(&mut out, "largest layers:");
            for layer in layers.iter().take(6) {
                push_line(
                    &mut out,
                    &format!(
                        "- {}: {}",
                        truncate_chars(&layer.layer_name, 54),
                        format_tokens(layer.tokens_est)
                    ),
                );
            }
        }
        None => {
            push_line(&mut out, "largest layers:");
            push_line(&mut out, "- (manifest pending for this turn)");
        }
    }
    fenced_report(out)
}

fn render_recovery_report(record: &RecoveryAuditRecord) -> String {
    let mut out = String::new();
    push_line(&mut out, "Recovery Context");
    push_kv(&mut out, "source", &human_recovery_source(&record.source));
    push_kv(&mut out, "created", &format_kst(record.created_at));
    push_kv(&mut out, "messages", &record.message_count.to_string());
    push_kv(
        &mut out,
        "max chars/message",
        &record.max_chars_per_message.to_string(),
    );
    push_kv(
        &mut out,
        "authors",
        &truncate_chars(&record.authors.join(", "), 76),
    );
    push_kv(
        &mut out,
        "consumed_by_turn",
        opt_or_none(record.consumed_by_turn_id.as_deref()),
    );
    push_kv(
        &mut out,
        "sha256",
        &truncate_chars(&record.content_sha256, 16),
    );
    push_line(&mut out, "");
    push_line(&mut out, "Preview (redacted):");
    let mut wrote_preview = false;
    for (idx, line) in record
        .redacted_preview
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(RECOVERY_PREVIEW_MAX_LINES)
        .enumerate()
    {
        wrote_preview = true;
        push_line(
            &mut out,
            &format!(
                "{}. {}",
                idx + 1,
                truncate_chars(line.trim(), RECOVERY_PREVIEW_LINE_MAX_CHARS)
            ),
        );
    }
    if !wrote_preview {
        push_line(&mut out, "(redacted preview 없음)");
    }
    fenced_report(out)
}

fn format_prompt_layer_summary(layer: &PromptManifestLayer) -> String {
    let marker = if layer.enabled { "+" } else { "-" };
    let source = layer
        .source
        .as_deref()
        .or(layer.reason.as_deref())
        .map(|value| truncate_chars(value, SOURCE_MAX_CHARS))
        .unwrap_or_else(|| "(source 없음)".to_string());
    format!(
        "{marker} {:<24} {:<30} {:>7} {}",
        truncate_chars(&layer.layer_name, 24),
        source,
        format_tokens(layer.tokens_est),
        visibility_label(layer.content_visibility)
    )
}

fn layer_display_body(layer: &PromptManifestLayer) -> String {
    let raw = match layer.content_visibility {
        PromptContentVisibility::AdkProvided => layer
            .full_content
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("(content 없음)"),
        PromptContentVisibility::UserDerived => layer
            .redacted_preview
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("(redacted preview 없음)"),
    };
    truncate_chars(raw.trim(), PROMPT_LAYER_CONTENT_MAX_CHARS)
}

fn format_context_usage(turn: &LatestTurn, context: &InspectContextConfig) -> String {
    let used = turn.context_occupancy_input_tokens();
    if context.context_window_tokens == 0 {
        return format!("unknown ({} tokens)", format_tokens(used as i64));
    }
    let pct = crate::services::discord::adk_session::context_usage_percent(
        used,
        context.context_window_tokens,
    );
    format!(
        "{}% ({} / {} tokens), compact threshold {}%",
        pct,
        format_tokens(used as i64),
        format_tokens(context.context_window_tokens as i64),
        context.compact_percent
    )
}

fn format_prompt_summary(manifest: Option<&PromptManifest>) -> String {
    let Some(manifest) = manifest else {
        return "(없음)".to_string();
    };
    let profile = manifest.profile.as_deref().unwrap_or("(profile 없음)");
    format!(
        "{profile}, {} layers, {} tokens",
        manifest.layers.len(),
        format_tokens(manifest.total_input_tokens_est)
    )
}

fn format_compaction(event: Option<&LifecycleEventRow>) -> String {
    let Some(event) = event else {
        return "(없음)".to_string();
    };
    let before = json_u64(&event.details_json, &["before_pct", "beforePct"]);
    let after = json_u64(&event.details_json, &["after_pct", "afterPct"]);
    match (before, after) {
        (Some(before), Some(after)) => {
            format!(
                "{} (before {}% -> after {}%)",
                format_kst(event.created_at),
                before,
                after
            )
        }
        _ => format!("{} ({})", format_kst(event.created_at), event.summary),
    }
}

fn session_status_label(event: &LifecycleEventRow) -> &'static str {
    match event.kind.as_str() {
        "session_fresh" => "fresh",
        "session_resumed" => "resumed",
        "session_resume_failed_with_recovery" => "recovery",
        _ => "unknown",
    }
}

fn session_id_from_event(event: Option<&LifecycleEventRow>) -> Option<&str> {
    event.and_then(|event| {
        json_str(
            &event.details_json,
            &[
                "provider_session_id",
                "providerSessionId",
                "raw_provider_session_id",
                "rawProviderSessionId",
                "session_id",
                "sessionId",
                "claude_session_id",
                "claudeSessionId",
            ],
        )
    })
}

fn adk_session_from_event(event: Option<&LifecycleEventRow>) -> Option<&str> {
    event.and_then(|event| {
        json_str(
            &event.details_json,
            &[
                "recovered_session_key",
                "recoveredSessionKey",
                "previous_session_key",
                "previousSessionKey",
                "session_key",
                "sessionKey",
            ],
        )
    })
}

fn tmux_action_label(event: Option<&LifecycleEventRow>) -> String {
    let Some(event) = event else {
        return "(없음)".to_string();
    };
    if let Some(value) = json_str(
        &event.details_json,
        &["tmux_action", "tmuxAction", "tmux", "backend_action"],
    ) {
        return value.to_string();
    }
    match event.kind.as_str() {
        "session_fresh" => "new session".to_string(),
        "session_resumed" => "reused existing".to_string(),
        "session_resume_failed_with_recovery" => "recovered after resume failure".to_string(),
        _ => "(없음)".to_string(),
    }
}

fn human_recovery_source(source: &str) -> String {
    match source {
        "discord_recent" => "Discord recent messages".to_string(),
        other => other.to_string(),
    }
}

fn json_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn json_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
}

fn visibility_label(visibility: PromptContentVisibility) -> &'static str {
    match visibility {
        PromptContentVisibility::AdkProvided => "adk",
        PromptContentVisibility::UserDerived => "redacted",
    }
}

fn fenced_report(body: impl Into<String>) -> String {
    let body = body.into();
    format!("```text\n{}\n```", escape_for_code_fence(body.trim_end()))
}

fn no_data_report() -> String {
    fenced_report(NO_RECENT_TURN_DATA)
}

fn push_kv(out: &mut String, key: &str, value: impl AsRef<str>) {
    push_line(out, &format!("{key}: {}", value.as_ref()));
}

fn push_line(out: &mut String, line: &str) {
    out.push_str(&truncate_chars(line, REPORT_LINE_MAX));
    out.push('\n');
}

fn opt_or_none(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| truncate_chars(value, ID_MAX_CHARS))
        .unwrap_or_else(|| "(없음)".to_string())
}

fn format_duration(duration_ms: Option<i64>) -> String {
    let Some(duration_ms) = duration_ms.filter(|value| *value >= 0) else {
        return "(없음)".to_string();
    };
    if duration_ms < 1_000 {
        return format!("{duration_ms}ms");
    }
    let secs = duration_ms as f64 / 1_000.0;
    format!("{secs:.1}s")
}

fn format_tokens(tokens: i64) -> String {
    if tokens.abs() >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens.abs() >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn format_kst(value: DateTime<Utc>) -> String {
    let offset = FixedOffset::east_opt(9 * 60 * 60).expect("KST offset is valid");
    let kst = value.with_timezone(&offset);
    kst.format("%Y-%m-%d %H:%M KST").to_string()
}

fn non_negative_u64(value: i64) -> u64 {
    u64::try_from(value.max(0)).unwrap_or(u64::MAX)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let char_count = value.chars().count();
    if char_count <= max_chars {
        return value.to_string();
    }
    let keep = max_chars.saturating_sub(3);
    format!("{}...", value.chars().take(keep).collect::<String>())
}

fn wrap_line(line: &str, max_chars: usize) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    for word in line.split_whitespace() {
        let current_len = current.chars().count();
        let word_len = word.chars().count();
        if current_len > 0 && current_len + 1 + word_len > max_chars {
            result.push(current);
            current = String::new();
        }
        if word_len > max_chars {
            if !current.is_empty() {
                result.push(current);
                current = String::new();
            }
            result.push(truncate_chars(word, max_chars));
            continue;
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        result.push(current);
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manifest() -> PromptManifest {
        PromptManifest {
            id: Some(42),
            created_at: None,
            turn_id: "discord:1:2".to_string(),
            channel_id: "1".to_string(),
            dispatch_id: None,
            profile: Some("Full".to_string()),
            total_input_tokens_est: 1_600,
            layer_count: 2,
            layers: vec![
                PromptManifestLayer {
                    id: None,
                    manifest_id: None,
                    layer_name: "role_prompt".to_string(),
                    enabled: true,
                    source: Some("agents/project-agentdesk.prompt.md".to_string()),
                    reason: None,
                    chars: 400,
                    tokens_est: 100,
                    content_sha256: "a".repeat(64),
                    content_visibility: PromptContentVisibility::AdkProvided,
                    full_content: Some("ADK full body with ``` fence".to_string()),
                    redacted_preview: None,
                },
                PromptManifestLayer {
                    id: None,
                    manifest_id: None,
                    layer_name: "user_message".to_string(),
                    enabled: true,
                    source: Some("discord".to_string()),
                    reason: None,
                    chars: 800,
                    tokens_est: 200,
                    content_sha256: "b".repeat(64),
                    content_visibility: PromptContentVisibility::UserDerived,
                    full_content: Some("SECRET USER BODY MUST NOT LEAK".to_string()),
                    redacted_preview: Some("redacted user preview".to_string()),
                },
            ],
        }
    }

    #[test]
    fn prompt_report_uses_full_adk_body_but_only_redacted_user_preview() {
        let report = render_prompt_manifest_report(&test_manifest());

        assert!(report.contains("ADK full body"));
        assert!(report.contains("redacted user preview"));
        assert!(!report.contains("SECRET USER BODY"));
        assert!(report.contains("``\u{200B}` fence"));
    }

    #[test]
    fn context_report_orders_largest_layers_and_formats_compaction() {
        let turn = LatestTurn {
            turn_id: "discord:1:2".to_string(),
            channel_id: "1".to_string(),
            provider: Some("codex".to_string()),
            session_key: Some("channel:1".to_string()),
            session_id: Some("codex-session".to_string()),
            dispatch_id: None,
            finished_at: DateTime::parse_from_rfc3339("2026-05-01T00:01:00Z")
                .unwrap()
                .with_timezone(&Utc),
            duration_ms: Some(60_000),
            input_tokens: 500,
            cache_create_tokens: 250,
            cache_read_tokens: 250,
        };
        let mut manifest = test_manifest();
        manifest.layers[0].tokens_est = 100;
        manifest.layers[1].tokens_est = 900;
        let compaction = LifecycleEventRow {
            kind: "context_compacted".to_string(),
            severity: "info".to_string(),
            summary: "context compacted".to_string(),
            details_json: serde_json::json!({"before_pct": 88, "after_pct": 41}),
            created_at: turn.finished_at,
        };
        let context = InspectContextConfig {
            provider: ProviderKind::Codex,
            model: Some("gpt-test".to_string()),
            context_window_tokens: 2_000,
            compact_percent: 85,
        };

        let report = render_context_report(&turn, Some(&manifest), Some(&compaction), &context);
        let user_idx = report.find("- user_message").unwrap();
        let role_idx = report.find("- role_prompt").unwrap();

        assert!(report.contains("usage: 50%"));
        assert!(report.contains("before 88% -> after 41%"));
        assert!(user_idx < role_idx);
    }

    #[test]
    fn context_report_omits_other_turns_manifest_when_none() {
        // Regression for #1691: when the latest turn has no manifest yet,
        // we must NOT pull layer data from a different turn's manifest.
        let turn = LatestTurn {
            turn_id: "discord:1:99".to_string(),
            channel_id: "1".to_string(),
            provider: Some("codex".to_string()),
            session_key: Some("channel:1".to_string()),
            session_id: Some("codex-session".to_string()),
            dispatch_id: None,
            finished_at: DateTime::parse_from_rfc3339("2026-05-01T00:01:00Z")
                .unwrap()
                .with_timezone(&Utc),
            duration_ms: Some(60_000),
            input_tokens: 500,
            cache_create_tokens: 250,
            cache_read_tokens: 250,
        };
        let other_turn_manifest = test_manifest();
        let context = InspectContextConfig {
            provider: ProviderKind::Codex,
            model: Some("gpt-test".to_string()),
            context_window_tokens: 2_000,
            compact_percent: 85,
        };

        let report = render_context_report(&turn, None, None, &context);

        // Identifies that this report is bound to the current turn.
        assert!(report.contains("turn_id: discord:1:99"));
        // Manifest-pending indicator is shown.
        assert!(report.contains("manifest pending for this turn"));
        // Layer details from a different turn must not appear.
        for layer in &other_turn_manifest.layers {
            assert!(
                !report.contains(&layer.layer_name),
                "report leaked layer name {} from a different turn",
                layer.layer_name
            );
        }
        // Token usage from the latest turn is still rendered.
        assert!(report.contains("usage: 50%"));
    }

    #[test]
    fn context_report_clamps_context_usage_to_window() {
        let turn = LatestTurn {
            turn_id: "discord:1:3".to_string(),
            channel_id: "1".to_string(),
            provider: Some("codex".to_string()),
            session_key: Some("channel:1".to_string()),
            session_id: Some("codex-session".to_string()),
            dispatch_id: None,
            finished_at: DateTime::parse_from_rfc3339("2026-05-01T00:01:00Z")
                .unwrap()
                .with_timezone(&Utc),
            duration_ms: Some(60_000),
            input_tokens: 50,
            cache_create_tokens: 0,
            cache_read_tokens: 400_000,
        };
        let context = InspectContextConfig {
            provider: ProviderKind::Codex,
            model: Some("gpt-test".to_string()),
            context_window_tokens: 200_000,
            compact_percent: 85,
        };

        let rendered = format_context_usage(&turn, &context);

        assert!(rendered.starts_with("100% "));
        assert!(!rendered.starts_with("200% "));
    }

    #[test]
    fn no_data_report_uses_required_phrase_inside_code_fence() {
        assert_eq!(no_data_report(), "```text\n최근 턴 데이터 없음\n```");
    }
}
