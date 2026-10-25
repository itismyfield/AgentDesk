use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;

#[tokio::test]
async fn system_prompt_active_native_binding_precedes_persisted_session() {
    use crate::services::provider::{CancelToken, session_prompt};
    let token = CancelToken::new();
    session_prompt::arm(&token, None, &ProviderKind::Codex, 42, "first-turn", 0);
    session_prompt::observe(&token, "new-native");
    assert_eq!(
        inspection_binding(
            None,
            "42",
            Some(&token),
            ProviderKind::Claude,
            Some("old-native".into())
        )
        .await
        .unwrap(),
        Some((ProviderKind::Codex, "new-native".into(), false))
    );
    assert_eq!(
        inspection_binding(None, "42", Some(&token), ProviderKind::Claude, None)
            .await
            .unwrap(),
        Some((ProviderKind::Codex, "new-native".into(), false))
    );
    assert_eq!(
        inspection_binding(
            None,
            "42",
            None,
            ProviderKind::Claude,
            Some("persisted".into())
        )
        .await
        .unwrap(),
        Some((ProviderKind::Claude, "persisted".into(), false))
    );
    assert_eq!(
        inspection_binding(None, "42", None, ProviderKind::Claude, None)
            .await
            .unwrap(),
        None
    );
}

#[test]
fn system_prompt_command_registered() {
    let commands = crate::services::discord::runtime_bootstrap::run_bot_build_slash_commands();
    let command = commands
        .iter()
        .find(|c| c.name == "system-prompt")
        .expect("registered /system-prompt");
    assert!(command.slash_action.is_some());
    assert!(
        command
            .description
            .as_ref()
            .is_some_and(|d| d.contains("system prompt"))
    );
}

#[tokio::test]
async fn system_prompt_missing_snapshot_is_labeled_reconstructed() {
    let inspection = load_or_reconstruct(None, "claude", "42", "abcdefgh-session", || async {
        Ok(("--key secret-value\nrebuilt current settings".into(), 0))
    })
    .await
    .unwrap();
    let messages = inspection.messages("abcdefgh-session", false);
    assert!(messages[0].contains("재구성"));
    assert!(messages[0].contains("시작 알 수 없음"));
    assert!(messages[0].contains("앵커 없음"));
    assert!(messages[1].contains("--key ***"));
    assert!(!messages.iter().any(|m| m.contains("secret-value")));
}

#[tokio::test]
async fn system_prompt_saved_snapshot_and_pg_absence_choose_correct_source_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let saved = provider_session_prompts::SessionPrompt {
        provider: "codex".into(),
        channel_id: "42".into(),
        provider_session_id: "native".into(),
        source_turn_id: "first-turn".into(),
        started_at: chrono::Utc::now(),
        system_prompt: "actual developer_instructions\n[ANCHOR MEMORY]\n- original".into(),
        anchor_count: 1,
    };
    provider_session_prompts::save(&pool, &saved).await.unwrap();
    let inspection = load_or_reconstruct(Some(&pool), "codex", "42", "native", || async {
        panic!("must not rebuild a saved original")
    })
    .await
    .unwrap();
    assert_eq!(inspection.prompt, saved.system_prompt);
    assert!(inspection.messages("native", false)[0].ends_with("원본"));
    assert!(inspection.messages("native", false)[0].contains("앵커 1개"));
    let missing = load_or_reconstruct(Some(&pool), "codex", "42", "other-native", || async {
        Ok(("rebuilt".into(), 0))
    })
    .await
    .unwrap();
    assert!(missing.messages("other-native", false)[0].ends_with("재구성"));
    assert!(missing.messages("other-native", false)[0].contains("시작 알 수 없음"));
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn system_prompt_idle_channel_uses_latest_scoped_snapshot_read_only_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let saved = provider_session_prompts::SessionPrompt {
        provider: "codex".into(),
        channel_id: "42".into(),
        provider_session_id: "z-latest-session".into(),
        source_turn_id: "first-turn".into(),
        started_at: chrono::Utc::now() - chrono::Duration::days(1),
        system_prompt: "saved before restart".into(),
        anchor_count: 0,
    };
    provider_session_prompts::save(&pool, &saved).await.unwrap();
    for (provider, channel, session, offset) in [
        ("codex", "42", "zz-older-session", -1),
        ("codex", "42", "a-same-time", 0),
        ("claude", "42", "wrong-provider", 60),
        ("codex", "43", "wrong-channel", 60),
    ] {
        let mut other = saved.clone();
        other.provider = provider.into();
        other.channel_id = channel.into();
        other.provider_session_id = session.into();
        other.started_at += chrono::Duration::seconds(offset);
        other.system_prompt = "must not select this row".into();
        provider_session_prompts::save(&pool, &other).await.unwrap();
    }
    let read_only = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET default_transaction_read_only = on")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let (provider, session, historical) =
        inspection_binding(Some(&read_only), "42", None, ProviderKind::Codex, None)
            .await
            .unwrap()
            .expect("idle channel must use the stored session");
    assert_eq!(session, saved.provider_session_id);
    assert!(historical);
    let inspection = load_or_reconstruct(
        Some(&read_only),
        provider.as_str(),
        "42",
        &session,
        || async { panic!("must not reconstruct an idle channel's saved prompt") },
    )
    .await
    .unwrap();
    assert_eq!(inspection.prompt, saved.system_prompt);
    let messages = inspection.messages(&session, historical);
    assert!(messages[0].contains("원본"));
    assert!(messages[0].contains("현재 세션 없음 · 최근 저장본"));
    assert!(messages[0].contains(&saved.started_at.format("%Y-%m-%d %H:%M:%S UTC").to_string()));
    assert!(messages[1].contains("saved before restart"));
    assert!(
        inspection_binding(Some(&read_only), "44", None, ProviderKind::Codex, None)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        inspection_binding(
            Some(&read_only),
            "42",
            None,
            ProviderKind::Codex,
            Some("current-session".into())
        )
        .await
        .unwrap(),
        Some((ProviderKind::Codex, "current-session".into(), false))
    );
    read_only.close().await;
    pool.close().await;
    db.drop().await;
}
