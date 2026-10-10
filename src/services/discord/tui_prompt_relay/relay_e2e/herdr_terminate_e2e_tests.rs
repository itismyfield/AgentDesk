//! #5340 P11 M2-A (P2-2): a real message that arrives once the terminated execution's hold is
//! gone waits through settlement, and the queue listener, run first, hands it on unconsumed.
//!
//! Intake runs with cluster intake routing `disabled` (single node, this node owns the channel),
//! which is what this fixture verifies; the witness that intake ran is the queued intervention
//! carrying the message's own id and text. The Herdr start after the hand-off is not asserted
//! here: the queued-turn promote gate defers every non-Legacy host row (#5340 M2-A report).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use poise::serenity_prelude::MessageId;

use super::discord_mock::user_message;
use super::{CHANNEL_ID, RelayE2eHarness, serenity, wait_until};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::herdr_terminate::{
    OperatorTerminate, TerminationResult, probe_after_settlement, probe_settlement_window,
    terminate_explicit_herdr, test_turn,
};
use crate::services::discord::queue_io::cancel_backstop_test_support as observed;
use crate::services::discord::router;
use crate::services::provider::ProviderKind;
use crate::services::session_host::herdr_termination_rig as herdr;

const ORIGINAL: &str = "input arriving inside the settlement window [5340-m2a]";

fn recent_message_id(low: u64) -> MessageId {
    const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
    let recent_ms = chrono::Utc::now().timestamp_millis() - 30_000 - DISCORD_EPOCH_MS;
    MessageId::new((u64::try_from(recent_ms).unwrap() << 22) | low)
}

/// One run of the 1621 order: active A with matching inflight and a Bound PG row, the queue
/// listener started and its initial reconcile done, terminate, the message arriving in the
/// settlement window through `router::handle_event`, then settlement while the listener runs
/// first against the still-held transition.
#[test]
fn m2_1621_real_intake_waits_through_settlement_and_listener_hands_off_pg() {
    let path = concat!(
        module_path!(),
        "::m2_1621_real_intake_waits_through_settlement_and_listener_hands_off_pg"
    );
    if !herdr::in_child(path, CHANNEL_ID) {
        return;
    }
    let rig = herdr::P11TerminationRig::boot(CHANNEL_ID);
    let install = rig.thread_installer();
    install();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .on_thread_start(install)
        .build()
        .unwrap()
        .block_on(scenario(rig));
}

