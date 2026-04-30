use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

use crate::services::discord::outbound::{
    DeliveryResult, DiscordOutboundMessage, DiscordOutboundPolicy, HttpOutboundClient,
    OutboundDeduper, deliver_outbound,
};

#[derive(Clone, Debug)]
pub struct IssueAnnouncementCreate {
    pub repo: String,
    pub issue_number: i64,
    pub issue_url: String,
    pub title: String,
    pub agent_id: Option<String>,
    pub announcement_channel_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueAnnouncementCreated {
    pub channel_id: String,
    pub message_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IssueCompletionKind {
    Closed,
    Merged,
}

impl IssueCompletionKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Merged => "merged",
        }
    }
}

#[derive(Clone, Debug)]
pub struct IssueCompletionEvent {
    pub repo: String,
    pub issue_number: i64,
    pub title: Option<String>,
    pub kind: IssueCompletionKind,
    pub pr_number: Option<i64>,
    pub pr_url: Option<String>,
}

#[derive(Clone, Debug)]
struct IssueAnnouncementRow {
    title: String,
    channel_id: String,
    message_id: String,
    created_at: DateTime<Utc>,
}

pub async fn create_issue_announcement_pg(
    pool: &PgPool,
    input: IssueAnnouncementCreate,
) -> Result<Option<IssueAnnouncementCreated>, String> {
    let Some(channel_id) = resolve_announcement_channel_pg(
        pool,
        input.announcement_channel_id.as_deref(),
        input.agent_id.as_deref(),
    )
    .await?
    else {
        return Ok(None);
    };

    let token = crate::credential::read_bot_token("notify")
        .ok_or_else(|| "no notify bot token configured".to_string())?;
    let created_at = Utc::now();
    let content = render_active_card(
        input.issue_number,
        &input.title,
        input.agent_id.as_deref(),
        created_at,
    );
    let message_id = send_issue_announcement_message(
        &token,
        &channel_id,
        None,
        &content,
        &format!("issue_announcement:{}:{}", input.repo, input.issue_number),
        "created",
    )
    .await?;

    sqlx::query(
        "INSERT INTO issue_announcements (
            repo, issue_number, issue_url, title, agent_id,
            channel_id, message_id, created_at, updated_at
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW())
         ON CONFLICT (repo, issue_number) DO UPDATE
         SET issue_url = EXCLUDED.issue_url,
             title = EXCLUDED.title,
             agent_id = EXCLUDED.agent_id,
             channel_id = EXCLUDED.channel_id,
             message_id = EXCLUDED.message_id,
             last_edit_error = NULL,
             invalid_at = NULL,
             updated_at = NOW()",
    )
    .bind(&input.repo)
    .bind(input.issue_number)
    .bind(&input.issue_url)
    .bind(&input.title)
    .bind(&input.agent_id)
    .bind(&channel_id)
    .bind(&message_id)
    .bind(created_at)
    .execute(pool)
    .await
    .map_err(|error| {
        format!(
            "persist issue announcement {}#{}: {error}",
            input.repo, input.issue_number
        )
    })?;

    Ok(Some(IssueAnnouncementCreated {
        channel_id,
        message_id,
    }))
}

pub async fn complete_issue_announcement_pg(
    pool: &PgPool,
    event: IssueCompletionEvent,
) -> Result<bool, String> {
    let Some(row) = load_open_announcement_pg(pool, &event.repo, event.issue_number).await? else {
        return Ok(false);
    };

    let title = event.title.as_deref().unwrap_or(&row.title);
    let content = render_completed_card(title, &row, &event);
    let log_key = format!("issue_announcement:{}:{}", event.repo, event.issue_number);
    let edit_result = edit_announcement_with_legacy_fallback(
        &row.channel_id,
        &row.message_id,
        &content,
        &log_key,
    )
    .await;

    match edit_result {
        Ok(_) => {
            sqlx::query(
                "UPDATE issue_announcements
                 SET completed_at = NOW(),
                     completion_kind = $3,
                     completion_pr_number = $4,
                     completion_pr_url = $5,
                     last_edit_error = NULL,
                     updated_at = NOW()
                 WHERE repo = $1 AND issue_number = $2",
            )
            .bind(&event.repo)
            .bind(event.issue_number)
            .bind(event.kind.as_str())
            .bind(event.pr_number)
            .bind(&event.pr_url)
            .execute(pool)
            .await
            .map_err(|error| {
                format!(
                    "mark issue announcement completed {}#{}: {error}",
                    event.repo, event.issue_number
                )
            })?;
            Ok(true)
        }
        Err(error) => {
            sqlx::query(
                "UPDATE issue_announcements
                 SET last_edit_error = $3,
                     invalid_at = CASE
                         WHEN $3 ILIKE '%404%' OR $3 ILIKE '%unknown message%' THEN NOW()
                         ELSE invalid_at
                     END,
                     updated_at = NOW()
                 WHERE repo = $1 AND issue_number = $2",
            )
            .bind(&event.repo)
            .bind(event.issue_number)
            .bind(&error)
            .execute(pool)
            .await
            .map_err(|update_error| {
                format!(
                    "record issue announcement edit failure {}#{}: {update_error}",
                    event.repo, event.issue_number
                )
            })?;
            Err(error)
        }
    }
}

