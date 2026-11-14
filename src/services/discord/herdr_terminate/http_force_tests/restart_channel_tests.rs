use super::*;
use crate::services::discord::commands::{SayProbe, restart_channel_probed_for_tests};
use crate::services::discord::commands::{
    probe_restart_disconnect_for_tests, restart_channel_for_tests,
};
use crate::services::discord::queue_io::cancel_backstop_test_support as wake_probe;
use crate::services::discord::queue_io::input_owner::restart_gate;
use crate::services::discord::queue_io::input_owner::restart_seed::{
    Admitted, Seed, SeedStatus, Target,
};
use crate::services::discord::router::message_handler::{
    install_root_recovery_launch, wake_drive_for_test,
};
use crate::services::herdr_launch::clear_epoch;
use crate::services::turn_orchestrator::restart_operation::Operation;
use poise::serenity_prelude as serenity;

const SELECTOR: &str = "019ee441-173c-735c-9a24-812800000001";
const WAIT: Duration = Duration::from_secs(10);

fn data(node: &Node) -> crate::services::discord::Data {
    crate::services::discord::Data {
        shared: node.shared.clone(),
        token: "restart-common-fixture".into(),
        provider: ProviderKind::Claude,
    }
}

async fn invoke(node: &Node, operation: u64) -> Vec<String> {
    invoke_command(node, operation, "!restart").await
}

async fn invoke_command(node: &Node, operation: u64, command: &'static str) -> Vec<String> {
    let http = Arc::new(serenity::Http::new(""));
    tokio::time::timeout(
        WAIT,
        restart_channel_for_tests(
            &http,
            &data(node),
            channel(),
            ("restart-fixture", command, operation),
        ),
    )
    .await
    .expect("the actual restart common entry completes")
    .unwrap()
}

async fn seed_selector(node: &Node) {
    sqlx::query(
        "UPDATE sessions SET claude_session_id=$2, raw_provider_session_id=$2 WHERE session_key=$1",
    )
    .bind(&node.key)
    .bind(SELECTOR)
    .execute(&node.pool)
    .await
    .unwrap();
}

async fn selectors(node: &Node) -> (Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT claude_session_id, raw_provider_session_id FROM sessions WHERE session_key=$1",
    )
    .bind(&node.key)
    .fetch_one(&node.pool)
    .await
    .unwrap()
}

async fn install_owner(node: &Node) {
    // A greeting is a queued original: it needs the process generation every boot allocates.
    assert_ne!(runtime_store::allocate_process_generation(), 0);
    let persistence = crate::services::discord::queue_persistence_context(
        &node.shared,
        &ProviderKind::Claude,
        channel(),
    );
    node.shared
        .mailbox(channel())
        .v2_ensure_channel(&persistence, channel())
        .await
        .unwrap();
}

type WakeHook = Arc<dyn Fn() + Send + Sync>;
struct Wakes {
    previous: Option<WakeHook>,
    observations: Arc<Mutex<Vec<(bool, bool)>>>,
}

impl Wakes {
    fn observe(shared: &Arc<SharedData>) -> Self {
        let observations = Arc::new(Mutex::new(Vec::new()));
        let saved = observations.clone();
        let shared = shared.clone();
        let previous = wake_probe::set_owner_wake_for_tests(
            channel(),
            Some(Arc::new(move || {
                let free = shared
                    .session_transition_lock(channel())
                    .try_lock_owned()
                    .is_ok();
                let gated = restart_gate::inflight("claude", &shared.token_hash, CHANNEL);
                saved.lock().unwrap().push((free, gated));
            })),
        );
        Self {
            previous,
            observations,
        }
    }

