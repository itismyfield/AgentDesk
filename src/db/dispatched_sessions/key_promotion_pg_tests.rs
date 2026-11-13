use super::key_promotion::*;
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::platform::tmux::session_inventory::{self, Inventory};

const CHANNEL: &str = "81190201";
const TOKEN: &str = "discord_0123456789abcdef";

fn identity() -> CanonicalSessionIdentity<'static> {
    CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: TOKEN,
        channel_id: CHANNEL,
    }
}
fn params<'a>(key: &'a str, nonce: &'a str) -> HookSessionUpsert<'a> {
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
        channel_id: Some(CHANNEL),
        claude_session_id: None,
        raw_provider_session_id: None,
        turn_start_nonce: Some(nonce),
        dispatched_origin: false,
    }
}
fn key(name: &str) -> String {
    format!("claude/{TOKEN}/host:{name}")
}

#[tokio::test]
async fn key_promotion_recreation_commits_primary_alias_and_outcome_pg() {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let dir = tempfile::tempdir().unwrap();
    let _namespace = session_inventory::tests::ChannelNamespace::new(
        CHANNEL.parse().unwrap(),
        dir.path().join("isolated.sock"),
    );
    let observation = session_inventory::observe_inventory(CHANNEL.parse().unwrap());
    assert!(matches!(observation.inventory, Inventory::Empty(_)));
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (old, new) = (key("AgentDesk-claude-adk-cc"), key("adk-cc"));
    upsert_hook_session_with_identity_pg(&pool, params(&old, "old"), Some(identity()))
        .await
        .unwrap();
    let snapshot = discover_registration_pg(&pool, "claude", identity())
        .await
        .unwrap()
        .unwrap();
    let absent = || observation.current() && matches!(observation.inventory, Inventory::Empty(_));
    let witness = RegistrationWitness {
        snapshot: &snapshot,
        new_key: &new,
        new_nonce: "new",
        old_host_absent: &absent,
    };
    let outcome = register_hook_session_pg(&pool, params(&new, "new"), identity(), &witness)
        .await
        .unwrap();
    let primary: (i64, String, Option<String>) =
        sqlx::query_as("SELECT id, session_key, active_turn_nonce FROM sessions WHERE id = $1")
            .bind(snapshot.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let alias: Option<i64> =
        sqlx::query_scalar("SELECT session_id FROM session_key_aliases WHERE session_key = $1")
            .bind(&old)
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(
        (primary, alias, outcome.session_key),
        (
            (snapshot.id, new.clone(), Some("new".into())),
            Some(snapshot.id),
            new
        )
    );
    pool.close().await;
    db.drop().await;
}

async fn setup() -> (TestPostgresDb, PgPool, String, String, RegistrationSnapshot) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (old, new) = (key("AgentDesk-claude-adk-cc"), key("adk-cc"));
    upsert_hook_session_with_identity_pg(&pool, params(&old, "old"), Some(identity()))
        .await
        .unwrap();
    let snapshot = discover_registration_pg(&pool, "claude", identity())
        .await
        .unwrap()
        .unwrap();
    (db, pool, old, new, snapshot)
}
async fn promote(
    pool: &PgPool,
    snapshot: &RegistrationSnapshot,
    new: &str,
) -> Result<HookSessionUpsertOutcome, HookSessionUpsertError> {
    let witness = RegistrationWitness {
        snapshot,
        new_key: new,
        new_nonce: "new",
        old_host_absent: &|| true,
    };
    let mut fresh = params(new, "new");
    fresh.status = "turn_active";
    register_hook_session_pg(pool, fresh, identity(), &witness).await
}
async fn state(pool: &PgPool) -> serde_json::Value {
    sqlx::query_scalar("SELECT jsonb_build_object('rows', (SELECT jsonb_agg(to_jsonb(s) ORDER BY id) FROM sessions s), 'aliases', (SELECT jsonb_agg(to_jsonb(a) ORDER BY session_key) FROM session_key_aliases a))")
        .fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn key_promotion_late_alias_nonce_and_actor_pin_refuse_before_update_pg() {
    let (db, pool, old, new, snapshot) = setup().await;
    let old_pin = capture_hook_session_actor_pin_pg(&pool, &old)
        .await
        .unwrap();
    promote(&pool, &snapshot, &new).await.unwrap();
    let before = state(&pool).await;
    assert!(
        upsert_hook_session_terminal_pg(&pool, params_without_nonce(&old), Some(identity()), "old")
            .await
            .is_err()
    );
    assert!(
        upsert_hook_session_with_identity_pg(&pool, params(&old, "old"), Some(identity()))
            .await
            .is_err()
    );
    assert!(
        upsert_hook_session_with_identity_pg(&pool, params_without_nonce(&old), Some(identity()))
            .await
            .is_err()
    );
    assert!(
        upsert_hook_session_with_actor_pin_pg(&pool, params(&old, "old"), &old_pin)
            .await
            .is_err()
    );
    assert_eq!(state(&pool).await, before);
    pool.close().await;
    db.drop().await;
}
fn params_without_nonce(key: &str) -> HookSessionUpsert<'_> {
    let mut value = params(key, "");
    value.turn_start_nonce = None;
    value
}

