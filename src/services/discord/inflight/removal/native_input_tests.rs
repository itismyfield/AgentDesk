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
        let mut expected = json!({"namespace":registration["namespace"],
            "settlement":registration["native_settlement"],"registration":registration});
        let mut entry = snapshot["live"]
            .as_array()
            .unwrap()
            .iter()
            .find(|bound| {
                bound["registration"]["native_settlement"]["ticket"]["channel_id"] == channel
            })
            .unwrap()
            .clone();
        entry.as_object_mut().unwrap().remove("registration");
        expected["entry"] = entry;
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

fn boot_shared(pool: Option<sqlx::PgPool>) -> Arc<crate::services::discord::SharedData> {
    let mut shared = crate::services::discord::make_shared_data_for_tests_with_storage(pool);
    Arc::get_mut(&mut shared)
        .unwrap()
        .restart
        .current_generation = 2;
    shared
}

fn boot_saved(
    env: &Env,
    channel: u64,
    withdraw: bool,
) -> (Fixture, native::Outcome, InflightTurnState, PathBuf) {
    use crate::services::tui_o::{
        shadow::{ShadowProvider, SourceBinding, SourceId},
        writer::input_facts::InputFacts,
    };
    let root = env.dir().parent().unwrap().to_path_buf();
    let f = Fixture::at(root.join("injected_frames.json"), channel, true);
    let source = f.ticket.source.as_ref().unwrap();
    let uuid = format!("own-{channel}");
    let rows = if withdraw {
        vec![
            json!({"type":"queue-operation","operation":"enqueue","content":"한국어 원본 입력"}),
            json!({"type":"queue-operation","operation":"remove","content":"한국어 원본 입력"}),
        ]
    } else {
        vec![
            json!({"type":"user","sessionId":"native","uuid":uuid,"message":{"content":"한국어 원본 입력"}}),
            json!({"type":"system","subtype":"turn_duration"}),
        ]
    };
    fs::write(
        &source.transcript,
        rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
    if !withdraw {
        assert_eq!(
            f.history.receipt(
                "native-fixture",
                &f.ticket.key,
                (
                    &source.transcript,
                    0,
                    "user",
                    Some(&uuid),
                    "한국어 원본 입력"
                )
            ),
            Some(true)
        );
    }
    f.history
        .note_native_any_key("native-fixture", &f.ticket)
        .unwrap();
    let binding = SourceBinding {
        channel_id: channel,
        provider: ShadowProvider::Claude,
        source: SourceId {
            session_id: source.session_id.clone(),
            path: source.transcript.clone(),
            dev: source.file[0],
            ino: source.file[1],
        },
    };
    let mut facts = InputFacts::open(binding.clone()).unwrap();
    facts.observe_native().unwrap();
    facts.poll(u64::MAX).unwrap();
    let registrations: Vec<_> = f
        .history
        .native_evidence_registrations()
        .unwrap()
        .into_iter()
        .filter(|r| r.matches(&binding))
        .collect();
    let proof = facts.native_evidence(&registrations).unwrap().remove(0);
    let outcome = native::Outcome {
        disposition: proof.disposition.clone(),
        range: Some([proof.range.0, proof.range.1]),
        extra: serde_json::Map::from_iter([(
            "native_proof".into(),
            serde_json::to_value(proof).unwrap(),
        )]),
    };
    let stamp = crate::services::tui_o::writer::input_facts::native::registrations_fingerprint(
        &registrations,
    )
    .unwrap();
    assert!(
        f.history
            .record_native_outcome_at_stamp("native-fixture", &f.ticket, &outcome, &stamp)
            .unwrap()
    );
    let mut state = row(channel, None);
    state.born_generation = 1;
    state.turn_nonce = Some(f.ticket.owning_episode.as_ref().unwrap().turn_nonce.clone());
    state.session_id = Some(source.session_id.clone());
    state.output_path = Some(source.transcript.to_string_lossy().into_owned());
    state.hosted_execution_nonce = Some("incarnation-1".into());
    let path = env.seed(&state, STALE);
    (f, outcome, state, path)
}

#[tokio::test]
async fn native_8148_boot_saved_result_and_custody_only_recovery_drive_next_head_pg() {
    use crate::services::discord::turn_finalizer::native_prepare::BootInputs;
    for custody_only in [false, true] {
        let env = Env::new();
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let root = env.dir().parent().unwrap().to_path_buf();
        let channel = 81489960 + u64::from(custody_only);
        let (f, outcome, state, path) = boot_saved(&env, channel, false);
        super::boot_custody::preserve_native_before_boot_reap(&root, &CLAUDE).unwrap();
        if custody_only {
            fs::remove_file(root.join("injected_frames.json")).unwrap();
        }
        let shared = boot_shared(Some(pool.clone()));
        let plan = BootInputs::prepare(&root, shared.restart.current_generation).unwrap();
        let reap =
            reap_inflight_rows_at_boot_with_guard(&BootReapOnce::default(), &CLAUDE, None).await;
        assert!(reap.native_preparation_complete());
        assert!(!path.exists());
        let mut events =
            crate::services::discord::turn_completion_events::subscribe_turn_completion_events(
                &shared,
            );
        plan.restore(&shared).await.unwrap();
        assert!(
            shared
                .turn_finalizer
                .native_boot_ready(ChannelId::new(channel))
                .await
        );
        let history = Registry::at(root.join("injected_frames.json"));
        let after = history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap();
        assert_eq!(after.outcome, Some(outcome));
        assert!(after.cleanup_complete);
        assert!(
            !history
                .record_native_outcome("native-fixture", &f.ticket, after.outcome.as_ref().unwrap())
                .unwrap()
        );
        assert!(
            shared
                .mailbox(ChannelId::new(channel))
                .snapshot()
                .await
                .cancel_token
                .is_none()
        );
        assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 0);
        let wake = events.try_recv().unwrap();
        assert_eq!(wake.channel_id, ChannelId::new(channel));
        assert!(events.try_recv().is_err());
        next_head(shared.mailbox(ChannelId::new(channel)), channel, &root).await;
        // A lost registry after cleanup must recover the terminal copy, not an older pending debt.
        fs::remove_file(root.join("injected_frames.json")).unwrap();
        BootInputs::prepare(&root, 2)
            .unwrap()
            .restore(&shared)
            .await
            .unwrap();
        assert!(events.try_recv().is_err());
        assert!(
            shared
                .turn_finalizer
                .native_boot_ready(ChannelId::new(channel))
                .await
        );
        assert!(!path.exists());
        assert_eq!(state.born_generation, 1);
        pool.close().await;
        db.drop().await;
    }
}

