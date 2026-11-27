use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::{
    inflight::{self, GuardedClearOutcome, InflightTurnIdentity},
    queue_io::input_owner::drive::{Drive, Front, claim_turn},
    router::message_handler::owner_drive_for_test,
    turn_completion_events::{TurnCompletionEvent, subscribe_turn_completion_events},
    turn_finalizer::{FinalizeContext, FinalizeOutcome, TerminalEvent},
};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::broadcast::error::TryRecvError;

const INTERRUPTED: &str = r#"{"type":"user","uuid":"stop","sessionId":"native","message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#;

fn save_origin(f: &Fixture) -> inflight::InflightTurnState {
    let mut row = b2b_row(f);
    row.input_origin_ids = vec!["native-fixture-origin".into()];
    inflight::save_inflight_state(&row).unwrap();
    inflight::load_inflight_state_read_only(&ProviderKind::Claude, f.ticket.channel_id).unwrap()
}

fn remove_projection(row: &inflight::InflightTurnState) {
    assert_eq!(
        inflight::clear_inflight_state_if_matches_identity_turn_nonce(
            &ProviderKind::Claude,
            row.channel_id,
            &InflightTurnIdentity::from_state(row),
            row.turn_nonce.as_deref(),
        ),
        GuardedClearOutcome::Cleared
    );
}

async fn watcher_complete(
    shared: &Arc<SharedData>,
    key: TurnKey,
    claim: Option<SyntheticClaimSnapshot>,
) {
    let outcome = shared
        .turn_finalizer
        .submit_terminal_with_claim_snapshot(
            TurnKey::new(key.channel_id, key.user_msg_id, key.generation),
            ProviderKind::Claude,
            TerminalEvent::Complete,
            FinalizeContext::watcher(),
            claim,
            shared.clone(),
        )
        .await;
    assert!(matches!(outcome, FinalizeOutcome::Deferred));
}

