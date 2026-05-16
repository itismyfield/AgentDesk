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
//!   * [`insert_voice_turn_link_pg`] — initial link creation when the voice
//!     turn dispatches to a background text channel.
//!   * [`retarget_voice_turn_link_pg`] — atomic "cancel previous generation,
//!     insert new active generation". Same-generation collisions (simple
//!     retries) are deduped via `ON CONFLICT DO NOTHING`.
//!   * [`lookup_voice_turn_link_by_dispatch_id_pg`] /
//!     [`lookup_voice_turn_link_by_announce_message_id_pg`] — reverse
//!     lookups for call sites that only know one of those ids.
//!   * [`mark_terminal_voice_turn_link_pg`] — flip status when the routed
//!     turn completes (TTS done, run_completed, etc.).
//!   * [`gc_terminal_voice_turn_links_pg`] — leader-only maintenance sweep
//!     for old terminal rows. Active and cancelled rows are intentionally
//!     left in place to preserve audit/lookup behaviour for long-lived
//!     background turns (24h+ runs are normal).
//!
//! This module deliberately ships only the store. Call-site changes
//! (insert / retarget / lookup wiring into the dispatch / barge-in / TTS
//! paths) land in #2364, #2365, #2366 as separate sub-issues of #2164.

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
    pub fn as_str(self) -> &'static str {
        match self {
            VoiceTurnLinkStatus::Active => "active",
            VoiceTurnLinkStatus::Cancelled => "cancelled",
            VoiceTurnLinkStatus::Terminal => "terminal",
        }
    }

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
    pub status: VoiceTurnLinkStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Payload accepted by [`insert_voice_turn_link_pg`] and
/// [`retarget_voice_turn_link_pg`]. Built by call sites once, then passed
/// to whichever helper applies to the situation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceTurnLinkInsert {
    pub guild_id: u64,
    pub voice_channel_id: u64,
    pub background_channel_id: u64,
    pub utterance_id: String,
    pub generation: i32,
    pub announce_message_id: Option<u64>,
    pub dispatch_id: Option<String>,
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
        status,
        created_at: row.get::<DateTime<Utc>, _>("created_at"),
        updated_at: row.get::<DateTime<Utc>, _>("updated_at"),
    }
}

/// SQL `RETURNING` projection used by every helper that yields a
/// [`VoiceTurnLink`]. Kept centralised so column drift is impossible.
const RETURNING_COLUMNS: &str = "id, guild_id, voice_channel_id, background_channel_id, \
    utterance_id, generation, announce_message_id, dispatch_id, status, created_at, updated_at";

/// Insert a new voice turn link as `active`. Idempotent on
/// `(guild_id, voice_channel_id, utterance_id, generation)`: simple retries
/// that supply identical content collide on the unique key and are deduped
/// (`Ok(None)` returned). Conflicting payloads (e.g. different
/// `background_channel_id` for the same key) also return `Ok(None)` —
/// callers that need retarget semantics should use
/// [`retarget_voice_turn_link_pg`].
///
/// Returns the inserted row on success, or `None` on idempotent dedup.
pub async fn insert_voice_turn_link_pg(
    pool: &PgPool,
    insert: &VoiceTurnLinkInsert,
) -> Result<Option<VoiceTurnLink>> {
    let sql = format!(
        "INSERT INTO voice_turn_link (
             guild_id, voice_channel_id, background_channel_id,
             utterance_id, generation, announce_message_id, dispatch_id,
             status, created_at, updated_at
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, 'active', NOW(), NOW())
         ON CONFLICT (guild_id, voice_channel_id, utterance_id, generation)
         DO NOTHING
         RETURNING {RETURNING_COLUMNS}"
    );

    let row = sqlx::query(&sql)
        .bind(u64_to_i64(insert.guild_id))
        .bind(u64_to_i64(insert.voice_channel_id))
        .bind(u64_to_i64(insert.background_channel_id))
        .bind(&insert.utterance_id)
        .bind(insert.generation)
        .bind(insert.announce_message_id.map(u64_to_i64))
        .bind(insert.dispatch_id.as_deref())
        .fetch_optional(pool)
        .await?;

    Ok(row.as_ref().map(row_to_link))
}

