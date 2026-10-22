//! The execution an HTTP force cancel or force-kill first selected binds its run to the end: a
//! successor execution published on the same row after that read is refused before any close.

use crate::db::dispatched_sessions::hosted_execution::{
    self as hosted, ExpectedExecution, HostedCasOutcome, HostedExecution, HostedLookup,
    HostedLookupKey, HostedObservation, HostedRecord,
};
use crate::services::discord::herdr_terminate::http_force::Expected;
use crate::services::herdr_launch::{
    EvidenceProbe, HerdrCreateOutcome, HerdrCreateRequest, HerdrLaunch, HerdrLaunchCommand,
    HerdrLaunchEndpoint, HerdrLaunchHost, HerdrLaunchOutcome, launch_herdr_session,
};
use crate::services::session_host::{EvidenceGap, RestoreResume, ServerWitness};
use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;

use super::*;

const IDENTITY: &str = "execution identity differs";
const SUCCESSOR_NONCE: &str = "successor-execution";
const MOVED_ROOT: &str = "/moved-runtime-root";

/// The row's record as another execution of it: a new nonce throughout, on the launched pane.
const NONCE_SWAP: &str = "UPDATE sessions SET hosted_execution = jsonb_set(jsonb_set(jsonb_set(jsonb_set(hosted_execution, '{execution_nonce}', '\"successor-execution\"'), '{source_ref,execution_nonce}', '\"successor-execution\"'), '{expected,binding_nonce}', '\"successor-execution\"'), '{location,pane_id}', '\"w2-1\"') WHERE session_key = $1";
/// The row's record under another owner field, one the service's own checks never read.
const OWNER_MOVE: &str = "UPDATE sessions SET hosted_execution = jsonb_set(jsonb_set(hosted_execution, '{owner,runtime_root}', '\"/moved-runtime-root\"'), '{source_ref,runtime_root}', '\"/moved-runtime-root\"') WHERE session_key = $1";

/// Each edit with the nonce and owner root the row records once it applied.
const EDITS: [(&str, &str, &str); 2] = [
    (NONCE_SWAP, SUCCESSOR_NONCE, ""),
    (OWNER_MOVE, herdr::EXECUTION, MOVED_ROOT),
];

/// The rig for test `name` in its own child, on a current-thread runtime so every probe runs.
fn rig(name: &str) -> Option<(herdr::P11TerminationRig, tokio::runtime::Runtime)> {
    if !herdr::in_child(&format!("{}::{name}", module_path!()), CHANNEL) {
        return None;
    }
    let rig = herdr::P11TerminationRig::boot_with(CHANNEL, false);
    rig.thread_installer()();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_start(rig.thread_installer())
        .build()
        .unwrap();
    Some((rig, runtime))
}

/// A probe applying `edit` to the selected row right after the selection read.
fn rewrite(node: &Node, edit: &'static str, flag: Arc<AtomicBool>) -> SettlementWindowProbe {
    let (pool, key) = (node.pool.clone(), node.key.clone());
    Box::new(move || {
        Box::pin(async move {
            let done = sqlx::query(edit).bind(&key).execute(&pool).await.unwrap();
            assert_eq!(done.rows_affected(), 1);
            flag.store(true, Ordering::SeqCst);
        })
    })
}

/// The record's owner root, asserting the row still decodes as a known record of `nonce`.
async fn recorded_root(node: &Node, nonce: &str) -> String {
    let row = herdr::row(&node.pool, &node.key).await;
    assert_eq!(row, Some((herdr::HostedState::Bound, nonce.to_string())));
    let root =
        "SELECT hosted_execution->'owner'->>'runtime_root' FROM sessions WHERE session_key = $1";
    let root: String = sqlx::query_scalar(root)
        .bind(&node.key)
        .fetch_one(&node.pool)
        .await
        .unwrap();
    root
}

/// The turn holding the channel is `nonce`'s and nothing cancelled it.
async fn runs(node: &Node, nonce: &str) {
    let turn = test_turn::active(&node.shared, channel()).await;
    let turn = turn.expect("the turn still runs");
    assert_eq!(turn.turn_nonce(), Some(nonce));
    assert!(!turn.cancelled.load(Ordering::SeqCst));
}

/// A fresh database and runtime with the row active on `dispatch`.
async fn rebind(previous: Node, rig: &herdr::P11TerminationRig, dispatch: Option<&str>) -> Node {
    previous.finish().await;
    Node::bind(rig, herdr::HostedState::Bound, dispatch).await
}

