#![cfg(unix)]
//! Both turn creators persist the submission boundary before the provider runs, and a refused
//! fence reaches the producer and bridge as a hold rather than a provider failure.

use super::super::*;
use crate::services::discord::inflight::{SubmissionPhase, fault};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Creator {
    Headless,
    Intake,
}

/// What the stand-in Claude driver saw on the real provider thread.
#[derive(Debug)]
struct AtProvider {
    phase_at_entry: Option<SubmissionPhase>,
    fence: Result<(), String>,
    phase_after_fence: Option<SubmissionPhase>,
}

fn durable_phase(channel: ChannelId) -> Option<SubmissionPhase> {
    crate::services::discord::inflight::load_inflight_state(&ProviderKind::Claude, channel.get())
        .and_then(|row| row.managed_submission)
        .map(|record| record.phase)
}

struct Outcome {
    at_provider: AtProvider,
    row: Option<crate::services::discord::inflight::InflightTurnState>,
    mailbox_active: bool,
    requeued: usize,
    /// Every Discord write of the turn, so a generic failure post is visible.
    discord: Vec<String>,
}

/// One turn of the matrix; `fail_after_fence` is the provider failing once the prompt may
/// have landed, the path a held prompt must not take.
#[derive(Debug, Clone, Copy)]
struct Cell {
    warm: bool,
    refuse: bool,
    fail_after_fence: bool,
    guarded: bool,
    cancel_pending: bool,
}

const PROVIDER_EXIT: &str = "zp1 provider exited after submit";

/// The Discord, API and workspace fixtures every cell of one creator shares.
struct Fixture<'a> {
    ctx: &'a serenity::Context,
    mock: &'a discord_mock::DiscordMockState,
    api: &'a crate::services::discord::admin_host_guard::tests::Recorder,
    shared: &'a Arc<SharedData>,
    workspace: &'a std::path::Path,
}