/// Atomic retarget: mark every prior `active` row for
/// `(guild_id, voice_channel_id, utterance_id)` as `cancelled`, then insert
/// the new generation as `active`. Wrapped in a single transaction so a
/// crash mid-retarget can never leave two `active` rows for the same
/// utterance.
///
/// If a row with the same `(guild_id, voice_channel_id, utterance_id,
/// generation)` already exists (simple retry — same generation), the
/// insert no-ops via `ON CONFLICT DO NOTHING` and `Ok(None)` is returned.
/// The cancellation pass still runs in that case so it remains a safe
/// "re-apply" operation.
pub async fn retarget_voice_turn_link_pg(
    pool: &PgPool,
    insert: &VoiceTurnLinkInsert,
) -> Result<Option<VoiceTurnLink>> {
    let mut tx = pool.begin().await?;

    // 1. Cancel every prior generation still 'active' for this utterance.
    //    We deliberately exclude rows whose generation == the new
    //    generation so that an idempotent retry against the same generation
    //    does not flip its own row to 'cancelled' before the conflict
    //    check.
    sqlx::query(
        "UPDATE voice_turn_link
            SET status = 'cancelled', updated_at = NOW()
          WHERE guild_id = $1
            AND voice_channel_id = $2
            AND utterance_id = $3
            AND generation <> $4
            AND status = 'active'",
    )
    .bind(u64_to_i64(insert.guild_id))
    .bind(u64_to_i64(insert.voice_channel_id))
    .bind(&insert.utterance_id)
    .bind(insert.generation)
    .execute(&mut *tx)
    .await?;

    // 2. Insert the new generation. Same-key collision (e.g. naive retry of
    //    the same retarget) is deduped via ON CONFLICT DO NOTHING.
    let sql = format!(
        "INSERT INTO voice_turn_link (
             guild_id, voice_channel_id, background_channel_id,
             utterance_id, generation, announce_message_id, dispatch_id,
             status, created_at, updated_at
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, 'active', NOW(), NOW())
         ON CONFLICT (guild_id, voice_channel_id, utterance_id, generation)
         DO NOTHING
         RETURNING {RETURNING_COLUMNS}"
    );
    let inserted = sqlx::query(&sql)
        .bind(u64_to_i64(insert.guild_id))
        .bind(u64_to_i64(insert.voice_channel_id))
        .bind(u64_to_i64(insert.background_channel_id))
        .bind(&insert.utterance_id)
        .bind(insert.generation)
        .bind(insert.announce_message_id.map(u64_to_i64))
        .bind(insert.dispatch_id.as_deref())
        .fetch_optional(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(inserted.as_ref().map(row_to_link))
}

/// Reverse lookup by `dispatch_id`. Returns the most recently updated row
/// matching the dispatch_id; in normal operation there is exactly one
/// because dispatch_id is a globally unique opaque token, but the
/// `ORDER BY updated_at DESC` is defensive against any future scheme where
/// a single dispatch is reused.
pub async fn lookup_voice_turn_link_by_dispatch_id_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<Option<VoiceTurnLink>> {
    let sql = format!(
        "SELECT {RETURNING_COLUMNS}
           FROM voice_turn_link
          WHERE dispatch_id = $1
          ORDER BY updated_at DESC
          LIMIT 1"
    );
    let row = sqlx::query(&sql)
        .bind(dispatch_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(row_to_link))
}

/// Reverse lookup by `announce_message_id`. Same shape as the
/// dispatch_id lookup; primarily used by barge-in cancel resolution when
/// only the announce message anchor is available.
pub async fn lookup_voice_turn_link_by_announce_message_id_pg(
    pool: &PgPool,
    announce_message_id: u64,
) -> Result<Option<VoiceTurnLink>> {
    let sql = format!(
        "SELECT {RETURNING_COLUMNS}
           FROM voice_turn_link
          WHERE announce_message_id = $1
          ORDER BY updated_at DESC
          LIMIT 1"
    );
    let row = sqlx::query(&sql)
        .bind(u64_to_i64(announce_message_id))
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
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(row_to_link))
}

