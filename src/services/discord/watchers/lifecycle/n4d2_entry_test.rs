//! Isolated production entry fixtures; every tmux command goes to the child executable.
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::health::legacy_supervision::test_support::MockDiscord;
use crate::services::discord::inflight::o_seed_observation::{Guard, Snapshot};
use crate::services::discord::task_supervisor::watcher_completion::{self, Outcome};
use crate::services::tmux_common as tc;
use crate::services::tui_prompt_dedupe::{self as binding, TuiRuntimeBinding};
use std::io::Write;
use std::path::{Path, PathBuf};

pub(in crate::services::discord) struct InstallFixture {
    pub shared: Arc<SharedData>,
    pub registry: Arc<HealthRegistry>,
    pub http: Arc<serenity::Http>,
    pub provider: ProviderKind,
    pub channel: ChannelId,
    pub session: String,
    pub output: String,
}

impl InstallFixture {
    pub async fn new(provider: ProviderKind, channel: u64) -> Self {
        Self::new_with_pool(provider, channel, None).await
    }
    pub(in crate::services::discord) async fn new_with_pool(
        provider: ProviderKind,
        channel: u64,
        pool: Option<sqlx::PgPool>,
    ) -> Self {
        let channel = ChannelId::new(channel);
        let registry = Arc::new(HealthRegistry::new());
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared).unwrap().health_registry = Arc::downgrade(&registry);
        Arc::get_mut(&mut shared).unwrap().provider = provider.clone();
        Arc::get_mut(&mut shared).unwrap().pg_pool = pool;
        {
            let mut settings = shared.settings.write().await;
            settings.provider = provider.clone();
            settings.allowed_channel_ids = vec![channel.get()];
        }
        registry
            .register(provider.as_str().to_owned(), shared.clone())
            .await;
        let session = provider.build_tmux_session_name(&format!(
            "n4d-{}-{}",
            channel.get(),
            if provider == ProviderKind::Codex {
                "cdx"
            } else {
                "cc"
            }
        ));
        let output = tc::session_temp_path(&session, "jsonl");
        std::fs::create_dir_all(Path::new(&output).parent().unwrap()).unwrap();
        std::fs::write(&output, []).unwrap();
        std::fs::write(tc::session_temp_path(&session, "generation"), "n4d-source").unwrap();
        std::fs::write(
            tc::tmux_owner_path(&session),
            tc::current_tmux_owner_marker(),
        )
        .unwrap();
        tc::write_tmux_channel_binding(&session, channel.get()).unwrap();
        tc::write_tmux_runtime_kind_marker(
            &session,
            match provider {
                ProviderKind::Codex => RuntimeHandoffKind::CodexTui,
                ProviderKind::Claude => RuntimeHandoffKind::ClaudeTui,
                _ => panic!("fixture requires TUI provider"),
            },
        )
        .unwrap();
        Self {
            shared,
            registry,
            http: Arc::new(serenity::Http::new("n4d-fixture")),
            provider,
            channel,
            session,
            output,
        }
    }

    /// Start the actual watcher through the general claim and task supervision paths.
    pub async fn spawn_watcher(&self) {
        let host = watch_host_of(
            &self.shared,
            &self.provider,
            self.channel.get(),
            &self.session,
        )
        .await;
        let offset = std::fs::metadata(&self.output).unwrap().len();
        let handle = TmuxWatcherHandle {
            tmux_session_name: self.session.clone(),
            output_path: self.output.clone(),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume_offset: Arc::new(Mutex::new(None)),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                crate::services::discord::tmux_watcher_now_ms(),
            )),
        };
        let claim = claim_or_reuse_watcher_for_host(
            &self.shared.tmux_watchers,
            self.channel,
            handle,
            &self.provider,
            "watcher_test_fixture",
            None,
            host,
        )
        .unwrap();
        assert!(claim.should_spawn(), "fixture must submit a fresh watcher");
        let handle = self.handle();
        self.shared.record_tmux_watcher_reconnect(self.channel);
        crate::services::discord::task_supervisor::spawn_observed_tmux_watcher(
            "watcher_test_fixture",
            self.shared.clone(),
            self.session.clone(),
            handle.cancel.clone(),
            tmux_output_watcher_with_restore(
                self.channel,
                self.http.clone(),
                self.shared.clone(),
                self.output.clone(),
                self.session.clone(),
                offset,
                handle.cancel.clone(),
                handle.paused.clone(),
                handle.resume_offset.clone(),
                handle.pause_epoch.clone(),
                handle.turn_delivered.clone(),
                handle.last_heartbeat_ts_ms.clone(),
                None,
            ),
        );
    }
    pub fn handle(&self) -> TmuxWatcherHandle {
        let handle = self.shared.tmux_watchers.get(&self.channel).unwrap();
        TmuxWatcherHandle {
            tmux_session_name: handle.tmux_session_name.clone(),
            output_path: handle.output_path.clone(),
            paused: handle.paused.clone(),
            resume_offset: handle.resume_offset.clone(),
            cancel: handle.cancel.clone(),
            pause_epoch: handle.pause_epoch.clone(),
            turn_delivered: handle.turn_delivered.clone(),
            last_heartbeat_ts_ms: handle.last_heartbeat_ts_ms.clone(),
        }
    }
    pub async fn cancel_and_join(&self) {
        let handle = self.handle();
        let ticket = watcher_completion::observe(&handle.cancel).expect("observed task registered");
        crate::services::discord::tmux::tmux_watcher::cancel_for_test(
            &handle.cancel,
            Ordering::SeqCst,
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(8), ticket.wait())
                .await
                .unwrap(),
            Outcome::Returned
        );
    }
}

