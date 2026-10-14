use super::*;
use crate::services::discord::input_runtime::fence::{self, Gate, Mode};
use crate::services::turn_orchestrator::installed_actor_handoff::{HandoffOutcome, Intent};
use crate::services::turn_orchestrator::pending_queue_persistence::{
    QueueExitDebt, pending_queue_file_path, pending_queue_item_from_intervention, post_save_fault,
    read_back_fault, save_fault,
};
use crate::services::turn_orchestrator::policy_close::{
    PolicyAuthority, PolicyClear, PolicyCloseRequest, PolicyMailboxOutcome, PolicyReceipt,
    PolicyRetain, test_hooks,
};
use crate::services::turn_orchestrator::queued_head::{
    SnapshotMeta, parse_snapshot, snapshot_json,
};
use crate::services::turn_orchestrator::registry_purge::MailboxPurgeOutcome;
use std::sync::atomic::AtomicUsize;
use std::time::SystemTime;

const TOKEN: &str = "b3b1b1-scoped";
const EPISODE: &str = "episode-b3b1b1";

fn ctx() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, TOKEN, None)
}

fn human(id: u64) -> Intervention {
    let source = SourceMessageQueuedGeneration::user_instruction(MessageId::new(id), 5);
    Intervention {
        author_id: UserId::new(id),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 5,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: vec![source],
        source_text_segments: Vec::new(),
        text: format!("input {id}"),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

fn proof(channel: ChannelId, message: u64) -> Installation {
    let nonce = format!("nonce-{channel}-{message}");
    Installation {
        channel,
        token: Arc::new(CancelToken::from_persisted_turn_nonce(Some(nonce))),
        episode: EPISODE.into(),
        birth: "birth".into(),
        owner: UserId::new(7),
        message: MessageId::new(message),
        generation: 1,
        current: Arc::new(AtomicU64::new(1)),
    }
}

fn bytes(channel: ChannelId) -> Option<Vec<u8>> {
    let path = pending_queue_file_path(&ProviderKind::Claude, TOKEN, channel).unwrap();
    std::fs::read(path).ok()
}

fn disk(channel: ChannelId) -> serde_json::Value {
    serde_json::from_slice(&bytes(channel).expect("queue file")).unwrap()
}

fn disk_debts(channel: ChannelId) -> Vec<(String, u64)> {
    let debts = disk(channel)["queue_exit_debts"].as_array().cloned();
    let row = |d: &serde_json::Value| {
        let id = d["event_id"].as_str().unwrap().to_string();
        (id, d["intervention"]["message_id"].as_u64().unwrap())
    };
    debts.unwrap_or_default().iter().map(row).collect()
}

/// A spawned actor whose canonical queue file holds `queued`, written by the actor.
async fn actor(registry: &ChannelMailboxRegistry, channel: u64, queued: &[u64]) -> Fixture {
    let channel = ChannelId::new(channel);
    let handle = registry.handle(channel);
    for id in queued {
        assert!(handle.enqueue(human(*id), ctx()).await.enqueued);
    }
    Fixture { channel, handle }
}

struct Fixture {
    channel: ChannelId,
    handle: ChannelMailboxHandle,
}

/// An actor that hydrated `queued` from a file already holding a debt for message 299, and the
/// snapshot with that same metadata and no items.
async fn with_debt(
    registry: &ChannelMailboxRegistry,
    channel: u64,
    queued: &[u64],
) -> (Fixture, Vec<u8>) {
    let fx = actor(registry, channel, &[]).await;
    let event = QueueExitEvent {
        intervention: human(299),
        kind: QueueExitKind::Superseded,
    };
    let meta = SnapshotMeta {
        queue_exit_debts: vec![QueueExitDebt::new(EPISODE, &event, fx.channel)],
        ..SnapshotMeta::default()
    };
    let (wall, now) = (SystemTime::now(), Instant::now());
    let item = |id| pending_queue_item_from_intervention(&human(id), fx.channel, None, wall, now);
    let items = queued.iter().map(|id| item(*id)).collect();
    let path = pending_queue_file_path(&ProviderKind::Claude, TOKEN, fx.channel).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, snapshot_json(items, Some(&meta)).unwrap()).unwrap();
    let hydrated = fx.handle.hydrate_pending_queue_from_disk(ctx()).await;
    assert_eq!(hydrated.absorbed, queued.len());
    (
        fx,
        snapshot_json(Vec::new(), Some(&meta)).unwrap().into_bytes(),
    )
}

/// Rewrites the published head's text or its source's generation, leaving its identity.
fn retouch(path: std::path::PathBuf, field: &'static str) -> impl FnOnce() + Send {
    move || {
        let mut file: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let head = &mut file["items"][0];
        match field {
            "text" => head["text"] = "retouched".into(),
            _ => head["source_message_queued_generations"][0]["queued_generation"] = 9.into(),
        }
        std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
    }
}

/// Everything a refused claim must leave as it was: file bytes, queue, slot and pending record.
#[derive(Debug, PartialEq)]
struct Seen {
    bytes: Option<Vec<u8>>,
    queue: Vec<u64>,
    slot: Option<usize>,
    pending: Option<(String, u64, Vec<String>, PendingFeedback, PendingDriver)>,
}

impl Fixture {
    async fn claim(&self, proof: &Installation) -> ScopedClaimOutcome {
        let outcome = self.handle.claim_scoped(proof.clone(), ctx()).await;
        outcome.expect("actor answers")
    }

    async fn seen(&self) -> Seen {
        let snapshot = self.handle.snapshot().await;
        let pending = self.handle.installed_pending().await.map(|p| {
            let ids = p.exits.iter().map(|e| e.debt.event_id.clone()).collect();
            (p.episode, p.generation, ids, p.feedback, p.driver)
        });
        Seen {
            bytes: bytes(self.channel),
            queue: (snapshot.intervention_queue.iter())
                .map(|item| item.message_id.get())
                .collect(),
            slot: snapshot.cancel_token.map(|t| Arc::as_ptr(&t) as usize),
            pending,
        }
    }

    async fn installed(&self, proof: &Installation) -> Vec<String> {
        match self.claim(proof).await {
            ScopedClaimOutcome::Installed(ids) => ids,
            ScopedClaimOutcome::Inherited(_) => panic!("inherited"),
            ScopedClaimOutcome::Observed => panic!("observed"),
            ScopedClaimOutcome::Refused(refusal) => panic!("refused: {refusal:?}"),
        }
    }

    async fn close(&self, proof: &Installation, clear: PolicyClear) -> PolicyMailboxOutcome {
        tokio::time::sleep(Duration::from_millis(2)).await;
        let token = &proof.token;
        let request = PolicyCloseRequest {
            actor: Arc::downgrade(token),
            user_message_id: proof.message,
            turn_nonce: token.turn_nonce().unwrap().to_string(),
            interrupt_generation: token.claude_interrupt_generation(),
            observed_before: Instant::now(),
            receipt: Box::new(Receipt(self.channel.get(), clear)),
            active: Arc::new(AtomicUsize::new(1)),
            persistence: ctx(),
            trace: Default::default(),
        };
        // The request carries this thread's armed hook, if any, on its own probe.
        let request = test_hooks::bind(request);
        let outcome = self.handle.commit_policy_close(request).await;
        outcome.unwrap_or_else(|stage| panic!("policy close lost at {stage:?}"))
    }
}

struct Receipt(u64, PolicyClear);

impl PolicyReceipt for Receipt {
    fn channel(&self) -> u64 {
        self.0
    }
    fn try_clear(&self, _authority: &PolicyAuthority<'_>) -> PolicyClear {
        self.1
    }
}

/// The installed actor's own checks alone, with a row check that reads nothing.
async fn handoff(fx: &Fixture, proof: &Installation, intent: Intent) -> HandoffOutcome {
    let pass = || Ok(Box::new(()) as Box<dyn Send>);
    fx.handle
        .handoff_installed(proof.clone(), intent, pass)
        .await
}

/// Each debt's id, issuing episode and the text its event is rebuilt with, as the typed snapshot
/// reader returns it from the file and as the actor's record holds it.
async fn debts_read_and_held(
    fx: &Fixture,
) -> (Vec<(String, String, String)>, Vec<(String, String, String)>) {
    let content = String::from_utf8(bytes(fx.channel).unwrap()).unwrap();
    let (_, meta) = parse_snapshot(&content).unwrap();
    let debts = meta.unwrap().queue_exit_debts;
    let read = debts.iter().map(|debt| {
        (
            debt.event_id.clone(),
            debt.episode.clone(),
            debt.event().intervention.text,
        )
    });
    let record = fx.handle.installed_pending().await.unwrap();
    let held = record.exits.iter().map(|exit| {
        let debt = &exit.debt;
        (
            debt.event_id.clone(),
            debt.episode.clone(),
            exit.event.intervention.text.clone(),
        )
    });
    (read.collect(), held.collect())
}

fn refused(outcome: ScopedClaimOutcome) -> ScopedClaimRefusal {
    match outcome {
        ScopedClaimOutcome::Refused(refusal) => refusal,
        ScopedClaimOutcome::Installed(_) => panic!("installed"),
        ScopedClaimOutcome::Inherited(_) => panic!("inherited"),
        ScopedClaimOutcome::Observed => panic!("observed"),
    }
}

/// Arms the next policy close built on this thread to panic once at `stage`, as a crash after the
/// receipt clear would; the returned flag tells whether that panic was injected.
fn panic_at(stage: Option<test_hooks::Stage>) -> Arc<std::sync::atomic::AtomicBool> {
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    assert!(!test_hooks::disarm(), "stale policy-close hook");
    if let Some(stage) = stage {
        let flag = fired.clone();
        test_hooks::arm(Box::new(move |seen| {
            if seen == stage && !flag.swap(true, Ordering::Relaxed) {
                panic!("injected policy close panic at {seen:?}");
            }
        }));
    }
    fired
}

/// One claim step writes the queue without the message and its debt in a single snapshot, and
/// installs the actor with that debt pending; a save that fails first changes none of the four.
#[tokio::test(flavor = "current_thread")]
async fn scoped_claim_installs_queue_debt_actor_and_pending_together() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let fx = actor(&registry, 5_342_101, &[201, 202]).await;
    let proof = proof(fx.channel, 201);
    let before = fx.seen().await;
    save_fault::fail_next(fx.channel);
    let failed = refused(fx.claim(&proof).await);
    assert!(
        matches!(failed, ScopedClaimRefusal::NotCommitted(_)),
        "{failed:?}"
    );
    assert!(
        failed
            .detail()
            .unwrap()
            .contains("injected pending queue save failure")
    );
    assert_eq!(fx.seen().await, before);
    assert!(before.slot.is_none() && before.pending.is_none());

    let ids = fx.installed(&proof).await;
    let after = fx.seen().await;
    assert_eq!(disk(fx.channel)["items"].as_array().unwrap().len(), 1);
    assert_eq!(disk(fx.channel)["items"][0]["message_id"], 202);
    assert_eq!(disk_debts(fx.channel), [(ids[0].clone(), 201)]);
    assert_eq!(after.queue, [202]);
    assert_eq!(after.slot, Some(Arc::as_ptr(&proof.token) as usize));
    let pending = (
        EPISODE.to_string(),
        1,
        ids,
        PendingFeedback::Owed,
        PendingDriver::NotScheduled,
    );
    assert_eq!(after.pending, Some(pending));
}