    async fn after_reopen(&self) {
        tokio::time::timeout(WAIT, async {
            loop {
                let reopened = self
                    .observations
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(free, gated)| *free && !gated);
                if reopened {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("an installed owner is woken after transition and restart gate reopen");
    }
}

impl Drop for Wakes {
    fn drop(&mut self) {
        wake_probe::set_owner_wake_for_tests(channel(), self.previous.take());
    }
}

const GREETING: &str = "안녕하세요. 세션 재시작 완료.";

async fn ended(node: &Node, rig: &herdr::P11TerminationRig, closes: usize, greeting: bool) {
    let effects = node.effects(rig).await;
    assert_eq!(effects.row, retired());
    assert_eq!(effects.session, disconnected());
    assert_eq!(effects.closes(), closes);
    assert_eq!(
        selectors(node).await,
        (Some(SELECTOR.into()), Some(SELECTOR.into()))
    );
    assert_eq!(
        effects.dispatches,
        dispatches("dispatched", "dispatched"),
        "these two dispatch rows are not the captured turn's dispatch"
    );
    assert!(
        effects.queued.is_empty(),
        "the legacy queue view holds nothing"
    );
    if greeting {
        assert_eq!(
            items(node).await,
            [GREETING],
            "the owner holds the greeting"
        );
    }
}

async fn replace_with_unknown_successor(node: &Node) -> Value {
    let unknown = json!({"schema":9917,"state":"bound","successor":"new-incarnation"});
    sqlx::query("UPDATE sessions SET hosted_execution=$2 WHERE session_key=$1")
        .bind(&node.key)
        .bind(&unknown)
        .execute(&node.pool)
        .await
        .unwrap();
    unknown
}

async fn raw(node: &Node) -> Value {
    sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE session_key=$1")
        .bind(&node.key)
        .fetch_one(&node.pool)
        .await
        .unwrap()
}

async fn dispatch_state(node: &Node) -> Vec<(String, String, Option<i64>)> {
    sqlx::query_as("SELECT id, status, retry_count::BIGINT FROM task_dispatches ORDER BY id")
        .fetch_all(&node.pool)
        .await
        .unwrap()
}

#[test]
fn restart_common_retires_disconnects_keeps_selector_and_wakes_owner_pg() {
    let Some((rig, runtime)) = rig_child(
        "restart_channel_tests::restart_common_retires_disconnects_keeps_selector_and_wakes_owner_pg",
    ) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, None).await;
        seed_selector(&node).await;
        install_owner(&node).await;
        node.start(51, "restart-common-turn", None).await;
        let (observed, pool, key, shared) = (
            ran(),
            node.pool.clone(),
            node.key.clone(),
            node.shared.clone(),
        );
        let seen = observed.clone();
        probe_before_postlude(Box::new(move || {
            Box::pin(async move {
                assert_eq!(
                    herdr::row(&pool, &key).await,
                    retired(),
                    "retirement precedes disconnect"
                );
                let selected: Option<String> = sqlx::query_scalar(
                    "SELECT raw_provider_session_id FROM sessions WHERE session_key=$1",
                )
                .bind(&key)
                .fetch_one(&pool)
                .await
                .unwrap();
                assert_eq!(selected.as_deref(), Some(SELECTOR));
                assert!(
                    shared
                        .session_transition_lock(channel())
                        .try_lock_owned()
                        .is_err()
                );
                seen.store(true, Ordering::SeqCst);
            })
        }));
        let wakes = Wakes::observe(&node.shared);
        let response = invoke_command(&node, 900_812_801, "/restart").await;
        assert!(
            response
                .last()
                .is_some_and(|text| text.contains("기존 세션 종료를 확인")),
            "{response:?}"
        );
        assert!(
            observed.load(Ordering::SeqCst),
            "the real termination postlude was observed"
        );
        ended(&node, &rig, 1, true).await;
        assert!(test_turn::active(&node.shared, channel()).await.is_none());
        wakes.after_reopen().await;
        drop(wakes);
        node.finish().await;
    });
}

#[test]
fn restart_common_same_operation_returns_cached_result_before_new_row_lookup_pg() {
    let Some((rig, runtime)) = rig_child(
        "restart_channel_tests::restart_common_same_operation_returns_cached_result_before_new_row_lookup_pg",
    ) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, None).await;
        seed_selector(&node).await;
        let first = invoke(&node, 900_812_802).await;
        assert!(
            first
                .last()
                .is_some_and(|text| text.contains("기존 세션 종료를 확인")),
            "{first:?}"
        );
        ended(&node, &rig, 1, false).await;
        let writes = rig.writes();
        let unknown = replace_with_unknown_successor(&node).await;
        let second = invoke(&node, 900_812_802).await;
        assert_eq!(
            second.last(),
            first.last(),
            "the same operation is answered before the now-unknown row is read"
        );
        assert_eq!(raw(&node).await, unknown);
        assert_eq!(rig.writes(), writes, "a retry never closes a successor");
        assert_eq!(
            selectors(&node).await,
            (Some(SELECTOR.into()), Some(SELECTOR.into()))
        );
        node.finish().await;
    });
}

