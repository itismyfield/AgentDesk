//! Isolated production entry fixtures; every tmux command goes to the child executable.
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::health::test_support::MockDiscord;
use crate::services::discord::inflight::o_seed_observation::{Guard, Snapshot};
use crate::services::tmux_common as tc;
use crate::services::tui_prompt_dedupe::{self as binding, TuiRuntimeBinding};
use std::path::{Path, PathBuf};

static READER_STARTS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<u64, u64>>> =
    std::sync::LazyLock::new(std::sync::Mutex::default);

pub(in crate::services::discord) fn record_reader_start(channel: u64, offset: u64) {
    READER_STARTS.lock().unwrap().insert(channel, offset);
}

pub(in crate::services::discord) fn reader_start(channel: u64) -> Option<u64> {
    READER_STARTS.lock().unwrap().get(&channel).copied()
}

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
            "wfx-{}-{}",
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
        std::fs::write(tc::session_temp_path(&session, "generation"), "wfx-source").unwrap();
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
            http: Arc::new(serenity::Http::new("wfx-fixture")),
            provider,
            channel,
            session,
            output,
        }
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
}

const CHILD: &str = "ADK_WATCHER_FIXTURE_CHILD";
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
  capture-pane) [ -s "ROOT/draft" ] && { /usr/bin/awk -v draft="ROOT/draft" 'index($0, "Ask Codex") {
        lead = "\033[1m›\033[0m "; while ((getline row < draft) > 0) { print lead row; lead = "  " }; next } { print }' "ROOT/composer"; exit 0; }
    [ -f "ROOT/composer" ] && { /bin/cat "ROOT/composer"; exit 0; }
    [ "$state" = busy ] && printf '%s\n' '⏺ Running a fixture command'; exit 0 ;;
  send-keys|paste-buffer|delete-buffer|load-buffer)
    [ -f "ROOT/composer" ] || { printf '%s\n' "$*" >> "ROOT/tmux-errors"; exit 97; }
    printf '%s\n' "$*" >> "ROOT/tmux-input"
    buffer=; target=; prev=
    for arg; do case "$prev" in -b) buffer=$arg ;; -t) target=$arg ;; esac; prev=$arg; last=$arg; done
    refuse() { printf '%s\n' "refused $*" >> "ROOT/tmux-errors"; exit 97; }
    [ "$1" = load-buffer ] || [ "$1" = delete-buffer ] || [ "$target" = "$(/bin/cat "ROOT/composer-target")" ] || refuse "$@"
    /bin/mkdir -p "ROOT/buffers"
    case "$1 $*" in
      load-buffer*) [ -n "$buffer" ] || refuse "$@"; /bin/cat "$last" > "ROOT/buffers/$buffer" ;;
      delete-buffer*) /bin/rm -f "ROOT/buffers/$buffer" ;;
      paste-buffer*) [ -f "ROOT/buffers/$buffer" ] || refuse "$@"; /bin/cat "ROOT/buffers/$buffer" >> "ROOT/draft"
        case " $* " in *" -d "*) /bin/rm -f "ROOT/buffers/$buffer" ;; esac ;;
      *" -l "*) printf '%s' "$last" >> "ROOT/draft" ;;
      *) for key; do case "$key" in
           C-u) /bin/rm -f "ROOT/draft" ;;
           Enter|C-m) [ -s "ROOT/draft" ] && { /bin/cat "ROOT/draft"; printf '\n%s\n' "--8<--"; } >> "ROOT/tmux-submits"
             /bin/rm -f "ROOT/draft" ;; esac; done ;;
    esac; exit 0 ;;
  show-options|show-option) exit 0 ;;
  display-message) [ -f "ROOT/composer" ] && case "$*" in *pane_dead*) echo 0; exit 0 ;;
      *pane_width*) echo '120 40'; exit 0 ;; esac
    [ -f "ROOT/pane.pid" ] && /bin/cat "ROOT/pane.pid"; exit 0 ;;
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

/// A ready Codex composer for `session` on `capture-pane`. Writes must name its pane and a loaded
/// buffer; typed or pasted text shows as the draft, and Enter on a non-empty draft submits it.
pub(in crate::services::discord) fn ready_codex_composer(session: &str) {
    std::fs::write(root().join("composer-target"), format!("={session}:")).unwrap();
    std::fs::write(
        root().join("composer"),
        include_str!("../../../../../tests/fixtures/tui_input/codex-status-sol-80-idle.ansi"),
    )
    .unwrap();
}

/// Every pane write the composer received, one command per line.
pub(in crate::services::discord) fn pane_input() -> String {
    std::fs::read_to_string(root().join("tmux-input")).unwrap_or_default()
}

/// Each whole draft Enter submitted to the composer's pane, in order.
pub(in crate::services::discord) fn pane_submits() -> Vec<String> {
    let submits = std::fs::read_to_string(root().join("tmux-submits")).unwrap_or_default();
    submits
        .split_terminator("\n--8<--\n")
        .map(str::to_owned)
        .collect()
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
