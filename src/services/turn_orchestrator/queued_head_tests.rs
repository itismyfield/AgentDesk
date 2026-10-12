use std::future::Future;
use std::path::PathBuf;
use std::sync::MutexGuard;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::*;
use crate::services::turn_orchestrator::test_support::{
    AGENTDESK_ROOT_DIR_ENV, fail_queue_saves, lock_test_env,
};

const TOKEN: &str = "queued-head";
const GENERATION: u64 = 5;

struct Root {
    _dir: tempfile::TempDir,
    _lock: MutexGuard<'static, ()>,
}

impl Drop for Root {
    fn drop(&mut self) {
        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }
}

fn root() -> Root {
    let lock = lock_test_env();
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, dir.path()) };
    Root {
        _dir: dir,
        _lock: lock,
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(future)
}

fn ctx() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, TOKEN, None)
}

/// A person's plain text as intake queues it: one marked source, segments left to the actor.
fn human(id: u64, author: u64, text: &str) -> Intervention {
    let source = SourceMessageQueuedGeneration::user_instruction(MessageId::new(id), GENERATION);
    Intervention {
        author_id: UserId::new(author),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: GENERATION,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: vec![source],
        source_text_segments: Vec::new(),
        text: text.to_string(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: true,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

fn queue_path(channel: ChannelId) -> PathBuf {
    let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    root.join("claude")
        .join(TOKEN)
        .join(format!("{}.json", channel.get()))
}

fn disk(channel: ChannelId) -> serde_json::Value {
    let content = std::fs::read_to_string(queue_path(channel)).unwrap_or_else(|_| "null".into());
    serde_json::from_str(&content).unwrap()
}

fn label(claim: &QueuedHeadClaim) -> &'static str {
    match claim {
        QueuedHeadClaim::Reserved(_) => "reserved",
        QueuedHeadClaim::NotEligible => "not_eligible",
        QueuedHeadClaim::Contended => "contended",
        QueuedHeadClaim::BlockedUnknown => "blocked_unknown",
        QueuedHeadClaim::PersistenceFailed(error) => {
            assert!(!error.is_empty());
            "persistence_failed"
        }
    }
}

async fn reserve(handle: &ChannelMailboxHandle, head: &Intervention) -> HeadTicket {
    match handle
        .claim_queued_head(QueueIdentity::of(head), ctx())
        .await
    {
        QueuedHeadClaim::Reserved(ticket) => ticket,
        other => panic!("claim of {} was {}", head.message_id, label(&other)),
    }
}

async fn settle(
    handle: &ChannelMailboxHandle,
    ticket: HeadTicket,
    outcome: HeadOutcome,
) -> &'static str {
    match handle.settle_queued_head(ticket, outcome, ctx()).await {
        HeadSettle::Committed(_) => "committed",
        HeadSettle::NotCommitted(_ticket, error) => {
            assert!(!error.is_empty());
            "not_committed"
        }
        HeadSettle::Unknown => "unknown",
        HeadSettle::Stale => "stale",
    }
}

/// Claims `head`, marks it possibly sent and records it submitted.
async fn submit(handle: &ChannelMailboxHandle, head: &Intervention) {
    let ticket = reserve(handle, head).await;
    let HeadSettle::Committed(Some(ticket)) = handle
        .settle_queued_head(ticket, HeadOutcome::MayHaveSent, ctx())
        .await
    else {
        panic!("MayHaveSent was not committed");
    };
    assert_eq!(
        settle(handle, ticket, HeadOutcome::Injected).await,
        "committed"
    );
}

async fn queued(handle: &ChannelMailboxHandle) -> Vec<(u64, Vec<u64>, String)> {
    let queue = handle.snapshot().await.intervention_queue;
    let row = |item: &Intervention| {
        let sources = item.source_message_ids.iter().map(|id| id.get()).collect();
        (item.message_id.get(), sources, item.text.clone())
    };
    queue.iter().map(row).collect()
}

async fn taken(handle: &ChannelMailboxHandle) -> Option<u64> {
    let taken = handle.take_next_soft(ctx()).await;
    taken.intervention.map(|item| item.message_id.get())
}

async fn starts(handle: &ChannelMailboxHandle, id: u64, order: TurnAdmissionOrder) -> bool {
    let token = Arc::new(CancelToken::new());
    let kind = ActiveTurnKind::UserOrAgent;
    (handle.try_start_turn_kinded_with_persistence(
        token,
        UserId::new(9),
        MessageId::new(id),
        kind,
        order,
        ctx(),
    ))
    .await
    .started
}

/// A legacy array and a V1 snapshot each restart into the same eligible singleton with the same
/// identity; the enqueue alone never writes V1.
#[test]
fn legacy_and_v1_restarts_judge_the_same_singleton_eligible() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_813_011);
        let head = human(11, 7, "status?");
        let live = ChannelMailboxRegistry::default();
        assert!(
            live.handle(channel)
                .enqueue(head.clone(), ctx())
                .await
                .enqueued
        );
        assert!(
            disk(channel).is_array(),
            "an enqueue alone stays a legacy array"
        );

        let legacy_restart = ChannelMailboxRegistry::default();
        let _held = reserve(&legacy_restart.handle(channel), &head).await;
        let reserved = disk(channel);
        assert_eq!(reserved["version"], 1);
        assert_eq!(reserved["head_injection"]["phase"], "reserved");

        let v1_restart = ChannelMailboxRegistry::default();
        let _again = reserve(&v1_restart.handle(channel), &head).await;
        assert_eq!(
            disk(channel)["head_injection"]["identity"],
            reserved["head_injection"]["identity"]
        );
        assert_eq!(
            disk(channel)["head_injection"]["original"],
            reserved["head_injection"]["original"]
        );
    });
}

