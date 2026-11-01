//! A fast change on a local Claude TUI, applied by restarting it on its own session, through the
//! turn-start consumer and the launch that follows.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use poise::serenity_prelude::ChannelId;
use serde_json::Value;

use super::relaunch_confirm::{Confirm, Waits, confirm_notice};
use super::*;
use crate::config::TestEnvVarGuard;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::admin_host_guard::tests::{Recorder, api_child};
use crate::services::discord::host_defer_gate::tests::{Case, map_channel, postgres};
use crate::services::discord::host_teardown_gate::test_support::{Stored, channel_key, shared_on};
use crate::services::provider_teardown::tests::test_support::{FakeTmux, cleared};
use crate::services::tmux_common as tc;

const SESSION: &str = "64860000-0000-4000-8000-000000000001";

/// A channel whose live tmux pane runs `kind` on `SESSION` under the `work` account, with a legacy
/// sessions row, a saved fast setting and its pending change.
struct Fixture {
    shared: Arc<SharedData>,
    channel: ChannelId,
    name: String,
    cwd: tempfile::TempDir,
    tmux: FakeTmux,
    discord: Recorder,
    api: Recorder,
    /// The agent a turn on the channel runs as, as its provider context names it.
    agent: Option<&'static str>,
    _hosting: HostingConfig,
    _account: (TestEnvVarGuard, tempfile::TempDir, PathBuf),
    _db: crate::db::auto_queue::test_support::TestPostgresDb,
}

/// Claude channels on the hosted TUI driver for the fixture's life.
struct HostingConfig;

impl HostingConfig {
    fn tui() -> Self {
        let mut config = crate::config::Config::default();
        let claude = crate::config::ProviderConfig {
            tui_hosting: Some(true),
            ..Default::default()
        };
        config.providers.insert("claude".into(), claude);
        crate::services::provider_hosting::install_provider_hosting_config(&config);
        Self
    }
}

impl Drop for HostingConfig {
    fn drop(&mut self) {
        let default = crate::config::Config::default();
        crate::services::provider_hosting::install_provider_hosting_config(&default);
    }
}

