//! While a selected Codex channel's takeover turn still runs on Legacy, a live message waits queued
//! in the mailbox, attachments included, and its checkpoint moves only once the queue holds it.

use super::*;
use crate::services::cluster::attachment_transfer::{store, uploads::Upload};
use crate::services::discord::router::message_handler::{AttachmentDescriptor, TEST_DOWNLOADS};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::tui_o::channel_policy::{Adoption, BootChannels};
use crate::services::tui_o::cutover::{intake_route::test_probe, test_override};
use crate::services::tui_o::turn_mode::{self, TestConfirmation};
use std::sync::atomic::{AtomicUsize, Ordering};

const CHANNEL: u64 = 4_370_001;
const URL: &str = "https://cdn.discordapp.com/attachments/4370001/1/report.txt";

fn committed_and_selected() -> test_override::ChannelsGuard {
    let config = serde_json::from_value(serde_json::json!({
        "server": {},
        "agents": [{"id": "w", "name": "W",
            "channels": {"codex": {"id": CHANNEL.to_string(), "runtime": "tui"}}}],
        "tui_o": {"writer": {"channels": [CHANNEL]}, "turn": {"channels": [CHANNEL]}}
    }))
    .unwrap();
    let boot = BootChannels::validate(&config).unwrap();
    test_override::force_boot(boot.adopted(Adoption::Committed))
}

/// An isolated root with routing off and an empty config; the root guard drops last.
struct IntakeEnv {
    _config: crate::config::TestEnvVarGuard,
    _mode: crate::config::TestEnvVarGuard,
    _root: crate::config::TestRuntimeRootGuard,
}

fn intake_env() -> IntakeEnv {
    let root = crate::config::TestRuntimeRootGuard::new();
    let mode = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_INTAKE_ROUTING_MODE",
        std::ffi::OsStr::new("disabled"),
    );
    let config = crate::runtime_layout::config_file_path(&crate::config::runtime_root().unwrap());
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "server: {}\n").unwrap();
    let config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config,
    );
    IntakeEnv {
        _config: config,
        _mode: mode,
        _root: root,
    }
}

fn codex_message(message_id: u64) -> IntakeSubmission {
    IntakeSubmission {
        provider: ProviderKind::Codex,
        ..submission_for_admission(ChannelId::new(CHANNEL), message_id)
    }
}

fn with_report(mut submission: IntakeSubmission) -> IntakeSubmission {
    submission.attachments = vec![AttachmentDescriptor {
        filename: "report.txt".to_string(),
        url: URL.to_string(),
    }];
    submission
}

/// The live entry point's result and how many times it moved the checkpoint.
async fn dispatched(deps: &IntakeDeps<'_>, submission: IntakeSubmission) -> (bool, usize) {
    let moved = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&moved);
    let checkpoint = move || {
        counted.fetch_add(1, Ordering::SeqCst);
    };
    let outcome = super::super::dispatch_live_text_intake(deps, submission, checkpoint).await;
    (outcome.is_ok(), moved.load(Ordering::SeqCst))
}

async fn queued_ids(shared: &Arc<crate::services::discord::SharedData>) -> Vec<u64> {
    let mailbox = crate::services::discord::mailbox_snapshot(shared, ChannelId::new(CHANNEL));
    let queue = mailbox.await.intervention_queue;
    queue.iter().map(|i| i.message_id.get()).collect()
}

#[tokio::test(flavor = "current_thread")]
async fn a_message_during_the_codex_takeover_turn_waits_queued_until_turn_mode() {
    let _env = intake_env();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);
    let _boot = committed_and_selected();
    let _ready = test_probe::answer_with(|_| true);
    assert!(super::super::live_checkpoint_waits(
        &ProviderKind::Codex,
        ChannelId::new(CHANNEL)
    ));

    let held = codex_message(4_370_011);
    let admission = super::super::admit_text_intake(&deps, &held).await;
    assert!(
        matches!(admission, IntakeAdmission::DeferredTurnMode(Ok(_))),
        "{admission:?}"
    );
    super::super::finish_text_intake_admission(&deps, admission, held)
        .await
        .unwrap();
    assert_eq!(
        queued_ids(&shared).await,
        [4_370_011],
        "the message waits in the mailbox queue"
    );

    assert!(turn_mode::confirm(CHANNEL));
    let _confirmed = TestConfirmation::confirmed(CHANNEL);
    let next = codex_message(4_370_012);
    let admission = super::super::admit_text_intake(&deps, &next).await;
    assert!(
        matches!(admission, IntakeAdmission::Local(_)),
        "{admission:?}"
    );
}