/// A claim whose caller already left installs nothing, so no actor exists without its pending
/// record and the file keeps its bytes.
#[tokio::test(flavor = "current_thread")]
async fn scoped_claim_with_a_dropped_reply_leaves_no_actor() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let fx = actor(&registry, 5_342_201, &[211, 212]).await;
    let before = fx.seen().await;
    let (reply, answer) = oneshot::channel();
    drop(answer);
    let msg = ScopedMsg::Claim {
        proof: proof(fx.channel, 211),
        persistence: ctx(),
        reply,
    };
    fx.handle
        .sender
        .send((ChannelMailboxMsg::ScopedClaim(msg), None))
        .unwrap_or_else(|_| panic!("actor gone"));
    assert_eq!(fx.seen().await, before);
}

/// On a protected channel the step runs off the actor task; a caller that leaves after the last
/// check gets no answer, and the claim changed either nothing or the actor and its record together.
#[tokio::test(flavor = "current_thread")]
async fn scoped_claim_whose_caller_leaves_mid_commit_installs_all_or_nothing() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    for (n, committed) in [false, true].into_iter().enumerate() {
        let fx = actor(&registry, 5_342_161 + n as u64, &[311, 312]).await;
        let gate = Gate::protect(ProviderKind::Claude, fx.channel.get()).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        let proof = proof(fx.channel, 311);
        let before = fx.seen().await;
        let (reply, answer) = oneshot::channel();
        let (path, old) = (
            pending_queue_file_path(&ProviderKind::Claude, TOKEN, fx.channel).unwrap(),
            before.bytes.clone().unwrap(),
        );
        post_save_fault::fail_next(fx.channel);
        read_back_fault::before_next(fx.channel, move || {
            drop(answer);
            if !committed {
                std::fs::write(path, old).unwrap();
            }
        });
        let msg = ScopedMsg::Claim {
            proof: proof.clone(),
            persistence: ctx(),
            reply,
        };
        fx.handle
            .sender
            .send((ChannelMailboxMsg::ScopedClaim(msg), None))
            .unwrap_or_else(|_| panic!("actor gone"));
        let after = fx.seen().await;
        if !committed {
            assert_eq!(after, before);
            continue;
        }
        let (_, _, ids, _, _) = after.pending.expect("the actor keeps its pending record");
        assert_eq!(after.slot, Some(Arc::as_ptr(&proof.token) as usize));
        assert_eq!(disk_debts(fx.channel), [(ids[0].clone(), 311)]);
    }
}