impl Fixture {
    async fn new(n: u64, kind: RuntimeHandoffKind) -> Self {
        let account = crate::services::claude::herdr_return::tests::claude_account("work");
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(1_486_486_000_000_000_000 + n);
        let channel_name = format!("i6486-relaunch-{n}");
        let name = provider.build_tmux_session_name(&channel_name);
        let tmux = FakeTmux::install(&name);
        tmux.become_missing_after_kill();
        let (db, pool) = postgres().await;
        let shared = shared_on(&pool).await;
        let api = Recorder::start().await;
        crate::services::discord::internal_api::init(api.port, None);
        let discord = Recorder::start().await;
        map_channel(&shared, channel, &channel_name).await;
        let cwd = tempfile::tempdir().unwrap();
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        session.session_id = Some(SESSION.to_string());
        session.current_path = Some(cwd.path().to_str().unwrap().to_string());
        drop(core);
        let transcript = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            cwd.path(),
            SESSION,
            Some(&account.2),
        )
        .unwrap();
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{}\n").unwrap();
        tc::write_tmux_runtime_kind_marker(&name, kind).unwrap();
        tc::write_tmux_session_auth_profile(&name, "work").unwrap();
        tc::write_claude_session_auth(&name, SESSION, "work").unwrap();
        Case::Stored(Stored::Legacy)
            .seed(&pool, &channel_key(&shared, &name), &name, channel.get())
            .await;
        let key = super::super::config::fast_mode_reset_pending_key(channel, &provider);
        shared.overrides.fast_mode_session_reset_pending.insert(key);
        let fast = channel.get().to_string();
        shared
            .settings
            .write()
            .await
            .channel_fast_modes
            .insert(fast, true);
        tmux.take_calls();
        Self {
            shared,
            channel,
            name,
            cwd,
            tmux,
            discord,
            api,
            agent: None,
            _hosting: HostingConfig::tui(),
            _account: account,
            _db: db,
        }
    }

    async fn consume(&self) -> Option<ManagedReset> {
        self.consume_on(self.channel).await
    }

    /// A turn start on `channel`, whose fast setting is the fixture channel's.
    async fn consume_on(&self, channel: ChannelId) -> Option<ManagedReset> {
        let http = &self.discord.http;
        let provider = &ProviderKind::Claude;
        reset_provider_session_if_pending(http, &self.shared, provider, channel, self.channel).await
    }

    /// The provider context a turn on `channel` runs its launch under.
    fn context(
        &self,
        channel: ChannelId,
    ) -> crate::services::provider_cli::ProviderExecutionContext {
        crate::services::provider_cli::ProviderExecutionContext {
            provider: "claude".to_string(),
            agent_id: self.agent.map(str::to_string),
            channel_id: Some(channel.get().to_string()),
            ..Default::default()
        }
    }

    /// Binds the channel to agent `owner` on the `work` account while the provider primary stays
    /// the default account.
    fn bind_channel_account(&mut self) {
        let home = self._account.2.display().to_string();
        let org = serde_yaml::to_string(&serde_json::json!({"version": 1,
            "agents": {"owner": {"display_name": "owner", "provider": "claude"}},
            "channels": {"by_id": {self.channel.get().to_string():
                {"agent": "owner", "auth_profile": "work"}}},
            "provider_auth_profiles": {"work": {"provider": "claude", "home": home}}}))
        .unwrap();
        let root = crate::config::runtime_root().unwrap();
        std::fs::write(crate::runtime_layout::org_schema_path(&root), org).unwrap();
        self.agent = Some("owner");
    }

    /// The real streaming entry with fast mode on, launching `name` on `session` with a CLI that
    /// records its arguments and the settings file it was handed; both, once it ran.
    fn launch_fast(&self, channel: ChannelId, name: &str, session: &str) -> (Vec<String>, Value) {
        use std::os::unix::fs::PermissionsExt as _;
        let bin = tempfile::tempdir().unwrap();
        let (claude, argv) = (bin.path().join("claude"), bin.path().join("argv"));
        let settings = bin.path().join("settings.json");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{argv}.tmp'\nwhile [ $# -gt 0 ]; do [ \"$1\" = --settings ] && cp \"$2\" '{}'; shift; done\nmv '{argv}.tmp' '{argv}'\n",
            settings.display(),
            argv = argv.display()
        );
        std::fs::write(&claude, script).unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        let set = TestEnvVarGuard::set_path_after_shared_test_env_lock;
        let _cli = set("AGENTDESK_CLAUDE_PATH", &claude);
        self.tmux.enable_fixture_launch();
        crate::services::platform::tmux::mark_available_from_live_session();
        let _hooks = crate::services::claude_tui::hook_server::publish_hook_endpoint(
            "http://127.0.0.1:9".to_string(),
        );
        let cancel = Arc::new(CancelToken::new());
        let (cwd, context) = (self.cwd.path().to_str().unwrap(), self.context(channel));
        std::thread::scope(|scope| {
            let (token, clearance) = (cancel.clone(), cleared(name));
            let turn = scope.spawn(move || {
                crate::services::platform::with_provider_execution_context(context, || {
                    let (tx, _rx) = std::sync::mpsc::channel();
                    crate::services::claude::execute_command_streaming(
                        "hello",
                        Some(session),
                        cwd,
                        tx,
                        None,
                        None,
                        Some(token),
                        Some(name),
                        Some(&clearance),
                        Some(channel.get()),
                        None,
                        None,
                        Some(true),
                        None,
                        0,
                        None,
                        None,
                    )
                })
            });
            let deadline = Instant::now() + Duration::from_secs(60);
            while !argv.exists() && !turn.is_finished() {
                assert!(Instant::now() < deadline, "the relaunch never ran its CLI");
                std::thread::sleep(Duration::from_millis(50));
            }
            cancel.cancelled.store(true, Ordering::SeqCst);
            let _ = turn.join().unwrap();
        });
        let args = std::fs::read_to_string(&argv).expect("the CLI ran");
        let settings = std::fs::read(&settings).expect("the CLI got a settings file");
        let args = args.lines().map(str::to_string).collect();
        (args, serde_json::from_slice(&settings).unwrap())
    }

    async fn session_id(&self) -> Option<String> {
        let core = self.shared.core.lock().await;
        core.sessions[&self.channel].session_id.clone()
    }

    fn pending(&self) -> bool {
        let pending = super::super::config::fast_mode_reset_pending_for_provider;
        pending(&self.shared, self.channel, &ProviderKind::Claude)
    }

    fn markers(&self) -> (Option<String>, Option<String>) {
        let read = |ext| std::fs::read_to_string(tc::session_temp_path(&self.name, ext)).ok();
        (read("auth_profile"), read("auth_session"))
    }

    fn said(&self) -> Vec<String> {
        let calls = self.discord.take().into_iter();
        calls.filter(|call| call.starts_with("POST ")).collect()
    }

    /// One tmux turn on the channel's session; with `argv` it runs until the launched CLI wrote its
    /// arguments there, then is cancelled.
    fn turn(&self, argv: Option<&Path>) -> Result<(), String> {
        let cancel = Arc::new(CancelToken::new());
        let (cwd, name, channel) = (self.cwd.path().to_str().unwrap(), &self.name, self.channel);
        let context = self.context(channel);
        std::thread::scope(|scope| {
            let (token, clearance) = (cancel.clone(), cleared(name));
            let turn = scope.spawn(move || {
                crate::services::platform::with_provider_execution_context(context, || {
                    let turn = crate::services::claude::tmux_tui_turn_for_tests;
                    turn(cwd, Some(SESSION), token, name, &clearance, channel.get())
                })
            });
            if let Some(argv) = argv {
                let deadline = Instant::now() + Duration::from_secs(60);
                while !argv.exists() && !turn.is_finished() {
                    assert!(Instant::now() < deadline, "the relaunch never ran its CLI");
                    std::thread::sleep(Duration::from_millis(50));
                }
                cancel.cancelled.store(true, Ordering::SeqCst);
            }
            turn.join().unwrap()
        })
    }
}

