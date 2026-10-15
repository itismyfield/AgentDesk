//! Intake publishes a turn's route only after its mailbox claim, so a policy close's late tail can
//! neither take a successor's resolved route nor run while that successor holds the channel.
use super::*;
use crate::services::discord::turn_finalizer::{PolicyCloseIntent, PolicyCloseOutcome, TurnKey};
use crate::services::turn_orchestrator::policy_close::tail::{PolicyTailDecision, PolicyTailSkip};
use crate::services::turn_orchestrator::policy_close::{
    PolicyAuthority, PolicyClear, PolicyCloseRequest, PolicyReceipt, test_hooks,
};
use std::time::Duration;

/// A receipt the step clears at once; the routing contract does not depend on its row.
struct Cleared(u64);

impl PolicyReceipt for Cleared {
    fn channel(&self) -> u64 {
        self.0
    }

    fn try_clear(&self, _: &PolicyAuthority<'_>) -> PolicyClear {
        PolicyClear::Cleared
    }
}

struct Ids {
    thread: ChannelId,
    parent: ChannelId,
    alt: ChannelId,
}

impl Ids {
    fn new(base: u64) -> Self {
        Self {
            thread: ChannelId::new(base),
            parent: ChannelId::new(base + 1),
            alt: ChannelId::new(base + 2),
        }
    }

    fn route(&self, shared: &SharedData) -> (Option<ChannelId>, Option<ChannelId>) {
        let dispatch = &shared.dispatch;
        (
            dispatch.thread_parents.get(&self.parent).map(|t| *t),
            dispatch.role_overrides.get(&self.thread).map(|r| *r),
        )
    }
}

/// A, an exact turn on the thread whose claimed route names the role channel C will also use.
/// Its close commits and seals, then panics at the seam before its tail; it returns A's re-ask.
async fn sealed_close(shared: &Arc<SharedData>, ids: &Ids) -> impl Fn() -> PolicyCloseIntent {
    let actor = Arc::new(CancelToken::new());
    let message = MessageId::new(ids.thread.get() + 40);
    assert!(
        crate::services::discord::mailbox_try_start_turn_kinded(
            shared,
            ids.thread,
            actor.clone(),
            UserId::new(7),
            message,
            crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent
        )
        .await
    );
    crate::services::discord::increment_global_active(shared, "route_claim_a");
    let generation = shared.restart.current_generation;
    let key =
        TurnKey::new(ids.thread, message.get(), generation).with_episode_nonce(actor.turn_nonce());
    let provider = shared.provider.clone();
    let none = crate::services::discord::inflight::RelayOwnerKind::None;
    shared
        .turn_finalizer
        .register_start(key, provider.clone(), none, shared);
    shared
        .dispatch
        .thread_parents
        .insert(ids.parent, ids.thread);
    shared.dispatch.role_overrides.insert(ids.thread, ids.alt);
    let (shared, thread) = (shared.clone(), ids.thread);
    let mut row = crate::services::discord::inflight::InflightTurnState::new(
        provider.clone(),
        thread.get(),
        None,
        7,
        message.get(),
        message.get() + 1,
        "a".into(),
        None,
        None,
        None,
        None,
        0,
    );
    row.turn_nonce = actor.turn_nonce().map(str::to_owned);
    let intent = move || {
        let request = PolicyCloseRequest {
            actor: Arc::downgrade(&actor),
            user_message_id: message,
            turn_nonce: actor.turn_nonce().unwrap().to_owned(),
            interrupt_generation: actor.claude_interrupt_generation(),
            observed_before: std::time::Instant::now(),
            receipt: Box::new(Cleared(thread.get())),
            active: shared.restart.global_active.clone(),
            persistence: crate::services::discord::queue_persistence_context(
                &shared, &provider, thread,
            ),
            release_reconcile_allowed: false,
            ticket: None,
            trace: Default::default(),
        };
        PolicyCloseIntent {
            key,
            provider: provider.clone(),
            mailbox: shared.mailbox_peek(thread).unwrap(),
            request: test_hooks::bind(request),
            snapshot: crate::services::discord::turn_finalizer::SyntheticClaimSnapshot::from_row(
                &row,
            ),
        }
    };
    let first = intent();
    first.request.trace.probe().on_stage(Box::new(|stage| {
        if stage == test_hooks::Stage::BeforeFinalize {
            panic!("A's tail stops before it runs");
        }
    }));
    assert!(matches!(
        submit(first).await,
        PolicyCloseOutcome::Unconfirmed(_)
    ));
    intent
}