async fn run_turn(
    creator: Creator,
    fixture: &Fixture<'_>,
    channel: ChannelId,
    cell: Cell,
) -> Outcome {
    let Fixture {
        ctx,
        mock,
        api,
        shared,
        workspace,
    } = *fixture;
    crate::services::discord::host_defer_gate::tests::map_channel(shared, channel, "").await;
    {
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        session.channel_name = Some(format!("zp1-entry-{}", channel.get()));
        session.current_path = Some(workspace.display().to_string());
        session.session_id = cell.warm.then(|| "zp1-existing-session".to_string());
    }
    if cell.refuse {
        fault::arm(channel.get(), fault::FenceWriteFault::Write);
    }
    let (report, at_provider) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let driver_shared = Arc::clone(shared);
    *super::CLAUDE_LEGACY_PROBE.lock().unwrap() = Some((
        channel.get(),
        Box::new(move || {
            if cell.guarded {
                crate::services::claude_tui::guarded_outcome::begin();
            }
            let phase_at_entry = durable_phase(channel);
            let fence = crate::services::claude_tui::submission_fence::before_first_payload();
            let _ = report.send(AtProvider {
                phase_at_entry,
                fence: fence.clone(),
                phase_after_fence: durable_phase(channel),
            });
            if cell.guarded && fence.is_ok() {
                if cell.cancel_pending {
                    let mailbox = driver_shared.mailbox_peek(channel).unwrap();
                    let snapshot = tokio::runtime::Handle::current().block_on(mailbox.snapshot());
                    snapshot
                        .cancel_token
                        .unwrap()
                        .cancelled
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    released
                        .recv_timeout(std::time::Duration::from_secs(20))
                        .unwrap();
                }
                use crate::services::claude_tui::{
                    guarded_outcome::SubmissionError,
                    host_input::{InputRun, StopCause},
                };
                return Err(SubmissionError::Guarded(InputRun::Indeterminate {
                    confirmed: 0,
                    cause: StopCause::Send("guarded payload ACK unknown".into()),
                })
                .into_legacy());
            }
            match fence {
                Ok(()) if cell.fail_after_fence => Err(PROVIDER_EXIT.to_string()),
                fence => fence,
            }
        }),
    ));
    let (completed, bridge_completed) = tokio::sync::oneshot::channel();
    *crate::services::discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
        .lock()
        .unwrap() = Some((channel, completed));
    let timeout = std::time::Duration::from_secs(20);
    match creator {
        Creator::Headless => {
            let started = tokio::time::timeout(
                timeout,
                start_reserved_headless_turn_with_owner(
                    ctx,
                    channel,
                    "zp1 headless prompt",
                    "owner",
                    UserId::new(7),
                    shared,
                    "",
                    None,
                    Some(serde_json::json!({"silent": true})),
                    None,
                    None,
                    Some(true),
                    reserve_headless_turn(),
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(started.status, HeadlessTurnStartStatus::Started);
        }
        Creator::Intake => {
            let request = IntakeRequest {
                intake_outbox_id: None,
                channel_id: channel,
                user_msg_id: MessageId::new(channel.get() + 1),
                source_message_ids: Vec::new(),
                busy_followup_retry_user_msg_id: MessageId::new(channel.get() + 1),
                request_owner: UserId::new(7),
                request_owner_name: "owner".into(),
                user_text: "zp1 intake prompt".into(),
                reply_to_user_message: false,
                defer_watcher_resume: false,
                wait_for_completion: false,
                merge_consecutive: false,
                reply_context: None,
                has_reply_boundary: false,
                dm_hint: Some(false),
                turn_kind: TurnKind::Foreground,
                preserve_on_cancel: false,
            };
            tokio::time::timeout(
                timeout,
                execute_intake_turn_core(&api.http, shared, "", request, Vec::new()),
            )
            .await
            .unwrap()
            .unwrap();
        }
    }
    let at_provider = tokio::task::spawn_blocking(move || at_provider.recv_timeout(timeout))
        .await
        .unwrap()
        .expect("the stand-in Claude driver ran inside the submission scope");
    tokio::time::timeout(timeout, bridge_completed)
        .await
        .unwrap()
        .unwrap();
    let row = crate::services::discord::inflight::load_inflight_state(
        &ProviderKind::Claude,
        channel.get(),
    );
    let mailbox = shared.mailbox_peek(channel).unwrap().snapshot().await;
    let mut discord = mock.written_texts();
    discord.extend(api.take());
    let outcome = Outcome {
        at_provider,
        row,
        mailbox_active: mailbox.cancel_token.is_some(),
        requeued: mailbox.intervention_queue.len(),
        discord,
    };
    let _ = release.send(());
    outcome
}

async fn creator_cells(creator: Creator, guarded: bool) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = crate::config::runtime_root().unwrap();
    let _tmux = crate::services::tui_prompt_dedupe::binding_context::tests::fake_tmux(&root);
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(
        "http://127.0.0.1:9".to_string(),
    );
    let boot = serde_json::from_value(serde_json::json!({"server": {}, "agents": []})).unwrap();
    crate::services::tui_o::channel_policy::install(&boot).unwrap();
    let api = crate::services::discord::admin_host_guard::tests::Recorder::start().await;
    crate::services::discord::internal_api::init(api.port, None);
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Claude;
    Arc::get_mut(&mut shared).unwrap().api_port = api.port;
    let mock = discord_mock::DiscordMockState::new();
    let (proxy, gateway, server) = discord_mock::start(mock.clone()).await;
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    let workspace = tempfile::tempdir().unwrap();
    let base = match creator {
        Creator::Headless => 6_806_000,
        Creator::Intake => 6_807_000,
    };
    let cell = |warm, refuse, fail_after_fence| Cell {
        warm,
        refuse,
        fail_after_fence,
        guarded: false,
        cancel_pending: false,
    };
    let cells = if guarded {
        vec![
            Cell {
                guarded: true,
                ..cell(true, false, false)
            },
            Cell {
                guarded: true,
                cancel_pending: true,
                ..cell(true, false, false)
            },
        ]
    } else {
        vec![
            cell(false, false, false),
            cell(true, false, false),
            cell(false, true, false),
            cell(true, true, false),
            cell(false, false, true),
        ]
    };
    let mut guarded_failures = 0;
    for (index, cell) in cells.into_iter().enumerate() {
        let channel = ChannelId::new(base + index as u64 * 10);
        mock.allow_channel(channel.get());
        let case = format!("{creator:?} {cell:?}");
        let fixture = Fixture {
            ctx: &ctx,
            mock: &mock,
            api: &api,
            shared: &shared,
            workspace: workspace.path(),
        };
        let outcome = run_turn(creator, &fixture, channel, cell).await;
        let at = &outcome.at_provider;
        assert_eq!(
            at.phase_at_entry,
            Some(SubmissionPhase::Waiting),
            "{case}: the created row already carries Waiting when the provider starts"
        );
        // The headless fixture is a silent turn, so only intake surfaces a failure post.
        let visible = creator == Creator::Intake;
        let posted = |text: &str| outcome.discord.iter().any(|write| write.contains(text));
        if let Ok(directory) = std::env::var("CG_B2C_EVIDENCE_DIR") {
            let row_phase = outcome
                .row
                .as_ref()
                .and_then(|row| row.managed_submission.as_ref())
                .map(|record| format!("{:?}", record.phase));
            let writes: Vec<_> = outcome
                .discord
                .iter()
                .map(|write| {
                    write
                        .replace(&workspace.path().display().to_string(), "<workspace>")
                        .replace(&root.display().to_string(), "<root>")
                })
                .collect();
            let record = serde_json::json!({"case": case, "phase_at_entry": format!("{:?}", at.phase_at_entry), "fence": at.fence, "phase_after_fence": format!("{:?}", at.phase_after_fence), "row_phase": row_phase, "mailbox_active": outcome.mailbox_active, "requeued": outcome.requeued, "writes": writes});
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                std::path::Path::new(&directory).join(format!("{creator:?}-{index}.json")),
                serde_json::to_vec_pretty(&record).unwrap(),
            )
            .unwrap();
        }
        if cell.guarded {
            let phase = outcome
                .row
                .as_ref()
                .and_then(|row| row.managed_submission.as_ref())
                .map(|record| &record.phase);
            let held = phase == Some(&SubmissionPhase::MayHaveSubmitted)
                && outcome.mailbox_active
                && outcome.requeued == 0
                && !posted("guarded payload ACK unknown");
            guarded_failures += usize::from(!held);
        } else if cell.refuse {
            let error = at.fence.as_ref().expect_err(&case);
            assert!(error.contains("held before submit"), "{case}: {error}");
            let row = outcome.row.as_ref().unwrap_or_else(|| {
                panic!("{case}: a held prompt keeps its row instead of a generic cleanup")
            });
            assert_eq!(
                row.managed_submission.as_ref().map(|r| r.phase.clone()),
                Some(SubmissionPhase::Waiting),
                "{case}"
            );
            assert!(
                outcome.mailbox_active,
                "{case}: no Done or error finalize released the turn"
            );
            assert!(
                !posted("held before submit"),
                "{case}: the hold never reaches Discord as a provider failure: {:?}",
                outcome.discord
            );
        } else if cell.fail_after_fence {
            assert_eq!(
                at.phase_after_fence,
                Some(SubmissionPhase::MayHaveSubmitted),
                "{case}"
            );
            assert!(
                !visible || posted(PROVIDER_EXIT),
                "{case}: a failure after the fence takes the generic error path: {:?}",
                outcome.discord
            );
            assert!(outcome.row.is_none() && !outcome.mailbox_active, "{case}");
        } else {
            assert_eq!(at.fence, Ok(()), "{case}");
            assert_eq!(
                at.phase_after_fence,
                Some(SubmissionPhase::MayHaveSubmitted),
                "{case}"
            );
            assert!(
                outcome.row.is_none() && !outcome.mailbox_active,
                "{case}: a fenced turn finishes through the normal bridge path"
            );
        }
        shared.mailboxes.remove_fixture_for_test(channel);
    }
    server.abort();
    assert_eq!(
        guarded_failures, 0,
        "guarded stops and pending cancellation retain the row and slot"
    );
}