fn called(calls: &[String], command: &str) -> bool {
    calls.iter().any(|call| call.starts_with(command))
}

// A fast change on a live Claude TUI ends only its process: the session id, the DB selector and the
// account markers stay, so a launch that first fails and then runs resumes the same session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fast_toggle_on_live_claude_tui_relaunches_the_same_session_pg() {
    let test = "services::discord::commands::control::relaunch_tests::fast_toggle_on_live_claude_tui_relaunches_the_same_session_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(1, RuntimeHandoffKind::ClaudeTui).await;
    let _idle = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);
    let kept = (Some("work".to_string()), Some(format!("{SESSION}\nwork")));

    let reset = fx.consume().await;
    let relaunched = matches!(&reset, Some(ManagedReset::Applied(Some(name))) if *name == fx.name);
    assert!(relaunched, "{reset:?}");
    let calls = fx.tmux.take_calls();
    assert!(called(&calls, "kill-session"), "the pane ends: {calls:?}");
    assert_eq!(
        fx.session_id().await.as_deref(),
        Some(SESSION),
        "session kept"
    );
    let api = fx.api.take();
    let cleared = api.iter().any(|call| call.contains("clear-session-id"));
    assert!(!cleared, "the DB selector is kept: {api:?}");
    assert_eq!(fx.markers(), kept, "account markers kept");
    assert!(!fx.pending(), "the change is consumed");
    let said = fx.said();
    let announced = said
        .iter()
        .any(|call| call.contains("같은 세션 `64860000`으로 TUI를 재기동"));
    assert!(announced, "{said:?}");
    super::relaunch::forget(fx.channel);

    let failed = fx.turn(None);
    assert!(failed.is_err(), "the first launch has no CLI: {failed:?}");
    assert_eq!(
        fx.markers(),
        kept,
        "a failed launch keeps the account markers"
    );
    assert_eq!(fx.session_id().await.as_deref(), Some(SESSION));

    let (args, settings) = fx.launch_fast(fx.channel, &fx.name, SESSION);
    let resumed = args.windows(2).any(|pair| pair == ["--resume", SESSION]);
    assert!(resumed, "{args:?}");
    assert_eq!(
        settings["fastMode"], true,
        "the saved fast setting reaches the CLI"
    );
    assert_eq!(fx.session_id().await.as_deref(), Some(SESSION));
    tc::cleanup_session_temp_files(&fx.name);
}

