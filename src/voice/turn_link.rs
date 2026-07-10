//! VoiceTurnLink durable store (#2362 / #2164 Voice A).
//!
//! Canonical bridge between a voice channel and the background text channel
//! that owns a routed voice turn. Survives process restarts and powers
//! reverse lookups for final TTS playback target resolution (#2164 C6),
//! barge-in cancel routing (#2164 C7), and agent:done feedback routing
//! (#2164 C8).
//!
//! The lifecycle is intentionally narrow:
//!
//!   * [`upsert_active_voice_turn_link_pg`] — create or advance the active
//!     durable link, attaching identifiers learned after initial dispatch.
//!   * [`retarget_voice_turn_link_pg`] — atomic "cancel previous generation,
//!     insert new active generation" used by the upsert path.
//!   * `lookup_active_voice_turn_link_by_*` — active-only reverse lookups for
//!     call sites that know a dispatch, announce message, turn, or utterance.
//!   * [`attach_voice_turn_link_ids_pg`] — fill announce/dispatch/turn
//!     identifiers when downstream models learn them after link creation.
//!   * [`mark_terminal_voice_turn_link_pg`] — flip status when the routed
//!     turn completes (TTS done, run_completed, etc.).
//!   * [`gc_terminal_voice_turn_links_pg`] — leader-only maintenance sweep
//!     for old terminal rows. Active and cancelled rows are intentionally
//!     left in place to preserve durable history for long-lived background
//!     turns (24h+ runs are normal).
//!
//! The live call sites are wired through voice background handoff, barge-in,
//! final playback resolution, and leader maintenance.

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

/// Status values stored in `voice_turn_link.status`. Mirrors the SQL
/// `CHECK (status IN ('active', 'cancelled', 'terminal'))` constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceTurnLinkStatus {
    Active,
    Cancelled,
    Terminal,
}

impl VoiceTurnLinkStatus {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(VoiceTurnLinkStatus::Active),
            "cancelled" => Some(VoiceTurnLinkStatus::Cancelled),
            "terminal" => Some(VoiceTurnLinkStatus::Terminal),
            _ => None,
        }
    }
}

/// In-memory representation of one `voice_turn_link` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceTurnLink {
    pub id: i64,
    pub guild_id: u64,
    pub voice_channel_id: u64,
    pub background_channel_id: u64,
    pub utterance_id: String,
    pub generation: i32,
    pub announce_message_id: Option<u64>,
    pub dispatch_id: Option<String>,
    pub turn_id: Option<String>,
    pub status: VoiceTurnLinkStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Payload accepted by [`upsert_active_voice_turn_link_pg`] and
/// [`retarget_voice_turn_link_pg`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceTurnLinkInsert {
    pub guild_id: u64,
    pub voice_channel_id: u64,
    pub background_channel_id: u64,
    pub utterance_id: String,
    pub generation: i32,
    pub announce_message_id: Option<u64>,
    pub dispatch_id: Option<String>,
    pub turn_id: Option<String>,
}

/// Optional identifiers learned after the initial voice link row exists.
/// Each supplied value is "attach only": it fills a NULL column or confirms
/// an identical value, but never overwrites a different durable identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceTurnLinkIdentityPatch {
    pub guild_id: u64,
    pub voice_channel_id: u64,
    pub utterance_id: String,
    pub generation: i32,
    pub announce_message_id: Option<u64>,
    pub dispatch_id: Option<String>,
    pub turn_id: Option<String>,
}

/// The result of [`attach_voice_turn_link_ids_pg`].
///
/// Callers distinguish "nothing existed" from "something conflicted" by
/// matching on this outcome directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachOutcome {
    /// At least one NULL column was filled in; `updated_at` was bumped.
    Attached(VoiceTurnLink),
    /// Row found but no column needed updating — all-None patch or every
    /// supplied value already matched the stored durable identity.
    /// `updated_at` was **not** touched.
    Unchanged(VoiceTurnLink),
    /// No row exists for the supplied `(guild_id, voice_channel_id,
    /// utterance_id, generation)` key.
    NotFound,
    /// The row exists but carries a different durable identity for at
    /// least one supplied non-`None` field. The row was not modified.
    Conflict,
}

fn u64_to_i64(value: u64) -> i64 {
    value as i64
}

fn i64_to_u64(value: i64) -> u64 {
    value as u64
}

fn row_to_link(row: &sqlx::postgres::PgRow) -> VoiceTurnLink {
    let status_raw: String = row.get("status");
    let status = VoiceTurnLinkStatus::parse(&status_raw).unwrap_or_else(|| {
        // Defensive: the CHECK constraint should make this unreachable, but
        // if a future migration ever loosens the constraint we want a sane
        // fallback rather than a panic in production.
        tracing::warn!(
            status = %status_raw,
            "[voice_turn_link] unknown status value in row; defaulting to active"
        );
        VoiceTurnLinkStatus::Active
    });
    VoiceTurnLink {
        id: row.get::<i64, _>("id"),
        guild_id: i64_to_u64(row.get::<i64, _>("guild_id")),
        voice_channel_id: i64_to_u64(row.get::<i64, _>("voice_channel_id")),
        background_channel_id: i64_to_u64(row.get::<i64, _>("background_channel_id")),
        utterance_id: row.get::<String, _>("utterance_id"),
        generation: row.get::<i32, _>("generation"),
        announce_message_id: row
            .get::<Option<i64>, _>("announce_message_id")
            .map(i64_to_u64),
        dispatch_id: row.get::<Option<String>, _>("dispatch_id"),
        turn_id: row.get::<Option<String>, _>("turn_id"),
        status,
        created_at: row.get::<DateTime<Utc>, _>("created_at"),
        updated_at: row.get::<DateTime<Utc>, _>("updated_at"),
    }
}

/// SQL `RETURNING` projection used by every helper that yields a
/// [`VoiceTurnLink`]. Kept centralised so column drift is impossible.
const RETURNING_COLUMNS: &str = "id, guild_id, voice_channel_id, background_channel_id, \
    utterance_id, generation, announce_message_id, dispatch_id, turn_id, status, created_at, \
    updated_at";

