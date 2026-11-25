//! The one choice of a request. A click is checked and stored under the request's row lock,
//! with its selection event, before anything is delivered; the request then takes no other.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use super::AttemptState;

/// The interface of a starter whose buttons take a selection.
pub(crate) const ACTIVE_BUTTONS: i16 = 2;

/// A button click as the gateway delivered it: the handle, the post it sits on and its bot.
pub(crate) struct Click<'a> {
    pub handle: &'a str,
    pub thread_id: u64,
    pub message_id: u64,
    pub bot_user_id: u64,
    pub actor: u64,
}

/// A stored selection with what its one delivery and its card need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Selection {
    pub request_id: Uuid,
    pub event_id: Uuid,
    pub thread_id: u64,
    pub logical_channel: u64,
    pub provider: String,
    pub label: String,
    /// The input the selection event approved, byte for byte.
    pub text: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Chosen {
    /// This click stored the request's choice; its event waits unstarted.
    First(Selection),
    /// The request already holds a choice: its label, and whether this click named the same one.
    Already { label: String, same: bool },
    /// The request takes no selection: its buttons were never enabled, or it is closed.
    NotOpen,
}

/// The input a selection delivers to the requester's session.
fn input_text(
    (request_ref, request_id, title): (&str, Uuid, &str),
    (label, percent): (&str, Option<i16>),
) -> String {
    let recommended = percent.map_or("추천 확인 불가".to_string(), |percent| {
        format!("Gemini 추천 {percent}%")
    });
    format!(
        "[사용자 결정 ref={request_ref} decision={request_id}] {title} → 선택: {label} ({recommended})"
    )
}

/// Stores the click as the request's choice unless it already has one. The request row lock
/// orders every selection of a request, so the first commit wins; `None` is a click on no post
/// of ours. Whether the requester's session is alive is not asked here.
pub(crate) async fn choose_once(
    pool: &PgPool,
    click: &Click<'_>,
) -> Result<Option<Chosen>, sqlx::Error> {
    type Row = (
        Uuid,
        String,
        i16,
        String,
        String,
        Option<String>,
        String,
        String,
        String,
        Option<i16>,
    );
    let mut tx = pool.begin().await?;
    let found: Option<Row> = sqlx::query_as(
        "SELECT r.request_id, r.lifecycle, o.ordinal, r.requester_agent_id, r.logical_channel_id,
                r.provider, r.request_ref, r.title, o.label, o.recommendation_percent
         FROM decision_options o JOIN decision_requests r USING (request_id)
         WHERE o.handle = $1 AND r.thread_id = $2 AND r.starter_message_id = $3
           AND r.bot_user_id = $4
         FOR UPDATE OF r",
    )
    .bind(click.handle)
    .bind(click.thread_id.to_string())
    .bind(click.message_id.to_string())
    .bind(click.bot_user_id.to_string())
    .fetch_optional(&mut *tx)
    .await?;
    let Some((
        request_id,
        lifecycle,
        ordinal,
        agent,
        channel,
        provider,
        request_ref,
        title,
        label,
        percent,
    )) = found
    else {
        return Ok(None);
    };
    let held: Option<(i16, String)> = sqlx::query_as(
        "SELECT c.ordinal, o.label FROM decision_choices c
         JOIN decision_options o ON o.request_id = c.request_id AND o.ordinal = c.ordinal
         WHERE c.request_id = $1",
    )
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((first, label)) = held {
        let same = first == ordinal;
        return Ok(Some(Chosen::Already { label, same }));
    }
    let (Some(provider), Ok(logical_channel)) = (provider, channel.parse::<u64>()) else {
        return Ok(Some(Chosen::NotOpen));
    };
    if lifecycle != "open" {
        return Ok(Some(Chosen::NotOpen));
    }
    let event_id = Uuid::new_v4();
    let text = input_text((&request_ref, request_id, &title), (&label, percent));
    sqlx::query(
        "INSERT INTO decision_input_events (event_id, kind, source_key, origin,
             requester_agent_id, logical_channel_id, actor_user_id, text_sha256)
         VALUES ($1, 'selection', $2, $3, $4, $5, $6, $7)",
    )
    .bind(event_id)
    .bind(request_id.to_string())
    .bind(format!("decision:{event_id}"))
    .bind(&agent)
    .bind(&channel)
    .bind(click.actor.to_string())
    .bind(hex::encode(Sha256::digest(text.as_bytes())))
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO decision_delivery_attempts (event_id) VALUES ($1)")
        .bind(event_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO decision_choices (request_id, ordinal, actor_user_id, source,
             selection_event_id)
         VALUES ($1, $2, $3, 'button', $4)",
    )
    .bind(request_id)
    .bind(ordinal)
    .bind(click.actor.to_string())
    .bind(event_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE decision_requests SET lifecycle = 'selecting' WHERE request_id = $1")
        .bind(request_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(Chosen::First(Selection {
        request_id,
        event_id,
        thread_id: click.thread_id,
        logical_channel,
        provider,
        label,
        text,
    })))
}