async fn scenario(rig: herdr::P11TerminationRig) {
    let mut database = None;
    let harness = RelayE2eHarness::start_bound_on(async {
        let fixture = TestPostgresDb::create().await;
        let pool = fixture.connect_and_migrate().await;
        database = Some(fixture);
        pool
    })
    .await;
    let database = database.expect("an isolated PostgreSQL database");
    let pool = harness.shared.pg_pool.clone().expect("PG-backed runtime");
    let (shared, channel) = (harness.shared.clone(), harness.channel_id);
    harness.answer_placeholders_immediately();
    harness.cache_relay_transport();
    shared.restart.reconcile_done.store(true, Ordering::SeqCst);
    let previous = shared
        .core
        .lock()
        .await
        .sessions
        .get_mut(&channel)
        .expect("bound fixture channel")
        .channel_name
        .replace("p11-m2-herdr".into());
    assert!(previous.is_none());
    std::mem::forget(
        crate::services::claude_tui::hook_server::publish_hook_endpoint(
            "http://127.0.0.1:1".into(),
        ),
    );
    let (store, era) =
        crate::services::herdr_launch::o_store_for_test(harness.root.path(), &[CHANNEL_ID]);
    let mut seeded = store.open_channel(&era, CHANNEL_ID).unwrap().unwrap();
    seeded.set_binding_checkpoint(3).unwrap();

    let provider = ProviderKind::Claude;
    let session_key = crate::services::discord::adk_session::build_adk_session_key(
        &shared, channel, &provider, None,
    )
    .await
    .expect("a namespaced session key");
    let _switches = rig.bind(&pool, &shared.token_hash, &session_key).await;
    let record = rig.record(&shared.token_hash, &session_key);
    let token_a = test_turn::start(&shared, channel, 51, "turn-a").await;

    // The listener runs its initial reconcile before anything else can wake it.
    crate::services::discord::queue_io::spawn_turn_completion_idle_queue_listener(
        shared.clone(),
        provider.clone(),
    );
    assert!(
        wait_until(Duration::from_secs(15), || {
            Box::pin(async move { observed::listener_completed_reconciles(channel) >= 1 })
        })
        .await,
        "the listener finished its initial reconcile"
    );
    assert!(!observed::backstop_waiting(channel));

    let queued = recent_message_id(701);
    let window = Arc::new(Mutex::new(None));
    let (ctx, data, probe_data_shared) =
        (harness.ctx.clone(), harness.clone_data(), shared.clone());
    let (probe_window, probe_record) = (window.clone(), record.clone());
    probe_settlement_window(Box::new(move || {
        Box::pin(async move {
            let held = herdr::held();
            let event = serenity::FullEvent::Message {
                new_message: user_message(queued.get(), ORIGINAL),
            };
            let intake = router::handle_event(&ctx, &event, &data).await;
            assert!(intake.is_ok(), "{intake:?}");
            let delivered = wait_until(Duration::from_secs(15), || {
                let shared = probe_data_shared.clone();
                Box::pin(async move {
                    crate::services::discord::mailbox_snapshot(&shared, channel)
                        .await
                        .intervention_queue
                        .iter()
                        .any(|item| item.message_id == queued)
                })
            })
            .await;
            let snapshot =
                crate::services::discord::mailbox_snapshot(&probe_data_shared, channel).await;
            let queue: Vec<_> = snapshot
                .intervention_queue
                .iter()
                .map(|item| {
                    (
                        item.message_id,
                        item.source_message_ids.clone(),
                        item.text.clone(),
                    )
                })
                .collect();
            *probe_window.lock().unwrap() =
                Some((held, delivered, queue, herdr::input_pin(&probe_record)));
        })
    }));
    let listener_first = Arc::new(AtomicBool::new(false));
    let probe_listener_first = listener_first.clone();
    probe_after_settlement(Box::new(move || {
        Box::pin(async move {
            // The listener takes QueueEligible while the transition is still held: its kickoff
            // must not start, and the queued message is handed to the event backstop.
            let waited = wait_until(Duration::from_secs(15), || {
                Box::pin(async move { observed::backstop_waiting(channel) })
            })
            .await;
            probe_listener_first.store(waited, Ordering::SeqCst);
        })
    }));

    let result = terminate_explicit_herdr(
        OperatorTerminate {
            session_key: session_key.clone(),
            execution_nonce: herdr::EXECUTION.into(),
        },
        shared.clone(),
        &pool,
    )
    .await;
    assert_eq!(result, TerminationResult::Retired);
    let (held_in_window, delivered, queue, pin) = window
        .lock()
        .unwrap()
        .take()
        .expect("the settlement window ran");
    assert!(!held_in_window, "the window opens after the hold release");
    assert!(delivered, "intake queued the message inside the window");
    assert_eq!(
        queue,
        vec![(queued, vec![queued], ORIGINAL.to_string())],
        "the original text and its source identity wait in the queue"
    );
    assert!(pin.is_err(), "the execution fence refuses input: {pin:?}");
    assert_eq!(
        rig.writes(),
        vec![("pane.close".to_string(), "w1-1".to_string())],
        "nothing but the close reached Herdr before settlement"
    );
    assert_eq!(harness.provider_starts(), 0);
    assert!(
        test_turn::active(&shared, channel).await.is_none(),
        "A was settled and nothing else holds the channel"
    );
    assert!(!token_a.cancelled.load(Ordering::Relaxed));

    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    assert!(
        listener_first.load(Ordering::SeqCst),
        "the listener took QueueEligible and handed the no-start to the event backstop before \
         the transition was released"
    );
    // Released, the message is neither lost nor consumed without a start.
    let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel).await;
    let waiting = snapshot
        .intervention_queue
        .iter()
        .any(|item| item.message_id == queued && item.text == ORIGINAL);
    let durable = harness
        .durable_queue()
        .iter()
        .any(|item| item.message_id == queued && item.text == ORIGINAL);
    let launches = rig
        .writes()
        .iter()
        .filter(|(method, _)| method == "workspace.create")
        .count();
    assert!(
        (waiting && durable && launches == 0) || launches == 1,
        "waiting={waiting} durable={durable} writes={:?}",
        rig.writes()
    );
    drop(harness);
    pool.close().await;
    database.drop().await;
}