/// Atomic retarget: mark every strictly-prior `active` row for
/// `(guild_id, voice_channel_id, utterance_id)` as `cancelled`, then insert
/// the new generation as `active`. Wrapped in a single transaction with a
/// per-utterance `pg_advisory_xact_lock` so concurrent retargets for the
/// same utterance run serially and cannot both leave `active` rows behind.
/// The partial unique index `voice_turn_link_unique_active` provides a
/// schema-level backstop for the same invariant.
///
/// Stale-retry semantics: if a delayed retry arrives for `generation = N`
/// after a later retarget has already advanced the utterance to
/// `generation = M > N`, the stale call is treated as a no-op. The newer
/// active row is **not** cancelled, and `Ok(None)` is returned. Likewise a
/// same-generation retry of the most recent active row deduplicates to
/// `Ok(None)` without mutation.
pub async fn retarget_voice_turn_link_pg(
    pool: &PgPool,
    insert: &VoiceTurnLinkInsert,
) -> Result<Option<VoiceTurnLink>> {
    let mut tx = pool.begin().await?;

    // 0. Serialize concurrent retargets for the same utterance. The lock
    //    key is derived from (guild_id, voice_channel_id, utterance_id)
    //    so it does not collide across utterances. `xact` flavour
    //    releases automatically at COMMIT/ROLLBACK.
    let lock_key = advisory_lock_key(
        insert.guild_id,
        insert.voice_channel_id,
        &insert.utterance_id,
    );
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(lock_key)
        .execute(&mut *tx)
        .await?;

    // 1. Look at the latest generation for this utterance and its
    //    status. The state machine treats 'terminal' as closing the
    //    *current* generation (i.e. the utterance as a whole) only when
    //    the latest generation is terminal. A late completion that
    //    terminalises an older, already-cancelled generation must NOT
    //    block a newer retarget — gen1 can still legitimately retarget
    //    to gen2 even if gen0 just transitioned cancelled→terminal.
    //
    //    Concretely:
    //      latest_row.status = 'terminal' → utterance closed, no-op.
    //      latest_row.status = 'active'   → proceed with normal retarget.
    //      latest_row.status = 'cancelled'→ proceed (history watermark
    //        still applies via the generation check below).
    //      no rows                        → proceed (fresh insert).
    //
    //    The generation-watermark check (`<= latest_generation`) catches
    //    stale retries regardless of latest_status.
    #[derive(sqlx::FromRow)]
    struct LatestRow {
        generation: i32,
        status: String,
    }
    let latest: Option<LatestRow> = sqlx::query_as(
        "SELECT generation, status
           FROM voice_turn_link
          WHERE guild_id = $1
            AND voice_channel_id = $2
            AND utterance_id = $3
          ORDER BY generation DESC
          LIMIT 1",
    )
    .bind(u64_to_i64(insert.guild_id))
    .bind(u64_to_i64(insert.voice_channel_id))
    .bind(&insert.utterance_id)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some(latest_row) = latest.as_ref() {
        if latest_row.status == "terminal" {
            // The current generation of this utterance is closed.
            // Further retargets would resurrect a finished turn.
            tx.commit().await?;
            return Ok(None);
        }
        if insert.generation <= latest_row.generation {
            // Stale or same-generation retry — do not mutate.
            tx.commit().await?;
            return Ok(None);
        }
    }

    // 2. Cancel strictly-prior active rows. Using `<` (not `<>`) protects
    //    a future-generation that may already exist in 'cancelled' or
    //    'terminal' state — those are immutable history.
    sqlx::query(
        "UPDATE voice_turn_link
            SET status = 'cancelled', updated_at = NOW()
          WHERE guild_id = $1
            AND voice_channel_id = $2
            AND utterance_id = $3
            AND generation < $4
            AND status = 'active'",
    )
    .bind(u64_to_i64(insert.guild_id))
    .bind(u64_to_i64(insert.voice_channel_id))
    .bind(&insert.utterance_id)
    .bind(insert.generation)
    .execute(&mut *tx)
    .await?;

    // 3. Insert the new generation. ON CONFLICT covers the rare case where
    //    the same (utterance, generation) was inserted by a prior commit
    //    that we somehow raced past — defensive only, since the advisory
    //    lock already serialises us.
    let sql = format!(
        "INSERT INTO voice_turn_link (
             guild_id, voice_channel_id, background_channel_id,
             utterance_id, generation, announce_message_id, dispatch_id, turn_id,
             status, created_at, updated_at
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'active', NOW(), NOW())
         ON CONFLICT (guild_id, voice_channel_id, utterance_id, generation)
         DO NOTHING
         RETURNING {RETURNING_COLUMNS}"
    );
    let result = sqlx::query(&sql)
        .bind(u64_to_i64(insert.guild_id))
        .bind(u64_to_i64(insert.voice_channel_id))
        .bind(u64_to_i64(insert.background_channel_id))
        .bind(&insert.utterance_id)
        .bind(insert.generation)
        .bind(insert.announce_message_id.map(u64_to_i64))
        .bind(insert.dispatch_id.as_deref())
        .bind(insert.turn_id.as_deref())
        .fetch_optional(&mut *tx)
        .await;

    // A turn_id collision with a different row is treated as a dedup: the
    // caller gets Ok(None) rather than a propagated UniqueViolation. The tx
    // drops here and auto-rolls back in the conflict branch.
    let inserted = match result {
        Ok(row) => row,
        Err(sqlx::Error::Database(e)) if e.constraint() == Some("voice_turn_link_turn_id_uq") => {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };

    tx.commit().await?;

    Ok(inserted.as_ref().map(row_to_link))
}

/// Create or advance the active link for an utterance.
///
/// This is the C-series friendly entry point: with no existing row it creates
/// generation 0 (or whichever generation the caller supplies); with a higher
/// generation it cancels prior active rows and inserts the new active row; with
/// an already-present same generation it attaches any newly learned identifiers
/// and returns the active row. Stale or post-terminal attempts return `None`.
///
/// **Conflict semantics**: if the row already carries a different durable
/// identity for any supplied non-`None` field (dispatch_id, announce_message_id,
/// or turn_id), the call returns `Ok(None)` — the conflicting insert is
/// rejected, not silently ignored. Callers must not treat that `None` as
/// success; the stored link belongs to a different identity set.
pub async fn upsert_active_voice_turn_link_pg(
    pool: &PgPool,
    insert: &VoiceTurnLinkInsert,
) -> Result<Option<VoiceTurnLink>> {
    if let Some(link) = retarget_voice_turn_link_pg(pool, insert).await? {
        return Ok(Some(link));
    }

    let patch = VoiceTurnLinkIdentityPatch {
        guild_id: insert.guild_id,
        voice_channel_id: insert.voice_channel_id,
        utterance_id: insert.utterance_id.clone(),
        generation: insert.generation,
        announce_message_id: insert.announce_message_id,
        dispatch_id: insert.dispatch_id.clone(),
        turn_id: insert.turn_id.clone(),
    };
    match attach_voice_turn_link_ids_pg(pool, &patch).await? {
        AttachOutcome::Attached(link) | AttachOutcome::Unchanged(link) => {
            if link.status == VoiceTurnLinkStatus::Active {
                return Ok(Some(link));
            }
            // Row exists but is not active (cancelled/terminal) — fall through.
        }
        AttachOutcome::Conflict => {
            // Caller supplied IDs that conflict with stored durable identities.
            // Reject rather than silently returning the old link.
            return Ok(None);
        }
        AttachOutcome::NotFound => {
            // Row doesn't exist for this generation — fall through to lookup.
        }
    }

    let active = lookup_active_voice_turn_link_by_utterance_pg(
        pool,
        insert.guild_id,
        insert.voice_channel_id,
        &insert.utterance_id,
    )
    .await?;
    Ok(active.filter(|link| link.generation == insert.generation))
}

