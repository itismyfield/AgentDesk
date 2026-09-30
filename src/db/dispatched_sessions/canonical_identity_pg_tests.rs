use super::{
    CanonicalSessionIdentity, HookSessionActorPin, HookSessionUpsertError,
    SessionIdentityConflictKind, SessionIdentityKind, capture_hook_session_actor_pin_pg,
    upsert_hook_session_with_actor_pin_pg, upsert_hook_session_with_identity_pg,
};
use crate::db::dispatched_sessions::hosted_execution::tests::{
    TOKEN, expected, future_schema, location, owner, pending, record, wire,
};
use crate::db::dispatched_sessions::hosted_execution::*;
use crate::db::dispatched_sessions::{
    HookSessionUpsert, cleanup_disconnected_sessions_pg, clear_session_id_by_key_pg,
    delete_session_by_key_pg, disconnect_stale_fixed_session_by_key_pg,
    gc_stale_thread_sessions_with_probe_pg, load_force_kill_session_pg,
    load_provider_session_ids_pg, load_session_rebind_context_pg, rebind_session_provider_pg,
    refresh_session_heartbeat_by_key_to_unix_nanos_pg, session_last_seen_unix_nanos_pg,
    update_raw_provider_transcript_len_watermark_pg,
};
use crate::services::platform::tmux::SessionPresence;
use serde_json::{Value, json};
use sqlx::PgPool;

struct CanonicalIdentityPgDatabase {
    _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
    admin_url: String,
    database_name: String,
    database_url: String,
}