/// Edits an existing issue-announcement message, falling back to the
/// legacy `announce` token when the message was authored by announce-bot
/// before the #1448 follow-up moved announcements to `notify`.
///
/// Fallback triggers (the announce-only deployment shape is the legacy
/// reality we have to support during the cutover window):
/// - `notify` token is not configured (deployment hasn't seeded notify yet)
/// - `notify` PATCH returns Discord 50005 (cross-bot edit on a message
///   authored by announce-bot)
/// - `notify` PATCH returns 50001 / 403 / "missing access" (notify bot
///   doesn't yet have access to the legacy announcement channel)
///
/// Once all legacy announce-authored rows have closed and notify-bot has
/// permissions on every announcement channel, both this fallback and the
/// matching `is_legacy_announce_issue_card` gate in `mod.rs` can be
/// removed (sunset target 2026-06-01).
async fn edit_announcement_with_legacy_fallback(
    channel_id: &str,
    message_id: &str,
    content: &str,
    log_key: &str,
) -> Result<String, String> {
    let notify_token = crate::credential::read_bot_token("notify");
    if let Some(token) = notify_token.as_deref() {
        match send_issue_announcement_message(
            token,
            channel_id,
            Some(message_id),
            content,
            log_key,
            "completed",
        )
        .await
        {
            Ok(id) => return Ok(id),
            Err(error) if should_try_legacy_announce_fallback(&error) => {
                tracing::info!(
                    target: "issue_announcements",
                    "{log_key}: notify edit failed with `{error}`, retrying with announce token"
                );
            }
            Err(error) => return Err(error),
        }
    }
    let Some(announce_token) = crate::credential::read_bot_token("announce") else {
        return Err(if notify_token.is_some() {
            "notify edit failed and no announce bot token configured for legacy fallback"
                .to_string()
        } else {
            "no notify or announce bot token configured".to_string()
        });
    };
    send_issue_announcement_message(
        &announce_token,
        channel_id,
        Some(message_id),
        content,
        log_key,
        "completed-legacy-fallback",
    )
    .await
}

/// Decides whether a notify-side PATCH failure should trigger the
/// announce-token fallback. Cross-bot (50005) and missing-access (50001 /
/// 403) errors all indicate the message was originally authored by
/// announce-bot or that notify-bot has no permission on the legacy
/// channel — both correctable by retrying with announce.
fn should_try_legacy_announce_fallback(error: &str) -> bool {
    let lowered = error.to_ascii_lowercase();
    error.contains("50005")
        || error.contains("50001")
        || lowered.contains("cannot edit a message authored by another user")
        || lowered.contains("missing access")
        || lowered.contains("403 forbidden")
}

async fn resolve_announcement_channel_pg(
    pool: &PgPool,
    requested_channel_id: Option<&str>,
    agent_id: Option<&str>,
) -> Result<Option<String>, String> {
    if let Some(channel_id) = requested_channel_id.and_then(trim_non_empty) {
        return Ok(Some(normalize_channel_id(&channel_id)));
    }

    let Some(agent_id) = agent_id.and_then(trim_non_empty) else {
        return Ok(None);
    };
    let Some(channel_id) = crate::db::agents::resolve_agent_primary_channel_pg(pool, &agent_id)
        .await
        .map_err(|error| format!("resolve announcement channel for {agent_id}: {error}"))?
    else {
        return Ok(None);
    };
    Ok(Some(normalize_channel_id(&channel_id)))
}

async fn load_open_announcement_pg(
    pool: &PgPool,
    repo: &str,
    issue_number: i64,
) -> Result<Option<IssueAnnouncementRow>, String> {
    let row = sqlx::query(
        "SELECT title, channel_id, message_id, created_at
         FROM issue_announcements
         WHERE repo = $1
           AND issue_number = $2
           AND completed_at IS NULL
           AND invalid_at IS NULL
         ORDER BY id DESC
         LIMIT 1",
    )
    .bind(repo)
    .bind(issue_number)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load issue announcement {repo}#{issue_number}: {error}"))?;

    row.map(|row| {
        Ok(IssueAnnouncementRow {
            title: row
                .try_get("title")
                .map_err(|error| format!("read issue announcement title: {error}"))?,
            channel_id: row
                .try_get("channel_id")
                .map_err(|error| format!("read issue announcement channel_id: {error}"))?,
            message_id: row
                .try_get("message_id")
                .map_err(|error| format!("read issue announcement message_id: {error}"))?,
            created_at: row
                .try_get("created_at")
                .map_err(|error| format!("read issue announcement created_at: {error}"))?,
        })
    })
    .transpose()
}

