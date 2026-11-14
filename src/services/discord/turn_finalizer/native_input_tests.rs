use super::*;
use crate::services::tui_input::human_frame::history::v2::{
    native::Disposition, native_tests::Fixture,
};
use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding, SourceId};
use crate::services::tui_o::writer::input_facts::native::Observation;

#[test]
fn native_8148_b2_typed_admission_refuses_stale_stamp_source_and_actor() {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    std::fs::create_dir_all(runtime.path().join("runtime")).unwrap();
    let dir = tempfile::tempdir_in(runtime.path().join("runtime")).unwrap();
    let f = Fixture::at(dir.path().join("frames.json"), 8148990, true);
    let source = f.ticket.source.as_ref().unwrap();
    let episode = f.ticket.owning_episode.as_ref().unwrap();
    let actor = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        episode.turn_nonce.clone(),
    )));
    let key = TurnKey::new(
        ChannelId::new(f.ticket.channel_id),
        episode.user_message_id,
        episode.generation,
    )
    .with_episode_nonce(Some(&episode.turn_nonce));
    let binding = SourceBinding {
        channel_id: f.ticket.channel_id,
        provider: ShadowProvider::Claude,
        source: SourceId {
            session_id: source.session_id.clone(),
            path: source.transcript.clone(),
            dev: source.file[0],
            ino: source.file[1],
        },
    };
    let rows = [
        serde_json::json!({"type":"user","uuid":"own","sessionId":"native","message":{"content":"한국어 원본 입력"}}),
        serde_json::json!({"type":"system","subtype":"turn_duration"}),
    ];
    std::fs::write(
        &source.transcript,
        rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
    let mut facts =
        crate::services::tui_o::writer::input_facts::InputFacts::open(binding.clone()).unwrap();
    facts.observe_native().unwrap();
    facts.poll(u64::MAX).unwrap();
    let batch = Observation {
        binding,
        generation: 3,
        binding_seq: 4,
        revision: 5,
        through: facts.observation_progress().0,
        registrations_fingerprint:
            crate::services::tui_o::writer::input_facts::native::registrations_fingerprint(
                &f.history.native_evidence_registrations().unwrap(),
            )
            .unwrap(),
        evidence: facts
            .native_evidence(&f.history.native_evidence_registrations().unwrap())
            .unwrap(),
    };
    assert_eq!(batch.evidence.len(), 1);
    let admitted = NativeAdmission::capture(
        &batch,
        &batch,
        0,
        &source.target,
        key,
        Some(&actor),
        Some(&actor),
    )
    .unwrap();
    assert_eq!(admitted.evidence, batch.evidence[0]);
    assert!(Arc::ptr_eq(&admitted.cleanup.unwrap().1, &actor));
    let mut stale = Vec::new();
    let mut changed = batch.clone();
    changed.registrations_fingerprint.push_str("stale");
    stale.push(changed);
    let mut changed = batch.clone();
    changed.generation += 1;
    stale.push(changed);
    let mut changed = batch.clone();
    changed.binding_seq += 1;
    stale.push(changed);
    let mut changed = batch.clone();
    changed.revision += 1;
    stale.push(changed);
    let mut changed = batch.clone();
    changed.through += 1;
    stale.push(changed);
    let mut changed = batch.clone();
    changed.binding.source.ino += 1;
    stale.push(changed);
    for changed in stale {
        assert!(
            NativeAdmission::capture(
                &batch,
                &changed,
                0,
                &source.target,
                key,
                Some(&actor),
                Some(&actor)
            )
            .is_err()
        );
    }
    let successor = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        episode.turn_nonce.clone(),
    )));
    assert!(
        NativeAdmission::capture(
            &batch,
            &batch,
            0,
            &source.target,
            key,
            Some(&actor),
            Some(&successor)
        )
        .is_err()
    );
    assert!(
        NativeAdmission::capture(&batch, &batch, 0, &source.target, key, None, Some(&actor))
            .is_err()
    );
    let mut wrong = batch.clone();
    wrong.evidence[0].ticket.source.as_mut().unwrap().target["execution_nonce"] =
        "successor".into();
    assert!(
        NativeAdmission::capture(
            &wrong,
            &wrong,
            0,
            &source.target,
            key,
            Some(&actor),
            Some(&actor)
        )
        .is_err()
    );
    let mut target = source.target.clone();
    target["execution_nonce"] = "successor".into();
    assert!(
        NativeAdmission::capture(&batch, &batch, 0, &target, key, Some(&actor), Some(&actor))
            .is_err()
    );
    let mut broken = batch.clone();
    broken.evidence[0].closer_range.as_mut().unwrap().0 = 0;
    assert!(
        NativeAdmission::capture(
            &broken,
            &broken,
            0,
            &source.target,
            key,
            Some(&actor),
            Some(&actor)
        )
        .is_err()
    );
    let before = std::fs::read(dir.path().join("frames.json")).unwrap();
    for disposition in [Disposition::Executed, Disposition::WithdrawnNotExecuted] {
        let mut follower = batch.clone();
        let evidence = &mut follower.evidence[0];
        evidence.ticket.owning_episode = None;
        evidence.disposition = disposition.clone();
        if disposition == Disposition::WithdrawnNotExecuted {
            evidence.run = None;
            evidence.range = (0, batch.through);
            evidence.membership_range = None;
            evidence.membership_row = None;
            evidence.closer_range = None;
        }
        let preview = NativeAdmission::capture(
            &follower,
            &follower,
            0,
            &source.target,
            key,
            Some(&actor),
            Some(&actor),
        )
        .unwrap();
        assert!(
            preview.cleanup.is_none(),
            "follower has result authority only"
        );
        assert_eq!(preview.evidence.disposition, disposition);
    }
    assert_eq!(
        std::fs::read(dir.path().join("frames.json")).unwrap(),
        before,
        "admission remains inactive"
    );
    assert!(!actor.cancelled.load(std::sync::atomic::Ordering::Relaxed));
}

