//! Channel footer preferences shared by the Discord gateway and execution writer.

use sqlx::PgPool;

fn key(channel: u64) -> String {
    format!("discord_usage_footer:{channel}")
}

pub(crate) async fn enabled(pool: Option<&PgPool>, channel: u64) -> bool {
    let Some(pool) = pool else { return true };
    match sqlx::query_scalar::<_, bool>("SELECT value::boolean FROM kv_meta WHERE key = $1")
        .bind(key(channel))
        .fetch_optional(pool)
        .await
    {
        Ok(saved) => saved.unwrap_or(true),
        Err(error) => {
            tracing::warn!(channel, %error, "usage footer preference read failed");
            false
        }
    }
}

pub(in crate::services::discord) async fn update(
    pool: Option<&PgPool>,
    channel: u64,
    requested: Option<bool>,
) -> Result<bool, String> {
    let pool = pool.ok_or("shared database unavailable")?;
    // The conflict update locks the existing row before reading its value for a toggle.
    sqlx::query_scalar(
        "INSERT INTO kv_meta (key, value) VALUES ($1, COALESCE($2, FALSE)::text)
         ON CONFLICT (key) DO UPDATE SET value = COALESCE($2, NOT kv_meta.value::boolean)::text
         RETURNING value::boolean",
    )
    .bind(key(channel))
    .bind(requested)
    .fetch_one(pool)
    .await
    .map_err(|error| error.to_string())
}