/// Attach announce/dispatch/turn identifiers to an existing durable link.
///
/// Returns [`AttachOutcome`] so callers can distinguish the four cases:
///
/// * `Attached(row)` — at least one NULL column was filled; `updated_at`
///   was bumped.
/// * `Unchanged(row)` — row found but nothing was written (all-None patch or
///   every supplied value already matched the stored identity); `updated_at`
///   was **not** touched, so route resolution order is preserved.
/// * `NotFound` — no row exists for `(guild_id, voice_channel_id,
///   utterance_id, generation)`.
/// * `Conflict` — the row exists but already carries a different durable
///   identity for at least one supplied non-`None` field; the row is
///   unchanged.
pub async fn attach_voice_turn_link_ids_pg(
    pool: &PgPool,
    patch: &VoiceTurnLinkIdentityPatch,
) -> Result<AttachOutcome> {
    // Read the current row first so we can detect conflicts and no-ops
    // without an UPDATE that would spuriously bump updated_at.
    let select_sql = format!(
        "SELECT {RETURNING_COLUMNS}
           FROM voice_turn_link
          WHERE guild_id = $1
            AND voice_channel_id = $2
            AND utterance_id = $3
            AND generation = $4"
    );
    let current_row = sqlx::query(&select_sql)
        .bind(u64_to_i64(patch.guild_id))
        .bind(u64_to_i64(patch.voice_channel_id))
        .bind(&patch.utterance_id)
        .bind(patch.generation)
        .fetch_optional(pool)
        .await?;

    let Some(current_row) = current_row else {
        return Ok(AttachOutcome::NotFound);
    };
    let current = row_to_link(&current_row);

    // Detect conflicts: a supplied non-None value that differs from an
    // already-stored non-None value. First-writer wins; the identity is
    // durable once set.
    if let (Some(new), Some(old)) = (patch.announce_message_id, current.announce_message_id) {
        if new != old {
            return Ok(AttachOutcome::Conflict);
        }
    }
    if let (Some(new), Some(old)) = (&patch.dispatch_id, &current.dispatch_id) {
        if new != old {
            return Ok(AttachOutcome::Conflict);
        }
    }
    if let (Some(new), Some(old)) = (&patch.turn_id, &current.turn_id) {
        if new != old {
            return Ok(AttachOutcome::Conflict);
        }
    }

    // Determine which columns actually need to be written.
    let needs_announce =
        patch.announce_message_id.is_some() && current.announce_message_id.is_none();
    let needs_dispatch = patch.dispatch_id.is_some() && current.dispatch_id.is_none();
    let needs_turn = patch.turn_id.is_some() && current.turn_id.is_none();

    if !needs_announce && !needs_dispatch && !needs_turn {
        // Nothing to write — return the row without touching updated_at so
        // route resolution order (ORDER BY updated_at DESC) is not disturbed.
        return Ok(AttachOutcome::Unchanged(current));
    }

    // At least one NULL column can be filled. Do the UPDATE.  The WHERE
    // clause is still conflict-safe for any race between the SELECT above
    // and this UPDATE.  A concurrent writer that beat us to one of these
    // columns will cause the UPDATE to return nothing → Conflict.
    let update_sql = format!(
        "UPDATE voice_turn_link
            SET announce_message_id = COALESCE(announce_message_id, $5),
                dispatch_id = COALESCE(dispatch_id, $6),
                turn_id = COALESCE(turn_id, $7),
                updated_at = NOW()
          WHERE guild_id = $1
            AND voice_channel_id = $2
            AND utterance_id = $3
            AND generation = $4
            AND ($5::BIGINT IS NULL OR announce_message_id IS NULL OR announce_message_id = $5)
            AND ($6::TEXT IS NULL OR dispatch_id IS NULL OR dispatch_id = $6)
            AND ($7::TEXT IS NULL OR turn_id IS NULL OR turn_id = $7)
          RETURNING {RETURNING_COLUMNS}"
    );
    let result = sqlx::query(&update_sql)
        .bind(u64_to_i64(patch.guild_id))
        .bind(u64_to_i64(patch.voice_channel_id))
        .bind(&patch.utterance_id)
        .bind(patch.generation)
        .bind(patch.announce_message_id.map(u64_to_i64))
        .bind(patch.dispatch_id.as_deref())
        .bind(patch.turn_id.as_deref())
        .fetch_optional(pool)
        .await;

    match result {
        Ok(Some(row)) => Ok(AttachOutcome::Attached(row_to_link(&row))),
        Ok(None) => {
            // A concurrent writer filled in a conflicting value between our
            // SELECT and this UPDATE.
            Ok(AttachOutcome::Conflict)
        }
        Err(sqlx::Error::Database(e)) if e.constraint() == Some("voice_turn_link_turn_id_uq") => {
            // Another row already holds this turn_id.
            Ok(AttachOutcome::Conflict)
        }
        Err(e) => Err(e.into()),
    }
}

/// Derive a stable i64 advisory-lock key from the
/// `(guild_id, voice_channel_id, utterance_id)` triple.
///
/// Stability is load-bearing here: during a rolling deploy, two
/// different binaries on different nodes must compute identical keys
/// for the same utterance, otherwise `mark_terminal` and `retarget` can
/// take different advisory locks and reintroduce the READ COMMITTED
/// interleaving the lock is designed to prevent.
///
/// We therefore use a hand-rolled FNV-1a 64-bit hash over a fixed byte
/// encoding: domain tag, little-endian guild_id, little-endian
/// voice_channel_id, utf-8 utterance_id bytes. FNV-1a is documented and
/// trivially stable across Rust versions and platforms. The fixed-vector
/// test in `tests::advisory_lock_key_is_stable` pins the output.
fn advisory_lock_key(guild_id: u64, voice_channel_id: u64, utterance_id: &str) -> i64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash: u64 = FNV_OFFSET_BASIS;
    let mut absorb = |bytes: &[u8]| {
        for &byte in bytes {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    };

    // Domain tag so other future advisory-lock users in the same DB
    // cannot collide on the same numeric key by accident.
    absorb(b"voice_turn_link\0");
    absorb(&guild_id.to_le_bytes());
    absorb(&voice_channel_id.to_le_bytes());
    absorb(utterance_id.as_bytes());

    hash as i64
}