async fn machine_front(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    root: &Path,
) -> (Arc<Drive>, Front, Arc<AtomicUsize>) {
    let mailbox = shared.mailbox(channel);
    let id = channel.get();
    let message = id + 2;
    mailbox
        .v2_adopt(root.join(format!("front-{id}.json")), format!("owner-{id}"))
        .await
        .unwrap();
    let input = serde_json::from_value(serde_json::json!({
        "author_id":7,"message_id":message,"channel_id":id,
        "source_message_ids":[message],"queued_generation":1,"text":"machine fixture"
    }))
    .unwrap();
    mailbox
        .v2_admit_ticket_with_context(
            input,
            format!("ticket-{id}"),
            serde_json::json!({"origin":"headless"}),
        )
        .await
        .unwrap();
    let front = mailbox
        .v2_request(|slot| {
            let owner = slot.as_mut().unwrap();
            let front = owner.front().unwrap();
            owner.grant_front(&front)?;
            Ok(front)
        })
        .await
        .unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let deliveries = count.clone();
    let owned = mailbox.clone();
    let drive = owner_drive_for_test(
        (mailbox, None),
        (ProviderKind::Claude, id),
        Arc::new(move |front| {
            let mailbox = owned.clone();
            let deliveries = deliveries.clone();
            Box::pin(async move {
                assert_eq!(front.1.message_id, message);
                claim_turn(&mailbox, message, &front)
                    .await?
                    .ok_or("the idle front was not granted")?;
                mailbox.v2_request(move |slot| {
                    let owner = slot.as_mut().unwrap();
                    let attempt = serde_json::from_value(serde_json::json!({
                        "nonce":format!("next-{id}"),"provider":"claude",
                        "target":{"host":"herdr","owner":"next-pane","root":"/test","execution_nonce":"next-epoch"},
                        "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}
                    }))
                    .map_err(|error| error.to_string())?;
                    let key = owner.reserve_unit(attempt, |item| Some(("user".into(), item.text.clone())))?;
                    drop(owner.arm(&key, 1)?);
                    owner.settle(&key, true, 2)
                })
                .await?;
                deliveries.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }),
        None,
    );
    (drive, front, count)
}

fn fixture(runtime: &Path, rig: &HerdrRig, channel: u64) -> Fixture {
    Fixture::at_target(
        runtime
            .join("runtime")
            .join(format!("herdr-{channel}.json")),
        channel,
        true,
        serde_json::json!({"host":"herdr","owner":"fixture-pane","execution_nonce":NONCE,"root":rig.socket()}),
    )
}

async fn terminal_orders(closer: &str, disposition: Disposition, receipt: &str, channel: u64) {
    let root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    for (id, native_first) in [(channel, false), (channel + 10, true)] {
        let f = fixture(root.path(), &rig, id);
        let path = &f.ticket.source.as_ref().unwrap().transcript;
        fs::write(path, format!("{OWN_ROW}\n{closer}\n")).unwrap();
        let shared = b2b_shared(Some(pool.clone()));
        let (key, actor) = b2b_active(&f, &shared).await;
        let row = save_origin(&f);
        supply_register(&f, &shared, key, Some(actor.clone())).await;
        barrier_receipt(&f);
        let mut events = subscribe_turn_completion_events(&shared);
        let mut capture = SupplyCapture::new(&f, &root.path().join(format!("store-{id}")), true);
        let (drive, front, deliveries) = machine_front(&shared, key.channel_id, root.path()).await;

        remove_projection(&row);
        let claim = || Some(SyntheticClaimSnapshot::from_row(&row));
        assert_eq!(
            (drive.handoff)(front.clone()).await,
            Err("NotSent(\"synthetic_waits_idle\")".into())
        );
        assert_eq!(deliveries.load(Ordering::SeqCst), 0);
        if !native_first {
            watcher_complete(&shared, key, None).await;
            watcher_complete(&shared, key, claim()).await;
            let snapshot = shared.mailbox(key.channel_id).snapshot().await;
            assert!(Arc::ptr_eq(snapshot.cancel_token.as_ref().unwrap(), &actor));
            assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 1);
            assert_eq!(events.try_recv(), Err(TryRecvError::Empty));
        }

        capture.poll(&shared).await;
        assert!(
            !shared
                .mailbox(key.channel_id)
                .has_active_turn()
                .await
                .unwrap(),
            "{disposition:?} must release its actor, native_first={native_first}"
        );
        assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 0);
        let settled = f
            .history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap();
        assert!(settled.cleanup_complete);
        assert_eq!(&settled.outcome.as_ref().unwrap().disposition, &disposition);
        let pg_receipt: (Vec<String>, String) = sqlx::query_as(
            "SELECT consumed_origin_ids, terminal_outcome FROM turns WHERE turn_id = $1",
        )
        .bind(format!("discord:{id}:{}", row.user_msg_id))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            pg_receipt,
            (vec!["native-fixture-origin".into()], receipt.into())
        );
        assert_eq!(
            events.try_recv(),
            Ok(TurnCompletionEvent::for_turn(
                key.channel_id,
                key.user_msg_id
            ))
        );
        (drive.handoff)(front).await.unwrap();
        assert_eq!(deliveries.load(Ordering::SeqCst), 1);
        if native_first {
            watcher_complete(&shared, key, None).await;
            watcher_complete(&shared, key, claim()).await;
        }
        capture.poll(&shared).await;
        assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 0);
        assert_eq!(events.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            f.history
                .native_settlement("native-fixture", &f.ticket)
                .unwrap(),
            settled
        );
        assert!(
            inflight::load_inflight_state_read_only_result(&ProviderKind::Claude, id)
                .unwrap()
                .is_none()
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn herdr_rowless_wait_executed_and_watcher_complete_in_either_order_pg() {
    terminal_orders(RAW_TAIL, Disposition::Executed, "completed", 845_706_101).await;
}

#[tokio::test]
async fn herdr_rowless_wait_aborted_and_watcher_complete_in_either_order_pg() {
    terminal_orders(
        INTERRUPTED,
        Disposition::Cancelled,
        "cancelled",
        845_706_102,
    )
    .await;
}

#[derive(Clone, Copy)]
enum Held {
    Corrupt,
    Unpinned,
    Withdrawal,
}

async fn held_case(shape: Held, channel: u64) {
    let root = crate::config::TestRuntimeRootGuard::new();
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let f = fixture(root.path(), &rig, channel);
    if matches!(shape, Held::Withdrawal) {
        supply_rows(&f, true, true);
    } else {
        fs::write(
            &f.ticket.source.as_ref().unwrap().transcript,
            format!("{OWN_ROW}\n{RAW_TAIL}\n"),
        )
        .unwrap();
    }
    let shared = b2b_shared(None);
    let (key, actor) = b2b_active(&f, &shared).await;
    let registered_row = (!matches!(shape, Held::Unpinned)).then(|| b2b_row(&f));
    supply_register(&f, &shared, key, Some(actor.clone())).await;
    if !matches!(shape, Held::Withdrawal) {
        barrier_receipt(&f);
    }
    let row_path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        channel,
    );
    match shape {
        Held::Corrupt => fs::write(&row_path, b"{broken").unwrap(),
        Held::Unpinned | Held::Withdrawal => {
            remove_projection(&registered_row.unwrap_or_else(|| b2b_row(&f)));
        }
    }
    let mut events = subscribe_turn_completion_events(&shared);
    let mut capture = SupplyCapture::new(&f, &root.path().join("held-store"), true);
    capture.poll(&shared).await;
    let saved = f
        .history
        .native_settlement("native-fixture", &f.ticket)
        .unwrap();
    assert!(saved.outcome.is_none() && !saved.cleanup_complete);
    let snapshot = shared.mailbox(key.channel_id).snapshot().await;
    assert!(Arc::ptr_eq(snapshot.cancel_token.as_ref().unwrap(), &actor));
    assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 1);
    assert_eq!(events.try_recv(), Err(TryRecvError::Empty));
    if matches!(shape, Held::Corrupt) {
        assert_eq!(fs::read(row_path).unwrap(), b"{broken");
    }
}

#[tokio::test]
async fn herdr_rowless_wait_corrupt_projection_is_not_absence() {
    held_case(Held::Corrupt, 845_706_103).await;
}

#[tokio::test]
async fn herdr_rowless_wait_registration_before_the_row_cannot_release() {
    held_case(Held::Unpinned, 845_706_104).await;
}

#[tokio::test]
async fn herdr_rowless_wait_withdrawal_cannot_use_the_prepared_projection() {
    held_case(Held::Withdrawal, 845_706_105).await;
}
