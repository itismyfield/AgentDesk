use super::*;

#[test]
fn scheduled_message_bot_defaults_to_non_triggering_notify() {
    assert_eq!(scheduled_message_bot_or_default(None), "notify");
    assert_eq!(scheduled_message_bot_or_default(Some("   ")), "notify");
    assert_eq!(scheduled_message_bot_or_default(Some(" notify ")), "notify");
    assert_eq!(
        scheduled_message_bot_or_default(Some("announce")),
        "announce"
    );
}

#[test]
fn scheduled_push_rejects_agent_only_fields_but_allows_explicit_clears() {
    for (agent_id, instruction, explicit_failure, expected_error) in [
        (
            Some("unused-agent"),
            None,
            false,
            "agentId is only valid for agent delivery",
        ),
        (
            None,
            Some("unused instruction"),
            false,
            "agentInstruction is only valid for agent delivery",
        ),
        (
            None,
            None,
            true,
            "onAgentFailure is only valid for agent delivery",
        ),
    ] {
        let (status, Json(body)) =
            validate_agent_only_fields(db::KIND_PUSH, agent_id, instruction, explicit_failure)
                .expect_err("push must reject agent-only values");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body.get("error").and_then(JsonValue::as_str),
            Some(expected_error)
        );
    }

    validate_agent_only_fields(db::KIND_PUSH, None, None, false)
        .expect("an ordinary push or explicit null clears have no agent-only value");
    validate_agent_only_fields(
        db::KIND_AGENT,
        Some("scheduled-agent"),
        Some("delivery instruction"),
        true,
    )
    .expect("agent delivery accepts its dedicated fields");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_scheduled_message_create_persists_trimmed_explicit_bot() {
    let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "agentdesk_smsg_trimmed_bot",
        "scheduled message explicit bot normalization",
    )
    .await;
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
    let body = CreateScheduledMessageBody {
        content: "trim explicit bot before persistence".to_string(),
        title: None,
        target_channel_id: Some("123456789".to_string()),
        bot: Some(" notify ".to_string()),
        delivery_kind: Some(db::KIND_PUSH.to_string()),
        agent_id: None,
        agent_instruction: None,
        on_agent_failure: None,
        scheduled_at: (Utc::now() + chrono::Duration::minutes(5)).to_rfc3339(),
        schedule: None,
        timezone: Some("UTC".to_string()),
        expires_at: None,
        source: Some("postgres_test".to_string()),
        created_by: Some("postgres_test".to_string()),
        dedupe_key: None,
    };

    let new = validate_create(&pool, &body)
        .await
        .expect("validate explicit bot create");
    assert_eq!(new.bot, "notify");
    let row = db::insert_scheduled_message_pg(&pool, &new)
        .await
        .expect("persist explicit bot create");
    assert_eq!(row.bot, "notify");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_scheduled_push_rejects_agent_id_before_foreign_key_insert() {
    let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "agentdesk_smsg_push_agent_id",
        "scheduled push rejects agent-only foreign key input",
    )
    .await;
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
    let body = CreateScheduledMessageBody {
        content: "push must not persist an unused agent association".to_string(),
        title: None,
        target_channel_id: Some("123456789".to_string()),
        bot: None,
        delivery_kind: Some(db::KIND_PUSH.to_string()),
        agent_id: Some("typo-missing-agent".to_string()),
        agent_instruction: None,
        on_agent_failure: None,
        scheduled_at: (Utc::now() + chrono::Duration::minutes(5)).to_rfc3339(),
        schedule: None,
        timezone: Some("UTC".to_string()),
        expires_at: None,
        source: Some("postgres_test".to_string()),
        created_by: Some("postgres_test".to_string()),
        dedupe_key: None,
    };

    let (status, Json(error)) = validate_create(&pool, &body)
        .await
        .expect_err("push agentId must fail as a request error before INSERT");
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.get("error").and_then(JsonValue::as_str),
        Some("agentId is only valid for agent delivery")
    );
    let stored_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM scheduled_messages")
        .fetch_one(&pool)
        .await
        .expect("count scheduled definitions after rejected create");
    assert_eq!(stored_count, 0);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_scheduled_push_patch_distinguishes_values_from_null_clears() {
    let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "agentdesk_smsg_push_patch_fields",
        "scheduled push patch agent-only field policy",
    )
    .await;
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
    let existing = db::insert_scheduled_message_pg(
        &pool,
        &db::NewScheduledMessage {
            content: "ordinary push patch definition".to_string(),
            title: None,
            target_channel_id: Some("123456789".to_string()),
            bot: "notify".to_string(),
            delivery_kind: db::KIND_PUSH.to_string(),
            agent_id: None,
            agent_instruction: None,
            on_agent_failure: "fail".to_string(),
            scheduled_at: Utc::now() + chrono::Duration::minutes(5),
            schedule: None,
            timezone: "UTC".to_string(),
            expires_at: None,
            source: "postgres_test".to_string(),
            created_by: Some("postgres_test".to_string()),
            dedupe_key: None,
        },
    )
    .await
    .expect("insert ordinary push definition");

    let metadata_body = json!({"title": "metadata-only update"});
    let metadata_patch = build_patch(
        &pool,
        metadata_body.as_object().expect("metadata patch object"),
        &existing,
    )
    .await
    .expect("metadata-only PATCH must not treat stored default fail as explicit input");
    assert_eq!(
        metadata_patch.title,
        Some(Some("metadata-only update".to_string()))
    );

    let clear_body = json!({"agentId": null, "agentInstruction": null});
    let clear_patch = build_patch(
        &pool,
        clear_body.as_object().expect("agent clear patch object"),
        &existing,
    )
    .await
    .expect("explicit null clears leave no effective agent-only value");
    assert_eq!(clear_patch.agent_id, Some(None));
    assert_eq!(clear_patch.agent_instruction, Some(None));

    for (body, expected_error) in [
        (
            json!({"agentId": "typo-missing-agent"}),
            "agentId is only valid for agent delivery",
        ),
        (
            json!({"agentInstruction": "unused instruction"}),
            "agentInstruction is only valid for agent delivery",
        ),
        (
            json!({"onAgentFailure": "push_raw"}),
            "onAgentFailure is only valid for agent delivery",
        ),
    ] {
        let (status, Json(error)) = build_patch(
            &pool,
            body.as_object().expect("invalid push patch object"),
            &existing,
        )
        .await
        .expect_err("push PATCH must reject effective agent-only values");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.get("error").and_then(JsonValue::as_str),
            Some(expected_error)
        );
    }

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_scheduled_message_explicit_target_still_requires_agent_primary_channel() {
    let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "agentdesk_smsg_primary_channel",
        "scheduled message agent primary channel validation",
    )
    .await;
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
         VALUES ('scheduled-agent-without-primary', 'Scheduled Agent Without Primary', NULL)",
    )
    .execute(&pool)
    .await
    .expect("seed agent without a primary channel");

    let (status, Json(body)) = validate_targeting(
        &pool,
        db::KIND_AGENT,
        Some("987654321"),
        Some("scheduled-agent-without-primary"),
    )
    .await
    .expect_err("an explicit delivery target must not bypass the owner-channel requirement");

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body.get("error").and_then(JsonValue::as_str),
        Some("agent 'scheduled-agent-without-primary' has no primary Discord channel")
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_scheduled_message_rejects_invalid_agent_primary_channel() {
    let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "agentdesk_smsg_invalid_primary",
        "scheduled message invalid agent primary channel validation",
    )
    .await;
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
         VALUES ('scheduled-agent-invalid-primary', 'Scheduled Agent Invalid Primary',
                 'not-a-known-channel-alias')",
    )
    .execute(&pool)
    .await
    .expect("seed agent with an invalid primary channel");

    let (status, Json(body)) = validate_targeting(
        &pool,
        db::KIND_AGENT,
        None,
        Some("scheduled-agent-invalid-primary"),
    )
    .await
    .expect_err("an invalid owner channel must fail before fire time");

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body.get("error").and_then(JsonValue::as_str),
        Some("agent 'scheduled-agent-invalid-primary' has an invalid primary Discord channel")
    );

    pool.close().await;
    pg_db.drop().await;
}