/// Only the installed actor's exact identity and episode at its current generation inherits;
/// another episode at a newer generation and a stale or unreadable idle claim change nothing.
#[tokio::test(flavor = "current_thread")]
async fn scoped_claim_refuses_foreign_identity_and_stale_generation() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let fx = actor(&registry, 5_342_301, &[221, 222]).await;
    let exact = proof(fx.channel, 221);
    fx.installed(&exact).await;
    let held = fx.seen().await;
    type Tamper = (&'static str, fn(&mut Installation));
    let tampered: [Tamper; 8] = [
        ("token", |p| p.token = Arc::new(CancelToken::new())),
        ("birth", |p| p.birth = "reborn".into()),
        ("episode", |p| p.episode = "another".into()),
        ("owner", |p| p.owner = UserId::new(8)),
        ("message", |p| p.message = MessageId::new(222)),
        ("lower generation", |p| p.generation = 0),
        ("stale generation", |p| p.generation = 2),
        ("another episode at a newer generation", |p| {
            p.episode = "newer".into();
            p.generation = 2;
            p.current = Arc::new(AtomicU64::new(2));
        }),
    ];
    for (case, tamper) in tampered {
        let mut other = exact.clone();
        tamper(&mut other);
        let refusal = refused(fx.claim(&other).await);
        assert!(
            matches!(refusal, ScopedClaimRefusal::ForeignActor),
            "{case}: {refusal:?}"
        );
        assert_eq!(fx.seen().await, held, "{case}");
    }
    let fresh = Installation {
        generation: 2,
        current: Arc::new(AtomicU64::new(2)),
        ..exact.clone()
    };
    assert!(matches!(
        fx.claim(&fresh).await,
        ScopedClaimOutcome::Inherited(PendingDriver::NotScheduled)
    ));
    let inherited = fx.seen().await;
    let (episode, _, ids, feedback, driver) = held.pending.clone().unwrap();
    assert_eq!(inherited.pending, Some((episode, 2, ids, feedback, driver)));
    assert_eq!((&inherited.bytes, inherited.slot), (&held.bytes, held.slot));
    let pending = fx.handle.installed_pending().await.unwrap();
    assert!(
        pending
            .exits
            .iter()
            .all(|exit| exit.debt.episode == EPISODE)
    );

    let idle = actor(&registry, 5_342_302, &[231, 232]).await;
    let before = idle.seen().await;
    let stale = Installation {
        current: Arc::new(AtomicU64::new(2)),
        ..proof(idle.channel, 231)
    };
    let refusal = refused(idle.claim(&stale).await);
    assert!(matches!(refusal, ScopedClaimRefusal::Stale), "{refusal:?}");
    assert_eq!(idle.seen().await, before);

    // An actor that never read its file refuses a claim over a snapshot it cannot read.
    let torn = actor(&registry, 5_342_303, &[]).await;
    let path = pending_queue_file_path(&ProviderKind::Claude, TOKEN, torn.channel).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{\"version\":1,\"items\":[").unwrap();
    let before = torn.seen().await;
    let refusal = refused(torn.claim(&proof(torn.channel, 241)).await);
    assert!(
        matches!(refusal, ScopedClaimRefusal::Adopt(_)),
        "{refusal:?}"
    );
    assert!(refusal.detail().is_some_and(|error| !error.is_empty()));
    assert_eq!(torn.seen().await, before);
}

