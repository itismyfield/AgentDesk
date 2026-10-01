//! A Claude TUI pane bound to a headless SDK transcript in its project dir returns to its TUI one.
use super::*;

/// A live pane whose TUI transcript went quiet before a headless `claude -p` session in the same
/// cwd started, and whose launch script and hook settings were cut over to that headless session.
struct HeadlessPane {
    tmux: String,
    channel: u64,
    tui: String,
    headless: String,
    dir: PathBuf,
    shared: Arc<SharedData>,
    _claude_home: crate::config::TestEnvVarGuard,
}

impl HeadlessPane {
    fn new(root: &Path, channel: u64) -> Self {
        use crate::services::tmux_common as tc;
        let (tmux, tui, headless) = (format!("headless-{}", uuid()), uuid(), uuid());
        let home = root.join("claude-home");
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let claude_home = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "CLAUDE_CONFIG_DIR",
            &home,
        );
        let tui_path =
            crate::services::claude_tui::transcript_tail::claude_transcript_path(&cwd, &tui, None)
                .unwrap();
        let dir = tui_path.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let context = BindingContext {
            schema: 1,
            provider: "claude".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: tmux.clone(),
            channel_id: Some(channel),
            owner_runtime_root: root.display().to_string(),
            host: None,
            expected_native_session_id: Some(headless.clone()),
            launch_mode: "fresh".into(),
            provider_root: Some(home.clone()),
        };
        let prepared = PreparedIncarnation::create(context).unwrap();
        let script = tc::session_temp_path(&tmux, tc::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT);
        std::fs::create_dir_all(Path::new(&script).parent().unwrap()).unwrap();
        let exec = format!(
            "cd '{}'\nexec 'claude' '--session-id' '{headless}'\n",
            cwd.display()
        );
        std::fs::write(&script, format!("{}{exec}", prepared.env_lines())).unwrap();
        let hook = format!("adk hook --session-id {headless}");
        let settings = serde_json::json!({"hooks":{"SessionStart":[{"hooks":[{"command":hook}]}]}});
        std::fs::write(
            tc::session_temp_path(&tmux, tc::CLAUDE_TUI_HOOK_SETTINGS_TEMP_EXT),
            settings.to_string(),
        )
        .unwrap();
        let nonce = &prepared.context.execution_nonce;
        std::fs::write(tc::session_temp_path(&tmux, "spawn_nonce"), nonce).unwrap();
        VIEW.with_borrow_mut(|v| {
            *v = Some(View {
                tmux: tmux.clone(),
                channel,
                home,
                peers: Vec::new(),
            })
        });
        let pane = Self {
            tmux,
            channel,
            tui,
            headless,
            dir,
            shared: crate::services::discord::make_shared_data_for_tests(),
            _claude_home: claude_home,
        };
        let now = std::time::SystemTime::now();
        let ago = |secs| now - std::time::Duration::from_secs(secs);
        let record = |session: &str, entrypoint: &str, at: &str| {
            let record = serde_json::json!({
                "type": "user", "sessionId": session, "entrypoint": entrypoint, "timestamp": at,
            });
            record.to_string()
        };
        let tui_record = record(&pane.tui, "cli", "2026-10-01T06:00:00Z");
        let queued = serde_json::json!({"type": "queue-operation", "sessionId": pane.headless});
        let sdk_record = record(&pane.headless, "sdk-cli", "2026-10-01T06:20:00Z");
        let headless_records = format!("{queued}\n{sdk_record}");
        set_mtime(&script, ago(600));
        set_mtime(&pane.write(&pane.tui, &tui_record), ago(120));
        set_mtime(&pane.write(&pane.headless, &headless_records), ago(60));
        pane
    }

    fn path(&self, session: &str) -> PathBuf {
        self.dir.join(format!("{session}.jsonl"))
    }

    fn write(&self, session: &str, records: &str) -> String {
        let path = self.path(session);
        std::fs::write(&path, format!("{records}\n")).unwrap();
        path.display().to_string()
    }

    fn bound(&self) -> Option<(String, Option<String>)> {
        dedupe::runtime_binding_for_tmux_session(&self.tmux).map(|b| (b.output_path, b.session_id))
    }

    fn expect_bound_to_tui(&self) {
        let tui = (
            self.path(&self.tui).display().to_string(),
            Some(self.tui.clone()),
        );
        assert_eq!(self.bound(), Some(tui), "bound to the TUI transcript");
        let script = crate::services::tmux_common::session_temp_path(
            &self.tmux,
            crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
        );
        let script = std::fs::read_to_string(script).unwrap();
        assert!(
            script.contains(&self.tui) && !script.contains(&self.headless),
            "the launch script names the TUI session again: {script}"
        );
    }

    fn bind_headless(&self) -> crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        let binding = claude(&self.path(&self.headless), &self.headless);
        dedupe::register_tmux_channel(&self.tmux, self.channel);
        assert!(dedupe::register_rehydrated_tmux_runtime_binding(
            "claude",
            &self.tmux,
            self.channel,
            binding.clone(),
        ));
        binding
    }
}