/// An attachment that cannot be made portable, a refused enqueue and a failed queue write each
/// fail the live dispatch before its checkpoint, so the message is read again.
#[tokio::test(flavor = "current_thread")]
async fn a_turn_mode_input_the_queue_cannot_hold_fails_before_its_checkpoint() {
    let _env = intake_env();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);
    let _boot = committed_and_selected();
    let _ready = test_probe::answer_with(|_| true);
    let channel = ChannelId::new(CHANNEL);

    let unportable = with_report(codex_message(4_370_021));
    assert_eq!(
        dispatched(&deps, unportable).await,
        (false, 0),
        "no Postgres"
    );
    let active = MessageId::new(4_370_022);
    let token = Arc::new(crate::services::provider::CancelToken::new());
    let started = crate::services::discord::mailbox_try_start_turn(
        shared.as_ref(),
        channel,
        token,
        UserId::new(4350),
        active,
    );
    assert!(started.await);
    let refused = codex_message(4_370_022);
    assert_eq!(dispatched(&deps, refused).await, (false, 0), "refused");
    let queue_file = crate::services::discord::runtime_store::discord_pending_queue_root()
        .expect("queue root")
        .join("codex")
        .join(&shared.token_hash)
        .join(format!("{CHANNEL}.json"));
    std::fs::create_dir_all(&queue_file).unwrap();
    let unwritten = codex_message(4_370_023);
    assert_eq!(dispatched(&deps, unwritten).await, (false, 0), "unwritten");
    assert!(!queued_ids(&shared).await.contains(&4_370_021));
}

/// A message with an attachment waits queued as its text and its portable bundle, ahead of the
/// next message; once turn mode is confirmed the dequeue admits it once, in order.
#[tokio::test(flavor = "current_thread")]
async fn a_turn_mode_input_keeps_its_attachment_queued_in_order_until_turn_mode_pg() {
    let _env = intake_env();
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);
    let _boot = committed_and_selected();
    let _ready = test_probe::answer_with(|_| true);
    let channel = ChannelId::new(CHANNEL);
    let body = b"the report the takeover turn must not lose".to_vec();
    TEST_DOWNLOADS.with(|served| served.borrow_mut().insert(URL.to_string(), body.clone()));

    let mut first = with_report(codex_message(4_370_031));
    first.request.user_text = "read the report".to_string();
    assert_eq!(dispatched(&deps, first).await, (true, 1));
    assert_eq!(dispatched(&deps, codex_message(4_370_032)).await, (true, 1));
    let mailbox = crate::services::discord::mailbox_snapshot(&shared, channel).await;
    let queue = mailbox.intervention_queue;
    assert_eq!(queued_ids(&shared).await, [4_370_031, 4_370_032]);
    assert_eq!(queue[0].text, "read the report");
    let [Upload::Bundle(reference)] = queue[0].pending_uploads.as_slice() else {
        panic!("the attachment rides the queue item as one bundle");
    };
    let bundle = store::load(&pool, reference).await.unwrap();
    assert_eq!(bundle.as_bundle().entries[0].bytes, body);
    assert!(queue[1].pending_uploads.is_empty());

    assert!(turn_mode::confirm(CHANNEL));
    let _confirmed = TestConfirmation::confirmed(CHANNEL);
    let persistence =
        crate::services::discord::queue_persistence_context(&shared, &ProviderKind::Codex, channel);
    let dequeued = shared.mailbox(channel).take_next_soft(persistence).await;
    let head = dequeued
        .intervention
        .expect("the attachment message is the head");
    assert_eq!(
        (head.message_id.get(), head.pending_uploads.len()),
        (4_370_031, 1)
    );
    let admitted = admit_queued_intake(
        &deps,
        ProviderKind::Codex,
        channel,
        &head,
        head.author_id,
        "codex-owner".to_string(),
        false,
        false,
        "codex_turn_hold_test",
        dequeued.dispatch_lease,
    )
    .await;
    assert!(matches!(admitted, QueuedAdmissionDisposition::Admitted(_)));
    assert_eq!(queued_ids(&shared).await, [4_370_032], "admitted once");
    pool.close().await;
    fixture.drop().await;
}

