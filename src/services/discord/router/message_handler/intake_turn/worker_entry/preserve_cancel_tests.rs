//! A claimed intake's preservation decision reaches the turn it executes: a start called off after
//! it claimed the channel hands its input back to the queue still marked to outlive a cancel.

use super::*;
use crate::services::discord::admin_host_guard::tests::api_child;
use crate::services::discord::health::test_support::{MockDiscord, message_json};
use crate::services::discord::inflight;
use axum::http::Method;
use std::sync::atomic::{AtomicU64, Ordering};

const TS: &str = "2026-11-05T00:00:00+00:00";

fn child(test: &str) -> bool {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    api_child(test.strip_prefix("agentdesk::").unwrap_or(test))
}

/// What the queue holds for `message` after its start was called off: in memory, and as a
/// restart reads it back from disk. `None` when the input is gone.
struct Handback {
    live: Option<bool>,
    persisted: Option<bool>,
}

/// Claude pipe channels whose durable inflight row belongs to an external turn, so a start there
/// claims the channel, is refused by that row, and hands its input back.
struct Host {
    discord: MockDiscord,
    shared: Arc<SharedData>,
    _workspace: tempfile::TempDir,
    _env: Vec<crate::config::TestEnvVarGuard>,
    _root: crate::config::TestRuntimeRootGuard,
}

impl Host {
    async fn new(channels: &[ChannelId]) -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let runtime_root = crate::services::discord::runtime_store::agentdesk_root().unwrap();
        let config_path = crate::runtime_layout::config_file_path(&runtime_root);
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let body =
            "server: {}\ndata: {dir: data}\nagents: []\nproviders: {claude: {runtime: pipe}}\n";
        std::fs::write(&config_path, body).unwrap();
        let set = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock;
        let env = vec![
            set("AGENTDESK_CONFIG", config_path.as_os_str()),
            set("ADK_INTAKE_ROUTING_MODE", "disabled".as_ref()),
            set("ADK_TEST_CODEX_PROMOTE_READY", "1".as_ref()),
        ];
        let boot = serde_json::from_value(serde_json::json!({"server": {}, "agents": [],
            "providers": {"claude": {"runtime": "pipe"}}}))
        .unwrap();
        crate::services::tui_o::channel_policy::install(&boot).unwrap();
        crate::services::provider_hosting::install_provider_hosting_config(&boot);
        let next = AtomicU64::new(980_001);
        let discord = MockDiscord::start_with(Arc::new(move |method: &Method, path: &str| {
            let channel: u64 = path
                .strip_prefix("/api/v10/channels/")?
                .strip_suffix("/messages")?
                .parse()
                .ok()?;
            (*method == Method::POST).then(|| {
                let id = next.fetch_add(1, Ordering::SeqCst);
                (200, message_json(id, channel, 1, "", TS))
            })
        }))
        .await;
        crate::services::discord::internal_api::init(discord.port, None);
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        let own = Arc::get_mut(&mut shared).unwrap();
        own.provider = ProviderKind::Claude;
        own.api_port = discord.port;
        shared.settings.write().await.provider = ProviderKind::Claude;
        let workspace = tempfile::tempdir().unwrap();
        for &channel in channels {
            crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, "")
                .await;
            let mut core = shared.core.lock().await;
            let session = core.sessions.get_mut(&channel).unwrap();
            session.channel_name = Some(format!("preserve-cancel-{channel}"));
            session.current_path = Some(workspace.path().display().to_string());
            drop(core);
            let mut external = inflight::InflightTurnState::new(
                ProviderKind::Claude,
                channel.get(),
                None,
                0,
                0,
                channel.get() + 90,
                "subagent report".to_string(),
                None,
                None,
                None,
                None,
                0,
            );
            external.turn_source = inflight::TurnSource::ExternalInput;
            inflight::save_inflight_state_create_new(&external).expect("external row");
        }
        Self {
            discord,
            shared,
            _workspace: workspace,
            _env: env,
            _root: root,
        }
    }

    /// The claimed request through the real worker entry, then what its queue keeps.
    async fn refused_start(&self, channel: ChannelId, preserve_on_cancel: bool) -> Handback {
        let message = MessageId::new(channel.get() + 1);
        let request = IntakeRequest {
            intake_outbox_id: None,
            channel_id: channel,
            user_msg_id: message,
            source_message_ids: vec![message],
            busy_followup_retry_user_msg_id: message,
            request_owner: UserId::new(7),
            request_owner_name: "owner".into(),
            user_text: "keep this instruction".into(),
            reply_to_user_message: false,
            defer_watcher_resume: false,
            wait_for_completion: false,
            merge_consecutive: false,
            reply_context: None,
            has_reply_boundary: false,
            dm_hint: Some(false),
            turn_kind: TurnKind::Foreground,
            preserve_on_cancel,
        };
        let entry =
            execute_intake_turn_core(&self.discord.http, &self.shared, "", request, Vec::new());
        tokio::time::timeout(std::time::Duration::from_secs(30), entry)
            .await
            .expect("intake returns")
            .expect("a called-off start keeps its input instead of failing");
        let row = inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get());
        assert!(
            row.is_some_and(|row| row.turn_source == inflight::TurnSource::ExternalInput),
            "the start was called off by the external row"
        );
        let marked = |queue: &[crate::services::turn_orchestrator::Intervention]| {
            let [item] = queue else {
                panic!("the input is queued once: {queue:?}")
            };
            assert_eq!(item.message_id, message);
            Some(item.preserve_on_cancel())
        };
        let live = crate::services::discord::mailbox_snapshot(&self.shared, channel)
            .await
            .intervention_queue;
        let (disk, _) = crate::services::turn_orchestrator::load_pending_queues(
            &ProviderKind::Claude,
            &self.shared.token_hash,
        );
        Handback {
            live: marked(&live),
            persisted: disk.get(&channel).and_then(|queue| marked(queue)),
        }
    }
}

async fn preserved_and_unpreserved_handbacks() {
    let (kept, plain) = (ChannelId::new(7_344_701), ChannelId::new(7_344_711));
    let host = Host::new(&[kept, plain]).await;
    let preserved = host.refused_start(kept, true).await;
    assert_eq!(
        (preserved.live, preserved.persisted),
        (Some(true), Some(true)),
        "a preserved input stays marked to outlive a cancel, in memory and on disk"
    );
    let unpreserved = host.refused_start(plain, false).await;
    assert_eq!(
        (unpreserved.live, unpreserved.persisted),
        (Some(false), Some(false)),
        "an unpreserved input keeps the plain cancel behavior"
    );
    for channel in [kept, plain] {
        host.shared.mailboxes.remove_fixture_for_test(channel);
    }
}

#[tokio::test]
async fn a_called_off_start_requeues_its_input_with_the_preservation_it_was_claimed_with() {
    let name = concat!(
        module_path!(),
        "::a_called_off_start_requeues_its_input_with_the_preservation_it_was_claimed_with"
    );
    if !child(name) {
        return;
    }
    preserved_and_unpreserved_handbacks().await;
}
