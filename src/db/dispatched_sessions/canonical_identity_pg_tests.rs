use super::{
    CanonicalSessionIdentity, HookSessionUpsertError, SessionIdentityKind,
    upsert_hook_session_with_identity_pg,
};
use crate::db::dispatched_sessions::HookSessionUpsert;

struct CanonicalIdentityPgDatabase {
    _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
    admin_url: String,
    database_name: String,
    database_url: String,
}

impl CanonicalIdentityPgDatabase {
    async fn create() -> Option<Self> {
        let base = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE")
            .ok()
            .filter(|value| !value.trim().is_empty())?;
        let lifecycle = crate::db::postgres::lock_test_lifecycle();
        let base = base.trim().trim_end_matches('/').to_string();
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        let admin_url = format!("{base}/{admin_db}");
        let database_name = format!(
            "agentdesk_canonical_identity_{}",
            uuid::Uuid::new_v4().simple()
        );
        let database_url = format!("{base}/{database_name}");
        crate::db::postgres::create_test_database(
            &admin_url,
            &database_name,
            "canonical identity pg",
        )
        .await
        .expect("create canonical identity postgres test db");
        Some(Self {
            _lifecycle: lifecycle,
            admin_url,
            database_name,
            database_url,
        })
    }

    async fn migrate(&self) -> sqlx::PgPool {
        crate::db::postgres::connect_test_pool_and_migrate(
            &self.database_url,
            "canonical identity pg",
        )
        .await
        .expect("connect + migrate canonical identity postgres test db")
    }

    async fn drop(self) {
        crate::db::postgres::drop_test_database(
            &self.admin_url,
            &self.database_name,
            "canonical identity pg",
        )
        .await
        .expect("drop canonical identity postgres test db");
    }
}

fn params<'a>(key: &'a str, channel_id: &'a str) -> HookSessionUpsert<'a> {
    HookSessionUpsert {
        session_key: key,
        instance_id: Some("test-node"),
        agent_id: None,
        provider: "claude",
        status: "idle",
        session_info: None,
        model: None,
        tokens: None,
        cwd: None,
        active_dispatch_id: None,
        thread_channel_id: None,
        channel_id: Some(channel_id),
        claude_session_id: None,
        raw_provider_session_id: None,
        turn_start_nonce: None,
        dispatched_origin: false,
    }
}

fn identity<'a>(channel_id: &'a str) -> CanonicalSessionIdentity<'a> {
    CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: "discord_0123456789abcdef",
        channel_id,
    }
}