// A channel bound to its own account, not the provider primary, relaunches on that account: the
// session, its selector and markers stay, and the launch resumes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fast_relaunch_uses_channel_auth_profile_not_provider_primary_pg() {
    let test = "services::discord::commands::control::relaunch_tests::fast_relaunch_uses_channel_auth_profile_not_provider_primary_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut fx = Fixture::new(4, RuntimeHandoffKind::ClaudeTui).await;
    fx.bind_channel_account();
    let _idle = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);

    let reset = fx.consume().await;
    let relaunched = matches!(&reset, Some(ManagedReset::Applied(Some(name))) if *name == fx.name);
    assert!(relaunched, "{reset:?}");
    assert_eq!(
        fx.session_id().await.as_deref(),
        Some(SESSION),
        "session kept"
    );
    let api = fx.api.take();
    assert!(
        !api.iter().any(|call| call.contains("clear-session-id")),
        "{api:?}"
    );
    let kept = (Some("work".to_string()), Some(format!("{SESSION}\nwork")));
    assert_eq!(fx.markers(), kept, "account markers kept");
    super::relaunch::forget(fx.channel);

    let (args, _) = fx.launch_fast(fx.channel, &fx.name, SESSION);
    assert!(
        args.windows(2).any(|pair| pair == ["--resume", SESSION]),
        "{args:?}"
    );
    tc::cleanup_session_temp_files(&fx.name);
}

/// A second live Claude TUI on `n`, a thread sharing the fixture channel's fast setting, on its own
/// session with the given launch fast setting.
async fn sharing_thread(
    fx: &Fixture,
    n: u64,
    session: &str,
    fast: Option<bool>,
) -> (ChannelId, String) {
    let channel = ChannelId::new(1_486_486_000_000_000_000 + n);
    let channel_name = format!("i6486-relaunch-{n}");
    let name = ProviderKind::Claude.build_tmux_session_name(&channel_name);
    map_channel(&fx.shared, channel, &channel_name).await;
    let mut core = fx.shared.core.lock().await;
    let entry = core.sessions.get_mut(&channel).unwrap();
    entry.session_id = Some(session.to_string());
    entry.current_path = Some(fx.cwd.path().to_str().unwrap().to_string());
    drop(core);
    let path = crate::services::claude_tui::transcript_tail::claude_transcript_path;
    let transcript = path(fx.cwd.path(), session, Some(&fx._account.2)).unwrap();
    std::fs::write(&transcript, "{}\n").unwrap();
    tc::write_tmux_runtime_kind_marker(&name, RuntimeHandoffKind::ClaudeTui).unwrap();
    tc::write_tmux_session_auth_profile(&name, "work").unwrap();
    tc::write_claude_session_auth(&name, session, "work").unwrap();
    let pool = fx.shared.pg_pool.as_ref().unwrap();
    Case::Stored(Stored::Legacy)
        .seed(pool, &channel_key(&fx.shared, &name), &name, channel.get())
        .await;
    launched_with(&name, session, fast);
    (channel, name)
}

/// The settings file a launch of `name` on `session` writes, through the launch's own writer.
fn launched_with(name: &str, session: &str, fast: Option<bool>) {
    let config = crate::services::claude_tui::session::ClaudeTuiLaunchConfig {
        tmux_session_name: name.to_string(),
        working_dir: std::env::temp_dir(),
        claude_bin: crate::services::claude_command::ClaudeBinary::from_tmux_wrapper_argv("claude"),
        agentdesk_exe: PathBuf::from("/fixture/agentdesk"),
        hook_endpoint: "http://127.0.0.1:9".to_string(),
        session_id: session.to_string(),
        system_prompt: None,
        model: None,
        resume: true,
    };
    crate::services::claude_tui::session::prepare_claude_tui_launch_with_fast(&config, fast)
        .unwrap();
}

