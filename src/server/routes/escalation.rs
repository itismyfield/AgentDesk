use axum::{Json, extract::State, http::StatusCode};
use chrono::{DateTime, NaiveTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::{Config, EscalationMode};
use crate::db::agents::load_agent_channel_bindings;
use crate::server::routes::AppState;
use crate::services::discord::health::active_request_owner_for_channel;

const ESCALATION_SETTINGS_OVERRIDE_KEY: &str = "escalation-settings-override";
const ESCALATION_THREAD_KEY_PREFIX: &str = "escalation_thread:";
const DEFAULT_PM_HOURS: &str = "00:00-08:00";
const DEFAULT_TIMEZONE: &str = "Asia/Seoul";
const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct EscalationScheduleSettings {
    pub pm_hours: String,
    pub timezone: String,
}

impl Default for EscalationScheduleSettings {
    fn default() -> Self {
        Self {
            pm_hours: DEFAULT_PM_HOURS.to_string(),
            timezone: DEFAULT_TIMEZONE.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct EscalationSettings {
    pub mode: EscalationMode,
    pub owner_user_id: Option<u64>,
    pub pm_channel_id: Option<String>,
    pub schedule: EscalationScheduleSettings,
}

impl Default for EscalationSettings {
    fn default() -> Self {
        Self {
            mode: EscalationMode::Pm,
            owner_user_id: None,
            pm_channel_id: None,
            schedule: EscalationScheduleSettings::default(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EscalationSettingsResponse {
    pub current: EscalationSettings,
    pub defaults: EscalationSettings,
}

#[derive(Debug, Deserialize)]
pub struct EmitEscalationBody {
    pub card_id: String,
    #[serde(default)]
    pub reasons: Vec<String>,
}

#[derive(Debug)]
struct CardEscalationSummary {
    title: String,
    issue_number: Option<i64>,
    assigned_agent_id: Option<String>,
}

#[derive(Debug, Clone)]
struct OwnerRoutingTarget {
    user_id: Option<u64>,
    parent_channel_id: Option<u64>,
    source: &'static str,
}

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
}

fn parse_channel_reference(channel: &str) -> Option<u64> {
    channel
        .trim()
        .parse::<u64>()
        .ok()
        .or_else(|| crate::server::routes::dispatches::resolve_channel_alias_pub(channel))
}

fn escalation_defaults(config: &Config) -> EscalationSettings {
    EscalationSettings {
        mode: config.escalation.mode.clone(),
        owner_user_id: config.escalation.owner_user_id.or(config.discord.owner_id),
        pm_channel_id: normalize_optional_string(
            config
                .escalation
                .pm_channel_id
                .clone()
                .or_else(|| config.kanban.manager_channel_id.clone()),
        ),
        schedule: EscalationScheduleSettings {
            pm_hours: config
                .escalation
                .schedule
                .pm_hours
                .clone()
                .unwrap_or_else(|| DEFAULT_PM_HOURS.to_string()),
            timezone: config
                .escalation
                .schedule
                .timezone
                .clone()
                .unwrap_or_else(|| DEFAULT_TIMEZONE.to_string()),
        },
    }
}

fn load_override(conn: &rusqlite::Connection) -> Option<EscalationSettings> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = ?1",
            [ESCALATION_SETTINGS_OVERRIDE_KEY],
            |row| row.get(0),
        )
        .ok();
    raw.and_then(|raw| serde_json::from_str::<EscalationSettings>(&raw).ok())
}

fn merged_settings(conn: &rusqlite::Connection, config: &Config) -> EscalationSettings {
    load_override(conn).unwrap_or_else(|| escalation_defaults(config))
}

pub(in crate::server::routes) fn effective_owner_user_id(
    conn: &rusqlite::Connection,
    config: &Config,
) -> Option<u64> {
    merged_settings(conn, config).owner_user_id
}

fn store_override(
    conn: &rusqlite::Connection,
    settings: &EscalationSettings,
) -> Result<(), String> {
    let raw = serde_json::to_string(settings).map_err(|err| err.to_string())?;
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![ESCALATION_SETTINGS_OVERRIDE_KEY, raw],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

fn clear_override(conn: &rusqlite::Connection) -> Result<(), String> {
    conn.execute(
        "DELETE FROM kv_meta WHERE key = ?1",
        [ESCALATION_SETTINGS_OVERRIDE_KEY],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

fn parse_time_window(raw: &str) -> Option<(NaiveTime, NaiveTime)> {
    let (start, end) = raw.trim().split_once('-')?;
    let start = NaiveTime::parse_from_str(start.trim(), "%H:%M").ok()?;
    let end = NaiveTime::parse_from_str(end.trim(), "%H:%M").ok()?;
    Some((start, end))
}

fn within_pm_window(now: NaiveTime, raw: &str) -> bool {
    let Some((start, end)) = parse_time_window(raw) else {
        return false;
    };
    if start == end {
        return true;
    }
    if start < end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

fn resolve_mode_at(settings: &EscalationSettings, now: DateTime<Utc>) -> EscalationMode {
    match settings.mode {
        EscalationMode::Pm => EscalationMode::Pm,
        EscalationMode::User => EscalationMode::User,
        EscalationMode::Scheduled => {
            let tz = settings
                .schedule
                .timezone
                .parse::<Tz>()
                .unwrap_or(chrono_tz::Asia::Seoul);
            let local_time = now.with_timezone(&tz).time();
            if within_pm_window(local_time, &settings.schedule.pm_hours) {
                EscalationMode::Pm
            } else {
                EscalationMode::User
            }
        }
    }
}

fn load_card_summary(
    conn: &rusqlite::Connection,
    card_id: &str,
) -> Result<CardEscalationSummary, String> {
    conn.query_row(
        "SELECT title, github_issue_number, assigned_agent_id
         FROM kanban_cards
         WHERE id = ?1",
        [card_id],
        |row| {
            Ok(CardEscalationSummary {
                title: row.get(0)?,
                issue_number: row.get(1)?,
                assigned_agent_id: row.get(2)?,
            })
        },
    )
    .map_err(|_| format!("card not found: {card_id}"))
}

fn latest_dispatch_agent_id(conn: &rusqlite::Connection, card_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT to_agent_id
         FROM task_dispatches
         WHERE kanban_card_id = ?1
           AND to_agent_id IS NOT NULL
           AND TRIM(to_agent_id) != ''
         ORDER BY datetime(created_at) DESC, rowid DESC
         LIMIT 1",
        [card_id],
        |row| row.get(0),
    )
    .ok()
}

fn candidate_parent_channels(
    conn: &rusqlite::Connection,
    card_id: &str,
    assigned_agent_id: Option<&str>,
) -> Vec<u64> {
    let mut agent_ids = Vec::new();
    if let Some(agent_id) = latest_dispatch_agent_id(conn, card_id) {
        agent_ids.push(agent_id);
    }
    if let Some(agent_id) = assigned_agent_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if !agent_ids.iter().any(|existing| existing == agent_id) {
            agent_ids.push(agent_id.to_string());
        }
    }

    let mut channels = Vec::new();
    for agent_id in agent_ids {
        let Ok(Some(bindings)) = load_agent_channel_bindings(conn, &agent_id) else {
            continue;
        };
        for channel in bindings.all_channels() {
            let Some(channel_id) = parse_channel_reference(&channel) else {
                continue;
            };
            if !channels.contains(&channel_id) {
                channels.push(channel_id);
            }
        }
    }
    channels
}

async fn resolve_owner_target(
    state: &AppState,
    channels: &[u64],
    configured_owner_user_id: Option<u64>,
) -> OwnerRoutingTarget {
    if let Some(registry) = state.health_registry.as_ref() {
        for channel_id in channels {
            if let Some(owner) = active_request_owner_for_channel(registry, *channel_id).await {
                return OwnerRoutingTarget {
                    user_id: Some(owner),
                    parent_channel_id: Some(*channel_id),
                    source: "live_owner",
                };
            }
        }
    }

    for channel_id in channels {
        if let Some(owner) =
            crate::services::discord::latest_request_owner_user_id_for_channel(*channel_id)
        {
            return OwnerRoutingTarget {
                user_id: Some(owner),
                parent_channel_id: Some(*channel_id),
                source: "inflight_owner",
            };
        }
    }

    OwnerRoutingTarget {
        user_id: configured_owner_user_id,
        parent_channel_id: channels.first().copied(),
        source: "configured_owner",
    }
}

fn escalation_thread_key(card_id: &str) -> String {
    format!("{ESCALATION_THREAD_KEY_PREFIX}{card_id}")
}

fn load_cached_thread_id(conn: &rusqlite::Connection, card_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM kv_meta WHERE key = ?1",
        [escalation_thread_key(card_id)],
        |row| row.get(0),
    )
    .ok()
}

fn save_cached_thread_id(
    conn: &rusqlite::Connection,
    card_id: &str,
    thread_id: &str,
) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![escalation_thread_key(card_id), thread_id],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

fn clear_cached_thread_id(conn: &rusqlite::Connection, card_id: &str) -> Result<(), String> {
    conn.execute(
        "DELETE FROM kv_meta WHERE key = ?1",
        [escalation_thread_key(card_id)],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

fn format_card_label(summary: &CardEscalationSummary) -> String {
    if let Some(issue_number) = summary.issue_number {
        format!("#{issue_number} {}", summary.title)
    } else {
        summary.title.clone()
    }
}

fn build_user_thread_name(summary: &CardEscalationSummary) -> String {
    if let Some(issue_number) = summary.issue_number {
        format!("⚠️ [에스컬레이션] #{issue_number} {}", summary.title)
    } else {
        format!("⚠️ [에스컬레이션] {}", summary.title)
    }
}

fn format_reason_lines(reasons: &[String]) -> String {
    reasons
        .iter()
        .map(|reason| format!("- {reason}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_user_message(
    summary: &CardEscalationSummary,
    owner_user_id: u64,
    reasons: &[String],
) -> String {
    format!(
        "⚠️ [에스컬레이션] {}\n<@{}> 수동 판단이 필요합니다.\n사유:\n{}\n선택지: `resume`, `rework`, `dismiss`, `requeue`\n결정 API: `POST /api/pm-decision`",
        format_card_label(summary),
        owner_user_id,
        format_reason_lines(reasons)
    )
}

fn build_pm_message(
    summary: &CardEscalationSummary,
    reasons: &[String],
    fallback_note: Option<&str>,
) -> String {
    let mut lines = vec![format!("⚠️ [PM 결정 요청] {}", format_card_label(summary))];
    if let Some(note) = fallback_note {
        lines.push(format!("fallback: {note}"));
    }
    lines.push("카드에 수동 판단이 필요합니다. 다음 조치를 결정해주세요.".to_string());
    lines.push("사유:".to_string());
    lines.push(format_reason_lines(reasons));
    lines.push("선택지: `resume`, `rework`, `dismiss`, `requeue`".to_string());
    lines.push("결정 API: `POST /api/pm-decision`".to_string());
    lines.join("\n")
}

async fn discord_get(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    path: &str,
) -> Result<reqwest::Response, String> {
    client
        .get(format!("{}{}", base_url.trim_end_matches('/'), path))
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
        .map_err(|err| err.to_string())
}

async fn discord_post_json(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> Result<reqwest::Response, String> {
    client
        .post(format!("{}{}", base_url.trim_end_matches('/'), path))
        .header("Authorization", format!("Bot {}", token))
        .json(body)
        .send()
        .await
        .map_err(|err| err.to_string())
}

async fn discord_patch_json(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> Result<reqwest::Response, String> {
    client
        .patch(format!("{}{}", base_url.trim_end_matches('/'), path))
        .header("Authorization", format!("Bot {}", token))
        .json(body)
        .send()
        .await
        .map_err(|err| err.to_string())
}

async fn send_channel_message(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    channel_id: &str,
    content: &str,
) -> Result<(), String> {
    let response = discord_post_json(
        client,
        base_url,
        token,
        &format!("/channels/{channel_id}/messages"),
        &json!({ "content": content }),
    )
    .await?;
    if response.status().is_success() {
        Ok(())
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(format!("discord message failed: {status} {body}"))
    }
}

async fn try_reuse_escalation_thread(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    thread_id: &str,
    desired_name: &str,
    message: &str,
) -> Result<bool, String> {
    let response = discord_get(client, base_url, token, &format!("/channels/{thread_id}")).await?;
    if !response.status().is_success() {
        return Ok(false);
    }
    let body: serde_json::Value = response.json().await.map_err(|err| err.to_string())?;
    let metadata = body.get("thread_metadata");
    if metadata
        .and_then(|value| value.get("locked"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        return Ok(false);
    }
    if metadata
        .and_then(|value| value.get("archived"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        let response = discord_patch_json(
            client,
            base_url,
            token,
            &format!("/channels/{thread_id}"),
            &json!({ "archived": false }),
        )
        .await?;
        if !response.status().is_success() {
            return Ok(false);
        }
    }
    let current_name = body
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if !desired_name.is_empty() && current_name != desired_name {
        let _ = discord_patch_json(
            client,
            base_url,
            token,
            &format!("/channels/{thread_id}"),
            &json!({ "name": desired_name }),
        )
        .await;
    }
    send_channel_message(client, base_url, token, thread_id, message).await?;
    Ok(true)
}

async fn create_escalation_thread(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    parent_channel_id: u64,
    thread_name: &str,
) -> Result<String, String> {
    let response = discord_post_json(
        client,
        base_url,
        token,
        &format!("/channels/{parent_channel_id}/threads"),
        &json!({
            "name": thread_name,
            "type": 11,
            "auto_archive_duration": 1440
        }),
    )
    .await?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("discord thread create failed: {status} {body}"));
    }
    let body: serde_json::Value = response.json().await.map_err(|err| err.to_string())?;
    body.get("id")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .ok_or_else(|| "discord thread create missing id".to_string())
}

async fn emit_escalation_with_base_url(
    state: &AppState,
    body: EmitEscalationBody,
    base_url: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    let card_id = body.card_id.trim().to_string();
    let reasons = body
        .reasons
        .into_iter()
        .map(|reason| reason.trim().to_string())
        .filter(|reason| !reason.is_empty())
        .collect::<Vec<_>>();
    if card_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "card_id is required"})),
        );
    }
    if reasons.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "reasons must not be empty"})),
        );
    }

    let (settings, summary, parent_channels, cached_thread_id) = {
        let conn = match state.db.separate_conn() {
            Ok(conn) => conn,
            Err(err) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("db open failed: {err}")})),
                );
            }
        };
        let settings = merged_settings(&conn, &state.config);
        let summary = match load_card_summary(&conn, &card_id) {
            Ok(summary) => summary,
            Err(err) => return (StatusCode::NOT_FOUND, Json(json!({"error": err}))),
        };
        let parent_channels =
            candidate_parent_channels(&conn, &card_id, summary.assigned_agent_id.as_deref());
        let cached_thread_id = load_cached_thread_id(&conn, &card_id);
        (settings, summary, parent_channels, cached_thread_id)
    };

    let client = reqwest::Client::new();
    let requested_mode = settings.mode.clone();
    let resolved_mode = resolve_mode_at(&settings, Utc::now());

    // PM 에스컬레이션 → announce 봇 (PM 에이전트가 반응해야 함)
    // User 에스컬레이션 → notify 봇 (사람이 읽고 직접 반응)
    let bot_name = if resolved_mode == EscalationMode::User {
        "notify"
    } else {
        "announce"
    };
    let announce_token = match crate::credential::read_bot_token(bot_name)
        .or_else(|| crate::credential::read_bot_token("announce"))
    {
        Some(token) => token,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "bot token not found"})),
            );
        }
    };

    if resolved_mode == EscalationMode::User {
        let owner_target =
            resolve_owner_target(state, &parent_channels, settings.owner_user_id).await;
        if let Some(owner_user_id) = owner_target.user_id {
            let thread_name = build_user_thread_name(&summary);
            let message = build_user_message(&summary, owner_user_id, &reasons);

            if let Some(thread_id) = cached_thread_id {
                match try_reuse_escalation_thread(
                    &client,
                    base_url,
                    &announce_token,
                    &thread_id,
                    &thread_name,
                    &message,
                )
                .await
                {
                    Ok(true) => {
                        return (
                            StatusCode::OK,
                            Json(json!({
                                "ok": true,
                                "requested_mode": requested_mode,
                                "resolved_mode": resolved_mode,
                                "delivery": "user_thread_reused",
                                "thread_id": thread_id,
                                "owner_user_id": owner_user_id,
                                "owner_source": owner_target.source,
                            })),
                        );
                    }
                    Ok(false) => {
                        if let Ok(conn) = state.db.separate_conn() {
                            let _ = clear_cached_thread_id(&conn, &card_id);
                        }
                    }
                    Err(err) => {
                        tracing::warn!("[escalation] thread reuse failed for {card_id}: {err}");
                        if let Ok(conn) = state.db.separate_conn() {
                            let _ = clear_cached_thread_id(&conn, &card_id);
                        }
                    }
                }
            }

            if let Some(parent_channel_id) = owner_target.parent_channel_id {
                match create_escalation_thread(
                    &client,
                    base_url,
                    &announce_token,
                    parent_channel_id,
                    &thread_name,
                )
                .await
                {
                    Ok(thread_id) => {
                        if let Err(err) = send_channel_message(
                            &client,
                            base_url,
                            &announce_token,
                            &thread_id,
                            &message,
                        )
                        .await
                        {
                            tracing::warn!(
                                "[escalation] failed to send initial thread message for {card_id}: {err}"
                            );
                            let fallback_note =
                                "user thread creation succeeded but message send failed";
                            return deliver_pm_fallback(
                                &client,
                                base_url,
                                &announce_token,
                                &settings,
                                &summary,
                                &reasons,
                                fallback_note,
                                requested_mode,
                                resolved_mode,
                            )
                            .await;
                        }
                        if let Ok(conn) = state.db.separate_conn() {
                            if let Err(err) = save_cached_thread_id(&conn, &card_id, &thread_id) {
                                tracing::warn!(
                                    "[escalation] failed to cache thread for {card_id}: {err}"
                                );
                            }
                        }
                        return (
                            StatusCode::OK,
                            Json(json!({
                                "ok": true,
                                "requested_mode": requested_mode,
                                "resolved_mode": resolved_mode,
                                "delivery": "user_thread_created",
                                "thread_id": thread_id,
                                "parent_channel_id": parent_channel_id,
                                "owner_user_id": owner_user_id,
                                "owner_source": owner_target.source,
                            })),
                        );
                    }
                    Err(err) => {
                        tracing::warn!("[escalation] thread create failed for {card_id}: {err}");
                    }
                }
            }
        }

        return deliver_pm_fallback(
            &client,
            base_url,
            &announce_token,
            &settings,
            &summary,
            &reasons,
            "owner routing unavailable",
            requested_mode,
            resolved_mode,
        )
        .await;
    }

    deliver_pm_fallback(
        &client,
        base_url,
        &announce_token,
        &settings,
        &summary,
        &reasons,
        None,
        requested_mode,
        resolved_mode,
    )
    .await
}

