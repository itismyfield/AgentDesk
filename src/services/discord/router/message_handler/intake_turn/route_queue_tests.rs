//! A turn that cannot claim keeps the route it resolved with its queued item, so the replay's
//! claim publishes that route; a claim's route replaces whatever the channel held.
use super::*;
use crate::services::turn_orchestrator::QueuedRoute;

fn deps<'a>(shared: &'a Arc<SharedData>, http: &'a Arc<serenity::http::Http>) -> IntakeDeps<'a> {
    IntakeDeps {
        http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared,
        token: "",
    }
}

/// Keeps queued admission local, with no cluster routing, as the queued intake tests do.
fn local_admission() -> crate::config::TestEnvVarGuard {
    let root = std::path::PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
    let config = crate::runtime_layout::config_file_path(&root);
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "server: {}\ndata: {dir: data}\n").unwrap();
    crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_CONFIG", &config)
}

fn hold_effect(
    channel: ChannelId,
) -> (
    tokio::sync::oneshot::Receiver<(bool, bool)>,
    std::sync::mpsc::Sender<()>,
) {
    super::super::super::super::hold_input_effect_for_tests(channel.get())
}

/// Whether the held effect starts within the wait.
async fn starts(at: tokio::sync::oneshot::Receiver<(bool, bool)>, wait: Duration) -> bool {
    matches!(tokio::time::timeout(wait, at).await, Ok(Ok(_)))
}

/// A follow-up the parent's thread guard queued while the thread ran; only a kick starts it.
async fn held_on_parent(shared: &Arc<SharedData>, ids: &Ids) {
    let message = MessageId::new(ids.thread.get() + 70);
    let held = build_race_requeued_intervention(
        UserId::new(7),
        message,
        "parent follow-up",
        false,
        None,
        false,
        false,
        Vec::new(),
        None,
    );
    let persistence =
        crate::services::discord::queue_persistence_context(shared, &shared.provider, ids.parent);
    (shared.mailbox(ids.parent))
        .replace_queue(vec![held], persistence)
        .await;
}

fn queued_routes(
    snapshot: &crate::services::turn_orchestrator::ChannelMailboxSnapshot,
) -> Vec<Option<QueuedRoute>> {
    snapshot
        .intervention_queue
        .iter()
        .map(Intervention::queued_route)
        .collect()
}