fn b2b_shared(pool: Option<sqlx::PgPool>) -> Arc<SharedData> {
    let mut shared = crate::services::discord::make_shared_data_for_tests_with_storage(pool);
    Arc::get_mut(&mut shared)
        .unwrap()
        .restart
        .current_generation = 1;
    shared
}

async fn b2b_submit(
    shared: &Arc<SharedData>,
    work: NativeCleanup,
    history: &crate::services::tui_input::human_frame::history::v2::Registry,
) -> Result<bool, String> {
    crate::test_support::wait::await_within(
        shared
            .turn_finalizer
            .submit_native(work, history.clone(), shared.clone()),
        crate::test_support::wait::WAIT_CAP,
    )
    .await
}

fn b2b_batch(f: &Fixture) -> Observation {
    let source = f.ticket.source.as_ref().unwrap();
    let rows = [
        serde_json::json!({"type":"user","uuid":"own","sessionId":"native","message":{"content":"한국어 원본 입력"}}),
        serde_json::json!({"type":"system","subtype":"turn_duration"}),
    ];
    std::fs::write(
        &source.transcript,
        rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
    assert_eq!(
        f.history.receipt(
            "native-fixture",
            &f.ticket.key,
            (
                &source.transcript,
                0,
                "user",
                Some("own"),
                "한국어 원본 입력"
            )
        ),
        Some(true)
    );
    let binding = SourceBinding {
        channel_id: f.ticket.channel_id,
        provider: ShadowProvider::Claude,
        source: SourceId {
            session_id: source.session_id.clone(),
            path: source.transcript.clone(),
            dev: source.file[0],
            ino: source.file[1],
        },
    };
    let mut facts =
        crate::services::tui_o::writer::input_facts::InputFacts::open(binding.clone()).unwrap();
    facts.observe_native().unwrap();
    facts.poll(u64::MAX).unwrap();
    Observation {
        binding,
        generation: 3,
        binding_seq: 4,
        revision: 5,
        through: facts.observation_progress().0,
        registrations_fingerprint:
            crate::services::tui_o::writer::input_facts::native::registrations_fingerprint(
                &f.history.native_evidence_registrations().unwrap(),
            )
            .unwrap(),
        evidence: facts
            .native_evidence(&f.history.native_evidence_registrations().unwrap())
            .unwrap(),
    }
}

fn b2b_row(f: &Fixture) -> super::super::super::inflight::InflightTurnState {
    let episode = f.ticket.owning_episode.as_ref().unwrap();
    let source = f.ticket.source.as_ref().unwrap();
    let mut row = super::super::super::inflight::InflightTurnState::new(
        ProviderKind::Claude,
        f.ticket.channel_id,
        None,
        7,
        episode.user_message_id,
        episode.user_message_id,
        "original".into(),
        Some(source.session_id.clone()),
        Some("fixture-pane".into()),
        Some(source.transcript.display().to_string()),
        None,
        0,
    );
    row.turn_nonce = Some(episode.turn_nonce.clone());
    row.born_generation = episode.generation;
    row.hosted_execution_nonce = Some("incarnation-1".into());
    super::super::super::inflight::save_inflight_state(&row).unwrap();
    super::super::super::inflight::load_inflight_state(&ProviderKind::Claude, f.ticket.channel_id)
        .unwrap()
}

async fn b2b_active(f: &Fixture, shared: &Arc<SharedData>) -> (TurnKey, Arc<CancelToken>) {
    let episode = f.ticket.owning_episode.as_ref().unwrap();
    let key = TurnKey::new(
        ChannelId::new(f.ticket.channel_id),
        episode.user_message_id,
        episode.generation,
    )
    .with_episode_nonce(Some(&episode.turn_nonce));
    let actor = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        episode.turn_nonce.clone(),
    )));
    shared
        .mailbox(key.channel_id)
        .restore_active_turn(
            actor.clone(),
            serenity::model::id::UserId::new(7),
            serenity::model::id::MessageId::new(key.user_msg_id),
        )
        .await;
    shared
        .restart
        .global_active
        .store(1, std::sync::atomic::Ordering::SeqCst);
    (key, actor)
}