/// A save that errors after publishing is settled by re-reading the file: only its own snapshot
/// installs; a third one, or its own with a payload field changed, refuses this and later claims.
#[tokio::test(flavor = "current_thread")]
async fn scoped_claim_after_an_ambiguous_save_installs_only_its_published_snapshot() {
    const THIRD: &[u8] = br#"{"version":1,"items":[]}"#;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let cases = [
        "published",
        "third",
        "third over metadata",
        "text",
        "source generation",
    ];
    for (n, case) in cases.into_iter().enumerate() {
        let channel = 5_342_151 + n as u64;
        let (fx, third) = match case {
            "third over metadata" => with_debt(&registry, channel, &[301, 302]).await,
            _ => (actor(&registry, channel, &[301, 302]).await, THIRD.to_vec()),
        };
        let proof = proof(fx.channel, 301);
        let (before, debts) = (fx.seen().await, disk_debts(fx.channel));
        post_save_fault::fail_next(fx.channel);
        let path = pending_queue_file_path(&ProviderKind::Claude, TOKEN, fx.channel).unwrap();
        match case {
            "published" => {}
            "text" | "source generation" => {
                read_back_fault::before_next(fx.channel, retouch(path, case))
            }
            _ => {
                let third = third.clone();
                read_back_fault::before_next(fx.channel, move || {
                    std::fs::write(path, third).unwrap()
                });
            }
        }
        let outcome = fx.claim(&proof).await;
        let after = fx.seen().await;
        if case == "published" {
            let ScopedClaimOutcome::Installed(ids) = outcome else {
                panic!("a published claim installs");
            };
            assert_eq!(disk_debts(fx.channel), [(ids[0].clone(), 301)]);
            assert!(after.slot.is_some() && after.pending.is_some());
            continue;
        }
        let refusal = refused(outcome);
        assert!(
            matches!(refusal, ScopedClaimRefusal::Indeterminate(_)),
            "{case}: {refusal:?}"
        );
        assert!(
            refusal.detail().unwrap().contains("neither snapshot"),
            "{case}"
        );
        assert_eq!(
            (after.slot, &after.pending, &after.queue),
            (None, &None, &before.queue),
            "{case}"
        );
        if !matches!(case, "text" | "source generation") {
            assert_eq!(after.bytes.as_deref(), Some(third.as_slice()), "{case}");
            assert_eq!(disk_debts(fx.channel), debts, "{case}");
        }
        let again = refused(fx.claim(&proof).await);
        assert!(
            matches!(again, ScopedClaimRefusal::Undecided),
            "{case}: {again:?}"
        );
        assert_eq!(fx.seen().await, after, "{case}");
    }
}

