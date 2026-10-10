#![cfg(unix)]
//! Both turn creators persist the submission boundary before the provider runs, and a refused
//! fence reaches the producer and bridge as a hold rather than a provider failure.

use super::super::*;
use crate::services::discord::inflight::{SubmissionPhase, fault};
use crate::services::discord::input_runtime::fence;
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
    let gate = fence::Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
    let _health = fence::test_health::Clear::new(&gate);
    if cell.refuse {
        fault::arm(channel.get(), fault::FenceWriteFault::Write);
    }
    let (report, at_provider) = std::sync::mpsc::channel();
    *super::CLAUDE_LEGACY_PROBE.lock().unwrap() = Some((
        channel.get(),
        Box::new(move || {
            let phase_at_entry = durable_phase(channel);
            let fence = crate::services::claude_tui::submission_fence::before_first_payload();
            let _ = report.send(AtProvider {
                phase_at_entry,
                fence: fence.clone(),
                phase_after_fence: durable_phase(channel),
            });
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
        discord,
    };
    tokio::time::timeout(timeout, gate.close().unwrap().drain())
        .await
        .expect("no input effect outlives the turn");
    outcome
}

async fn creator_cells(creator: Creator) {
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
    };
    let cells = [
        cell(false, false, false),
        cell(true, false, false),
        cell(false, true, false),
        cell(true, true, false),
        cell(false, false, true),
    ];
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
        if cell.refuse {
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
}

#[tokio::test]
async fn headless_creator_installs_waiting_and_holds_a_refused_prompt() {
    if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::submission_tests::",
        "headless_creator_installs_waiting_and_holds_a_refused_prompt"
    )) {
        return;
    }
    creator_cells(Creator::Headless).await;
}

#[tokio::test]
async fn intake_creator_installs_waiting_and_holds_a_refused_prompt() {
    if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::submission_tests::",
        "intake_creator_installs_waiting_and_holds_a_refused_prompt"
    )) {
        return;
    }
    creator_cells(Creator::Intake).await;
}