#[tokio::test]
async fn headless_creator_installs_waiting_and_holds_a_refused_prompt() {
    if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::submission_tests::",
        "headless_creator_installs_waiting_and_holds_a_refused_prompt"
    )) {
        return;
    }
    creator_cells(Creator::Headless, false).await;
}

#[tokio::test]
async fn intake_creator_installs_waiting_and_holds_a_refused_prompt() {
    if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::submission_tests::",
        "intake_creator_installs_waiting_and_holds_a_refused_prompt"
    )) {
        return;
    }
    creator_cells(Creator::Intake, false).await;
}

#[tokio::test]
async fn cg_b2c_headless_bridge_holds_guarded_stop_and_pending_cancel() {
    if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::submission_tests::",
        "cg_b2c_headless_bridge_holds_guarded_stop_and_pending_cancel"
    )) {
        return;
    }
    creator_cells(Creator::Headless, true).await;
}

#[tokio::test]
async fn cg_b2c_intake_bridge_holds_guarded_stop_and_pending_cancel() {
    if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::submission_tests::",
        "cg_b2c_intake_bridge_holds_guarded_stop_and_pending_cancel"
    )) {
        return;
    }
    creator_cells(Creator::Intake, true).await;
}

type OwnerReport = (
    u64,
    std::path::PathBuf,
    tokio::sync::oneshot::Sender<Option<crate::services::discord::inflight::ManagedSubmission>>,
);
pub(super) static D1_OWNER: std::sync::Mutex<Option<OwnerReport>> = std::sync::Mutex::new(None);

