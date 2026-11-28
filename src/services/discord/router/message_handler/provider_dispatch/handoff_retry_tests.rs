use super::super::super::owner_input::herdr_tests::{
    HerdrOwner, ObservedRegistration as Registration, live_source,
};
use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    self as hosted, HostedObservation, HostedState,
};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::codex_tui::busy_inject::{TurnVerdict, read_turn};
use crate::services::discord::queue_io::input_owner::origin::InputOrigin;
use crate::services::discord::queue_io::input_owner::{claude_handoff, notice, target};
use crate::services::session_host::herdr_socket_rig_tests::NODE;
use crate::services::tui_prompt_dedupe::{
    TuiRuntimeBinding, clear_tmux_runtime_binding, register_tmux_runtime_binding,
};
use crate::services::turn_host::{HerdrTurnPlan, TurnHost};
use serde_json::Value;

#[derive(Clone, Default)]
pub(super) struct Trace(pub(super) Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for Trace {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
async fn state(mailbox: &ChannelMailboxHandle) -> Value {
    mailbox
        .v2_request(|slot| slot.as_ref().ok_or("owner")?.json())
        .await
        .unwrap()
}
async fn grant(mailbox: &ChannelMailboxHandle) -> Front {
    mailbox
        .v2_request(|slot| {
            let owner = slot.as_mut().ok_or("owner")?;
            let front = owner.front().ok_or("front")?;
            owner.grant_front(&front)?;
            Ok(front)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn expired_launch_deadline_keeps_backoff_on_a_prepare_refusal() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Claude;
    let channel = ChannelId::new(8_146_002);
    // The journal keeps its custody only while boot config still routes the channel to Herdr.
    let _hosts = crate::config::session_hosts::force_for_test(Some("n"), &[(channel.get(), "n")]);
    let mailbox = shared.mailbox(channel);
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None);
    let (path, namespace) =
        crate::services::turn_orchestrator::v2_queue(&persistence, channel).unwrap();
    mailbox.v2_adopt(path, namespace).await.unwrap();
    let original: PendingQueueItem = serde_json::from_value(json!({
        "author_id":7,"message_id":8146012,"channel_id":channel.get(),"queued_generation":1,"text":"held original"
    })).unwrap();
    mailbox
        .v2_admit_ticket(original, "expired-deadline".into())
        .await
        .unwrap();
    let front = grant(&mailbox).await;
    let begin = front.clone();
    mailbox
        .v2_request(move |slot| {
            let owner = slot.as_mut().unwrap();
            let journal = owner.begin_handoff(&begin, "owned".into(), "key".into())?;
            owner.handoff_ended(&journal.id)?;
            owner
                .snapshot
                .unfinished_handoff
                .as_mut()
                .unwrap()
                .retry_after_ms = 1;
            Ok(())
        })
        .await
        .unwrap();
    let (wake, mut woke) = tokio::sync::mpsc::unbounded_channel();
    counted::set_owner_wake_for_tests(
        channel,
        Some(Arc::new(move || {
            let _ = wake.send(());
        })),
    );
    let _scope = Scope(channel);
    let started = Instant::now();
    assert!(
        claude_handoff::prepare(
            Some(&shared),
            &mailbox,
            &ProviderKind::Claude,
            channel.get(),
            &front,
            claude_handoff::Start::Person
        )
        .await
        .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), woke.recv())
            .await
            .is_err(),
        "an expired launch deadline must not turn another hold into an immediate retry"
    );
    tokio::time::timeout(Duration::from_secs(5), woke.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(1));
    let snapshot = state(&mailbox).await;
    assert_eq!(snapshot["unfinished_handoff"]["failures"], 1);
    assert_eq!(snapshot["items"].as_array().unwrap().len(), 1);
    assert!(snapshot["retained_inputs"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn restored_ended_retired_launch_retries_backoff_then_retains_and_notifies_once_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Claude;
    let channel = ChannelId::new(8_146_001);
    let message = 8_146_011;
    let logical = "handoff-launch-retry";
    let key = format!("claude/{}/test-node:{logical}", shared.token_hash);
    let mut owner = hosted::test_support::owner(&channel.get().to_string());
    owner.discord_token_hash = shared.token_hash.clone();
    owner.logical_key = logical.into();
    let record = hosted::test_support::record(&owner, "retired-nonce", HostedState::Retired);
    sqlx::query("INSERT INTO sessions (session_key,provider,status,identity_kind,discord_token_hash,channel_id,hosted_execution) VALUES ($1,'claude','idle','discord_channel',$2,$3,$4)")
        .bind(&key).bind(&shared.token_hash).bind(channel.to_string()).bind(serde_json::to_value(record).unwrap()).execute(&pool).await.unwrap();
    let hosted::HostedLookup::Found(row) =
        hosted::load_hosted_execution_pg(&pool, hosted::HostedLookupKey::SessionKey(&key)).await
    else {
        panic!("retired row")
    };
    let _host = claude_handoff::tests::force_host(channel.get(), key.clone(), row);
    let mailbox = shared.mailbox(channel);
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None);
    let (path, namespace) =
        crate::services::turn_orchestrator::v2_queue(&persistence, channel).unwrap();
    mailbox
        .v2_adopt(path.clone(), namespace.clone())
        .await
        .unwrap();
    let original: PendingQueueItem = serde_json::from_value(json!({"author_id":7,"message_id":message,"channel_id":channel.get(),"queued_generation":7,"text":"launch original, exact bytes"})).unwrap();
    mailbox
        .v2_admit_ticket(original.clone(), "handoff-original".into())
        .await
        .unwrap();
    let first = grant(&mailbox).await;
    let raw = mailbox
        .v2_request(move |slot| {
            let owner = slot.as_mut().unwrap();
            let journal = owner.begin_handoff(&first, logical.into(), key)?;
            owner.handoff_ended(&journal.id)?;
            owner.json()
        })
        .await
        .unwrap();
    // Restore exactly the older ended journal format, with no deadline field.
    let mut old = raw;
    old["unfinished_handoff"]
        .as_object_mut()
        .unwrap()
        .remove("retry_after_ms");
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    mailbox
        .v2_adopt(path.clone(), namespace.clone())
        .await
        .unwrap();
    let (wake, mut woke) = tokio::sync::mpsc::unbounded_channel();
    counted::set_owner_wake_for_tests(
        channel,
        Some(Arc::new(move || {
            let _ = wake.send(());
        })),
    );
    let _scope = Scope(channel);
    let trace = Trace::default();
    let writer = trace.clone();
    crate::logging::test_capture::pin_callsite_interest();
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish(),
    );
    for attempt in 1..=5 {
        let front = grant(&mailbox).await;
        assert_eq!(front.1.message_id, message);
        assert_eq!(
            claude_handoff::prepare(
                Some(&shared),
                &mailbox,
                &ProviderKind::Claude,
                channel.get(),
                &front,
                claude_handoff::Start::Person
            )
            .await,
            Ok(true)
        );
        let claim = claim_turn(&mailbox, message, &front)
            .await
            .unwrap()
            .unwrap();
        assert!(
            claim.followers().is_empty(),
            "a new suffix never inherits the original's failure budget"
        );
        let duplicate = Failure::claimed(&mailbox, &claim);
        let before = chrono::Utc::now().timestamp_millis().max(0) as u64;
        assert_eq!(
            Failure::claimed(&mailbox, &claim)
                .commit(
                    Kind::Opaque,
                    "herdr launch auth_overlay: forced launch failure".into(),
                    Ok(None),
                    Some(shared.clone()),
                    channel,
                    None
                )
                .await
                .unwrap(),
            Some(attempt == 5)
        );
        assert_eq!(
            duplicate
                .commit(
                    Kind::Opaque,
                    "late duplicate".into(),
                    Ok(None),
                    Some(shared.clone()),
                    channel,
                    None
                )
                .await
                .unwrap(),
            None
        );
        let snapshot = state(&mailbox).await;
        if attempt == 5 {
            assert!(snapshot["unfinished_handoff"].is_null());
            assert_eq!(snapshot["retained_inputs"].as_array().unwrap().len(), 1);
            assert_eq!(
                snapshot["retained_inputs"][0]["input"]["original"],
                serde_json::to_value(&original).unwrap()
            );
            assert_eq!(
                snapshot["retained_inputs"][0]["reason"]["start_failure"]["failures"],
                5
            );
            assert_eq!(
                snapshot["retained_inputs"][0]["disposition"],
                "known_not_sent"
            );
            break;
        }
        assert_eq!(snapshot["unfinished_handoff"]["failures"], attempt);
        assert_eq!(
            snapshot["items"][0],
            serde_json::to_value(&original).unwrap()
        );
        assert!(snapshot["retained_inputs"].as_array().unwrap().is_empty());
        let due = snapshot["unfinished_handoff"]["retry_after_ms"]
            .as_u64()
            .unwrap();
        let delay = 1_000u64 << attempt;
        assert!(due >= before + delay);
        let front = grant(&mailbox).await;
        assert!(
            claude_handoff::prepare(
                Some(&shared),
                &mailbox,
                &ProviderKind::Claude,
                channel.get(),
                &front,
                claude_handoff::Start::Person
            )
            .await
            .is_err(),
            "an unrelated early wake cannot launch"
        );
        if attempt == 1 {
            let mut following = original.clone();
            following.message_id += 1;
            following.text = "following input".into();
            mailbox
                .v2_admit_ticket(following, "handoff-following".into())
                .await
                .unwrap();
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), woke.recv())
                .await
                .is_err(),
            "no immediate wake on launch failure"
        );
        tokio::time::timeout(Duration::from_millis(delay + 2_000), woke.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(chrono::Utc::now().timestamp_millis().max(0) as u64 >= due);
        // A process restart must preserve the failure budget and deadline.
        mailbox
            .v2_adopt(path.clone(), namespace.clone())
            .await
            .unwrap();
    }
    let snapshot = state(&mailbox).await;
    let retained = snapshot["retained_inputs"][0].clone();
    assert_eq!(
        notice::drain_synchronously_for_tests(&pool, &mailbox, "claude", channel.get())
            .await
            .unwrap(),
        1
    );
    crate::services::discord::queue_io::retained_notice::enqueue(
        &pool,
        "claude",
        channel.get(),
        &namespace,
        &retained,
    )
    .await
    .unwrap();
    assert_eq!(
        notice::drain_synchronously_for_tests(&pool, &mailbox, "claude", channel.get())
            .await
            .unwrap(),
        0
    );
    let posts: Vec<String> = sqlx::query_scalar(
        "SELECT content FROM message_outbox WHERE target=$1 AND source='retained_input_notice'",
    )
    .bind(format!("channel:{channel}"))
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(posts.len(), 1);
    assert!(posts[0].contains("다음 입력은 계속 처리합니다"));
    assert_eq!(
        state(&mailbox).await["retained_inputs"][0]["notice"]["enqueued"],
        true
    );
    let front = grant(&mailbox).await;
    assert_eq!(front.1.message_id, message + 1);
    assert_eq!(
        claude_handoff::prepare(
            Some(&shared),
            &mailbox,
            &ProviderKind::Claude,
            channel.get(),
            &front,
            claude_handoff::Start::Person
        )
        .await,
        Ok(false),
        "Retired starts no new migration journal"
    );
    let claim = claim_turn(&mailbox, message + 1, &front)
        .await
        .unwrap()
        .unwrap();
    mailbox
        .v2_request(move |slot| {
            slot.as_mut()
                .unwrap()
                .complete_claimed_locally((claim.front(), claim.number()), 0)
        })
        .await
        .unwrap();
    let snapshot = state(&mailbox).await;
    assert!(snapshot["items"].as_array().unwrap().is_empty());
    assert_eq!(snapshot["retained_inputs"].as_array().unwrap().len(), 1);
    let log = String::from_utf8(trace.0.lock().unwrap().clone()).unwrap();
    assert!(log.contains("channel_id=8146001"), "{log}");
    assert!(log.contains("front_id=8146011"), "{log}");
    assert!(log.contains("stage=\"provider_start\""), "{log}");
    assert!(log.contains("auth_overlay: forced launch failure"), "{log}");
    pool.close().await;
    db.drop().await;
}

