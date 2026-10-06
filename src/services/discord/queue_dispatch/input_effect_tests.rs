use super::super::input_runtime::fence::{self, Gate, Mode, effect};
use super::*;
use futures::FutureExt;
use std::cell::RefCell;

struct SelectionHook {
    channel: ChannelId,
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}
thread_local! {
    static SELECTION: RefCell<Option<SelectionHook>> = const { RefCell::new(None) };
}
struct Reset;
impl Drop for Reset {
    fn drop(&mut self) {
        SELECTION.with(|slot| *slot.borrow_mut() = None);
    }
}
pub(super) async fn after_selection(channel: ChannelId) {
    let hook = SELECTION.with(|slot| {
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
        voice_announcement: None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn c1_automatic_dequeue_restores_capped_head_with_original_effect_after_closing() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let shared = make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(6_325_431);
    let head = item(6_325_432);
    let tail = item(6_325_433);
    let gate = Gate::protect(provider.clone(), channel.get()).unwrap();
    let _health = fence::test_health::Clear::new(&gate);
    shared
        .mailbox(channel)
        .replace_queue(
            vec![head.clone(), tail.clone()],
            persistence_context(&shared, &provider, channel),
        )
        .await;
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    SELECTION.with(|slot| {
        *slot.borrow_mut() = Some(SelectionHook {
            channel,
            entered: entered_tx,
            release: release_rx,
        })
    });
    let _reset = Reset;
    let cap_writer = gate.admit().unwrap();
    let select = mailbox_take_next_automatic_intervention(&shared, &provider, channel);
    let observe = async {
        entered_rx.await.unwrap();
        let closing = gate.close().unwrap();
        assert!(
            closing.drain().now_or_never().is_none(),
            "selection owns the effect before restore"
        );
        // An independently admitted cap writer races the selected head.
        let permit = effect::admit(&provider, channel.get());
        assert!(matches!(permit, Err(fence::Failure::Mode(Mode::Closing))));
        let writer_provider = provider.clone();
        effect::scope(
            Some(cap_writer),
            effect::io(move || {
                for _ in 0..super::super::busy_followup_retry_store::MAX_BUSY_RETRY_COUNT {
                    super::super::busy_followup_retry_store::record_busy_retry(
                        &writer_provider,
                        channel.get(),
                        6_325_432,
                        6_325_434,
                    )
                    .unwrap();
                }
            }),
        )
        .await;
        assert!(
            closing.drain().now_or_never().is_none(),
            "dequeue remains admitted after the cap writer completes"
        );
        release_tx.send(()).unwrap();
        closing
    };
    let (selected, closing) = tokio::join!(select, observe);
    assert_eq!(
        selected.intervention.as_ref().map(|item| item.message_id),
        Some(tail.message_id)
    );
    assert!(selected.persistence_error.is_none());
    closing.drain().await;
    let snapshot = shared.mailbox(channel).snapshot().await;
    assert_eq!(snapshot.intervention_queue.len(), 1);
    assert_eq!(snapshot.intervention_queue[0].message_id, head.message_id);
    assert_eq!(snapshot.pending_user_dispatch, Some(tail.message_id));
    let (disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
        &provider,
        &shared.token_hash,
        channel,
    );
    assert_eq!(disk.len(), 1);
    assert_eq!(disk[0].message_id, head.message_id);
    shared.mailboxes.remove_fixture_for_test(channel);
}

#[tokio::test(flavor = "current_thread")]
async fn c1_closing_refuses_kickoff_before_settings_and_dequeue_without_spending_marker() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let shared = make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(6_325_435);
    let head = item(6_325_436);
    shared
        .mailbox(channel)
        .replace_queue(
            vec![head.clone()],
            persistence_context(&shared, &provider, channel),
        )
        .await;
    let path = root.path().join(format!(
        "runtime/discord_pending_queue/claude/{}/{}.json",
        shared.token_hash,
        channel.get()
    ));
    let before = std::fs::read(&path).unwrap();
    let gate = Gate::protect(provider.clone(), channel.get()).unwrap();
    let _health = fence::test_health::Clear::new(&gate);
    let closing = gate.close().unwrap();
    let settings = shared.settings.write().await;
    let http = Arc::new(serenity::Http::new("Bot C1-no-network"));
    let deps = router::IntakeDeps {
        http: &http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared: &shared,
        token: "C1-no-network",
    };
    let result = kickoff::kickoff_idle_queue_channel(&deps, &provider, channel)
        .now_or_never()
        .expect("Closing refusal occurs before the held settings lock");
    assert!(!result.started);
    drop(settings);
    let soft = mailbox_take_next_soft_intervention(&shared, &provider, channel).await;
    assert!(soft.intervention.is_none());
    let automatic = mailbox_take_next_automatic_intervention(&shared, &provider, channel).await;
    assert!(automatic.intervention.is_none());
    let snapshot = shared.mailbox(channel).snapshot().await;
    assert_eq!(snapshot.intervention_queue[0].message_id, head.message_id);
    assert!(snapshot.pending_user_dispatch.is_none());
    assert_eq!(std::fs::read(path).unwrap(), before);
    closing.drain().await;
    shared.mailboxes.remove_fixture_for_test(channel);
}