/// A Discord message carrying `attachments` to the selected Codex channel.
fn gateway_message(message_id: u64, attachments: &[(&str, &str)]) -> serenity::Message {
    let mut message = discord_mock::user_message(message_id, "read the report");
    message.channel_id = ChannelId::new(CHANNEL);
    message.attachments = attachments
        .iter()
        .enumerate()
        .map(|(index, (filename, url))| {
            let attachment = serde_json::json!({"id": (message_id + 100 + index as u64).to_string(),
                "filename": filename, "size": 12, "url": url, "proxy_url": url});
            serde_json::from_value(attachment).unwrap()
        })
        .collect();
    message
}

/// Production gateway intake of `message`, on its own task so the intake future leaves this stack.
async fn through_gateway(
    ctx: &serenity::Context,
    data: &Arc<crate::services::discord::Data>,
    message: serenity::Message,
) -> bool {
    let (ctx, data) = (ctx.clone(), Arc::clone(data));
    let event = serenity::FullEvent::Message {
        new_message: message,
    };
    let intake = tokio::spawn(async move {
        let handled = crate::services::discord::router::handle_event(&ctx, &event, &data);
        Box::pin(handled).await.is_ok()
    });
    intake.await.unwrap()
}

/// The gateway's own attachment branch: a download that fails for one file or a queue that cannot
/// hold the input leaves the checkpoint behind; a held input moves it to its own message.
#[tokio::test(flavor = "current_thread")]
async fn a_gateway_attachment_moves_the_checkpoint_once_turn_mode_holds_it_pg() {
    let _env = intake_env();
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let owner = discord_mock::user_message(1, "").author.id.get();
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(owner);
        settings.allow_all_users = true;
    }
    let _boot = committed_and_selected();
    let _ready = test_probe::answer_with(|_| true);
    let mock = discord_mock::DiscordMockState::new();
    mock.allow_channel(CHANNEL);
    let (proxy, gateway_url, server) = discord_mock::start(mock).await;
    let ctx = discord_mock::serenity_context(proxy, gateway_url).await;
    let voice_config = crate::voice::VoiceConfig::default();
    let data = Arc::new(crate::services::discord::Data {
        shared: shared.clone(),
        token: "test-token".to_string(),
        provider: ProviderKind::Codex,
        voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
        voice_config,
        ingress_shadow: None,
    });
    let channel = ChannelId::new(CHANNEL);
    let checkpoint = || shared.last_message_ids.get(&channel).map(|id| *id);
    let body = b"the report the takeover turn must not lose".to_vec();
    TEST_DOWNLOADS.with(|served| served.borrow_mut().insert(URL.to_string(), body.clone()));

    let unreachable = "https://files.invalid/report-2.txt";
    let partial = gateway_message(4_370_041, &[("report.txt", URL), ("two.txt", unreachable)]);
    assert!(!through_gateway(&ctx, &data, partial).await, "partial");
    let queue_file = crate::services::discord::runtime_store::discord_pending_queue_root()
        .expect("queue root")
        .join("codex")
        .join(&shared.token_hash)
        .join(format!("{CHANNEL}.json"));
    std::fs::create_dir_all(&queue_file).unwrap();
    let unwritten = gateway_message(4_370_042, &[("report.txt", URL)]);
    assert!(!through_gateway(&ctx, &data, unwritten).await, "unwritten");
    assert_eq!((checkpoint(), queued_ids(&shared).await), (None, vec![]));

    std::fs::remove_dir(&queue_file).unwrap();
    let held = gateway_message(4_370_043, &[("report.txt", URL)]);
    assert!(through_gateway(&ctx, &data, held).await, "held");
    let mailbox = crate::services::discord::mailbox_snapshot(&shared, channel).await;
    let queue = mailbox.intervention_queue;
    assert_eq!(queued_ids(&shared).await, [4_370_043]);
    assert_eq!(
        queue[0].pending_uploads.len(),
        1,
        "the file rides the queue"
    );
    assert_eq!(checkpoint(), Some(4_370_043), "the held input moves it");
    let saved = crate::services::discord::runtime_store::last_message_root().unwrap();
    let saved = std::fs::read_to_string(saved.join("codex").join(format!("{CHANNEL}.txt")));
    assert_eq!(
        saved.unwrap().trim(),
        "4370043",
        "the checkpoint is persisted"
    );
    server.abort();
    pool.close().await;
    fixture.drop().await;
}
