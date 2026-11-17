//! Decision forum cache and create journal, one row per (guild, category).

use sqlx::PgPool;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ForumState {
    Ready(String),
    /// Committed before the single create request; the forum may appear later.
    CreateMayHaveSent,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ForumRow {
    pub forum_name: String,
    pub state: ForumState,
}

pub(crate) async fn load(
    pool: &PgPool,
    guild_id: u64,
    category_id: u64,
) -> Result<Option<ForumRow>, sqlx::Error> {
    let row: Option<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT forum_name, state, forum_id, error FROM decision_forums
         WHERE guild_id = $1 AND category_id = $2",
    )
    .bind(guild_id.to_string())
    .bind(category_id.to_string())
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(forum_name, state, forum_id, error)| ForumRow {
        forum_name,
        state: match (state.as_str(), forum_id) {
            ("ready", Some(id)) => ForumState::Ready(id),
            ("create_may_have_sent", _) => ForumState::CreateMayHaveSent,
            _ => ForumState::Failed(error.unwrap_or_default()),
        },
    }))
}

/// A forum belongs to one category: a ready row takes the forum ID from any older row.
pub(crate) async fn store(
    pool: &PgPool,
    guild_id: u64,
    category_id: u64,
    forum_name: &str,
    state: &ForumState,
) -> Result<(), sqlx::Error> {
    let (state, forum_id, error) = match state {
        ForumState::Ready(id) => ("ready", Some(id.as_str()), None),
        ForumState::CreateMayHaveSent => ("create_may_have_sent", None, None),
        ForumState::Failed(error) => ("failed", None, Some(error.as_str())),
    };
    let (guild_id, category_id) = (guild_id.to_string(), category_id.to_string());
    let mut tx = pool.begin().await?;
    sqlx::query(
        "DELETE FROM decision_forums WHERE forum_id = $1 AND (guild_id, category_id) <> ($2, $3)",
    )
    .bind(forum_id)
    .bind(&guild_id)
    .bind(&category_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO decision_forums (guild_id, category_id, forum_name, state, forum_id, error)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (guild_id, category_id) DO UPDATE SET forum_name = EXCLUDED.forum_name,
             state = EXCLUDED.state, forum_id = EXCLUDED.forum_id, error = EXCLUDED.error,
             updated_at = NOW()",
    )
    .bind(&guild_id)
    .bind(&category_id)
    .bind(forum_name)
    .bind(state)
    .bind(forum_id)
    .bind(error)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}
