//! A model change saved on a local Claude TUI, kept across a restart and applied at the next turn
//! by restarting the TUI on its own session.

use super::*;

const TOKEN: &str = "test-token";

impl Fixture {
    /// Names `node`, else this node, as the owner on the channel's sessions row, as a save checks;
    /// returns the owner it named before.
    async fn own(&self, node: Option<String>) -> String {
        let local =
            crate::services::cluster::node_registry::resolve_self_instance_id_without_config;
        let pool = self.shared.pg_pool.as_ref().unwrap();
        let channel = self.channel.get().to_string();
        let before = sqlx::query_scalar("SELECT instance_id FROM sessions WHERE channel_id = $1")
            .bind(&channel)
            .fetch_one(pool)
            .await
            .unwrap();
        sqlx::query("UPDATE sessions SET instance_id = $2 WHERE channel_id = $1")
            .bind(&channel)
            .bind(node.unwrap_or_else(local))
            .execute(pool)
            .await
            .unwrap();
        before
    }

    /// Saves `model` for the channel through the real save, its pending fast change dropped so the
    /// model change stands alone.
    async fn save_model(&self, model: Option<&str>) {
        let keys = &self.shared.overrides.fast_mode_session_reset_pending;
        keys.clear();
        let save = super::super::super::config::update_channel_model_override;
        let model = model.map(str::to_string);
        let channel = self.channel;
        let owner = self.own(None).await;
        let saved = save(&self.shared, TOKEN, channel, &ProviderKind::Claude, model).await;
        self.own(Some(owner)).await;
        assert!(saved.unwrap(), "the change is saved");
        let _ = self.shared.http.cached_bot_token.set(TOKEN.to_string());
    }

    async fn relaunch_pending(&self) -> bool {
        let key = self.channel.get().to_string();
        let settings = self.shared.settings.read().await;
        settings.channel_relaunch_pending.contains(&key)
    }

    fn stored_relaunch_pending(&self) -> bool {
        let stored = crate::services::discord::settings::load_bot_settings(TOKEN);
        let key = self.channel.get().to_string();
        stored.channel_relaunch_pending.contains(&key)
    }

    /// A restart: memory starts empty, the boot audit rewrites the settings file, and the settings
    /// are read back from disk.
    async fn restart(&self) {
        self.shared
            .settings
            .write()
            .await
            .channel_relaunch_pending
            .clear();
        let root = crate::config::runtime_root().unwrap();
        let audit = crate::services::discord_config_audit::audit_and_reconcile_config_only;
        let loaded = crate::services::discord_config_audit::load_runtime_config(&root).unwrap();
        audit(&root, loaded.config, loaded.path, loaded.existed, false).unwrap();
        let stored = crate::services::discord::settings::load_bot_settings(TOKEN);
        *self.shared.settings.write().await = stored;
    }

    async fn requested_model(&self) -> Option<String> {
        let resolve = crate::services::discord::commands::resolve_model_for_turn;
        resolve(&self.shared, self.channel, &ProviderKind::Claude).await
    }
}

/// Makes `agentdesk.yaml` manage the bot, so saves take the runtime-only settings entry.
fn manage_bot_in_yaml() {
    let path = crate::config::resolved_config_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let server = "server:\n  host: 127.0.0.1\n  port: 18791\n";
    let yaml = format!(
        "{server}discord:\n  bots:\n    lane:\n      token: {TOKEN}\n      provider: claude\n"
    );
    std::fs::write(path, yaml).unwrap();
}

fn relaunched_as(reset: &Option<ManagedReset>, name: &str) -> bool {
    matches!(reset, Some(ManagedReset::Applied(Some(applied))) if applied == name)
}