#[tokio::test]
async fn native_8148_boot_pending_registration_is_adopted_without_a_result_or_lease() {
    use crate::services::discord::turn_finalizer::native_prepare::BootInputs;
    let env = Env::new();
    let root = env.dir().parent().unwrap().to_path_buf();
    let f = Fixture::at(root.join("injected_frames.json"), 81489962, true);
    super::boot_custody::preserve_native_before_boot_reap(&root, &CLAUDE).unwrap();
    fs::remove_file(root.join("injected_frames.json")).unwrap();
    let shared = boot_shared(None);
    let plan = BootInputs::prepare(&root, 2).unwrap();
    reap_inflight_rows_at_boot_with_guard(&BootReapOnce::default(), &CLAUDE, None).await;
    plan.restore(&shared).await.unwrap();
    assert!(
        !shared
            .turn_finalizer
            .native_boot_ready(ChannelId::new(81489962))
            .await
    );
    let restored = Registry::at(root.join("injected_frames.json"))
        .native_settlement("native-fixture", &f.ticket)
        .unwrap();
    assert!(restored.outcome.is_none() && !restored.cleanup_complete);
    assert!(
        shared
            .mailbox(ChannelId::new(81489962))
            .snapshot()
            .await
            .cancel_token
            .is_none()
    );
    assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 0);
    assert!(!crate::services::discord::turn_finalizer::native_prepare::enabled());
}