#[test]
fn restart_common_recovers_idle_ended_execution_without_a_close_pg() {
    let Some((rig, runtime)) = rig_child(
        "restart_channel_tests::restart_common_recovers_idle_ended_execution_without_a_close_pg",
    ) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, None).await;
        seed_selector(&node).await;
        node.sql("UPDATE sessions SET status='idle', active_dispatch_id=NULL, last_heartbeat=NULL WHERE session_key=$1 AND $2::TEXT IS NULL", None).await;
        rig.restore_shell();
        install_owner(&node).await;
        let wakes = Wakes::observe(&node.shared);
        let response = invoke(&node, 900_812_803).await;
        assert!(response.last().is_some_and(|text| text.contains("기존 세션 종료를 확인")), "{response:?}");
        ended(&node, &rig, 0, true).await;
        wakes.after_reopen().await;
        drop(wakes);
        node.finish().await;
    });
}

#[test]
fn restart_common_clear_epoch_during_disconnect_forgets_selector_pg() {
    let Some((rig, runtime)) = rig_child(
        "restart_channel_tests::restart_common_clear_epoch_during_disconnect_forgets_selector_pg",
    ) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, None).await;
        seed_selector(&node).await;
        install_owner(&node).await;
        let clear = Arc::new(Mutex::new(None));
        let (saved, shared, observed) = (clear.clone(), node.shared.clone(), ran());
        let seen = observed.clone();
        probe_restart_disconnect_for_tests(Box::new(move || Box::pin(async move {
            *saved.lock().unwrap() = Some(clear_epoch::begin_clear("claude", &shared.token_hash, CHANNEL));
            seen.store(true, Ordering::SeqCst);
        })));
        let response = invoke(&node, 900_812_804).await;
        assert!(observed.load(Ordering::SeqCst), "the first epoch check preceded this clear");
        assert_eq!(herdr::row(&node.pool, &node.key).await, retired());
        assert_eq!(selectors(&node).await, (None, None),
            "the real post-disconnect epoch recheck must apply clear without another cleanup doing it");
        assert_eq!(node.effects(&rig).await.closes(), 1);
        assert!(response.last().is_some_and(|text| text.contains("겹친 비우기 요청")), "{response:?}");
        assert!(items(&node).await.is_empty(), "the overtaking clear leaves no greeting to run");
        assert_eq!(Seed::prior(&node.shared.mailbox(channel()), &operation(&node, 900_812_804)).await,
            Ok(SeedStatus::Gone), "the greeting was committed and then withdrawn, never absent");
        drop(clear.lock().unwrap().take());
        node.finish().await;
    });
}

#[test]
fn restart_common_cancelled_request_still_caches_its_completed_operation_pg() {
    let Some((rig, runtime)) = rig_child(
        "restart_channel_tests::restart_common_cancelled_request_still_caches_its_completed_operation_pg",
    ) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, None).await;
        seed_selector(&node).await;
        install_owner(&node).await;
        let wakes = Wakes::observe(&node.shared);
        let (arrived, paused) = tokio::sync::oneshot::channel();
        let (resume, released) = tokio::sync::oneshot::channel();
        let (pool, key) = (node.pool.clone(), node.key.clone());
        probe_before_postlude(Box::new(move || Box::pin(async move {
            assert_eq!(herdr::row(&pool, &key).await, retired(), "this barrier follows the real close and retirement");
            arrived.send(()).unwrap();
            tokio::time::timeout(WAIT, released).await.expect("the retired postlude is released").unwrap();
        })));
        let (shared, http) = (node.shared.clone(), Arc::new(serenity::Http::new("")));
        let request = tokio::spawn(async move {
            let data = crate::services::discord::Data { shared, token:"restart-common-fixture".into(), provider:ProviderKind::Claude };
            restart_channel_for_tests(&http, &data, channel(), ("restart-fixture", "!restart", 900_812_805)).await
        });
        tokio::time::timeout(WAIT, paused).await.expect("the actual close retired before its postlude").unwrap();
        assert_eq!(node.effects(&rig).await.closes(), 1);
        request.abort();
        assert!(tokio::time::timeout(WAIT, request).await.unwrap().unwrap_err().is_cancelled());
        resume.send(()).expect("the operation outlives its cancelled request");
        wakes.after_reopen().await;
        ended(&node, &rig, 1, true).await;
        let writes = rig.writes();
        let unknown = replace_with_unknown_successor(&node).await;
        let retry = invoke(&node, 900_812_805).await;
        assert!(retry.last().is_some_and(|text| text.contains("기존 세션 종료를 확인")),
            "request cancellation cannot replace the completed outcome with an unreadable-row refusal: {retry:?}");
        assert_eq!(rig.writes(), writes);
        assert_eq!(raw(&node).await, unknown);
        drop(wakes);
        node.finish().await;
    });
}