// A saved model change survives a restart through either settings file, restarts only the TUI
// whose model changed, on its own session, once, and the next launch resumes it with that model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_saved_model_relaunches_its_tui_once_on_the_same_session_pg() {
    let test = "services::discord::commands::control::relaunch_tests::model_tests::a_saved_model_relaunches_its_tui_once_on_the_same_session_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(10, RuntimeHandoffKind::ClaudeTui).await;
    let _idle = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);
    const THREAD: &str = "64860000-0000-4000-8000-000000000011";
    let (thread, thread_name) = sharing_thread(&fx, 11, THREAD, Some(true)).await;

    fx.save_model(Some("sonnet")).await;
    let reset_asked = &fx.shared.overrides.session_reset_pending;
    assert!(!reset_asked.contains(&fx.channel), "no reset is asked");
    fx.restart().await;
    assert!(fx.relaunch_pending().await, "the change outlives a restart");
    assert!(
        fx.consume_on(thread).await.is_none(),
        "an unchanged TUI runs on"
    );
    assert!(!called(&fx.tmux.take_calls(), "kill-session"));

    let reset = fx.consume().await;
    assert!(relaunched_as(&reset, &fx.name), "{reset:?}");
    assert!(called(&fx.tmux.take_calls(), "kill-session"));
    assert_eq!(
        fx.session_id().await.as_deref(),
        Some(SESSION),
        "session kept"
    );
    let api = fx.api.take();
    let cleared = api.iter().any(|call| call.contains("clear-session-id"));
    assert!(!cleared, "the DB selector is kept: {api:?}");
    let kept = (Some("work".to_string()), Some(format!("{SESSION}\nwork")));
    assert_eq!(fx.markers(), kept, "account markers kept");
    assert!(!fx.relaunch_pending().await, "the change is consumed");
    assert!(!fx.stored_relaunch_pending(), "and dropped on disk");
    let said = fx.said();
    let announced = said.iter().any(|call| call.contains("TUI를 재기동합니다"));
    assert!(announced, "{said:?}");
    super::super::relaunch::forget(fx.channel);

    let model = fx.requested_model().await;
    let (args, _) = fx.launch(fx.channel, &fx.name, SESSION, model.as_deref());
    assert!(args.windows(2).any(|pair| pair == ["--resume", SESSION]));
    assert!(
        args.windows(2).any(|pair| pair == ["--model", "sonnet"]),
        "{args:?}"
    );
    fx.tmux.mark_present();
    fx.tmux.take_calls();
    assert!(fx.consume().await.is_none(), "the change applies once");
    assert!(!called(&fx.tmux.take_calls(), "kill-session"));

    manage_bot_in_yaml();
    fx.save_model(None).await;
    let root = crate::config::runtime_root().unwrap();
    let file = crate::runtime_layout::config_dir(&root).join("bot_settings.json");
    let stored = std::fs::read_to_string(file);
    assert!(
        !stored.unwrap().contains(TOKEN),
        "the runtime-only entry is written"
    );
    fx.restart().await;
    let reset = fx.consume().await;
    assert!(relaunched_as(&reset, &fx.name), "{reset:?}");
    super::super::relaunch::forget(fx.channel);
    let model = fx.requested_model().await;
    let (args, _) = fx.launch(fx.channel, &fx.name, SESSION, model.as_deref());
    assert!(args.windows(2).any(|pair| pair == ["--resume", SESSION]));
    assert!(
        args.windows(2).any(|pair| pair == ["--model", "default"]),
        "a resumed session returns to the default model: {args:?}"
    );
    tc::cleanup_session_temp_files(&fx.name);
    tc::cleanup_session_temp_files(&thread_name);
}

// A model change whose session left no transcript cannot resume: the session is reset, the channel
// is told its context is gone, and the change is consumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_change_without_a_transcript_resets_with_context_loss_pg() {
    let test = "services::discord::commands::control::relaunch_tests::model_tests::a_model_change_without_a_transcript_resets_with_context_loss_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(14, RuntimeHandoffKind::ClaudeTui).await;
    let _idle = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);
    fx.save_model(Some("sonnet")).await;
    let path = crate::services::claude_tui::transcript_tail::claude_transcript_path;
    std::fs::remove_file(path(fx.cwd.path(), SESSION, Some(&fx._account.2)).unwrap()).unwrap();

    let reset = fx.consume().await;
    assert!(matches!(reset, Some(ManagedReset::Applied(_))), "{reset:?}");
    assert_eq!(fx.session_id().await, None, "the session is reset");
    assert!(!fx.relaunch_pending().await);
    let said = fx.said();
    let lost = said
        .iter()
        .any(|call| call.contains("새 세션으로 시작합니다(맥락 소실)"));
    assert!(lost, "{said:?}");
    tc::cleanup_session_temp_files(&fx.name);
}