#[tokio::test]
async fn native_8148_boot_rejects_corrupt_proof_current_epoch_and_unreadable_row_before_reap() {
    use crate::services::discord::turn_finalizer::native_prepare::BootInputs;
    for mutation in [
        "namespace",
        "ticket",
        "membership",
        "any_key",
        "generation",
        "row_io",
    ] {
        let env = Env::new();
        let root = env.dir().parent().unwrap().to_path_buf();
        let (f, _, _, path) = boot_saved(&env, 81489963, false);
        let registry = root.join("injected_frames.json");
        let mut data = f.history.json().unwrap();
        let settled = &mut data["live"][0]["registration"]["native_settlement"];
        match mutation {
            "namespace" => settled["outcome"]["native_proof"]["namespace"] = json!("foreign"),
            "ticket" => settled["outcome"]["native_proof"]["ticket"]["opaque"] = json!(true),
            "membership" => {
                settled["outcome"]["native_proof"]["membership_row"] = serde_json::Value::Null
            }
            "any_key" => settled["any_key"] = json!(false),
            "generation" => {}
            _ => {
                fs::remove_file(&path).unwrap();
                fs::create_dir(&path).unwrap();
            }
        }
        fs::write(&registry, data.to_string()).unwrap();
        let before = fs::read(&registry).unwrap();
        assert!(
            BootInputs::prepare(&root, if mutation == "generation" { 1 } else { 2 }).is_err(),
            "{mutation}"
        );
        assert_eq!(fs::read(&registry).unwrap(), before, "{mutation}");
        assert!(path.exists(), "{mutation}");
    }
}