#[tokio::test]
async fn canonical_identity_concurrent_upsert_and_alias_resolution_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let first_key = "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-same-name";
    let second_key = "claude/discord_0123456789abcdef/host-b:AgentDesk-claude-same-name";
    let channel_id = "1490141479707086938";

    let (first, second) = tokio::join!(
        upsert_hook_session_with_identity_pg(
            &pool,
            params(first_key, channel_id),
            Some(identity(channel_id))
        ),
        upsert_hook_session_with_identity_pg(
            &pool,
            params(second_key, channel_id),
            Some(identity(channel_id))
        ),
    );
    let first = first.expect("first canonical upsert");
    let second = second.expect("second canonical upsert");
    assert_eq!(first.inserted as u8 + second.inserted as u8, 1);
    assert_eq!(first.session_key, second.session_key);

    let row_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions
         WHERE provider = 'claude'
           AND discord_token_hash = 'discord_0123456789abcdef'
           AND channel_id = $1
           AND identity_kind = 'discord_channel'",
    )
    .bind(channel_id)
    .fetch_one(&pool)
    .await
    .expect("count canonical rows");
    assert_eq!(row_count, 1);

    let alias_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_key_aliases
         WHERE session_key IN ($1, $2)",
    )
    .bind(first_key)
    .bind(second_key)
    .fetch_one(&pool)
    .await
    .expect("count locator aliases");
    assert_eq!(alias_count, 1);

    for locator in [first_key, second_key] {
        let resolved = super::resolve_session_key_pg(&pool, locator)
            .await
            .expect("resolve primary or alias locator");
        assert_eq!(resolved.as_deref(), Some(first.session_key.as_str()));
    }

    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_ambiguous_legacy_rows_are_untouched_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let channel_id = "1480015244062490774";
    for host in ["host-a", "host-b"] {
        let key = format!("claude/discord_0123456789abcdef/{host}:AgentDesk-claude-collision");
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, channel_id)
             VALUES ($1, 'claude', 'disconnected', $2)",
        )
        .bind(key)
        .bind(channel_id)
        .execute(&pool)
        .await
        .expect("seed ambiguous legacy row");
    }

    let error = upsert_hook_session_with_identity_pg(
        &pool,
        params(
            "claude/discord_0123456789abcdef/host-c:AgentDesk-claude-collision",
            channel_id,
        ),
        Some(identity(channel_id)),
    )
    .await
    .expect_err("ambiguous legacy rows must fail closed");
    assert!(matches!(error, HookSessionUpsertError::Conflict(_)));

    let untouched: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions
         WHERE channel_id = $1
           AND identity_kind IS NULL
           AND discord_token_hash IS NULL",
    )
    .bind(channel_id)
    .fetch_one(&pool)
    .await
    .expect("count untouched ambiguous rows");
    assert_eq!(untouched, 2);

    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_safe_legacy_promotion_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let old_key = "claude/discord_0123456789abcdef/old-host:AgentDesk-claude-promote";
    let new_key = "claude/discord_0123456789abcdef/new-host:AgentDesk-claude-promote";
    let channel_id = "1479671301387059200";
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, channel_id)
         VALUES ($1, 'claude', 'disconnected', $2)",
    )
    .bind(old_key)
    .bind(channel_id)
    .execute(&pool)
    .await
    .expect("seed unique legacy row");

    let outcome = upsert_hook_session_with_identity_pg(
        &pool,
        params(new_key, channel_id),
        Some(identity(channel_id)),
    )
    .await
    .expect("promote unique legacy row");
    assert!(!outcome.inserted);
    assert_eq!(outcome.session_key, old_key);

    let promoted: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT identity_kind, discord_token_hash FROM sessions WHERE session_key = $1",
    )
    .bind(old_key)
    .fetch_one(&pool)
    .await
    .expect("load promoted identity");
    assert_eq!(promoted.0.as_deref(), Some("discord_channel"));
    assert_eq!(promoted.1.as_deref(), Some("discord_0123456789abcdef"));

    let alias_target: String = sqlx::query_scalar(
        "SELECT s.session_key FROM session_key_aliases a
         JOIN sessions s ON s.id = a.session_id
         WHERE a.session_key = $1",
    )
    .bind(new_key)
    .fetch_one(&pool)
    .await
    .expect("load preserved locator alias");
    assert_eq!(alias_target, old_key);

    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_locator_collision_never_reassigns_channel_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let colliding_key =
        "claude/discord_0123456789abcdef/same-host:AgentDesk-claude-sanitized-or-truncated";
    let first_channel_id = "1479671301387059210";
    let second_channel_id = "1479671301387059211";

    upsert_hook_session_with_identity_pg(
        &pool,
        params(colliding_key, first_channel_id),
        Some(identity(first_channel_id)),
    )
    .await
    .expect("seed first canonical channel");
    let error = upsert_hook_session_with_identity_pg(
        &pool,
        params(colliding_key, second_channel_id),
        Some(identity(second_channel_id)),
    )
    .await
    .expect_err("same locator must not be reassigned to another channel");
    assert!(matches!(error, HookSessionUpsertError::Conflict(_)));

    let owner: (String, String) = sqlx::query_as(
        "SELECT channel_id, discord_token_hash FROM sessions WHERE session_key = $1",
    )
    .bind(colliding_key)
    .fetch_one(&pool)
    .await
    .expect("load collision owner");
    assert_eq!(owner.0, first_channel_id);
    assert_eq!(owner.1, "discord_0123456789abcdef");

    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_resolver_requires_convergent_exact_alias_and_canonical_evidence_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let canonical_key = "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-canonical-owner";
    let conflicting_key =
        "claude/discord_fedcba9876543210/host-b:AgentDesk-claude-conflicting-owner";
    let channel_id = "1479671301387059201";

    upsert_hook_session_with_identity_pg(
        &pool,
        params(canonical_key, channel_id),
        Some(identity(channel_id)),
    )
    .await
    .expect("seed canonical owner");
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, channel_id)
         VALUES ($1, 'claude', 'disconnected', '999')",
    )
    .bind(conflicting_key)
    .execute(&pool)
    .await
    .expect("seed conflicting exact locator");

    let resolved = super::resolve_session_key_with_identity_pg(
        &pool,
        "missing-exact-locator",
        Some("claude"),
        Some(identity(channel_id)),
    )
    .await
    .expect("unique canonical fallback resolves");
    assert_eq!(resolved.as_deref(), Some(canonical_key));

    let error = super::resolve_session_key_with_identity_pg(
        &pool,
        conflicting_key,
        Some("claude"),
        Some(identity(channel_id)),
    )
    .await
    .expect_err("conflicting exact and canonical evidence must fail closed");
    assert!(matches!(error, HookSessionUpsertError::Conflict(_)));

    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_old_alias_hook_updates_one_row_without_duplicate_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let primary_key = "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-mixed-version";
    let alias_key = "claude/discord_0123456789abcdef/host-b:AgentDesk-claude-mixed-version";
    let channel_id = "1479671301387059202";

    upsert_hook_session_with_identity_pg(
        &pool,
        params(primary_key, channel_id),
        Some(identity(channel_id)),
    )
    .await
    .expect("seed canonical owner");
    upsert_hook_session_with_identity_pg(
        &pool,
        params(alias_key, channel_id),
        Some(identity(channel_id)),
    )
    .await
    .expect("preserve alternate locator alias");

    let outcome = upsert_hook_session_with_identity_pg(
        &pool,
        HookSessionUpsert {
            status: "awaiting_user",
            ..params(alias_key, channel_id)
        },
        None,
    )
    .await
    .expect("old binary alias hook resolves existing row");
    assert!(!outcome.inserted);
    assert_eq!(outcome.session_key, primary_key);

    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions
         WHERE session_key IN ($1, $2)",
    )
    .bind(primary_key)
    .bind(alias_key)
    .fetch_one(&pool)
    .await
    .expect("count mixed-version rows");
    assert_eq!(rows, 1);
    let status: String = sqlx::query_scalar("SELECT status FROM sessions WHERE session_key = $1")
        .bind(primary_key)
        .fetch_one(&pool)
        .await
        .expect("load mixed-version target status");
    assert_eq!(status, "awaiting_user");

    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_provider_token_thread_and_scheduled_dimensions_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let parent_id = "1479671301387059203";
    let thread_id = "1479671301387059204";
    let same_name = "AgentDesk-claude-name-collision";
    let first_key = format!("claude/discord_0123456789abcdef/host-a:{same_name}");
    let thread_key = format!("claude/discord_0123456789abcdef/host-b:{same_name}");
    let token_key = format!("claude/discord_fedcba9876543210/host-c:{same_name}");
    let provider_key = format!("codex/discord_0123456789abcdef/host-d:{same_name}");
    let scheduled_key = format!("claude/discord_0123456789abcdef/host-e:{same_name}");

    upsert_hook_session_with_identity_pg(
        &pool,
        params(&first_key, parent_id),
        Some(identity(parent_id)),
    )
    .await
    .expect("insert parent channel");
    upsert_hook_session_with_identity_pg(
        &pool,
        params(&thread_key, thread_id),
        Some(identity(thread_id)),
    )
    .await
    .expect("insert exact thread snowflake");

    let different_token = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: "discord_fedcba9876543210",
        channel_id: parent_id,
    };
    upsert_hook_session_with_identity_pg(
        &pool,
        params(&token_key, parent_id),
        Some(different_token),
    )
    .await
    .expect("same channel under another bot token is distinct");

    let mut codex_params = params(&provider_key, parent_id);
    codex_params.provider = "codex";
    upsert_hook_session_with_identity_pg(&pool, codex_params, Some(identity(parent_id)))
        .await
        .expect("same channel under another provider is distinct");

    let scheduled = CanonicalSessionIdentity {
        kind: SessionIdentityKind::ScheduledSnapshot,
        discord_token_hash: "discord_0123456789abcdef",
        channel_id: parent_id,
    };
    upsert_hook_session_with_identity_pg(&pool, params(&scheduled_key, parent_id), Some(scheduled))
        .await
        .expect("scheduled snapshot is outside ordinary uniqueness");

    let ordinary_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE identity_kind = 'discord_channel'")
            .fetch_one(&pool)
            .await
            .expect("count ordinary canonical rows");
    assert_eq!(ordinary_count, 4);
    let scheduled_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions WHERE identity_kind = 'scheduled_snapshot'",
    )
    .fetch_one(&pool)
    .await
    .expect("count explicit scheduled rows");
    assert_eq!(scheduled_count, 1);

    test_db.drop().await;
}

