//! Refused starts through the real intake and kickoff entries leave no processing placeholder of
//! their own, keep a reused busy notice, and leave the admitted start its anchor.

use super::super::super::provider_dispatch::INPUT_PROMPT_PROBE;
use super::*;
use crate::services::discord::admin_host_guard::tests::{Recorder, api_child};
use crate::services::discord::{busy_followup_retry_store, inflight, queue_dispatch};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

const FIRST_POSTED: u64 = 950_001;

/// A Discord stand-in whose message POSTs get distinct ids, so each placeholder is its own message.
struct Discord {
    api: Recorder,
    channel: ChannelId,
    posted: Arc<Mutex<Vec<u64>>>,
    calls: Mutex<Vec<String>>,
}

impl Discord {
    async fn start(channel: ChannelId) -> Self {
        let posted: Arc<Mutex<Vec<u64>>> = Arc::default();
        let (minted, next) = (posted.clone(), Arc::new(AtomicU64::new(FIRST_POSTED)));
        let messages = format!("/api/v10/channels/{channel}/messages");
        let api = Recorder::start_with(Arc::new(move |method, path| {
            if method != axum::http::Method::POST || path != messages {
                return None;
            }
            let id = next.fetch_add(1, Ordering::SeqCst);
            minted.lock().unwrap().push(id);
            Some(serde_json::json!({
                "id": id.to_string(), "channel_id": channel.get().to_string(), "content": "",
                "author": {"id": "1", "username": "t", "discriminator": "0001", "avatar": null},
                "timestamp": "2026-10-28T00:00:00+00:00", "edited_timestamp": null,
                "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
                "attachments": [], "embeds": [], "pinned": false, "type": 0
            }))
        }))
        .await;
        Self {
            api,
            channel,
            posted,
            calls: Mutex::default(),
        }
    }

    fn calls(&self) -> Vec<String> {
        let mut calls = self.calls.lock().unwrap();
        calls.extend(self.api.take());
        calls.clone()
    }

    /// Message ids DELETEd in the channel, in call order.
    fn deleted(&self) -> Vec<u64> {
        let prefix = format!("DELETE /api/v10/channels/{}/messages/", self.channel);
        self.calls()
            .iter()
            .filter_map(|call| call.strip_prefix(&prefix)?.trim().parse().ok())
            .collect()
    }

    /// Posted processing placeholders (by POST order) that were never deleted.
    fn live_placeholders(&self) -> Vec<u64> {
        let messages = format!("POST /api/v10/channels/{}/messages ", self.channel);
        let posts: Vec<bool> = self
            .calls()
            .iter()
            .filter_map(|call| Some(call.strip_prefix(&messages)?.contains('⠋')))
            .collect();
        let posted = self.posted.lock().unwrap().clone();
        assert_eq!(posts.len(), posted.len(), "every POST got one minted id");
        let deleted = self.deleted();
        posted
            .into_iter()
            .zip(posts)
            .filter(|(id, placeholder)| *placeholder && !deleted.contains(id))
            .map(|(id, _)| id)
            .collect()
    }

    fn posted(&self) -> usize {
        self.posted.lock().unwrap().len()
    }
}

struct Host {
    discord: Discord,
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel: ChannelId,
    _workspace: tempfile::TempDir,
    _routing: crate::config::TestEnvVarGuard,
    _ready: crate::config::TestEnvVarGuard,
    _config: crate::config::TestEnvVarGuard,
    _root: crate::config::TestRuntimeRootGuard,
}