fn set_mtime(path: &str, at: std::time::SystemTime) {
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(at).unwrap();
}

#[test]
fn a_restart_binds_the_tui_transcript_when_the_launch_script_names_a_headless_one() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _reset = Reset;
    let pane = HeadlessPane::new(root.path(), 7_560);

    super::super::rehydrate_claude_tui_pane(&pane.shared, &pane.tmux);

    pane.expect_bound_to_tui();
}

#[test]
fn the_rehydrate_pass_moves_a_live_headless_binding_back_to_the_tui_transcript() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _reset = Reset;
    let pane = HeadlessPane::new(root.path(), 7_561);
    pane.bind_headless();

    super::super::rehydrate_claude_tui_pane(&pane.shared, &pane.tmux);

    pane.expect_bound_to_tui();
}

#[test]
fn the_idle_relay_tails_the_tui_transcript_instead_of_a_bound_headless_one() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _reset = Reset;
    let pane = HeadlessPane::new(root.path(), 7_562);
    let binding = pane.bind_headless();

    let tailed = resolved_claude_idle_relay_transcript_path(
        &pane.shared,
        &pane.tmux,
        ChannelId::new(pane.channel),
        &binding,
    );

    assert_eq!(tailed, Some(pane.path(&pane.tui)));
    pane.expect_bound_to_tui();
}

#[test]
fn a_restart_leaves_a_log_verified_headless_source_for_the_tui_transcript() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _ingress = Ingress::new();
    let _reset = Reset;
    let pane = HeadlessPane::new(root.path(), 7_563);
    let headless = pane.path(&pane.headless);
    let headless_path = headless.display().to_string();
    dedupe::register_tmux_channel(&pane.tmux, pane.channel);
    dedupe::register_provider_session("claude", &pane.headless, &pane.tmux);
    dedupe::register_launched_tmux_runtime_binding(&pane.tmux, claude(&headless, &pane.headless));
    // The hook check verified the headless session as this pane's source under its spawn nonce.
    let (dev, ino) = crate::services::tui_o::shadow::capture::file_identity(
        &std::fs::metadata(&headless).unwrap(),
    );
    let source = SourceId {
        session_id: pane.headless.clone(),
        path: headless.clone(),
        dev,
        ino,
    };
    let proposal = Proposal {
        channel_id: pane.channel,
        provider: "claude",
        tmux_session: &pane.tmux,
        session_id: Some(&pane.headless),
        path: &headless_path,
        replaced: None,
        cause: CauseSource::Hook(BindingCause::Unknown),
        hook: None,
    };
    assert_eq!(
        record_verified(&proposal, &source).unwrap(),
        Committed::Appended
    );
    let mut row = crate::services::discord::inflight::InflightTurnState::new(
        crate::services::provider::ProviderKind::Claude,
        pane.channel,
        None,
        1,
        7_563_001,
        7_563_002,
        "headless lane brief".to_string(),
        None,
        Some(pane.tmux.clone()),
        Some(headless_path.clone()),
        None,
        0,
    );
    row.full_response = "headless lane output".to_string();
    crate::services::discord::inflight::save_inflight_state(&row).unwrap();
    // A restart forgets every in-memory binding; only the log and the launch artifacts remain.
    forget_channel_for_tests(pane.channel);
    dedupe::reset_state_for_tests();
    crate::services::tui_prompt_dedupe::pending::reset_restore_outcomes_for_tests();

    super::super::rehydrate_claude_tui_pane(&pane.shared, &pane.tmux);
    pane.expect_bound_to_tui();
    // The next pass reads the log the replacement wrote and keeps the TUI binding.
    super::super::rehydrate_claude_tui_pane(&pane.shared, &pane.tmux);
    pane.expect_bound_to_tui();
}