impl CanonicalIdentityPgDatabase {
    async fn create() -> Option<Self> {
        let base = crate::db::postgres::postgres_test_database_url_base()?;
        let lifecycle = crate::db::postgres::lock_test_lifecycle();
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

async fn actor_pin_session_snapshot(pool: &sqlx::PgPool, key: &str) -> serde_json::Value {
    sqlx::query_scalar("SELECT to_jsonb(s) FROM sessions s WHERE session_key = $1")
        .bind(key)
        .fetch_one(pool)
        .await
        .expect("snapshot every session column")
}

#[tokio::test]
async fn canonical_identity_actor_pin_missing_insert_and_retry_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        return;
    };
    let pool = test_db.migrate().await;
    let key = "claude/actor-pin-missing";
    let pin = capture_hook_session_actor_pin_pg(&pool, key).await.unwrap();
    assert_eq!(pin, HookSessionActorPin::Missing);
    assert!(!pin.active_for_other_actor(Some("actor-a")));
    for nonce in [None, Some(" \t")] {
        let mut invalid = params(key, "101");
        invalid.turn_start_nonce = nonce;
        assert_eq!(
            upsert_hook_session_with_actor_pin_pg(&pool, invalid, &pin)
                .await
                .unwrap_err()
                .conflict_kind(),
            Some(SessionIdentityConflictKind::OwnershipMismatch),
        );
        assert_eq!(
            capture_hook_session_actor_pin_pg(&pool, key).await.unwrap(),
            pin
        );
    }
    let mut actor = params(key, "101");
    actor.status = "turn_active";
    actor.turn_start_nonce = Some("actor-a");
    let committed = upsert_hook_session_with_actor_pin_pg(&pool, actor, &pin)
        .await
        .unwrap();
    assert!(!committed.active_for_other_actor(Some("actor-a")));
    assert!(committed.active_for_other_actor(Some("actor-b")));
    assert!(committed.active_for_other_actor(None));
    assert_eq!(
        committed,
        capture_hook_session_actor_pin_pg(&pool, key).await.unwrap()
    );
    let row = actor_pin_session_snapshot(&pool, key).await;
    assert_eq!(row["status"], "turn_active");
    assert_eq!(row["active_turn_nonce"], "actor-a");
    assert!(row["dispatched_origin_turn_nonce"].is_null());
    let mut retry = params(key, "101");
    retry.status = "turn_active";
    retry.turn_start_nonce = Some("actor-a");
    assert_eq!(
        upsert_hook_session_with_actor_pin_pg(&pool, retry, &committed)
            .await
            .unwrap(),
        committed,
    );
    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_actor_pin_nullable_and_prior_nonce_alias_update_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        return;
    };
    let pool = test_db.migrate().await;
    let key = "claude/actor-pin-primary";
    let alias = "claude/actor-pin-alias";
    for previous in [None, Some("finished-actor")] {
        let mut seed = params(key, "102");
        seed.turn_start_nonce = previous;
        upsert_hook_session_with_identity_pg(&pool, seed, Some(identity("102")))
            .await
            .unwrap();
        upsert_hook_session_with_identity_pg(&pool, params(alias, "102"), Some(identity("102")))
            .await
            .unwrap();
        if previous.is_none() {
            sqlx::query("UPDATE sessions SET status = NULL WHERE session_key = $1")
                .bind(key)
                .execute(&pool)
                .await
                .unwrap();
        }
        let before = actor_pin_session_snapshot(&pool, key).await;
        let pin = capture_hook_session_actor_pin_pg(&pool, alias)
            .await
            .unwrap();
        assert_eq!(
            pin,
            capture_hook_session_actor_pin_pg(&pool, key).await.unwrap()
        );
        assert!(
            matches!(&pin, HookSessionActorPin::Existing { active_turn_nonce, .. }
            if active_turn_nonce.as_deref() == previous)
        );
        assert_eq!(
            actor_pin_session_snapshot(&pool, key).await,
            before,
            "capture is read-only"
        );
        let mut actor = params(alias, "102");
        actor.status = "turn_active";
        actor.turn_start_nonce = Some("actor-a");
        let committed = upsert_hook_session_with_actor_pin_pg(&pool, actor, &pin)
            .await
            .unwrap();
        assert_eq!(
            committed,
            capture_hook_session_actor_pin_pg(&pool, key).await.unwrap()
        );
        let after = actor_pin_session_snapshot(&pool, key).await;
        assert_eq!(after["id"], before["id"]);
        assert_eq!(after["session_key"], key);
        assert_eq!(after["active_turn_nonce"], "actor-a");
        assert_eq!(after["status"], "turn_active");
    }
    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_actor_pin_rejects_successor_nonce_and_row_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        return;
    };
    let pool = test_db.migrate().await;
    let key = "claude/actor-pin-successor";
    let mut predecessor = params(key, "103");
    predecessor.turn_start_nonce = Some("observed-actor");
    upsert_hook_session_with_identity_pg(&pool, predecessor, None)
        .await
        .unwrap();
    let pin = capture_hook_session_actor_pin_pg(&pool, key).await.unwrap();
    // A status-only race also rejects; a successor's later idle state is no bypass.
    for (status, nonce) in [
        ("turn_active", "observed-actor"),
        ("turn_active", "actor-b"),
        ("idle", "actor-b"),
    ] {
        let mut successor = params(key, "103");
        successor.status = status;
        successor.turn_start_nonce = Some(nonce);
        successor.dispatched_origin = true;
        successor.model = Some("successor-model");
        upsert_hook_session_with_identity_pg(&pool, successor, None)
            .await
            .unwrap();
        let before = actor_pin_session_snapshot(&pool, key).await;
        assert_eq!(before["active_turn_nonce"], nonce);
        assert_eq!(before["dispatched_origin_turn_nonce"], nonce);
        let mut stale_actor = params(key, "103");
        stale_actor.status = "turn_active";
        stale_actor.turn_start_nonce = Some("actor-a");
        assert_eq!(
            upsert_hook_session_with_actor_pin_pg(&pool, stale_actor, &pin)
                .await
                .unwrap_err()
                .conflict_kind(),
            Some(SessionIdentityConflictKind::OwnershipMismatch)
        );
        assert_eq!(actor_pin_session_snapshot(&pool, key).await, before);
    }
    sqlx::query("DELETE FROM sessions WHERE session_key = $1")
        .bind(key)
        .execute(&pool)
        .await
        .unwrap();
    let mut replacement = params(key, "103");
    replacement.turn_start_nonce = Some("observed-actor");
    upsert_hook_session_with_identity_pg(&pool, replacement, None)
        .await
        .unwrap();
    let before = actor_pin_session_snapshot(&pool, key).await;
    let mut stale_actor = params(key, "103");
    stale_actor.turn_start_nonce = Some("actor-a");
    assert_eq!(
        upsert_hook_session_with_actor_pin_pg(&pool, stale_actor, &pin)
            .await
            .unwrap_err()
            .conflict_kind(),
        Some(SessionIdentityConflictKind::OwnershipMismatch)
    );
    assert_eq!(
        actor_pin_session_snapshot(&pool, key).await,
        before,
        "same nonce on a replacement row is foreign"
    );
    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_actor_pin_missing_rejects_intervening_insert_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        return;
    };
    let pool = test_db.migrate().await;
    let key = "claude/actor-pin-insert-race";
    let pin = capture_hook_session_actor_pin_pg(&pool, key).await.unwrap();
    assert_eq!(pin, HookSessionActorPin::Missing);
    let mut successor = params(key, "104");
    successor.status = "turn_active";
    successor.turn_start_nonce = Some("actor-b");
    successor.dispatched_origin = true;
    upsert_hook_session_with_identity_pg(&pool, successor, None)
        .await
        .unwrap();
    let before = actor_pin_session_snapshot(&pool, key).await;
    let mut stale_actor = params(key, "104");
    stale_actor.turn_start_nonce = Some("actor-a");
    assert_eq!(
        upsert_hook_session_with_actor_pin_pg(&pool, stale_actor, &pin)
            .await
            .unwrap_err()
            .conflict_kind(),
        Some(SessionIdentityConflictKind::OwnershipMismatch)
    );
    assert_eq!(actor_pin_session_snapshot(&pool, key).await, before);
    test_db.drop().await;
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
    assert_eq!(
        error.conflict_kind(),
        Some(SessionIdentityConflictKind::AmbiguousLegacy)
    );

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
async fn canonical_identity_promotes_unique_operational_legacy_row_among_stale_shells_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let channel_id = "1480015244062490888";
    let live_key = "claude/discord_0123456789abcdef/mac-book:AgentDesk-claude-live-survivor";
    let stale_key = "claude/discord_0123456789abcdef/mac-mini:AgentDesk-claude-live-survivor";
    sqlx::query(
        "INSERT INTO sessions (
             session_key, provider, status, channel_id, raw_provider_session_id
         ) VALUES ($1, 'claude', 'idle', $3, 'provider-session-current'),
                  ($2, 'claude', 'disconnected', $3, NULL)",
    )
    .bind(live_key)
    .bind(stale_key)
    .bind(channel_id)
    .execute(&pool)
    .await
    .expect("seed operational and stale legacy rows");

    let outcome = upsert_hook_session_with_identity_pg(
        &pool,
        params(live_key, channel_id),
        Some(identity(channel_id)),
    )
    .await
    .expect("promote the unique operational legacy row");
    assert!(!outcome.inserted);
    assert_eq!(outcome.session_key, live_key);

    let promoted: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT identity_kind, discord_token_hash
         FROM sessions WHERE session_key = $1",
    )
    .bind(live_key)
    .fetch_one(&pool)
    .await
    .expect("load promoted survivor");
    assert_eq!(promoted.0.as_deref(), Some("discord_channel"));
    assert_eq!(promoted.1.as_deref(), Some("discord_0123456789abcdef"));

    let stale_identity: Option<String> =
        sqlx::query_scalar("SELECT identity_kind FROM sessions WHERE session_key = $1")
            .bind(stale_key)
            .fetch_one(&pool)
            .await
            .expect("load untouched stale shell");
    assert_eq!(stale_identity, None);

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
async fn canonical_identity_scheduled_legacy_row_is_not_promoted_as_ordinary_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let scheduled_key =
        "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-scheduled-smsg_shared";
    let ordinary_key = "claude/discord_0123456789abcdef/host-b:AgentDesk-claude-ordinary-shared";
    let channel_id = "1479671301387059205";
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, channel_id)
         VALUES ($1, 'claude', 'disconnected', $2)",
    )
    .bind(scheduled_key)
    .bind(channel_id)
    .execute(&pool)
    .await
    .expect("seed scheduled legacy row");

    let outcome = upsert_hook_session_with_identity_pg(
        &pool,
        params(ordinary_key, channel_id),
        Some(identity(channel_id)),
    )
    .await
    .expect("ordinary identity inserts beside scheduled legacy row");
    assert!(outcome.inserted);
    assert_eq!(outcome.session_key, ordinary_key);

    let scheduled: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT identity_kind, discord_token_hash FROM sessions WHERE session_key = $1",
    )
    .bind(scheduled_key)
    .fetch_one(&pool)
    .await
    .expect("load scheduled legacy row");
    assert_eq!(scheduled, (None, None));
    let ordinary_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions
         WHERE provider = 'claude'
           AND discord_token_hash = 'discord_0123456789abcdef'
           AND channel_id = $1
           AND identity_kind = 'discord_channel'",
    )
    .bind(channel_id)
    .fetch_one(&pool)
    .await
    .expect("count ordinary row");
    assert_eq!(ordinary_count, 1);

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
    assert_eq!(
        error.conflict_kind(),
        Some(SessionIdentityConflictKind::OwnershipMismatch)
    );

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
    assert_eq!(
        error.conflict_kind(),
        Some(SessionIdentityConflictKind::EvidenceDivergence)
    );

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
async fn canonical_identity_alias_provider_resume_read_and_write_seams_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let primary_key = "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-resume-primary";
    let alias_key = "claude/discord_0123456789abcdef/host-b:AgentDesk-claude-resume-primary";
    let channel_id = "1479671301387059206";
    let resume_id = "4c474e5d-37e7-4b6a-bcf7-d68854a31c49";
    let rebound_id = "2d941d6e-a582-4a2d-8fc4-f61b876f2bf2";

    upsert_hook_session_with_identity_pg(
        &pool,
        HookSessionUpsert {
            claude_session_id: Some(resume_id),
            raw_provider_session_id: Some(resume_id),
            cwd: Some("/before"),
            ..params(primary_key, channel_id)
        },
        Some(identity(channel_id)),
    )
    .await
    .expect("seed canonical resume owner");
    upsert_hook_session_with_identity_pg(
        &pool,
        params(alias_key, channel_id),
        Some(identity(channel_id)),
    )
    .await
    .expect("preserve resume alias");

    let force_kill = load_force_kill_session_pg(&pool, alias_key, Some("claude"))
        .await
        .expect("load force-kill metadata through alias")
        .expect("aliased force-kill session row");
    assert_eq!(force_kill.3.as_deref(), Some("claude"));

    let ids = load_provider_session_ids_pg(&pool, alias_key, Some("claude"))
        .await
        .expect("load selectors through alias")
        .expect("aliased session row");
    assert_eq!(ids.resolved_session_key, primary_key);
    assert_eq!(ids.claude_session_id.as_deref(), Some(resume_id));
    assert_eq!(ids.raw_provider_session_id.as_deref(), Some(resume_id));
    assert_eq!(
        update_raw_provider_transcript_len_watermark_pg(
            &pool,
            &ids.resolved_session_key,
            Some("claude"),
            resume_id,
            42,
        )
        .await
        .expect("record watermark on resolved owner"),
        1
    );

    let context = load_session_rebind_context_pg(&pool, alias_key)
        .await
        .expect("load rebind context through alias")
        .expect("rebind context");
    assert_eq!(context.resolved_session_key, primary_key);
    assert!(context.session_id > 0);
    assert_eq!(context.cwd.as_deref(), Some("/before"));
    assert_eq!(context.claude_session_id.as_deref(), Some(resume_id));
    assert_eq!(
        rebind_session_provider_pg(&pool, alias_key, "/after", rebound_id)
            .await
            .expect("rebind through alias"),
        1
    );
    let rebound: (Option<String>, Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT cwd, claude_session_id, raw_provider_transcript_len_watermark
         FROM sessions WHERE session_key = $1",
    )
    .bind(primary_key)
    .fetch_one(&pool)
    .await
    .expect("load rebound primary");
    assert_eq!(rebound.0.as_deref(), Some("/after"));
    assert_eq!(rebound.1.as_deref(), Some(rebound_id));
    assert_eq!(rebound.2, Some(42));
    let previous_last_seen = session_last_seen_unix_nanos_pg(&pool, alias_key)
        .await
        .expect("load aliased last-seen timestamp");
    assert!(
        refresh_session_heartbeat_by_key_to_unix_nanos_pg(
            &pool,
            alias_key,
            previous_last_seen + 1_000_000_000,
        )
        .await
    );
    let refreshed_last_seen = session_last_seen_unix_nanos_pg(&pool, primary_key)
        .await
        .expect("load refreshed primary timestamp");
    assert!(refreshed_last_seen >= previous_last_seen + 1_000_000_000);

    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_alias_clear_and_backlog_transition_target_primary_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let primary_key = "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-clear-primary";
    let alias_key = "claude/discord_0123456789abcdef/host-b:AgentDesk-claude-clear-primary";
    let channel_id = "1479671301387059215";

    upsert_hook_session_with_identity_pg(
        &pool,
        HookSessionUpsert {
            status: "turn_active",
            active_dispatch_id: Some("dispatch-clear-alias"),
            claude_session_id: Some("selector-clear-alias"),
            raw_provider_session_id: Some("raw-clear-alias"),
            ..params(primary_key, channel_id)
        },
        Some(identity(channel_id)),
    )
    .await
    .expect("seed clear owner");
    upsert_hook_session_with_identity_pg(
        &pool,
        params(alias_key, channel_id),
        Some(identity(channel_id)),
    )
    .await
    .expect("preserve clear alias");

    assert_eq!(
        clear_session_id_by_key_pg(&pool, alias_key)
            .await
            .expect("clear provider selectors through alias"),
        1
    );
    let selectors: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT claude_session_id, raw_provider_session_id
         FROM sessions WHERE session_key = $1",
    )
    .bind(primary_key)
    .fetch_one(&pool)
    .await
    .expect("load cleared primary selectors");
    assert_eq!(selectors, (None, None));

    sqlx::query(
        "UPDATE sessions
         SET status = 'turn_active',
             active_dispatch_id = 'dispatch-clear-alias',
             claude_session_id = 'selector-clear-alias'
         WHERE session_key = $1",
    )
    .bind(primary_key)
    .execute(&pool)
    .await
    .expect("restore backlog target state");
    crate::db::kanban_cards::clear_session_for_turn_target_pg(&pool, alias_key)
        .await
        .expect("clear backlog turn target through alias");
    let state: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT status, active_dispatch_id, claude_session_id
         FROM sessions WHERE session_key = $1",
    )
    .bind(primary_key)
    .fetch_one(&pool)
    .await
    .expect("load backlog-cleared primary");
    assert_eq!(state, ("disconnected".to_string(), None, None));

    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_alias_delete_targets_primary_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let primary_key = "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-delete-primary";
    let alias_key = "claude/discord_0123456789abcdef/host-b:AgentDesk-claude-delete-primary";
    let channel_id = "1479671301387059216";

    upsert_hook_session_with_identity_pg(
        &pool,
        params(primary_key, channel_id),
        Some(identity(channel_id)),
    )
    .await
    .expect("seed delete owner");
    upsert_hook_session_with_identity_pg(
        &pool,
        params(alias_key, channel_id),
        Some(identity(channel_id)),
    )
    .await
    .expect("preserve delete alias");
    let owner_id: i64 = sqlx::query_scalar("SELECT id FROM sessions WHERE session_key = $1")
        .bind(primary_key)
        .fetch_one(&pool)
        .await
        .expect("load delete owner id");

    let result = delete_session_by_key_pg(&pool, alias_key)
        .await
        .expect("delete primary through alias");
    assert_eq!(result.session_id, Some(owner_id));
    assert_eq!(result.deleted, 1);
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE id = $1")
        .bind(owner_id)
        .fetch_one(&pool)
        .await
        .expect("count deleted owner");
    assert_eq!(remaining, 0);
    let alias_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM session_key_aliases WHERE session_key = $1")
            .bind(alias_key)
            .fetch_one(&pool)
            .await
            .expect("count cascaded alias");
    assert_eq!(alias_count, 0);

    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_ambiguous_mutations_fail_closed_without_row_change_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let locator = "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-ambiguous-write";
    let alias_owner_key = "claude/discord_0123456789abcdef/host-b:AgentDesk-claude-ambiguous-owner";
    let channel_id = "1479671301387059217";

    upsert_hook_session_with_identity_pg(
        &pool,
        HookSessionUpsert {
            claude_session_id: Some("primary-selector"),
            raw_provider_session_id: Some("primary-raw"),
            ..params(locator, channel_id)
        },
        Some(identity(channel_id)),
    )
    .await
    .expect("seed exact mutation owner");
    let alias_owner_id: i64 = sqlx::query_scalar(
        "INSERT INTO sessions (
             session_key, provider, status, claude_session_id, raw_provider_session_id
         ) VALUES ($1, 'claude', 'idle', 'alias-selector', 'alias-raw')
         RETURNING id",
    )
    .bind(alias_owner_key)
    .fetch_one(&pool)
    .await
    .expect("seed alternate alias owner");
    sqlx::query("DROP TRIGGER trg_session_key_aliases_locator_namespace ON session_key_aliases")
        .execute(&pool)
        .await
        .expect("disable namespace trigger for corrupted evidence fixture");
    sqlx::query("DELETE FROM session_locator_namespace WHERE session_key = $1")
        .bind(locator)
        .execute(&pool)
        .await
        .expect("release fixture locator claim");
    sqlx::query("INSERT INTO session_key_aliases (session_key, session_id) VALUES ($1, $2)")
        .bind(locator)
        .bind(alias_owner_id)
        .execute(&pool)
        .await
        .expect("seed conflicting alias evidence");

    assert!(clear_session_id_by_key_pg(&pool, locator).await.is_err());
    assert!(delete_session_by_key_pg(&pool, locator).await.is_err());
    assert!(
        crate::db::kanban_cards::clear_session_for_turn_target_pg(&pool, locator)
            .await
            .is_err()
    );
    let unchanged: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT session_key, claude_session_id, raw_provider_session_id
         FROM sessions
         WHERE session_key IN ($1, $2)
         ORDER BY session_key",
    )
    .bind(locator)
    .bind(alias_owner_key)
    .fetch_all(&pool)
    .await
    .expect("load unchanged conflicting rows");
    assert_eq!(unchanged.len(), 2);
    assert!(
        unchanged.iter().any(|row| {
            row.0 == locator && row.1 == "primary-selector" && row.2 == "primary-raw"
        })
    );
    assert!(unchanged.iter().any(|row| {
        row.0 == alias_owner_key && row.1 == "alias-selector" && row.2 == "alias-raw"
    }));

    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_backlog_transition_rolls_back_on_divergent_alias_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let card_id = "card-canonical-divergent-backlog";
    let dispatch_id = "dispatch-canonical-divergent-backlog";
    let locator = "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-divergent-backlog";
    let alias_owner_key =
        "claude/discord_0123456789abcdef/host-b:AgentDesk-claude-divergent-backlog-owner";
    let channel_id = "1479671301387059218";

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id)
         VALUES ($1, 'Divergent backlog cleanup', 'in_progress', $2)",
    )
    .bind(card_id)
    .bind(dispatch_id)
    .execute(&pool)
    .await
    .expect("seed backlog transition card");
    sqlx::query(
        "INSERT INTO task_dispatches (
             id, kanban_card_id, dispatch_type, status, title, context
         ) VALUES ($1, $2, 'implementation', 'dispatched', 'Divergent backlog cleanup', '{}')",
    )
    .bind(dispatch_id)
    .bind(card_id)
    .execute(&pool)
    .await
    .expect("seed active backlog dispatch");
    upsert_hook_session_with_identity_pg(
        &pool,
        HookSessionUpsert {
            status: "turn_active",
            active_dispatch_id: Some(dispatch_id),
            claude_session_id: Some("primary-selector"),
            ..params(locator, channel_id)
        },
        Some(identity(channel_id)),
    )
    .await
    .expect("seed exact backlog target");
    let alias_owner_id: i64 = sqlx::query_scalar(
        "INSERT INTO sessions (
             session_key, provider, status, claude_session_id
         ) VALUES ($1, 'claude', 'idle', 'alias-selector')
         RETURNING id",
    )
    .bind(alias_owner_key)
    .fetch_one(&pool)
    .await
    .expect("seed alternate backlog alias owner");
    sqlx::query("DROP TRIGGER trg_session_key_aliases_locator_namespace ON session_key_aliases")
        .execute(&pool)
        .await
        .expect("disable namespace trigger for divergent backlog fixture");
    sqlx::query("DELETE FROM session_locator_namespace WHERE session_key = $1")
        .bind(locator)
        .execute(&pool)
        .await
        .expect("release divergent backlog locator claim");
    sqlx::query("INSERT INTO session_key_aliases (session_key, session_id) VALUES ($1, $2)")
        .bind(locator)
        .bind(alias_owner_id)
        .execute(&pool)
        .await
        .expect("seed divergent backlog alias evidence");

    let mut config = crate::config::Config::default();
    config.policies.dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    config.policies.hot_reload = false;
    let engine = crate::engine::PolicyEngine::new_with_pg(&config, Some(pool.clone()))
        .expect("create backlog transition policy engine");
    let broadcast_tx = crate::eventbus::new_broadcast();
    let batch_buffer = crate::eventbus::spawn_batch_flusher(broadcast_tx.clone());
    let state = crate::app_state::AppState {
        pg_pool: Some(pool.clone()),
        engine,
        config: std::sync::Arc::new(config),
        broadcast_tx,
        batch_buffer,
        health_registry: None,
        cluster_instance_id: None,
    };
    let error = crate::server::routes::kanban::transition_card_to_backlog_with_cleanup(
        &state,
        card_id,
        "test:divergent-backlog",
    )
    .await
    .expect_err("divergent alias evidence must fail the backlog transition");
    assert!(
        format!("{error:#}").contains("EvidenceDivergence"),
        "error must preserve the closed conflict category: {error:#}"
    );

    let card_state: (String, Option<String>) =
        sqlx::query_as("SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = $1")
            .bind(card_id)
            .fetch_one(&pool)
            .await
            .expect("load rolled-back backlog card");
    assert_eq!(card_state.0, "in_progress");
    assert_eq!(card_state.1.as_deref(), Some(dispatch_id));
    let dispatch_status: String =
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind(dispatch_id)
            .fetch_one(&pool)
            .await
            .expect("load rolled-back backlog dispatch");
    assert_eq!(dispatch_status, "dispatched");
    let session_state: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT status, active_dispatch_id, claude_session_id
         FROM sessions
         WHERE session_key = $1",
    )
    .bind(locator)
    .fetch_one(&pool)
    .await
    .expect("load unchanged backlog session");
    assert_eq!(session_state.0, "turn_active");
    assert_eq!(session_state.1.as_deref(), Some(dispatch_id));
    assert_eq!(session_state.2.as_deref(), Some("primary-selector"));

    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_backlog_transition_rolls_back_on_session_clear_db_failure_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let card_id = "card-canonical-clear-failure-backlog";
    let dispatch_id = "dispatch-canonical-clear-failure-backlog";
    let locator = "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-clear-failure";
    let channel_id = "1479671301387059219";

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id)
         VALUES ($1, 'Clear failure backlog cleanup', 'in_progress', $2)",
    )
    .bind(card_id)
    .bind(dispatch_id)
    .execute(&pool)
    .await
    .expect("seed clear-failure backlog card");
    sqlx::query(
        "INSERT INTO task_dispatches (
             id, kanban_card_id, dispatch_type, status, title, context
         ) VALUES ($1, $2, 'implementation', 'dispatched', 'Clear failure backlog cleanup', '{}')",
    )
    .bind(dispatch_id)
    .bind(card_id)
    .execute(&pool)
    .await
    .expect("seed clear-failure backlog dispatch");
    upsert_hook_session_with_identity_pg(
        &pool,
        HookSessionUpsert {
            status: "turn_active",
            active_dispatch_id: Some(dispatch_id),
            claude_session_id: Some("clear-failure-selector"),
            ..params(locator, channel_id)
        },
        Some(identity(channel_id)),
    )
    .await
    .expect("seed clear-failure backlog session");
    sqlx::query(
        "CREATE OR REPLACE FUNCTION reject_backlog_session_clear()
         RETURNS trigger
         LANGUAGE plpgsql
         AS $$
         BEGIN
             RAISE EXCEPTION 'injected session clear failure';
         END;
         $$;",
    )
    .execute(&pool)
    .await
    .expect("install deterministic session-clear failure function");
    sqlx::query(
        "CREATE TRIGGER trg_reject_backlog_session_clear
         BEFORE UPDATE ON sessions
         FOR EACH ROW
         WHEN (NEW.status = 'disconnected')
         EXECUTE FUNCTION reject_backlog_session_clear();",
    )
    .execute(&pool)
    .await
    .expect("install deterministic session-clear failure trigger");

    let mut config = crate::config::Config::default();
    config.policies.dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    config.policies.hot_reload = false;
    let engine = crate::engine::PolicyEngine::new_with_pg(&config, Some(pool.clone()))
        .expect("create clear-failure transition policy engine");
    let broadcast_tx = crate::eventbus::new_broadcast();
    let batch_buffer = crate::eventbus::spawn_batch_flusher(broadcast_tx.clone());
    let state = crate::app_state::AppState {
        pg_pool: Some(pool.clone()),
        engine,
        config: std::sync::Arc::new(config),
        broadcast_tx,
        batch_buffer,
        health_registry: None,
        cluster_instance_id: None,
    };
    let error = crate::server::routes::kanban::transition_card_to_backlog_with_cleanup(
        &state,
        card_id,
        "test:clear-failure-backlog",
    )
    .await
    .expect_err("session-clear database failure must fail the backlog transition");
    assert!(
        format!("{error:#}").contains("injected session clear failure"),
        "error must propagate the session-clear database failure: {error:#}"
    );

    let card_state: (String, Option<String>) =
        sqlx::query_as("SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = $1")
            .bind(card_id)
            .fetch_one(&pool)
            .await
            .expect("load clear-failure rolled-back card");
    assert_eq!(card_state.0, "in_progress");
    assert_eq!(card_state.1.as_deref(), Some(dispatch_id));
    let dispatch_status: String =
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind(dispatch_id)
            .fetch_one(&pool)
            .await
            .expect("load clear-failure rolled-back dispatch");
    assert_eq!(dispatch_status, "dispatched");
    let session_state: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT status, active_dispatch_id, claude_session_id
         FROM sessions
         WHERE session_key = $1",
    )
    .bind(locator)
    .fetch_one(&pool)
    .await
    .expect("load unchanged clear-failure session");
    assert_eq!(session_state.0, "turn_active");
    assert_eq!(session_state.1.as_deref(), Some(dispatch_id));
    assert_eq!(session_state.2.as_deref(), Some("clear-failure-selector"));

    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_alias_stale_cleanup_targets_primary_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let primary_key = "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-stale-primary";
    let alias_key = "claude/discord_0123456789abcdef/host-b:AgentDesk-claude-stale-primary";
    let channel_id = "1479671301387059207";

    upsert_hook_session_with_identity_pg(
        &pool,
        params(primary_key, channel_id),
        Some(identity(channel_id)),
    )
    .await
    .expect("seed stale primary");
    upsert_hook_session_with_identity_pg(
        &pool,
        params(alias_key, channel_id),
        Some(identity(channel_id)),
    )
    .await
    .expect("preserve stale alias");
    sqlx::query(
        "UPDATE sessions
         SET status = 'turn_active', last_heartbeat = NOW() - INTERVAL '7 hours'
         WHERE session_key = $1",
    )
    .bind(primary_key)
    .execute(&pool)
    .await
    .expect("age primary session");

    assert_eq!(
        disconnect_stale_fixed_session_by_key_pg(&pool, alias_key).await,
        1
    );
    let status: String = sqlx::query_scalar("SELECT status FROM sessions WHERE session_key = $1")
        .bind(primary_key)
        .fetch_one(&pool)
        .await
        .expect("load cleaned primary");
    assert_eq!(status, "disconnected");

    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_manual_rebind_alias_converges_without_new_primary_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let primary_key = "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-rebind-primary";
    let alias_key = "claude/discord_0123456789abcdef/host-b:AgentDesk-claude-rebind-primary";
    let channel_id = "1479671301387059208";
    let override_id = "4c474e5d-37e7-4b6a-bcf7-d68854a31c49";

    upsert_hook_session_with_identity_pg(
        &pool,
        params(primary_key, channel_id),
        Some(identity(channel_id)),
    )
    .await
    .expect("seed manual rebind primary");
    upsert_hook_session_with_identity_pg(
        &pool,
        params(alias_key, channel_id),
        Some(identity(channel_id)),
    )
    .await
    .expect("preserve manual rebind alias");

    crate::db::dispatched_session_rebind_override::upsert_rebind_session_override_pg(
        &pool,
        alias_key,
        "claude",
        override_id,
    )
    .await
    .expect("manual rebind converges through alias");
    let primary_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE session_key IN ($1, $2)")
            .bind(primary_key)
            .bind(alias_key)
            .fetch_one(&pool)
            .await
            .expect("count manual rebind primary rows");
    assert_eq!(primary_count, 1);
    let stored_id: Option<String> =
        sqlx::query_scalar("SELECT claude_session_id FROM sessions WHERE session_key = $1")
            .bind(primary_key)
            .fetch_one(&pool)
            .await
            .expect("load override from primary");
    assert_eq!(stored_id.as_deref(), Some(override_id));
    assert_eq!(
        super::resolve_session_key_pg(&pool, alias_key)
            .await
            .expect("resolve manual rebind alias")
            .as_deref(),
        Some(primary_key)
    );

    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_manual_rebind_provider_mismatch_fails_closed_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let primary_key = "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-rebind-owner";
    let alias_key = "claude/discord_0123456789abcdef/host-b:AgentDesk-claude-rebind-owner";
    let channel_id = "1479671301387059213";

    upsert_hook_session_with_identity_pg(
        &pool,
        params(primary_key, channel_id),
        Some(identity(channel_id)),
    )
    .await
    .expect("seed manual rebind owner");
    upsert_hook_session_with_identity_pg(
        &pool,
        params(alias_key, channel_id),
        Some(identity(channel_id)),
    )
    .await
    .expect("preserve manual rebind owner alias");

    let error = crate::db::dispatched_session_rebind_override::upsert_rebind_session_override_pg(
        &pool,
        alias_key,
        "codex",
        "4c474e5d-37e7-4b6a-bcf7-d68854a31c49",
    )
    .await
    .expect_err("manual rebind cannot cross provider ownership");
    assert!(error.contains("provider ownership mismatch"));
    let primary_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE session_key IN ($1, $2)")
            .bind(primary_key)
            .bind(alias_key)
            .fetch_one(&pool)
            .await
            .expect("count provider-owned rows");
    assert_eq!(primary_count, 1);

    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_namespace_trigger_blocks_old_primary_on_alias_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let primary_key = "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-trigger-primary";
    let alias_key = "claude/discord_0123456789abcdef/host-b:AgentDesk-claude-trigger-primary";
    let channel_id = "1479671301387059209";

    upsert_hook_session_with_identity_pg(
        &pool,
        params(primary_key, channel_id),
        Some(identity(channel_id)),
    )
    .await
    .expect("seed trigger primary");
    upsert_hook_session_with_identity_pg(
        &pool,
        params(alias_key, channel_id),
        Some(identity(channel_id)),
    )
    .await
    .expect("preserve trigger alias");

    let error = sqlx::query(
        "INSERT INTO sessions (session_key, provider, status)
         VALUES ($1, 'claude', 'idle')
         ON CONFLICT(session_key) DO UPDATE SET status = EXCLUDED.status",
    )
    .bind(alias_key)
    .execute(&pool)
    .await
    .expect_err("old direct writer must not claim alias locator");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("session_locator_namespace")
    );
    let primary_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE session_key IN ($1, $2)")
            .bind(primary_key)
            .bind(alias_key)
            .fetch_one(&pool)
            .await
            .expect("count trigger-protected rows");
    assert_eq!(primary_count, 1);

    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_namespace_claim_follows_key_update_and_delete_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let old_key = "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-claim-old";
    let new_key = "claude/discord_0123456789abcdef/host-b:AgentDesk-claude-claim-new";

    let session_id: i64 = sqlx::query_scalar(
        "INSERT INTO sessions (session_key, provider, status)
         VALUES ($1, 'claude', 'idle') RETURNING id",
    )
    .bind(old_key)
    .fetch_one(&pool)
    .await
    .expect("seed claim owner");
    sqlx::query("UPDATE sessions SET session_key = $2 WHERE id = $1")
        .bind(session_id)
        .bind(new_key)
        .execute(&pool)
        .await
        .expect("move primary locator claim");
    let old_claim: Option<String> = sqlx::query_scalar(
        "SELECT owner_kind FROM session_locator_namespace WHERE session_key = $1",
    )
    .bind(old_key)
    .fetch_optional(&pool)
    .await
    .expect("load released old claim");
    assert_eq!(old_claim, None);
    let new_claim: String = sqlx::query_scalar(
        "SELECT owner_kind FROM session_locator_namespace WHERE session_key = $1",
    )
    .bind(new_key)
    .fetch_one(&pool)
    .await
    .expect("load moved primary claim");
    assert_eq!(new_claim, "primary");

    sqlx::query("DELETE FROM sessions WHERE id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("delete primary locator owner");
    let released_claim: Option<String> = sqlx::query_scalar(
        "SELECT owner_kind FROM session_locator_namespace WHERE session_key = $1",
    )
    .bind(new_key)
    .fetch_optional(&pool)
    .await
    .expect("load released deleted claim");
    assert_eq!(released_claim, None);

    test_db.drop().await;
}

