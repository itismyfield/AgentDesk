//! A Codex input held while Legacy's takeover turn was unfinished waits in the owner's queue; once
//! that turn is gone with no completion here, one owner drive starts it once and settles its front.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::queue_io::cancel_backstop_test_support as counted;
use crate::services::discord::queue_io::input_owner::drive::wake;
use crate::services::discord::router::message_handler::provider_dispatch as dispatch;
use crate::services::discord::router::{IntakeOrigin, IntakeSubmission};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::tui_o::channel_policy::{Adoption, BootChannels};
use crate::services::tui_o::cutover::{self, test_override};
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{ShadowProvider, binding_reader::source_id_for};
use crate::services::tui_o::turn_mode::{TestConfirmation, transcript_turns};
use crate::services::tui_o::writer::host::{self, HostParts, test_io::TestHost};
use crate::test_support::wait::{WAIT_CAP, await_within};
use poise::serenity_prelude::{self as serenity, ChannelId, MessageId, UserId};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::Ordering;

const CHANNEL: u64 = 7_942_001;
const MESSAGE: u64 = 7_942_001_011;
const TEXT: &str = "the question held behind the takeover turn";

/// The channel O owns once its gateway adopts it, selected for turn mode.
fn selected_for_turn_mode() -> BootChannels {
    let config = serde_json::from_value(json!({
        "server": {},
        "agents": [{"id": "w", "name": "W",
            "channels": {"codex": {"id": CHANNEL.to_string(), "runtime": "tui"}}}],
        "tui_o": {"writer": {"channels": [CHANNEL]}, "turn": {"channels": [CHANNEL]}}
    }))
    .unwrap();
    BootChannels::validate(&config)
        .unwrap()
        .adopted(Adoption::Pending)
}

/// Legacy's unfinished takeover turn, as a restart leaves its row while the pane still runs.
fn legacy_turn_running() {
    use crate::services::discord::inflight::{self, InflightTurnState};
    let row = InflightTurnState::new(
        ProviderKind::Codex,
        CHANNEL,
        None,
        1,
        7,
        0,
        "sleep".into(),
        None,
        Some("codex-takeover-fixture".into()),
        None,
        None,
        0,
    );
    let root = inflight::inflight_runtime_root().unwrap();
    let path = inflight::inflight_state_path(&root, &ProviderKind::Codex, CHANNEL);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let _lock = inflight::lock_inflight_state_path(&path).unwrap();
    fs::write(&path, serde_json::to_vec(&row).unwrap()).unwrap();
}

fn live_input() -> IntakeSubmission {
    let message = MessageId::new(MESSAGE);
    let request = crate::services::discord::IntakeRequest {
        intake_outbox_id: None,
        channel_id: ChannelId::new(CHANNEL),
        user_msg_id: message,
        source_message_ids: vec![message],
        busy_followup_retry_user_msg_id: message,
        request_owner: UserId::new(7),
        request_owner_name: "author".into(),
        user_text: TEXT.into(),
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        merge_consecutive: false,
        reply_context: None,
        has_reply_boundary: true,
        dm_hint: Some(false),
        turn_kind: crate::services::discord::TurnKind::Foreground,
        preserve_on_cancel: false,
    };
    IntakeSubmission::text_for_tests(ProviderKind::Codex, request, IntakeOrigin::LiveMessage)
}

/// Undoes this test's process-wide probes, also when an assertion unwinds.
struct Probes(ChannelId);
impl Drop for Probes {
    fn drop(&mut self) {
        counted::set_owner_wake_for_tests(self.0, None);
        *dispatch::COUNTED_DISPATCHES.lock().unwrap() = None;
        *dispatch::INPUT_EFFECT_OBSERVER.lock().unwrap() = None;
        *dispatch::INPUT_EFFECT_PROBE.lock().unwrap() = None;
    }
}

fn dispatches() -> usize {
    dispatch::COUNTED_DISPATCHES
        .lock()
        .unwrap()
        .map_or(0, |(_, count)| count)
}

