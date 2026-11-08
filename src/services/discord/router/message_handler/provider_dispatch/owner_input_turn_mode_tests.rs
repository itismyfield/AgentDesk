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
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
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