// A thread sharing its parent's fast setting relaunches after the parent consumed the change; a
// settings file naming another session is not trusted, and an applied setting relaunches nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_fast_pending_applies_to_each_live_tui_pg() {
    let test = "services::discord::commands::control::relaunch_tests::shared_fast_pending_applies_to_each_live_tui_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(5, RuntimeHandoffKind::ClaudeTui).await;
    let _idle = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);
    const THREAD: &str = "64860000-0000-4000-8000-000000000005";
    let (thread, name) = sharing_thread(&fx, 6, THREAD, Some(false)).await;

    let parent = fx.consume().await;
    assert!(
        matches!(parent, Some(ManagedReset::Applied(_))),
        "{parent:?}"
    );
    assert!(!fx.pending(), "the parent consumed the shared change");
    fx.tmux.mark_present();
    launched_with(&name, "77770000-0000-4000-8000-000000000009", Some(false));
    fx.tmux.take_calls();
    assert!(
        fx.consume_on(thread).await.is_none(),
        "another session's settings are not trusted"
    );
    assert!(!called(&fx.tmux.take_calls(), "kill-session"));

    launched_with(&name, THREAD, Some(false));
    let reset = fx.consume_on(thread).await;
    let relaunched = matches!(&reset, Some(ManagedReset::Applied(Some(n))) if *n == name);
    assert!(relaunched, "the thread relaunches too: {reset:?}");
    assert!(called(&fx.tmux.take_calls(), "kill-session"));
    let core = fx.shared.core.lock().await;
    assert_eq!(core.sessions[&thread].session_id.as_deref(), Some(THREAD));
    drop(core);
    super::relaunch::forget(thread);

    let (args, settings) = fx.launch_fast(thread, &name, THREAD);
    assert!(
        args.windows(2).any(|pair| pair == ["--resume", THREAD]),
        "{args:?}"
    );
    assert_eq!(settings["fastMode"], true);
    launched_with(&name, THREAD, Some(true));
    fx.tmux.mark_present();
    fx.tmux.take_calls();
    assert!(
        fx.consume_on(thread).await.is_none(),
        "an applied setting relaunches nothing"
    );
    assert!(!called(&fx.tmux.take_calls(), "kill-session"));
    tc::cleanup_session_temp_files(&fx.name);
    tc::cleanup_session_temp_files(&name);
}

// Kill failures count per fast value: a failure for the old value does not use up the retry a newly
// saved value gets.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_fast_value_restarts_the_kill_failure_count_pg() {
    let test = "services::discord::commands::control::relaunch_tests::a_new_fast_value_restarts_the_kill_failure_count_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(7, RuntimeHandoffKind::ClaudeTui).await;
    let _idle = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);
    fx.tmux.fail_kills();

    assert!(fx.consume().await.is_none());
    let key = fx.channel.get().to_string();
    fx.shared
        .settings
        .write()
        .await
        .channel_fast_modes
        .insert(key, false);
    assert!(fx.consume().await.is_none());
    assert!(fx.pending(), "the new value keeps its retry");
    let said = fx.said();
    let retries = said
        .iter()
        .filter(|call| call.contains("한 번 더 시도"))
        .count();
    assert_eq!(retries, 2, "{said:?}");
    assert!(
        !said.iter().any(|call| call.contains("2회 실패")),
        "{said:?}"
    );
    super::relaunch::forget(fx.channel);
    tc::cleanup_session_temp_files(&fx.name);
}

// A TUI busy with a turn of its own is left running: the change waits, told once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_tui_defers_the_relaunch_and_keeps_pending_pg() {
    let test = "services::discord::commands::control::relaunch_tests::busy_tui_defers_the_relaunch_and_keeps_pending_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(2, RuntimeHandoffKind::ClaudeTui).await;
    let _busy = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(true);

    for _ in 0..2 {
        assert!(fx.consume().await.is_none());
    }
    let calls = fx.tmux.take_calls();
    assert!(
        !called(&calls, "kill-session"),
        "the pane runs on: {calls:?}"
    );
    assert!(fx.pending(), "the change waits");
    assert_eq!(fx.session_id().await.as_deref(), Some(SESSION));
    let said = fx.said();
    let deferred = said
        .iter()
        .filter(|call| call.contains("다음 턴으로 미룹니다"))
        .count();
    assert_eq!(deferred, 1, "{said:?}");
    super::relaunch::forget(fx.channel);
    tc::cleanup_session_temp_files(&fx.name);
}

