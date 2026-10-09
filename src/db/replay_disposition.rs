//! Reads the request's durable replay disposition and its dispatch projection.
//! Unclassified starts, no-effect attempts without live permits and withheld requests block reruns.

use sqlx::PgPool;

/// Name every replay fence trigger raises under, so callers can tell a refusal from a DB fault.
#[cfg(test)]
pub(crate) const REPLAY_DISPOSITION_FENCE: &str = "replay_disposition_fence";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayDisposition {
    RegisteredNotStarted,
    StartedUnclassified,
    StartupFailedNoEffect,
    ClassifiedNormal,
    Withheld,
}

impl ReplayDisposition {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "registered_not_started" => Self::RegisteredNotStarted,
            "started_unclassified" => Self::StartedUnclassified,
            "startup_failed_no_effect" => Self::StartupFailedNoEffect,
            "classified_normal" => Self::ClassifiedNormal,
            "withheld" => Self::Withheld,
            _ => return None,
        })
    }

    /// Unclassified starts may already have effects; no-effect retries require a live permit.
    pub(crate) fn blocks_auto_rerun(self) -> bool {
        match self {
            Self::StartedUnclassified | Self::StartupFailedNoEffect | Self::Withheld => true,
            Self::RegisteredNotStarted | Self::ClassifiedNormal => false,
        }
    }
}

/// Whether a stored disposition blocks a rerun; an unrecognised spelling blocks too.
pub(crate) fn stored_disposition_blocks_rerun(value: Option<&str>) -> bool {
    value.is_some_and(|value| ReplayDisposition::parse(value).is_none_or(|d| d.blocks_auto_rerun()))
}

/// Whether the error is a replay fence refusal rather than a database fault.
#[cfg(test)]
pub(crate) fn is_replay_fence_refusal(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|error| error.constraint())
        == Some(REPLAY_DISPOSITION_FENCE)
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
#[path = "replay_disposition_tests.rs"]
pub(crate) mod tests;