/// A reservation a restart finds never reached the pane: the head is released and dispatched.
#[test]
fn a_reserved_head_found_by_a_restart_is_released_unsent() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_813_012);
        let head = human(12, 7, "status?");
        let live = ChannelMailboxRegistry::default();
        assert!(
            live.handle(channel)
                .enqueue(head.clone(), ctx())
                .await
                .enqueued
        );
        let _held = reserve(&live.handle(channel), &head).await;

        let restart = ChannelMailboxRegistry::default();
        assert_eq!(taken(&restart.handle(channel)).await, Some(12));
        assert!(
            disk(channel).is_null(),
            "the released head left as a plain dequeue"
        );
    });
}

/// A head that may have been pasted becomes Unknown after a restart; nothing takes it, claims
/// it or starts ahead of it, and the next write records Unknown.
#[test]
fn a_possibly_sent_head_found_by_a_restart_is_unknown_and_never_resubmitted() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_813_013);
        let head = human(13, 7, "status?");
        let live = ChannelMailboxRegistry::default();
        let handle = live.handle(channel);
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        let ticket = reserve(&handle, &head).await;
        let marked = handle
            .settle_queued_head(ticket, HeadOutcome::MayHaveSent, ctx())
            .await;
        assert!(matches!(marked, HeadSettle::Committed(Some(_))));
        assert_eq!(disk(channel)["head_injection"]["phase"], "may_have_sent");

        let restart = ChannelMailboxRegistry::default().handle(channel);
        assert!(restart.enqueue(human(14, 8, "next"), ctx()).await.enqueued);
        assert_eq!(disk(channel)["head_injection"]["phase"], "unknown");
        assert_eq!(taken(&restart).await, None);
        let claim = restart
            .claim_queued_head(QueueIdentity::of(&head), ctx())
            .await;
        assert_eq!(label(&claim), "blocked_unknown");
        assert!(!starts(&restart, 15, TurnAdmissionOrder::Immediate).await);
        assert_eq!(
            queued(&restart)
                .await
                .iter()
                .map(|q| q.0)
                .collect::<Vec<_>>(),
            [13, 14]
        );
    });
}