#[tokio::test]
async fn native_8148_b2b_cleanup_retries_row_error_without_second_release_or_early_wake() {
    use super::super::super::inflight;
    use std::sync::atomic::Ordering;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    std::fs::create_dir_all(runtime.path().join("runtime")).unwrap();
    let dir = tempfile::tempdir_in(runtime.path().join("runtime")).unwrap();
    let f = Fixture::at(dir.path().join("frames.json"), 8148911, true);
    let shared = b2b_shared(None);
    let (key, actor) = b2b_active(&f, &shared).await;
    let row = b2b_row(&f);
    let path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        key.channel_id.get(),
    );
    let bytes = std::fs::read(&path).unwrap();
    let mut events =
        super::super::super::turn_completion_events::subscribe_turn_completion_events(&shared);
    let prepared = NativePrepared::capture(
        &f.history,
        "native-fixture",
        f.ticket.clone(),
        key,
        Some(actor.clone()),
        &shared,
    )
    .await
    .unwrap();
    let batch = b2b_batch(&f);
    let target = &f.ticket.source.as_ref().unwrap().target;
    assert!(
        prepared
            .admit(&batch, &batch, 0, target, &f.history, &shared)
            .await
            .is_err(),
        "no-key must remain original"
    );
    assert!(
        f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap()
            .outcome
            .is_none()
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    prepared.any_key(&f.history).unwrap();
    let cleanup = prepared
        .admit(&batch, &batch, 0, target, &f.history, &shared)
        .await
        .unwrap();
    std::fs::write(&path, b"{corrupt row").unwrap();
    assert!(
        b2b_submit(&shared, cleanup.clone(), &f.history)
            .await
            .is_err()
    );
    assert!(Arc::ptr_eq(
        shared
            .mailbox(key.channel_id)
            .snapshot()
            .await
            .cancel_token
            .as_ref()
            .unwrap(),
        &actor
    ));
    assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 1);
    assert!(
        !actor.cancelled.load(Ordering::SeqCst),
        "native release is neutral"
    );
    assert!(events.try_recv().is_err(), "row error must withhold wake");
    let pending = f
        .history
        .native_settlement("native-fixture", &f.ticket)
        .unwrap();
    assert!(pending.outcome.is_some());
    assert!(!pending.cleanup_complete);
    assert_eq!(std::fs::read(&path).unwrap(), b"{corrupt row");
    std::fs::write(&path, bytes).unwrap();
    // Replay a crash after the exact lease step, before its row and wake steps.
    let released = crate::test_support::wait::await_within(
        super::super::super::mailbox_finish::mailbox_finish_turn_if_matches_episode_started_before_with_actor_without_completion(
            &shared, &ProviderKind::Claude, key.channel_id,
            serenity::model::id::MessageId::new(key.user_msg_id),
            actor.turn_nonce().map(str::to_owned), cleanup.started_before, Some(actor.clone()),
        ), crate::test_support::wait::WAIT_CAP,
    ).await;
    assert!(
        released
            .removed_token
            .as_ref()
            .is_some_and(|removed| Arc::ptr_eq(removed, &actor))
    );
    assert!(released.persistence_error.is_none());
    actor.mark_completion_cleanup();
    assert!(super::super::super::saturating_decrement_global_active(
        &shared
    ));
    assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 0);
    assert!(
        super::super::episode::claim_normal_episode(
            &shared,
            &ProviderKind::Claude,
            key,
            true,
            Some(actor.clone())
        )
        .await
        .is_err(),
        "the old exact-actor claim cannot resume a released lease's row debt"
    );
    assert!(path.exists());
    assert!(
        b2b_submit(&shared, cleanup.clone(), &f.history)
            .await
            .unwrap()
    );
    assert!(!path.exists());
    assert!(events.try_recv().unwrap().queue_is_eligible());
    assert!(
        b2b_submit(&shared, cleanup.clone(), &f.history)
            .await
            .unwrap()
    );
    assert!(events.try_recv().is_err());
    assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 0);
    let done = f
        .history
        .native_settlement("native-fixture", &f.ticket)
        .unwrap();
    assert!(done.cleanup_complete);
    assert_eq!(
        done.outcome.unwrap().extra["native_proof"]["ticket"],
        serde_json::to_value(&f.ticket).unwrap()
    );
    assert_eq!(row.turn_nonce.as_deref(), actor.turn_nonce());
}