async fn submit(intent: PolicyCloseIntent) -> PolicyCloseOutcome {
    let shared = SHARED.lock().unwrap().clone().unwrap();
    shared
        .turn_finalizer
        .submit_policy_close(intent, shared.clone())
        .await
}

/// The test's one shared data, so a re-ask needs nothing but its intent.
static SHARED: std::sync::Mutex<Option<Arc<SharedData>>> = std::sync::Mutex::new(None);

/// The real runtime C runs in: a mocked Discord and card API, both channels mapped, and the alt
/// channel's role on the counter-model provider.
async fn runtime(
    ids: &Ids,
    workspace: &std::path::Path,
) -> (Arc<SharedData>, impl Sized, Arc<serenity::http::Http>) {
    let (thread, alt) = (ids.thread, ids.alt);
    let dispatch_workspace = workspace.display().to_string();
    let api = crate::services::discord::admin_host_guard::tests::Recorder::start_with(Arc::new(
        move |_, path| {
            if path == "/api/internal/card-thread" {
                Some(serde_json::json!({
                    "active_thread_id": thread.to_string(),
                    "dispatch_type": "review",
                    "discord_channel_alt": alt.to_string(),
                    "dispatch_context":
                        serde_json::json!({"worktree_path": dispatch_workspace}).to_string()
                }))
            } else if path.ends_with(&format!("/channels/{thread}")) {
                let name = "route-claim";
                Some(serde_json::json!({"id": thread.to_string(), "type": 0, "name": name, "position": 0}))
            } else {
                None
            }
        },
    ))
    .await;
    crate::services::discord::internal_api::init(api.port, None);
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().api_port = api.port;
    for channel in [ids.thread, ids.parent] {
        crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, "").await;
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        session.channel_name = None;
        session.current_path = Some(workspace.display().to_string());
        session.session_id = Some("route-claim-existing".into());
    }
    let path = crate::services::discord::runtime_store::role_map_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let role = serde_json::json!({"roleId": "route-claim-reviewer", "promptFile": "", "provider": "codex"});
    let roles = serde_json::json!({"byChannelId": {alt.get().to_string(): role}});
    std::fs::write(&path, roles.to_string()).unwrap();
    *SHARED.lock().unwrap() = Some(shared.clone());
    let http = api.http.clone();
    (shared, api, http)
}