/// Whether the request targets its agent's own channel; a thread target is not wired yet.
pub(crate) async fn targets_main_channel(
    pool: &PgPool,
    request_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT logical_channel_id = target_channel_id FROM decision_requests
         WHERE request_id = $1",
    )
    .bind(request_id)
    .fetch_one(pool)
    .await
}

/// Opens a posted request to selection, once; `reason` instead records why its buttons stay
/// disabled. `true` when the request is open after the call.
pub(crate) async fn open(
    pool: &PgPool,
    request_id: Uuid,
    reason: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let moved = sqlx::query(
        "UPDATE decision_requests
         SET lifecycle = CASE WHEN $2::TEXT IS NULL THEN 'open' ELSE lifecycle END,
             interface_version = CASE WHEN $2::TEXT IS NULL THEN $3 ELSE interface_version END,
             inactive_reason = $2
         WHERE request_id = $1 AND lifecycle = 'disabled'",
    )
    .bind(request_id)
    .bind(reason)
    .bind(ACTIVE_BUTTONS)
    .execute(pool)
    .await?;
    Ok(moved.rows_affected() == 1 && reason.is_none())
}

/// The starter's buttons in option order: label, handle, and whether it is the stored choice.
pub(crate) async fn buttons(
    pool: &PgPool,
    request_id: Uuid,
) -> Result<Vec<(String, String, bool)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT o.label, COALESCE(o.handle, ''), c.ordinal IS NOT NULL
         FROM decision_options o
         LEFT JOIN decision_choices c ON c.request_id = o.request_id AND c.ordinal = o.ordinal
         WHERE o.request_id = $1 ORDER BY o.ordinal",
    )
    .bind(request_id)
    .fetch_all(pool)
    .await
}

/// A selection whose delivery reached its end, with the end its card shows.
#[derive(Clone, Debug)]
pub(crate) struct Ended {
    pub selection: Selection,
    pub state: AttemptState,
    pub detail: Option<String>,
}

type Stored = (
    Uuid,
    Uuid,
    Option<String>,
    String,
    Option<String>,
    String,
    String,
    String,
    Option<i16>,
    String,
    Option<String>,
);

/// The stored selections whose attempt is in one of `states` and whose thread is not sealed.
async fn stored(pool: &PgPool, states: &[&str]) -> Result<Vec<Ended>, sqlx::Error> {
    let rows: Vec<Stored> = sqlx::query_as(
        "SELECT r.request_id, c.selection_event_id, r.thread_id, r.logical_channel_id, r.provider,
                r.request_ref, r.title, o.label, o.recommendation_percent, a.state, a.detail
         FROM decision_choices c
         JOIN decision_requests r USING (request_id)
         JOIN decision_options o ON o.request_id = c.request_id AND o.ordinal = c.ordinal
         JOIN decision_delivery_attempts a ON a.event_id = c.selection_event_id
         WHERE a.state = ANY($1) AND r.sealed_at IS NULL
           AND r.lifecycle IN ('selecting', 'closed')
         ORDER BY c.chosen_at",
    )
    .bind(states)
    .fetch_all(pool)
    .await?;
    let read = |row: Stored| {
        let (request_id, event_id, thread, channel, provider, request_ref, title, label, percent) = (
            row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8,
        );
        let text = input_text((&request_ref, request_id, &title), (&label, percent));
        let selection = Selection {
            request_id,
            event_id,
            thread_id: thread?.parse().ok()?,
            logical_channel: channel.parse().ok()?,
            provider: provider?,
            label,
            text,
        };
        let (state, detail) = (AttemptState::parse(&row.9)?, row.10);
        Some(Ended {
            selection,
            state,
            detail,
        })
    };
    Ok(rows.into_iter().filter_map(read).collect())
}

/// The stored selections no attempt has started: a restart between the store and the start
/// leaves one, and it is started once.
pub(crate) async fn unstarted(pool: &PgPool) -> Result<Vec<Selection>, sqlx::Error> {
    let found = stored(pool, &["not_started"]).await?;
    Ok(found.into_iter().map(|ended| ended.selection).collect())
}

/// The selections whose delivery ended and whose thread is not archived and locked yet.
pub(crate) async fn ended(pool: &PgPool) -> Result<Vec<Ended>, sqlx::Error> {
    stored(pool, &["delivered", "failed", "unknown_closed"]).await
}

/// Whether a stored selection is still being delivered; only then are attempts settled here.
pub(crate) async fn delivering(pool: &PgPool) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM decision_requests WHERE lifecycle = 'selecting')",
    )
    .fetch_one(pool)
    .await
}

/// Closes the request once its selection's delivery ended; `sealed` then records that its
/// thread was archived and locked, or the refusal that keeps it from being so.
pub(crate) async fn close(
    pool: &PgPool,
    request_id: Uuid,
    sealed: Option<Option<&str>>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE decision_requests
         SET lifecycle = 'closed',
             sealed_at = CASE WHEN $2 THEN NOW() ELSE sealed_at END,
             moderation_error = CASE WHEN $2 THEN $3 ELSE moderation_error END
         WHERE request_id = $1 AND lifecycle IN ('selecting', 'closed')",
    )
    .bind(request_id)
    .bind(sealed.is_some())
    .bind(sealed.flatten())
    .execute(pool)
    .await
    .map(drop)
}