#[tokio::test]
async fn canonical_identity_namespace_primary_alias_race_has_one_owner_pg() {
    let Some(test_db) = CanonicalIdentityPgDatabase::create().await else {
        eprintln!("skipping canonical identity pg test: postgres unavailable");
        return;
    };
    let pool = test_db.migrate().await;
    let owner_key = "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-race-owner";
    let racing_key = "claude/discord_0123456789abcdef/host-b:AgentDesk-claude-race-key";
    let channel_id = "1479671301387059212";
    let owner_id: i64 = sqlx::query_scalar(
        "INSERT INTO sessions (session_key, provider, status, channel_id)
         VALUES ($1, 'claude', 'idle', $2) RETURNING id",
    )
    .bind(owner_key)
    .bind(channel_id)
    .fetch_one(&pool)
    .await
    .expect("seed race alias owner");

    let primary = sqlx::query(
        "INSERT INTO sessions (session_key, provider, status)
         VALUES ($1, 'claude', 'idle')",
    )
    .bind(racing_key)
    .execute(&pool);
    let alias =
        sqlx::query("INSERT INTO session_key_aliases (session_key, session_id) VALUES ($1, $2)")
            .bind(racing_key)
            .bind(owner_id)
            .execute(&pool);
    let (primary, alias) = tokio::join!(primary, alias);
    assert_eq!(primary.is_ok() as u8 + alias.is_ok() as u8, 1);
    let failed = if primary.is_err() {
        primary.expect_err("primary loses race")
    } else {
        alias.expect_err("alias loses race")
    };
    assert_eq!(
        failed
            .as_database_error()
            .and_then(|database_error| database_error.constraint()),
        Some("session_locator_namespace")
    );
    let primary_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE session_key = $1")
            .bind(racing_key)
            .fetch_one(&pool)
            .await
            .expect("count racing primary owner");
    let alias_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM session_key_aliases WHERE session_key = $1")
            .bind(racing_key)
            .fetch_one(&pool)
            .await
            .expect("count racing alias owner");
    assert_eq!(primary_count + alias_count, 1);
    assert!(
        super::resolve_session_key_pg(&pool, racing_key)
            .await
            .expect("resolve race winner")
            .is_some()
    );

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
    let error =
        super::hook_session_upsert_error_to_app_error(HookSessionUpsertError::test_conflict(
            SessionIdentityConflictKind::AmbiguousCanonical,
            "ambiguous canonical identity",
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
        include_str!("../../../migrations/postgres/0101_canonical_discord_session_identity.sql");
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

async fn insert_session(pool: &PgPool, key: &str, status: &str, thread: Option<&str>) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO sessions (session_key, provider, status, thread_channel_id, last_heartbeat)
         VALUES ($1, 'claude', $2, $3, NOW() - INTERVAL '2 hours') RETURNING id",
    )
    .bind(key)
    .bind(status)
    .bind(thread)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn set_raw(pool: &PgPool, key: &str, raw: Option<Value>) {
    sqlx::query("UPDATE sessions SET hosted_execution = $2 WHERE session_key = $1")
        .bind(key)
        .bind(raw)
        .execute(pool)
        .await
        .unwrap();
}

async fn remaining_keys(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT session_key FROM sessions ORDER BY session_key")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn set_identity(pool: &PgPool, key: &str, channel_id: &str) {
    sqlx::query(
        "UPDATE sessions SET identity_kind = 'discord_channel', discord_token_hash = $2,
                channel_id = $3
         WHERE session_key = $1",
    )
    .bind(key)
    .bind(TOKEN)
    .bind(channel_id)
    .execute(pool)
    .await
    .unwrap();
}

/// One row per record kind, each on its own canonical channel unless the name says it
/// has no identity; `None` leaves the record NULL (legacy).
fn retention_rows(prefix: &str, base: &str) -> Vec<(String, Option<String>, Option<Value>)> {
    let names = [
        "a-legacy",
        "b-retired",
        "c-pending",
        "d-bound",
        "e-future",
        "f-json-null",
        "g-retired-extra",
        "h-retired-other-owner",
        "i-retired-no-identity",
    ];
    let rows = names.into_iter().enumerate().map(|(index, name)| {
        let channel = format!("{base}{index}");
        let owner = owner(&channel);
        let mut retired = wire(&record(&owner, "n1", HostedState::Retired));
        let raw = match name {
            "a-legacy" => None,
            "c-pending" => Some(wire(&record(&owner, "n1", HostedState::Pending))),
            "d-bound" => Some(wire(&record(&owner, "n1", HostedState::Bound))),
            "e-future" => Some(future_schema(&owner)),
            "f-json-null" => Some(Value::Null),
            // Passes the coarse SQL check; only the full decode sees the unknown field.
            "g-retired-extra" => {
                retired["lease"] = json!(1);
                Some(retired)
            }
            "h-retired-other-owner" => {
                Some(wire(&record(&owner_other(), "n1", HostedState::Retired)))
            }
            _ => Some(retired),
        };
        let identity = (name != "i-retired-no-identity").then_some(channel);
        (format!("{prefix}{name}"), identity, raw)
    });
    rows.collect()
}

async fn seed_retention(pool: &PgPool, prefix: &str, base: &str, status: &str, thread: bool) {
    for (index, (key, channel, raw)) in retention_rows(prefix, base).into_iter().enumerate() {
        let thread = thread.then(|| format!("15006283718294283{index:02}"));
        insert_session(pool, &key, status, thread.as_deref()).await;
        if let Some(channel) = channel {
            set_identity(pool, &key, &channel).await;
        }
        set_raw(pool, &key, raw).await;
    }
}

const KEPT: [&str; 7] = [
    "c-pending",
    "d-bound",
    "e-future",
    "f-json-null",
    "g-retired-extra",
    "h-retired-other-owner",
    "i-retired-no-identity",
];

#[tokio::test]
async fn hosted_execution_bulk_cleanup_keeps_live_and_unknown_rows_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed_retention(&pool, "disc-", "2000", "disconnected", false).await;
    insert_session(&pool, "idle-legacy", "idle", None).await;

    assert_eq!(cleanup_disconnected_sessions_pg(&pool).await.unwrap(), 2);
    let mut expected: Vec<String> = KEPT.iter().map(|name| format!("disc-{name}")).collect();
    expected.push("idle-legacy".into());
    assert_eq!(remaining_keys(&pool).await, expected);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn hosted_execution_thread_gc_keeps_live_unknown_and_changed_rows_pg() {
    // Keys name tmux sessions, so markers are read from an isolated runtime root.
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let prefix = "test-host:AgentDesk-claude-gc-";
    seed_retention(&pool, prefix, "3000", "idle", true).await;
    // During the external probe one row gets a new incarnation, another a new channel.
    let (pending, rehomed) = (
        format!("{prefix}z-then-pending"),
        format!("{prefix}z-then-rehomed"),
    );
    for (index, key) in [&pending, &rehomed].into_iter().enumerate() {
        let channel = format!("30009{index}");
        insert_session(
            &pool,
            key,
            "idle",
            Some(&format!("150062837182942839{index}")),
        )
        .await;
        set_identity(&pool, key, &channel).await;
        set_raw(
            &pool,
            key,
            Some(wire(&record(&owner(&channel), "n1", HostedState::Retired))),
        )
        .await;
    }
    let pending_again = wire(&record(&owner("300090"), "n2", HostedState::Pending));
    let deleted = gc_stale_thread_sessions_with_probe_pg(&pool, |key| {
        let (pool, pending_again) = (pool.clone(), pending_again.clone());
        let (pending, rehomed) = (pending.clone(), rehomed.clone());
        async move {
            if key == pending {
                set_raw(&pool, &key, Some(pending_again)).await;
            } else if key == rehomed {
                set_identity(&pool, &key, "300092").await;
            }
            SessionPresence::Missing
        }
    })
    .await;
    let mut deleted = deleted;
    deleted.sort();
    assert_eq!(
        deleted,
        [format!("{prefix}a-legacy"), format!("{prefix}b-retired")]
    );
    let mut kept: Vec<String> = KEPT.iter().map(|name| format!("{prefix}{name}")).collect();
    kept.extend([pending, rehomed]);
    kept.sort();
    assert_eq!(remaining_keys(&pool).await, kept);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn hosted_execution_explicit_delete_refuses_live_and_unknown_rows_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed_retention(&pool, "del-", "4000", "idle", false).await;
    for name in KEPT {
        let key = format!("del-{name}");
        let error = delete_session_by_key_pg(&pool, &key).await.err();
        assert!(
            error
                .as_deref()
                .is_some_and(|e| e.contains("hosted execution")),
            "{key}: {error:?}"
        );
    }
    for key in ["del-a-legacy", "del-b-retired"] {
        let result = delete_session_by_key_pg(&pool, key).await.unwrap();
        assert!(result.session_id.is_some(), "{key}");
        assert_eq!(result.deleted, 1, "{key}");
    }
    let kept: Vec<String> = KEPT.iter().map(|name| format!("del-{name}")).collect();
    assert_eq!(remaining_keys(&pool).await, kept);
    pool.close().await;
    db.drop().await;
}

/// A NULL record keeps its row at every cleanup site unless each locator's `.host_kind`
/// marker is absent or tmux; the alias row has its Herdr marker only on the alias.
#[tokio::test]
async fn hosted_execution_cleanup_keeps_null_rows_with_a_non_tmux_host_marker_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let names = [
        "absent",
        "tmux",
        "herdr",
        "process",
        "zellij",
        "unreadable",
        "alias",
    ];
    for site in ["bulk", "gc", "explicit"] {
        let name_of = |name: &str| format!("AgentDesk-claude-{site}-{name}");
        let key = |name: &str| format!("test-host:{}", name_of(name));
        for (index, name) in names.into_iter().enumerate() {
            let thread = format!("15006283718294284{index:02}");
            let id = insert_session(&pool, &key(name), "disconnected", Some(&thread)).await;
            let marker = |name: &str| {
                crate::services::tmux_common::session_temp_path(&name_of(name), "host_kind")
            };
            match name {
                "absent" => {}
                "unreadable" => std::fs::create_dir(marker(name)).unwrap(),
                "alias" => {
                    sqlx::query(
                        "INSERT INTO session_key_aliases (session_key, session_id) VALUES ($1, $2)",
                    )
                    .bind(key("alias-old"))
                    .bind(id)
                    .execute(&pool)
                    .await
                    .unwrap();
                    std::fs::write(marker("alias-old"), "herdr").unwrap();
                }
                _ => std::fs::write(marker(name), name).unwrap(),
            }
        }
        let deletable = |name: &str| matches!(name, "absent" | "tmux");
        match site {
            "bulk" => assert_eq!(cleanup_disconnected_sessions_pg(&pool).await.unwrap(), 2),
            "gc" => {
                let mut deleted = gc_stale_thread_sessions_with_probe_pg(&pool, |_| async {
                    SessionPresence::Missing
                })
                .await;
                deleted.sort();
                assert_eq!(deleted, [key("absent"), key("tmux")]);
            }
            _ => {
                for name in names {
                    let result = delete_session_by_key_pg(&pool, &key(name)).await;
                    let deleted = result.as_ref().map(|result| result.deleted);
                    assert_eq!(deleted.is_ok(), deletable(name), "{name}: {deleted:?}");
                }
            }
        }
        let mut kept: Vec<String> = names
            .into_iter()
            .filter(|n| !deletable(n))
            .map(key)
            .collect();
        kept.sort();
        assert_eq!(remaining_keys(&pool).await, kept, "{site}");
        sqlx::query("DELETE FROM sessions")
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;
    db.drop().await;
}

async fn seed_canonical(pool: &PgPool, key: &str, channel_id: &str) {
    upsert_hook_session_with_identity_pg(pool, params(key, channel_id), Some(identity(channel_id)))
        .await
        .unwrap();
}

async fn observe(pool: &PgPool, key: &str) -> HostedObservation {
    match load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(key)).await {
        HostedLookup::Found(observation) => observation,
        other => panic!("{key}: {other:?}"),
    }
}

#[tokio::test]
async fn hosted_execution_lookup_resolves_alias_and_canonical_tuple_to_one_row_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = "1479671301387059300";
    let primary = "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-hosted";
    let alias = "claude/discord_0123456789abcdef/host-b:AgentDesk-claude-hosted";
    seed_canonical(&pool, primary, channel).await;
    seed_canonical(&pool, alias, channel).await;
    let owner = owner(channel);
    let legacy = observe(&pool, primary).await;
    assert_eq!(legacy.record, HostedRecord::Legacy);
    let installed = install_pending_pg(&pool, &legacy, pending(&owner, "n1")).await;
    assert_eq!(installed, Ok(HostedCasOutcome::Written));

    let identity = |kind, token| CanonicalSessionIdentity {
        kind,
        discord_token_hash: token,
        channel_id: channel,
    };
    let canonical = HostedLookupKey::Canonical {
        provider: "claude",
        identity: identity(SessionIdentityKind::DiscordChannel, TOKEN),
    };
    let by_alias = observe(&pool, alias).await;
    let HostedLookup::Found(by_tuple) = load_hosted_execution_pg(&pool, canonical).await else {
        panic!("canonical tuple lookup");
    };
    assert_eq!(by_alias.session_id(), legacy.session_id());
    assert_eq!(by_alias, by_tuple);
    assert_eq!(by_alias.record, HostedRecord::Known(pending(&owner, "n1")));

    for key in [
        HostedLookupKey::SessionKey(" "),
        HostedLookupKey::Canonical {
            provider: "claude",
            identity: identity(SessionIdentityKind::DiscordChannel, ""),
        },
        HostedLookupKey::Canonical {
            provider: "claude",
            identity: identity(SessionIdentityKind::ScheduledSnapshot, TOKEN),
        },
    ] {
        let lookup = load_hosted_execution_pg(&pool, key).await;
        assert!(
            matches!(lookup, HostedLookup::Unknown(_)),
            "{key:?}: {lookup:?}"
        );
    }
    let absent = HostedLookupKey::SessionKey("claude/absent");
    assert_eq!(
        load_hosted_execution_pg(&pool, absent).await,
        HostedLookup::Missing
    );

    // A record whose owner is another channel is a conflict, not this row's record.
    set_raw(&pool, primary, Some(wire(&pending(&owner_other(), "n1")))).await;
    assert_eq!(
        load_hosted_execution_pg(&pool, HostedLookupKey::SessionKey(alias)).await,
        HostedLookup::Conflict(SessionIdentityConflictKind::OwnershipMismatch)
    );
    pool.close().await;
    db.drop().await;
}

