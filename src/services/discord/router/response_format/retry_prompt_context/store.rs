pub(in crate::services::discord) fn peek(pool: Option<&sqlx::PgPool>, key: &str) -> Option<String> {
    let Some(pool) = pool else {
        return crate::services::discord::internal_api::get_kv_value(key)
            .ok()
            .flatten();
    };
    let key = key.to_owned();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |pool| async move {
            sqlx::query_scalar::<_, String>(
                "SELECT value FROM kv_meta WHERE key = $1
                 AND (expires_at IS NULL OR expires_at > NOW())",
            )
            .bind(key)
            .fetch_optional(&pool)
            .await
            .map_err(|error| error.to_string())
        },
        |error| error,
    )
    .ok()
    .flatten()
}