// A model change finding a live pane that is not the Claude TUI resets it as before and says the
// context is gone, though the save promised to keep it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_change_on_another_runtime_resets_with_context_loss_pg() {
    let test = "services::discord::commands::control::relaunch_tests::model_tests::a_model_change_on_another_runtime_resets_with_context_loss_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(15, RuntimeHandoffKind::LegacyTmuxWrapper).await;
    let _idle = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);
    fx.save_model(Some("sonnet")).await;

    let reset = fx.consume().await;
    assert!(matches!(reset, Some(ManagedReset::Applied(_))), "{reset:?}");
    assert_eq!(fx.session_id().await, None, "the session is reset");
    assert!(!fx.relaunch_pending().await);
    let said = fx.said();
    let lost = said
        .iter()
        .any(|call| call.contains("세션을 리셋했습니다(맥락 소실)"));
    assert!(lost, "{said:?}");
    let relaunched = said.iter().any(|call| call.contains("TUI를 재기동합니다"));
    assert!(!relaunched, "{said:?}");
    tc::cleanup_session_temp_files(&fx.name);
}

// `/clear` drops a saved relaunch in memory and on disk: the next turn restarts nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_drops_the_saved_relaunch_pg() {
    let test = "services::discord::commands::control::relaunch_tests::model_tests::clear_drops_the_saved_relaunch_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(16, RuntimeHandoffKind::ClaudeTui).await;
    let _idle = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);
    fx.save_model(Some("sonnet")).await;
    assert!(fx.stored_relaunch_pending());

    super::super::native::clear_process_reset_pending(&fx.shared, fx.channel).await;
    assert!(!fx.relaunch_pending().await);
    assert!(!fx.stored_relaunch_pending(), "dropped on disk");
    assert!(fx.consume().await.is_none());
    assert!(!called(&fx.tmux.take_calls(), "kill-session"));
    tc::cleanup_session_temp_files(&fx.name);
}

// A new save gives every TUI sharing the setting its retry again, though the saved value is the one
// whose kill already failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_save_restores_the_retry_of_each_tui_sharing_it_pg() {
    let test = "services::discord::commands::control::relaunch_tests::model_tests::a_new_save_restores_the_retry_of_each_tui_sharing_it_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(12, RuntimeHandoffKind::ClaudeTui).await;
    let _idle = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);
    const THREAD: &str = "64860000-0000-4000-8000-000000000013";
    let (thread, thread_name) = sharing_thread(&fx, 13, THREAD, Some(false)).await;
    fx.tmux.fail_kills();
    assert!(fx.consume().await.is_none());
    assert!(fx.consume_on(thread).await.is_none());

    let save = super::super::super::config::update_channel_fast_mode;
    let owner = fx.own(None).await;
    for enabled in [false, true] {
        let saved = save(
            &fx.shared,
            TOKEN,
            fx.channel,
            &ProviderKind::Claude,
            enabled,
            fx.channel,
        );
        assert!(saved.await.unwrap());
    }
    fx.own(Some(owner)).await;
    assert!(fx.consume().await.is_none());
    assert!(fx.pending(), "the new save keeps its retry");
    assert!(fx.consume_on(thread).await.is_none());
    let said = fx.said();
    let retries = said
        .iter()
        .filter(|call| call.contains("한 번 더 시도"))
        .count();
    assert_eq!(retries, 4, "{said:?}");
    assert!(
        !said.iter().any(|call| call.contains("2회 실패")),
        "{said:?}"
    );
    super::super::relaunch::forget(fx.channel);
    super::super::relaunch::forget(thread);
    tc::cleanup_session_temp_files(&fx.name);
    tc::cleanup_session_temp_files(&thread_name);
}

/// A transcript holding one answer of `session` by `old`, then, past the returned offset, `new`.
fn transcript(dir: &Path, old: &str, new: Option<Value>) -> (PathBuf, u64) {
    let path = dir.join("session.jsonl");
    let answer = |model: &str| {
        serde_json::json!({"type": "assistant", "sessionId": SESSION,
            "message": {"model": model}})
        .to_string()
    };
    let before = format!("{}\n", answer(old));
    let after = new.map_or(String::new(), |record| format!("{record}\n"));
    std::fs::write(&path, format!("{before}{after}")).unwrap();
    (path, before.len() as u64)
}

