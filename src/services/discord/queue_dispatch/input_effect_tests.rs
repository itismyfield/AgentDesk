use super::*;
use std::cell::RefCell;

struct SelectionHook {
    channel: ChannelId,
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}
thread_local! {
    static BEFORE_TAKE: RefCell<Option<SelectionHook>> = const { RefCell::new(None) };
}
pub(in crate::services::discord) fn pause_before_take(
    channel: ChannelId,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (entered, observed) = tokio::sync::oneshot::channel();
    let (resume, release) = tokio::sync::oneshot::channel();
    BEFORE_TAKE.with(|slot| {
        *slot.borrow_mut() = Some(SelectionHook {
            channel,
            entered,
            release,
        })
    });
    (observed, resume)
}
pub(super) async fn before_take(channel: ChannelId) {
    let hook = BEFORE_TAKE.with(|slot| {
        if slot
            .borrow()
            .as_ref()
            .is_some_and(|hook| hook.channel == channel)
        {
            slot.borrow_mut().take()
        } else {
            None
        }
    });
    if let Some(hook) = hook {
        hook.entered.send(()).unwrap();
        hook.release.await.unwrap();
    }
}

fn item(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 1,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("queue {id}"),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    }
}

/// A head that reaches its retry cap between selection and take goes back to the front, in
/// memory and on disk, and the automatic dequeue moves on to the eligible tail.
#[tokio::test(flavor = "current_thread")]
async fn automatic_dequeue_restores_a_head_capped_after_selection_and_takes_the_tail() {
    use super::super::busy_followup_retry_store::{MAX_BUSY_RETRY_COUNT, record_busy_retry};
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let shared = make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(6_325_431);
    let (head, tail) = (item(6_325_432), item(6_325_433));
    shared
        .mailbox(channel)
        .replace_queue(
            vec![head.clone(), tail.clone()],
            persistence_context(&shared, &provider, channel),
        )
        .await;
    for _ in 1..MAX_BUSY_RETRY_COUNT {
        record_busy_retry(&provider, channel.get(), 6_325_432, 6_325_434).unwrap();
    }
    let (entered, release) = pause_before_take(channel);
    let select = mailbox_take_next_automatic_intervention(&shared, &provider, channel);
    let cap = async {
        entered.await.unwrap();
        let decision = record_busy_retry(&provider, channel.get(), 6_325_432, 6_325_434).unwrap();
        assert!(
            decision.capped,
            "the selected head reaches its cap before the take"
        );
        release.send(()).unwrap();
    };
    let (selected, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(select, cap)
    })
    .await
    .unwrap();
    assert_eq!(
        selected.intervention.as_ref().map(|item| item.message_id),
        Some(tail.message_id)
    );
    assert!(selected.persistence_error.is_none());
    let snapshot = shared.mailbox(channel).snapshot().await;
    let queued: Vec<_> = snapshot
        .intervention_queue
        .iter()
        .map(|item| item.message_id)
        .collect();
    assert_eq!(queued, [head.message_id]);
    assert_eq!(snapshot.pending_user_dispatch, Some(tail.message_id));
    let (disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
        &provider,
        &shared.token_hash,
        channel,
    );
    let on_disk: Vec<_> = disk.iter().map(|item| item.message_id).collect();
    assert_eq!(on_disk, [head.message_id]);
    shared.mailboxes.remove_fixture_for_test(channel);
}
