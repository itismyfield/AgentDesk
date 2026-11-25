//! Where a request's starter post stands. The journal is one compare-and-set from `stored`, so a
//! single run ever holds the right to POST, and nothing here turns `posting` back into a POST.

use sqlx::PgPool;
use uuid::Uuid;

/// The starter post as PG knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Publication {
    /// Nothing was sent.
    Stored,
    /// The single POST may have gone out under this forum and bot; only finding it settles it.
    Posting { forum_id: u64, bot_user_id: u64 },
    /// The post exists with its buttons disabled.
    Disabled { thread_id: u64 },
    /// Discord refused the post; it is not sent again.
    PostFailed(String),
}

/// The interface the starter is rendered with: buttons disabled, no selection taken.
pub(crate) const DISABLED_BUTTONS: i16 = 1;

pub(crate) async fn load(pool: &PgPool, request_id: Uuid) -> Result<Publication, sqlx::Error> {
    type Row = (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let (lifecycle, forum, bot, thread, error): Row = sqlx::query_as(
        "SELECT lifecycle, forum_id, bot_user_id, thread_id, post_error FROM decision_requests
         WHERE request_id = $1",
    )
    .bind(request_id)
    .fetch_one(pool)
    .await?;
    let id = |text: Option<String>| text.and_then(|text| text.parse::<u64>().ok()).unwrap_or(0);
    Ok(match lifecycle.as_str() {
        "posting" => Publication::Posting {
            forum_id: id(forum),
            bot_user_id: id(bot),
        },
        "disabled" => Publication::Disabled {
            thread_id: id(thread),
        },
        "post_failed" => Publication::PostFailed(error.unwrap_or_default()),
        _ => Publication::Stored,
    })
}

/// Who posts, and where, as the journal records it before the POST.
pub(crate) struct Journal<'a> {
    pub provider: &'a str,
    pub bot_user_id: u64,
    pub forum_id: u64,
    /// One opaque handle per option, in option order.
    pub handles: &'a [String],
}

/// Moves `stored` to `posting` with the options' handles in one transaction. `false` when the
/// request is no longer `stored`: another run holds or held the POST.
pub(crate) async fn journal(
    pool: &PgPool,
    request_id: Uuid,
    journal: &Journal<'_>,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let moved = sqlx::query(
        "UPDATE decision_requests SET lifecycle = 'posting', provider = $2, bot_user_id = $3,
             forum_id = $4, interface_version = $5
         WHERE request_id = $1 AND lifecycle = 'stored'",
    )
    .bind(request_id)
    .bind(journal.provider)
    .bind(journal.bot_user_id.to_string())
    .bind(journal.forum_id.to_string())
    .bind(DISABLED_BUTTONS)
    .execute(&mut *tx)
    .await?;
    if moved.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(false);
    }
    for (index, handle) in journal.handles.iter().enumerate() {
        sqlx::query(
            "UPDATE decision_options SET handle = $3 WHERE request_id = $1 AND ordinal = $2",
        )
        .bind(request_id)
        .bind(index as i16 + 1)
        .bind(handle)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// The post exists: a forum post's thread and starter message share one id.
pub(crate) async fn posted(
    pool: &PgPool,
    request_id: Uuid,
    thread_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE decision_requests SET lifecycle = 'disabled', thread_id = $2,
             starter_message_id = $2
         WHERE request_id = $1 AND lifecycle = 'posting'",
    )
    .bind(request_id)
    .bind(thread_id.to_string())
    .execute(pool)
    .await
    .map(drop)
}

/// Discord refused the post for good.
pub(crate) async fn refused(
    pool: &PgPool,
    request_id: Uuid,
    error: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE decision_requests SET lifecycle = 'post_failed', post_error = $2
         WHERE request_id = $1 AND lifecycle = 'posting'",
    )
    .bind(request_id)
    .bind(error)
    .execute(pool)
    .await
    .map(drop)
}