/// Promotes the thread's queued C through the real kickoff and checks that its claim publishes the
/// queued route, that it runs once on the role channel's provider, and that its end kicks the parent.
async fn replay_runs_once_and_kicks_the_parent(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    ids: &Ids,
) {
    held_on_parent(shared, ids).await;
    let (at, release) = hold_effect(ids.thread);
    let steps = async {
        assert!(starts(at, Duration::from_secs(15)).await, "C replays");
        assert_eq!(ids.route(shared), (Some(ids.thread), Some(ids.alt)));
        let codex = crate::services::discord::inflight::load_inflight_state(
            &ProviderKind::Codex,
            ids.thread.get(),
        );
        assert!(codex.is_some(), "C runs on the role channel's provider");
        let kicked = || {
            shared
                .restart
                .deferred_hook_channels
                .contains_key(&ids.parent)
        };
        assert!(!kicked(), "the parent stays held while C runs");
        release.send(()).unwrap();
        let wait = tokio::time::timeout(Duration::from_secs(15), async {
            while !kicked() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        assert!(
            wait.await.is_ok(),
            "C's end releases its parent edge and kicks the parent"
        );
    };
    let deps = deps(shared, http);
    let kick =
        crate::services::discord::kickoff_idle_queue_channel(&deps, &shared.provider, ids.thread);
    let (_, ()) = tokio::join!(kick, steps);
    let snapshot = crate::services::discord::mailbox_snapshot(shared, ids.thread).await;
    assert!(snapshot.intervention_queue.is_empty(), "C left the queue");
    let (again, _release) = hold_effect(ids.thread);
    let kick =
        crate::services::discord::kickoff_idle_queue_channel(&deps, &shared.provider, ids.thread);
    assert!(!kick.await.started);
    assert!(!starts(again, Duration::from_secs(1)).await, "C runs once");
    *super::super::super::provider_dispatch::INPUT_EFFECT_PROBE
        .lock()
        .unwrap() = None;
}

fn c_route(ids: &Ids) -> QueuedRoute {
    QueuedRoute {
        target: ids.thread,
        parent: Some(ids.parent),
        role: Some(ids.alt),
    }
}

/// C resolves the parent edge and role, then loses the thread's claim to another input: its queued
/// item keeps that route through a busy restore, and the replay's claim publishes it.
#[tokio::test]
async fn s2f_lost_claim_replays_its_resolved_route() {
    if !child(concat!(
        "services::discord::router::message_handler::intake_turn::adk_thread::route_claim_tests::",
        "queue::s2f_lost_claim_replays_its_resolved_route"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _config = local_admission();
    opened();
    let ids = Ids::new(6_342_121);
    let workspace = tempfile::tempdir().unwrap();
    let (shared, _api, http) = runtime(&ids, workspace.path()).await;
    let (resolved, resume) = super::super::hold_route(ids.thread);
    let winner = Arc::new(CancelToken::new());
    let steps = async {
        resolved.await.unwrap();
        assert!(
            crate::services::discord::mailbox_try_start_turn_kinded(
                &shared,
                ids.thread,
                winner.clone(),
                UserId::new(8),
                MessageId::new(ids.thread.get() + 50),
                crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent
            )
            .await
        );
        resume.send(()).unwrap();
    };
    let (returned, ()) = tokio::join!(intake(&shared, &http, &ids), steps);
    assert!(returned, "C's intake returns");
    let snapshot = crate::services::discord::mailbox_snapshot(&shared, ids.thread).await;
    assert_eq!(queued_routes(&snapshot), [Some(c_route(&ids))]);
    assert_eq!(
        ids.route(&shared),
        (None, None),
        "nothing is published without a claim"
    );
    crate::services::discord::mailbox_finish_turn(&shared, &shared.provider, ids.thread).await;

    // The replay meets a busy transition after it dequeued: the restore hands back its route.
    let (resolved, resume) = super::super::hold_route(ids.thread);
    let deps = deps(&shared, &http);
    let kick =
        crate::services::discord::kickoff_idle_queue_channel(&deps, &shared.provider, ids.thread);
    let busy = async {
        resolved.await.unwrap();
        let held = shared
            .session_transition_lock(ids.thread)
            .lock_owned()
            .await;
        resume.send(()).unwrap();
        held
    };
    let (outcome, held) = tokio::join!(kick, busy);
    assert!(!outcome.started);
    let snapshot = crate::services::discord::mailbox_snapshot(&shared, ids.thread).await;
    assert_eq!(queued_routes(&snapshot), [Some(c_route(&ids))]);
    assert!(
        shared
            .restart
            .deferred_hook_channels
            .contains_key(&ids.thread),
        "the restore ran"
    );
    assert_eq!(ids.route(&shared), (None, None));
    drop(held);

    replay_runs_once_and_kicks_the_parent(&shared, &http, &ids).await;
    shared.mailboxes.remove_fixture_for_test(ids.thread);
    shared.mailboxes.remove_fixture_for_test(ids.parent);
}

/// A fresh C meets the thread's busy transition after it resolved its route: the durable queued
/// item keeps that route, and the replay's claim publishes it.
#[tokio::test]
async fn s2f_transition_busy_intake_replays_its_resolved_route() {
    if !child(concat!(
        "services::discord::router::message_handler::intake_turn::adk_thread::route_claim_tests::",
        "queue::s2f_transition_busy_intake_replays_its_resolved_route"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _config = local_admission();
    opened();
    let ids = Ids::new(6_342_131);
    let workspace = tempfile::tempdir().unwrap();
    let (shared, _api, http) = runtime(&ids, workspace.path()).await;
    let held = shared
        .session_transition_lock(ids.thread)
        .lock_owned()
        .await;
    assert!(intake(&shared, &http, &ids).await, "C's intake returns");
    let snapshot = crate::services::discord::mailbox_snapshot(&shared, ids.thread).await;
    assert_eq!(queued_routes(&snapshot), [Some(c_route(&ids))]);
    assert_eq!(
        ids.route(&shared),
        (None, None),
        "nothing is published without a claim"
    );
    drop(held);

    replay_runs_once_and_kicks_the_parent(&shared, &http, &ids).await;
    shared.mailboxes.remove_fixture_for_test(ids.thread);
    shared.mailboxes.remove_fixture_for_test(ids.parent);
}

/// A plain follow-up in `channel` through the real intake; it routes by what the channel holds.
async fn follow_up(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel: ChannelId,
    message: MessageId,
) -> bool {
    let request = super::super::super::IntakeRequest {
        intake_outbox_id: None,
        channel_id: channel,
        user_msg_id: message,
        source_message_ids: vec![message],
        busy_followup_retry_user_msg_id: message,
        request_owner: UserId::new(7),
        request_owner_name: "owner".into(),
        user_text: "follow up".into(),
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
    let run = super::super::super::execute_intake_turn_core(http, shared, "", request, Vec::new());
    let result = tokio::time::timeout(Duration::from_secs(20), run).await;
    result.unwrap().is_ok()
}

fn codex_runs(thread: ChannelId, message: MessageId) -> bool {
    let row =
        crate::services::discord::inflight::load_inflight_state(&ProviderKind::Codex, thread.get());
    row.is_some_and(|row| row.user_msg_id == message.get())
}

/// C inherits A's role and parent edge, and A's granted tail releases both before C claims; C's
/// claim publishes them again, and D, queued behind C, takes the role channel's provider.
#[tokio::test]
async fn s2f_inherited_route_is_republished_by_its_claim() {
    if !child(concat!(
        "services::discord::router::message_handler::intake_turn::adk_thread::route_claim_tests::",
        "queue::s2f_inherited_route_is_republished_by_its_claim"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _config = local_admission();
    opened();
    let ids = Ids::new(6_342_141);
    let workspace = tempfile::tempdir().unwrap();
    let (shared, _api, http) = runtime(&ids, workspace.path()).await;
    let intent = sealed_close(&shared, &ids).await;
    let (c, d) = (
        MessageId::new(ids.thread.get() + 80),
        MessageId::new(ids.thread.get() + 81),
    );
    let (resolved, resume) = super::super::hold_route(ids.thread);
    let (at, release) = hold_effect(ids.thread);
    let steps = async {
        resolved.await.unwrap();
        let second = intent();
        let probe = second.request.trace.probe();
        let outcome = submit(second).await;
        assert!(
            matches!(outcome, PolicyCloseOutcome::Settled),
            "{outcome:?}"
        );
        assert_eq!(probe.tail(), Some(PolicyTailDecision::Granted));
        assert_eq!(
            ids.route(&shared),
            (None, None),
            "A's tail releases its route"
        );
        resume.send(()).unwrap();
        assert!(starts(at, Duration::from_secs(15)).await, "C runs");
        assert_eq!(ids.route(&shared), (Some(ids.thread), Some(ids.alt)));
        assert!(
            codex_runs(ids.thread, c),
            "C keeps the role channel's provider"
        );
        assert!(
            follow_up(&shared, &http, ids.thread, d).await,
            "D queues behind C"
        );
        let snapshot = crate::services::discord::mailbox_snapshot(&shared, ids.thread).await;
        let routes = queued_routes(&snapshot);
        assert_eq!(
            routes,
            [Some(QueuedRoute {
                target: ids.thread,
                ..c_route(&ids)
            })]
        );
        release.send(()).unwrap();
    };
    let (returned, ()) = tokio::join!(follow_up(&shared, &http, ids.thread, c), steps);
    assert!(returned, "C's intake returns");
    let (at, release) = hold_effect(ids.thread);
    let deps = deps(&shared, &http);
    let steps = async {
        assert!(starts(at, Duration::from_secs(15)).await, "D replays");
        assert!(
            codex_runs(ids.thread, d),
            "D runs on the role channel's provider"
        );
        release.send(()).unwrap();
    };
    let wait = tokio::time::timeout(Duration::from_secs(15), async {
        while crate::services::discord::mailbox_snapshot(&shared, ids.thread)
            .await
            .cancel_token
            .is_some()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    assert!(wait.await.is_ok(), "C ends");
    let kick =
        crate::services::discord::kickoff_idle_queue_channel(&deps, &shared.provider, ids.thread);
    let (_, ()) = tokio::join!(kick, steps);
    shared.mailboxes.remove_fixture_for_test(ids.thread);
    shared.mailboxes.remove_fixture_for_test(ids.parent);
}

/// C resolves with no role and no parent edge; a stale override and parent edge appear before its
/// claim. The claim replaces both, and the replaced parent loses its guard with a queue kick.
#[tokio::test]
async fn s2f_claim_replaces_stale_role_and_parent_edge() {
    if !child(concat!(
        "services::discord::router::message_handler::intake_turn::adk_thread::route_claim_tests::",
        "queue::s2f_claim_replaces_stale_role_and_parent_edge"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _config = local_admission();
    opened();
    let ids = Ids::new(6_342_151);
    let workspace = tempfile::tempdir().unwrap();
    let (shared, _api, http) = runtime(&ids, workspace.path()).await;
    held_on_parent(&shared, &ids).await;
    let c = MessageId::new(ids.thread.get() + 80);
    let (resolved, resume) = super::super::hold_route(ids.thread);
    let (at, release) = hold_effect(ids.thread);
    let kicked = || {
        shared
            .restart
            .deferred_hook_channels
            .contains_key(&ids.parent)
    };
    let steps = async {
        resolved.await.unwrap();
        let dispatch = &shared.dispatch;
        dispatch.thread_parents.insert(ids.parent, ids.thread);
        dispatch.role_overrides.insert(ids.thread, ids.alt);
        assert!(!kicked());
        resume.send(()).unwrap();
        assert!(starts(at, Duration::from_secs(15)).await, "C runs");
        assert_eq!(
            ids.route(&shared),
            (None, None),
            "C's claim replaces the stale route"
        );
        assert!(kicked(), "the replaced parent is kicked");
        assert!(
            !codex_runs(ids.thread, c),
            "C keeps the provider it resolved"
        );
        release.send(()).unwrap();
    };
    let (returned, ()) = tokio::join!(follow_up(&shared, &http, ids.thread, c), steps);
    assert!(returned, "C's intake returns");
    shared.mailboxes.remove_fixture_for_test(ids.thread);
    shared.mailboxes.remove_fixture_for_test(ids.parent);
}

fn queued(id: u64, text: &str, route: Option<QueuedRoute>) -> Intervention {
    let item = build_race_requeued_intervention(
        UserId::new(7),
        MessageId::new(id),
        text,
        false,
        None,
        false,
        true,
        Vec::new(),
        None,
    );
    match route {
        Some(route) => item.with_route(route),
        None => item,
    }
}

/// Consecutive inputs merge only when they carry the same route, and every merged source keeps
/// it, so whichever source an item keeps, its route is the same.
#[test]
fn s2f_only_inputs_with_one_route_merge() {
    let route = |parent: Option<u64>| QueuedRoute {
        target: ChannelId::new(6_342_161),
        parent: parent.map(ChannelId::new),
        role: None,
    };
    let mut queue = Vec::new();
    let inputs = [
        (1, Some(route(Some(9)))),
        (2, Some(route(Some(9)))),
        (3, Some(route(None))),
        (4, None),
        (5, None),
    ];
    for (id, route) in inputs {
        let text = format!("input {id}");
        let enqueue = crate::services::turn_orchestrator::enqueue_intervention;
        assert!(enqueue(&mut queue, queued(id, &text, route), None).enqueued);
    }
    let routes: Vec<_> = queue.iter().map(Intervention::queued_route).collect();
    assert_eq!(routes, [Some(route(Some(9))), Some(route(None)), None]);
    let sources = |item: &Intervention| -> Vec<(u64, Option<QueuedRoute>)> {
        let records = item.source_message_queued_generations.iter();
        records.map(|s| (s.message_id.get(), s.route)).collect()
    };
    assert_eq!(
        sources(&queue[0]),
        [(1, Some(route(Some(9)))), (2, Some(route(Some(9))))]
    );
    assert_eq!(sources(&queue[2]), [(4, None), (5, None)]);
}

/// A queue file keeps the route through a restart; a file written before routes were kept reads
/// as none, and that item resolves its route again on replay.
#[tokio::test]
async fn s2f_old_queue_file_replays_by_resolving_again() {
    if !child(concat!(
        "services::discord::router::message_handler::intake_turn::adk_thread::route_claim_tests::",
        "queue::s2f_old_queue_file_replays_by_resolving_again"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _config = local_admission();
    opened();
    let ids = Ids::new(6_342_171);
    let workspace = tempfile::tempdir().unwrap();
    let (shared, _api, http) = runtime(&ids, workspace.path()).await;
    let message = ids.thread.get() + 60;
    let persistence =
        crate::services::discord::queue_persistence_context(&shared, &shared.provider, ids.thread);
    let item = queued(
        message,
        "DISPATCH:route-claim-review - review",
        Some(c_route(&ids)),
    );
    (shared.mailbox(ids.thread))
        .replace_queue(vec![item], persistence.clone())
        .await;
    let hydrate = || async {
        shared.mailboxes.remove_fixture_for_test(ids.thread);
        let mailbox = shared.mailbox(ids.thread);
        let result = mailbox
            .try_hydrate_pending_queue_from_disk(persistence.clone())
            .await;
        assert_eq!(result.unwrap().absorbed, 1);
        let snapshot = crate::services::discord::mailbox_snapshot(&shared, ids.thread).await;
        queued_routes(&snapshot)
    };
    assert_eq!(
        hydrate().await,
        [Some(c_route(&ids))],
        "the route survives a restart"
    );

    let file = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join(shared.provider.as_str())
        .join(&shared.token_hash)
        .join(format!("{}.json", ids.thread.get()));
    let written = std::fs::read_to_string(&file).unwrap();
    assert!(written.contains("\"route\""));
    fn strip(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                map.remove("route");
                map.values_mut().for_each(strip);
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let mut old: serde_json::Value = serde_json::from_str(&written).unwrap();
    strip(&mut old);
    std::fs::write(&file, old.to_string()).unwrap();
    assert_eq!(hydrate().await, [None], "an old file has no route");

    let (at, release) = hold_effect(ids.thread);
    let steps = async {
        assert!(
            starts(at, Duration::from_secs(15)).await,
            "the old item replays"
        );
        assert_eq!(
            ids.route(&shared),
            (None, Some(ids.alt)),
            "it resolves again"
        );
        release.send(()).unwrap();
    };
    let deps = deps(&shared, &http);
    let kick =
        crate::services::discord::kickoff_idle_queue_channel(&deps, &shared.provider, ids.thread);
    let (_, ()) = tokio::join!(kick, steps);
    shared.mailboxes.remove_fixture_for_test(ids.thread);
}