#[path = "n4d2_o_writer_test.rs"]
mod o_writer;

const CHILD: &str = "ADK_N4D2_ENTRY_CHILD";
const FAKE_TMUX: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "ROOT/tmux-calls"
while [ "${1#-}" != "$1" ]; do
  case "$1" in -V) echo 'tmux fixture'; exit 0 ;; -L|-S) shift ;; esac
  shift
done
read -r state < "ROOT/pane" || state="unreadable"
case "$state" in busy|idle|dead|deadpane|unanswered|listfail) ;; *) printf '%s\n' "$* on pane '$state'" >> "ROOT/tmux-errors"; exit 97 ;; esac
case "$1" in
  list-sessions) /bin/cat "ROOT/tmux-sessions"; exit $? ;;
  has-session) [ "$state" = dead ] && { echo "can't find session" >&2; exit 1; }
    [ "$state" = unanswered ] && { echo 'error connecting to fixture socket' >&2; exit 1; }; exit 0 ;;
  list-panes) [ "$state" = listfail ] && exit 1
    case "$state" in dead|deadpane) echo 1 ;; *) echo 0 ;; esac; exit 0 ;;
  capture-pane) [ "$state" = busy ] && printf '%s\n' '⏺ Running a fixture command'; exit 0 ;;
  show-options|show-option) exit 0 ;;
  display-message) [ -f "ROOT/pane.pid" ] && /bin/cat "ROOT/pane.pid"; exit 0 ;;
  kill-session) printf '%s\n' "$*" >> "ROOT/tmux-kills"; exit 0 ;;
  *) printf '%s\n' "$*" >> "ROOT/tmux-errors"; exit 97 ;;
esac
"#;

pub(in crate::services::discord) fn root() -> PathBuf {
    PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap())
}

pub(in crate::services::discord) fn isolated(module: &str, name: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    use std::os::unix::fs::PermissionsExt;
    let scratch = tempfile::tempdir().unwrap();
    let path = scratch.path();
    for dir in ["codex/sessions", "claude/projects"] {
        std::fs::create_dir_all(path.join(dir)).unwrap();
    }
    std::fs::write(path.join("pane"), "busy\n").unwrap();
    std::fs::write(path.join("tmux-sessions"), []).unwrap();
    let tmux = path.join("tmux");
    std::fs::write(
        &tmux,
        FAKE_TMUX.replace("ROOT", &path.display().to_string()),
    )
    .unwrap();
    std::fs::set_permissions(tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
    let module = module.split_once("::").unwrap().1;
    let exact = format!("{module}::{name}");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &exact, "--nocapture"])
        .env(CHILD, "1")
        .env("AGENTDESK_ROOT_DIR", path)
        .env("TMPDIR", path)
        .env("CODEX_HOME", path.join("codex"))
        .env("CLAUDE_CONFIG_DIR", path.join("claude"))
        .env("PATH", format!("{}:/usr/bin:/bin", path.display()))
        .env("AGENTDESK_STATUS_INTERVAL_SECS", "0")
        .env(
            crate::services::tui_o::cutover::test_override::CHILD_ENV,
            "1",
        )
        .env(
            crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
            "[]",
        )
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env_remove("DATABASE_URL")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{exact}:\n{stdout}\n{stderr}");
    assert!(stdout.contains("running 1 test"), "{stdout}");
    assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
    let errors = std::fs::read_to_string(path.join("tmux-errors")).unwrap_or_default();
    assert!(errors.is_empty(), "unexpected fake tmux command: {errors}");
    false
}

pub(in crate::services::discord) fn list_sessions(sessions: &[&str]) {
    std::fs::write(root().join("tmux-sessions"), sessions.join("\n")).unwrap();
}

pub(in crate::services::discord) fn pane(state: &str) {
    std::fs::write(root().join("pane.next"), format!("{state}\n")).unwrap();
    std::fs::rename(root().join("pane.next"), root().join("pane")).unwrap();
}