pub(super) fn observe_owner(
    channel: u64,
    submission: Option<&crate::services::discord::inflight::ManagedSubmission>,
    prompt: &str,
) {
    let mut probe = D1_OWNER.lock().unwrap();
    if probe.as_ref().is_some_and(|(id, _, _)| *id == channel) {
        let (_, renderer, sender) = probe.take().unwrap();
        std::fs::write(renderer.join("expected"), prompt).unwrap();
        let _ = sender.send(submission.cloned());
    }
}

/// The cancelled turn still owns its birth row, fenced phase and own mailbox slot, unrequeued.
async fn assert_kept(
    stage: &str,
    shared: &SharedData,
    channel: ChannelId,
    born: &crate::services::discord::inflight::InflightEpisodePin,
    token: &Arc<crate::services::provider::CancelToken>,
) {
    let row = crate::services::discord::inflight::load_inflight_state(
        &ProviderKind::Claude,
        channel.get(),
    )
    .unwrap_or_else(|| panic!("{stage}: the cancelled turn's row was finalized"));
    assert!(
        crate::services::discord::inflight::InflightEpisodePin::from_state(&row)
            .is_same_episode_as(born),
        "{stage}: the row is the original birth episode, not a successor"
    );
    assert_eq!(
        row.managed_submission
            .as_ref()
            .map(|record| record.phase.clone()),
        Some(SubmissionPhase::MayHaveSubmitted),
        "{stage}: the fenced phase never drops back"
    );
    let mailbox = shared.mailbox_peek(channel).unwrap().snapshot().await;
    assert!(
        mailbox
            .cancel_token
            .as_ref()
            .is_some_and(|slot| Arc::ptr_eq(slot, token)),
        "{stage}: the slot still holds the cancelled turn's own token"
    );
    assert!(mailbox.intervention_queue.is_empty(), "{stage}: requeued");
    assert!(
        crate::services::discord::busy_followup_retry_store::load(
            &ProviderKind::Claude,
            channel.get(),
            row.effective_busy_followup_retry_user_msg_id(),
        )
        .is_none(),
        "{stage}: no busy follow-up retry was persisted"
    );
}

