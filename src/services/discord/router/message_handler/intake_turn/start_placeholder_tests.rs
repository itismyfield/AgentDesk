//! Refused starts through the real intake and kickoff entries leave none of their own placeholders;
//! a failed delete is retried by the runtime's consumer and the admitted start ends in one answer.

use super::super::super::provider_dispatch::{COUNTED_DISPATCHES, INPUT_PROMPT_PROBE};
use super::*;
use crate::services::discord::admin_host_guard::tests::api_child;
use crate::services::discord::health::test_support::{MockDiscord, message_json};
use crate::services::discord::{
    busy_followup_retry_store, inflight, queue_dispatch, status_panel_orphan_store,
};
use axum::http::Method;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const FIRST_POSTED: u64 = 950_001;
const ANSWER: &str = "pong from the admitted turn";
const TS: &str = "2026-10-28T00:00:00+00:00";

/// A Discord stand-in whose message POSTs get distinct ids and whose DELETEs can be made to fail.
struct Discord {
    mock: MockDiscord,
    channel: ChannelId,
    minted: Arc<Mutex<Vec<u64>>>,
    failing_deletes: Arc<AtomicBool>,
    seeded: BTreeMap<u64, String>,
}

impl Discord {
    async fn start(channel: ChannelId, seeded: BTreeMap<u64, String>) -> Self {
        let minted: Arc<Mutex<Vec<u64>>> = Arc::default();
        let failing_deletes = Arc::new(AtomicBool::new(false));
        let (mint, failing) = (minted.clone(), failing_deletes.clone());
        let next = AtomicU64::new(FIRST_POSTED);
        let messages = format!("/api/v10/channels/{channel}/messages");
        let mock = MockDiscord::start_with(Arc::new(move |method: &Method, path: &str| {
            if *method == Method::POST && path == messages {
                let id = next.fetch_add(1, Ordering::SeqCst);
                mint.lock().unwrap().push(id);
                return Some((200, message_json(id, channel.get(), 1, "", TS)));
            }
            let id: u64 = path
                .strip_prefix(messages.as_str())?
                .strip_prefix('/')?
                .parse()
                .ok()?;
            match *method {
                Method::DELETE if failing.load(Ordering::SeqCst) => Some((
                    500,
                    serde_json::json!({"message": "injected outage", "code": 0}),
                )),
                Method::PATCH => Some((200, message_json(id, channel.get(), 1, "", TS))),
                _ => None,
            }
        }))
        .await;
        Self {
            mock,
            channel,
            minted,
            failing_deletes,
            seeded,
        }
    }

    fn messages_path(&self) -> String {
        format!("/api/v10/channels/{}/messages", self.channel)
    }

