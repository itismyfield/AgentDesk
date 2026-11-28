use crate::services::discord::inflight::{
    InflightTurnState, TurnSource, inflight_runtime_root, save_inflight_state_in_root,
};
use crate::services::discord::inflight::{
    RebindReapOutcome, reap_abandoned_rebind_origin_for_test,
};
use crate::services::provider::ProviderKind;
// The boot reaper's reap gives no notice of its own; the reconcile that follows wakes every
// installed owner once.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_boot_reap_gives_no_notice_and_the_reconcile_wakes_the_owner_once() {
    use crate::services::discord::queue_io::cancel_backstop_test_support as support;
    use crate::services::discord::queue_io::input_owner::admission::{AdmissionTicket, admit};
    use crate::services::turn_orchestrator::{PendingQueueItem, QueuePersistenceContext, v2_queue};
    use adk_test_support::test_support::wait::{WAIT_CAP, await_within};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let (id, generation) = (7_753_610u64, 5);
    let channel = poise::serenity_prelude::ChannelId::new(id);
    let mut row = InflightTurnState::new(
        provider.clone(),
        id,
        None,
        0,
        0,
        0,
        String::new(),
        None,
        Some("c3-boot-reap".to_string()),
        None,
        None,
        0,
    );
    row.rebind_origin = true;
    row.turn_source = TurnSource::ExternalAdopted;
    row.turn_start_offset = Some(0);
    row.rebind_origin_birth_generation = Some(generation - 1);
    let root = inflight_runtime_root().expect("runtime root");
    save_inflight_state_in_root(&root, &row).expect("persist the orphan");
    let mailbox = shared.mailbox(channel);
    let context = QueuePersistenceContext::new(&provider, &shared.token_hash, None);
    let (path, namespace) = v2_queue(&context, channel).unwrap();
    mailbox.v2_adopt(path, namespace).await.unwrap();
    let front: PendingQueueItem = serde_json::from_value(serde_json::json!({
        "author_id": 7, "message_id": id + 3, "channel_id": id,
        "source_message_ids": [id + 3], "queued_generation": 1, "text": "held front"}))
    .unwrap();
    let _ = admit(&mailbox, &front, AdmissionTicket::new()).await;
    let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = wakes.clone();
    let hook = std::sync::Arc::new(move || {
        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    support::set_owner_wake_for_tests(channel, Some(hook));
    let completion = support::KickoffCompletion::install(channel);

    let reaped = reap_abandoned_rebind_origin_for_test(&root, &provider, &row, generation);
    await_within(completion.processed(0, WAIT_CAP), WAIT_CAP).await;
    let before = wakes.load(std::sync::atomic::Ordering::SeqCst);
    crate::services::discord::runtime_bootstrap::complete_active_reconcile(&shared).await;
    let after = wakes.load(std::sync::atomic::Ordering::SeqCst);
    support::set_owner_wake_for_tests(channel, None);
    assert_eq!(
        reaped,
        RebindReapOutcome::Reaped,
        "the boot reap removes the orphan"
    );
    assert_eq!(
        (before, after),
        (0, 1),
        "only the reconcile wakes the owner"
    );
}