#[tokio::test]
async fn native_8148_b2b_same_nonce_successor_actor_and_row_are_preserved() {
    use super::super::super::inflight;
    use std::sync::atomic::Ordering;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    std::fs::create_dir_all(runtime.path().join("runtime")).unwrap();
    let dir = tempfile::tempdir_in(runtime.path().join("runtime")).unwrap();
    let f = Fixture::at(dir.path().join("frames.json"), 8148912, true);
    let shared = b2b_shared(None);
    let (key, actor) = b2b_active(&f, &shared).await;
    b2b_row(&f);
    let prepared = NativePrepared::capture(
        &f.history,
        "native-fixture",
        f.ticket.clone(),
        key,
        Some(actor.clone()),
        &shared,
    )
    .await
    .unwrap();
    prepared.any_key(&f.history).unwrap();
    let batch = b2b_batch(&f);
    let target = &f.ticket.source.as_ref().unwrap().target;
    let cleanup = prepared
        .admit(&batch, &batch, 0, target, &f.history, &shared)
        .await
        .unwrap();
    let successor = Arc::new(CancelToken::from_persisted_turn_nonce(
        actor.turn_nonce().map(str::to_owned),
    ));
    shared
        .mailbox(key.channel_id)
        .restore_active_turn(
            successor.clone(),
            serenity::model::id::UserId::new(7),
            serenity::model::id::MessageId::new(key.user_msg_id),
        )
        .await;
    let path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        key.channel_id.get(),
    );
    let before = std::fs::read(&path).unwrap();
    assert!(
        prepared
            .admit(&batch, &batch, 0, target, &f.history, &shared)
            .await
            .is_err()
    );
    assert!(
        !b2b_submit(&shared, cleanup.clone(), &f.history)
            .await
            .unwrap()
    );
    assert!(Arc::ptr_eq(
        shared
            .mailbox(key.channel_id)
            .snapshot()
            .await
            .cancel_token
            .as_ref()
            .unwrap(),
        &successor
    ));
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 1);
    assert!(
        !f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap()
            .cleanup_complete
    );
}