#[test]
fn restart_common_ends_when_captured_turn_does_not_own_the_row_dispatch_pg() {
    let Some((rig, runtime)) = rig_child(
        "restart_channel_tests::restart_common_ends_when_captured_turn_does_not_own_the_row_dispatch_pg",
    ) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        seed_selector(&node).await;
        install_owner(&node).await;
        node.start(51, "restart-non-dispatched-capture", None).await;
        let captured =
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL).unwrap();
        assert!(
            captured.dispatch_id.is_none(),
            "the captured actor owns no dispatch"
        );
        let before = node.effects(&rig).await;
        assert_eq!(
            before.session,
            active(Some(DISPATCH)),
            "the sessions row independently names a dispatch"
        );
        let dispatches_before = dispatch_state(&node).await;
        let wakes = Wakes::observe(&node.shared);
        let response = invoke(&node, 900_812_806).await;
        let after = node.effects(&rig).await;
        assert_eq!(
            after.closes(),
            1,
            "a dispatch mismatch cannot prevent exact execution termination"
        );
        assert!(
            response
                .last()
                .is_some_and(|text| text.contains("기존 세션 종료를 확인")),
            "{response:?}"
        );
        assert_eq!(after.row, retired());
        assert_eq!(
            after.session, before.session,
            "a false disconnect leaves the other dispatch's session status and binding intact"
        );
        assert_eq!(
            dispatch_state(&node).await,
            dispatches_before,
            "no dispatch status or retry count changes"
        );
        assert_eq!(
            selectors(&node).await,
            (Some(SELECTOR.into()), Some(SELECTOR.into()))
        );
        assert!(test_turn::active(&node.shared, channel()).await.is_none());
        wakes.after_reopen().await;
        drop(wakes);
        node.finish().await;
    });
}