/// An Unknown head is a barrier: no dequeue, turn claim, finish, clear or purge passes it or drops it.
#[test]
fn an_unknown_head_blocks_every_consumer_and_survives_every_drain() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_813_014);
        let head = human(21, 7, "status?");
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        let ticket = reserve(&handle, &head).await;
        assert_eq!(
            settle(&handle, ticket, HeadOutcome::Unconfirmed).await,
            "unknown"
        );
        assert!(handle.enqueue(human(22, 8, "behind"), ctx()).await.enqueued);

        assert_eq!(taken(&handle).await, None);
        let skip_to_next = handle
            .take_soft_matching(ctx(), Some(MessageId::new(22)))
            .await;
        assert!(
            skip_to_next.intervention.is_none(),
            "the item behind is not taken past it"
        );
        assert!(!starts(&handle, 22, TurnAdmissionOrder::BehindQueue).await);
        assert!(!starts(&handle, 23, TurnAdmissionOrder::Immediate).await);
        let _ = handle.finish_turn(ctx()).await;
        let before = std::fs::read(queue_path(channel)).unwrap();
        let cleared = handle.clear(ctx()).await;
        assert!(cleared.persistence_error.is_some());
        assert!(cleared.queue_exit_events.is_empty());
        assert!(!cleared.discarded_message_ids.contains(&MessageId::new(21)));
        assert_eq!(std::fs::read(queue_path(channel)).unwrap(), before);
        let purged = handle.purge_queue(ctx(), true).await;
        assert!(purged.input_refusal.is_some());
        assert_eq!(std::fs::read(queue_path(channel)).unwrap(), before);
        assert_eq!(
            queued(&handle)
                .await
                .iter()
                .map(|q| q.0)
                .collect::<Vec<_>>(),
            [21, 22]
        );
        assert_eq!(disk(channel)["head_injection"]["phase"], "unknown");
    });
}

/// Later input from the same author queues behind a reserved head instead of merging into it;
/// later items still merge with each other, and the head leaves with its own payload.
#[test]
fn a_reserved_head_is_never_merged_and_later_input_keeps_its_place() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_813_002);
        let head = human(31, 7, "first");
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        let ticket = reserve(&handle, &head).await;
        assert!(handle.enqueue(human(32, 7, "second"), ctx()).await.enqueued);
        assert!(handle.enqueue(human(33, 7, "third"), ctx()).await.enqueued);
        let expected_head = (31, vec![31], "first".to_string());
        let tail = (33, vec![32, 33], "second\nthird".to_string());
        assert_eq!(queued(&handle).await, [expected_head, tail.clone()]);

        let HeadSettle::Committed(Some(ticket)) = handle
            .settle_queued_head(ticket, HeadOutcome::MayHaveSent, ctx())
            .await
        else {
            panic!("MayHaveSent was not committed");
        };
        assert_eq!(
            settle(&handle, ticket, HeadOutcome::Injected).await,
            "committed"
        );
        assert_eq!(queued(&handle).await, [tail]);
        let saved = disk(channel);
        assert_eq!(saved["terminal_identities"][0]["primary"], 31);
        assert_eq!(saved["items"].as_array().map(Vec::len), Some(1));
    });
}

/// A paste known not to have happened releases the reservation and leaves the queue as it was.
#[test]
fn a_not_sent_head_keeps_its_place_and_payload() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_813_005);
        let head = human(41, 7, "first");
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        assert!(handle.enqueue(human(42, 8, "second"), ctx()).await.enqueued);
        let before = queued(&handle).await;
        let ticket = reserve(&handle, &head).await;
        let HeadSettle::Committed(Some(ticket)) = handle
            .settle_queued_head(ticket, HeadOutcome::MayHaveSent, ctx())
            .await
        else {
            panic!("MayHaveSent was not committed");
        };
        assert_eq!(
            settle(&handle, ticket, HeadOutcome::NotSent).await,
            "committed"
        );
        assert_eq!(queued(&handle).await, before);
        assert!(
            disk(channel).is_array(),
            "nothing left to record: back to a legacy array"
        );
        assert_eq!(taken(&handle).await, Some(41));
    });
}