pub async fn lookup_active_voice_turn_link_by_dispatch_id_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<Option<VoiceTurnLink>> {
    let sql = format!(
        "SELECT {RETURNING_COLUMNS}
           FROM voice_turn_link
          WHERE dispatch_id = $1
            AND status = 'active'
          ORDER BY updated_at DESC, id DESC
          LIMIT 1"
    );
    let row = sqlx::query(&sql)
        .bind(dispatch_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(row_to_link))
}

pub async fn lookup_active_voice_turn_link_by_announce_message_id_pg(
    pool: &PgPool,
    announce_message_id: u64,
) -> Result<Option<VoiceTurnLink>> {
    let sql = format!(
        "SELECT {RETURNING_COLUMNS}
           FROM voice_turn_link
          WHERE announce_message_id = $1
            AND status = 'active'
          ORDER BY updated_at DESC, id DESC
          LIMIT 1"
    );
    let row = sqlx::query(&sql)
        .bind(u64_to_i64(announce_message_id))
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(row_to_link))
}

pub async fn lookup_active_voice_turn_link_by_turn_id_pg(
    pool: &PgPool,
    turn_id: &str,
) -> Result<Option<VoiceTurnLink>> {
    let sql = format!(
        "SELECT {RETURNING_COLUMNS}
           FROM voice_turn_link
          WHERE turn_id = $1
            AND status = 'active'
          ORDER BY updated_at DESC, id DESC
          LIMIT 1"
    );
    let row = sqlx::query(&sql).bind(turn_id).fetch_optional(pool).await?;
    Ok(row.as_ref().map(row_to_link))
}

pub async fn lookup_active_voice_turn_link_by_utterance_pg(
    pool: &PgPool,
    guild_id: u64,
    voice_channel_id: u64,
    utterance_id: &str,
) -> Result<Option<VoiceTurnLink>> {
    let sql = format!(
        "SELECT {RETURNING_COLUMNS}
           FROM voice_turn_link
          WHERE guild_id = $1
            AND voice_channel_id = $2
            AND utterance_id = $3
            AND status = 'active'
          ORDER BY generation DESC, updated_at DESC, id DESC
          LIMIT 1"
    );
    let row = sqlx::query(&sql)
        .bind(u64_to_i64(guild_id))
        .bind(u64_to_i64(voice_channel_id))
        .bind(utterance_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(row_to_link))
}

