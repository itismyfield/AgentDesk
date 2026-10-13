//! Channel recovery custody rows: every write is one statement conditioned on the expected
//! revision and format; nothing deletes a row or takes one over by age.

use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};

const COLUMNS: &str = "channel_id, revision, provider, token_hash, owner_node, \
     process_incarnation, attempt_id, home_epoch, activation_generation, issued_episode, phase, \
     active_operation, canonical_seal, ever_scoped, format_version";

/// A write sets `ever_scoped` only by OR-ing in these phases, so no transition clears it.
const SCOPED_PHASES: &str = "('scoped_reserved', 'installing', 'installed')";

/// A row as stored. The typed meaning, including the held reading of unknown shapes, belongs to
/// the recovery custody model.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StoredCustody {
    pub(crate) channel_id: String,
    pub(crate) revision: i64,
    pub(crate) provider: String,
    pub(crate) token_hash: String,
    pub(crate) owner_node: String,
    pub(crate) process_incarnation: String,
    pub(crate) attempt_id: String,
    pub(crate) home_epoch: Option<i64>,
    pub(crate) activation_generation: Option<i64>,
    pub(crate) issued_episode: Option<serde_json::Value>,
    pub(crate) phase: String,
    pub(crate) active_operation: Option<serde_json::Value>,
    pub(crate) canonical_seal: serde_json::Value,
    pub(crate) ever_scoped: bool,
    pub(crate) format_version: i32,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CasWrite {
    Applied {
        revision: i64,
    },
    /// The row was not at the expected revision or format; nothing changed.
    Stale,
}

fn decode(row: &PgRow) -> Result<StoredCustody, sqlx::Error> {
    Ok(StoredCustody {
        channel_id: row.try_get("channel_id")?,
        revision: row.try_get("revision")?,
        provider: row.try_get("provider")?,
        token_hash: row.try_get("token_hash")?,
        owner_node: row.try_get("owner_node")?,
        process_incarnation: row.try_get("process_incarnation")?,
        attempt_id: row.try_get("attempt_id")?,
        home_epoch: row.try_get("home_epoch")?,
        activation_generation: row.try_get("activation_generation")?,
        issued_episode: row.try_get("issued_episode")?,
        phase: row.try_get("phase")?,
        active_operation: row.try_get("active_operation")?,
        canonical_seal: row.try_get("canonical_seal")?,
        ever_scoped: row.try_get("ever_scoped")?,
        format_version: row.try_get("format_version")?,
    })
}

fn written(row: Option<PgRow>) -> Result<CasWrite, sqlx::Error> {
    Ok(match row {
        Some(row) => CasWrite::Applied {
            revision: row.try_get("revision")?,
        },
        None => CasWrite::Stale,
    })
}

/// The channel's row; `None` only when no row exists. A failed query is an error, never "none".
pub(crate) async fn read(
    conn: &mut PgConnection,
    channel_id: &str,
) -> Result<Option<StoredCustody>, sqlx::Error> {
    let row = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM channel_recovery_custody WHERE channel_id = $1"
    ))
    .bind(channel_id)
    .fetch_optional(conn)
    .await?;
    row.as_ref().map(decode).transpose()
}

/// The channel's first row at revision 1; `Stale` when any row already exists.
pub(crate) async fn insert(
    conn: &mut PgConnection,
    row: &StoredCustody,
) -> Result<CasWrite, sqlx::Error> {
    let inserted = sqlx::query(&format!(
        "INSERT INTO channel_recovery_custody ({COLUMNS})
         VALUES ($1, 1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                 $10 IN {SCOPED_PHASES}, $13)
         ON CONFLICT (channel_id) DO NOTHING
         RETURNING revision"
    ))
    .bind(&row.channel_id)
    .bind(&row.provider)
    .bind(&row.token_hash)
    .bind(&row.owner_node)
    .bind(&row.process_incarnation)
    .bind(&row.attempt_id)
    .bind(row.home_epoch)
    .bind(row.activation_generation)
    .bind(&row.issued_episode)
    .bind(&row.phase)
    .bind(&row.active_operation)
    .bind(&row.canonical_seal)
    .bind(row.format_version)
    .fetch_optional(conn)
    .await?;
    written(inserted)
}

/// Replaces the row held at `expected_revision` in this build's format and bumps the revision.
pub(crate) async fn replace(
    conn: &mut PgConnection,
    expected_revision: i64,
    row: &StoredCustody,
) -> Result<CasWrite, sqlx::Error> {
    let updated = sqlx::query(&format!(
        "UPDATE channel_recovery_custody
            SET revision = revision + 1, provider = $3, token_hash = $4, owner_node = $5,
                process_incarnation = $6, attempt_id = $7, home_epoch = $8,
                activation_generation = $9, issued_episode = $10, phase = $11,
                active_operation = $12, canonical_seal = $13,
                ever_scoped = ever_scoped OR $11 IN {SCOPED_PHASES}, updated_at = NOW()
          WHERE channel_id = $1 AND revision = $2 AND format_version = $14
          RETURNING revision"
    ))
    .bind(&row.channel_id)
    .bind(expected_revision)
    .bind(&row.provider)
    .bind(&row.token_hash)
    .bind(&row.owner_node)
    .bind(&row.process_incarnation)
    .bind(&row.attempt_id)
    .bind(row.home_epoch)
    .bind(row.activation_generation)
    .bind(&row.issued_episode)
    .bind(&row.phase)
    .bind(&row.active_operation)
    .bind(&row.canonical_seal)
    .bind(row.format_version)
    .fetch_optional(conn)
    .await?;
    written(updated)
}