/// A failed reservation write issues no ticket; a failed write once a paste may have started
/// leaves the head Unknown instead of releasing it.
#[test]
fn write_failures_issue_no_ticket_or_leave_the_head_unknown() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_813_009);
        let head = human(51, 7, "first");
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        fail_queue_saves(channel, 1);
        let claim = handle
            .claim_queued_head(QueueIdentity::of(&head), ctx())
            .await;
        assert_eq!(label(&claim), "persistence_failed");
        assert!(disk(channel).is_array());

        let ticket = reserve(&handle, &head).await;
        let HeadSettle::Committed(Some(ticket)) = handle
            .settle_queued_head(ticket, HeadOutcome::MayHaveSent, ctx())
            .await
        else {
            panic!("MayHaveSent was not committed");
        };
        fail_queue_saves(channel, 1);
        assert_eq!(
            settle(&handle, ticket, HeadOutcome::Injected).await,
            "unknown"
        );
        assert_eq!(
            queued(&handle)
                .await
                .iter()
                .map(|q| q.0)
                .collect::<Vec<_>>(),
            [51]
        );
        assert_eq!(taken(&handle).await, None);
        assert_eq!(disk(channel)["head_injection"]["phase"], "unknown");

        let second = ChannelId::new(6_813_019);
        let other = human(52, 7, "second");
        let handle = ChannelMailboxRegistry::default().handle(second);
        assert!(handle.enqueue(other.clone(), ctx()).await.enqueued);
        let ticket = reserve(&handle, &other).await;
        fail_queue_saves(second, 2);
        assert_eq!(
            settle(&handle, ticket, HeadOutcome::MayHaveSent).await,
            "unknown"
        );
        assert_eq!(taken(&handle).await, None);
        assert_eq!(
            queued(&handle)
                .await
                .iter()
                .map(|q| q.0)
                .collect::<Vec<_>>(),
            [52]
        );
    });
}

/// While reserved, a front restore, a cancel, a clear and a purge each keep the head first and whole.
#[test]
fn restore_cancel_clear_and_purge_keep_a_reserved_head() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_813_017);
        let head = human(61, 7, "first");
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        let _ticket = reserve(&handle, &head).await;
        assert!(handle.enqueue(human(62, 8, "second"), ctx()).await.enqueued);

        let restored = handle.requeue_front(human(60, 9, "restored"), ctx()).await;
        assert!(!restored.enqueued && restored.persistence_error.is_some());
        let cancelled = handle
            .cancel_queued_primary_message(MessageId::new(61), ctx())
            .await;
        assert!(cancelled.removed.is_none());
        assert_eq!(
            queued(&handle)
                .await
                .iter()
                .map(|q| q.0)
                .collect::<Vec<_>>(),
            [61, 62]
        );
        let cleared = handle.clear(ctx()).await;
        assert!(cleared.persistence_error.is_some());
        assert!(!cleared.discarded_message_ids.contains(&MessageId::new(61)));
        assert!(handle.enqueue(human(63, 8, "third"), ctx()).await.enqueued);
        let purged = handle.purge_queue(ctx(), true).await;
        assert!(purged.input_refusal.is_some());
        assert_eq!(purged.drained, 0);
        assert_eq!(
            queued(&handle).await,
            [
                (61, vec![61], "first".to_string()),
                (63, vec![62, 63], "second\nthird".to_string()),
            ]
        );
        assert_eq!(disk(channel)["head_injection"]["phase"], "reserved");
    });
}

