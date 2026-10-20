use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;

/// Runs the onboarding agent write on a fully migrated schema, so the status
/// under test is the value the write path binds, checked by the live CHECK.
#[tokio::test(flavor = "current_thread")]
async fn onboarding_status_is_accepted_by_migrated_constraint_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let body: CompleteBody = serde_json::from_value(serde_json::json!({
        "token": "onboarding-token", "guild_id": "guild-1", "channels": []
    }))
    .expect("onboarding body");
    let mapping = ResolvedChannelMapping {
        channel_id: "channel-1".to_string(),
        channel_name: "dev".to_string(),
        requested_channel_name: "dev".to_string(),
        role_id: "onboarded-agent".to_string(),
        description: None,
        system_prompt: None,
        created: false,
        resolution: ChannelResolutionKind::ProvidedId,
    };

    persist_onboarding_pg(&pool, &body, "codex", &[mapping])
        .await
        .expect("the onboarding agent write must satisfy agents_status_known_check");
    let stored: String =
        sqlx::query_scalar("SELECT status FROM agents WHERE id = 'onboarded-agent'")
            .fetch_one(&pool)
            .await
            .expect("read the onboarded agent");
    assert_eq!(
        stored, "idle",
        "new onboarding agents start in canonical idle"
    );

    let rejected =
        sqlx::query("UPDATE agents SET status = 'archived' WHERE id = 'onboarded-agent'")
            .execute(&pool)
            .await
            .expect_err("the migrated CHECK must still narrow agents.status");
    assert_eq!(
        rejected.as_database_error().and_then(|db| db.constraint()),
        Some("agents_status_known_check")
    );

    pool.close().await;
    fixture.drop().await;
}