fn owner_other() -> HostedOwner {
    owner("1479671301387059999")
}

async fn raw_of(pool: &PgPool, key: &str) -> Option<Value> {
    sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE session_key = $1")
        .bind(key)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn hosted_execution_cas_writes_only_the_observed_value_and_nonce_pg() {
    use HostedCasOutcome::{Stale, Written};
    use HostedTransitionError as E;
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = "1479671301387059400";
    let key = "claude/discord_0123456789abcdef/host-a:AgentDesk-claude-cas";
    seed_canonical(&pool, key, channel).await;
    let owner = owner(channel);
    let (loc, evidence) = (location("pane-1"), expected("n1", 100));

    let legacy = observe(&pool, key).await;
    assert_eq!(
        install_pending_pg(&pool, &legacy, pending(&owner, "n1")).await,
        Ok(Written)
    );
    assert_eq!(
        install_pending_pg(&pool, &legacy, pending(&owner, "n1b")).await,
        Ok(Stale)
    );
    assert_eq!(raw_of(&pool, key).await, Some(wire(&pending(&owner, "n1"))));

    let fresh = observe(&pool, key).await;
    // Evidence the record could not read back as its own is refused before any write.
    let mut foreign_nonce = evidence.clone();
    foreign_nonce.binding_nonce = "n2".into();
    let mut zero_pid = evidence.clone();
    zero_pid.root.pid = 0;
    let mut tmux_location = loc.clone();
    tmux_location.host = "tmux".into();
    for (bad_loc, bad) in [
        (loc.clone(), foreign_nonce),
        (loc.clone(), zero_pid),
        (tmux_location, evidence.clone()),
    ] {
        let refused = record_launch_evidence_pg(&pool, &fresh, &owner, "n1", bad_loc, bad);
        assert_eq!(refused.await, Err(E::Incomplete));
    }
    assert_eq!(raw_of(&pool, key).await, Some(wire(&pending(&owner, "n1"))));
    let other =
        record_launch_evidence_pg(&pool, &fresh, &owner, "n0", loc.clone(), evidence.clone());
    assert_eq!(other.await, Err(E::NonceMismatch));
    assert_eq!(
        bind_pg(&pool, &fresh, &owner, "n1").await,
        Err(E::Incomplete)
    );
    let filled =
        record_launch_evidence_pg(&pool, &fresh, &owner, "n1", loc.clone(), evidence.clone());
    assert_eq!(filled.await, Ok(Written));

    // Stored launch evidence is never replaced by a later observation.
    let recorded = observe(&pool, key).await;
    let before = raw_of(&pool, key).await;
    let replaced = expected("n1", 200);
    let overwrite =
        record_launch_evidence_pg(&pool, &recorded, &owner, "n1", loc.clone(), replaced);
    assert_eq!(overwrite.await, Err(E::ExpectedOverwrite));
    let moved =
        record_launch_evidence_pg(&pool, &recorded, &owner, "n1", location("pane-2"), evidence);
    assert_eq!(moved.await, Err(E::ExpectedOverwrite));
    assert_eq!(raw_of(&pool, key).await, before);
    assert_eq!(bind_pg(&pool, &recorded, &owner, "n1").await, Ok(Written));

    let bound_n1 = observe(&pool, key).await;
    assert_eq!(
        retire_pg(&pool, &bound_n1, &owner_other(), "n1").await,
        Err(E::OwnerMismatch)
    );
    assert_eq!(retire_pg(&pool, &bound_n1, &owner, "n1").await, Ok(Written));
    let retired_n1 = observe(&pool, key).await;
    let reuse = install_pending_pg(&pool, &retired_n1, pending(&owner, "n1")).await;
    assert_eq!(reuse, Err(E::NonceMismatch));
    assert_eq!(
        install_pending_pg(&pool, &retired_n1, pending(&owner, "n2")).await,
        Ok(Written)
    );
    let pending_n2 = observe(&pool, key).await;
    let n2 = expected("n2", 300);
    let filled = record_launch_evidence_pg(&pool, &pending_n2, &owner, "n2", loc, n2);
    assert_eq!(filled.await, Ok(Written));
    assert_eq!(
        bind_pg(&pool, &observe(&pool, key).await, &owner, "n2").await,
        Ok(Written)
    );

    // The old incarnation's retire, stale or freshly observed, leaves the new Bound.
    let bound_n2 = raw_of(&pool, key).await;
    assert_eq!(retire_pg(&pool, &bound_n1, &owner, "n1").await, Ok(Stale));
    let fresh_n2 = observe(&pool, key).await;
    assert_eq!(
        retire_pg(&pool, &fresh_n2, &owner, "n1").await,
        Err(E::NonceMismatch)
    );
    assert_eq!(raw_of(&pool, key).await, bound_n2);

    set_raw(&pool, key, Some(future_schema(&owner))).await;
    let unknown = observe(&pool, key).await;
    assert!(matches!(unknown.record, HostedRecord::Unknown(_)));
    let over_unknown = install_pending_pg(&pool, &unknown, pending(&owner, "n3")).await;
    assert_eq!(over_unknown, Err(E::UnknownRecord));
    assert_eq!(
        retire_pg(&pool, &unknown, &owner, "n-future").await,
        Err(E::UnknownRecord)
    );
    assert_eq!(raw_of(&pool, key).await, Some(future_schema(&owner)));

    // A legacy row without canonical identity never receives a record.
    insert_session(&pool, "claude/legacy-only", "idle", None).await;
    let legacy_only = observe(&pool, "claude/legacy-only").await;
    let refused = install_pending_pg(&pool, &legacy_only, pending(&owner, "n4")).await;
    assert_eq!(refused, Ok(Stale));
    assert_eq!(raw_of(&pool, "claude/legacy-only").await, None);
    pool.close().await;
    db.drop().await;
}