#[tokio::test(flavor = "current_thread")]
async fn a_held_codex_input_runs_once_on_one_owner_drive_after_the_unseen_takeover_turn_is_gone_pg()
{
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let root = crate::config::runtime_root().unwrap();
    let config = crate::runtime_layout::config_file_path(&root);
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "server: {}\ndata: {dir: data}\n").unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config,
    );
    let _tmux = crate::services::tui_prompt_dedupe::binding_context::tests::fake_tmux(&root);
    fs::write(
        root.join("tmux"),
        "#!/bin/sh\ncase \"$*\" in *pane_dead*) echo 1;; *has-session*) echo \"can't find session: fixture\" >&2; exit 1;; esac\n",
    )
    .unwrap();
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    fs::create_dir_all(generation.parent().unwrap()).unwrap();
    fs::write(generation, "1").unwrap();
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(
        "http://127.0.0.1:9".into(),
    );
    let api = crate::services::discord::admin_host_guard::tests::Recorder::start().await;
    crate::services::discord::internal_api::init(api.port, None);
    let (proxy, gateway, server) = discord_mock::start(discord_mock::DiscordMockState::new()).await;
    let mut ctx = discord_mock::serenity_context(proxy, gateway).await;
    ctx.http = api.http.clone();
    let mut shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Codex;
    Arc::get_mut(&mut shared).unwrap().api_port = api.port;
    shared.settings.write().await.provider = ProviderKind::Codex;
    shared.restart.reconcile_done.store(true, Ordering::SeqCst);
    shared.http.cached_serenity_ctx.set(ctx).ok().unwrap();
    shared
        .http
        .cached_bot_token
        .set("turn-mode-fixture".into())
        .ok()
        .unwrap();
    let channel = ChannelId::new(CHANNEL);
    crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, "").await;
    let workspace = tempfile::tempdir().unwrap();
    {
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        session.channel_name = Some("turn-mode-redrive".into());
        session.current_path = Some(workspace.path().display().to_string());
    }

    // The gateway hosts the channel's writer and adopts it once Owned; turn mode is not confirmed.
    let _boot = test_override::force_boot(selected_for_turn_mode());
    let rollout = workspace.path().join("rollout-turn-mode.jsonl");
    let header = json!({"type":"session_meta","payload":{"id":"s-turn-mode"}});
    fs::write(&rollout, format!("{header}\n")).unwrap();
    let io = TestHost::new([(CHANNEL, source_id_for("s-turn-mode", &rollout).unwrap())]);
    let gate = Arc::new(OwnershipGate::default());
    let parts = HostParts {
        io: Arc::clone(&io),
        runtime_root: Some(root.clone()),
        gate: Arc::clone(&gate),
        readiness: host::process_readiness(),
    };
    let hosts = host::start_managed(
        ShadowProvider::Codex,
        true,
        cutover::boot_ownership(),
        || parts,
    );
    gate.acquired();
    await_within(
        async {
            while !host::channel_accepts(CHANNEL) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        },
        WAIT_CAP,
    )
    .await;

    let mailbox = shared.mailbox(channel);
    let path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("codex")
        .join(&shared.token_hash)
        .join(format!("{channel}.json"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    mailbox
        .v2_adopt(path, shared.token_hash.clone())
        .await
        .unwrap();
    // Ingress wakes are recorded only; this test runs the owner's drive itself.
    assert!(counted::set_owner_wake_for_tests(channel, Some(Arc::new(|| {}))).is_none());
    let _probes = Probes(channel);
    *dispatch::COUNTED_DISPATCHES.lock().unwrap() = Some((CHANNEL, 0));

    // While Legacy's turn is unfinished the live input is held in the owner's queue, unstarted.
    legacy_turn_running();
    let deps = crate::services::discord::router::IntakeDeps {
        http: &api.http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared: &shared,
        token: "",
    };
    let held =
        crate::services::discord::router::dispatch_live_text_intake(&deps, live_input(), || {});
    held.await.unwrap();
    let early = transcript_turns(CHANNEL).then(|| TestConfirmation::confirmed(CHANNEL));
    assert!(
        early.is_none(),
        "a running Legacy turn keeps turn mode awaited"
    );
    let front = mailbox.v2_request(|slot| {
        Ok(slot
            .as_ref()
            .ok_or("no owner")?
            .front()
            .map(|f| f.1.message_id))
    });
    assert_eq!(
        front.await.unwrap(),
        Some(MESSAGE),
        "held in the owner's queue"
    );
    assert_eq!(dispatches(), 0, "nothing starts while it is held");

    // The pane ended unseen and the clear removed Legacy's row; no completion runs here.
    assert!(crate::services::discord::inflight::clear_inflight_state(
        &ProviderKind::Codex,
        CHANNEL
    ));
    let (frames_tx, frames) = std::sync::mpsc::channel();
    *dispatch::INPUT_EFFECT_OBSERVER.lock().unwrap() = Some((
        CHANNEL,
        Box::new(move |_| {
            let hook = scope::launch_hook().expect("the owner waits for the first prompt");
            let target = json!({"host":"tmux","owner":"turn-mode-fixture","root":"fixture",
                "execution_nonce":"fixture-born"});
            let source = json!({"kind":"pre_source","expected_session_id":null,
                "expected_transcript":null,"binding_watermark":null});
            let submit: scope::CreatedSubmit = Box::new(move |frame| {
                frames_tx.send(frame).unwrap();
                Ok(())
            });
            hook(scope::Lineage::Created, target, source, None, submit)
                .expect("the owner takes the first prompt");
        }),
    ));
    let (entered, release) =
        crate::services::discord::router::message_handler::hold_input_effect_for_tests(CHANNEL);
    let (completed, bridge) = tokio::sync::oneshot::channel();
    *crate::services::discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
        .lock()
        .unwrap() = Some((channel, completed));
    let starts = shared.clone();
    let idle: Handoff = Arc::new(move |front| Box::pin(start_idle(starts.clone(), channel, front)));
    let mut drive = owner_drive(
        (mailbox.clone(), Some(pool.clone())),
        (ProviderKind::Codex, CHANNEL),
        idle,
        Some(shared.clone()),
    );
    let (pass_tx, mut passes) = tokio::sync::mpsc::unbounded_channel();
    let settled = drive.settled.clone();
    Arc::get_mut(&mut drive).unwrap().settled = Some(Arc::new(move || {
        let (settled, pass) = (settled.clone(), pass_tx.clone());
        Box::pin(async move {
            if let Some(settled) = settled {
                settled().await;
            }
            let _ = pass.send(());
        })
    }));

    // One drive, with no further input, starts the held front.
    wake(drive.clone());
    await_within(passes.recv(), WAIT_CAP).await.unwrap();
    await_within(entered, WAIT_CAP).await.unwrap();
    let confirmed = transcript_turns(CHANNEL).then(|| TestConfirmation::confirmed(CHANNEL));
    assert!(
        confirmed.is_some(),
        "the drive's admission confirmed turn mode"
    );
    let frame = frames
        .try_recv()
        .expect("the held input reached the provider once");
    assert!(frame.contains(TEXT), "{frame}");
    release.send(()).unwrap();
    await_within(bridge, WAIT_CAP).await.unwrap();
    let state = mailbox.v2_request(|slot| slot.as_ref().ok_or("no owner")?.json());
    let state = state.await.unwrap();
    assert_eq!(state["items"], json!([]), "{state}");
    assert!(state["head_injection"].is_null(), "{state}");

    // Later wakes find nothing to start again.
    for _ in 0..2 {
        wake(drive.clone());
        await_within(passes.recv(), WAIT_CAP).await.unwrap();
    }
    assert!(frames.try_recv().is_err(), "the input started only once");
    assert_eq!(dispatches(), 1);

    for handle in hosts {
        await_within(handle.stop_and_join(), WAIT_CAP)
            .await
            .unwrap();
    }
    server.abort();
    shared.mailboxes.remove_fixture_for_test(channel);
    pool.close().await;
    db.drop().await;
}

const LIVE_CHANNEL: u64 = 7_942_002;
const LIVE_MESSAGE: u64 = 1_568_820_654_013_415_475;
const LIVE_SESSION: &str = "019e7942-0000-7000-8000-000000000002";
const ROWS: usize = 40;

fn codex_fixture(name: &str) -> String {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    fs::read_to_string(dir.join(name)).unwrap()
}

/// Shows the bottom rows of a captured busy or idle Codex screen; the renderer redraws it.
fn show(root: &Path, idle: bool) {
    let shown = if idle {
        "\n".repeat(ROWS) + &codex_fixture("tui_input/codex-status-sol-80-idle.ansi")
    } else {
        codex_fixture("codex_busy_inject/busy_empty.ansi")
    };
    let lines: Vec<&str> = shown.lines().collect();
    let rows = &lines[lines.len() - ROWS..];
    let plain = |row: &str| crate::services::codex_tui::input::strip_ansi_escape_sequences(row);
    let prompt = rows.iter().rposition(|row| plain(row).starts_with('›'));
    fs::write(root.join("prompt-row"), prompt.unwrap().to_string()).unwrap();
    fs::write(root.join("screen"), rows.join("\n")).unwrap();
}

/// A live pane drawing a captured Codex screen with the cursor in its composer; it redraws when
/// the screen changes, records a bracketed paste and each Enter.
fn codex_pane(f: &Fixture, session: &str) -> std::path::PathBuf {
    let root = f.root.path().join("live-codex");
    fs::create_dir(&root).unwrap();
    show(&root, false);
    let script = root.join("renderer.py");
    fs::write(
        &script,
        r#"import os, pathlib, select, sys, tty
tty.setraw(0)
root = pathlib.Path(sys.argv[1])
seen = None
def render(draft):
    global seen
    seen = (root / 'screen').stat().st_mtime_ns
    rows = (root / 'screen').read_text().split('\n')
    at = int((root / 'prompt-row').read_text())
    if draft: rows[at] = '› ' + draft
    out = '\x1b[?2004h\x1b[2J' + ''.join('\x1b[%d;1H%s' % (i + 1, row) for i, row in enumerate(rows))
    os.write(1, (out + '\x1b[%d;%dH' % (at + 1, 3 + len(draft))).encode())
def event(name):
    with (root / 'events').open('a') as out: out.write(name + '\n')
render('')
(root / 'ready').write_text('ready')
pending = b''
while True:
    if not select.select([0], [], [], 0.05)[0]:
        if (root / 'screen').stat().st_mtime_ns != seen: render('')
        continue
    pending += os.read(0, 65536)
    if pending.startswith(b'\x1b[200~') and b'\x1b[201~' in pending:
        body, pending = pending[6:].split(b'\x1b[201~', 1)
        (root / 'body').write_bytes(body); event('paste')
        render(body.decode().split('\n')[0])
    if b'\r' in pending:
        event('Enter'); render(''); pending = b''
"#,
    )
    .unwrap();
    let command = format!("python3 '{}' '{}'", script.display(), root.display());
    let height = ROWS.to_string();
    let pane = f.tmux(&[
        "new-session",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-s",
        session,
        "-x",
        "160",
        "-y",
        &height,
        &command,
    ]);
    f.tmux(&["set-window-option", "-t", &pane, "window-size", "manual"]);
    f.tmux(&["resize-window", "-t", &pane, "-x", "160", "-y", &height]);
    root
}

fn events(root: &Path) -> Vec<String> {
    fs::read_to_string(root.join("events"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn append(path: &Path, records: &[serde_json::Value]) {
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    for record in records {
        writeln!(file, "{record}").unwrap();
    }
}

fn marks(shared: &crate::services::discord::SharedData, message: u64) -> Vec<char> {
    let mut shown = std::collections::BTreeSet::new();
    let ops = shared.turn_view_reconciler.ops();
    for op in ops
        .into_iter()
        .filter(|op| op.target.message_id.get() == message)
    {
        if op.add {
            shown.insert(op.emoji);
        } else {
            shown.remove(&op.emoji);
        }
    }
    shown.into_iter().collect()
}

#[track_caller]
fn until(mut done: impl FnMut() -> bool) -> impl std::future::Future<Output = ()> {
    await_within(
        async move {
            while !done() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        },
        WAIT_CAP,
    )
}

#[tokio::test(flavor = "current_thread")]
async fn a_held_codex_input_runs_once_after_its_live_busy_pane_turns_idle_pg() {
    use crate::services::discord::turn_presence::{activity, supervisor};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = Fixture::after_shared_test_env_lock();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let root = crate::config::runtime_root().unwrap();
    let config = crate::runtime_layout::config_file_path(&root);
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "server: {}\ndata: {dir: data}\n").unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config,
    );
    let hosting: crate::config::Config = serde_json::from_value(json!({
        "server": {},
        "agents": [{"id": "w", "name": "W",
            "channels": {"codex": {"id": LIVE_CHANNEL.to_string(), "runtime": "tui"}}}]
    }))
    .unwrap();
    crate::services::provider_hosting::install_provider_hosting_config(&hosting);
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    fs::create_dir_all(generation.parent().unwrap()).unwrap();
    fs::write(generation, "1").unwrap();
    let api = crate::services::discord::admin_host_guard::tests::Recorder::start().await;
    crate::services::discord::internal_api::init(api.port, None);
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "http://127.0.0.1:{}",
        api.port
    ));
    let mock = discord_mock::DiscordMockState::new();
    mock.allow_channel(LIVE_CHANNEL);
    let (proxy, gateway, server) = discord_mock::start(mock).await;
    let mut ctx = discord_mock::serenity_context(proxy, gateway).await;
    ctx.http = api.http.clone();
    let gateway_ctx = ctx.clone();
    let mut shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Codex;
    Arc::get_mut(&mut shared).unwrap().api_port = api.port;
    shared.settings.write().await.provider = ProviderKind::Codex;
    shared.restart.reconcile_done.store(true, Ordering::SeqCst);
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(discord_mock::user_message(1, "").author.id.get());
        settings.allow_all_users = true;
    }
    shared.http.cached_serenity_ctx.set(ctx).ok().unwrap();
    shared
        .http
        .cached_bot_token
        .set("turn-mode-live".into())
        .ok()
        .unwrap();
    let channel = ChannelId::new(LIVE_CHANNEL);
    crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, "").await;
    let workspace = tempfile::tempdir().unwrap();
    let name = "turn-mode-live";
    {
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        session.channel_name = Some(name.into());
        session.current_path = Some(workspace.path().display().to_string());
        session.session_id = Some(LIVE_SESSION.into());
    }

    // A live Codex pane whose rollout is in a model turn: busy, with no end record yet.
    let session = ProviderKind::Codex.build_tmux_session_name(name);
    let pane = codex_pane(&tmux, &session);
    until(|| pane.join("ready").exists()).await;
    // The rollout lives where Codex keeps its sessions, so the pane's own reader resolves it.
    let codex_home = tempfile::tempdir().unwrap();
    let _codex_home = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "CODEX_HOME",
        codex_home.path(),
    );
    let day = codex_home.path().join("sessions/2026/11/08");
    fs::create_dir_all(&day).unwrap();
    let rollout = day.join(format!("rollout-2026-11-08T00-00-00-{LIVE_SESSION}.jsonl"));
    let meta = json!({"type":"session_meta","payload":{"id":LIVE_SESSION,"cwd":workspace.path(),
        "source":"cli","originator":"codex-tui"}});
    let turn = "019e7942-0000-7000-8000-0000000000a1";
    let open = format!(
        "{}\n{}\n",
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":turn}}),
        json!({"type":"response_item","payload":{"type":"message","id":"msg_sleep","role":"user",
            "content":[{"type":"input_text","text":"sleep 2100"}]}}),
    );
    fs::write(&rollout, format!("{meta}\n")).unwrap();
    let nonce = format!("{:032x}", LIVE_CHANNEL);
    let context = json!({"schema":1,"provider":"codex","created_at":chrono::Utc::now(),
        "execution_nonce":&nonce,"tmux_session":&session,"channel_id":LIVE_CHANNEL,
        "owner_runtime_root":crate::services::tmux_common::current_tmux_owner_marker(),
        "host":"turn-mode-live","expected_native_session_id":null,"launch_mode":"fresh",
        "provider_root":null});
    crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(
        serde_json::from_value(context).unwrap(),
    )
    .unwrap();
    let marker = crate::services::tmux_common::session_temp_path(&session, "spawn_nonce");
    fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
    fs::write(marker, &nonce).unwrap();
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        &session,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
            output_path: rollout.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(LIVE_SESSION.into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    crate::services::codex_tui::session::write_codex_tui_rollout_marker(
        &session,
        &rollout,
        Some(LIVE_SESSION),
    )
    .unwrap();
    let binding_log = tempfile::tempdir().unwrap();
    let _binding_log = activity::tests::BindingRoot::enter(binding_log.path());
    shared.tmux_watchers.insert(
        channel,
        crate::services::discord::TmuxWatcherHandle {
            tmux_session_name: session.clone(),
            output_path: rollout.display().to_string(),
            paused: Arc::new(false.into()),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(false.into()),
            pause_epoch: Arc::new(0.into()),
            turn_delivered: Arc::new(false.into()),
            last_heartbeat_ts_ms: Arc::new(chrono::Utc::now().timestamp_millis().into()),
        },
    );
    // The channel's binding log names the rollout as the pane's source, as the binding writer does.
    {
        use crate::services::tui_prompt_dedupe::binding_events as p5;
        let now = chrono::Utc::now();
        let event = p5::BindingEvent {
            seq: 1,
            channel_id: LIVE_CHANNEL,
            provider: "codex".into(),
            tmux_session: session.clone(),
            execution_nonce: Some(nonce.clone()),
            old: None,
            new: p5::BindingTarget::Source(source_id_for(LIVE_SESSION, &rollout).unwrap()),
            cause: p5::BindingCause::Startup,
            parent_hint: None,
            evidence: p5::BindingEvidence {
                hook_event: Some("SessionStart".into()),
                received_at: now,
            },
            committed_at: now,
        };
        let dir = p5::test_root().unwrap().join(p5::BINDING_EVENTS_DIR);
        fs::create_dir_all(&dir).unwrap();
        let line = format!("{}\n", serde_json::to_string(&event).unwrap());
        fs::write(dir.join(format!("{LIVE_CHANNEL}.log")), line).unwrap();
    }

    // The gateway hosts the channel's writer; turn mode waits on Legacy's unfinished turn.
    let config = serde_json::from_value(json!({
        "server": {},
        "agents": [{"id": "w", "name": "W",
            "channels": {"codex": {"id": LIVE_CHANNEL.to_string(), "runtime": "tui"}}}],
        "tui_o": {"writer": {"channels": [LIVE_CHANNEL]}, "turn": {"channels": [LIVE_CHANNEL]}}
    }))
    .unwrap();
    let boot = BootChannels::validate(&config)
        .unwrap()
        .adopted(Adoption::Pending);
    let _boot = test_override::force_boot(boot);
    let io = TestHost::new([(LIVE_CHANNEL, source_id_for(LIVE_SESSION, &rollout).unwrap())]);
    io.sessions
        .lock()
        .unwrap()
        .insert(LIVE_CHANNEL, session.clone());
    let gate = Arc::new(OwnershipGate::default());
    let parts = HostParts {
        io: Arc::clone(&io),
        runtime_root: Some(root.clone()),
        gate: Arc::clone(&gate),
        readiness: host::process_readiness(),
    };
    let hosts = host::start_managed(
        ShadowProvider::Codex,
        true,
        cutover::boot_ownership(),
        || parts,
    );
    gate.acquired();
    until(|| host::channel_accepts(LIVE_CHANNEL)).await;
    // The writer adopted the metadata-only rollout; its model turn is open from here on.
    fs::OpenOptions::new()
        .append(true)
        .open(&rollout)
        .unwrap()
        .write_all(open.as_bytes())
        .unwrap();
    let mailbox = shared.mailbox(channel);
    let adopted = crate::services::discord::queue_io::input_ingress::ADOPTS.scope(
        true,
        crate::services::discord::queue_io::input_ingress::adopt(
            &shared,
            &ProviderKind::Codex,
            channel,
        ),
    );
    assert_eq!(
        adopted.await,
        Ok(true),
        "the local TUI admission installs the owner"
    );
    let presence =
        supervisor::tests::LiveSupervisor::new(shared.clone(), ProviderKind::Codex, LIVE_CHANNEL);
    let front = || {
        let mailbox = mailbox.clone();
        async move {
            let front = mailbox.v2_request(|slot| {
                Ok(slot
                    .as_ref()
                    .ok_or("no owner")?
                    .front()
                    .map(|f| f.1.message_id))
            });
            front.await.unwrap()
        }
    };

    // (a) While Legacy's turn is unfinished the live input waits in the owner's queue.
    {
        use crate::services::discord::inflight::{self, InflightTurnState};
        let row = InflightTurnState::new(
            ProviderKind::Codex,
            LIVE_CHANNEL,
            None,
            1,
            7,
            0,
            "sleep".into(),
            Some(LIVE_SESSION.into()),
            Some(session.clone()),
            Some(rollout.display().to_string()),
            None,
            0,
        );
        let at = inflight::inflight_runtime_root().unwrap();
        let file = inflight::inflight_state_path(&at, &ProviderKind::Codex, LIVE_CHANNEL);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let _lock = inflight::lock_inflight_state_path(&file).unwrap();
        fs::write(&file, serde_json::to_vec(&row).unwrap()).unwrap();
    }
    // The live gateway takes the message as Discord delivers it.
    let mut message = discord_mock::user_message(LIVE_MESSAGE, TEXT);
    message.channel_id = channel;
    let data = Arc::new(crate::services::discord::Data {
        shared: shared.clone(),
        token: "test-token".to_string(),
        provider: ProviderKind::Codex,
    });
    let event = serenity::FullEvent::Message {
        new_message: message,
    };
    let handled = tokio::spawn(async move {
        crate::services::discord::queue_io::input_ingress::ADOPTS
            .scope(true, async {
                let handled =
                    crate::services::discord::router::handle_event(&gateway_ctx, &event, &data);
                Box::pin(handled).await.map_err(|error| error.to_string())
            })
            .await
    });
    let took = await_within(handled, WAIT_CAP).await.unwrap();
    assert_eq!(took, Ok(()), "the gateway took the input");
    assert_eq!(
        front().await,
        Some(LIVE_MESSAGE),
        "held in the owner's queue"
    );
    assert!(
        events(&pane).is_empty(),
        "nothing reaches the pane while it is held"
    );

    // (b) Legacy's row goes with no completion here; the pane still runs, so the front stays.
    assert!(crate::services::discord::inflight::clear_inflight_state(
        &ProviderKind::Codex,
        LIVE_CHANNEL
    ));
    until(|| transcript_turns(LIVE_CHANNEL)).await;
    let _confirmed = TestConfirmation::confirmed(LIVE_CHANNEL);
    // The confirmed start meets the busy pane: either it writes into it or it is held with 📬.
    until(|| marks(&shared, LIVE_MESSAGE).contains(&'📬') || !events(&pane).is_empty()).await;
    assert!(events(&pane).is_empty(), "a busy pane takes no input");
    assert!(
        marks(&shared, LIVE_MESSAGE).contains(&'📬'),
        "the held input shows 📬"
    );
    // The supervisor's reader catches up on the rollout and reads the open turn as busy.
    await_within(
        async {
            while presence.state()["activity"] != "busy" {
                presence.tick().await;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        },
        WAIT_CAP,
    )
    .await;
    assert!(events(&pane).is_empty(), "a busy pane still takes no input");
    assert_eq!(
        front().await,
        Some(LIVE_MESSAGE),
        "the front stays the owner's"
    );

    // (c) The rollout ends its turn and the pane turns idle; the supervisor's idle wakes the owner.
    append(
        &rollout,
        &[
            json!({"type":"response_item","payload":{"type":"message","id":"msg_slept",
                "role":"assistant","content":[{"type":"output_text","text":"slept"}]}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":turn}}),
        ],
    );
    show(&pane, true);
    let (completed, bridge) = tokio::sync::oneshot::channel();
    *crate::services::discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
        .lock()
        .unwrap() = Some((channel, completed));
    await_within(
        async {
            while events(&pane).len() < 2 {
                presence.tick().await;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        },
        WAIT_CAP,
    )
    .await;
    assert_eq!(events(&pane), ["paste", "Enter"]);
    let body = fs::read_to_string(pane.join("body")).unwrap();
    assert!(body.contains(TEXT), "{body}");
    let next = "019e7942-0000-7000-8000-0000000000aa";
    append(
        &rollout,
        &[
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":next}}),
            json!({"type":"response_item","payload":{"type":"message","id":"msg_input",
                "role":"user","content":[{"type":"input_text","text":body}]}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":body}}),
            json!({"type":"response_item","payload":{"type":"message","id":"msg_answer",
                "role":"assistant","content":[{"type":"output_text","text":"answered"}]}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":next}}),
        ],
    );
    await_within(bridge, WAIT_CAP).await.unwrap();
    let state = mailbox.v2_request(|slot| slot.as_ref().ok_or("no owner")?.json());
    let state = state.await.unwrap();
    assert_eq!(state["items"], json!([]), "{state}");
    assert!(state["head_injection"].is_null(), "{state}");

    // (d) Later supervisor ticks start nothing again.
    for _ in 0..3 {
        presence.tick().await;
    }
    assert_eq!(
        events(&pane),
        ["paste", "Enter"],
        "the input started only once"
    );

    for handle in hosts {
        await_within(handle.stop_and_join(), WAIT_CAP)
            .await
            .unwrap();
    }
    server.abort();
    shared.mailboxes.remove_fixture_for_test(channel);
    pool.close().await;
    db.drop().await;
}