#[test]
fn restart_common_full_queue_still_terminates_and_preserves_waiting_originals_pg() {
    let Some((rig, runtime)) = rig_child(
        "restart_channel_tests::restart_common_full_queue_still_terminates_and_preserves_waiting_originals_pg",
    ) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        use crate::services::discord::queue_io::input_owner::admission::{
            Admission, AdmissionTicket, admit,
        };
        use crate::services::turn_orchestrator::{MAX_INTERVENTIONS_PER_CHANNEL, PendingQueueItem};

        let generation = runtime_store::allocate_process_generation();
        assert_ne!(generation, 0);
        let node = Node::bind(&rig, herdr::HostedState::Bound, None).await;
        seed_selector(&node).await;
        install_owner(&node).await;
        // Observe wakes before admission; this fixture deliberately never auto-drains originals.
        let wakes = Wakes::observe(&node.shared);
        let mailbox = node.shared.mailbox(channel());
        let candidate = |index: usize| -> PendingQueueItem {
            let id = CHANNEL + u64::try_from(index).unwrap() + 1;
            serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":CHANNEL,
                "queued_generation":generation,"source_message_ids":[id],
                "has_reply_boundary":true,"merge_consecutive":false,
                "text":format!("full original {index}\n_ raw bytes + voice")}))
            .unwrap()
        };
        for index in 0..MAX_INTERVENTIONS_PER_CHANNEL {
            assert!(
                matches!(
                    admit(&mailbox, &candidate(index), AdmissionTicket::new()).await,
                    Admission::Accepted(_)
                ),
                "each original must actually enter durable custody"
            );
        }
        let overflow = candidate(MAX_INTERVENTIONS_PER_CHANNEL);
        assert!(
            matches!(admit(&mailbox, &overflow, AdmissionTicket::new()).await,
            Admission::Rejected(reason) if reason == "Full"),
            "Full is a real admission refusal"
        );
        let before = mailbox
            .v2_request(|slot| slot.as_ref().ok_or("owner missing")?.json())
            .await
            .unwrap();
        assert_eq!(
            before["items"].as_array().unwrap().len(),
            MAX_INTERVENTIONS_PER_CHANNEL
        );
        let before_bytes = serde_json::to_vec(&before["items"]).unwrap();
        let before_order: Vec<_> = before["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["message_id"].as_u64().unwrap())
            .collect();
        let persistence = crate::services::discord::queue_persistence_context(
            &node.shared,
            &ProviderKind::Claude,
            channel(),
        );
        let (path, _) =
            crate::services::turn_orchestrator::v2_queue(&persistence, channel()).unwrap();
        let disk_before: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_vec(&disk_before["items"]).unwrap(),
            before_bytes
        );
        wakes.observations.lock().unwrap().clear();

        let response = invoke(&node, 900_812_807).await;
        let effects = node.effects(&rig).await;
        assert_eq!(
            effects.closes(),
            1,
            "a full queue cannot veto exact termination"
        );
        assert_eq!(effects.row, retired());
        assert_eq!(effects.session, disconnected());
        assert_eq!(
            selectors(&node).await,
            (Some(SELECTOR.into()), Some(SELECTOR.into()))
        );
        assert!(
            response
                .last()
                .is_some_and(|text| text.contains("기존 세션 종료를 확인")),
            "{response:?}"
        );
        wakes.after_reopen().await;
        let after = mailbox
            .v2_request(|slot| slot.as_ref().ok_or("owner missing")?.json())
            .await
            .unwrap();
        // The greeting takes the queue's one spare slot in front; every original keeps its bytes.
        let waiting = |snapshot: &Value| {
            let items = snapshot["items"].as_array().unwrap();
            assert_eq!(items[0]["text"], GREETING, "the greeting runs first");
            Value::Array(items[1..].to_vec())
        };
        assert_eq!(
            serde_json::to_vec(&waiting(&after)).unwrap(),
            before_bytes,
            "restart preserves every waiting original byte and its order"
        );
        let after_order: Vec<_> = after["items"]
            .as_array()
            .unwrap()
            .iter()
            .skip(1)
            .map(|item| item["message_id"].as_u64().unwrap())
            .collect();
        assert_eq!(after_order, before_order);
        let disk_after: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_vec(&waiting(&disk_after)).unwrap(),
            before_bytes
        );
        assert!(
            response
                .last()
                .is_some_and(|text| text.contains("인사말을 대기열에 등록")),
            "{response:?}"
        );
        assert!(
            matches!(admit(&mailbox, &overflow, AdmissionTicket::new()).await,
            Admission::Rejected(reason) if reason == "Full"),
            "custody stays Full after restart"
        );
        node.finish().await;
        drop(wakes);
    });
}

async fn prior_of(node: &Node, id: u64) -> Result<SeedStatus, String> {
    Seed::prior(&node.shared.mailbox(channel()), &operation(node, id)).await
}

fn operation(node: &Node, id: u64) -> Operation {
    Operation::new("claude", &node.shared.token_hash, CHANNEL, &id.to_string()).unwrap()
}

/// The texts the owner holds queued, in order.
async fn items(node: &Node) -> Vec<String> {
    queued_texts(&node.shared.mailbox(channel())).await
}

async fn queued_texts(
    mailbox: &crate::services::turn_orchestrator::ChannelMailboxHandle,
) -> Vec<String> {
    let snapshot = mailbox
        .v2_request(|slot| slot.as_ref().ok_or("owner missing")?.json())
        .await
        .unwrap();
    (snapshot["items"].as_array().unwrap().iter())
        .map(|item| item["text"].as_str().unwrap().to_string())
        .collect()
}

/// Why the owner retained each input it could not start.
async fn retained(node: &Node) -> Vec<(String, String)> {
    let snapshot = node
        .shared
        .mailbox(channel())
        .v2_request(|slot| slot.as_ref().ok_or("owner missing")?.json())
        .await
        .unwrap();
    (snapshot["retained_inputs"].as_array().unwrap().iter())
        .map(|kept| {
            (
                kept["input"]["original"]["text"].as_str().unwrap().into(),
                kept["reason"].to_string(),
            )
        })
        .collect()
}

const WAITING: &str = "waiting original\n_ raw bytes";