fn render_active_card(
    issue_number: i64,
    title: &str,
    agent_id: Option<&str>,
    created_at: DateTime<Utc>,
) -> String {
    let assignee = agent_id
        .and_then(trim_non_empty)
        .map(|agent_id| format!("agent:{agent_id}"))
        .unwrap_or_else(|| "unassigned".to_string());
    format!(
        "📋 **새 이슈 #{issue_number}** — {title}\n> 상태: 🟡 open\n> 담당: {assignee}\n> 발행: <t:{}:R>",
        created_at.timestamp()
    )
}

fn render_completed_card(
    title: &str,
    row: &IssueAnnouncementRow,
    event: &IssueCompletionEvent,
) -> String {
    let completion = match event.kind {
        IssueCompletionKind::Merged => match (event.pr_number, event.pr_url.as_deref()) {
            (Some(number), Some(url)) => format!("머지: PR #{number} {url}"),
            (Some(number), None) => format!("머지: PR #{number}"),
            (None, _) => "머지: PR merged".to_string(),
        },
        IssueCompletionKind::Closed => "종료: issue closed".to_string(),
    };
    format!(
        "✅ **#{} 완료** — {title}\n> {completion}\n> 소요: {}\n> 발행: <t:{}:R>",
        event.issue_number,
        format_elapsed(Utc::now().signed_duration_since(row.created_at)),
        row.created_at.timestamp()
    )
}

fn format_elapsed(duration: chrono::Duration) -> String {
    let seconds = duration.num_seconds().max(0);
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

async fn send_issue_announcement_message(
    token: &str,
    channel_id: &str,
    edit_message_id: Option<&str>,
    content: &str,
    correlation_id: &str,
    event_id: &str,
) -> Result<String, String> {
    let client = HttpOutboundClient::new(
        reqwest::Client::new(),
        token.to_string(),
        crate::server::routes::dispatches::discord_delivery::discord_api_base_url(),
    );
    let mut message = DiscordOutboundMessage::new(channel_id.to_string(), content.to_string())
        .with_correlation(correlation_id.to_string(), event_id.to_string());
    if let Some(message_id) = edit_message_id {
        message = message.with_edit_message_id(message_id.to_string());
    }
    let policy = DiscordOutboundPolicy::review_notification(None);
    match deliver_outbound(&client, issue_announcement_deduper(), message, policy).await {
        DeliveryResult::Success { message_id } | DeliveryResult::Fallback { message_id, .. } => {
            Ok(message_id)
        }
        DeliveryResult::Duplicate { message_id } => {
            message_id.ok_or_else(|| "duplicate issue announcement without message id".to_string())
        }
        DeliveryResult::Skipped { .. } => Err("issue announcement delivery skipped".to_string()),
        DeliveryResult::PermanentFailure { detail } => Err(detail),
    }
}

fn issue_announcement_deduper() -> &'static OutboundDeduper {
    static DEDUPER: std::sync::OnceLock<OutboundDeduper> = std::sync::OnceLock::new();
    DEDUPER.get_or_init(OutboundDeduper::new)
}

fn normalize_channel_id(channel_id: &str) -> String {
    let trimmed = channel_id.trim();
    if trimmed.parse::<u64>().is_ok() {
        return trimmed.to_string();
    }
    crate::server::routes::dispatches::resolve_channel_alias_pub(trimmed)
        .map(|value| value.to_string())
        .unwrap_or_else(|| trimmed.to_string())
}

fn trim_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;

    #[test]
    fn renders_active_card_with_agent_and_timestamp() {
        let created_at = DateTime::parse_from_rfc3339("2026-04-29T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let rendered = render_active_card(1331, "Lifecycle", Some("project-agentdesk"), created_at);

        assert!(rendered.contains("📋 **새 이슈 #1331** — Lifecycle"));
        assert!(rendered.contains("> 상태: 🟡 open"));
        assert!(rendered.contains("> 담당: agent:project-agentdesk"));
        assert!(rendered.contains("> 발행: <t:1777420800:R>"));
    }

    #[test]
    fn renders_completed_card_for_pr_merge() {
        let created_at = DateTime::parse_from_rfc3339("2026-04-29T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let row = IssueAnnouncementRow {
            title: "Lifecycle".to_string(),
            channel_id: "123".to_string(),
            message_id: "456".to_string(),
            created_at,
        };
        let event = IssueCompletionEvent {
            repo: "owner/repo".to_string(),
            issue_number: 1331,
            title: Some("Lifecycle".to_string()),
            kind: IssueCompletionKind::Merged,
            pr_number: Some(1410),
            pr_url: Some("https://github.com/owner/repo/pull/1410".to_string()),
        };

        let rendered = render_completed_card("Lifecycle", &row, &event);

        assert!(rendered.contains("✅ **#1331 완료** — Lifecycle"));
        assert!(rendered.contains("> 머지: PR #1410 https://github.com/owner/repo/pull/1410"));
    }
}
