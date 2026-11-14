//! An effort level saved by `/effort` on a local Claude TUI, kept across a restart and launched
//! with on the same session, over an inherited effort variable.

use super::model_tests::{TOKEN, model_confirm, transcript};
use super::*;
use crate::services::discord::commands::tui_passthrough::effort_save_notice;

impl Fixture {
    /// Saves `level` through the `/effort` command, its pending fast change dropped so the effort
    /// change stands alone; what the command said.
    async fn save_effort(&self, level: &str) -> String {
        let keys = &self.shared.overrides.fast_mode_session_reset_pending;
        keys.clear();
        let (shared, channel, provider) = (&self.shared, self.channel, &ProviderKind::Claude);
        let owner = self.own(None).await;
        // The running bot holds its token, so a setting released by the command is saved too.
        let _ = self.shared.http.cached_bot_token.set(TOKEN.to_string());
        let said = effort_save_notice(shared, TOKEN, channel, provider, level).await;
        self.own(Some(owner)).await;
        said
    }

    /// The level a turn on the channel launches with, as the turn start reads it.
    async fn effort(&self) -> Option<String> {
        let read = crate::services::discord::commands::channel_effort_level;
        read(&self.shared, &ProviderKind::Claude, self.channel).await
    }

    fn stored_effort(&self) -> Option<String> {
        let stored = crate::services::discord::settings::load_bot_settings(TOKEN);
        let key = self.channel.get().to_string();
        stored.channel_effort_levels.get(&key).cloned()
    }

    async fn effort_state(&self) -> (Option<String>, bool, Option<String>, bool) {
        let key = self.channel.get().to_string();
        let settings = self.shared.settings.read().await;
        let memory = settings.channel_effort_levels.get(&key).cloned();
        let pending = settings.channel_relaunch_pending.contains(&key);
        (
            memory,
            pending,
            self.stored_effort(),
            self.stored_relaunch_pending(),
        )
    }
}

// A saved level ends the idle TUI at once and survives a server restart; the next launch resumes
// its session with `--effort` over an inherited effort variable, and no relaunch is left over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_saved_effort_relaunches_its_tui_on_the_same_session_pg() {
    let test = "services::discord::commands::control::relaunch_tests::effort_tests::a_saved_effort_relaunches_its_tui_on_the_same_session_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(20, RuntimeHandoffKind::ClaudeTui).await;
    let _idle = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);

    let said = fx.save_effort("high").await;
    assert!(said.contains("effort를 `high`로 저장했습니다"), "{said}");
    assert!(said.contains("다음 입력부터 같은 세션"), "{said}");
    let calls = fx.tmux.take_calls();
    assert!(
        !called(&calls, "send-keys"),
        "nothing is typed into the TUI"
    );
    assert!(called(&calls, "kill-session"), "the TUI ends with the save");
    let reset_asked = &fx.shared.overrides.session_reset_pending;
    assert!(!reset_asked.contains(&fx.channel), "no reset is asked");
    let session = fx.session_id().await;
    assert_eq!(session.as_deref(), Some(SESSION), "session kept");
    let kept = (Some("work".to_string()), Some(format!("{SESSION}\nwork")));
    assert_eq!(fx.markers(), kept, "account markers kept");
    assert!(!fx.stored_relaunch_pending(), "no relaunch is left on disk");
    fx.restart().await;
    assert_eq!(
        fx.effort().await.as_deref(),
        Some("high"),
        "the level outlives a restart"
    );
    assert!(!fx.relaunch_pending().await, "and brings no relaunch back");
    assert!(
        fx.consume().await.is_none(),
        "the next turn restarts nothing"
    );
    assert!(!called(&fx.tmux.take_calls(), "kill-session"));

    let set = TestEnvVarGuard::set_value_after_shared_test_env_lock;
    let _inherited = set("CLAUDE_CODE_EFFORT_LEVEL", std::ffi::OsStr::new("max"));
    let effort = fx.effort().await;
    let (args, _, env) = fx.launch_with(fx.channel, &fx.name, SESSION, None, effort.as_deref());
    assert!(args.windows(2).any(|pair| pair == ["--resume", SESSION]));
    assert!(
        args.windows(2).any(|pair| pair == ["--effort", "high"]),
        "{args:?}"
    );
    assert_eq!(env, "-", "the inherited effort variable is cleared");
    fx.tmux.mark_present();
    fx.tmux.take_calls();

    let said = fx.save_effort("high").await;
    assert!(said.contains("이미 `high`"), "{said}");
    assert!(
        !called(&fx.tmux.take_calls(), "kill-session"),
        "an unchanged level ends nothing"
    );
    tc::cleanup_session_temp_files(&fx.name);
}

// `/clear` drops the level's pending relaunch in memory and on disk but keeps the level, which the
// next launch still takes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_keeps_the_saved_effort_and_drops_its_relaunch_pg() {
    let test = "services::discord::commands::control::relaunch_tests::effort_tests::clear_keeps_the_saved_effort_and_drops_its_relaunch_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(21, RuntimeHandoffKind::ClaudeTui).await;
    let _idle = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);
    // The TUI does not end, so the level waits for the next turn's relaunch.
    fx.tmux.fail_kills();
    let said = fx.save_effort("low").await;
    assert!(said.contains("지금은 재기동하지 않았습니다"), "{said}");
    assert!(fx.stored_relaunch_pending());
    fx.tmux.take_calls();

    super::super::native::clear_process_reset_pending(&fx.shared, fx.channel).await;
    let low = Some("low".to_string());
    assert_eq!(fx.effort_state().await, (low.clone(), false, low, false));
    assert!(fx.consume().await.is_none());
    assert!(!called(&fx.tmux.take_calls(), "kill-session"));
    // A later launch of the channel, its pane gone for another reason.
    fx.tmux.mark_missing();
    let effort = fx.effort().await;
    let (args, _, _) = fx.launch_with(fx.channel, &fx.name, SESSION, None, effort.as_deref());
    assert!(
        args.windows(2).any(|pair| pair == ["--effort", "low"]),
        "{args:?}"
    );
    tc::cleanup_session_temp_files(&fx.name);
}