/// Without a live actor, force purge removes only namespaces whose file holds no head or
/// submitted source and parses cleanly.
#[test]
fn an_actorless_force_purge_keeps_held_or_unreadable_snapshots() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_813_171);
        let item = r#"{"author_id":7,"message_id":71,"text":"h"}"#;
        let identity = r#"{"primary":71,"queued_generation":5,"sources":[{"message_id":71,"queued_generation":5}]}"#;
        let unknown = format!(
            r#"{{"version":1,"items":[{item}],"head_injection":{{"identity":{identity},"original":{item},"phase":"unknown"}}}}"#
        );
        let terminal = format!(r#"{{"version":1,"items":[],"terminal_identities":[{identity}]}}"#);
        let base = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
        let file = |token: &str| {
            base.join("claude")
                .join(token)
                .join(format!("{}.json", channel.get()))
        };
        for (token, body) in [
            ("unknown", unknown.as_str()),
            ("terminal", terminal.as_str()),
            ("legacy", "[]"),
            ("torn", r#"{"version":1,"items":["#),
            ("broken", "[{"),
        ] {
            std::fs::create_dir_all(file(token).parent().unwrap()).unwrap();
            std::fs::write(file(token), body).unwrap();
        }
        let target = crate::services::turn_lifecycle::TurnLifecycleTarget {
            provider: Some(ProviderKind::Claude),
            channel_id: Some(channel),
            tmux_name: String::new(),
        };
        let purged = crate::services::queue::force_purge_channel_mailbox(None, &target, None).await;
        assert_eq!(purged.map(|counts| counts.disk_files_removed), Some(1));
        let kept = ["unknown", "terminal", "legacy", "torn"].map(|token| file(token).exists());
        assert_eq!(kept, [true, true, false, true]);
        assert_eq!(std::fs::read(file("broken")).unwrap(), b"[{");
    });
}

/// A legacy queue file that does not parse is never read as empty: the enqueue fails and the
/// file keeps its bytes, whether the actor meets it at start or after it was adopted.
#[test]
fn a_corrupt_legacy_queue_refuses_input_and_keeps_its_bytes() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_813_172);
        let path = queue_path(channel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[{").unwrap();
        let restart = ChannelMailboxRegistry::default();
        let refused = restart
            .handle(channel)
            .enqueue(human(81, 7, "b"), ctx())
            .await;
        assert!(!refused.enqueued && refused.persistence_error.is_some());
        assert_eq!(std::fs::read(&path).unwrap(), b"[{");

        std::fs::remove_file(&path).unwrap();
        let live = ChannelMailboxRegistry::default();
        let handle = live.handle(channel);
        assert!(handle.enqueue(human(82, 7, "a"), ctx()).await.enqueued);
        std::fs::write(&path, "[{").unwrap();
        let refused = handle.enqueue(human(83, 8, "c"), ctx()).await;
        assert!(!refused.enqueued && refused.persistence_error.is_some());
        assert_eq!(std::fs::read(&path).unwrap(), b"[{");
    });
}

/// A full queue evicts the oldest unreserved tail, while the reserved head stays first.
/// A restored item cannot land ahead of that head.
#[test]
fn overflow_and_restore_never_displace_a_reserved_head() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_813_018);
        let head = human(1_000, 7, "first");
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        let _ticket = reserve(&handle, &head).await;
        for n in 1..MAX_INTERVENTIONS_PER_CHANNEL as u64 {
            let item = human(1_000 + n, 100 + n, &format!("item {n}"));
            assert!(handle.enqueue(item, ctx()).await.enqueued);
        }
        let overflow = handle.enqueue(human(2_000, 99, "overflow"), ctx()).await;
        assert!(overflow.enqueued && overflow.persistence_error.is_none());
        assert_eq!(overflow.queue_exit_events.len(), 1);
        assert!(matches!(
            overflow.queue_exit_events[0].kind,
            QueueExitKind::Overflow
        ));
        assert_eq!(
            overflow.queue_exit_events[0].intervention.message_id.get(),
            1_001
        );
        let restored = handle
            .merge_restored_queue_items(vec![human(999, 98, "restored")], ctx())
            .await;
        assert!(restored.persistence_error.is_some());
        let queue = queued(&handle).await;
        assert_eq!(
            (queue.len(), queue[0].0),
            (MAX_INTERVENTIONS_PER_CHANNEL, 1_000)
        );
    });
}