/// A Claude pipe channel whose durable row is held by another (external) turn.
async fn blocked_host(channel: u64) -> Host {
    let root = crate::config::TestRuntimeRootGuard::new();
    let runtime_root = crate::services::discord::runtime_store::agentdesk_root().unwrap();
    let config_path = crate::runtime_layout::config_file_path(&runtime_root);
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(
        &config_path,
        "server: {}\ndata: {dir: data}\nagents: []\nproviders: {claude: {runtime: pipe}}\n",
    )
    .unwrap();
    let config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config_path,
    );
    let routing = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_INTAKE_ROUTING_MODE",
        "disabled".as_ref(),
    );
    // No tmux host backs this fixture; the hosted-TUI probe would otherwise park every kickoff.
    let ready = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "ADK_TEST_CODEX_PROMOTE_READY",
        "1".as_ref(),
    );
    let boot = serde_json::from_value(serde_json::json!({"server": {}, "agents": [], "providers": {"claude": {"runtime": "pipe"}}})).unwrap();
    crate::services::tui_o::channel_policy::install(&boot).unwrap();
    crate::services::provider_hosting::install_provider_hosting_config(&boot);
    let channel = ChannelId::new(channel);
    let discord = Discord::start(channel).await;
    crate::services::discord::internal_api::init(discord.api.port, None);
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    let own = Arc::get_mut(&mut shared).unwrap();
    own.provider = ProviderKind::Claude;
    own.api_port = discord.api.port;
    shared.settings.write().await.provider = ProviderKind::Claude;
    let workspace = tempfile::tempdir().unwrap();
    crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, "").await;
    {
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        session.channel_name = Some(format!("start-placeholder-{channel}"));
        session.current_path = Some(workspace.path().display().to_string());
    }
    let provider = ProviderKind::Claude;
    crate::services::discord::health::seed_external_turn_row_for_tests(&provider, channel.get());
    Host {
        discord,
        shared,
        provider,
        channel,
        _workspace: workspace,
        _routing: routing,
        _ready: ready,
        _config: config,
        _root: root,
    }
}