#[test]
fn canonical_identity_conflict_is_http_409_ready() {
    let error = super::hook_session_upsert_error_to_app_error(HookSessionUpsertError::Conflict(
        "ambiguous canonical identity".to_string(),
    ));
    assert_eq!(error.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(error.code(), crate::error::ErrorCode::Conflict);
}

#[tokio::test]
async fn canonical_identity_migration_backfills_only_unique_legacy_tuple_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let unique_id: i64 = sqlx::query_scalar(
        "INSERT INTO sessions (session_key, provider, status, channel_id)
         VALUES ('claude/discord_aaaaaaaaaaaaaaaa/host:AgentDesk-claude-unique',
                 'claude', 'disconnected', '101') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("seed unique legacy row after migration");
    let scheduled_id: i64 = sqlx::query_scalar(
        "INSERT INTO sessions (session_key, provider, status, channel_id)
         VALUES ('claude/discord_aaaaaaaaaaaaaaaa/host:AgentDesk-claude-scheduled-smsg_abc',
                 'claude', 'disconnected', '303') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("seed scheduled snapshot legacy row after migration");
    for host in ["host-a", "host-b"] {
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, channel_id)
             VALUES ($1, 'claude', 'disconnected', '202')",
        )
        .bind(format!(
            "claude/discord_bbbbbbbbbbbbbbbb/{host}:AgentDesk-claude-ambiguous"
        ))
        .execute(&pool)
        .await
        .expect("seed ambiguous legacy row after migration");
    }

    let migration =
        include_str!("../../../migrations/postgres/0100_canonical_discord_session_identity.sql");
    let backfill = migration
        .split("WITH eligible AS (")
        .nth(1)
        .expect("0100 migration contains backfill");
    sqlx::raw_sql(&format!("WITH eligible AS ({backfill}"))
        .execute(&pool)
        .await
        .expect("rerun idempotent migration backfill");

    let unique: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT identity_kind, discord_token_hash FROM sessions WHERE id = $1")
            .bind(unique_id)
            .fetch_one(&pool)
            .await
            .expect("load unique backfill row");
    assert_eq!(unique.0.as_deref(), Some("discord_channel"));
    assert_eq!(unique.1.as_deref(), Some("discord_aaaaaaaaaaaaaaaa"));

    let scheduled: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT identity_kind, discord_token_hash FROM sessions WHERE id = $1")
            .bind(scheduled_id)
            .fetch_one(&pool)
            .await
            .expect("load scheduled snapshot legacy row");
    assert_eq!(scheduled, (None, None));

    let ambiguous_promoted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions
         WHERE channel_id = '202' AND identity_kind IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("count ambiguous promoted rows");
    assert_eq!(ambiguous_promoted, 0);

    test_db.drop().await;
}