async fn deliver_pm_fallback(
    client: &reqwest::Client,
    base_url: &str,
    announce_token: &str,
    settings: &EscalationSettings,
    summary: &CardEscalationSummary,
    reasons: &[String],
    fallback_note: impl Into<Option<&'static str>>,
    requested_mode: EscalationMode,
    resolved_mode: EscalationMode,
) -> (StatusCode, Json<serde_json::Value>) {
    let fallback_note = fallback_note.into();
    let pm_channel = settings
        .pm_channel_id
        .as_deref()
        .and_then(parse_channel_reference);
    let Some(pm_channel_id) = pm_channel else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "pm_channel_id is not configured"})),
        );
    };
    let pm_channel_id = pm_channel_id.to_string();

    let message = build_pm_message(summary, reasons, fallback_note);
    match send_channel_message(client, base_url, announce_token, &pm_channel_id, &message).await {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "requested_mode": requested_mode,
                "resolved_mode": resolved_mode,
                "delivery": "pm_channel",
                "pm_channel_id": pm_channel_id,
                "fallback_note": fallback_note,
            })),
        ),
        Err(err) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("pm delivery failed: {err}")})),
        ),
    }
}

pub fn seed_escalation_defaults(conn: &rusqlite::Connection, config: &Config) {
    if config.runtime.reset_overrides_on_restart {
        let _ = clear_override(conn);
    }
}