/// A rollout whose model turn `turn` still runs.
fn rollout(dir: &std::path::Path, native: &str, turn: &str) -> std::path::PathBuf {
    let path = dir.join(format!("rollout-2026-11-26T09-13-34-{native}.jsonl"));
    let event = |payload: Value| format!("{}\n", json!({"type":"event_msg","payload":payload}));
    let context = json!({"type":"turn_context","payload":{"turn_id":turn,
        "sandbox_policy":{"type":"danger-full-access"}}});
    let body = event(json!({"type":"task_started","turn_id":turn,"root_turn_id":turn}))
        + &format!("{context}\n")
        + &event(json!({"type":"item_completed","turn_id":turn,"item":{"type":"UserMessage"}}));
    std::fs::write(&path, body).unwrap();
    path
}

fn bind(logical: &str, kind: RuntimeHandoffKind, path: &std::path::Path, native: Option<&str>) {
    let binding = TuiRuntimeBinding {
        runtime_kind: kind,
        output_path: path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: native.map(str::to_owned),
        last_offset: 0,
        relay_last_offset: None,
    };
    register_tmux_runtime_binding(logical, binding);
}

/// The row a turn on `logical` holds as intake seeded it; on Herdr it names no path.
fn row(logical: &str, output: Option<String>) -> InflightTurnState {
    let text = "turn a".to_string();
    let native = Some("row-native".to_string());
    let (name, tmux) = (Some("adk-cdx".to_string()), Some(logical.to_string()));
    InflightTurnState::new(
        ProviderKind::Codex,
        8_426,
        name,
        7,
        1,
        0,
        text,
        native,
        tmux,
        output,
        None,
        0,
    )
}