#[tokio::test]
async fn native_8148_b2b_follower_withdrawal_outbox_retry_and_full_ticket_dedupe_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use std::sync::atomic::Ordering;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    std::fs::create_dir_all(runtime.path().join("runtime")).unwrap();
    let dir = tempfile::tempdir_in(runtime.path().join("runtime")).unwrap();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    {
        let shared = b2b_shared(Some(pool.clone()));
        let parent = Fixture::at(dir.path().join("parent.json"), 8148913, true);
        let (key, parent_actor) = b2b_active(&parent, &shared).await;
        b2b_row(&parent);
        let root = super::super::super::inflight::inflight_runtime_root().unwrap();
        let path = super::super::super::inflight::inflight_state_path(
            &root,
            &ProviderKind::Claude,
            key.channel_id.get(),
        );
        let before = std::fs::read(&path).unwrap();
        let mut events =
            super::super::super::turn_completion_events::subscribe_turn_completion_events(&shared);
        for n in 0..2 {
            let f = Fixture::at(
                dir.path().join(format!("follower-{n}.json")),
                key.channel_id.get(),
                false,
            );
            let prepared = NativePrepared::capture(
                &f.history,
                "native-fixture",
                f.ticket.clone(),
                TurnKey::new(key.channel_id, key.user_msg_id, 1),
                None,
                &shared,
            )
            .await
            .unwrap();
            assert!(
                NativePrepared::capture(
                    &f.history,
                    "native-fixture",
                    f.ticket.clone(),
                    key,
                    Some(parent_actor.clone()),
                    &shared
                )
                .await
                .is_err()
            );
            prepared.any_key(&f.history).unwrap();
            let mut batch = b2b_batch(&f);
            let proof = &mut batch.evidence[0];
            proof.disposition = Disposition::WithdrawnNotExecuted;
            proof.run = None;
            proof.membership_range = None;
            proof.membership_row = None;
            proof.closer_range = None;
            let target = &f.ticket.source.as_ref().unwrap().target;
            let cleanup = prepared
                .admit(&batch, &batch, 0, target, &f.history, &shared)
                .await
                .unwrap();
            let unavailable = b2b_shared(None);
            assert!(
                b2b_submit(&unavailable, cleanup.clone(), &f.history)
                    .await
                    .is_err()
            );
            assert!(
                !f.history
                    .native_settlement("native-fixture", &f.ticket)
                    .unwrap()
                    .withdrawal_notice
            );
            for _ in 0..2 {
                assert!(
                    b2b_submit(&shared, cleanup.clone(), &f.history)
                        .await
                        .unwrap()
                );
            }
            let done = f
                .history
                .native_settlement("native-fixture", &f.ticket)
                .unwrap();
            assert!(done.withdrawal_notice && done.cleanup_complete);
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert!(Arc::ptr_eq(
                shared
                    .mailbox(key.channel_id)
                    .snapshot()
                    .await
                    .cancel_token
                    .as_ref()
                    .unwrap(),
                &parent_actor
            ));
            assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 1);
            assert!(events.try_recv().is_err());
        }
        let count:i64 = sqlx::query_scalar("SELECT count(*) FROM message_outbox WHERE target=$1 AND source='retained_input_notice'")
            .bind(format!("channel:{}",key.channel_id)).fetch_one(&pool).await.unwrap();
        assert_eq!(
            count, 2,
            "different full tickets on one channel each owe a notice"
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn native_8148_b2b_row_replacement_and_cutoff_keep_cleanup_debt() {
    use super::super::super::inflight;
    use std::sync::atomic::Ordering;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    std::fs::create_dir_all(runtime.path().join("runtime")).unwrap();
    let dir = tempfile::tempdir_in(runtime.path().join("runtime")).unwrap();
    for (n, replacement) in [false, true].into_iter().enumerate() {
        let f = Fixture::at(
            dir.path().join(format!("frames-{n}.json")),
            8148920 + n as u64,
            true,
        );
        let shared = b2b_shared(None);
        let before_claim = std::time::Instant::now();
        let (key, actor) = b2b_active(&f, &shared).await;
        let row = b2b_row(&f);
        let path = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            key.channel_id.get(),
        );
        let prepared = NativePrepared::capture(
            &f.history,
            "native-fixture",
            f.ticket.clone(),
            key,
            Some(actor.clone()),
            &shared,
        )
        .await
        .unwrap();
        prepared.any_key(&f.history).unwrap();
        let batch = b2b_batch(&f);
        let target = &f.ticket.source.as_ref().unwrap().target;
        let mut cleanup = prepared
            .admit(&batch, &batch, 0, target, &f.history, &shared)
            .await
            .unwrap();
        if replacement {
            let mut successor = row;
            successor.hosted_execution_nonce = Some("replacement-incarnation".into());
            inflight::save_inflight_state(&successor).unwrap();
        } else {
            cleanup.started_before = before_claim;
        }
        let bytes = std::fs::read(&path).unwrap();
        let mut events =
            super::super::super::turn_completion_events::subscribe_turn_completion_events(&shared);
        assert!(!b2b_submit(&shared, cleanup, &f.history).await.unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(
            !f.history
                .native_settlement("native-fixture", &f.ticket)
                .unwrap()
                .cleanup_complete
        );
        assert!(events.try_recv().is_err());
        assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(
            shared
                .mailbox(key.channel_id)
                .snapshot()
                .await
                .cancel_token
                .as_ref()
                .unwrap(),
            &actor
        ));
    }
}