/// GET /api/settings/escalation
pub async fn get_escalation_settings(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.read_conn() {
        Ok(conn) => conn,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("db open failed: {err}")})),
            );
        }
    };
    let defaults = escalation_defaults(&state.config);
    let current = merged_settings(&conn, &state.config);
    (
        StatusCode::OK,
        Json(
            serde_json::to_value(EscalationSettingsResponse { current, defaults })
                .unwrap_or_else(|_| json!({"error": "serialization failed"})),
        ),
    )
}

/// PUT /api/settings/escalation
pub async fn put_escalation_settings(
    State(state): State<AppState>,
    Json(mut body): Json<EscalationSettings>,
) -> (StatusCode, Json<serde_json::Value>) {
    body.pm_channel_id = normalize_optional_string(body.pm_channel_id.take());
    if body.schedule.timezone.parse::<Tz>().is_err() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "schedule.timezone must be a valid IANA timezone"})),
        );
    }
    if parse_time_window(&body.schedule.pm_hours).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "schedule.pm_hours must be HH:MM-HH:MM"})),
        );
    }

    let defaults = escalation_defaults(&state.config);
    let conn = match state.db.separate_conn() {
        Ok(conn) => conn,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("db open failed: {err}")})),
            );
        }
    };

    let store_result = if body == defaults {
        clear_override(&conn)
    } else {
        store_override(&conn, &body)
    };
    if let Err(err) = store_result {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err})),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "current": merged_settings(&conn, &state.config),
            "defaults": defaults,
        })),
    )
}