fn model_confirm(transcript: PathBuf, offset: u64) -> Confirm {
    let model = super::super::relaunch_confirm::ModelCheck {
        requested: Some("sonnet".to_string()),
        transcript: Some(transcript),
        offset,
    };
    Confirm {
        model: Some(model),
        ..confirm(999.0)
    }
}

// The model a change brought up is the first answer written after the old process ended, reported
// beside what was asked; an answer the API failed is not taken for the model.
#[tokio::test]
async fn the_confirmation_reports_the_first_answer_after_the_old_process() {
    let dir = tempfile::tempdir().unwrap();
    let same = || async { Some(SESSION.to_string()) };
    write_record(dir.path(), SESSION, 1_000.0);
    let status = "재기동 확인 — 같은 세션 `64860000` · 표시 모델 `claude-opus-4-7` · effort `high` · fast 설정 전달됨(켬 요청, 표시 꺼짐). 실제 활성은 계정·모델 조건에 따릅니다";
    let new = serde_json::json!({"type": "assistant", "sessionId": SESSION,
        "message": {"model": "claude-sonnet-5-5"}});

    let (path, offset) = transcript(dir.path(), "claude-haiku-5-5", Some(new));
    let notice = confirm_notice(&model_confirm(path, offset), dir.path(), same, &WAITS).await;
    assert_eq!(
        notice,
        format!("{status}\n응답 모델 `claude-sonnet-5-5`(요청 `sonnet`).")
    );

    let failed = serde_json::json!({"type": "assistant", "sessionId": SESSION,
        "isApiErrorMessage": true, "message": {"model": "<synthetic>"}});
    let (path, offset) = transcript(dir.path(), "claude-haiku-5-5", Some(failed));
    let notice = confirm_notice(&model_confirm(path, offset), dir.path(), same, &WAITS).await;
    assert_eq!(
        notice,
        format!("{status}\n모델 적용 미확인(응답 오류, 요청 `sonnet`).")
    );

    let (path, offset) = transcript(dir.path(), "claude-haiku-5-5", None);
    let notice = confirm_notice(&model_confirm(path, offset), dir.path(), same, &WAITS).await;
    assert_eq!(
        notice,
        format!("{status}\n응답 모델 미확인(요청 `sonnet`).")
    );
}

// A move to another session while the answer is awaited ends the wait with the context-loss notice.
#[tokio::test]
async fn a_switch_while_awaiting_the_answer_ends_with_its_notice() {
    let dir = tempfile::tempdir().unwrap();
    write_record(dir.path(), SESSION, 1_000.0);
    let (path, offset) = transcript(dir.path(), "claude-haiku-5-5", None);
    let rounds = std::sync::atomic::AtomicUsize::new(0);
    let current = || {
        let round = rounds.fetch_add(1, Ordering::SeqCst);
        async move {
            let other = "77770000-0000-4000-8000-000000000002";
            Some(if round < 20 { SESSION } else { other }.to_string())
        }
    };

    let notice = confirm_notice(&model_confirm(path, offset), dir.path(), current, &WAITS).await;
    assert_eq!(
        notice,
        "재기동 뒤 다른 세션 `77770000`으로 시작됐습니다(이전 세션 `64860000`의 맥락 소실)."
    );
}

// A channel moved to Herdr after the save is not restarted: the change is dropped for its next
// launch to take, and the channel is told so once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_channel_moved_to_herdr_drops_the_relaunch_with_a_notice_pg() {
    let test = "services::discord::commands::control::relaunch_tests::model_tests::a_channel_moved_to_herdr_drops_the_relaunch_with_a_notice_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(17, RuntimeHandoffKind::ClaudeTui).await;
    let _idle = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);
    fx.save_model(Some("sonnet")).await;
    let local = crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
    let _herdr =
        crate::config::session_hosts::force_for_test(Some(&local), &[(fx.channel.get(), &local)]);

    assert!(fx.consume().await.is_none());
    assert!(!called(&fx.tmux.take_calls(), "kill-session"));
    assert!(
        !fx.relaunch_pending().await,
        "the change is left to the next launch"
    );
    let said = fx.said();
    let told = said
        .iter()
        .filter(|call| call.contains("Herdr 채널로 바뀌어"))
        .count();
    assert_eq!(told, 1, "{said:?}");
    assert!(fx.consume().await.is_none());
    assert!(fx.said().is_empty(), "told once");
    tc::cleanup_session_temp_files(&fx.name);
}