/// A one-agent config for the real creators, with a fake Claude binary that never starts.
pub(super) fn d1_fixture_config(
    f: &crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture,
    ids: &[u64],
) -> (Vec<crate::config::TestEnvVarGuard>, crate::config::Config) {
    use crate::config::TestEnvVarGuard as Env;
    use std::os::unix::fs::PermissionsExt;
    let path = f.root.path().join("cg-d1-fixture.yaml");
    let bindings = ids.iter().enumerate().map(|(index, id)| format!("  - id: cg-d1-{index}\n    name: cg-d1-{index}\n    channels:\n      claude: {{id: '{id}', runtime: tui}}\n")).collect::<String>();
    let data = serde_json::to_string(&f.root.path().join("data")).unwrap();
    let yaml = format!(
        "server: {{}}\ndata: {{dir: {data}}}\ncluster: {{instance_id: cg-d1}}\nproviders:\n  claude: {{runtime: tui}}\nagents:\n{bindings}"
    );
    std::fs::write(&path, &yaml).unwrap();
    let fake = f.root.path().join("claude-fixture");
    std::fs::write(&fake, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let env = vec![
        Env::set_path_after_shared_test_env_lock("AGENTDESK_CONFIG", &path),
        Env::set_path_after_shared_test_env_lock("CLAUDE_CONFIG_DIR", f.root.path()),
        Env::set_path_after_shared_test_env_lock("AGENTDESK_CLAUDE_PATH", &fake),
    ];
    (env, serde_yaml::from_str(&yaml).unwrap())
}

/// Stamps the markers and incarnation a spawned Claude pane `name` would carry.
pub(super) fn d1_execution_proof(
    name: &str,
    channel: u64,
) -> crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation {
    use crate::services::tmux_common as tc;
    use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
    std::fs::create_dir_all(
        std::path::Path::new(&tc::session_temp_path(name, "proof"))
            .parent()
            .unwrap(),
    )
    .unwrap();
    tc::write_tmux_owner_marker(name).unwrap();
    tc::host_marker::record_tmux_host_marker(name);
    tc::write_tmux_runtime_kind_marker(
        name,
        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
    )
    .unwrap();
    let sid = uuid::Uuid::new_v4().to_string();
    let prepared =
        PreparedIncarnation::prepare("claude", name, Some(channel), Some(&sid), false).unwrap();
    prepared
        .finish_spawn(crate::services::discord::stamp_spawn_markers(
            name,
            Some(&prepared),
        ))
        .unwrap();
    prepared
}

async fn actual_creator(creator: Creator) {
    use crate::services::claude_tui::input::submission::tests::{
        frame_renderer as renderer, screen,
    };
    use crate::services::discord::inflight::{InflightEpisodePin, SubmissionFailure};
    use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture as Tmux;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Tmux::after_shared_test_env_lock();
    let id = match creator {
        Creator::Headless => 6_809_001,
        Creator::Intake => 6_809_002,
    };
    let channel = ChannelId::new(id);
    let (_env, config) = d1_fixture_config(&f, &[id]);
    crate::config::session_hosts::install(&config).unwrap();
    crate::services::tui_o::channel_policy::install(&config).unwrap();
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(
        "http://127.0.0.1:9".into(),
    );
    let api = crate::services::discord::admin_host_guard::tests::Recorder::start().await;
    crate::services::discord::internal_api::init(api.port, None);
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Claude;
    Arc::get_mut(&mut shared).unwrap().api_port = api.port;
    let mock = discord_mock::DiscordMockState::new();
    mock.allow_channel(id);
    let (proxy, gateway, server) = discord_mock::start(mock.clone()).await;
    let ctx = discord_mock::serenity_context(proxy, gateway).await;
    let workspace = f.root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let channel_name = format!("cg-d1-real-{id}");
    let name = ProviderKind::Claude.build_tmux_session_name(&channel_name);
    const BODY: &str = "cg d1 actual";
    renderer(&f, &name, BODY, &screen(""), &screen(BODY), &screen(""));
    let proof = d1_execution_proof(&name, id);
    let sid = proof.context.expected_native_session_id.as_deref().unwrap();
    let transcript = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        &workspace,
        sid,
        Some(f.root.path()),
    )
    .unwrap();
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n").unwrap();
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        &name,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            output_path: transcript.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(sid.into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, &channel_name)
        .await;
    {
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        session.current_path = Some(workspace.display().to_string());
        session.session_id = Some(sid.into());
    }
    let shim = f.root.path().join("tmux");
    let script = std::fs::read_to_string(&shim).unwrap();
    let entered = f.root.path().join("d1-payload-barrier");
    let release = f.root.path().join("d1-payload-release");
    let quote =
        |p: &std::path::Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
    let barrier = format!(
        "if [ \"$2\" = paste-buffer ]; then\n touch {entered}\n {real} -L {socket} -f /dev/null \"$@\" >/dev/null\n while [ ! -f {release} ]; do sleep .02; done\n exit 0\nfi\n",
        real = quote(&f.real),
        socket = f.socket,
        entered = quote(&entered),
        release = quote(&release)
    );
    let old = format!(
        "exec {} -L {} -f /dev/null \"$@\"",
        quote(&f.real),
        f.socket
    );
    assert!(script.contains(&old));
    std::fs::write(&shim, script.replace(&old, &(barrier + &old))).unwrap();
    let (observed, owner) = tokio::sync::oneshot::channel();
    *D1_OWNER.lock().unwrap() = Some((id, f.root.path().join(&name), observed));
    let (finished, bridge) = tokio::sync::oneshot::channel();
    *crate::services::discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
        .lock()
        .unwrap() = Some((channel, finished));
    assert!(
        super::CLAUDE_LEGACY_PROBE.lock().unwrap().is_none(),
        "no stand-in driver may replace the real input entry"
    );
    let timeout = std::time::Duration::from_secs(20);
    match creator {
        Creator::Headless => {
            tokio::time::timeout(
                timeout,
                start_reserved_headless_turn_with_owner(
                    &ctx,
                    channel,
                    BODY,
                    "owner",
                    UserId::new(7),
                    &shared,
                    "",
                    None,
                    Some(serde_json::json!({"silent":true})),
                    None,
                    None,
                    Some(true),
                    reserve_headless_turn(),
                ),
            )
            .await
            .unwrap()
            .unwrap();
        }
        Creator::Intake => {
            let request = IntakeRequest {
                intake_outbox_id: None,
                channel_id: channel,
                user_msg_id: MessageId::new(channel.get() + 1),
                source_message_ids: Vec::new(),
                busy_followup_retry_user_msg_id: MessageId::new(channel.get() + 1),
                request_owner: UserId::new(7),
                request_owner_name: "owner".into(),
                user_text: BODY.into(),
                reply_to_user_message: false,
                defer_watcher_resume: false,
                wait_for_completion: false,
                merge_consecutive: false,
                reply_context: None,
                has_reply_boundary: false,
                dm_hint: Some(false),
                turn_kind: TurnKind::Foreground,
                preserve_on_cancel: false,
            };
            tokio::time::timeout(
                timeout,
                execute_intake_turn_core(&api.http, &shared, "", request, Vec::new()),
            )
            .await
            .unwrap()
            .unwrap();
        }
    }
    let cap = tokio::time::timeout(timeout, owner)
        .await
        .unwrap()
        .unwrap()
        .expect("the actual creator installed a managed capability");
    let mailbox = shared.mailbox_peek(channel).unwrap();
    let token = mailbox
        .snapshot()
        .await
        .cancel_token
        .expect("an active real provider");
    struct Release {
        token: Arc<crate::services::provider::CancelToken>,
        path: std::path::PathBuf,
    }
    impl Drop for Release {
        fn drop(&mut self) {
            self.token
                .cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = std::fs::write(&self.path, "release");
        }
    }
    let release_guard = Release {
        token: Arc::clone(&token),
        path: release.clone(),
    };
    tokio::time::timeout(timeout, async {
        while !f.root.path().join(&name).join("payloads").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        entered.exists(),
        "the real entry must reach the managed paste"
    );
    assert_eq!(
        durable_phase(channel),
        Some(SubmissionPhase::MayHaveSubmitted)
    );
    // T0: the paste is parked, so only the managed attempt's own begin() can have made the owner
    // Running; nothing in this test calls begin().
    assert!(
        matches!(cap.classify_failure(), SubmissionFailure::GuardedRunning),
        "T0: the managed attempt must notify begin before its paste"
    );
    let pane = f.root.path().join(&name);
    let count = |file: &std::path::Path| std::fs::read(file).map_or(0, |bytes| bytes.len());
    assert_eq!(
        (count(&pane.join("payloads")), count(&pane.join("enters"))),
        (1, 0),
        "T0: one payload and no Enter"
    );
    let frame = std::fs::read(pane.join("frame")).unwrap();
    let delivered = match std::fs::read_to_string(pane.join("kind")).unwrap().as_str() {
        "paste" => [b"\x1b[200~".as_slice(), &frame, b"\x1b[201~"].concat(),
        _ => frame,
    };
    assert_eq!(std::fs::read(pane.join("received")).unwrap(), delivered);
    let born = InflightEpisodePin::from_state(
        &crate::services::discord::inflight::load_inflight_state(&ProviderKind::Claude, id)
            .unwrap(),
    );
    token
        .cancelled
        .store(true, std::sync::atomic::Ordering::SeqCst);
    tokio::time::timeout(timeout, bridge)
        .await
        .unwrap()
        .unwrap();
    // T1: the bridge is done while the executor still has no result to report.
    assert_kept("T1", &shared, channel, &born, &token).await;
    assert!(matches!(
        cap.classify_failure(),
        SubmissionFailure::GuardedRunning
    ));
    assert_eq!(
        count(&pane.join("enters")),
        0,
        "T1: no Enter before release"
    );
    assert_eq!(
        std::fs::read(f.root.path().join(&name).join("frame")).unwrap(),
        std::fs::read(f.root.path().join(&name).join("expected")).unwrap()
    );
    let submitted = std::fs::read_to_string(f.root.path().join(&name).join("expected")).unwrap();
    assert!(
        submitted.contains(BODY),
        "the real creator must preserve the requested body"
    );
    let mut writes = mock.written_texts();
    writes.extend(api.take());
    assert!(
        !writes
            .iter()
            .any(|write| write.contains("guarded submission") || write.contains("ACK unknown"))
    );
    println!(
        "CG_D1_ACTUAL_CREATOR {creator:?} phase=MayHaveSubmitted owner=Running payload=1 enter=0 requeue=0"
    );
    drop(release_guard);
    // T2: the released paste is followed by its one Enter, which completes the attempt.
    tokio::time::timeout(timeout, async {
        while !matches!(cap.classify_failure(), SubmissionFailure::IdleStashObserved) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        (
            std::fs::read(pane.join("received")).unwrap(),
            count(&pane.join("payloads")),
            count(&pane.join("enters")),
        ),
        ([delivered, b"\r".to_vec()].concat(), 1, 1),
        "T2: one payload and one Enter reached the pane"
    );
    assert_kept("T2", &shared, channel, &born, &token).await;
    println!("CG_C32 {creator:?} T2=Applied payload=1 enter=1");
    shared.mailboxes.remove_fixture_for_test(channel);
    server.abort();
}

/// The child inherits this process's environment, so no env fixture may change it meanwhile.
fn creator_child(test: &str) -> bool {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    crate::services::discord::admin_host_guard::tests::api_child(test)
}

#[tokio::test]
async fn cg_d1_headless_actual_begin_preserves_running_cancelled_bridge() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::submission_tests::",
        "cg_d1_headless_actual_begin_preserves_running_cancelled_bridge"
    )) {
        return;
    }
    actual_creator(Creator::Headless).await;
}
#[tokio::test]
async fn cg_d1_intake_actual_begin_preserves_running_cancelled_bridge() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::submission_tests::",
        "cg_d1_intake_actual_begin_preserves_running_cancelled_bridge"
    )) {
        return;
    }
    actual_creator(Creator::Intake).await;
}
