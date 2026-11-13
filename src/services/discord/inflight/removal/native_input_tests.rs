use super::nondestructive_loader_tests::{CLAUDE, Env, STALE, row};
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::queue_io::input_owner::drive::{self, Drive, Front};
use crate::services::provider::CancelToken;
use crate::services::tui_input::human_frame::history::v2::{
    Registry, native, native_tests::Fixture,
};
use crate::services::turn_orchestrator::{PendingQueueItem, QueuePersistenceContext};
use crate::test_support::wait::{WAIT_CAP, await_within};
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn native_8148_follower_results_and_notice_preserve_parent_pg() {
    let env = Env::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    {
        let root = env.dir().parent().unwrap().to_path_buf();
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        for (n, kind) in [
            native::Disposition::Executed,
            native::Disposition::WithdrawnNotExecuted,
        ]
        .into_iter()
        .enumerate()
        {
            let channel = 8148100 + n as u64;
            let f = Fixture::at(root.join(format!("follower-{n}.json")), channel, false);
            let mut parent = row(channel, None);
            parent.user_msg_id = channel + 100;
            parent.turn_nonce = Some("parent-episode".into());
            let path = env.seed(&parent, 0);
            let before = fs::read(&path).unwrap();
            let token = Arc::new(CancelToken::from_persisted_turn_nonce(
                parent.turn_nonce.clone(),
            ));
            let mailbox = shared.mailbox(ChannelId::new(channel));
            mailbox
                .restore_active_turn(
                    token.clone(),
                    UserId::new(7),
                    MessageId::new(parent.user_msg_id),
                )
                .await;
            shared.restart.global_active.store(1, Ordering::SeqCst);
            let outcome = Fixture::outcome(kind.clone());
            let mut outcomes = 0;
            let mut notices = 0;
            for _ in 0..2 {
                outcomes += usize::from(
                    f.history
                        .record_native_outcome("native-fixture", &f.ticket, &outcome)
                        .unwrap(),
                );
                if kind == native::Disposition::WithdrawnNotExecuted {
                    crate::services::message_outbox::enqueue_outbox_pg_returning_id_with_persistent_dedupe(
                        &pool, crate::services::message_outbox::OutboxMessage {
                            target: &format!("channel:{channel}"), content:"입력이 실행 전에 철회되었습니다.", bot:"claude",
                            source:"retained_input_notice", reason_code:None,
                            session_key:Some(&format!("native-withdrawal:{channel}")),
                        }).await.unwrap();
                    notices += usize::from(
                        f.history
                            .ack_native_withdrawal_notice("native-fixture", &f.ticket)
                            .unwrap(),
                    );
                }
            }
            assert_eq!(outcomes, 1);
            assert_eq!(
                notices,
                usize::from(kind == native::Disposition::WithdrawnNotExecuted)
            );
            let stored: i64 =
                sqlx::query_scalar("SELECT count(*) FROM message_outbox WHERE target=$1")
                    .bind(format!("channel:{channel}"))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(stored, notices as i64);
            let after = mailbox.snapshot().await;
            assert!(Arc::ptr_eq(after.cancel_token.as_ref().unwrap(), &token));
            assert_eq!(
                after.active_user_message_id,
                Some(MessageId::new(parent.user_msg_id))
            );
            assert_eq!(after.active_turn_nonce, parent.turn_nonce);
            assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 1);
            assert_eq!(fs::read(path).unwrap(), before);
        }
    }
    pool.close().await;
    db.drop().await;
}

