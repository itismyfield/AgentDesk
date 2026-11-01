use super::LegacyFixture;
use crate::services::discord::health::{
    HealthRegistry, HumanInputDelivery, HumanInputRequest, deliver_human_input,
    external_turn_hold_for_start, start_without_gateway,
};
use crate::services::discord::router::message_handler::{
    INPUT_EFFECT_PROBE, InputEffectProbe, input_result_for_tests,
};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE;
use crate::services::discord::turn_presence::activity::{Activity, Observed};
use crate::services::discord::{self, Data, mailbox_snapshot, router};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{Intervention, InterventionMode};
use poise::serenity_prelude::{self as serenity, ChannelId, MessageId};
use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;

/// The real human-input entry must claim an empty mailbox instead of parking behind stale rows.
#[tokio::test(flavor = "current_thread")]
async fn legacy_divergence_human_input_starts_instead_of_queueing_pg() {
    let fixture = LegacyFixture::connect_and_migrate(69_193_021).await;
    let rig = &fixture.rig;
    let channel = rig.channel.get();
    let _confirmed = crate::services::tui_o::turn_mode::TestConfirmation::new(channel);
    {
        let mut settings = rig.shared.settings.write().await;
        settings.owner_user_id = Some(100);
        settings.allowed_user_ids = vec![200];
        settings.allowed_channel_ids = vec![channel];
    }
    assert_eq!(fixture.observation().await, idle());
    let registry = HealthRegistry::new();
    registry.register("claude".into(), rig.shared.clone()).await;
    let started = start_without_gateway(channel);
    assert_eq!(
        external_turn_hold_for_start(Some(&registry), &ProviderKind::Claude, channel).await,
        None
    );
    let request = || HumanInputRequest {
        channel_id: rig.channel,
        provider: ProviderKind::Claude,
        text: "new input after clear".into(),
        author_id: 200,
        source: "test".into(),
        metadata: Some(serde_json::json!({"silent": true})),
        channel_name_hint: None,
    };
    let first = deliver_human_input(&registry, request()).await.unwrap();
    assert!(
        matches!(first, HumanInputDelivery::Started { .. }),
        "{first:?}"
    );
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("the real start claim did not run");
    let snapshot = mailbox_snapshot(&rig.shared, rig.channel).await;
    assert!(snapshot.cancel_token.is_some());
    assert!(snapshot.intervention_queue.is_empty());
    let again = deliver_human_input(&registry, request()).await.unwrap();
    assert!(
        matches!(again, HumanInputDelivery::Queued { ref reason, .. } if reason == "turn_active"),
        "a second input bypassed the claimed turn: {again:?}"
    );
    eprintln!("LEGACY_DIVERGENCE_HUMAN_INPUT channel={channel} claimed=1 unknown_queued=0 skip=0");
}

fn idle() -> Observed {
    Observed {
        activity: Activity::Idle,
        reason: "no_session",
    }
}

struct Transport {
    ctx: serenity::Context,
    mock: discord_mock::DiscordMockState,
    server: tokio::task::JoinHandle<()>,
}

impl Transport {
    async fn new(channel: u64) -> Self {
        let mock = discord_mock::DiscordMockState::new();
        mock.allow_channel(channel);
        mock.park_first_placeholder.store(false, Ordering::SeqCst);
        let (proxy, gateway, server) = discord_mock::start(mock.clone()).await;
        let ctx = discord_mock::serenity_context(proxy, gateway).await;
        Self { ctx, mock, server }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.server.abort();
    }
}

struct ProviderProbe {
    entered: tokio::sync::oneshot::Receiver<std::thread::ThreadId>,
    release: std::sync::mpsc::Sender<()>,
    completed: tokio::sync::oneshot::Receiver<()>,
}

impl ProviderProbe {
    fn arm(channel: ChannelId) -> Self {
        let (entered, at_provider) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        assert!(
            INPUT_EFFECT_PROBE
                .lock()
                .unwrap()
                .replace(InputEffectProbe {
                    channel: channel.get(),
                    entered,
                    release: wait,
                    terminal_before_release: false,
                })
                .is_none()
        );
        let (completed, at_completion) = tokio::sync::oneshot::channel();
        assert!(
            BRIDGE_COMPLETION_PROBE
                .lock()
                .unwrap()
                .replace((channel, completed))
                .is_none()
        );
        Self {
            entered: at_provider,
            release,
            completed: at_completion,
        }
    }

    async fn reached(&mut self) {
        let worker = tokio::time::timeout(Duration::from_secs(15), &mut self.entered)
            .await
            .expect("the queued input never reached the selected provider")
            .unwrap();
        assert_ne!(worker, std::thread::current().id());
    }

    async fn finish(self) {
        self.release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(15), self.completed)
            .await
            .expect("the selected provider turn did not finish its bridge")
            .unwrap();
    }
}