// A Herdr channel and a channel off the local TUI refuse the save and keep the setting, its pending
// relaunch and the session as they were; without a level the CLI takes, the launch adds no effort.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn effort_off_the_local_tui_is_refused_with_the_setting_kept_pg() {
    let test = "services::discord::commands::control::relaunch_tests::effort_tests::effort_off_the_local_tui_is_refused_with_the_setting_kept_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(22, RuntimeHandoffKind::ClaudeTui).await;
    let _idle = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);
    let untouched = (None, false, None, false);

    let local = crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
    let herdr =
        crate::config::session_hosts::force_for_test(Some(&local), &[(fx.channel.get(), &local)]);
    let said = fx.save_effort("max").await;
    assert!(
        said.contains("effort 적용 미확인") && said.contains("pane 실행 기록 미확인"),
        "{said}"
    );
    assert_eq!(fx.effort_state().await, untouched);
    assert!(!called(&fx.tmux.take_calls(), "send-keys"));
    drop(herdr);

    let mut headless = crate::config::Config::default();
    let claude = crate::config::ProviderConfig {
        tui_hosting: Some(false),
        ..Default::default()
    };
    headless.providers.insert("claude".into(), claude);
    crate::services::provider_hosting::install_provider_hosting_config(&headless);
    let said = fx.save_effort("max").await;
    assert!(said.contains("로컬 Claude TUI 채널에서만"), "{said}");
    assert_eq!(fx.effort_state().await, untouched);
    assert!(fx.consume().await.is_none());
    assert!(!called(&fx.tmux.take_calls(), "kill-session"));
    assert_eq!(fx.session_id().await.as_deref(), Some(SESSION));

    let key = fx.channel.get().to_string();
    let mut settings = fx.shared.settings.write().await;
    settings
        .channel_effort_levels
        .insert(key, "ultracode".into());
    drop(settings);
    assert_eq!(fx.effort().await, None, "an unknown level is not launched");
    let _tui = HostingConfig::tui();
    fx.tmux.mark_missing();
    let set = TestEnvVarGuard::set_value_after_shared_test_env_lock;
    let _inherited = set("CLAUDE_CODE_EFFORT_LEVEL", std::ffi::OsStr::new("max"));
    let effort = fx.effort().await;
    let (args, _, env) = fx.launch_with(fx.channel, &fx.name, SESSION, None, effort.as_deref());
    assert!(!args.iter().any(|arg| arg == "--effort"), "{args:?}");
    assert_eq!(env, "max", "without a level the inherited variable stays");
    // Values the channel does not take are refused before any save: an unprobed ultra, the raw
    // CLI word, and text that is no level, which is not echoed back.
    let before = fx.effort_state().await;
    fx.tmux.take_calls();
    for (given, said_part) in [
        ("ultra", "확인하지 못했습니다"),
        ("ultracode", "알 수 없는 effort 값"),
        ("@everyone", "알 수 없는 effort 값"),
    ] {
        let said = fx.save_effort(given).await;
        assert!(
            said.starts_with("effort를 바꾸지 않았습니다") && said.contains(said_part),
            "{said}"
        );
        assert!(!said.contains("@everyone"), "{said}");
        assert_eq!(fx.effort_state().await, before, "{given}");
    }
    assert!(!called(&fx.tmux.take_calls(), "kill-session"));
    tc::cleanup_session_temp_files(&fx.name);
}

// The relaunch confirmation reports the effort of the first answer after the old process beside the
// level asked, and never takes an answer without one for the level applied.
#[tokio::test]
async fn the_confirmation_reports_the_answer_effort_beside_the_level_asked() {
    let dir = tempfile::tempdir().unwrap();
    let same = || async { Some(SESSION.to_string()) };
    write_record(dir.path(), SESSION, 1_000.0);
    let model = "응답 모델 `claude-sonnet-5-5`(요청 `sonnet`)";
    let cases = [
        (Some("high"), "응답 effort `high`(요청 `high`)"),
        (Some("low"), "응답 effort `low`(요청 `high`와 다름)"),
        (None, "effort 적용 미확인(요청 `high`)"),
    ];
    for (seen, said) in cases {
        let answer = serde_json::json!({"type": "assistant", "sessionId": SESSION,
            "effort": seen, "message": {"model": "claude-sonnet-5-5"}});
        let (path, offset) = transcript(dir.path(), "claude-haiku-5-5", Some(answer));
        let mut check = model_confirm(path, offset);
        check.model.as_mut().unwrap().effort = Some("high".to_string());
        let notice = confirm_notice(&check, dir.path(), same, &WAITS).await;
        assert!(
            notice.ends_with(&format!("\n{model} · {said}.")),
            "{notice}"
        );
    }
}