/// `request` as the owner receives it forwarded from another node.
async fn forwarded(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let bearer = format!("Bearer {AUTH}");
    let headers = [
        ("authorization", bearer.as_str()),
        JSON,
        ("x-agentdesk-forwarded-by", "origin-node"),
        ("x-agentdesk-session-owner", herdr::NODE),
    ];
    let body = body.to_string();
    crate::server::routes::home_stop_test_post(app, uri, &headers, &body).await
}

// A forced cancel retires only the execution its own read selected: a later nonce or owner on the
// same row, a forwarded cancel's receiver read, a no-dispatch turn giving way to a dispatched one,
// and a successor the production launch made after the selected one retired are each refused
// before any close, and the running turn, session, dispatches and queue stay.
#[test]
fn e07_initial_execution_identity_is_immutable_pg() {
    let Some((rig, runtime)) = rig("e07_initial_execution_identity_is_immutable_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let mut node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        for dispatch in [Some(DISPATCH), None] {
            for (edit, nonce, root) in EDITS {
                node = rebind(node, &rig, dispatch).await;
                node.start(51, "turn-a", dispatch).await;
                test_queue::queue_original(&node.shared, channel()).await;
                let untouched = node.effects(&rig).await;
                let flag = ran();
                probe_before_selection(rewrite(&node, edit, flag.clone()));
                let (status, reply) = node.cancel().await;
                assert!(flag.load(Ordering::SeqCst));
                let refused = (StatusCode::CONFLICT, result("refused", IDENTITY));
                assert_eq!(termination(status, &reply), refused, "{dispatch:?} {reply}");
                let row = Some((herdr::HostedState::Bound, nonce.to_string()));
                assert_eq!(node.effects(&rig).await, Effects { row, ..untouched });
                let moved = recorded_root(&node, nonce).await;
                assert!(root.is_empty() || moved == root, "{moved}");
                runs(&node, "turn-a").await;
            }
        }

        // A forwarded cancel: the receiver's own selection read is where the identity starts.
        node = rebind(node, &rig, Some(DISPATCH)).await;
        let owner = Some(herdr::NODE);
        node.sql(
            "UPDATE sessions SET instance_id = $2 WHERE session_key = $1",
            owner,
        )
        .await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let untouched = node.effects(&rig).await;
        let flag = ran();
        probe_before_selection(rewrite(&node, NONCE_SWAP, flag.clone()));
        let uri = format!("/turns/{CHANNEL}/cancel");
        let (status, reply) = forwarded(&node.app(), &uri, json!({"force": true})).await;
        assert!(
            flag.load(Ordering::SeqCst),
            "the receiver's selection was reached"
        );
        let refused = (StatusCode::CONFLICT, result("refused", IDENTITY));
        assert_eq!(termination(status, &reply), refused, "{reply}");
        let row = Some((herdr::HostedState::Bound, SUCCESSOR_NONCE.to_string()));
        assert_eq!(node.effects(&rig).await, Effects { row, ..untouched });
        runs(&node, "turn-a").await;

        // No dispatch selected is an expectation too: a dispatched successor turn is refused.
        node = rebind(node, &rig, None).await;
        node.start(51, "turn-a", None).await;
        let untouched = node.effects(&rig).await;
        let flag = ran();
        probe_before_selection(successor_takes_over(&node, flag.clone()));
        let (status, reply) = node.cancel().await;
        assert!(flag.load(Ordering::SeqCst));
        let refused = result("refused", "captured turn runs another dispatch");
        assert_eq!(
            termination(status, &reply),
            (StatusCode::CONFLICT, refused),
            "{reply}"
        );
        let session = active(Some(SUCCESSOR));
        assert_eq!(
            node.effects(&rig).await,
            Effects {
                session,
                ..untouched
            }
        );
        runs(&node, "turn-b").await;

        // The production order: A's turn ends, A retires, B launches fresh and runs a turn.
        node = rebind(node, &rig, None).await;
        node.start(51, "turn-a", None).await;
        let untouched = node.effects(&rig).await;
        let (flag, launched) = (ran(), Arc::new(Mutex::new(None)));
        probe_before_selection(successor_launched(&node, flag.clone(), launched.clone()));
        let (status, reply) = node.cancel().await;
        assert!(flag.load(Ordering::SeqCst));
        let b = launched.lock().unwrap().take().expect("B launched");
        assert_ne!(b, herdr::EXECUTION);
        let refused = (StatusCode::CONFLICT, result("refused", IDENTITY));
        assert_eq!(termination(status, &reply), refused, "{reply}");
        let row = Some((herdr::HostedState::Bound, b));
        let after = node.effects(&rig).await;
        // Only the launch released A's hold; B's pane got no close and its row is still Bound.
        let kept = Effects {
            row,
            held: false,
            ..untouched
        };
        assert_eq!((after.closes(), after), (0, kept));
        runs(&node, "turn-b").await;
        node.finish().await;
    });
}