/// A policy close of an episode, committed normally or by its guard after a panic past the
/// clear, raises the re-mint fence: a scoped claim of that episode is refused with no debt.
#[tokio::test(flavor = "current_thread")]
async fn policy_close_then_scoped_claim() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    for (n, stage) in [None, Some(test_hooks::Stage::AfterClear)]
        .into_iter()
        .enumerate()
    {
        let fx = actor(&registry, 5_342_401 + n as u64, &[241, 242]).await;
        let started = proof(fx.channel, 241);
        let token = started.token.clone();
        let nonce = token.turn_nonce().map(str::to_owned);
        let kind = ActiveTurnKind::UserOrAgent;
        let start = (fx.handle).try_start_turn_adopting(
            token,
            started.owner,
            started.message,
            kind,
            nonce,
            ctx(),
        );
        assert!(start.await.started);
        let fired = panic_at(stage);
        let closed = fx.close(&started, PolicyClear::Cleared).await;
        assert!(!test_hooks::disarm(), "{stage:?}: the close took the hook");
        assert_eq!(fired.load(Ordering::Relaxed), stage.is_some(), "{stage:?}");
        assert!(
            matches!(closed, PolicyMailboxOutcome::Committed(_)),
            "{stage:?}"
        );
        let released = fx.seen().await;
        assert!(released.slot.is_none());

        let again = Installation {
            token: Arc::new(CancelToken::from_persisted_turn_nonce(
                started.token.turn_nonce().map(str::to_owned),
            )),
            ..started.clone()
        };
        let refusal = refused(fx.claim(&again).await);
        assert!(
            matches!(refusal, ScopedClaimRefusal::ReleasedEpisode),
            "{stage:?}: {refusal:?}"
        );
        assert_eq!(fx.seen().await, released, "{stage:?}");
        assert!(disk_debts(fx.channel).is_empty(), "{stage:?}");
    }
}