#[tokio::test]
async fn native_8148_boot_transfer_preserves_successor_and_cannot_borrow_live_actor() {
    use crate::services::discord::turn_finalizer::native_prepare::BootInputs;
    for successor in [false, true] {
        let env = Env::new();
        let root = env.dir().parent().unwrap().to_path_buf();
        let (f, _, mut next, path) = boot_saved(&env, 81489964, false);
        let shared = boot_shared(None);
        let plan = BootInputs::prepare(&root, 2).unwrap();
        reap_inflight_rows_at_boot_with_guard(&BootReapOnce::default(), &CLAUDE, None).await;
        if successor {
            next.born_generation = 2;
            next.user_msg_id += 10;
            next.turn_nonce = Some("successor".into());
            next.hosted_execution_nonce = Some("new-execution".into());
        }
        let token = Arc::new(CancelToken::from_persisted_turn_nonce(
            next.turn_nonce.clone(),
        ));
        let mailbox = shared.mailbox(ChannelId::new(next.channel_id));
        mailbox
            .restore_active_turn(
                token.clone(),
                UserId::new(7),
                MessageId::new(next.user_msg_id),
            )
            .await;
        shared.restart.global_active.store(1, Ordering::SeqCst);
        let bytes = serde_json::to_vec(&next).unwrap();
        fs::write(&path, &bytes).unwrap();
        let mut events =
            crate::services::discord::turn_completion_events::subscribe_turn_completion_events(
                &shared,
            );
        let result = plan.restore(&shared).await;
        assert_eq!(result.is_ok(), successor);
        assert_eq!(fs::read(path).unwrap(), bytes);
        assert!(Arc::ptr_eq(
            mailbox.snapshot().await.cancel_token.as_ref().unwrap(),
            &token
        ));
        assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 1);
        assert!(events.try_recv().is_err());
        let status = f
            .history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap();
        assert_eq!(status.cleanup_complete, successor);
        assert_eq!(
            shared
                .turn_finalizer
                .native_boot_ready(ChannelId::new(next.channel_id))
                .await,
            successor
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_8148_boot_withdrawal_pg_wait_is_outside_actor_and_keeps_admission_held() {
    use crate::services::discord::turn_finalizer::native_prepare::BootInputs;
    let env = Env::new();
    let root = env.dir().parent().unwrap().to_path_buf();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(1).await;
    let (f, _, _, path) = boot_saved(&env, 81489965, true);
    let shared = boot_shared(Some(pool.clone()));
    let plan = BootInputs::prepare(&root, 2).unwrap();
    reap_inflight_rows_at_boot_with_guard(&BootReapOnce::default(), &CLAUDE, None).await;
    let held = pool.acquire().await.unwrap();
    let task = tokio::spawn({
        let shared = shared.clone();
        async move { plan.restore(&shared).await }
    });
    await_within(
        async {
            while shared
                .turn_finalizer
                .native_boot_ready(ChannelId::new(81489965))
                .await
            {
                tokio::task::yield_now().await;
            }
        },
        WAIT_CAP,
    )
    .await;
    assert!(!task.is_finished());
    assert!(
        !await_within(
            shared
                .turn_finalizer
                .native_boot_ready(ChannelId::new(81489965)),
            WAIT_CAP
        )
        .await
    );
    assert!(
        await_within(task, WAIT_CAP)
            .await
            .unwrap()
            .unwrap_err()
            .contains("timed out")
    );
    assert!(!path.exists());
    assert!(
        !f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap()
            .cleanup_complete
    );
    drop(held);
    BootInputs::prepare(&root, 2)
        .unwrap()
        .restore(&shared)
        .await
        .unwrap();
    assert!(
        f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap()
            .withdrawal_notice
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM message_outbox WHERE source='retained_input_notice'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    BootInputs::prepare(&root, 2)
        .unwrap()
        .restore(&shared)
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM message_outbox WHERE source='retained_input_notice'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn native_8148_boot_custody_digest_manifest_and_incomplete_repair_keep_rows() {
    use crate::services::discord::turn_finalizer::native_prepare::BootInputs;
    let env = Env::new();
    let root = env.dir().parent().unwrap().to_path_buf();
    let (_, _, _, row) = boot_saved(&env, 81489966, false);
    super::boot_custody::preserve_native_before_boot_reap(&root, &CLAUDE).unwrap();
    let mut native_copy = None;
    for episode in fs::read_dir(root.join("discord_custody/claude"))
        .unwrap()
        .flatten()
    {
        for rev in fs::read_dir(episode.path())
            .unwrap()
            .flatten()
            .filter(|r| r.file_name().to_string_lossy().starts_with("rev-"))
        {
            for copy in fs::read_dir(rev.path()).unwrap().flatten() {
                if copy
                    .file_name()
                    .to_string_lossy()
                    .ends_with("-native_input.json")
                {
                    native_copy = Some(copy.path());
                }
            }
        }
    }
    let copy = native_copy.unwrap();
    let bytes = fs::read(&copy).unwrap();
    let manifest = copy.parent().unwrap().join("manifest.json");
    let original_manifest = fs::read(&manifest).unwrap();
    fs::remove_file(root.join("injected_frames.json")).unwrap();
    fs::write(&copy, b"{}").unwrap();
    assert!(BootInputs::prepare(&root, 2).is_err());
    assert!(row.exists());
    assert!(!root.join("injected_frames.json").exists());
    let mut tampered: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    tampered["settlement"]["warned"] = json!(true);
    tampered["registration"]["native_settlement"]["warned"] = json!(true);
    fs::write(&copy, tampered.to_string()).unwrap();
    assert!(BootInputs::prepare(&root, 2).is_err());
    assert!(row.exists());
    fs::write(&copy, &bytes).unwrap();
    let saved = manifest.with_extension("saved");
    fs::rename(&manifest, &saved).unwrap();
    assert!(BootInputs::prepare(&root, 2).is_err());
    assert!(row.exists());
    fs::rename(&saved, &manifest).unwrap();
    let mut broken: serde_json::Value = serde_json::from_slice(&original_manifest).unwrap();
    broken["complete"] = json!(false);
    fs::write(&manifest, broken.to_string()).unwrap();
    assert!(BootInputs::prepare(&root, 2).is_err());
    assert!(row.exists());
    fs::write(&manifest, &original_manifest).unwrap();
    let shared = boot_shared(None);
    let plan = BootInputs::prepare(&root, 2).unwrap();
    reap_inflight_rows_at_boot_with_guard(&BootReapOnce::default(), &CLAUDE, None).await;
    plan.restore(&shared).await.unwrap();
    assert!(!row.exists());
    assert!(
        shared
            .turn_finalizer
            .native_boot_ready(ChannelId::new(81489966))
            .await
    );
}

#[tokio::test]
async fn native_8148_boot_terminal_custody_failure_keeps_admission_pending_until_repair() {
    use crate::services::discord::turn_finalizer::native_prepare::BootInputs;
    let env = Env::new();
    let root = env.dir().parent().unwrap().to_path_buf();
    let (f, _, _, row) = boot_saved(&env, 81489967, false);
    let shared = boot_shared(None);
    let plan = BootInputs::prepare(&root, 2).unwrap();
    reap_inflight_rows_at_boot_with_guard(&BootReapOnce::default(), &CLAUDE, None).await;
    let custody = root.join("discord_custody/claude");
    let kept = root.join("discord_custody/claude-kept");
    fs::rename(&custody, &kept).unwrap();
    fs::write(&custody, b"not a directory").unwrap();
    assert!(plan.restore(&shared).await.is_err());
    assert!(!row.exists());
    assert!(
        !shared
            .turn_finalizer
            .native_boot_ready(ChannelId::new(81489967))
            .await
    );
    assert!(
        f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap()
            .cleanup_complete
    );
    fs::remove_file(&custody).unwrap();
    fs::rename(&kept, &custody).unwrap();
    let retry = BootInputs::prepare(&root, 2).unwrap();
    fs::rename(&custody, &kept).unwrap();
    fs::write(&custody, b"not a directory").unwrap();
    assert!(retry.restore(&shared).await.is_err());
    assert!(
        !shared
            .turn_finalizer
            .native_boot_ready(ChannelId::new(81489967))
            .await
    );
    fs::remove_file(&custody).unwrap();
    fs::rename(&kept, &custody).unwrap();
    BootInputs::prepare(&root, 2)
        .unwrap()
        .restore(&shared)
        .await
        .unwrap();
    assert!(
        shared
            .turn_finalizer
            .native_boot_ready(ChannelId::new(81489967))
            .await
    );
    next_head(shared.mailbox(ChannelId::new(81489967)), 81489967, &root).await;
}

#[tokio::test]
async fn native_8148_p2_custody_restores_receipts_before_the_next_identical_input() {
    use crate::services::discord::turn_finalizer::native_prepare::BootInputs;
    let env = Env::new();
    let root = env.dir().parent().unwrap().to_path_buf();
    let (f, outcome, _, _) = boot_saved(&env, 81489971, false);
    assert!(
        f.history
            .ack_native_cleanup("native-fixture", &f.ticket, &outcome)
            .unwrap()
    );
    let record = f
        .history
        .native_boot_record("native-fixture", &f.ticket)
        .unwrap();
    super::super::persist_native_boot_record(&root, &record).unwrap();
    fs::remove_file(root.join("injected_frames.json")).unwrap();
    let plan = BootInputs::prepare(&root, 2).unwrap();
    reap_inflight_rows_at_boot_with_guard(&BootReapOnce::default(), &CLAUDE, None).await;
    plan.restore(&boot_shared(None)).await.unwrap();
    let source = f.ticket.source.as_ref().unwrap();
    let next_offset = fs::metadata(&source.transcript).unwrap().len();
    let mut next = f.input.clone();
    next["identity"]["primary"] = json!(81489973u64);
    next["identity"]["sources"][0]["message_id"] = json!(81489973u64);
    next["attempt"]["nonce"] = json!("same-frame-next");
    next["attempt"]["source"]["offset"] = json!(next_offset);
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&source.transcript)
        .unwrap();
    std::io::Write::write_all(&mut file, b"{\"type\":\"user\",\"uuid\":\"next-user\"}\n").unwrap();
    f.history.prepare("native-fixture", &next).unwrap();
    let key = json!({"identity":next["identity"],"attempt":next["attempt"]});
    assert_eq!(
        f.history.receipt(
            "native-fixture",
            &key,
            (
                &source.transcript,
                next_offset,
                "user",
                Some("next-user"),
                "한국어 원본 입력"
            )
        ),
        Some(true)
    );
    assert_eq!(
        f.history.receipt(
            "native-fixture",
            &f.ticket.key,
            (
                &source.transcript,
                0,
                "user",
                Some("own-81489971"),
                "한국어 원본 입력"
            )
        ),
        Some(true)
    );
    let twin_offset = fs::metadata(&source.transcript).unwrap().len();
    std::io::Write::write_all(&mut file, b"{}\n").unwrap();
    let mut twins = Vec::new();
    for n in 0..2u64 {
        let mut input = next.clone();
        input["identity"]["primary"] = json!(81489980 + n);
        input["identity"]["sources"][0]["message_id"] = json!(81489980 + n);
        input["attempt"]["nonce"] = json!(format!("unresolved-twin-{n}"));
        input["attempt"]["source"]["offset"] = json!(twin_offset);
        f.history.prepare("native-fixture", &input).unwrap();
        twins.push(json!({"identity":input["identity"],"attempt":input["attempt"]}));
    }
    for key in twins {
        assert_eq!(
            f.history.receipt(
                "native-fixture",
                &key,
                (
                    &source.transcript,
                    twin_offset,
                    "user",
                    Some("unknown-twin"),
                    "한국어 원본 입력"
                )
            ),
            None
        );
    }
}