/// Flip a specific (guild, voice channel, utterance, generation) row to
/// `terminal`. Returns the updated row, or `None` if no matching row
/// exists. Status transitions from `active` and `cancelled` are both
/// permitted: a turn that gets retargeted *and then* completes from the
/// cancelled branch (rare race, but possible during reconnection) is
/// still observable as terminal.
pub async fn mark_terminal_voice_turn_link_pg(
    pool: &PgPool,
    guild_id: u64,
    voice_channel_id: u64,
    utterance_id: &str,
    generation: i32,
) -> Result<Option<VoiceTurnLink>> {
    let mut tx = pool.begin().await?;

    // Same advisory lock as retarget/upsert so completion can never
    // interleave with a concurrent retarget in a way that resurrects the
    // closed utterance back to active.
    let lock_key = advisory_lock_key(guild_id, voice_channel_id, utterance_id);
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(lock_key)
        .execute(&mut *tx)
        .await?;

    let sql = format!(
        "UPDATE voice_turn_link
            SET status = 'terminal', updated_at = NOW()
          WHERE guild_id = $1
            AND voice_channel_id = $2
            AND utterance_id = $3
            AND generation = $4
          RETURNING {RETURNING_COLUMNS}"
    );
    let row = sqlx::query(&sql)
        .bind(u64_to_i64(guild_id))
        .bind(u64_to_i64(voice_channel_id))
        .bind(utterance_id)
        .bind(generation)
        .fetch_optional(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(row.as_ref().map(row_to_link))
}

/// GC sweep for old terminal rows. Only `terminal` rows older than
/// `older_than` are deleted; `active` and `cancelled` rows are
/// intentionally preserved because background turns can live 24h+ and the
/// cancelled tombstones preserve durable history during late completion and
/// reconciliation (e.g. after a retarget already happened).
///
/// Returns the number of rows actually deleted.
pub async fn gc_terminal_voice_turn_links_pg(
    pool: &PgPool,
    older_than: DateTime<Utc>,
) -> Result<u64> {
    let deleted = sqlx::query(
        "DELETE FROM voice_turn_link
          WHERE status = 'terminal'
            AND updated_at < $1",
    )
    .bind(older_than)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    struct TestPostgresDb {
        _lock: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn try_create() -> Option<Self> {
            let lock = crate::db::postgres::lock_test_lifecycle();
            let admin_url = postgres_admin_database_url();
            let database_name = format!(
                "agentdesk_voice_turn_link_{}",
                uuid::Uuid::new_v4().simple()
            );
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            if let Err(error) = crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "voice turn link tests",
            )
            .await
            {
                eprintln!("skipping postgres-backed voice_turn_link test: {error}");
                drop(lock);
                return None;
            }
            Some(Self {
                _lock: lock,
                admin_url,
                database_name,
                database_url,
            })
        }

        async fn connect_and_migrate(&self) -> PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "voice turn link tests",
            )
            .await
            .unwrap()
        }

        async fn connect_and_migrate_with_max_connections(&self, max_connections: u32) -> PgPool {
            crate::db::postgres::connect_test_pool_with_max_connections_and_migrate(
                &self.database_url,
                "voice turn link tests",
                max_connections,
            )
            .await
            .unwrap()
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "voice turn link tests",
            )
            .await
            .unwrap();
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }
        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());
        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    fn sample_insert(generation: i32) -> VoiceTurnLinkInsert {
        VoiceTurnLinkInsert {
            guild_id: 100,
            voice_channel_id: 200,
            background_channel_id: 300,
            utterance_id: "utt-42".to_string(),
            generation,
            announce_message_id: Some(400 + generation as u64),
            dispatch_id: Some(format!("dispatch-{generation}")),
            turn_id: Some(format!("turn-{generation}")),
        }
    }

    #[tokio::test]
    async fn retarget_cancels_prior_active_and_inserts_new_pg() {
        let Some(pg) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg.connect_and_migrate().await;

        retarget_voice_turn_link_pg(&pool, &sample_insert(0))
            .await
            .unwrap()
            .expect("seed insert");

        let mut next = sample_insert(1);
        next.background_channel_id = 999; // retarget to a different background channel
        next.dispatch_id = Some("dispatch-retarget".to_string());
        next.announce_message_id = Some(700);

        let inserted = retarget_voice_turn_link_pg(&pool, &next)
            .await
            .unwrap()
            .expect("retarget must insert the new generation");
        assert_eq!(inserted.generation, 1);
        assert_eq!(inserted.background_channel_id, 999);
        assert_eq!(inserted.status, VoiceTurnLinkStatus::Active);

        // Prior generation should now be cancelled.
        let prior: (i32, String) =
            sqlx::query_as("SELECT generation, status FROM voice_turn_link WHERE dispatch_id = $1")
                .bind("dispatch-0")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(prior.0, 0);
        assert_eq!(prior.1, "cancelled");

        pool.close().await;
        pg.drop().await;
    }

    #[tokio::test]
    async fn retarget_with_same_generation_is_idempotent_pg() {
        let Some(pg) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg.connect_and_migrate().await;

        retarget_voice_turn_link_pg(&pool, &sample_insert(0))
            .await
            .unwrap()
            .expect("first retarget inserts");
        // Re-applying the same generation is a no-op; the existing row
        // stays active and no new row is inserted.
        let again = retarget_voice_turn_link_pg(&pool, &sample_insert(0))
            .await
            .unwrap();
        assert!(
            again.is_none(),
            "same-generation collision must dedup to None"
        );

        let row = lookup_active_voice_turn_link_by_dispatch_id_pg(&pool, "dispatch-0")
            .await
            .unwrap()
            .expect("row exists");
        assert_eq!(row.status, VoiceTurnLinkStatus::Active);
        assert_eq!(row.generation, 0);

        pool.close().await;
        pg.drop().await;
    }

    #[tokio::test]
    async fn attach_ids_and_lookup_active_by_turn_id_pg() {
        let Some(pg) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg.connect_and_migrate().await;

        let mut insert = sample_insert(0);
        insert.announce_message_id = None;
        insert.dispatch_id = None;
        insert.turn_id = None;
        retarget_voice_turn_link_pg(&pool, &insert)
            .await
            .unwrap()
            .expect("seed insert");

        let attached = match attach_voice_turn_link_ids_pg(
            &pool,
            &VoiceTurnLinkIdentityPatch {
                guild_id: 100,
                voice_channel_id: 200,
                utterance_id: "utt-42".to_string(),
                generation: 0,
                announce_message_id: Some(777),
                dispatch_id: Some("dispatch-attached".to_string()),
                turn_id: Some("turn-attached".to_string()),
            },
        )
        .await
        .unwrap()
        {
            AttachOutcome::Attached(link) => link,
            other => panic!("expected Attached, got {other:?}"),
        };
        assert_eq!(attached.announce_message_id, Some(777));
        assert_eq!(attached.dispatch_id.as_deref(), Some("dispatch-attached"));
        assert_eq!(attached.turn_id.as_deref(), Some("turn-attached"));

        let by_dispatch =
            lookup_active_voice_turn_link_by_dispatch_id_pg(&pool, "dispatch-attached")
                .await
                .unwrap()
                .expect("active lookup by dispatch_id");
        assert_eq!(by_dispatch.utterance_id, "utt-42");

        let by_announce = lookup_active_voice_turn_link_by_announce_message_id_pg(&pool, 777)
            .await
            .unwrap()
            .expect("active lookup by announce_message_id");
        assert_eq!(
            by_announce.dispatch_id.as_deref(),
            Some("dispatch-attached")
        );

        let by_turn = lookup_active_voice_turn_link_by_turn_id_pg(&pool, "turn-attached")
            .await
            .unwrap()
            .expect("active lookup by turn_id");
        assert_eq!(by_turn.voice_channel_id, 200);

        let conflict = attach_voice_turn_link_ids_pg(
            &pool,
            &VoiceTurnLinkIdentityPatch {
                guild_id: 100,
                voice_channel_id: 200,
                utterance_id: "utt-42".to_string(),
                generation: 0,
                announce_message_id: None,
                dispatch_id: Some("dispatch-different".to_string()),
                turn_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            conflict,
            AttachOutcome::Conflict,
            "attach must report and preserve a different dispatch_id"
        );

        pool.close().await;
        pg.drop().await;
    }

    #[tokio::test]
    async fn upsert_active_returns_existing_same_generation_with_new_ids_pg() {
        let Some(pg) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg.connect_and_migrate().await;

        let mut seed = sample_insert(0);
        seed.announce_message_id = None;
        seed.dispatch_id = None;
        seed.turn_id = None;
        let inserted = upsert_active_voice_turn_link_pg(&pool, &seed)
            .await
            .unwrap()
            .expect("initial upsert inserts");
        assert_eq!(inserted.status, VoiceTurnLinkStatus::Active);

        let mut same_generation = seed.clone();
        same_generation.announce_message_id = Some(910);
        same_generation.dispatch_id = Some("dispatch-upsert".to_string());
        same_generation.turn_id = Some("turn-upsert".to_string());
        let updated = upsert_active_voice_turn_link_pg(&pool, &same_generation)
            .await
            .unwrap()
            .expect("same generation upsert returns existing active row");
        assert_eq!(updated.id, inserted.id);
        assert_eq!(updated.announce_message_id, Some(910));
        assert_eq!(updated.dispatch_id.as_deref(), Some("dispatch-upsert"));
        assert_eq!(updated.turn_id.as_deref(), Some("turn-upsert"));

        pool.close().await;
        pg.drop().await;
    }

    #[tokio::test]
    async fn mark_terminal_updates_status_pg() {
        let Some(pg) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg.connect_and_migrate().await;

        retarget_voice_turn_link_pg(&pool, &sample_insert(0))
            .await
            .unwrap();

        let updated = mark_terminal_voice_turn_link_pg(&pool, 100, 200, "utt-42", 0)
            .await
            .unwrap()
            .expect("mark_terminal returns the updated row");
        assert_eq!(updated.status, VoiceTurnLinkStatus::Terminal);

        // Missing row → None.
        let missing = mark_terminal_voice_turn_link_pg(&pool, 100, 200, "utt-missing", 0)
            .await
            .unwrap();
        assert!(missing.is_none());

        pool.close().await;
        pg.drop().await;
    }

    #[tokio::test]
    async fn gc_deletes_only_old_terminal_rows_pg() {
        let Some(pg) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg.connect_and_migrate().await;

        // Active row — must survive GC.
        let mut active = sample_insert(0);
        active.utterance_id = "utt-active".to_string();
        active.dispatch_id = Some("dispatch-active".to_string());
        active.turn_id = Some("turn-active".to_string());
        retarget_voice_turn_link_pg(&pool, &active).await.unwrap();

        // Cancelled row — must survive GC (long-lived background turn
        // tombstone preserved for late lookups).
        let mut cancelled = sample_insert(0);
        cancelled.utterance_id = "utt-cancelled".to_string();
        cancelled.dispatch_id = Some("dispatch-cancelled".to_string());
        cancelled.turn_id = Some("turn-cancelled".to_string());
        retarget_voice_turn_link_pg(&pool, &cancelled)
            .await
            .unwrap();
        let mut cancelled_next = cancelled.clone();
        cancelled_next.generation = 1;
        cancelled_next.dispatch_id = Some("dispatch-cancelled-next".to_string());
        cancelled_next.announce_message_id = Some(9991);
        cancelled_next.turn_id = Some("turn-cancelled-next".to_string());
        retarget_voice_turn_link_pg(&pool, &cancelled_next)
            .await
            .unwrap();

        // Terminal row — eligible for GC.
        let mut terminal = sample_insert(0);
        terminal.utterance_id = "utt-terminal".to_string();
        terminal.dispatch_id = Some("dispatch-terminal".to_string());
        terminal.turn_id = Some("turn-terminal".to_string());
        retarget_voice_turn_link_pg(&pool, &terminal).await.unwrap();
        mark_terminal_voice_turn_link_pg(&pool, 100, 200, "utt-terminal", 0)
            .await
            .unwrap();

        // Backdate the terminal row's updated_at past the cutoff. We rely
        // on the test DB's NOW() being close to wall clock; setting
        // updated_at to an explicit past timestamp is more deterministic
        // than sleeping.
        sqlx::query(
            "UPDATE voice_turn_link
                SET updated_at = NOW() - INTERVAL '48 hours'
              WHERE utterance_id = 'utt-terminal'",
        )
        .execute(&pool)
        .await
        .unwrap();

        let cutoff = Utc::now() - ChronoDuration::hours(24);
        let deleted = gc_terminal_voice_turn_links_pg(&pool, cutoff)
            .await
            .unwrap();
        assert_eq!(
            deleted, 1,
            "exactly the aged terminal row should be deleted"
        );

        // Active and cancelled rows must remain.
        let active_row = lookup_active_voice_turn_link_by_dispatch_id_pg(&pool, "dispatch-active")
            .await
            .unwrap();
        assert!(active_row.is_some(), "active row survives GC");
        assert_eq!(active_row.unwrap().status, VoiceTurnLinkStatus::Active);

        let cancelled_status: Option<String> =
            sqlx::query_scalar("SELECT status FROM voice_turn_link WHERE dispatch_id = $1")
                .bind("dispatch-cancelled")
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert_eq!(
            cancelled_status.as_deref(),
            Some("cancelled"),
            "cancelled tombstone survives GC"
        );

        // Terminal row is gone.
        let terminal_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM voice_turn_link WHERE dispatch_id = $1")
                .bind("dispatch-terminal")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(terminal_count, 0, "terminal row deleted by GC");

        // Young terminal rows (after cutoff) must not be deleted. Add a
        // fresh terminal row and rerun GC.
        let mut fresh = sample_insert(0);
        fresh.utterance_id = "utt-fresh-terminal".to_string();
        fresh.dispatch_id = Some("dispatch-fresh".to_string());
        fresh.turn_id = Some("turn-fresh".to_string());
        retarget_voice_turn_link_pg(&pool, &fresh).await.unwrap();
        mark_terminal_voice_turn_link_pg(&pool, 100, 200, "utt-fresh-terminal", 0)
            .await
            .unwrap();
        let deleted_again = gc_terminal_voice_turn_links_pg(&pool, cutoff)
            .await
            .unwrap();
        assert_eq!(deleted_again, 0, "young terminal rows must not be GC'd");

        pool.close().await;
        pg.drop().await;
    }

    /// Stale-retry regression (Codex review #2362): a delayed retarget
    /// retry for generation N must NOT cancel a newer active row at
    /// generation M (M > N). Reapplying gen 1 after gen 2 is active is
    /// a no-op.
    #[tokio::test]
    async fn stale_retarget_retry_does_not_cancel_newer_active_pg() {
        let Some(pg) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg.connect_and_migrate().await;

        // Establish initial active gen 0.
        retarget_voice_turn_link_pg(&pool, &sample_insert(0))
            .await
            .unwrap()
            .expect("seed insert");

        // Advance to gen 2 via two retargets.
        retarget_voice_turn_link_pg(&pool, &sample_insert(1))
            .await
            .unwrap()
            .expect("retarget to gen 1");
        retarget_voice_turn_link_pg(&pool, &sample_insert(2))
            .await
            .unwrap()
            .expect("retarget to gen 2");

        // A *stale* retry for gen 1 arrives late.
        let stale = retarget_voice_turn_link_pg(&pool, &sample_insert(1))
            .await
            .unwrap();
        assert!(
            stale.is_none(),
            "stale retarget retry must dedupe to None, not mutate newer rows"
        );

        // The newer gen 2 row must still be active. We look it up by
        // dispatch_id directly to be unambiguous.
        let gen2 = lookup_active_voice_turn_link_by_dispatch_id_pg(&pool, "dispatch-2")
            .await
            .unwrap()
            .expect("gen 2 row exists");
        assert_eq!(gen2.generation, 2);
        assert_eq!(
            gen2.status,
            VoiceTurnLinkStatus::Active,
            "newer active row must NOT be cancelled by stale retry"
        );

        // And there must be exactly one active row for this utterance.
        let active_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM voice_turn_link
              WHERE guild_id = 100
                AND voice_channel_id = 200
                AND utterance_id = 'utt-42'
                AND status = 'active'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(active_count, 1, "exactly one active row per utterance");

        pool.close().await;
        pg.drop().await;
    }

    /// Stable advisory-lock key (Codex review #2362 round 3).
    /// `mark_terminal` and `retarget` must compute the same key for the
    /// same utterance across any binary that touches the table; a
    /// rolling deploy with two different hashers would silently
    /// reintroduce the READ COMMITTED interleaving the lock is supposed
    /// to prevent. This fixed-vector test pins the FNV-1a output.
    #[test]
    fn advisory_lock_key_is_stable() {
        // Pinned value — change here breaks rolling-deploy safety.
        // Regenerate ONLY together with a deliberate, communicated
        // schema/protocol bump.
        assert_eq!(
            advisory_lock_key(100, 200, "utt-42"),
            4_421_636_910_427_734_922
        );
        // Different inputs must produce different keys.
        assert_ne!(
            advisory_lock_key(100, 200, "utt-42"),
            advisory_lock_key(100, 200, "utt-43")
        );
        assert_ne!(
            advisory_lock_key(100, 200, "utt-42"),
            advisory_lock_key(101, 200, "utt-42")
        );
        assert_ne!(
            advisory_lock_key(100, 200, "utt-42"),
            advisory_lock_key(100, 201, "utt-42")
        );
    }

    /// Stale-terminal regression (Codex review #2362 round 3). A late
    /// completion that terminalises an already-cancelled prior
    /// generation must NOT block a fresh retarget of the live
    /// generation. The "is utterance closed" probe must look at the
    /// LATEST generation, not "any terminal row anywhere".
    #[tokio::test]
    async fn late_terminal_on_cancelled_generation_does_not_block_retarget_pg() {
        let Some(pg) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg.connect_and_migrate().await;

        // gen0 active.
        retarget_voice_turn_link_pg(&pool, &sample_insert(0))
            .await
            .unwrap()
            .expect("seed gen0");
        // Retarget to gen1: gen0 -> cancelled, gen1 -> active.
        retarget_voice_turn_link_pg(&pool, &sample_insert(1))
            .await
            .unwrap()
            .expect("retarget to gen1");

        // Late completion arrives for gen0 (which is now cancelled).
        // This flips gen0 cancelled -> terminal. gen1 stays active.
        mark_terminal_voice_turn_link_pg(&pool, 100, 200, "utt-42", 0)
            .await
            .unwrap()
            .expect("late terminal on cancelled gen0");

        // Now retarget to gen2 should STILL proceed because gen1 is the
        // current live generation and gen0's late terminal is just
        // tombstone hygiene.
        let result = retarget_voice_turn_link_pg(&pool, &sample_insert(2))
            .await
            .unwrap()
            .expect("retarget gen2 must proceed despite stale terminal on gen0");
        assert_eq!(result.generation, 2);
        assert_eq!(result.status, VoiceTurnLinkStatus::Active);

        // Verify state: exactly one active (gen2), gen0 terminal, gen1
        // cancelled.
        let active_gen: i32 = sqlx::query_scalar(
            "SELECT generation FROM voice_turn_link
              WHERE guild_id = 100
                AND voice_channel_id = 200
                AND utterance_id = 'utt-42'
                AND status = 'active'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(active_gen, 2);

        pool.close().await;
        pg.drop().await;
    }

    /// Resurrection regression (Codex review #2362 round 2): once the
    /// LATEST generation for an utterance is `terminal`, no subsequent
    /// retarget — even at a strictly higher generation — may resurrect
    /// the utterance back to `active`. The closed turn must stay closed
    /// and remain GC-eligible.
    #[tokio::test]
    async fn retarget_after_mark_terminal_does_not_resurrect_pg() {
        let Some(pg) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg.connect_and_migrate().await;

        retarget_voice_turn_link_pg(&pool, &sample_insert(0))
            .await
            .unwrap()
            .expect("seed insert");
        mark_terminal_voice_turn_link_pg(&pool, 100, 200, "utt-42", 0)
            .await
            .unwrap()
            .expect("mark_terminal");

        // A late retarget arrives — try gen 1 (strictly higher than the
        // terminalised gen 0). It MUST become a no-op; otherwise the
        // closed turn is resurrected.
        let result = retarget_voice_turn_link_pg(&pool, &sample_insert(1))
            .await
            .unwrap();
        assert!(
            result.is_none(),
            "retarget after mark_terminal must NOT resurrect the utterance"
        );

        // No active rows must exist for this utterance.
        let active_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM voice_turn_link
              WHERE guild_id = 100
                AND voice_channel_id = 200
                AND utterance_id = 'utt-42'
                AND status = 'active'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(active_count, 0, "no active row may exist post-terminal");

        // And the terminal row must still be the gen 0 we created.
        let terminal_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM voice_turn_link
              WHERE guild_id = 100
                AND voice_channel_id = 200
                AND utterance_id = 'utt-42'
                AND status = 'terminal'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(terminal_count, 1, "terminal row remains intact");

        pool.close().await;
        pg.drop().await;
    }

    /// Concurrent mark_terminal vs retarget (Codex review #2362 round 2):
    /// these two ops on the same utterance must serialize via the
    /// advisory lock. Whichever commits first wins; the other becomes a
    /// no-op rather than violating the "one active row" invariant or
    /// resurrecting a terminal turn.
    #[tokio::test]
    async fn concurrent_mark_terminal_and_retarget_serialize_pg() {
        let Some(pg) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg.connect_and_migrate_with_max_connections(8).await;

        retarget_voice_turn_link_pg(&pool, &sample_insert(0))
            .await
            .unwrap()
            .expect("seed insert");

        let pool_a = pool.clone();
        let pool_b = pool.clone();
        let terminal_handle = tokio::spawn(async move {
            mark_terminal_voice_turn_link_pg(&pool_a, 100, 200, "utt-42", 0).await
        });
        let retarget_handle =
            tokio::spawn(
                async move { retarget_voice_turn_link_pg(&pool_b, &sample_insert(1)).await },
            );
        let _ = terminal_handle.await.unwrap().unwrap();
        let _ = retarget_handle.await.unwrap().unwrap();

        // Either order:
        //  (A) terminal commits first → retarget sees has_terminal=true → no-op.
        //      Final: 1 terminal row, 0 active.
        //  (B) retarget commits first → terminal then flips gen 0 to terminal.
        //      Final: 1 active row (gen 1), 1 terminal row (gen 0).
        // Both orders are valid. The invariant we enforce: at most one
        // active row for the utterance.
        let active_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM voice_turn_link
              WHERE guild_id = 100
                AND voice_channel_id = 200
                AND utterance_id = 'utt-42'
                AND status = 'active'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            active_count <= 1,
            "at most one active row allowed; got {active_count}"
        );

        pool.close().await;
        pg.drop().await;
    }

    /// Concurrent-retarget regression (Codex review #2362): two
    /// retargets running in parallel for the same utterance must
    /// serialize and leave exactly one active row. The partial unique
    /// index `voice_turn_link_unique_active` is the schema-level
    /// backstop; the advisory lock makes the failure path observable as
    /// "the late writer becomes a no-op" rather than "constraint
    /// violation".
    #[tokio::test]
    async fn concurrent_retarget_leaves_exactly_one_active_pg() {
        let Some(pg) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg.connect_and_migrate_with_max_connections(8).await;

        retarget_voice_turn_link_pg(&pool, &sample_insert(0))
            .await
            .unwrap()
            .expect("seed insert");

        // Fire two retargets concurrently — one to gen 1, one to gen 2.
        // Whichever commits second observes the other's active row and
        // either no-ops (if its own generation is now stale) or cancels
        // the older one and inserts itself.
        let pool_a = pool.clone();
        let pool_b = pool.clone();
        let handle_a =
            tokio::spawn(
                async move { retarget_voice_turn_link_pg(&pool_a, &sample_insert(1)).await },
            );
        let handle_b =
            tokio::spawn(
                async move { retarget_voice_turn_link_pg(&pool_b, &sample_insert(2)).await },
            );
        let _ = handle_a.await.unwrap().unwrap();
        let _ = handle_b.await.unwrap().unwrap();

        let active_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM voice_turn_link
              WHERE guild_id = 100
                AND voice_channel_id = 200
                AND utterance_id = 'utt-42'
                AND status = 'active'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            active_count, 1,
            "concurrent retargets must leave exactly one active row"
        );

        // The winning generation must be the higher of the two we fired
        // (gen 2). Either order of commit yields gen 2 active because:
        //   - if gen 1 commits first, gen 2 sees active_max=1, 2>1 →
        //     cancels gen 1, inserts gen 2.
        //   - if gen 2 commits first, gen 1 sees active_max=2, 1<=2 →
        //     no-op; gen 2 stays active.
        let winning_generation: i32 = sqlx::query_scalar(
            "SELECT generation FROM voice_turn_link
              WHERE guild_id = 100
                AND voice_channel_id = 200
                AND utterance_id = 'utt-42'
                AND status = 'active'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            winning_generation, 2,
            "the higher generation must win regardless of commit order"
        );

        pool.close().await;
        pg.drop().await;
    }

    /// all-None attach must return Unchanged and must NOT bump updated_at.
    /// A spurious updated_at bump would reorder route resolution results
    /// (`ORDER BY updated_at DESC`) without any semantic change.
    #[tokio::test]
    async fn attach_all_none_does_not_bump_updated_at_pg() {
        let Some(pg) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg.connect_and_migrate().await;

        let mut seed = sample_insert(0);
        seed.announce_message_id = None;
        seed.dispatch_id = None;
        seed.turn_id = None;
        let inserted = retarget_voice_turn_link_pg(&pool, &seed)
            .await
            .unwrap()
            .expect("seed insert");
        let original_updated_at = inserted.updated_at;

        // Tiny sleep so NOW() would be strictly greater if we wrote.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let outcome = attach_voice_turn_link_ids_pg(
            &pool,
            &VoiceTurnLinkIdentityPatch {
                guild_id: 100,
                voice_channel_id: 200,
                utterance_id: "utt-42".to_string(),
                generation: 0,
                announce_message_id: None,
                dispatch_id: None,
                turn_id: None,
            },
        )
        .await
        .unwrap();

        match outcome {
            AttachOutcome::Unchanged(link) => {
                assert_eq!(
                    link.updated_at, original_updated_at,
                    "all-None patch must not bump updated_at"
                );
            }
            other => panic!("expected Unchanged, got {other:?}"),
        }

        pool.close().await;
        pg.drop().await;
    }

    /// Same-generation conflicting dispatch_id: upsert must return None
    /// (rejected), not silently hand back the old active link. This was
    /// the original NOT CLEAN finding: callers could not tell a conflict
    /// from success.
    #[tokio::test]
    async fn upsert_conflicting_ids_returns_none_not_old_link_pg() {
        let Some(pg) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg.connect_and_migrate().await;

        // Establish an active row with a specific dispatch_id.
        let mut first = sample_insert(0);
        first.dispatch_id = Some("dispatch-first".to_string());
        first.announce_message_id = None;
        first.turn_id = Some("turn-first-unique".to_string());
        let link = upsert_active_voice_turn_link_pg(&pool, &first)
            .await
            .unwrap()
            .expect("initial upsert inserts");
        assert_eq!(link.dispatch_id.as_deref(), Some("dispatch-first"));

        // Same generation, different dispatch_id → conflict.
        let mut conflict = first.clone();
        conflict.dispatch_id = Some("dispatch-conflict".to_string());
        let result = upsert_active_voice_turn_link_pg(&pool, &conflict)
            .await
            .unwrap();
        assert!(
            result.is_none(),
            "upsert with conflicting dispatch_id must return None, not the old link"
        );

        // The original row must be entirely unchanged.
        let row = lookup_active_voice_turn_link_by_dispatch_id_pg(&pool, "dispatch-first")
            .await
            .unwrap()
            .expect("original dispatch_id still present");
        assert_eq!(
            row.dispatch_id.as_deref(),
            Some("dispatch-first"),
            "original row must not be mutated"
        );

        // Direct attach also returns Conflict, not Unchanged.
        let attach_outcome = attach_voice_turn_link_ids_pg(
            &pool,
            &VoiceTurnLinkIdentityPatch {
                guild_id: 100,
                voice_channel_id: 200,
                utterance_id: "utt-42".to_string(),
                generation: 0,
                announce_message_id: None,
                dispatch_id: Some("dispatch-conflict".to_string()),
                turn_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            attach_outcome,
            AttachOutcome::Conflict,
            "attach with conflicting dispatch_id must return Conflict"
        );

        pool.close().await;
        pg.drop().await;
    }

    /// turn_id global-unique index conflict (attach path): attaching a
    /// turn_id that already belongs to a different row must return Conflict,
    /// not propagate a UniqueViolation panic.
    #[tokio::test]
    async fn turn_id_conflict_on_attach_returns_conflict_not_error_pg() {
        let Some(pg) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg.connect_and_migrate().await;

        // Row A owns "shared-turn".
        let mut row_a = sample_insert(0);
        row_a.utterance_id = "utt-turnid-attach-a".to_string();
        row_a.turn_id = Some("shared-turn-attach".to_string());
        row_a.dispatch_id = Some("dispatch-turnid-a".to_string());
        row_a.announce_message_id = None;
        retarget_voice_turn_link_pg(&pool, &row_a)
            .await
            .unwrap()
            .expect("insert row A");

        // Row B has no turn_id yet.
        let mut row_b = sample_insert(0);
        row_b.utterance_id = "utt-turnid-attach-b".to_string();
        row_b.turn_id = None;
        row_b.dispatch_id = Some("dispatch-turnid-b".to_string());
        row_b.announce_message_id = None;
        retarget_voice_turn_link_pg(&pool, &row_b)
            .await
            .unwrap()
            .expect("insert row B");

        // Trying to attach row A's turn_id to row B must return Conflict.
        let outcome = attach_voice_turn_link_ids_pg(
            &pool,
            &VoiceTurnLinkIdentityPatch {
                guild_id: 100,
                voice_channel_id: 200,
                utterance_id: "utt-turnid-attach-b".to_string(),
                generation: 0,
                announce_message_id: None,
                dispatch_id: None,
                turn_id: Some("shared-turn-attach".to_string()),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            AttachOutcome::Conflict,
            "turn_id already owned by another row must yield Conflict, not UniqueViolation"
        );

        pool.close().await;
        pg.drop().await;
    }
}