    /// The channel as shown now: seeded messages, then every successful POST, PATCH and DELETE.
    fn messages(&self) -> BTreeMap<u64, String> {
        let mut shown = self.seeded.clone();
        let mut minted = self.minted.lock().unwrap().clone().into_iter();
        let prefix = self.messages_path();
        for request in self.mock.successful_requests_for(self.channel.get()) {
            let (method, rest) = request.split_once(' ').unwrap();
            let (path, body) = rest.split_once(' ').unwrap_or((rest, ""));
            let content = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|body| body["content"].as_str().map(str::to_owned));
            if method == "POST" && path == prefix {
                shown.insert(minted.next().unwrap(), content.unwrap_or_default());
                continue;
            }
            let Some(id) = path
                .strip_prefix(prefix.as_str())
                .and_then(|id| id.strip_prefix('/')?.parse::<u64>().ok())
            else {
                continue;
            };
            match method {
                "PATCH" => {
                    if let (Some(text), Some(slot)) = (content, shown.get_mut(&id)) {
                        *slot = text;
                    }
                }
                "DELETE" => {
                    shown.remove(&id);
                }
                _ => {}
            }
        }
        shown
    }

    /// Shown messages still carrying the processing spinner.
    fn placeholders(&self) -> Vec<u64> {
        let shown = self.messages();
        shown
            .into_iter()
            .filter(|(_, text)| text.contains('⠋'))
            .map(|(id, _)| id)
            .collect()
    }

    /// Message ids a DELETE removed, in order.
    fn deleted(&self) -> Vec<u64> {
        let prefix = format!("DELETE {}/", self.messages_path());
        self.mock
            .successful_requests_for(self.channel.get())
            .iter()
            .filter_map(|request| request.strip_prefix(&prefix)?.trim().parse().ok())
            .collect()
    }

    /// Requests of `method` that named message `id`, whatever their answer.
    fn attempts(&self, method: &str, id: u64) -> usize {
        let target = format!("{method} {}/{id} ", self.messages_path());
        self.mock
            .requests_for(self.channel.get())
            .iter()
            .filter(|request| request.starts_with(&target))
            .count()
    }

    fn posted(&self) -> usize {
        self.minted.lock().unwrap().len()
    }

    /// The content of every message POST, as sent.
    fn posted_contents(&self) -> Vec<String> {
        let prefix = format!("POST {} ", self.messages_path());
        self.mock
            .successful_requests_for(self.channel.get())
            .iter()
            .filter_map(|request| request.strip_prefix(&prefix))
            .map(|body| {
                serde_json::from_str::<serde_json::Value>(body).unwrap_or_default()["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    }
}

struct Host {
    discord: Discord,
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel: ChannelId,
    blocker_anchor: u64,
    _workspace: tempfile::TempDir,
    _routing: crate::config::TestEnvVarGuard,
    _ready: crate::config::TestEnvVarGuard,
    _config: crate::config::TestEnvVarGuard,
    _root: crate::config::TestRuntimeRootGuard,
}

/// A Claude pipe channel whose durable row, anchored on its own message, belongs to another turn.
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
    let blocker_anchor = channel + 90;
    let channel = ChannelId::new(channel);
    let seeded = BTreeMap::from([(blocker_anchor, "blocker turn".to_string())]);
    let discord = Discord::start(channel, seeded).await;
    crate::services::discord::internal_api::init(discord.mock.port, None);
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    let own = Arc::get_mut(&mut shared).unwrap();
    own.provider = ProviderKind::Claude;
    own.api_port = discord.mock.port;
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
    let mut blocker = inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        0,
        0,
        blocker_anchor,
        "subagent report".to_string(),
        None,
        None,
        None,
        None,
        0,
    );
    blocker.turn_source = inflight::TurnSource::ExternalInput;
    inflight::save_inflight_state_create_new(&blocker).expect("blocker row");
    *COUNTED_DISPATCHES.lock().unwrap() = Some((channel.get(), 0));
    Host {
        discord,
        shared,
        provider,
        channel,
        blocker_anchor,
        _workspace: workspace,
        _routing: routing,
        _ready: ready,
        _config: config,
        _root: root,
    }
}

impl Host {
    /// The user's message through the real intake entry, as the local router hands it over.
    async fn intake_result(&self, message: MessageId) -> Result<IntakeExecution, Error> {
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
            &self.discord.mock.http,
            &self.shared,
            "",
            request,
            Vec::new(),
        );
        tokio::time::timeout(Duration::from_secs(30), entry)
            .await
            .expect("intake returns")
    }

    async fn intake(&self, message: MessageId) {
        self.intake_result(message)
            .await
            .expect("a refused start keeps its input instead of failing");
    }

    /// The next queued attempt through the real idle-queue kickoff. A dormant build reports a
    /// refused start as executed, so callers judge the attempt by its effects.
    async fn kickoff(&self) {
        let deps = crate::services::discord::router::IntakeDeps {
            http: &self.discord.mock.http,
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

    /// Runs `entry` with the queue save of its next handback failing once, so the requeue that
    /// a refused start asks for is refused.
    async fn with_refused_requeue<T>(&self, entry: impl std::future::Future<Output = T>) -> T {
        let channel = self.channel;
        let (reached, resume) =
            crate::services::discord::live_bridge::handback_gap::arm(channel.get());
        let fault = async {
            tokio::time::timeout(Duration::from_secs(30), reached.notified())
                .await
                .expect("the refused start reaches its handback");
            crate::services::turn_orchestrator::test_support::fail_queue_saves(channel, 1);
            resume.notify_one();
        };
        let (output, ()) = tokio::join!(entry, fault);
        assert_eq!(
            crate::services::turn_orchestrator::test_support::queue_save_faults(channel, true),
            0,
            "the requeue consumed the injected save failure"
        );
        output
    }

    async fn queued(&self) -> Vec<u64> {
        crate::services::discord::mailbox_snapshot(&self.shared, self.channel)
            .await
            .intervention_queue
            .iter()
            .map(|item| item.message_id.get())
            .collect()
    }

    fn dispatches(&self) -> usize {
        COUNTED_DISPATCHES.lock().unwrap().unwrap().1
    }

    /// The blocker row is still the external turn's, so no attempt replaced it.
    fn blocker_kept(&self) -> bool {
        inflight::load_inflight_state_read_only(&self.provider, self.channel.get()).is_some_and(
            |row| row.user_msg_id == 0 && row.turn_source == inflight::TurnSource::ExternalInput,
        )
    }

    fn orphan_queued(&self, message: u64) -> bool {
        status_panel_orphan_store::is_queued(
            &self.shared.provider,
            &self.shared.token_hash,
            self.channel.get(),
            message,
        )
    }

    /// Clears the blocker and runs the input to completion: only `anchor` spins meanwhile, then it
    /// holds the one answer (plus its footer) and a later kickoff starts nothing more.
    async fn admitted_start(&self, anchor: u64) {
        inflight::clear_inflight_state(&self.provider, self.channel.get());
        let _answer = super::super::super::provider_dispatch::tests::set_result(
            self.channel.get(),
            ANSWER.to_string(),
        );
        let (capture, submitted) = tokio::sync::oneshot::channel();
        *INPUT_PROMPT_PROBE.lock().unwrap() = Some((self.channel.get(), capture));
        let (entered, release) =
            super::super::super::hold_input_effect_for_tests(self.channel.get());
        let (completed, bridge_completed) = tokio::sync::oneshot::channel();
        *crate::services::discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
            .lock()
            .unwrap() = Some((self.channel, completed));
        self.kickoff().await;
        let wait = Duration::from_secs(30);
        tokio::time::timeout(wait, entered).await.unwrap().unwrap();
        let prompt = tokio::time::timeout(wait, submitted)
            .await
            .unwrap()
            .unwrap();
        assert!(prompt.prompt.contains("are you there?"));
        assert_eq!(
            self.discord.placeholders(),
            [anchor],
            "only the admitted start spins"
        );
        release.send(()).unwrap();
        tokio::time::timeout(wait, bridge_completed)
            .await
            .expect("the admitted turn completes")
            .unwrap();
        self.kickoff().await;
        assert_eq!(self.dispatches(), 1, "the input reached the provider once");
        assert!(
            self.queued().await.is_empty(),
            "the started input left the queue"
        );
        let shown = self.discord.messages();
        assert_eq!(
            shown.get(&self.blocker_anchor).map(String::as_str),
            Some("blocker turn")
        );
        assert!(
            shown[&anchor].contains(ANSWER) && self.discord.placeholders().is_empty(),
            "the anchor carries the answer and nothing spins: {shown:?}"
        );
        let others: Vec<&String> = shown
            .iter()
            .filter(|(id, _)| ![self.blocker_anchor, anchor].contains(id))
            .map(|(_, text)| text)
            .collect();
        assert!(
            others.len() <= 1 && others.iter().all(|text| text.starts_with("-# ")),
            "only the answer's footer follows it: {shown:?}"
        );
    }
}

/// N refusals (the first through intake, the rest through kickoff) each POST and refuse; the
/// admitted start afterwards converges to one answer on its own anchor.
async fn refused_fresh_starts(attempts: usize) {
    let host = blocked_host(7_290_100).await;
    let message = MessageId::new(7_290_101);
    host.intake(message).await;
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
        leaked.push(host.discord.placeholders().len());
    }
    assert_eq!(
        leaked,
        vec![0; attempts],
        "placeholders left after each refused attempt"
    );
    assert_eq!(
        host.dispatches(),
        0,
        "no refused attempt reached the provider"
    );
    let refused: Vec<u64> = (0..attempts as u64).map(|i| FIRST_POSTED + i).collect();
    assert_eq!(
        host.discord.deleted(),
        refused,
        "only each refused attempt's own id is deleted"
    );
    host.admitted_start(FIRST_POSTED + attempts as u64).await;
    assert_eq!(host.discord.deleted(), refused);
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
    assert_eq!(host.discord.attempts("PATCH", notice), attempts);
    assert_eq!(
        host.discord.deleted(),
        Vec::<u64>::new(),
        "the reused notice stays"
    );
    let kept = busy_followup_retry_store::load(&host.provider, host.channel.get(), message.get());
    assert_eq!(kept, Some(bound), "binding and retry budget are unchanged");
    host.shared.mailboxes.remove_fixture_for_test(host.channel);
}

/// A runner runtime (no gateway context) starts its own orphan consumer: a refused attempt's
/// delete that failed during an outage is retried once Discord recovers.
async fn runner_retries_a_failed_refusal_delete() {
    let host = blocked_host(7_290_300).await;
    let message = MessageId::new(7_290_301);
    *crate::services::discord::runtime_bootstrap::ORPHAN_RETRY_CADENCE_FOR_TESTS
        .lock()
        .unwrap() = Some((Duration::ZERO, Duration::from_millis(100)));
    {
        let _rest = crate::services::discord::shared_state::test_rest::install(
            host.discord.mock.http.clone(),
        );
        crate::services::discord::runtime_bootstrap::run_bot_spawn_execution_runtime_tasks(
            &host.shared,
            &host.provider,
        );
    }
    host.discord.failing_deletes.store(true, Ordering::SeqCst);
    host.intake(message).await;
    let refused = FIRST_POSTED;
    assert_eq!(host.discord.placeholders(), [refused], "the delete failed");
    assert!(host.discord.attempts("DELETE", refused) >= 1);
    assert!(
        host.orphan_queued(refused),
        "the failed delete is queued for retry"
    );
    host.discord.failing_deletes.store(false, Ordering::SeqCst);
    let mut retried = false;
    for _ in 0..200 {
        if !host.orphan_queued(refused) && host.discord.placeholders().is_empty() {
            retried = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        retried,
        "the runtime's consumer deleted the refused placeholder and its record"
    );
    assert_eq!(host.discord.deleted(), [refused]);
    host.admitted_start(refused + 1).await;
    assert_eq!(host.discord.deleted(), [refused]);
    host.shared.mailboxes.remove_fixture_for_test(host.channel);
}

/// A refused start whose requeue is refused too still removes its placeholder: direct intake
/// surfaces the error, and kickoff restores the dequeued input from it.
async fn refused_requeue_removes_the_placeholder() {
    let host = blocked_host(7_290_400).await;
    let message = MessageId::new(7_290_401);
    let error = host
        .with_refused_requeue(host.intake_result(message))
        .await
        .err()
        .expect("a refused requeue surfaces as an error");
    assert!(
        error.to_string().contains("retry enqueue refused"),
        "{error}"
    );
    assert!(
        host.queued().await.is_empty(),
        "the failed save rolled the requeue back"
    );
    assert_eq!(host.discord.placeholders(), Vec::<u64>::new());
    assert_eq!(host.discord.deleted(), [FIRST_POSTED]);

    let generation = crate::services::discord::runtime_store::process_generation();
    let item = crate::services::turn_orchestrator::Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: message,
        queued_generation: generation,
        source_message_ids: vec![message],
        source_message_queued_generations: vec![
            crate::services::turn_orchestrator::SourceMessageQueuedGeneration::user_instruction(
                message, generation,
            ),
        ],
        source_text_segments: Vec::new(),
        text: "are you there?".into(),
        mode: crate::services::turn_orchestrator::InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    };
    let enqueued = crate::services::discord::mailbox_enqueue_intervention(
        &host.shared,
        &host.provider,
        host.channel,
        item,
    )
    .await;
    assert!(enqueued.enqueued);
    host.with_refused_requeue(host.kickoff()).await;
    assert_eq!(host.discord.posted(), 2, "the kickoff reached its POST");
    assert_eq!(
        host.queued().await,
        [message.get()],
        "kickoff restores the dequeued input"
    );
    assert_eq!(host.discord.placeholders(), Vec::<u64>::new());
    assert_eq!(host.discord.deleted(), [FIRST_POSTED, FIRST_POSTED + 1]);
    assert_eq!(host.dispatches(), 0);
    assert!(host.blocker_kept());
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_runtime_retries_a_refused_placeholder_delete_that_failed() {
    if !child(concat!(
        "services::discord::router::message_handler::intake_turn::start_placeholder::tests::",
        "a_runner_runtime_retries_a_refused_placeholder_delete_that_failed"
    )) {
        return;
    }
    runner_retries_a_failed_refusal_delete().await;
}

#[tokio::test]
async fn a_refused_requeue_still_removes_the_placeholder_and_keeps_the_input() {
    if !child(concat!(
        "services::discord::router::message_handler::intake_turn::start_placeholder::tests::",
        "a_refused_requeue_still_removes_the_placeholder_and_keeps_the_input"
    )) {
        return;
    }
    refused_requeue_removes_the_placeholder().await;
}

/// On a turn-mode channel a Discord start shows no processing placeholder, so a refused one has
/// nothing to post or remove; off turn mode the same refusal posts its own and removes it.
async fn turn_mode_start(turn_mode: bool) {
    let host = blocked_host(7_290_400 + 10 * u64::from(turn_mode)).await;
    let _turn = turn_mode
        .then(|| crate::services::tui_o::turn_mode::TestConfirmation::new(host.channel.get()));
    let message = MessageId::new(host.channel.get() + 1);
    host.intake(message).await;
    assert_eq!(
        host.queued().await,
        [message.get()],
        "the input is kept once"
    );
    assert!(
        host.blocker_kept(),
        "the start was refused by the foreign row"
    );
    assert_eq!(host.dispatches(), 0);
    let spinners = host
        .discord
        .posted_contents()
        .iter()
        .filter(|content| content.contains('⠋'))
        .count();
    if turn_mode {
        assert_eq!(host.discord.posted(), 0, "no placeholder POST");
        assert_eq!(spinners, 0);
        assert!(host.discord.deleted().is_empty(), "nothing to remove");
    } else {
        assert_eq!((host.discord.posted(), spinners), (1, 1));
        assert_eq!(
            host.discord.deleted(),
            [FIRST_POSTED],
            "its own placeholder goes"
        );
    }
    host.shared.mailboxes.remove_fixture_for_test(host.channel);
}

#[tokio::test]
async fn a_turn_mode_discord_start_posts_no_processing_placeholder() {
    if !child(concat!(
        "services::discord::router::message_handler::intake_turn::start_placeholder::tests::",
        "a_turn_mode_discord_start_posts_no_processing_placeholder"
    )) {
        return;
    }
    turn_mode_start(true).await;
}

#[tokio::test]
async fn a_legacy_discord_start_still_posts_its_processing_placeholder() {
    if !child(concat!(
        "services::discord::router::message_handler::intake_turn::start_placeholder::tests::",
        "a_legacy_discord_start_still_posts_its_processing_placeholder"
    )) {
        return;
    }
    turn_mode_start(false).await;
}
