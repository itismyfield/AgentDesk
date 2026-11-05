//! A start whose text names a cancelled dispatch, through the real worker entry on PostgreSQL: the
//! turn-start guard drops an unpreserved input there, and lets a preserved one run to its answer.

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::admin_host_guard::tests::api_child;
use crate::services::discord::health::test_support::{MockDiscord, message_json};
use crate::services::discord::{inflight, runtime_store};
use axum::http::Method;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const DISPATCH: &str = "dispatch-guard-cancelled";
const ANSWER: &str = "answer from the preserved turn";
const TS: &str = "2026-11-05T00:00:00+00:00";
const WAIT: Duration = Duration::from_secs(30);

fn child(test: &str) -> bool {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    api_child(test.strip_prefix("agentdesk::").unwrap_or(test))
}

/// Claude pipe channels on a migrated database holding one cancelled dispatch.
struct Host {
    discord: MockDiscord,
    shared: Arc<SharedData>,
    pool: sqlx::PgPool,
    database: TestPostgresDb,
    _workspace: tempfile::TempDir,
    _env: Vec<crate::config::TestEnvVarGuard>,
    _rest: crate::services::discord::shared_state::test_rest::Guard,
    _root: crate::config::TestRuntimeRootGuard,
}

impl Host {
    async fn new(channels: &[ChannelId]) -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let runtime_root = runtime_store::agentdesk_root().unwrap();
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
        // The test-env lock the root guard holds comes before the database lifecycle lock.
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        sqlx::query("INSERT INTO task_dispatches (id, status) VALUES ($1, 'cancelled')")
            .bind(DISPATCH)
            .execute(&pool)
            .await
            .unwrap();
        crate::services::tui_o::channel_policy::install(&boot).unwrap();
        crate::services::provider_hosting::install_provider_hosting_config(&boot);
        let next = AtomicU64::new(1_479_734_480_200_001);
        let discord = MockDiscord::start_with(Arc::new(move |method: &Method, path: &str| {
            // No card owns the dispatch and no thread can be opened, so the turn stays on its channel.
            if path.starts_with("/api/internal/") {
                return Some((404, serde_json::json!({"error": "not found"})));
            }
            if path.ends_with("/threads") {
                let refused = serde_json::json!({"message": "Missing Permissions", "code": 50013});
                return Some((403, refused));
            }
            let rest = path.strip_prefix("/api/v10/channels/")?;
            let (channel, rest) = rest.split_once('/')?;
            let channel: u64 = channel.parse().ok()?;
            if *method == Method::POST && rest == "messages" {
                let id = next.fetch_add(1, Ordering::SeqCst);
                return Some((200, message_json(id, channel, 1, "", TS)));
            }
            let id: u64 = rest.strip_prefix("messages/")?.parse().ok()?;
            (*method == Method::PATCH).then(|| (200, message_json(id, channel, 1, "", TS)))
        }))
        .await;
        crate::services::discord::internal_api::init(discord.port, None);
        let mut shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let own = Arc::get_mut(&mut shared).unwrap();
        own.provider = ProviderKind::Claude;
        own.api_port = discord.port;
        shared.settings.write().await.provider = ProviderKind::Claude;
        // Reactions go out over HTTP to the mock instead of the in-memory test log.
        shared.turn_view_reconciler.enable_test_http_reactions();
        let rest = crate::services::discord::shared_state::test_rest::install(discord.http.clone());
        let workspace = tempfile::tempdir().unwrap();
        for &channel in channels {
            crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, "")
                .await;
            let mut core = shared.core.lock().await;
            let session = core.sessions.get_mut(&channel).unwrap();
            session.channel_name = Some(format!("dispatch-guard-{channel}"));
            session.current_path = Some(workspace.path().display().to_string());
        }
        Self {
            discord,
            shared,
            pool,
            database,
            _workspace: workspace,
            _env: env,
            _rest: rest,
            _root: root,
        }
    }

    /// The claimed request through the real worker entry, as a router hands it over; its result
    /// tells a worker whether the input is settled.
    async fn start(
        &self,
        channel: ChannelId,
        message: MessageId,
        preserve_on_cancel: bool,
    ) -> IntakeExecution {
        let request = IntakeRequest {
            intake_outbox_id: None,
            channel_id: channel,
            user_msg_id: message,
            source_message_ids: vec![message],
            busy_followup_retry_user_msg_id: message,
            request_owner: UserId::new(7),
            request_owner_name: "owner".into(),
            user_text: format!("DISPATCH:{DISPATCH} keep this instruction"),
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
        tokio::time::timeout(WAIT, entry)
            .await
            .expect("intake returns")
            .expect("the start returns without an error")
    }

    /// Requests that put the cancelled-exit reaction on `message`.
    fn exit_reactions(&self, channel: ChannelId, message: MessageId) -> usize {
        let target = format!("/messages/{message}/reactions/");
        self.discord
            .requests_for(channel.get())
            .iter()
            .filter(|request| request.starts_with("PUT ") && request.contains(&target))
            .filter(|request| request.contains("%F0%9F%9A%AB") || request.contains('🚫'))
            .count()
    }

    /// Message bodies sent to `channel`, by POST or PATCH.
    fn sent_contents(&self, channel: ChannelId) -> Vec<String> {
        self.discord
            .requests_for(channel.get())
            .iter()
            .filter(|request| request.starts_with("POST ") || request.starts_with("PATCH "))
            .filter_map(|request| {
                let body = request.splitn(3, ' ').nth(2)?;
                let body: serde_json::Value = serde_json::from_str(body).ok()?;
                body["content"].as_str().map(str::to_owned)
            })
            .collect()
    }

    async fn queued(&self, channel: ChannelId) -> usize {
        crate::services::discord::mailbox_snapshot(&self.shared, channel)
            .await
            .intervention_queue
            .len()
    }

    async fn dispatch_status(&self) -> String {
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind(DISPATCH)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

/// The checkpoint a restart resumes from for `channel`.
fn durable_checkpoint(channel: ChannelId) -> Option<u64> {
    let path = runtime_store::last_message_root()?
        .join(ProviderKind::Claude.as_str())
        .join(format!("{channel}.txt"));
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// An unpreserved start ends at the guard; a preserved one runs to its answer.
async fn dispatch_guard_rows() {
    let (dropped, kept) = (
        ChannelId::new(1_479_734_480_100_000),
        ChannelId::new(1_479_734_480_110_000),
    );
    let host = Host::new(&[dropped, kept]).await;

    // A start that got past the guard is refused before any real provider runs.
    *super::super::super::provider_dispatch::COUNTED_DISPATCHES
        .lock()
        .unwrap() = Some((dropped.get(), 0));
    let message = MessageId::new(dropped.get() + 1);
    assert_eq!(
        host.start(dropped, message, false).await,
        IntakeExecution::Executed,
        "a start dropped at the guard is settled, not held for replay"
    );
    assert_eq!(
        host.exit_reactions(dropped, message),
        1,
        "the guard read the cancelled dispatch and marked the input with its exit"
    );
    assert!(
        host.sent_contents(dropped).is_empty(),
        "nothing past the guard posted: {:?}",
        host.sent_contents(dropped)
    );
    assert!(
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, dropped.get()).is_none(),
        "no turn row was written for the dropped input"
    );
    assert_eq!(durable_checkpoint(dropped), Some(message.get()));
    assert_eq!(host.queued(dropped).await, 0, "the input is not requeued");
    assert!(
        crate::services::discord::mailbox_snapshot(&host.shared, dropped)
            .await
            .cancel_token
            .is_none(),
        "the channel is free again"
    );
    assert_eq!(host.dispatch_status().await, "cancelled");

    let message = MessageId::new(kept.get() + 1);
    let _answer =
        super::super::super::provider_dispatch::tests::set_result(kept.get(), ANSWER.to_string());
    let (entered, release) = super::super::super::hold_input_effect_for_tests(kept.get());
    let (completed, bridge_completed) = tokio::sync::oneshot::channel();
    *crate::services::discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
        .lock()
        .unwrap() = Some((kept, completed));
    assert_eq!(
        host.start(kept, message, true).await,
        IntakeExecution::Executed,
        "the preserved start ran"
    );
    assert_eq!(
        host.exit_reactions(kept, message),
        0,
        "the guard let the preserved input through"
    );
    tokio::time::timeout(WAIT, entered)
        .await
        .expect("the preserved start reaches the provider")
        .unwrap();
    release.send(()).unwrap();
    tokio::time::timeout(WAIT, bridge_completed)
        .await
        .expect("the preserved turn completes")
        .unwrap();
    assert!(
        host.sent_contents(kept)
            .iter()
            .any(|content| content.contains(ANSWER)),
        "the preserved input's answer reached Discord: {:?}",
        host.sent_contents(kept)
    );

    for channel in [dropped, kept] {
        host.shared.mailboxes.remove_fixture_for_test(channel);
    }
    let Host { pool, database, .. } = host;
    pool.close().await;
    database.drop().await;
}

#[tokio::test]
async fn a_cancelled_dispatch_drops_an_unpreserved_start_and_runs_a_preserved_one_pg() {
    let name = concat!(
        module_path!(),
        "::a_cancelled_dispatch_drops_an_unpreserved_start_and_runs_a_preserved_one_pg"
    );
    if !child(name) {
        return;
    }
    dispatch_guard_rows().await;
}