/// A policy close of the actor a scoped claim installed releases the slot but not what it owes:
/// the pending record and its debt stay, the same claim is refused, and so is the idle tombstone.
#[tokio::test(flavor = "current_thread")]
async fn scoped_pending_then_policy_close() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    for (n, stage) in [None, Some(test_hooks::Stage::AfterClear)]
        .into_iter()
        .enumerate()
    {
        let fx = actor(&registry, 5_342_501 + n as u64, &[251, 252]).await;
        let proof = proof(fx.channel, 251);
        let ids = fx.installed(&proof).await;
        let installed = fx.seen().await;
        let fired = panic_at(stage);
        let closed = fx.close(&proof, PolicyClear::Cleared).await;
        assert!(!test_hooks::disarm(), "{stage:?}: the close took the hook");
        assert_eq!(fired.load(Ordering::Relaxed), stage.is_some(), "{stage:?}");
        assert!(
            matches!(closed, PolicyMailboxOutcome::Committed(_)),
            "{stage:?}"
        );
        let released = fx.seen().await;
        assert!(released.slot.is_none(), "{stage:?}");
        assert_eq!(released.pending, installed.pending, "{stage:?}");
        assert_eq!(disk_debts(fx.channel), [(ids[0].clone(), 251)], "{stage:?}");
        let refusal = refused(fx.claim(&proof).await);
        assert!(
            matches!(refusal, ScopedClaimRefusal::PendingHeld),
            "{refusal:?}"
        );
        assert_eq!(fx.seen().await, released, "{stage:?}");
    }
}