async fn waiting(node: &Node) {
    use crate::services::discord::queue_io::input_owner::admission::{
        Admission, AdmissionTicket, admit,
    };
    let generation = runtime_store::process_generation();
    let original: crate::services::turn_orchestrator::PendingQueueItem =
        serde_json::from_value(json!({"author_id":7,"message_id":CHANNEL + 71,
            "channel_id":CHANNEL,"queued_generation":generation,
            "source_message_ids":[CHANNEL + 71],"has_reply_boundary":true,
            "merge_consecutive":false,"text":WAITING}))
        .unwrap();
    let mailbox = node.shared.mailbox(channel());
    let admitted = admit(&mailbox, &original, AdmissionTicket::new());
    assert!(matches!(admitted.await, Admission::Accepted(_)));
}

/// The durable record a restart leaves once its greeting is committed: nothing else survives
/// the daemon.
async fn crashed_after_commit(node: &Node, id: u64) {
    let mailbox = node.shared.mailbox(channel());
    let mut seed = Seed::of(&mailbox, &operation(node, id)).await.unwrap();
    let against = Target {
        nonce: Some(herdr::EXECUTION),
        dispatch: None,
    };
    let admitted = seed.admit(&mailbox, ("!restart", "restart-fixture"), against);
    assert!(matches!(admitted.await, Admitted::Created));
    assert_eq!(items(node).await, [GREETING]);
}

async fn gateway(node: &Node) -> impl Sized + use<> {
    use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
    let (proxy, gateway, server) = discord_mock::start(discord_mock::DiscordMockState::new()).await;
    let context = discord_mock::serenity_context(proxy, gateway).await;
    crate::services::discord::ready_gateway_for_test(&node.shared, context);
    server
}

async fn until(what: &str, mut done: impl AsyncFnMut() -> bool) {
    let waited = tokio::time::timeout(WAIT, async {
        while !done().await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    waited.await.unwrap_or_else(|_| panic!("{what}"));
}

// The greeting's commit is the acceptance: it is durable and in front before the old execution
// is closed, and a replay of the operation after the process forgot it ends nothing again.
#[test]
fn restart_common_commits_the_greeting_before_the_close_and_answers_a_replay_from_its_ticket_pg() {
    let Some((rig, runtime)) = rig_child(
        "restart_channel_tests::restart_common_commits_the_greeting_before_the_close_and_answers_a_replay_from_its_ticket_pg",
    ) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, None).await;
        seed_selector(&node).await;
        install_owner(&node).await;
        waiting(&node).await;
        node.start(51, "restart-greeting-turn", None).await;
        let (observed, mailbox, shared) =
            (ran(), node.shared.mailbox(channel()), node.shared.clone());
        let seen = observed.clone();
        probe_before_postlude(Box::new(move || {
            Box::pin(async move {
                assert_eq!(
                    queued_texts(&mailbox).await,
                    [GREETING, WAITING],
                    "the greeting was committed, ahead of waiting input, before the close"
                );
                assert!(restart_gate::inflight(
                    "claude",
                    &shared.token_hash,
                    CHANNEL
                ));
                seen.store(true, Ordering::SeqCst);
            })
        }));
        let wakes = Wakes::observe(&node.shared);
        let response = invoke(&node, 900_812_811).await;
        let last = response.last().unwrap();
        assert!(
            last.contains("기존 세션 종료를 확인") && last.contains("인사말을 대기열에 등록"),
            "{response:?}"
        );
        assert!(
            !last.contains("시작됐습니다"),
            "admission is never reported as a started greeting: {response:?}"
        );
        assert!(observed.load(Ordering::SeqCst));
        let effects = node.effects(&rig).await;
        assert_eq!((effects.closes(), effects.row.clone()), (1, retired()));
        assert_eq!(effects.session, disconnected());
        assert_eq!(items(&node).await, [GREETING, WAITING]);
        wakes.after_reopen().await;

        // A new process holds no result cache; the owner's ticket still names the operation.
        restart_gate::forget_for_tests(&operation(&node, 900_812_811));
        let writes = rig.writes();
        let replay = invoke(&node, 900_812_811).await;
        let last = replay.last().unwrap();
        assert!(
            last.contains("이미 처리한 재시작 요청") && last.contains("인사말을 대기열에 등록"),
            "{replay:?}"
        );
        assert_eq!(rig.writes(), writes, "a replayed operation closes nothing");
        assert_eq!(
            items(&node).await,
            [GREETING, WAITING],
            "and queues nothing"
        );
        drop(wakes);
        node.finish().await;
    });
}