/// A claim and a dispatch never own the same head: a handed-out head or a stale marker refuses
/// the claim, and a claimed head is never handed out.
#[test]
fn a_claim_and_a_dispatch_never_both_own_the_queue_front() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(68_130_191);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let next = human(82, 8, "second");
        assert!(handle.enqueue(human(81, 7, "first"), ctx()).await.enqueued);
        assert!(handle.enqueue(next.clone(), ctx()).await.enqueued);
        assert_eq!(taken(&handle).await, Some(81));
        let claim = handle
            .claim_queued_head(QueueIdentity::of(&next), ctx())
            .await;
        assert_eq!(label(&claim), "contended");

        let restart = ChannelMailboxRegistry::default().handle(channel);
        let claim = restart
            .claim_queued_head(QueueIdentity::of(&next), ctx())
            .await;
        assert_eq!(
            label(&claim),
            "contended",
            "the marker left by the dispatch still owns 81"
        );

        let other = ChannelId::new(68_130_192);
        let handle = ChannelMailboxRegistry::default().handle(other);
        let head = human(83, 7, "first");
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        let behind = human(84, 8, "second");
        assert!(handle.enqueue(behind.clone(), ctx()).await.enqueued);
        let stale = handle
            .claim_queued_head(QueueIdentity::of(&behind), ctx())
            .await;
        assert_eq!(
            label(&stale),
            "contended",
            "the claim names an item that is not the head"
        );
        let _ticket = reserve(&handle, &head).await;
        assert_eq!(taken(&handle).await, None);
        assert_eq!(
            queued(&handle)
                .await
                .iter()
                .map(|q| q.0)
                .collect::<Vec<_>>(),
            [83, 84]
        );
        let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
        let marker = root
            .join("claude")
            .join(TOKEN)
            .join(format!("{}.dispatch", other.get()));
        assert!(!marker.exists());
    });
}

/// A submitted source is removed from a restored item by its own id and generation, so the other
/// source survives even when the item's primary changed; the same source re-sent alone is refused.
#[test]
fn a_restored_item_drops_only_its_submitted_source() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_813_022);
        let head = human(91, 7, "h");
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        submit(&handle, &head).await;

        let mut merged = human(92, 7, "h\nb");
        let h_source =
            SourceMessageQueuedGeneration::user_instruction(MessageId::new(91), GENERATION);
        merged.source_message_ids.insert(0, MessageId::new(91));
        merged.source_message_queued_generations.insert(0, h_source);
        merged.source_text_segments = vec![
            SourceMessageTextSegment::new(MessageId::new(91), "h"),
            SourceMessageTextSegment::new(MessageId::new(92), "b"),
        ];
        let restored = handle.merge_restored_queue_items(vec![merged], ctx()).await;
        assert!(restored.persistence_error.is_none());
        assert_eq!(queued(&handle).await, [(92, vec![92], "b".to_string())]);

        let again = handle.enqueue(head.clone(), ctx()).await;
        assert_eq!(
            again.refusal_reason,
            Some(EnqueueRefusalReason::AlreadyActiveTurn)
        );
        assert_eq!(taken(&handle).await, Some(92));
        assert_eq!(taken(&handle).await, None);
        let emptied = disk(channel);
        assert_eq!(
            emptied["items"],
            serde_json::json!([]),
            "an empty queue keeps its terminal record"
        );
        assert_eq!(emptied["terminal_identities"][0]["primary"], 91);
    });
}