/// A policy close whose receipt clear is busy or mismatched keeps the scoped actor, its pending
/// record and the file as they were.
#[tokio::test(flavor = "current_thread")]
async fn policy_close_retained_preserves_pending() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    for (n, clear) in [PolicyClear::LockBusy, PolicyClear::Mismatch]
        .into_iter()
        .enumerate()
    {
        let fx = actor(&registry, 5_342_601 + n as u64, &[261, 262]).await;
        let proof = proof(fx.channel, 261);
        fx.installed(&proof).await;
        let installed = fx.seen().await;
        let closed = fx.close(&proof, clear).await;
        let retained =
            matches!(closed, PolicyMailboxOutcome::Retained(PolicyRetain::Clear(c)) if c == clear);
        assert!(retained, "{clear:?}");
        assert_eq!(fx.seen().await, installed, "{clear:?}");
    }
}

/// A tombstoned actor refuses a scoped claim by type and still answers the pending read.
#[tokio::test(flavor = "current_thread")]
async fn closed_actor_scoped_refusal() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let fx = actor(&registry, 5_342_701, &[]).await;
    assert_eq!(
        registry.remove_idle_entry(fx.channel).await,
        MailboxPurgeOutcome::Removed
    );
    let before = fx.seen().await;
    let refusal = refused(fx.claim(&proof(fx.channel, 271)).await);
    assert!(
        matches!(refusal, ScopedClaimRefusal::ActorClosed),
        "{refusal:?}"
    );
    assert_eq!(fx.seen().await, before);
}

/// The input fence judges a scoped claim by the namespace it names, not the other one the actor
/// last wrote: a closing gate or its own broken lock refuses by type, an open gate installs.
#[tokio::test(flavor = "current_thread")]
async fn input_fence_scoped_refusal() {
    use fence::Failure;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    for (n, case) in ["closing", "own lock", "open"].into_iter().enumerate() {
        let fx = actor(&registry, 5_342_801 + n as u64, &[]).await;
        let other = QueuePersistenceContext::new(&ProviderKind::Codex, TOKEN, None);
        assert!(fx.handle.enqueue(human(281), other).await.enqueued);
        let gate = Gate::protect(ProviderKind::Claude, fx.channel.get()).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        let _closing = (case == "closing").then(|| gate.close().unwrap());
        if case == "own lock" {
            let root = fence::population_root()
                .unwrap()
                .join("discord_inflight/claude");
            std::fs::create_dir_all(root.join(format!("{}.json.lock", fx.channel))).unwrap();
        }
        let before = fx.seen().await;
        let outcome = fx.claim(&proof(fx.channel, 282)).await;
        if case == "open" {
            assert!(
                matches!(outcome, ScopedClaimOutcome::Installed(_)),
                "{case}"
            );
            continue;
        }
        let refusal = refused(outcome);
        let expected = match case {
            "closing" => Failure::Mode(Mode::Closing),
            _ => Failure::Persistence,
        };
        let fenced = matches!(refusal, ScopedClaimRefusal::InputFence(seen) if seen == expected);
        assert!(fenced, "{case}: {refusal:?}");
        assert_eq!(fx.seen().await, before, "{case}");
    }
}