#[tokio::test]
async fn key_promotion_conflicts_roll_back_every_column_and_alias_pg() {
    let (db, pool, old, new, snapshot) = setup().await;
    // A primary owned by another canonical row cannot be merged into this registration.
    sqlx::query("INSERT INTO sessions(session_key, provider, status, channel_id) VALUES ($1, 'claude', 'idle', '811999')")
        .bind(&new).execute(&pool).await.unwrap();
    let before = state(&pool).await;
    assert!(promote(&pool, &snapshot, &new).await.is_err());
    assert_eq!(state(&pool).await, before);
    sqlx::query("DELETE FROM sessions WHERE session_key = $1")
        .bind(&new)
        .execute(&pool)
        .await
        .unwrap();
    // A stale source snapshot and an incorrect requested nonce both preserve all columns.
    sqlx::query("UPDATE sessions SET active_turn_nonce = 'raced' WHERE session_key = $1")
        .bind(&old)
        .execute(&pool)
        .await
        .unwrap();
    let before = state(&pool).await;
    assert!(promote(&pool, &snapshot, &new).await.is_err());
    assert_eq!(state(&pool).await, before);
    let snapshot = discover_registration_pg(&pool, "claude", identity())
        .await
        .unwrap()
        .unwrap();
    let witness = RegistrationWitness {
        snapshot: &snapshot,
        new_key: &new,
        new_nonce: "different",
        old_host_absent: &|| true,
    };
    assert!(
        register_hook_session_pg(&pool, params(&new, "new"), identity(), &witness)
            .await
            .is_err()
    );
    assert_eq!(state(&pool).await, before);
    // Corrupt legacy alias evidence is refused too, without repairing or deleting it.
    let foreign: i64 = sqlx::query_scalar("INSERT INTO sessions(session_key, provider, status) VALUES ('other:foreign', 'claude', 'idle') RETURNING id")
        .fetch_one(&pool).await.unwrap();
    sqlx::query(
        "ALTER TABLE session_key_aliases DISABLE TRIGGER trg_session_key_aliases_locator_namespace",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO session_key_aliases(session_key, session_id) VALUES ($1, $2)")
        .bind(&old)
        .bind(foreign)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "ALTER TABLE session_key_aliases ENABLE TRIGGER trg_session_key_aliases_locator_namespace",
    )
    .execute(&pool)
    .await
    .unwrap();
    let before = state(&pool).await;
    assert!(promote(&pool, &snapshot, &new).await.is_err());
    assert_eq!(state(&pool).await, before);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn key_promotion_live_failed_unknown_foreign_server_and_pending_hold_pg() {
    let (db, pool, _old, new, snapshot) = setup().await;
    for label in ["live", "probe_failed", "unknown", "different_server"] {
        let before = state(&pool).await;
        let absent = || false;
        let witness = RegistrationWitness {
            snapshot: &snapshot,
            new_key: &new,
            new_nonce: "new",
            old_host_absent: &absent,
        };
        assert!(
            register_hook_session_pg(&pool, params(&new, "new"), identity(), &witness)
                .await
                .is_err(),
            "{label}"
        );
        assert_eq!(state(&pool).await, before, "{label}");
    }
    let pending = crate::db::dispatched_sessions::hosted_execution::tests::pending(
        &crate::db::dispatched_sessions::hosted_execution::tests::owner(CHANNEL),
        "pending",
    );
    sqlx::query("UPDATE sessions SET hosted_execution = $1 WHERE id = $2")
        .bind(serde_json::to_value(pending).unwrap())
        .bind(snapshot.id)
        .execute(&pool)
        .await
        .unwrap();
    let snapshot = discover_registration_pg(&pool, "claude", identity())
        .await
        .unwrap()
        .unwrap();
    let before = state(&pool).await;
    assert!(promote(&pool, &snapshot, &new).await.is_err());
    assert_eq!(state(&pool).await, before);
    pool.close().await;
    db.drop().await;
}

async fn wait_for_locator_wait(pool: &PgPool, key: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND NOT granted AND classid = 4913 AND objid = (hashtext('locator:' || $1)::bigint & 4294967295))")
                .bind(key).fetch_one(pool).await.unwrap();
            if waiting { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("registration must reach the held locator");
}

#[tokio::test]
async fn key_promotion_locator_locks_precede_identity_and_do_not_deadlock_pg() {
    let (db, pool, old, new, snapshot) = setup().await;
    let mut held = pool.begin().await.unwrap();
    acquire_locator_lock(&mut held, &old).await.unwrap();
    let task_pool = pool.clone();
    let task_new = new.clone();
    let task = tokio::spawn(async move { promote(&task_pool, &snapshot, &task_new).await });
    wait_for_locator_wait(&pool, &old).await;
    let can_lock: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(4913, hashtext($1))")
        .bind(format!("identity:discord_channel:claude:{TOKEN}:{CHANNEL}"))
        .fetch_one(&mut *held)
        .await
        .unwrap();
    held.commit().await.unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    assert!(
        can_lock,
        "registration must not hold identity while waiting for old locator"
    );
    assert_eq!(result.unwrap().session_key, new);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn key_promotion_overlapping_old_hook_is_fenced_after_commit_pg() {
    let (db, pool, old, new, snapshot) = setup().await;
    let mut held = pool.begin().await.unwrap();
    acquire_locator_lock(&mut held, &new).await.unwrap();
    let task_pool = pool.clone();
    let task_new = new.clone();
    let registration = tokio::spawn(async move { promote(&task_pool, &snapshot, &task_new).await });
    wait_for_locator_wait(&pool, &new).await;
    let hook_pool = pool.clone();
    let hook_old = old.clone();
    let hook = tokio::spawn(async move {
        upsert_hook_session_terminal_pg(
            &hook_pool,
            params_without_nonce(&hook_old),
            Some(identity()),
            "old",
        )
        .await
    });
    wait_for_locator_wait(&pool, &old).await;
    held.commit().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        assert_eq!(registration.await.unwrap().unwrap().session_key, new);
        assert!(hook.await.unwrap().is_err());
    })
    .await
    .unwrap();
    let current: (String, Option<String>, Option<String>) = sqlx::query_as("SELECT session_key, active_turn_nonce, status FROM sessions WHERE identity_kind = 'discord_channel'").fetch_one(&pool).await.unwrap();
    assert_eq!(
        current,
        (new, Some("new".into()), Some("turn_active".into()))
    );
    pool.close().await;
    db.drop().await;
}