fn host(owner: &HerdrOwner, provider: &str, state: HostedState) -> TurnHost {
    let row = Some(HostedObservation::known_fixture(
        owner.record(provider, state),
    ));
    TurnHost::Herdr(Box::new(HerdrTurnPlan {
        endpoint: owner.endpoint(),
        row,
        bootstrap: false,
    }))
}

fn source(
    provider: ProviderKind,
    host: &TurnHost,
    row: &InflightTurnState,
) -> Option<(String, Option<String>)> {
    let session = row.tmux_session_name.clone().unwrap_or_default();
    live_source(provider, host, &session, row)
}

/// Clear, first turn, busy input on Herdr: the binding a clear left (A) is never read while the
/// row is Pending, and the Bound row's front registers the new rollout B.
#[test]
fn a_bound_codex_herdr_front_reads_the_rollout_its_attach_proved() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = HerdrOwner::start("codex", 8_426_101);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(owner.channel, NODE)]);
    let _registry = owner.rig.registry_on_this_thread();
    let _entered = owner.runtime.enter();
    let (old_native, new_native) = ("aaaa-8426", "bbbb-8426");
    let old = rollout(owner._dir.path(), old_native, "turn-a");
    let new = rollout(owner._dir.path(), new_native, "turn-b");
    let inflight = row(&owner.logical, None);
    let kind = ("codex", InputOrigin::Discord, true);
    // Contract GREEN before and after: a Pending row is Absent, so the front is never handed in.
    bind(
        &owner.logical,
        RuntimeHandoffKind::CodexTui,
        &old,
        Some(old_native),
    );
    let pending = host(&owner, "codex", HostedState::Pending);
    assert_eq!(source(ProviderKind::Codex, &pending, &inflight), None);
    let at = (&pending, owner.logical.as_str(), None);
    let registered = Registration::observed(owner.mailbox.clone(), kind, at, &owner.original, None);
    assert_eq!(registered.unwrap().phase, target::Phase::Absent);
    // RED before the fix: Codex took only the empty row path, so the transcript was "" and the
    // busy route answered NotSent("busy_turn: transcript unavailable").
    bind(
        &owner.logical,
        RuntimeHandoffKind::CodexTui,
        &new,
        Some(new_native),
    );
    let bound = host(&owner, "codex", HostedState::Bound);
    let chosen = source(ProviderKind::Codex, &bound, &inflight);
    assert_eq!(
        chosen,
        Some((new.display().to_string(), Some(new_native.to_string())))
    );
    let live = chosen
        .as_ref()
        .map(|(path, native)| (path.as_str(), native.as_deref()));
    let at = (&bound, owner.logical.as_str(), live);
    let registered = Registration::observed(owner.mailbox.clone(), kind, at, &owner.original, None);
    let registered = registered.unwrap();
    assert_eq!(registered.phase, target::Phase::Busy);
    assert_eq!(registered.transcript, new, "CodexInput.rollout is B");
    assert_eq!(registered.attempt["source"]["session_id"], new_native);
    let turn = Some(TurnVerdict::KnownModelTurn("turn-b".into()));
    assert_eq!(read_turn(&registered.transcript), turn);
    clear_tmux_runtime_binding(&owner.logical);
}

