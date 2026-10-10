//! Reads the request's durable replay disposition and its dispatch projection.
//! Unclassified starts, no-effect attempts without live permits and withheld requests block reruns.

use sqlx::PgPool;

/// Whether a stored disposition blocks a rerun; an unrecognised spelling blocks too.
pub(crate) fn stored_disposition_blocks_rerun(value: Option<&str>) -> bool {
    value.is_some_and(|value| !matches!(value, "registered_not_started" | "classified_normal"))
}

/// The disposition of one receipt; `Ok(None)` when the row or its disposition is absent.
pub(crate) async fn receipt_disposition(
    pool: &PgPool,
    receipt_id: i64,
) -> Result<Option<String>, sqlx::Error> {
    let disposition: Option<Option<String>> =
        sqlx::query_scalar("SELECT replay_disposition FROM intake_outbox WHERE id = $1")
            .bind(receipt_id)
            .fetch_optional(pool)
            .await?;
    Ok(disposition.flatten())
}

/// The `sources` that belong to no started request of `provider`, in input order.
pub(crate) async fn unblocked_sources(
    pool: &PgPool,
    provider: &str,
    channel: &str,
    sources: &[String],
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT source FROM unnest($3::TEXT[]) WITH ORDINALITY AS s(source, n)
          WHERE NOT replay_sources_blocked($1, $2, ARRAY[source])
          ORDER BY n",
    )
    .bind(provider)
    .bind(channel)
    .bind(sources)
    .fetch_all(pool)
    .await
}

pub(crate) async fn blocked_receipt_for_sources(
    pool: &PgPool,
    provider: &str,
    channel: &str,
    sources: &[String],
) -> Result<Option<(i64, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, replay_disposition FROM intake_outbox
          WHERE lower(btrim(provider)) = lower(btrim($1)) AND channel_id = $2
            AND replay_source_message_ids && $3::TEXT[]
            AND replay_disposition_blocks_rerun(replay_disposition)
          ORDER BY id LIMIT 1",
    )
    .bind(provider)
    .bind(channel)
    .bind(sources)
    .fetch_optional(pool)
    .await
}

/// Whether the dispatch's projected disposition forbids an automatic rerun.
pub(crate) async fn dispatch_blocked_on_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT replay_dispatch_blocked($1)")
        .bind(dispatch_id)
        .fetch_one(&mut **tx)
        .await
}

#[cfg(test)]
pub(crate) mod receipt;
#[cfg(test)]
pub(crate) mod write;
#[cfg(test)]
mod write_tests;

#[cfg(test)]
#[path = "replay_disposition_tests.rs"]
pub(crate) mod tests;