pub(in crate::services::discord) async fn fixture(
    provider: ProviderKind,
    channel: u64,
) -> (InstallFixture, MockDiscord) {
    fixture_with_pool(provider, channel, None).await
}

pub(in crate::services::discord) async fn fixture_with_pool(
    provider: ProviderKind,
    channel: u64,
    pool: Option<sqlx::PgPool>,
) -> (InstallFixture, MockDiscord) {
    pane("busy");
    let mut fixture = InstallFixture::new_with_pool(provider.clone(), channel, pool).await;
    let discord = MockDiscord::start().await;
    fixture.http = discord.http.clone();
    fixture
        .registry
        .register_http(provider.as_str().to_owned(), fixture.http.clone())
        .await;
    binding::register_tmux_runtime_binding(
        &fixture.session,
        TuiRuntimeBinding {
            runtime_kind: if provider == ProviderKind::Codex {
                RuntimeHandoffKind::CodexTui
            } else {
                RuntimeHandoffKind::ClaudeTui
            },
            output_path: fixture.output.clone(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("e2e".into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    std::fs::write(
        crate::services::tmux_common::session_temp_path(&fixture.session, "host_kind"),
        "tmux",
    )
    .unwrap();
    list_sessions(&[&fixture.session]);
    (fixture, discord)
}

pub(in crate::services::discord) fn append(fixture: &InstallFixture, bytes: &[u8]) {
    std::fs::OpenOptions::new()
        .append(true)
        .open(&fixture.output)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

pub(in crate::services::discord) fn observe(fixture: &InstallFixture) -> Guard {
    let observed = Guard::new(&fixture.provider, fixture.channel.get());
    observed.register_source(Path::new(&fixture.output));
    observed
}

pub(in crate::services::discord) async fn until(
    observed: &Guard,
    what: &str,
    done: impl Fn(&Snapshot) -> bool,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    loop {
        let snapshot = observed.snapshot();
        if done(&snapshot) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: key={snapshot:?} raw={:?}",
            observed.raw_process_snapshot()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub(in crate::services::discord) fn assert_zero_loaders(observed: &Guard) {
    for s in [observed.snapshot(), observed.raw_process_snapshot()] {
        assert_eq!(s.writable_inflight_load_calls, 0, "{s:?}");
        assert_eq!(s.readonly_inflight_load_calls, 0, "{s:?}");
        assert_eq!(s.compatibility_backfill_attempts, 0, "{s:?}");
        assert_eq!(s.legacy_prefix_restore_calls, 0, "{s:?}");
        assert_eq!(s.event_count("legacy_row_seed_adopted"), 0, "{s:?}");
        assert_eq!(s.event_count("capsule_legacy_adopted"), 0, "{s:?}");
        assert_eq!(s.event_count("terminal_preflight_entries"), 0, "{s:?}");
    }
}

pub(in crate::services::discord) struct OWriter(o_writer::WriterFixture);
pub(in crate::services::discord) struct OReceiptPause(Arc<o_writer::PendingPost>);
impl OReceiptPause {
    pub(in crate::services::discord) async fn entered(&self) {
        self.0.entered().await;
    }
    pub(in crate::services::discord) fn release(&self) {
        self.0.release();
    }
}
impl OWriter {
    pub(in crate::services::discord) fn new(fixture: &InstallFixture) -> Self {
        use crate::services::tui_o::shadow::ShadowProvider;
        let provider = if fixture.provider == ProviderKind::Codex {
            ShadowProvider::Codex
        } else {
            ShadowProvider::Claude
        };
        let inner = o_writer::WriterFixture::new(
            &root(),
            Path::new(&fixture.output),
            provider,
            fixture.channel.get(),
            &fixture.session,
        );
        inner.acquired();
        Self(inner)
    }
    pub(in crate::services::discord) fn start(
        &self,
    ) -> (
        tokio::sync::watch::Sender<bool>,
        tokio::task::JoinHandle<Option<crate::services::tui_o::writer::deliver::StopCause>>,
    ) {
        self.0.start()
    }
    pub(in crate::services::discord) fn posts(&self) -> Vec<String> {
        self.0.posts()
    }
    pub(in crate::services::discord) fn hold_next_post(&self) -> OReceiptPause {
        OReceiptPause(self.0.hold_next_post())
    }
    pub(in crate::services::discord) fn receipts(&self) -> Vec<String> {
        use crate::services::tui_o::store::ledger::PieceOutcome;
        let channel = self.0.channel();
        let ledger = channel.ledger();
        (0..ledger.next_serial())
            .filter_map(|serial| ledger.piece(serial))
            .filter(|piece| matches!(piece.outcome, Some(PieceOutcome::Posted(_))))
            .map(|piece| piece.payload.clone())
            .collect()
    }
}
