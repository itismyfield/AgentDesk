//! The actor consumes only the Unknown head it holds, unchanged, at the queue front and behind an
//! open gate; a failed write leaves that head Unknown before the same queue.
use super::*;
use crate::services::discord::health::inject_hook;
use crate::services::turn_orchestrator::test_support::{fail_queue_saves, lock_test_env};

const TOKEN: &str = "queued-head-mend";

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

fn ctx() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, TOKEN, None)
}

fn human(id: u64, text: &str) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 3,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: vec![SourceMessageQueuedGeneration::user_instruction(
            MessageId::new(id),
            3,
        )],
        source_text_segments: Vec::new(),
        text: text.into(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    }
}

fn disk(channel: ChannelId) -> serde_json::Value {
    let path = pending_queue_file_path(&ProviderKind::Claude, TOKEN, channel).unwrap();
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

async fn queue(handle: &ChannelMailboxHandle) -> Vec<u64> {
    let snapshot = handle.snapshot().await.intervention_queue;
    snapshot.iter().map(|item| item.message_id.get()).collect()
}

fn open(_: ChannelId, _: Option<ChannelId>) -> bool {
    true
}

fn shut(_: ChannelId, _: Option<ChannelId>) -> bool {
    false
}

#[test]
fn mend_consumes_only_the_actors_own_unchanged_unknown_head_behind_an_open_gate() {
    let _root = root();
    let channel = ChannelId::new(6_813_451);
    inject_hook::set_pane_program(channel.get(), "/usr/bin/false".into());
    inject_hook::set_queued_head(channel.get(), true);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let handle = ChannelMailboxRegistry::default().handle(channel);
            let (h, b) = (human(451, "H"), human(452, "B"));
            for item in [&h, &b] {
                assert!(handle.enqueue(item.clone(), ctx()).await.enqueued);
            }
            let QueuedHeadClaim::Reserved(ticket) =
                handle.claim_queued_head(QueueIdentity::of(&h), ctx()).await
            else {
                panic!("H is reserved");
            };
            let settled = handle.settle_queued_head(ticket, HeadOutcome::MayHaveSent, ctx());
            let HeadSettle::Committed(Some(ticket)) = settled.await else {
                panic!("H may have been sent");
            };
            let admitted = (channel, None, open as Gate);

            // A head its live owner may still settle is not Unknown, whatever proof is found.
            let original = disk(channel)["head_injection"]["original"].clone();
            let live = UnknownHead {
                identity: QueueIdentity::of(&h),
                original: serde_json::from_value(original).unwrap(),
                attempt: None,
            };
            let stale = Err("stale".to_string());
            assert_eq!(handle.mend_queued_head(live, ctx(), admitted).await, stale);
            assert!(handle.unknown_queued_head(ctx()).await.is_none());
            let unknown = handle.settle_queued_head(ticket, HeadOutcome::Unconfirmed, ctx());
            assert!(matches!(unknown.await, HeadSettle::Unknown));
            let head = handle
                .unknown_queued_head(ctx())
                .await
                .expect("H is Unknown");

            // Another payload, a closed gate or another channel's admission changes nothing.
            let mut edited = head.clone();
            edited.original.text = "H edited".into();
            assert_eq!(
                handle.mend_queued_head(edited, ctx(), admitted).await,
                stale
            );
            let closed = Err("gate_closed".to_string());
            let shut = (channel, None, shut as Gate);
            assert_eq!(
                handle.mend_queued_head(head.clone(), ctx(), shut).await,
                closed
            );
            let elsewhere = (ChannelId::new(1), None, open as Gate);
            assert_eq!(
                handle
                    .mend_queued_head(head.clone(), ctx(), elsewhere)
                    .await,
                closed
            );
            let before = disk(channel);

            // A failed write restores the queue and the record.
            fail_queue_saves(channel, 1);
            assert!(
                handle
                    .mend_queued_head(head.clone(), ctx(), admitted)
                    .await
                    .is_err()
            );
            assert_eq!(disk(channel), before);
            assert_eq!(queue(&handle).await, [451, 452]);
            assert!(handle.unknown_queued_head(ctx()).await.is_some());

            // Unchanged and admitted, it is consumed once, durably.
            assert_eq!(
                handle.mend_queued_head(head.clone(), ctx(), admitted).await,
                Ok(())
            );
            assert_eq!(queue(&handle).await, [452]);
            let after = disk(channel);
            assert!(after["head_injection"].is_null());
            assert_eq!(after["terminal_identities"].as_array().unwrap().len(), 1);
            assert_eq!(after["terminal_identities"][0]["primary"], 451);
            assert_eq!(handle.mend_queued_head(head, ctx(), admitted).await, stale);
            assert_eq!(disk(channel), after);
        });
    inject_hook::clear(channel.get());
}