// A restart whose close fails says so first and takes its greeting back after: the old session
// stays, the waiting original stays, and no greeting is left to run against it.
#[test]
fn restart_common_failed_close_answers_first_then_withdraws_its_greeting_pg() {
    let Some((rig, runtime)) = rig_child(
        "restart_channel_tests::restart_common_failed_close_answers_first_then_withdraws_its_greeting_pg",
    ) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, None).await;
        seed_selector(&node).await;
        install_owner(&node).await;
        waiting(&node).await;
        rig.require_confirmation(true);
        let wakes = Wakes::observe(&node.shared);
        let said = Arc::new(Mutex::new(Vec::new()));
        let (seen, mailbox) = (said.clone(), node.shared.mailbox(channel()));
        let probe: SayProbe = Arc::new(move |text| {
            let (seen, mailbox) = (seen.clone(), mailbox.clone());
            Box::pin(async move {
                let queued = queued_texts(&mailbox).await;
                seen.lock().unwrap().push((text, queued));
            })
        });
        let http = Arc::new(serenity::Http::new(""));
        let identity = ("restart-fixture", "!restart", 900_812_812);
        let fixture = data(&node);
        let call = restart_channel_probed_for_tests(&http, &fixture, channel(), identity, probe);
        let replies = tokio::time::timeout(WAIT, call).await.unwrap().unwrap();
        let last = replies.last().unwrap();
        assert!(
            !last.contains("종료를 확인했습니다") && !last.contains("대기열에 등록"),
            "a failed close is never reported as a restart: {replies:?}"
        );
        let said = said.lock().unwrap().clone();
        assert_eq!(
            said.last().map(|(_, queued)| queued.clone()),
            Some(vec![GREETING.to_string(), WAITING.to_string()]),
            "the failure is said while the greeting is still queued: {said:?}"
        );
        wakes.after_reopen().await;
        assert_eq!(items(&node).await, [WAITING], "the greeting is withdrawn");
        assert!(retained(&node).await.is_empty());
        assert_eq!(
            Seed::prior(
                &node.shared.mailbox(channel()),
                &operation(&node, 900_812_812)
            )
            .await,
            Ok(SeedStatus::Gone)
        );
        assert_eq!(
            herdr::row(&node.pool, &node.key).await,
            Some((herdr::HostedState::Bound, herdr::EXECUTION.into())),
            "the old session is kept"
        );
        assert_eq!(
            selectors(&node).await,
            (Some(SELECTOR.into()), Some(SELECTOR.into()))
        );
        drop(wakes);
        node.finish().await;
    });
}

// No execution to end: a missing or legacy row gets the greeting and a wake, and no close.
#[test]
fn restart_common_queues_the_greeting_for_a_missing_or_legacy_row_without_a_close_pg() {
    let Some((rig, runtime)) = rig_child(
        "restart_channel_tests::restart_common_queues_the_greeting_for_a_missing_or_legacy_row_without_a_close_pg",
    ) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        for (index, vacate) in [
            "UPDATE sessions SET hosted_execution=NULL, status='idle', active_dispatch_id=NULL WHERE session_key=$1 AND $2::TEXT IS NULL",
            "DELETE FROM sessions WHERE session_key=$1 AND $2::TEXT IS NULL",
        ]
        .into_iter()
        .enumerate()
        {
            let node = Node::bind(&rig, herdr::HostedState::Bound, None).await;
            node.sql(vacate, None).await;
            install_owner(&node).await;
            waiting(&node).await;
            let wakes = Wakes::observe(&node.shared);
            let writes = rig.writes();
            let response = invoke(&node, 900_812_813 + index as u64).await;
            let last = response.last().unwrap();
            assert!(
                last.contains("종료할 활성 세션이 없어") && last.contains("인사말을 대기열에 등록"),
                "{index}: {response:?}"
            );
            assert_eq!(rig.writes(), writes, "{index}: nothing is closed");
            assert_eq!(items(&node).await, [GREETING, WAITING], "{index}");
            wakes.after_reopen().await;
            drop(wakes);
            node.finish().await;
        }
    });
}

