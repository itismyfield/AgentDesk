use super::*;
use crate::services::discord::commands::{
    probe_restart_disconnect_for_tests, restart_channel_for_tests,
};
use crate::services::discord::queue_io::cancel_backstop_test_support as wake_probe;
use crate::services::discord::queue_io::input_owner::restart_gate;
use crate::services::herdr_launch::clear_epoch;
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

async fn ended(node: &Node, rig: &herdr::P11TerminationRig, closes: usize) {
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
    assert!(effects.queued.is_empty(), "this leg queues no greeting");
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
        ended(&node, &rig, 1).await;
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
        ended(&node, &rig, 1).await;
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
        ended(&node, &rig, 0).await;
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
        ended(&node, &rig, 1).await;
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
        assert_eq!(
            serde_json::to_vec(&after["items"]).unwrap(),
            before_bytes,
            "restart preserves every waiting original byte and its order"
        );
        let after_order: Vec<_> = after["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["message_id"].as_u64().unwrap())
            .collect();
        assert_eq!(after_order, before_order);
        let disk_after: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_vec(&disk_after["items"]).unwrap(),
            before_bytes
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
