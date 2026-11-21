//! The current primary key of the row an operator-supplied session key names.

use sqlx::PgPool;

/// A key the row kept as an alias resolves to its primary; a key naming no row is returned as
/// asked so the caller's own not-found answer stands. Diverging evidence is an error.
pub(crate) async fn current_session_key_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<String, String> {
    crate::db::dispatched_session_canonical_identity::resolve_session_key_pg(pool, session_key)
        .await
        .map(|current| current.unwrap_or_else(|| session_key.to_string()))
        .map_err(|error| format!("resolve session locator: {error:?}"))
}