// A daemon death after the greeting's commit and before the close: at boot the permit finds the
// old execution running, retains the greeting with a notice, launches nothing and closes nothing.
#[test]
fn restart_seed_left_before_the_close_is_refused_at_boot_and_never_reaches_the_live_session_pg() {
    let Some((rig, runtime)) = rig_child_with(
        "restart_channel_tests::restart_seed_left_before_the_close_is_refused_at_boot_and_never_reaches_the_live_session_pg",
        true,
    ) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, None).await;
        seed_selector(&node).await;
        node.sql("UPDATE sessions SET status='idle', active_dispatch_id=NULL, last_heartbeat=NULL WHERE session_key=$1 AND $2::TEXT IS NULL", None).await;
        install_owner(&node).await;
        crashed_after_commit(&node, 900_812_815).await;
        let _gateway = gateway(&node).await;
        let launched = ran();
        let reached = launched.clone();
        let _launch = install_root_recovery_launch(
            channel(),
            Box::new(move |owner, _| {
                reached.store(true, Ordering::SeqCst);
                owner.defer_before_submit("fixture_stop");
                Err("NotSent(\"fixture_stop\")".into())
            }),
        );
        wake_drive_for_test(&node.shared, channel());
        let prior = || prior_of(&node, 900_812_815);
        until("the boot drive settles the greeting", async || {
            prior().await != Ok(SeedStatus::Queued) || launched.load(Ordering::SeqCst)
        })
        .await;
        assert!(
            !launched.load(Ordering::SeqCst),
            "a greeting queued against a live execution is never launched"
        );
        assert_eq!(prior().await, Ok(SeedStatus::Retained));
        let kept = retained(&node).await;
        assert!(
            matches!(kept.as_slice(), [(text, reason)]
                if text == GREETING && reason.contains("restart greeting refused")),
            "the refusal is the owner's visible start failure: {kept:?}"
        );
        assert!(items(&node).await.is_empty());
        assert!(rig.writes().is_empty(), "the permit sends no close");
        assert_eq!(
            herdr::row(&node.pool, &node.key).await,
            Some((herdr::HostedState::Bound, herdr::EXECUTION.into()))
        );
        node.finish().await;
    });
}

// A daemon death after the close and before the retirement: at boot the permit retires and
// detaches the ended execution without a new close, and only then is the greeting launched.
#[test]
fn restart_seed_left_after_the_close_finishes_the_retirement_at_boot_then_launches_pg() {
    let Some((rig, runtime)) = rig_child(
        "restart_channel_tests::restart_seed_left_after_the_close_finishes_the_retirement_at_boot_then_launches_pg",
    ) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, None).await;
        seed_selector(&node).await;
        node.sql("UPDATE sessions SET status='idle', active_dispatch_id=NULL, last_heartbeat=NULL WHERE session_key=$1 AND $2::TEXT IS NULL", None).await;
        rig.restore_shell();
        install_owner(&node).await;
        crashed_after_commit(&node, 900_812_816).await;
        let _gateway = gateway(&node).await;
        let launched = Arc::new(Mutex::new(None));
        let reached = launched.clone();
        let _launch = install_root_recovery_launch(
            channel(),
            Box::new(move |owner, text| {
                *reached.lock().unwrap() = Some((owner.restart_seed(), text.to_string()));
                owner.defer_before_submit("fixture_stop");
                Err("NotSent(\"fixture_stop\")".into())
            }),
        );
        wake_drive_for_test(&node.shared, channel());
        until(
            "the boot drive hands the greeting to its launch",
            async || launched.lock().unwrap().is_some(),
        )
        .await;
        assert_eq!(
            launched.lock().unwrap().clone(),
            Some((true, GREETING.to_string()))
        );
        // The permit ran to its end before the launch was reached, and nothing ran since.
        assert_eq!(
            herdr::row(&node.pool, &node.key).await,
            retired(),
            "the launch sees the greeting only after the old execution is retired"
        );
        let effects = node.effects(&rig).await;
        assert_eq!(effects.closes(), 0, "the permit sends no close");
        assert_eq!(effects.session, disconnected());
        assert_eq!(
            selectors(&node).await,
            (Some(SELECTOR.into()), Some(SELECTOR.into())),
            "the conversation stays resumable"
        );
        assert!(retained(&node).await.is_empty());
        node.finish().await;
    });
}