/// FullEvent intake preserves FIFO, then the production automatic dequeue starts both the old
/// backlog and the fresh Discord input on their selected provider workers.
#[tokio::test(flavor = "current_thread")]
async fn legacy_divergence_discord_input_behind_backlog_reaches_provider_pg() {
    let test = concat!(
        module_path!(),
        "::legacy_divergence_discord_input_behind_backlog_reaches_provider_pg"
    );
    if !crate::services::tui_o::cutover::test_override::in_empty_list_process(test) {
        return;
    }
    tracing_subscriber::fmt()
        .with_env_filter("agentdesk::services::discord=debug,agentdesk::services::cluster=info")
        .with_test_writer()
        .try_init()
        .expect("isolated entry subscriber");
    let api = discord::admin_host_guard::tests::Recorder::start().await;
    discord::internal_api::init(api.port, None);
    let mut fixture = LegacyFixture::connect_and_migrate(69_193_022).await;
    Arc::get_mut(&mut fixture.rig.shared)
        .expect("the fixture still owns its runtime exclusively")
        .api_port = api.port;
    let rig = &fixture.rig;
    let channel = rig.channel;
    let _confirmed = crate::services::tui_o::turn_mode::TestConfirmation::new(channel.get());
    let transport = Transport::new(channel.get()).await;
    rig.shared
        .http
        .cached_serenity_ctx
        .set(transport.ctx.clone())
        .unwrap();
    rig.shared
        .http
        .cached_bot_token
        .set("test-token".into())
        .unwrap();
    let old_id = MessageId::new(940_721_900_000_001);
    let new_id = MessageId::new(old_id.get() + 1);
    let mut message = discord_mock::user_message(new_id.get(), "fresh Discord input after clear");
    message.channel_id = channel;
    {
        let mut settings = rig.shared.settings.write().await;
        settings.owner_user_id = Some(message.author.id.get());
        settings.allow_all_users = true;
        settings.allowed_channel_ids = vec![channel.get()];
    }
    let workspace = rig.root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    {
        let mut core = rig.shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        session.current_path = Some(workspace.display().to_string());
        session.cleared = true;
    }
    rig.shared
        .restart
        .reconcile_done
        .store(true, Ordering::Release);
    assert!(
        discord::provider_handles_channel(
            &transport.ctx,
            &ProviderKind::Claude,
            &rig.shared.settings.read().await.clone(),
            channel,
        )
        .await,
        "fixture must pass the real provider-routing gate"
    );
    assert!(
        discord::check_auth(
            message.author.id,
            &message.author.name,
            &rig.shared,
            "test-token"
        )
        .await,
        "fixture must admit the real Discord author"
    );
    assert_eq!(fixture.observation().await, idle());
    let queued = Intervention {
        author_id: message.author.id,
        author_is_bot: false,
        message_id: old_id,
        queued_generation: discord::runtime_store::process_generation(),
        source_message_ids: vec![old_id],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "older input held before clear recovery".into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    };
    let enqueued =
        discord::mailbox_enqueue_intervention(&rig.shared, &ProviderKind::Claude, channel, queued)
            .await;
    assert!(enqueued.enqueued);
    let _first_result = input_result_for_tests((channel.get(), "backlog completed".into()));
    let mut first = ProviderProbe::arm(channel);
    let data = Data {
        shared: rig.shared.clone(),
        token: "test-token".into(),
        provider: ProviderKind::Claude,
    };
    let event = serenity::FullEvent::Message {
        new_message: message,
    };
    tokio::time::timeout(
        Duration::from_secs(15),
        Box::pin(router::handle_event(&transport.ctx, &event, &data)),
    )
    .await
    .expect("the real Discord input did not finish admission")
    .unwrap();
    let snapshot = mailbox_snapshot(&rig.shared, channel).await;
    let queued: Vec<_> = snapshot
        .intervention_queue
        .iter()
        .map(|input| (input.message_id.get(), input.source_message_ids.clone()))
        .collect();
    eprintln!(
        "LEGACY_DIVERGENCE_ADMITTED active={:?} pending={:?} token={} queued={queued:?} activity={:?} checkpoint={:?} discord={:?} unhandled={:?} api={:?}",
        snapshot.active_user_message_id,
        snapshot.pending_user_dispatch,
        snapshot.cancel_token.is_some(),
        fixture.observation().await,
        rig.shared
            .last_message_ids
            .get(&channel)
            .map(|entry| *entry),
        transport.mock.written_texts(),
        transport.mock.unhandled.lock().unwrap(),
        api.take(),
    );
    first.reached().await;
    let snapshot = mailbox_snapshot(&rig.shared, channel).await;
    assert_eq!(snapshot.active_user_message_id, Some(old_id));
    assert_eq!(
        snapshot
            .intervention_queue
            .iter()
            .map(|input| input.message_id)
            .collect::<Vec<_>>(),
        [new_id],
        "the fresh input must wait behind the already-started backlog"
    );

    let mut second = ProviderProbe::arm(channel);
    // The first bridge waits for its chained turn, so let both completions make progress.
    let finishing_first = tokio::spawn(first.finish());
    second.reached().await;
    let snapshot = mailbox_snapshot(&rig.shared, channel).await;
    assert_eq!(snapshot.active_user_message_id, Some(new_id));
    assert!(snapshot.intervention_queue.is_empty());
    let _second_result = input_result_for_tests((channel.get(), "fresh input completed".into()));
    second.finish().await;
    finishing_first.await.unwrap();
    assert!(
        mailbox_snapshot(&rig.shared, channel)
            .await
            .cancel_token
            .is_none()
    );
    assert!(transport.mock.unhandled.lock().unwrap().is_empty());
    eprintln!("LEGACY_DIVERGENCE_DISCORD_ENTRY channel={channel} provider_turns=2 fifo=1 skip=0");
}