/// C, a counter-model review dispatch from the parent into the thread, through the real intake.
async fn intake(shared: &Arc<SharedData>, http: &Arc<serenity::http::Http>, ids: &Ids) -> bool {
    let message = MessageId::new(ids.thread.get() + 60);
    let request = super::super::IntakeRequest {
        intake_outbox_id: None,
        channel_id: ids.parent,
        user_msg_id: message,
        source_message_ids: vec![message],
        busy_followup_retry_user_msg_id: message,
        request_owner: UserId::new(7),
        request_owner_name: "owner".into(),
        user_text: "DISPATCH:route-claim-review - review".into(),
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
    let run = super::super::execute_intake_turn_core(http, shared, "", request, Vec::new());
    tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .unwrap()
        .inspect_err(|error| eprintln!("C's intake failed: {error}"))
        .is_ok()
}

/// Holds C at its provider, after it claimed and published, until the returned sender fires.
fn hold_provider(
    thread: ChannelId,
) -> (
    tokio::sync::oneshot::Receiver<(bool, bool)>,
    std::sync::mpsc::Sender<()>,
) {
    let (entered, at) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let probe = super::super::super::provider_dispatch::InputEffectProbe {
        channel: thread.get(),
        entered,
        release: wait,
        terminal_before_release: true,
    };
    *super::super::super::provider_dispatch::INPUT_EFFECT_PROBE
        .lock()
        .unwrap() = Some(probe);
    (at, release)
}

async fn entered(at: tokio::sync::oneshot::Receiver<(bool, bool)>) -> bool {
    let entered = tokio::time::timeout(Duration::from_secs(10), at).await;
    entered.unwrap().unwrap().0
}

fn opened() {
    let boot = serde_json::from_value(serde_json::json!({"server": {}, "agents": []})).unwrap();
    crate::services::tui_o::channel_policy::install(&boot).unwrap();
}

/// A's tail runs while C has resolved its route but not claimed: A releases only its own route,
/// and C publishes its parent edge and the same role channel once its claim succeeds.
#[tokio::test]
async fn s2f_route_resolved_before_the_tail_is_published_by_its_claim() {
    if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::intake_turn::adk_thread::route_claim_tests::",
        "s2f_route_resolved_before_the_tail_is_published_by_its_claim"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    opened();
    let ids = Ids::new(6_342_101);
    let workspace = tempfile::tempdir().unwrap();
    let (shared, _api, http) = runtime(&ids, workspace.path()).await;
    let intent = sealed_close(&shared, &ids).await;
    let (resolved, resume) = super::hold_route(ids.thread);
    let (at, release) = hold_provider(ids.thread);
    let steps = async {
        resolved.await.unwrap();
        let second = intent();
        let probe = second.request.trace.probe();
        let outcome = submit(second).await;
        assert!(
            matches!(outcome, PolicyCloseOutcome::Settled),
            "{outcome:?}"
        );
        assert_eq!(
            probe.tail(),
            Some(PolicyTailDecision::Granted),
            "C has not claimed"
        );
        assert_eq!(
            ids.route(&shared),
            (None, None),
            "A's own route is released"
        );
        resume.send(()).unwrap();
        entered(at).await;
        assert_eq!(ids.route(&shared), (Some(ids.thread), Some(ids.alt)));
        let codex = crate::services::discord::inflight::load_inflight_state(
            &ProviderKind::Codex,
            ids.thread.get(),
        );
        assert!(
            codex.is_some(),
            "C's turn runs on the role channel's provider"
        );
        release.send(()).unwrap();
    };
    let (started, ()) = tokio::join!(intake(&shared, &http, &ids), steps);
    assert!(started, "C's intake returns");
    shared.mailboxes.remove_fixture_for_test(ids.thread);
}

/// C claims and publishes before A's tail: the tail is skipped, and C's route, naming the same
/// role channel A's did, stays exactly as C published it.
#[tokio::test]
async fn s2f_claimed_route_survives_a_later_tail() {
    if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
        "services::discord::router::message_handler::intake_turn::adk_thread::route_claim_tests::",
        "s2f_claimed_route_survives_a_later_tail"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    opened();
    let ids = Ids::new(6_342_111);
    let workspace = tempfile::tempdir().unwrap();
    let (shared, _api, http) = runtime(&ids, workspace.path()).await;
    let intent = sealed_close(&shared, &ids).await;
    let (at, release) = hold_provider(ids.thread);
    let mut events =
        crate::services::discord::turn_completion_events::subscribe_turn_completion_events(&shared);
    let steps = async {
        entered(at).await;
        let published = ids.route(&shared);
        assert_eq!(published, (Some(ids.thread), Some(ids.alt)));
        let second = intent();
        let probe = second.request.trace.probe();
        let outcome = submit(second).await;
        assert!(
            matches!(outcome, PolicyCloseOutcome::Settled),
            "{outcome:?}"
        );
        let skipped = PolicyTailDecision::Skipped(PolicyTailSkip::SuccessorObserved);
        assert_eq!(probe.tail(), Some(skipped));
        assert_eq!(ids.route(&shared), published, "C's route is untouched");
        let mut released = 0;
        while let Ok(event) = events.try_recv() {
            released += usize::from(
                event.channel_id == ids.thread
                    && event.phase
                        == crate::services::discord::turn_completion_events::TurnCompletionPhase::MailboxReleased,
            );
        }
        assert_eq!(released, 0, "no release edge while C holds the thread");
        release.send(()).unwrap();
    };
    let (started, ()) = tokio::join!(intake(&shared, &http, &ids), steps);
    assert!(started, "C's intake returns");
    shared.mailboxes.remove_fixture_for_test(ids.thread);
}