/// Contract GREEN before and after: tmux has no attach proof, so its row path stands and a
/// pathless row (the clear window) stays without a source.
#[test]
fn a_tmux_codex_front_keeps_its_row_path() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let logical = "AgentDesk-codex-busy-source-tmux-8426";
    bind(
        logical,
        RuntimeHandoffKind::CodexTui,
        &rollout(dir.path(), "cccc-8426", "turn-c"),
        Some("cccc-8426"),
    );
    assert_eq!(
        source(ProviderKind::Codex, &TurnHost::Tmux, &row(logical, None)),
        None
    );
    let path = dir.path().join("row.jsonl").display().to_string();
    let seeded = row(logical, Some(path.clone()));
    let kept = Some((path, Some("row-native".to_string())));
    assert_eq!(source(ProviderKind::Codex, &TurnHost::Tmux, &seeded), kept);
    clear_tmux_runtime_binding(logical);
}

/// Contract GREEN before and after: without a proved CodexTui source the Bound front reads no
/// turn (NotSent "busy_turn: transcript unavailable"), and a Claude front never takes it.
#[test]
fn a_bound_herdr_front_without_a_proved_codex_source_reads_no_turn() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = HerdrOwner::start("codex", 8_426_103);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(owner.channel, NODE)]);
    let _registry = owner.rig.registry_on_this_thread();
    let _entered = owner.runtime.enter();
    let path = rollout(owner._dir.path(), "dddd-8426", "turn-d");
    let inflight = row(&owner.logical, None);
    let bound = host(&owner, "codex", HostedState::Bound);
    clear_tmux_runtime_binding(&owner.logical);
    assert_eq!(
        source(ProviderKind::Codex, &bound, &inflight),
        None,
        "no binding"
    );
    let unproved = [
        (RuntimeHandoffKind::CodexTui, None),
        (RuntimeHandoffKind::ClaudeTui, Some("dddd-8426")),
    ];
    for (kind, native) in unproved {
        bind(&owner.logical, kind, &path, native);
        assert_eq!(
            source(ProviderKind::Codex, &bound, &inflight),
            None,
            "{kind:?} {native:?}"
        );
    }
    bind(
        &owner.logical,
        RuntimeHandoffKind::CodexTui,
        &path,
        Some("dddd-8426"),
    );
    let claude = host(&owner, "claude", HostedState::Bound);
    assert_eq!(
        source(ProviderKind::Claude, &claude, &inflight),
        None,
        "Claude never reads it"
    );
    let at = (&bound, owner.logical.as_str(), None);
    let kind = ("codex", InputOrigin::Discord, true);
    let registered = Registration::observed(owner.mailbox.clone(), kind, at, &owner.original, None);
    assert_eq!(read_turn(&registered.unwrap().transcript), None);
    clear_tmux_runtime_binding(&owner.logical);
}