#[tokio::test]
async fn native_8148_b2b_cleanup_ack_requires_exact_result_ticket_and_notice() {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    std::fs::create_dir_all(runtime.path().join("runtime")).unwrap();
    let dir = tempfile::tempdir_in(runtime.path().join("runtime")).unwrap();
    let f = Fixture::at(dir.path().join("frames.json"), 8148922, false);
    let outcome = Fixture::outcome(Disposition::WithdrawnNotExecuted);
    assert!(
        f.history
            .ack_native_cleanup("native-fixture", &f.ticket, &outcome)
            .is_err()
    );
    f.history
        .record_native_outcome("native-fixture", &f.ticket, &outcome)
        .unwrap();
    assert!(
        f.history
            .ack_native_cleanup("native-fixture", &f.ticket, &outcome)
            .is_err()
    );
    f.history
        .ack_native_withdrawal_notice("native-fixture", &f.ticket)
        .unwrap();
    let bytes = std::fs::read(dir.path().join("frames.json")).unwrap();
    let mut changed = f.ticket.clone();
    changed.extra.insert("foreign".into(), true.into());
    assert!(
        f.history
            .ack_native_cleanup("native-fixture", &changed, &outcome)
            .is_err()
    );
    let mut changed_outcome = outcome.clone();
    changed_outcome
        .extra
        .insert("foreign_proof".into(), true.into());
    assert!(
        f.history
            .ack_native_cleanup("native-fixture", &f.ticket, &changed_outcome)
            .is_err()
    );
    assert!(
        f.history
            .note_native_any_key("native-fixture", &f.ticket)
            .is_err()
    );
    assert_eq!(
        std::fs::read(dir.path().join("frames.json")).unwrap(),
        bytes
    );
    assert!(
        f.history
            .ack_native_cleanup("native-fixture", &f.ticket, &outcome)
            .unwrap()
    );
    assert!(
        !f.history
            .ack_native_cleanup("native-fixture", &f.ticket, &outcome)
            .unwrap()
    );
}

