use std::{future::Future, time::Duration};

use sqlx::PgPool;
use sqlx::postgres::PgRow;

use super::{JsonColumnMode, pg_row_to_json, policy_db_error_json, run_async_bridge_pg};
use crate::utils::async_bridge::{PgWorkError, block_on_pg_result_before};

pub(super) fn run_pg<F, T>(
    pool: &PgPool,
    reserve: Duration,
    future_factory: impl FnOnce(PgPool) -> F + Send + 'static,
) -> Result<T, PgWorkError>
where
    F: Future<Output = Result<T, String>> + Send + 'static,
    T: Send + 'static,
{
    if reserve.is_zero() {
        run_async_bridge_pg(pool, future_factory).map_err(PgWorkError::Other)
    } else {
        block_on_pg_result_before(pool, reserve, future_factory)
    }
}

pub(super) fn work_error_json(operation: &str, sql: &str, error: PgWorkError) -> String {
    match error {
        PgWorkError::Deadline => serde_json::json!({
            "error": "postgres bridge work deadline exceeded",
            "deadline_exceeded": true,
        })
        .to_string(),
        PgWorkError::Other(error) => policy_db_error_json(operation, sql, error),
    }
}

pub(super) fn encode_query_rows(
    rows: &[PgRow],
    mode: JsonColumnMode,
) -> Result<String, (&'static str, String)> {
    let values = rows
        .iter()
        .map(|row| pg_row_to_json(row, mode))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ("collect_rows_pg", error))?;
    serde_json::to_string(&values)
        .map_err(|error| ("serialize_pg", format!("serialize query result: {error}")))
}
