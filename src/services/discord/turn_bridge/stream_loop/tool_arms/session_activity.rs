//! A tool frame's session-row activity: the tool-use mark and a background child's registration.
//! A reader that released its session row records neither; its frame bookkeeping goes on.

use sqlx::PgPool;

use crate::db::session_observability::{
    BackgroundChildSpawn, insert_background_child_pg, mark_session_tool_use_pg,
};

pub(super) async fn mark_tool_use(
    pool: &PgPool,
    session_key: &str,
    released: bool,
) -> Result<bool, sqlx::Error> {
    if released {
        return Ok(false);
    }
    mark_session_tool_use_pg(pool, session_key).await
}

pub(super) async fn register_background_child(
    pool: &PgPool,
    spawn: &BackgroundChildSpawn,
    released: bool,
) -> Result<Option<i64>, sqlx::Error> {
    if released {
        return Ok(None);
    }
    insert_background_child_pg(pool, spawn).await
}