#[tokio::test]
async fn native_8148_2a_receipt_version_rejects_old_candidate_and_transaction() {
    use crate::services::discord::turn_presence::activity::native_input::Reader;
    use crate::services::tui_input::human_frame::history::v2::native::Outcome;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    std::fs::create_dir_all(runtime.path().join("runtime")).unwrap();
    let dir = tempfile::tempdir_in(runtime.path().join("runtime")).unwrap();
    let f = Fixture::at(dir.path().join("frames.json"), 8148950, false);
    let source = f.ticket.source.as_ref().unwrap();
    let shared = b2b_shared(None);
    let key = TurnKey::new(
        ChannelId::new(f.ticket.channel_id),
        f.ticket.channel_id + 1,
        1,
    );
    let prepared = NativePrepared::capture(
        &f.history,
        "native-fixture",
        f.ticket.clone(),
        key,
        None,
        &shared,
    )
    .await
    .unwrap();
    prepared.any_key(&f.history).unwrap();
    let rows = [
        serde_json::json!({"type":"queue-operation","operation":"enqueue","content":"한국어 원본 입력"}),
        serde_json::json!({"type":"queue-operation","operation":"dequeue"}),
        serde_json::json!({"type":"user","uuid":"one","sessionId":"native","message":{"content":"한국어 원본 입력"}}),
        serde_json::json!({"type":"system","subtype":"turn_duration"}),
    ];
    std::fs::write(
        &source.transcript,
        rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
    let binding = SourceBinding {
        channel_id: f.ticket.channel_id,
        provider: ShadowProvider::Claude,
        source: SourceId {
            session_id: source.session_id.clone(),
            path: source.transcript.clone(),
            dev: source.file[0],
            ino: source.file[1],
        },
    };
    let mut reader = Reader::new(binding, f.history.clone());
    let old = reader.poll(u64::MAX).unwrap().unwrap();
    assert_eq!(old.evidence.len(), 1);
    assert_eq!(old.evidence[0].membership_row.as_deref(), Some("one"));
    let unconfirmed = prepared
        .admit(&old, &old, 0, &source.target, &f.history, &shared)
        .await
        .unwrap();
    assert!(
        b2b_submit(&shared, unconfirmed, &f.history).await.is_err(),
        "Executed waits for a confirmed receipt"
    );
    assert!(
        f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap()
            .outcome
            .is_none()
    );
    assert_eq!(
        f.history.receipt(
            "native-fixture",
            &f.ticket.key,
            (
                &source.transcript,
                0,
                "user",
                Some("two"),
                "한국어 원본 입력"
            )
        ),
        Some(true)
    );
    assert!(
        prepared
            .admit(&old, &old, 0, &source.target, &f.history, &shared)
            .await
            .is_err(),
        "unchanged transcript stamp must not borrow the old receipt set"
    );
    let withdrawn = Outcome {
        disposition: Disposition::WithdrawnNotExecuted,
        range: Some([old.evidence[0].range.0, old.evidence[0].range.1]),
        extra: Default::default(),
    };
    assert!(
        f.history
            .record_native_outcome_at_stamp(
                "native-fixture",
                &f.ticket,
                &withdrawn,
                &old.registrations_fingerprint
            )
            .is_err(),
        "writer rechecks under the publication lock"
    );
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&source.transcript)
        .unwrap();
    writeln!(file,"{}",serde_json::json!({"type":"user","uuid":"two","sessionId":"native","message":{"content":"한국어 원본 입력"}})).unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({"type":"system","subtype":"turn_duration"})
    )
    .unwrap();
    let current = reader.poll(u64::MAX).unwrap().unwrap();
    assert_eq!(current.evidence.len(), 1);
    assert_eq!(current.evidence[0].membership_row.as_deref(), Some("two"));
    let work = prepared
        .admit(&current, &current, 0, &source.target, &f.history, &shared)
        .await
        .unwrap();
    assert!(b2b_submit(&shared, work, &f.history).await.unwrap());
    assert_eq!(
        f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap()
            .outcome
            .unwrap()
            .extra["native_proof"]["membership_row"],
        "two"
    );
}