/// POST /api/internal/escalation/emit
pub async fn emit_escalation(
    State(state): State<AppState>,
    Json(body): Json<EmitEscalationBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    emit_escalation_with_base_url(&state, body, DISCORD_API_BASE).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{Path, State},
        response::IntoResponse,
        routing::{get, post},
    };
    use std::{
        ffi::OsString,
        path::Path as FsPath,
        sync::{Arc, Mutex},
    };

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::services::discord::runtime_store::lock_test_env()
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set_path(key: &'static str, value: &FsPath) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn write_test_bot_tokens(root: &FsPath) {
        let credential_dir = root.join("credential");
        std::fs::create_dir_all(&credential_dir).unwrap();
        std::fs::write(
            credential_dir.join("announce_bot_token"),
            "announce-token\n",
        )
        .unwrap();
        std::fs::write(credential_dir.join("notify_bot_token"), "notify-token\n").unwrap();
    }

    fn test_db() -> crate::db::Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    fn test_engine(db: &crate::db::Db) -> crate::engine::PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
        config.policies.hot_reload = false;
        crate::engine::PolicyEngine::new(&config, db.clone()).unwrap()
    }

    #[test]
    fn scheduled_mode_switches_between_pm_and_user() {
        let settings = EscalationSettings {
            mode: EscalationMode::Scheduled,
            owner_user_id: Some(1),
            pm_channel_id: Some("123".to_string()),
            schedule: EscalationScheduleSettings {
                pm_hours: "23:00-06:00".to_string(),
                timezone: "Asia/Seoul".to_string(),
            },
        };

        let pm_time = DateTime::parse_from_rfc3339("2026-04-11T16:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let user_time = DateTime::parse_from_rfc3339("2026-04-11T02:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        assert_eq!(resolve_mode_at(&settings, pm_time), EscalationMode::Pm);
        assert_eq!(resolve_mode_at(&settings, user_time), EscalationMode::User);
    }

    #[tokio::test]
    async fn put_and_get_escalation_settings_round_trip() {
        let db = test_db();
        let state = AppState::test_state_with_config(
            db.clone(),
            test_engine(&db),
            crate::config::Config::default(),
        );

        let (status, Json(body)) = put_escalation_settings(
            State(state.clone()),
            Json(EscalationSettings {
                mode: EscalationMode::User,
                owner_user_id: Some(343742347365974026),
                pm_channel_id: Some("123456789".to_string()),
                schedule: EscalationScheduleSettings {
                    pm_hours: "01:00-08:00".to_string(),
                    timezone: "Asia/Seoul".to_string(),
                },
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["current"]["mode"], json!("user"));

        let (status, Json(body)) = get_escalation_settings(State(state)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["current"]["mode"], json!("user"));
        assert_eq!(body["current"]["pm_channel_id"], json!("123456789"));
    }

    #[tokio::test]
    async fn user_mode_reuses_existing_thread_for_same_card() {
        let _env_lock = env_lock();
        let runtime_root = tempfile::tempdir().unwrap();
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
        write_test_bot_tokens(runtime_root.path());

        #[derive(Clone, Default)]
        struct MockDiscord {
            created_threads: Arc<Mutex<usize>>,
            sent_messages: Arc<Mutex<Vec<String>>>,
        }

        async fn get_channel(Path(channel_id): Path<String>) -> impl IntoResponse {
            match channel_id.as_str() {
                "thread-1" => (
                    StatusCode::OK,
                    Json(json!({
                        "id": "thread-1",
                        "name": "⚠️ [에스컬레이션] #422 Escalation card",
                        "thread_metadata": { "locked": false, "archived": false }
                    })),
                )
                    .into_response(),
                _ => StatusCode::NOT_FOUND.into_response(),
            }
        }

        async fn create_thread(
            State(mock): State<MockDiscord>,
            Path(_channel_id): Path<String>,
        ) -> impl IntoResponse {
            *mock.created_threads.lock().unwrap() += 1;
            (StatusCode::OK, Json(json!({"id": "thread-1"})))
        }

        async fn send_message(
            State(mock): State<MockDiscord>,
            Path(channel_id): Path<String>,
            Json(body): Json<serde_json::Value>,
        ) -> impl IntoResponse {
            mock.sent_messages.lock().unwrap().push(format!(
                "{channel_id}:{}",
                body["content"].as_str().unwrap_or("")
            ));
            (StatusCode::OK, Json(json!({"ok": true})))
        }

        async fn patch_channel(Path(_channel_id): Path<String>) -> impl IntoResponse {
            (StatusCode::OK, Json(json!({"ok": true})))
        }

        let mock = MockDiscord::default();
        let app = Router::new()
            .route(
                "/channels/{channel_id}",
                get(get_channel).patch(patch_channel),
            )
            .route("/channels/{channel_id}/threads", post(create_thread))
            .route("/channels/{channel_id}/messages", post(send_message))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
                 VALUES ('agent-1', 'Agent One', 'claude', '111', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, review_status, github_issue_number, assigned_agent_id, created_at, updated_at)
                 VALUES ('card-1', 'Escalation card', 'review', 'high', 'dilemma_pending', 422, 'agent-1', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        let mut config = crate::config::Config::default();
        config.escalation.mode = EscalationMode::User;
        config.escalation.owner_user_id = Some(343742347365974026);
        let state = AppState::test_state_with_config(db.clone(), test_engine(&db), config);

        let body = EmitEscalationBody {
            card_id: "card-1".to_string(),
            reasons: vec!["review rounds exceeded".to_string()],
        };
        let (status, Json(first_body)) =
            emit_escalation_with_base_url(&state, body, &format!("http://{addr}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first_body["delivery"], json!("user_thread_created"));

        let body = EmitEscalationBody {
            card_id: "card-1".to_string(),
            reasons: vec!["timeout".to_string()],
        };
        let (status, Json(second_body)) =
            emit_escalation_with_base_url(&state, body, &format!("http://{addr}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(second_body["delivery"], json!("user_thread_reused"));

        assert_eq!(*mock.created_threads.lock().unwrap(), 1);
        assert_eq!(mock.sent_messages.lock().unwrap().len(), 2);

        server.abort();
    }

    #[tokio::test]
    async fn user_mode_falls_back_to_pm_when_owner_routing_unavailable() {
        let _env_lock = env_lock();
        let runtime_root = tempfile::tempdir().unwrap();
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
        write_test_bot_tokens(runtime_root.path());

        async fn send_message(
            Path(channel_id): Path<String>,
            Json(body): Json<serde_json::Value>,
        ) -> impl IntoResponse {
            (
                StatusCode::OK,
                Json(json!({"channel_id": channel_id, "content": body["content"]})),
            )
        }

        let app = Router::new().route("/channels/{channel_id}/messages", post(send_message));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
                 VALUES ('agent-2', 'Agent Two', 'claude', NULL, datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, review_status, github_issue_number, assigned_agent_id, created_at, updated_at)
                 VALUES ('card-2', 'Fallback card', 'review', 'high', 'dilemma_pending', 434, 'agent-2', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        let mut config = crate::config::Config::default();
        config.escalation.mode = EscalationMode::User;
        config.escalation.pm_channel_id = Some("222".to_string());
        let state = AppState::test_state_with_config(db.clone(), test_engine(&db), config);

        let (status, Json(body)) = emit_escalation_with_base_url(
            &state,
            EmitEscalationBody {
                card_id: "card-2".to_string(),
                reasons: vec!["owner missing".to_string()],
            },
            &format!("http://{addr}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["delivery"], json!("pm_channel"));
        assert_eq!(body["fallback_note"], json!("owner routing unavailable"));

        server.abort();
    }
}