// A force-kill retires only the execution its own read selected, retry on: a later nonce or owner
// on the same row, a forwarded force-kill's receiver read and a dispatched successor turn after a
// no-dispatch read are refused before any close, with no retry and the running turn kept.
#[test]
fn e08_initial_execution_identity_is_immutable_pg() {
    let Some((rig, runtime)) = rig("e08_initial_execution_identity_is_immutable_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let undispatched = "UPDATE sessions SET active_dispatch_id = $2 WHERE session_key = $1";
        let mut node = Node::bind_e08(&rig, herdr::HostedState::Bound).await;
        for dispatch in [Some(DISPATCH), None] {
            for (edit, nonce, root) in EDITS {
                node = Node::again_e08(node, &rig).await;
                node.sql(undispatched, dispatch).await;
                node.start(51, "turn-a", dispatch).await;
                let untouched = node.killed(&rig).await;
                let flag = ran();
                probe_before_selection(rewrite(&node, edit, flag.clone()));
                let (status, reply) = node.kill(true).await;
                assert!(flag.load(Ordering::SeqCst));
                let refused = (StatusCode::CONFLICT, result("refused", IDENTITY));
                let answered = (status, reply["termination"].clone());
                assert_eq!(answered, refused, "{dispatch:?} {reply}");
                let row = Some((herdr::HostedState::Bound, nonce.to_string()));
                assert_eq!(node.killed(&rig).await, Killed { row, ..untouched });
                let moved = recorded_root(&node, nonce).await;
                assert!(root.is_empty() || moved == root, "{moved}");
                runs(&node, "turn-a").await;
            }
        }

        // A forwarded force-kill: the receiver's own read is where the identity starts.
        node = Node::again_e08(node, &rig).await;
        let owner = Some(herdr::NODE);
        node.sql(
            "UPDATE sessions SET instance_id = $2 WHERE session_key = $1",
            owner,
        )
        .await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let untouched = node.killed(&rig).await;
        let flag = ran();
        probe_before_selection(rewrite(&node, NONCE_SWAP, flag.clone()));
        let body = json!({"retry": true, "reason": "e08 test"});
        let (status, reply) = forwarded(&node.app_on(herdr::NODE), &path(&node.key), body).await;
        assert!(
            flag.load(Ordering::SeqCst),
            "the receiver's selection was reached"
        );
        let refused = (StatusCode::CONFLICT, result("refused", IDENTITY));
        assert_eq!((status, reply["termination"].clone()), refused, "{reply}");
        let row = Some((herdr::HostedState::Bound, SUCCESSOR_NONCE.to_string()));
        assert_eq!(node.killed(&rig).await, Killed { row, ..untouched });
        runs(&node, "turn-a").await;

        // No dispatch selected is an expectation too: a dispatched successor turn is refused.
        node = Node::again_e08(node, &rig).await;
        node.sql(undispatched, None).await;
        node.start(51, "turn-a", None).await;
        let untouched = node.killed(&rig).await;
        let flag = ran();
        probe_before_selection(successor_takes_over(&node, flag.clone()));
        let (status, reply) = node.kill(true).await;
        assert!(flag.load(Ordering::SeqCst));
        let refused = result("refused", "captured turn runs another dispatch");
        let answered = (status, reply["termination"].clone());
        assert_eq!(answered, (StatusCode::CONFLICT, refused), "{reply}");
        let session = kept("turn_active", Some(SUCCESSOR));
        assert_eq!(
            node.killed(&rig).await,
            Killed {
                session,
                ..untouched
            }
        );
        runs(&node, "turn-b").await;
        node.finish().await;
    });
}

/// An HTTP expectation of the rig's own owner and no dispatch.
pub(crate) fn unbound(rig: &herdr::P11TerminationRig, node: &Node) -> Expected {
    let owner = rig.record(&node.shared.token_hash, &node.key).owner;
    Expected {
        owner,
        dispatch: None,
    }
}

/// The selected row read as a known record.
async fn observed(pool: &sqlx::PgPool, key: &str) -> (HostedObservation, HostedExecution) {
    let lookup = hosted::load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(key)).await;
    let HostedLookup::Found(observed) = lookup else {
        panic!("{lookup:?}");
    };
    let HostedRecord::Known(record) = observed.record.clone() else {
        panic!("{observed:?}");
    };
    (observed, record)
}

