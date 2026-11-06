use std::io;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, Row};

// Include response drain and exceed relay_dead_letter's 3s statement timeout
// and ownership execution's 2s lock timeout before discarding.
pub(crate) const RETURN_CLEANUP_BUDGET: Duration = Duration::from_secs(5);
const CLEANUP: &str = "SELECT transaction_timestamp() <> statement_timestamp(); BEGIN; ROLLBACK;";

pub(crate) fn with_return_cleanup(options: PgPoolOptions, budget: Duration) -> PgPoolOptions {
    options.after_release(move |conn, meta| {
        Box::pin(async move {
            let result = if conn.is_in_transaction() {
                Err(sqlx::Error::Protocol(
                    "returned connection retained transaction depth".into(),
                ))
            } else {
                match tokio::time::timeout(budget, async {
                    // Consume the entire batch through ReadyForQuery before allowing reuse.
                    let rows = sqlx::raw_sql(CLEANUP).fetch_all(&mut *conn).await?;
                    rows.first()
                        .ok_or_else(|| sqlx::Error::Protocol("cleanup returned no state".into()))?
                        .try_get::<bool, _>(0)
                })
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(sqlx::Error::Io(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "pool return cleanup exceeded its budget",
                    ))),
                }
            };
            match result {
                Ok(carried_transaction) => {
                    if carried_transaction {
                        tracing::warn!(age = ?meta.age,
                            "returned connection carried an open transaction; rolled back");
                    }
                    Ok(true)
                }
                Err(error) => {
                    let sqlstate = error.as_database_error().and_then(|error| error.code());
                    tracing::warn!(%error, sqlstate = sqlstate.as_deref(), age = ?meta.age,
                        "discarding returned connection after cleanup failure");
                    Err(error)
                }
            }
        })
    })
}