/// GC sweep for old terminal rows. Only `terminal` rows older than
/// `older_than` are deleted; `active` and `cancelled` rows are
/// intentionally preserved because background turns can live 24h+ and the
/// cancelled tombstones support reverse lookup during late reconciliation
/// (e.g. a barge-in event arriving after the retarget already happened).
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
        }
    }

    #[tokio::test]
    async fn insert_voice_turn_link_persists_row_pg() {
        let Some(pg) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg.connect_and_migrate().await;

        let inserted = insert_voice_turn_link_pg(&pool, &sample_insert(0))
            .await
            .unwrap();
        let link = inserted.expect("first insert must return the new row");
        assert_eq!(link.guild_id, 100);
        assert_eq!(link.voice_channel_id, 200);
        assert_eq!(link.background_channel_id, 300);
        assert_eq!(link.utterance_id, "utt-42");
        assert_eq!(link.generation, 0);
        assert_eq!(link.status, VoiceTurnLinkStatus::Active);
        assert_eq!(link.dispatch_id.as_deref(), Some("dispatch-0"));

        // Same-key reinsert is a no-op (idempotent dedup).
        let again = insert_voice_turn_link_pg(&pool, &sample_insert(0))
            .await
            .unwrap();
        assert!(
            again.is_none(),
            "second insert of the same key must be deduped to None"
        );

        pool.close().await;
        pg.drop().await;
    }

    #[tokio::test]
    async fn retarget_cancels_prior_active_and_inserts_new_pg() {
        let Some(pg) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg.connect_and_migrate().await;

        insert_voice_turn_link_pg(&pool, &sample_insert(0))
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
        let prior = lookup_voice_turn_link_by_dispatch_id_pg(&pool, "dispatch-0")
            .await
            .unwrap()
            .expect("prior row still queryable");
        assert_eq!(prior.generation, 0);
        assert_eq!(prior.status, VoiceTurnLinkStatus::Cancelled);

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

        let row = lookup_voice_turn_link_by_dispatch_id_pg(&pool, "dispatch-0")
            .await
            .unwrap()
            .expect("row exists");
        assert_eq!(row.status, VoiceTurnLinkStatus::Active);
        assert_eq!(row.generation, 0);

        pool.close().await;
        pg.drop().await;
    }

    #[tokio::test]
    async fn lookup_by_announce_message_id_pg() {
        let Some(pg) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg.connect_and_migrate().await;

        let mut insert = sample_insert(0);
        insert.announce_message_id = Some(123_456_789);
        insert_voice_turn_link_pg(&pool, &insert).await.unwrap();

        let found = lookup_voice_turn_link_by_announce_message_id_pg(&pool, 123_456_789)
            .await
            .unwrap()
            .expect("row found by announce_message_id");
        assert_eq!(found.utterance_id, "utt-42");

        let missing = lookup_voice_turn_link_by_announce_message_id_pg(&pool, 999_999)
            .await
            .unwrap();
        assert!(missing.is_none());

        pool.close().await;
        pg.drop().await;
    }

    #[tokio::test]
    async fn mark_terminal_updates_status_pg() {
        let Some(pg) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg.connect_and_migrate().await;

        insert_voice_turn_link_pg(&pool, &sample_insert(0))
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
        insert_voice_turn_link_pg(&pool, &active).await.unwrap();

        // Cancelled row — must survive GC (long-lived background turn
        // tombstone preserved for late lookups).
        let mut cancelled = sample_insert(0);
        cancelled.utterance_id = "utt-cancelled".to_string();
        cancelled.dispatch_id = Some("dispatch-cancelled".to_string());
        insert_voice_turn_link_pg(&pool, &cancelled).await.unwrap();
        let mut cancelled_next = cancelled.clone();
        cancelled_next.generation = 1;
        cancelled_next.dispatch_id = Some("dispatch-cancelled-next".to_string());
        cancelled_next.announce_message_id = Some(9991);
        retarget_voice_turn_link_pg(&pool, &cancelled_next)
            .await
            .unwrap();

        // Terminal row — eligible for GC.
        let mut terminal = sample_insert(0);
        terminal.utterance_id = "utt-terminal".to_string();
        terminal.dispatch_id = Some("dispatch-terminal".to_string());
        insert_voice_turn_link_pg(&pool, &terminal).await.unwrap();
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
        let active_row = lookup_voice_turn_link_by_dispatch_id_pg(&pool, "dispatch-active")
            .await
            .unwrap();
        assert!(active_row.is_some(), "active row survives GC");
        assert_eq!(active_row.unwrap().status, VoiceTurnLinkStatus::Active);

        let cancelled_row = lookup_voice_turn_link_by_dispatch_id_pg(&pool, "dispatch-cancelled")
            .await
            .unwrap();
        assert!(cancelled_row.is_some(), "cancelled tombstone survives GC");
        assert_eq!(
            cancelled_row.unwrap().status,
            VoiceTurnLinkStatus::Cancelled
        );

        // Terminal row is gone.
        let terminal_row = lookup_voice_turn_link_by_dispatch_id_pg(&pool, "dispatch-terminal")
            .await
            .unwrap();
        assert!(terminal_row.is_none(), "terminal row deleted by GC");

        // Young terminal rows (after cutoff) must not be deleted. Add a
        // fresh terminal row and rerun GC.
        let mut fresh = sample_insert(0);
        fresh.utterance_id = "utt-fresh-terminal".to_string();
        fresh.dispatch_id = Some("dispatch-fresh".to_string());
        insert_voice_turn_link_pg(&pool, &fresh).await.unwrap();
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
}