/// What an installed actor owes outlives its exact release in every state: record and debt stay,
/// its handoffs and the idle tombstone are refused, and a stale release leaves the next actor be.
#[tokio::test(flavor = "current_thread")]
async fn pending_outlives_an_exact_release_in_every_state() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let reached: [(&str, &[Intent]); 3] = [
        ("owed", &[]),
        ("reserved", &[Intent::Feedback, Intent::Driver]),
        ("ready", &[Intent::Driver, Intent::Ready]),
    ];
    for (n, (case, intents)) in reached.into_iter().enumerate() {
        let fx = actor(&registry, 5_342_951 + n as u64, &[321]).await;
        let proof = proof(fx.channel, 321);
        let ids = fx.installed(&proof).await;
        for intent in intents {
            let outcome = handoff(&fx, &proof, *intent).await;
            assert!(
                !matches!(outcome, HandoffOutcome::Refused(_)),
                "{case} {intent:?}"
            );
        }
        let installed = fx.seen().await;
        let (read, held) = debts_read_and_held(&fx).await;
        assert_eq!((read.len(), &read), (1, &held), "{case}");
        assert_eq!(
            (&read[0].0, read[0].1.as_str()),
            (&ids[0], EPISODE),
            "{case}"
        );

        let closed = fx.close(&proof, PolicyClear::Cleared).await;
        assert!(
            matches!(closed, PolicyMailboxOutcome::Committed(_)),
            "{case}"
        );
        let released = fx.seen().await;
        assert!(released.slot.is_none(), "{case}");
        assert_eq!(
            (&released.pending, &released.bytes),
            (&installed.pending, &installed.bytes),
            "{case}"
        );
        assert_eq!(
            debts_read_and_held(&fx).await,
            (read.clone(), held),
            "{case}"
        );
        for intent in [Intent::Feedback, Intent::Driver, Intent::Ready] {
            let outcome = handoff(&fx, &proof, intent).await;
            assert!(
                matches!(outcome, HandoffOutcome::Refused(_)),
                "{case} {intent:?}"
            );
        }
        assert_eq!(fx.seen().await, released, "{case}");
        let tombstone = registry.remove_idle_entry(fx.channel).await;
        assert_eq!(
            tombstone,
            MailboxPurgeOutcome::RefusedLiveWork("installed_pending")
        );

        let next = Arc::new(CancelToken::from_persisted_turn_nonce(Some(format!(
            "next-{n}"
        ))));
        let kind = ActiveTurnKind::UserOrAgent;
        let start = (fx.handle).restore_active_turn_kinded(
            next.clone(),
            proof.owner,
            MessageId::new(323),
            kind,
        );
        start.await;
        let occupied = fx.seen().await;
        assert_eq!(occupied.slot, Some(Arc::as_ptr(&next) as usize), "{case}");
        let stale = fx.close(&proof, PolicyClear::Cleared).await;
        assert!(
            !matches!(stale, PolicyMailboxOutcome::Committed(_)),
            "{case}"
        );
        assert_eq!(fx.seen().await, occupied, "{case}");
    }
}

/// An unacknowledged pending record keeps the actor from the idle tombstone after its slot is
/// released, whether its feedback or driver is owed, reserved or ready.
#[tokio::test(flavor = "current_thread")]
async fn pending_prevents_idle_tombstone() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let fx = actor(&registry, 5_342_901, &[291]).await;
    let proof = proof(fx.channel, 291);
    fx.installed(&proof).await;
    let closed = fx.close(&proof, PolicyClear::Cleared).await;
    assert!(matches!(closed, PolicyMailboxOutcome::Committed(_)));
    let released = fx.seen().await;
    assert!(released.slot.is_none() && released.queue.is_empty());
    let outcome = registry.remove_idle_entry(fx.channel).await;
    assert_eq!(
        outcome,
        MailboxPurgeOutcome::RefusedLiveWork("installed_pending")
    );
    let again = refused(fx.claim(&proof).await);
    assert!(
        matches!(again, ScopedClaimRefusal::PendingHeld),
        "{again:?}"
    );

    let mut pending = fx.handle.installed_pending().await.unwrap();
    for feedback in [PendingFeedback::Owed, PendingFeedback::Reserved] {
        for driver in [
            PendingDriver::NotScheduled,
            PendingDriver::Reserved,
            PendingDriver::Ready,
        ] {
            (pending.feedback, pending.driver) = (feedback, driver);
            let mut state = ChannelMailboxState {
                installed_pending: Some(pending.clone()),
                ..ChannelMailboxState::default()
            };
            let verdict = registry_purge::close_if_idle_verdict(&mut state);
            assert_eq!(verdict, Err("installed_pending"), "{feedback:?} {driver:?}");
            assert!(!state.closed);
        }
    }
}