// A live pane running something other than the Claude TUI cannot resume in place: it is reset as
// before, and the channel is told its context is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn other_live_runtime_resets_with_context_loss_pg() {
    let test = "services::discord::commands::control::relaunch_tests::other_live_runtime_resets_with_context_loss_pg";
    if !api_child(test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::new(3, RuntimeHandoffKind::LegacyTmuxWrapper).await;
    let _idle = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);

    let reset = fx.consume().await;
    assert!(matches!(reset, Some(ManagedReset::Applied(_))), "{reset:?}");
    assert_eq!(fx.session_id().await, None, "the session is reset");
    assert!(!fx.pending());
    let said = fx.said();
    let lost = said
        .iter()
        .any(|call| call.contains("세션을 리셋했습니다(맥락 소실)"));
    assert!(lost, "{said:?}");
    let relaunched = said.iter().any(|call| call.contains("TUI를 재기동합니다"));
    assert!(!relaunched, "{said:?}");
    tc::cleanup_session_temp_files(&fx.name);
}

const WAITS: Waits = Waits {
    record: Duration::from_secs(5),
    settle: Duration::from_millis(50),
    poll: Duration::from_millis(10),
};

fn confirm(at: f64) -> Confirm {
    Confirm {
        session: SESSION.to_string(),
        requested_fast: Some(true),
        at,
    }
}

fn write_record(root: &Path, session: &str, written_at: f64) {
    let path = crate::services::claude_tui::hook_bundle::statusline::record_path(root, session);
    let record = serde_json::json!({"session_id": session, "rate_limits": {},
        "model": {"id": "claude-opus-4-7"}, "effort": {"level": "high"}, "fast_mode": false,
        "written_at": written_at});
    std::fs::write(path, record.to_string()).unwrap();
}

// The restarted process's own record is reported as observed beside what was asked; fast mode is
// delivered, never claimed on, and an older record is not taken for the new process's.
#[tokio::test]
async fn the_confirmation_reports_the_restarted_record_without_claiming_fast() {
    let records = tempfile::tempdir().unwrap();
    let same = || async { Some(SESSION.to_string()) };
    write_record(records.path(), SESSION, 1_000.0);

    let stale = confirm_notice(&confirm(1_000.0), records.path(), same, &WAITS).await;
    assert_eq!(
        stale,
        "재기동 뒤 세션 `64860000`의 상태 기록을 확인하지 못했어요. 설정 적용 여부는 아직 확인되지 않았습니다."
    );

    let seen = confirm_notice(&confirm(999.0), records.path(), same, &WAITS).await;
    assert_eq!(
        seen,
        "재기동 확인 — 같은 세션 `64860000` · 표시 모델 `claude-opus-4-7` · effort `high` · fast 설정 전달됨(켬 요청, 표시 꺼짐). 실제 활성은 계정·모델 조건에 따릅니다"
    );
}

// A channel that moved to another session ends the wait at once with the context-loss notice, even
// with the old session's new record in place.
#[tokio::test]
async fn a_switched_session_ends_the_confirmation_with_its_notice() {
    let records = tempfile::tempdir().unwrap();
    write_record(records.path(), SESSION, 1_000.0);
    let other = || async { Some("77770000-0000-4000-8000-000000000002".to_string()) };

    let notice = confirm_notice(&confirm(999.0), records.path(), other, &WAITS).await;
    assert_eq!(
        notice,
        "재기동 뒤 다른 세션 `77770000`으로 시작됐습니다(이전 세션 `64860000`의 맥락 소실)."
    );
}

// `/clear` drops the pending confirmation: no report follows a session it ended.
#[tokio::test]
async fn clear_aborts_the_pending_confirmation() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = ChannelId::new(1_486_486_000_000_000_009);
    let http = Arc::new(poise::serenity_prelude::Http::new(""));
    let long = Waits {
        record: Duration::from_secs(600),
        ..WAITS
    };
    let spawn = super::relaunch_confirm::spawn_with;
    spawn(
        http,
        shared.clone(),
        channel,
        confirm(super::relaunch_confirm::now()),
        long,
    );
    assert!(super::relaunch_confirm::running(channel));

    super::native::clear_process_reset_pending(&shared, channel).await;
    assert!(
        !super::relaunch_confirm::running(channel),
        "the confirmation is aborted"
    );
}
