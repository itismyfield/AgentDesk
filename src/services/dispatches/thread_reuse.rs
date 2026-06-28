//! Dispatch thread-reuse persistence and lookup service.
//!
//! The Axum route layer owns request extraction and HTTP response mapping; this
//! module owns the Postgres reads/writes and the channel-selection rules that
//! decide how dispatch threads are persisted and looked up.

use serde_json::Value;
use sqlx::{PgPool, Row as SqlxRow};

use crate::db::agents::{resolve_agent_counter_model_channel_pg, resolve_agent_primary_channel_pg};
use crate::services::dispatches::discord_delivery::{
    get_mapped_thread_for_channel_pg, get_thread_for_channel_pg,
    set_thread_for_channel_map_only_pg, set_thread_for_channel_pg,
};
use crate::services::dispatches::outbox_route::{parse_channel_id, use_counter_model_channel};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkDispatchThreadInput {
    pub dispatch_id: String,
    pub thread_id: String,
    pub channel_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkDispatchThreadOutcome {
    pub card_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CardThreadLookupOutcome {
    pub card_id: String,
    pub card_title: String,
    pub github_issue_url: Option<String>,
    pub github_issue_number: Option<i64>,
    pub issue_body: Option<String>,
    pub deferred_dod: Option<Value>,
    pub active_thread_id: Option<String>,
    pub dispatch_type: Option<String>,
    pub dispatch_title: Option<String>,
    pub discord_channel_id: Option<String>,
    pub discord_channel_alt: Option<String>,
    pub discord_channel_target: Option<String>,
    pub dispatch_context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DispatchThreadReuseError {
    NotFound(&'static str),
    Internal(String),
}

impl std::fmt::Display for DispatchThreadReuseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(message) => formatter.write_str(message),
            Self::Internal(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for DispatchThreadReuseError {}

impl From<sqlx::Error> for DispatchThreadReuseError {
    fn from(error: sqlx::Error) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<String> for DispatchThreadReuseError {
    fn from(error: String) -> Self {
        Self::Internal(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkThreadPersistence {
    LegacyActiveThread,
    ChannelMapAndActive(u64),
    ChannelMapOnly(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardThreadLookupPlan {
    None,
    ChannelWithLegacyFallback(u64),
    ChannelMapOnly(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CardThreadChannelSelection {
    target_channel: Option<String>,
    lookup_plan: CardThreadLookupPlan,
}

pub(crate) async fn link_dispatch_thread_pg(
    pool: &PgPool,
    input: LinkDispatchThreadInput,
) -> Result<LinkDispatchThreadOutcome, DispatchThreadReuseError> {
    let dispatch_row = sqlx::query(
        "SELECT kanban_card_id, dispatch_type, to_agent_id
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(&input.dispatch_id)
    .fetch_optional(pool)
    .await?;

    let Some(row) = dispatch_row else {
        return Err(DispatchThreadReuseError::NotFound("dispatch not found"));
    };

    let card_id: String = read_required_string(&row, "kanban_card_id")?;
    let dispatch_type: Option<String> = row.try_get("dispatch_type").ok().flatten();
    let to_agent_id: String = read_required_string(&row, "to_agent_id")?;

    sqlx::query(
        "UPDATE task_dispatches
         SET thread_id = $1,
             updated_at = NOW()
         WHERE id = $2",
    )
    .bind(&input.thread_id)
    .bind(&input.dispatch_id)
    .execute(pool)
    .await?;

    let counter_model_channel = if input
        .channel_id
        .as_deref()
        .and_then(|value| value.parse::<u64>().ok())
        .is_some()
    {
        resolve_agent_counter_model_channel_pg(pool, &to_agent_id)
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    match classify_link_thread_persistence(
        input.channel_id.as_deref(),
        dispatch_type.as_deref(),
        counter_model_channel.as_deref(),
    ) {
        LinkThreadPersistence::LegacyActiveThread => {
            set_card_active_thread_pg(pool, &card_id, &input.thread_id).await?;
        }
        LinkThreadPersistence::ChannelMapAndActive(channel_id) => {
            set_thread_for_channel_pg(pool, &card_id, channel_id, &input.thread_id).await?;
        }
        LinkThreadPersistence::ChannelMapOnly(channel_id) => {
            set_thread_for_channel_map_only_pg(pool, &card_id, channel_id, &input.thread_id)
                .await?;
        }
    }

    Ok(LinkDispatchThreadOutcome { card_id })
}

pub(crate) async fn get_card_thread_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<CardThreadLookupOutcome, DispatchThreadReuseError> {
    let row = sqlx::query(
        "SELECT kc.id AS card_id,
                kc.title AS card_title,
                kc.github_issue_url AS github_issue_url,
                kc.github_issue_number::BIGINT AS github_issue_number,
                kc.description AS issue_body,
                kc.deferred_dod_json::text AS deferred_dod_json,
                td.dispatch_type AS dispatch_type,
                td.title AS dispatch_title,
                td.to_agent_id AS dispatch_agent_id,
                td.context AS dispatch_context
         FROM task_dispatches td
         JOIN kanban_cards kc ON kc.id = td.kanban_card_id
         WHERE td.id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Err(DispatchThreadReuseError::NotFound("dispatch not found"));
    };

    let card_id: String = read_required_string(&row, "card_id")?;
    let card_title: String = read_required_string(&row, "card_title")?;
    let github_issue_url: Option<String> = row.try_get("github_issue_url").ok().flatten();
    let github_issue_number: Option<i64> = row.try_get("github_issue_number").ok().flatten();
    let issue_body: Option<String> = row.try_get("issue_body").ok().flatten();
    let deferred_dod_json: Option<String> = row.try_get("deferred_dod_json").ok().flatten();
    let dispatch_type: Option<String> = row.try_get("dispatch_type").ok().flatten();
    let dispatch_title: Option<String> = row.try_get("dispatch_title").ok().flatten();
    let to_agent_id: String = read_required_string(&row, "dispatch_agent_id")?;
    let dispatch_context: Option<String> = row.try_get("dispatch_context").ok().flatten();

    let primary_channel = resolve_agent_primary_channel_pg(pool, &to_agent_id)
        .await
        .ok()
        .flatten();
    let counter_model_channel = resolve_agent_counter_model_channel_pg(pool, &to_agent_id)
        .await
        .ok()
        .flatten();
    let selection = select_card_thread_channel(
        dispatch_type.as_deref(),
        primary_channel.as_deref(),
        counter_model_channel.as_deref(),
    );
    let active_thread_id = match selection.lookup_plan {
        CardThreadLookupPlan::None => None,
        CardThreadLookupPlan::ChannelWithLegacyFallback(channel_id) => {
            get_thread_for_channel_pg(pool, &card_id, channel_id).await?
        }
        CardThreadLookupPlan::ChannelMapOnly(channel_id) => {
            get_mapped_thread_for_channel_pg(pool, &card_id, channel_id).await?
        }
    };
    let deferred_dod = deferred_dod_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());

    Ok(CardThreadLookupOutcome {
        card_id,
        card_title,
        github_issue_url,
        github_issue_number,
        issue_body,
        deferred_dod,
        active_thread_id,
        dispatch_type,
        dispatch_title,
        discord_channel_id: primary_channel,
        discord_channel_alt: counter_model_channel,
        discord_channel_target: selection.target_channel,
        dispatch_context,
    })
}

pub(crate) async fn get_pending_dispatch_for_thread_pg(
    pool: &PgPool,
    thread_id: &str,
) -> Result<Option<String>, DispatchThreadReuseError> {
    let dispatch_id = sqlx::query_scalar::<_, String>(
        "SELECT td.id
         FROM task_dispatches td
         JOIN kanban_cards kc ON kc.id = td.kanban_card_id
         WHERE td.status IN ('pending', 'dispatched')
           AND (
             td.thread_id = $1
             OR kc.active_thread_id = $1
             OR EXISTS(
               SELECT 1
               FROM jsonb_each_text(COALESCE(kc.channel_thread_map, '{}'::jsonb)) AS entry(key, value)
               WHERE entry.value = $1
             )
           )
         ORDER BY td.created_at DESC
         LIMIT 1",
    )
    .bind(thread_id)
    .fetch_optional(pool)
    .await?;

    Ok(dispatch_id)
}

fn classify_link_thread_persistence(
    channel_id: Option<&str>,
    dispatch_type: Option<&str>,
    counter_model_channel: Option<&str>,
) -> LinkThreadPersistence {
    let Some(channel_id) = channel_id else {
        return LinkThreadPersistence::LegacyActiveThread;
    };
    let Ok(channel_id) = channel_id.parse::<u64>() else {
        return LinkThreadPersistence::LegacyActiveThread;
    };

    let counter_model_channel = counter_model_channel.and_then(parse_channel_id);
    if use_counter_model_channel(dispatch_type) && counter_model_channel == Some(channel_id) {
        LinkThreadPersistence::ChannelMapOnly(channel_id)
    } else {
        LinkThreadPersistence::ChannelMapAndActive(channel_id)
    }
}

fn select_card_thread_channel(
    dispatch_type: Option<&str>,
    primary_channel: Option<&str>,
    counter_model_channel: Option<&str>,
) -> CardThreadChannelSelection {
    let use_counter_model = use_counter_model_channel(dispatch_type);
    let target_channel = if use_counter_model {
        counter_model_channel
    } else {
        primary_channel
    };
    let lookup_plan = match (use_counter_model, target_channel.and_then(parse_channel_id)) {
        (_, None) => CardThreadLookupPlan::None,
        (true, Some(channel_id)) => CardThreadLookupPlan::ChannelMapOnly(channel_id),
        (false, Some(channel_id)) => CardThreadLookupPlan::ChannelWithLegacyFallback(channel_id),
    };

    CardThreadChannelSelection {
        target_channel: target_channel.map(str::to_string),
        lookup_plan,
    }
}

async fn set_card_active_thread_pg(
    pool: &PgPool,
    card_id: &str,
    thread_id: &str,
) -> Result<(), DispatchThreadReuseError> {
    sqlx::query(
        "UPDATE kanban_cards
         SET active_thread_id = $1,
             updated_at = NOW()
         WHERE id = $2",
    )
    .bind(thread_id)
    .bind(card_id)
    .execute(pool)
    .await?;
    Ok(())
}

fn read_required_string(
    row: &sqlx::postgres::PgRow,
    column: &'static str,
) -> Result<String, DispatchThreadReuseError> {
    row.try_get::<String, _>(column).map_err(|error| {
        tracing::warn!(%error, column, "[dispatch] failed to read required thread-reuse row column");
        DispatchThreadReuseError::Internal(error.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_counter_model_dispatch_on_counter_channel_updates_map_only() {
        assert_eq!(
            classify_link_thread_persistence(Some("123"), Some("review"), Some("123"),),
            LinkThreadPersistence::ChannelMapOnly(123)
        );
    }

    #[test]
    fn link_primary_or_non_counter_channel_updates_map_and_active_thread() {
        assert_eq!(
            classify_link_thread_persistence(Some("456"), Some("review"), Some("123"),),
            LinkThreadPersistence::ChannelMapAndActive(456)
        );
        assert_eq!(
            classify_link_thread_persistence(Some("123"), Some("implementation"), Some("123"),),
            LinkThreadPersistence::ChannelMapAndActive(123)
        );
    }

    #[test]
    fn link_missing_or_non_numeric_channel_preserves_legacy_active_thread_write() {
        assert_eq!(
            classify_link_thread_persistence(None, Some("review"), Some("123"),),
            LinkThreadPersistence::LegacyActiveThread
        );
        assert_eq!(
            classify_link_thread_persistence(Some("counter-model"), Some("review"), Some("123"),),
            LinkThreadPersistence::LegacyActiveThread
        );
    }

    #[test]
    fn card_thread_lookup_uses_counter_map_without_legacy_fallback() {
        assert_eq!(
            select_card_thread_channel(Some("plan-review"), Some("111"), Some("222"),),
            CardThreadChannelSelection {
                target_channel: Some("222".to_string()),
                lookup_plan: CardThreadLookupPlan::ChannelMapOnly(222),
            }
        );
    }

    #[test]
    fn card_thread_lookup_uses_primary_channel_with_legacy_fallback() {
        assert_eq!(
            select_card_thread_channel(Some("review-decision"), Some("111"), Some("222"),),
            CardThreadChannelSelection {
                target_channel: Some("111".to_string()),
                lookup_plan: CardThreadLookupPlan::ChannelWithLegacyFallback(111),
            }
        );
    }

    #[test]
    fn card_thread_lookup_preserves_unparseable_target_in_outcome() {
        assert_eq!(
            select_card_thread_channel(Some("review"), Some("111"), Some("not-a-channel"),),
            CardThreadChannelSelection {
                target_channel: Some("not-a-channel".to_string()),
                lookup_plan: CardThreadLookupPlan::None,
            }
        );
    }
}
