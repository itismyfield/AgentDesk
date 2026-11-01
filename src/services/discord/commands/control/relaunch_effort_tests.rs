//! An effort level saved by `/effort` on a local Claude TUI, kept across a restart and launched
//! with on the same session, over an inherited effort variable.

use super::model_tests::{TOKEN, model_confirm, relaunched_as, transcript};
use super::*;
use crate::services::discord::commands::tui_passthrough::{EffortLevel, effort_save_notice};

impl Fixture {
    /// Saves `level` through the `/effort` command, its pending fast change dropped so the effort
    /// change stands alone; what the command said.
    async fn save_effort(&self, level: EffortLevel) -> String {
        let keys = &self.shared.overrides.fast_mode_session_reset_pending;
        keys.clear();
        let (shared, channel, provider) = (&self.shared, self.channel, &ProviderKind::Claude);
        let owner = self.own(None).await;
        let said = effort_save_notice(shared, TOKEN, channel, provider, level).await;
        self.own(Some(owner)).await;
        let _ = self.shared.http.cached_bot_token.set(TOKEN.to_string());
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

// A saved level survives a restart, restarts the TUI once on its own session, and the next launch
// takes it as `--effort` with an inherited effort variable cleared so it cannot outrank the level.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_saved_effort_relaunches_its_tui_on_the_same_session_pg() {
    let test = "services::discord::commands::control::relaunch_tests::effort_tests::a_saved_effort_relaunches_its_tui_on_the_same_session_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(20, RuntimeHandoffKind::ClaudeTui).await;
    let _idle = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);

    let said = fx.save_effort(EffortLevel::High).await;
    assert!(said.contains("effort를 `high`로 저장했습니다"), "{said}");
    let typed = called(&fx.tmux.take_calls(), "send-keys");
    assert!(!typed, "nothing is typed into the TUI");
    let reset_asked = &fx.shared.overrides.session_reset_pending;
    assert!(!reset_asked.contains(&fx.channel), "no reset is asked");
    fx.restart().await;
    assert_eq!(
        fx.effort().await.as_deref(),
        Some("high"),
        "the level outlives a restart"
    );
    assert!(fx.relaunch_pending().await, "and so does its relaunch");

    let reset = fx.consume().await;
    assert!(relaunched_as(&reset, &fx.name), "{reset:?}");
    assert!(called(&fx.tmux.take_calls(), "kill-session"));
    let session = fx.session_id().await;
    assert_eq!(session.as_deref(), Some(SESSION), "session kept");
    let kept = (Some("work".to_string()), Some(format!("{SESSION}\nwork")));
    assert_eq!(fx.markers(), kept, "account markers kept");
    assert!(
        !fx.stored_relaunch_pending(),
        "the relaunch is dropped on disk"
    );
    super::super::relaunch::forget(fx.channel);

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
    assert!(fx.consume().await.is_none(), "the change applies once");
    assert!(!called(&fx.tmux.take_calls(), "kill-session"));

    let said = fx.save_effort(EffortLevel::High).await;
    assert!(said.contains("이미 `high`"), "{said}");
    assert!(
        !fx.relaunch_pending().await,
        "an unchanged level asks for nothing"
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
    fx.save_effort(EffortLevel::Low).await;
    assert!(fx.stored_relaunch_pending());

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
// relaunch and the session as they were; a stored level the CLI does not take launches nothing.
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
    let said = fx.save_effort(EffortLevel::Max).await;
    assert!(
        said.contains("Herdr 채널의 Claude TUI에는 /effort"),
        "{said}"
    );
    assert_eq!(fx.effort_state().await, untouched);
    drop(herdr);

    let mut headless = crate::config::Config::default();
    let claude = crate::config::ProviderConfig {
        tui_hosting: Some(false),
        ..Default::default()
    };
    headless.providers.insert("claude".into(), claude);
    crate::services::provider_hosting::install_provider_hosting_config(&headless);
    let said = fx.save_effort(EffortLevel::Max).await;
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
    let said = effort_save_notice(
        &fx.shared,
        TOKEN,
        fx.channel,
        &ProviderKind::Codex,
        EffortLevel::Max,
    );
    assert!(said.await.contains("Codex의 reasoning effort"));
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