/// A probe in which A's turn ends, its execution retires, B launches fresh on the row through the
/// production launch and binds, and B's turn starts with no dispatch; `launched` gets B's nonce.
fn successor_launched(
    node: &Node,
    flag: Arc<AtomicBool>,
    launched: Arc<Mutex<Option<String>>>,
) -> SettlementWindowProbe {
    let (shared, pool, key, app) = (
        node.shared.clone(),
        node.pool.clone(),
        node.key.clone(),
        node.app(),
    );
    Box::new(move || {
        Box::pin(async move {
            let a = test_turn::active(&shared, channel()).await.unwrap();
            test_turn::finish_naturally(&shared, channel(), 51, "turn-a", &a).await;
            let (row, record) = observed(&pool, &key).await;
            let (owner, nonce) = (&record.owner, &record.execution_nonce);
            let retired = hosted::retire_pg(&pool, &row, owner, nonce).await;
            assert_eq!(retired, Ok(HostedCasOutcome::Written));
            let b = launch_successor(&pool, &record).await;
            let (row, _) = observed(&pool, &key).await;
            let bound = hosted::bind_pg(&pool, &row, owner, &b).await;
            assert_eq!(bound, Ok(HostedCasOutcome::Written));
            let marker = json!({"session_key": key, "status": "turn_active", "provider": "claude",
                "channel_id": CHANNEL.to_string(), "turn_start_nonce": "turn-b"});
            let bearer = format!("Bearer {AUTH}");
            let headers = [("authorization", bearer.as_str()), JSON];
            let uri = "/dispatched-sessions/webhook";
            let post = crate::server::routes::home_stop_test_post;
            let (status, reply) = post(&app, uri, &headers, &marker.to_string()).await;
            assert!(status.is_success(), "{status}: {reply}");
            test_turn::start(&shared, channel(), 61, "turn-b").await;
            *launched.lock().unwrap() = Some(b);
            flag.store(true, Ordering::SeqCst);
        })
    })
}

/// B's fresh launch over `a`'s retired row on `a`'s endpoint, for the same owner.
async fn launch_successor(pool: &sqlx::PgPool, a: &HostedExecution) -> String {
    let location = a.location.as_ref().expect("a recorded pane");
    let cwd = crate::config::runtime_root().unwrap().join("successor-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let prepare = |prepared: &PreparedIncarnation| {
        let config = crate::services::claude_tui::session::ClaudeTuiLaunchConfig {
            tmux_session_name: prepared.context.tmux_session.clone(),
            working_dir: cwd.clone(),
            claude_bin: crate::services::claude_command::ClaudeBinary::from_tmux_wrapper_argv(
                "/usr/local/bin/claude",
            ),
            agentdesk_exe: std::path::PathBuf::from("/usr/local/bin/agentdesk"),
            hook_endpoint: "http://127.0.0.1:1".into(),
            session_id: session.clone(),
            system_prompt: None,
            model: None,
            resume: false,
        };
        let files = crate::services::claude_tui::session::prepare_claude_tui_launch(&config)?;
        Ok(HerdrLaunchCommand {
            cwd: cwd.clone(),
            command: format!("bash {}", files.launch_script_path.display()),
        })
    };
    let launch = HerdrLaunch {
        endpoint: Some(HerdrLaunchEndpoint {
            execution_node: location.execution_node.clone(),
            config_key: location.endpoint_config_key.clone(),
            socket_addr: location.socket_addr.clone(),
            herdr_session: location.named_session.clone(),
        }),
        owner: a.owner.clone(),
        channel_id: Some(CHANNEL),
        expected_native_session_id: Some(session.clone()),
        resume: false,
        provider_root: None,
    };
    let outcome = launch_herdr_session(pool, launch, prepare, Arc::new(Successor)).await;
    let Ok(HerdrLaunchOutcome::Launched {
        execution_nonce,
        evidence: Ok(()),
        ..
    }) = outcome
    else {
        panic!("{outcome:?}");
    };
    execution_nonce
}

/// B's launch host: resume-on-restore off, one created pane and its evidence.
struct Successor;

impl HerdrLaunchHost for Successor {
    fn restore_resume(&self, _: &HerdrLaunchEndpoint) -> RestoreResume {
        RestoreResume::Off {
            witness: ServerWitness::for_test(1),
        }
    }

    fn create(&self, _: &HerdrCreateRequest) -> HerdrCreateOutcome {
        HerdrCreateOutcome::Created {
            pane_id: herdr::LAUNCHED_PANE.into(),
        }
    }

    fn launch_evidence(&self, probe: &EvidenceProbe) -> Result<ExpectedExecution, EvidenceGap> {
        Ok(hosted::tests::expected(&probe.execution_nonce, 300))
    }
}