async fn next_head(
    mailbox: crate::services::turn_orchestrator::ChannelMailboxHandle,
    channel: u64,
    root: &Path,
) {
    mailbox
        .v2_adopt(
            root.join(format!("next-{channel}.json")),
            format!("next-{channel}"),
        )
        .await
        .unwrap();
    let original: PendingQueueItem = serde_json::from_value(json!({"author_id":7,"message_id":channel+2,
        "channel_id":channel,"source_message_ids":[channel+2],"queued_generation":1,"text":"다음 원본"})).unwrap();
    mailbox
        .v2_admit_ticket(original, format!("admission-{channel}"))
        .await
        .unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let hand_count = count.clone();
    let hand_mailbox = mailbox.clone();
    let (done, completion) = tokio::sync::oneshot::channel();
    let done = Arc::new(std::sync::Mutex::new(Some(done)));
    let drive = Arc::new(Drive {
        mailbox,
        channel,
        handoff: Arc::new(move |front: Front| {
            let mailbox = hand_mailbox.clone();
            let count = hand_count.clone();
            Box::pin(async move {
                let _claim = drive::claim_turn(&mailbox, front.1.message_id, &front)
                    .await?
                    .ok_or("claim missing")?;
                mailbox.v2_request(move |slot| {
                let owner = slot.as_mut().ok_or("owner missing")?;
                let attempt = serde_json::from_value(json!({"nonce":format!("next-{channel}"),"provider":"claude",
                    "target":{"host":"herdr","owner":"next-pane","root":"/test","execution_nonce":"next-epoch"},
                    "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}}))
                    .map_err(|error| error.to_string())?;
                let key = owner.reserve_unit(attempt, |item| Some(("user".into(), item.text.clone())))?;
                drop(owner.arm(&key, 1)?);
                owner.settle(&key, true, 2)
            }).await?;
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }),
        settled: Some(Arc::new(move || {
            let done = done.clone();
            Box::pin(async move {
                if let Some(done) = done.lock().unwrap().take() {
                    let _ = done.send(());
                }
            })
        })),
    });
    drive::wake(drive.clone());
    drive::wake(drive);
    await_within(completion, WAIT_CAP).await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn native_8148_boot_preparation_reaper_and_owner_drive_recover_both_crashes_pg() {
    let env = Env::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for already_recorded in [false, true] {
        let root = env.dir().parent().unwrap().to_path_buf();
        let channel = 8148200 + u64::from(already_recorded);
        let f = Fixture::at(root.join("injected_frames.json"), channel, true);
        let mut outcome = Fixture::outcome(native::Disposition::Executed);
        outcome
            .extra
            .insert("native_proof".into(), json!({"opener":17,"closer":41}));
        if already_recorded {
            assert!(
                f.history
                    .record_native_outcome("native-fixture", &f.ticket, &outcome)
                    .unwrap()
            );
        }
        let mut snapshot = f.history.json().unwrap();
        let registration = snapshot["live"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|bound| {
                bound["registration"]["native_settlement"]["ticket"]["channel_id"] == channel
            })
            .map(|bound| &mut bound["registration"])
            .unwrap();
        registration["native_settlement"]["proof"] = json!({"source_incarnation":"incarnation-1"});
        registration["native_settlement"]["warned"] = json!(true);
        let expected = json!({"namespace":registration["namespace"],
            "settlement":registration["native_settlement"],"registration":registration});
        fs::write(root.join("injected_frames.json"), snapshot.to_string()).unwrap();
        let mut old = row(channel, None);
        old.born_generation = 1;
        old.turn_nonce = Some(f.ticket.owning_episode.as_ref().unwrap().turn_nonce.clone());
        let path = env.seed(&old, STALE);
        let report =
            reap_inflight_rows_at_boot_with_guard(&BootReapOnce::default(), &CLAUDE, None).await;
        assert_eq!(report.reaped_stale, 1);
        assert!(!path.exists());
        let custody = root.join("discord_custody/claude");
        let copies: Vec<_> = fs::read_dir(custody)
            .unwrap()
            .flatten()
            .flat_map(|episode| {
                fs::read_dir(episode.path())
                    .unwrap()
                    .flatten()
                    .filter(|p| p.file_name().to_string_lossy().starts_with("rev-"))
                    .flat_map(|rev| fs::read_dir(rev.path()).unwrap().flatten())
                    .collect::<Vec<_>>()
            })
            .collect();
        let native_copies: Vec<_> = copies
            .iter()
            .filter(|copy| {
                copy.file_name()
                    .to_string_lossy()
                    .ends_with("-native_input.json")
                    && serde_json::from_slice::<serde_json::Value>(&fs::read(copy.path()).unwrap())
                        .unwrap()["settlement"]["ticket"]["channel_id"]
                        == channel
            })
            .collect();
        assert_eq!(
            native_copies.len(),
            1,
            "native ticket custody precedes row deletion"
        );
        let recovered: serde_json::Value =
            serde_json::from_slice(&fs::read(native_copies[0].path()).unwrap()).unwrap();
        assert_eq!(
            recovered, expected,
            "custody preserves the complete BootRecord"
        );
        let reopened = Registry::at(root.join("injected_frames.json"));
        let first = reopened
            .record_native_outcome("native-fixture", &f.ticket, &outcome)
            .unwrap();
        assert_eq!(usize::from(first) + usize::from(already_recorded), 1);
        assert!(
            !reopened
                .record_native_outcome("native-fixture", &f.ticket, &outcome)
                .unwrap()
        );
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let mailbox = shared.mailbox(ChannelId::new(channel));
        let token = Arc::new(CancelToken::from_persisted_turn_nonce(
            old.turn_nonce.clone(),
        ));
        mailbox
            .restore_active_turn(
                token.clone(),
                UserId::new(7),
                MessageId::new(old.user_msg_id),
            )
            .await;
        let finish = mailbox
            .finish_turn_if_matches_episode_and_actor_started_before(
                MessageId::new(old.user_msg_id),
                old.turn_nonce,
                std::time::Instant::now(),
                Some(token),
                QueuePersistenceContext::new(&CLAUDE, "native-fixture", None),
            )
            .await;
        assert!(finish.removed_token.is_some());
        next_head(mailbox, channel, &root).await;
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn native_8148_failed_ticket_custody_preserves_row_and_retry_uses_same_boot_gate() {
    let env = Env::new();
    let root = env.dir().parent().unwrap().to_path_buf();
    let channel = 8148300;
    let _f = Fixture::at(root.join("injected_frames.json"), channel, true);
    let path = env.seed(&row(channel, None), STALE);
    let blocker = root.join("discord_custody");
    fs::write(&blocker, "not a directory").unwrap();
    let guard = BootReapOnce::default();
    let failed = reap_inflight_rows_at_boot_with_guard(&guard, &CLAUDE, None).await;
    assert!(
        path.exists(),
        "failed ticket custody must not unlink the old row"
    );
    assert_eq!(failed.incomplete, 1);
    fs::remove_file(blocker).unwrap();
    let retried = reap_inflight_rows_at_boot_with_guard(&guard, &CLAUDE, None).await;
    assert!(
        !retried.already_ran,
        "failed preparation did not consume the boot pass"
    );
    assert_eq!(retried.reaped_stale, 1);
    assert!(!path.exists());
}

#[tokio::test]
async fn native_8148_malformed_v3_preserves_row_and_retries_same_boot_gate() {
    for (index, malformed) in [
        concat!(" \n", r#"{ "version" : 3, "live": ["#),
        r#"{
  "version": 3,
 "passive": ["#,
        r#"{"live": [], "planned": [], "version": 3, "passive": ["#,
    ]
    .into_iter()
    .enumerate()
    {
        let env = Env::new();
        let root = env.dir().parent().unwrap().to_path_buf();
        let channel = 8148400 + index as u64;
        let history = root.join("injected_frames.json");
        let _fixture = Fixture::at(history.clone(), channel, true);
        let good = fs::read(&history).unwrap();
        let path = env.seed(&row(channel, None), STALE);
        fs::write(&history, malformed).unwrap();
        let guard = BootReapOnce::default();
        let failed = reap_inflight_rows_at_boot_with_guard(&guard, &CLAUDE, None).await;
        assert!(path.exists(), "uncertain v3 must retain the old row");
        assert_eq!(failed.incomplete, 1);
        fs::write(history, good).unwrap();
        let retried = reap_inflight_rows_at_boot_with_guard(&guard, &CLAUDE, None).await;
        assert!(
            !retried.already_ran,
            "failed read cannot consume the boot gate"
        );
        assert_eq!(retried.reaped_stale, 1);
        assert!(!path.exists());
    }
}
