use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;

#[test]
fn system_prompt_active_native_binding_precedes_persisted_session() {
    use crate::services::provider::{CancelToken, session_prompt};
    let token = CancelToken::new();
    session_prompt::arm(&token, None, &ProviderKind::Codex, 42, "first-turn", 0);
    session_prompt::observe(&token, "new-native");
    assert_eq!(
        inspection_binding(
            Some(&token),
            ProviderKind::Claude,
            Some("old-native".into())
        ),
        Some((ProviderKind::Codex, "new-native".into()))
    );
    assert_eq!(
        inspection_binding(Some(&token), ProviderKind::Claude, None),
        Some((ProviderKind::Codex, "new-native".into()))
    );
    assert_eq!(
        inspection_binding(None, ProviderKind::Claude, Some("persisted".into())),
        Some((ProviderKind::Claude, "persisted".into()))
    );
    assert_eq!(inspection_binding(None, ProviderKind::Claude, None), None);
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
    let messages = inspection.messages("abcdefgh-session");
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
    assert!(inspection.messages("native")[0].ends_with("원본"));
    assert!(inspection.messages("native")[0].contains("앵커 1개"));
    let missing = load_or_reconstruct(Some(&pool), "codex", "42", "other-native", || async {
        Ok(("rebuilt".into(), 0))
    })
    .await
    .unwrap();
    assert!(missing.messages("other-native")[0].ends_with("재구성"));
    assert!(missing.messages("other-native")[0].contains("시작 알 수 없음"));
    pool.close().await;
    db.drop().await;
}