/// Bots, merged or decorated input and commands are never claimed.
#[test]
fn only_a_plain_singleton_from_a_person_is_eligible() {
    let _root = root();
    block_on(async {
        let mut bot = human(101, 7, "status?");
        bot.author_is_bot = true;
        let mut reply = human(102, 7, "status?");
        reply.reply_context = Some("earlier".into());
        let mut boundary = human(103, 7, "status?");
        boundary.has_reply_boundary = true;
        let mut upload = human(104, 7, "status?");
        upload.pending_uploads = vec!["/tmp/a.png".into()];
        let mut automation = human(105, 7, "status?");
        automation.source_message_queued_generations[0].preserve_on_cancel = false;
        let command = human(106, 7, "!stop");
        let cases = [bot, reply, boundary, upload, automation, command];
        for (n, case) in cases.into_iter().enumerate() {
            let handle =
                ChannelMailboxRegistry::default().handle(ChannelId::new(6_813_100 + n as u64));
            assert!(handle.enqueue(case.clone(), ctx()).await.enqueued);
            let claim = handle
                .claim_queued_head(QueueIdentity::of(&case), ctx())
                .await;
            assert_eq!(label(&claim), "not_eligible", "{}", case.message_id);
        }

        let channel = ChannelId::new(6_813_110);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(handle.enqueue(human(111, 7, "a"), ctx()).await.enqueued);
        let merged = human(112, 7, "b");
        assert!(handle.enqueue(merged.clone(), ctx()).await.merged);
        let head = handle.snapshot().await.intervention_queue.remove(0);
        let claim = handle
            .claim_queued_head(QueueIdentity::of(&head), ctx())
            .await;
        assert_eq!(label(&claim), "not_eligible", "a merged head");
    });
}

/// With no claim, every queue write is the legacy array, byte for byte as the array serializer
/// writes it, and an emptied queue removes its file.
#[test]
fn without_a_claim_the_queue_file_stays_the_legacy_array() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_813_016);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(handle.enqueue(human(121, 7, "a"), ctx()).await.enqueued);
        assert!(handle.enqueue(human(122, 7, "b"), ctx()).await.merged);
        assert!(handle.enqueue(human(123, 8, "c"), ctx()).await.enqueued);
        assert_eq!(taken(&handle).await, Some(122));
        let content = std::fs::read_to_string(queue_path(channel)).unwrap();
        let items: Vec<PendingQueueItem> = serde_json::from_str(&content).unwrap();
        assert_eq!(content, serde_json::to_string_pretty(&items).unwrap());
        let _ = handle.clear(ctx()).await;
        assert!(!queue_path(channel).exists());
    });
}

/// The offline downgrade rewrites only a safe V1 snapshot as the array an older binary reads.
#[test]
fn the_downgrade_converts_only_a_resolved_snapshot() {
    let _root = root();
    block_on(async {
        let downgrade = |channel, safe: bool| {
            let provider = ProviderKind::Claude;
            downgrade_channel_snapshot(&provider, TOKEN, channel, |_| safe)
        };
        let channel = ChannelId::new(6_813_116);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let head = human(131, 7, "h");
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        submit(&handle, &head).await;
        assert!(handle.enqueue(human(132, 8, "b"), ctx()).await.enqueued);
        let before = std::fs::read_to_string(queue_path(channel)).unwrap();
        assert!(downgrade(channel, false).is_err());
        assert_eq!(
            std::fs::read_to_string(queue_path(channel)).unwrap(),
            before
        );
        assert_eq!(downgrade(channel, true), Ok(true));
        let content = std::fs::read_to_string(queue_path(channel)).unwrap();
        let items: Vec<PendingQueueItem> = serde_json::from_str(&content).unwrap();
        assert_eq!(
            items.iter().map(|i| i.message_id).collect::<Vec<_>>(),
            [132]
        );
        assert_eq!(downgrade(channel, true), Ok(false));

        let unresolved = ChannelId::new(6_813_117);
        let handle = ChannelMailboxRegistry::default().handle(unresolved);
        let head = human(133, 7, "h");
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        let ticket = reserve(&handle, &head).await;
        assert_eq!(
            settle(&handle, ticket, HeadOutcome::Unconfirmed).await,
            "unknown"
        );
        let before = std::fs::read_to_string(queue_path(unresolved)).unwrap();
        assert!(downgrade(unresolved, true).is_err());
        assert_eq!(
            std::fs::read_to_string(queue_path(unresolved)).unwrap(),
            before
        );
    });
}