impl Host {
    /// The user's message through the real intake entry, as the local router hands it over.
    async fn intake(&self, message: MessageId) -> IntakeExecution {
        let request = IntakeRequest {
            intake_outbox_id: None,
            channel_id: self.channel,
            user_msg_id: message,
            source_message_ids: vec![message],
            busy_followup_retry_user_msg_id: message,
            request_owner: UserId::new(7),
            request_owner_name: "owner".into(),
            user_text: "are you there?".into(),
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
        let entry = execute_intake_turn_core(
            &self.discord.api.http,
            &self.shared,
            "",
            request,
            Vec::new(),
        );
        tokio::time::timeout(Duration::from_secs(30), entry)
            .await
            .expect("intake returns")
            .expect("a refused start keeps its input instead of failing")
    }

    /// The next queued attempt through the real idle-queue kickoff. A dormant build reports a
    /// refused start as executed, so callers judge the attempt by its effects.
    async fn kickoff(&self) {
        let deps = crate::services::discord::router::IntakeDeps {
            http: &self.discord.api.http,
            cache: None,
            ctx_for_chained_dispatch: None,
            shared: &self.shared,
            token: "start-placeholder-test",
        };
        let kickoff = queue_dispatch::kickoff::kickoff_idle_queue_channel(
            &deps,
            &self.provider,
            self.channel,
        );
        tokio::time::timeout(Duration::from_secs(30), kickoff)
            .await
            .expect("kickoff returns");
    }

    async fn queued(&self) -> Vec<u64> {
        crate::services::discord::mailbox_snapshot(&self.shared, self.channel)
            .await
            .intervention_queue
            .iter()
            .map(|item| item.message_id.get())
            .collect()
    }

    /// The blocker row is still the external turn's, so no attempt replaced it.
    fn blocker_kept(&self) -> bool {
        inflight::load_inflight_state_read_only(&self.provider, self.channel.get()).is_some_and(
            |row| row.user_msg_id == 0 && row.turn_source == inflight::TurnSource::ExternalInput,
        )
    }

    /// Clears the blocker, then checks the next kickoff reaches the provider exactly once; the
    /// returned sender releases the held provider.
    async fn admitted_start(&self) -> std::sync::mpsc::Sender<()> {
        inflight::clear_inflight_state(&self.provider, self.channel.get());
        let (capture, submitted) = tokio::sync::oneshot::channel();
        *INPUT_PROMPT_PROBE.lock().unwrap() = Some((self.channel.get(), capture));
        let (entered, release) =
            super::super::super::hold_input_effect_for_tests(self.channel.get());
        self.kickoff().await;
        let wait = Duration::from_secs(30);
        tokio::time::timeout(wait, entered).await.unwrap().unwrap();
        let prompt = tokio::time::timeout(wait, submitted)
            .await
            .unwrap()
            .unwrap();
        assert!(prompt.prompt.contains("are you there?"));
        assert!(
            self.queued().await.is_empty(),
            "the started input left the queue"
        );
        release
    }
}

/// N refusals (the first through intake, the rest through kickoff) each POST and refuse; the
/// admitted start afterwards keeps its own anchor as the only live placeholder.
async fn refused_fresh_starts(attempts: usize) {
    let host = blocked_host(7_290_100).await;
    let message = MessageId::new(7_290_101);
    let (capture, mut provider_input) = tokio::sync::oneshot::channel();
    *INPUT_PROMPT_PROBE.lock().unwrap() = Some((host.channel.get(), capture));
    assert!(matches!(
        host.intake(message).await,
        IntakeExecution::Executed | IntakeExecution::Deferred
    ));
    let mut leaked = Vec::new();
    for attempt in 1..=attempts {
        if attempt > 1 {
            host.kickoff().await;
        }
        assert_eq!(
            host.discord.posted(),
            attempt,
            "attempt {attempt} reached its placeholder POST"
        );
        assert_eq!(
            host.queued().await,
            [message.get()],
            "attempt {attempt} kept the input once"
        );
        assert!(
            host.blocker_kept(),
            "attempt {attempt} was refused by the foreign row"
        );
        leaked.push(host.discord.live_placeholders().len());
    }
    assert_eq!(
        leaked,
        vec![0; attempts],
        "placeholders left after each refused attempt"
    );
    assert!(
        provider_input.try_recv().is_err(),
        "no refused attempt reached the provider"
    );
    let refused: Vec<u64> = (0..attempts as u64).map(|i| FIRST_POSTED + i).collect();
    assert_eq!(
        host.discord.deleted(),
        refused,
        "only each refused attempt's own id is deleted"
    );
    let release = host.admitted_start().await;
    assert_eq!(
        host.discord.live_placeholders(),
        [FIRST_POSTED + attempts as u64],
        "the admitted start keeps its anchor"
    );
    assert_eq!(host.discord.deleted(), refused);
    drop(release);
    host.shared.mailboxes.remove_fixture_for_test(host.channel);
}

/// A bound busy notice is edited in place on every refused attempt and never deleted or unbound.
async fn refused_reused_notice(attempts: usize) {
    let host = blocked_host(7_290_200).await;
    let message = MessageId::new(7_290_201);
    let notice = 7_290_299;
    let bound = busy_followup_retry_store::bind_notice_if_absent(
        &host.provider,
        host.channel.get(),
        message.get(),
        notice,
    )
    .unwrap();
    host.intake(message).await;
    for _ in 2..=attempts {
        host.kickoff().await;
    }
    assert_eq!(host.queued().await, [message.get()]);
    assert!(host.blocker_kept());
    assert_eq!(
        host.discord.posted(),
        0,
        "every attempt reused the bound notice"
    );
    let edits = format!(
        "PATCH /api/v10/channels/{}/messages/{notice} ",
        host.channel
    );
    let calls = host.discord.calls();
    assert_eq!(
        calls.iter().filter(|call| call.starts_with(&edits)).count(),
        attempts
    );
    assert_eq!(
        host.discord.deleted(),
        Vec::<u64>::new(),
        "the reused notice stays"
    );
    let kept = busy_followup_retry_store::load(&host.provider, host.channel.get(), message.get());
    assert_eq!(kept, Some(bound), "binding and retry budget are unchanged");
    host.shared.mailboxes.remove_fixture_for_test(host.channel);
}

fn child(test: &str) -> bool {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    api_child(test)
}

#[tokio::test]
async fn repeated_refused_starts_leave_no_placeholder_and_the_admitted_start_keeps_one() {
    if !child(concat!(
        "services::discord::router::message_handler::intake_turn::start_placeholder::tests::",
        "repeated_refused_starts_leave_no_placeholder_and_the_admitted_start_keeps_one"
    )) {
        return;
    }
    refused_fresh_starts(5).await;
}

#[tokio::test]
async fn refused_starts_keep_a_reused_busy_notice_and_its_binding() {
    if !child(concat!(
        "services::discord::router::message_handler::intake_turn::start_placeholder::tests::",
        "refused_starts_keep_a_reused_busy_notice_and_its_binding"
    )) {
        return;
    }
    refused_reused_notice(3).await;
}
