use super::super::*;
use super::drain_head_overflow;
use crate::services::turn_orchestrator::test_support::lock_test_env;

const TOKEN: &str = "s2b-reserved-overflow";

fn human(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(id),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 5,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: vec![SourceMessageQueuedGeneration::user_instruction(
            MessageId::new(id),
            5,
        )],
        source_text_segments: Vec::new(),
        text: format!("human input {id}"),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    }
}

fn ids(queue: &[Intervention]) -> Vec<u64> {
    queue.iter().map(|item| item.message_id.get()).collect()
}

fn overflow_ids(events: &[QueueExitEvent]) -> Vec<u64> {
    events
        .iter()
        .map(|event| {
            assert!(matches!(event.kind, QueueExitKind::Overflow));
            event.intervention.message_id.get()
        })
        .collect()
}

#[test]
fn s2b_reserved_overflow_protects_head_at_both_enqueue_prunes() {
    let head = human(1);
    let mut queue = vec![head.clone()];
    queue.extend((2..=31).map(human));
    assert_eq!(queue.len(), MAX_INTERVENTIONS_PER_CHANNEL + 1);

    let result = queue_enqueue::enqueue_behind(&mut queue, human(32), None, None, true);

    assert!(result.enqueued);
    assert!(!result.merged);
    assert_eq!(overflow_ids(&result.queue_exit_events), [2, 3]);
    assert_eq!(ids(&queue), [vec![1], (4..=32).collect()].concat());
    assert_eq!(QueueIdentity::of(&queue[0]), QueueIdentity::of(&head));
    assert_eq!(queue[0].text, head.text);
}

#[test]
fn s2b_unreserved_overflow_retains_existing_drop_oldest_contract() {
    let mut queue: Vec<_> = (1..=33).map(human).collect();

    let events = drain_head_overflow(&mut queue);

    assert_eq!(overflow_ids(&events), [1, 2, 3]);
    assert_eq!(ids(&queue), (4..=33).collect::<Vec<_>>());
}

struct Root {
    _dir: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for Root {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(previous) => std::env::set_var("AGENTDESK_ROOT_DIR", previous),
                None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
            }
        }
    }
}

fn root() -> Root {
    let lock = lock_test_env();
    let dir = tempfile::tempdir().unwrap();
    let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", dir.path()) };
    Root {
        _dir: dir,
        previous,
        _lock: lock,
    }
}

#[test]
fn s2b_reserved_h_plus_b29_plus_c_is_durable_and_keeps_ticket() {
    let _root = root();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let channel = ChannelId::new(6_813_411);
            let ctx = || QueuePersistenceContext::new(&ProviderKind::Claude, TOKEN, None);
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(channel);
            let head_id = 6_813_411_001;
            let head = human(head_id);
            assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
            let ticket = match handle
                .claim_queued_head(QueueIdentity::of(&head), ctx())
                .await
            {
                queued_head::QueuedHeadClaim::Reserved(ticket) => ticket,
                _ => panic!("expected durable head reservation"),
            };
            let path = pending_queue_persistence::pending_queue_file_path(
                &ProviderKind::Claude,
                TOKEN,
                channel,
            )
            .unwrap();
            let read = || {
                serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap()
            };
            let reserved = read()["head_injection"].clone();
            for id in head_id + 1..=head_id + 29 {
                let result = handle.enqueue(human(id), ctx()).await;
                assert!(result.enqueued && result.persistence_error.is_none());
                assert!(result.queue_exit_events.is_empty());
            }
            let result = handle.enqueue(human(head_id + 30), ctx()).await;

            assert!(result.enqueued && result.persistence_error.is_none());
            assert!(!result.merged);
            assert_eq!(overflow_ids(&result.queue_exit_events), [head_id + 1]);
            let queue = handle.snapshot().await.intervention_queue;
            let expected = [vec![head_id], (head_id + 2..=head_id + 30).collect()].concat();
            assert_eq!(ids(&queue), expected);
            assert_eq!(QueueIdentity::of(&queue[0]), QueueIdentity::of(&head));
            assert_eq!(read()["head_injection"], reserved);
            let (durable, _) = pending_queue_persistence::load_channel_pending_queue_checked(
                &ProviderKind::Claude,
                TOKEN,
                channel,
            )
            .expect("reserved H remains a valid checked snapshot after overflow");
            assert_eq!(ids(&durable), expected);
            assert!(matches!(
                handle
                    .settle_queued_head(ticket, queued_head::HeadOutcome::NotSent, ctx())
                    .await,
                queued_head::HeadSettle::Committed(None)
            ));
            let restarted = ChannelMailboxRegistry::default().handle(channel);
            let restored = restarted.take_next_soft(ctx()).await;
            assert!(restored.persistence_error.is_none());
            let restored_head = restored.intervention.expect("H still dispatches first");
            assert_eq!(restored_head.message_id, head.message_id);
            assert_eq!(restored_head.text, head.text);
            assert_eq!(
                ids(&restarted.snapshot().await.intervention_queue),
                (head_id + 2..=head_id + 30).collect::<Vec<_>>()
            );
        });
}
