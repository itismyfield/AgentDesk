use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::dispatched_sessions::hosted_execution::tests::{owner, record, wire};
use crate::db::dispatched_sessions::hosted_execution::{HostedState, http_force};

fn fixture(channel: &str) -> HostedOwner {
    let mut owner = owner(channel);
    owner.logical_key = format!("AgentDesk-claude-{channel}");
    owner
}

async fn seed(pool: &PgPool, owner: &HostedOwner, raw: Option<Value>) -> (i64, String) {
    let key = format!(
        "{}/{}/fixture:{}",
        owner.provider, owner.discord_token_hash, owner.logical_key
    );
    let id = sqlx::query_scalar(
        "INSERT INTO sessions (session_key, provider, identity_kind, discord_token_hash,
             channel_id, claude_session_id, raw_provider_session_id, hosted_execution)
         VALUES ($1, $2, 'discord_channel', $3, $4, 'old-selector', 'old-selector', $5)
         RETURNING id",
    )
    .bind(&key)
    .bind(&owner.provider)
    .bind(&owner.discord_token_hash)
    .bind(&owner.channel_id)
    .bind(raw)
    .fetch_one(pool)
    .await
    .unwrap();
    (id, key)
}

async fn selectors(pool: &PgPool, id: i64) -> (Option<String>, Option<String>) {
    sqlx::query_as("SELECT claude_session_id, raw_provider_session_id FROM sessions WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn migration_keeps_existing_rows_unfenced_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let owner = fixture("resume-fence-migration");
    let (_, key) = seed(&pool, &owner, None).await;
    sqlx::query("ALTER TABLE sessions DROP COLUMN provider_resume_fence")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../../../../migrations/postgres/0194_provider_resume_fence.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(load_by_resolved_key_pg(&pool, &key).await.unwrap(), None);
    assert_eq!(load_for_owner_pg(&pool, &owner).await.unwrap(), None);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn channel_clear_fences_missing_legacy_and_unknown_rows_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let missing = fixture("resume-fence-missing");
    let key = "claude/discord_0123456789abcdef/fixture:AgentDesk-claude-resume-fence-missing";
    assert!(load_for_owner_pg(&pool, &missing).await.is_err());
    let fence = set_for_channel_pg(&pool, &missing.provider, identity(&missing), key)
        .await
        .unwrap();
    assert_eq!(
        load_for_owner_pg(&pool, &missing).await.unwrap(),
        Some(fence)
    );
    let hosted: Option<Value> =
        sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE session_key = $1")
            .bind(key)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        hosted, None,
        "a missing owner gets no invented hosted execution"
    );
    for (channel, raw) in [
        ("resume-fence-legacy", None),
        (
            "resume-fence-unknown",
            Some(serde_json::json!({"schema": 99})),
        ),
    ] {
        let owner = fixture(channel);
        let (id, key) = seed(&pool, &owner, raw.clone()).await;
        let fence = set_for_channel_pg(&pool, &owner.provider, identity(&owner), &key)
            .await
            .unwrap();
        assert_eq!(fence.session_row_id, id);
        assert_eq!(
            load_by_resolved_key_pg(&pool, &key).await.unwrap(),
            Some(fence)
        );
        let kept: Option<Value> =
            sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(kept, raw);
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn legacy_locator_clear_fences_without_rewriting_ownership_or_selectors_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let owner = fixture("resume-fence-old-locator");
    for channel in [None, Some(owner.channel_id.as_str())] {
        let key = format!(
            "claude/{}/fixture:legacy-{}",
            owner.discord_token_hash,
            channel.is_some()
        );
        let alias = format!("{key}-alias");
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO sessions (session_key, provider, channel_id, claude_session_id)
             VALUES ($1, 'claude', $2, 'old-selector') RETURNING id",
        )
        .bind(&key)
        .bind(channel)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO session_key_aliases (session_key, session_id) VALUES ($1, $2)")
            .bind(&alias)
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        for locator in [&key, &alias] {
            let fence = set_for_channel_pg(&pool, &owner.provider, identity(&owner), locator)
                .await
                .unwrap();
            assert_eq!(fence.session_row_id, id);
            assert_eq!(
                load_by_resolved_key_pg(&pool, &key).await.unwrap(),
                Some(fence)
            );
            let kept: (
                Option<String>,
                Option<String>,
                Option<String>,
                Option<Value>,
            ) = sqlx::query_as(
                "SELECT identity_kind, discord_token_hash, channel_id, hosted_execution
                     FROM sessions WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(kept, (None, None, channel.map(str::to_owned), None));
            assert_eq!(
                selectors(&pool, id).await.0.as_deref(),
                Some("old-selector")
            );
        }
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn legacy_locator_clear_rejects_other_channel_and_namespace_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let owner = fixture("resume-fence-legacy-owner");
    for (suffix, token, channel) in [
        (
            "channel",
            owner.discord_token_hash.as_str(),
            Some("another-channel"),
        ),
        ("namespace", "another-token", None),
    ] {
        let key = format!("claude/{token}/fixture:{suffix}");
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, channel_id, claude_session_id)
             VALUES ($1, 'claude', $2, 'old-selector')",
        )
        .bind(&key)
        .bind(channel)
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            set_for_channel_pg(&pool, &owner.provider, identity(&owner), &key)
                .await
                .is_err()
        );
        assert_eq!(load_by_resolved_key_pg(&pool, &key).await.unwrap(), None);
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn held_clear_requires_exact_execution_and_preserves_successor_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let owner = fixture("resume-fence-held");
    let old = record(&owner, "old-execution", HostedState::Bound);
    let (id, key) = seed(&pool, &owner, Some(wire(&old))).await;
    let mut wrong = old.clone();
    wrong.owner.owner_node = "other-owner".into();
    assert!(!set_for_execution_pg(&pool, id, &wrong).await.unwrap());
    let successor = record(&owner, "new-execution", HostedState::Bound);
    sqlx::query("UPDATE sessions SET hosted_execution = $2 WHERE id = $1")
        .bind(id)
        .bind(wire(&successor))
        .execute(&pool)
        .await
        .unwrap();
    assert!(!set_for_execution_pg(&pool, id, &old).await.unwrap());
    assert_eq!(
        selectors(&pool, id).await,
        (Some("old-selector".into()), Some("old-selector".into()))
    );
    assert_eq!(load_by_resolved_key_pg(&pool, &key).await.unwrap(), None);
    assert!(set_for_execution_pg(&pool, id, &successor).await.unwrap());
    assert_eq!(selectors(&pool, id).await, (None, None));
    assert!(
        load_by_resolved_key_pg(&pool, &key)
            .await
            .unwrap()
            .is_some()
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn retired_selector_clear_sets_fence_atomically_and_preserves_successor_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let owner = fixture("resume-fence-retired");
    let old = record(&owner, "old-execution", HostedState::Bound);
    let retired = record(&owner, "old-execution", HostedState::Retired);
    let (id, key) = seed(&pool, &owner, Some(wire(&retired))).await;
    assert!(
        http_force::clear_retired_selector_pg(&pool, id, &old)
            .await
            .unwrap()
    );
    assert_eq!(selectors(&pool, id).await, (None, None));
    let fence = load_by_resolved_key_pg(&pool, &key).await.unwrap().unwrap();
    let successor = record(&owner, "new-execution", HostedState::Bound);
    sqlx::query("UPDATE sessions SET hosted_execution = $2, claude_session_id = 'new-selector' WHERE id = $1")
        .bind(id).bind(wire(&successor)).execute(&pool).await.unwrap();
    assert!(
        !http_force::clear_retired_selector_pg(&pool, id, &old)
            .await
            .unwrap()
    );
    assert_eq!(
        selectors(&pool, id).await.0.as_deref(),
        Some("new-selector")
    );
    assert_eq!(
        load_by_resolved_key_pg(&pool, &key).await.unwrap(),
        Some(fence)
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn only_the_captured_token_releases_the_current_clear_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let owner = fixture("resume-fence-rollover");
    let (_, key) = seed(&pool, &owner, None).await;
    let first = set_for_channel_pg(&pool, &owner.provider, identity(&owner), &key)
        .await
        .unwrap();
    let second = set_for_channel_pg(&pool, &owner.provider, identity(&owner), &key)
        .await
        .unwrap();
    assert_ne!(first.token, second.token);
    assert!(!release_pg(&pool, &first).await.unwrap());
    assert_eq!(
        load_for_owner_pg(&pool, &owner).await.unwrap(),
        Some(second.clone())
    );
    assert!(release_pg(&pool, &second).await.unwrap());
    assert_eq!(load_for_owner_pg(&pool, &owner).await.unwrap(), None);
    assert!(!release_pg(&pool, &second).await.unwrap());
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn unreadable_or_missing_fence_rows_fail_closed_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let owner = fixture("resume-fence-unreadable");
    let (id, key) = seed(&pool, &owner, None).await;
    sqlx::query("UPDATE sessions SET provider_resume_fence = '{}'::jsonb WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(load_for_owner_pg(&pool, &owner).await.is_err());
    assert!(load_by_resolved_key_pg(&pool, &key).await.is_err());
    assert!(
        load_by_resolved_key_pg(&pool, "missing-resolved-key")
            .await
            .is_err()
    );
    sqlx::query("ALTER TABLE sessions DROP COLUMN provider_resume_fence")
        .execute(&pool)
        .await
        .unwrap();
    assert!(load_for_owner_pg(&pool, &owner).await.is_err());
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn codex_selector_clear_does_not_install_an_unreleasable_claude_fence_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut owner = fixture("resume-fence-codex");
    owner.provider = "codex".into();
    let bound = record(&owner, "codex-execution", HostedState::Bound);
    let (id, key) = seed(&pool, &owner, Some(wire(&bound))).await;
    assert!(set_for_execution_pg(&pool, id, &bound).await.unwrap());
    assert_eq!(selectors(&pool, id).await, (None, None));
    assert_eq!(load_by_resolved_key_pg(&pool, &key).await.unwrap(), None);
    let mut retired = bound.clone();
    retired.state = HostedState::Retired;
    sqlx::query("UPDATE sessions SET hosted_execution = $2 WHERE id = $1")
        .bind(id)
        .bind(wire(&retired))
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        http_force::clear_retired_selector_pg(&pool, id, &bound)
            .await
            .unwrap()
    );
    assert_eq!(load_by_resolved_key_pg(&pool, &key).await.unwrap(), None);
    assert!(
        set_for_channel_pg(&pool, &owner.provider, identity(&owner), &key)
            .await
            .is_err()
    );
    pool.close().await;
    db.drop().await;
}
