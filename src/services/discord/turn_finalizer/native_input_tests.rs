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

#[test]
fn native_8148_b2b2_runtime_witness_rejects_nonce_aba_under_row_lock() {
    use super::super::super::inflight;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::at(runtime.path().join("runtime/frames.json"), 8148964, true);
    let captured = b2b_row(&f);
    let path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        captured.channel_id,
    );
    let mut replacement = captured.clone();
    replacement.hosted_execution_nonce = Some("successor-execution".into());
    let bytes = serde_json::to_vec(&replacement).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let (outcome, detail) = inflight::GuardedClearOutcome::for_native_snapshot_with_detail(
        &ProviderKind::Claude,
        &captured,
    );
    assert_eq!(outcome, inflight::GuardedClearOutcome::UserMsgMismatch);
    assert_eq!(detail.reason, "native_runtime_changed");
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_8148_b2b2_registered_scope_rejects_legacy_completion_and_body() {
    use super::super::super::{inflight, turn_bridge::native_input};
    use super::super::native_prepare::Handle;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::at(runtime.path().join("runtime/frames.json"), 8148965, true);
    let shared = b2b_shared(None);
    let (key, actor) = b2b_active(&f, &shared).await;
    let row = b2b_row(&f);
    let bytes = std::fs::read(inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        key.channel_id.get(),
    ))
    .unwrap();
    shared.turn_finalizer.register_start(
        key,
        ProviderKind::Claude,
        super::super::RelayOwnerKind::Watcher,
        &shared,
    );
    let prepared = Handle::prepare(
        &shared,
        key,
        Some(actor.clone()),
        f.history.clone(),
        "native-fixture".into(),
        f.ticket.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        native_input::state(&shared, &ProviderKind::Claude, &row, &actor).await,
        native_input::State::Pending
    );
    assert!(native_input::neutral_exit(&shared, &ProviderKind::Claude, &row, &actor).await);
    let mut body = "foreign turn before our membership".to_string();
    assert!(
        native_input::reject_legacy_body(&shared, &ProviderKind::Claude, &row, &actor, &mut body)
            .await
    );
    assert!(body.is_empty());
    for event in [
        TerminalEvent::Complete,
        TerminalEvent::GateTimeout {
            pane_quiescent: Some(true),
        },
    ] {
        let outcome = shared
            .turn_finalizer
            .submit_terminal(
                key,
                ProviderKind::Claude,
                event,
                FinalizeContext::watcher(),
                shared.clone(),
            )
            .await;
        assert!(matches!(outcome, FinalizeOutcome::Deferred));
    }
    let other = CancelToken::from_persisted_turn_nonce(actor.turn_nonce().map(str::to_string));
    assert_eq!(
        native_input::state(&shared, &ProviderKind::Claude, &row, &other).await,
        native_input::State::Legacy
    );
    assert!(!native_input::neutral_exit(&shared, &ProviderKind::Claude, &row, &other).await);
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
    assert_eq!(
        std::fs::read(inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            key.channel_id.get()
        ))
        .unwrap(),
        bytes
    );
    actor.mark_completion_cleanup();
    assert_eq!(
        native_input::state(&shared, &ProviderKind::Claude, &row, &actor).await,
        native_input::State::Settled
    );
    drop(prepared);
}

#[test]
fn native_8148_b2b2_native_admission_waits_for_input_not_delivery() {
    let mut admission = super::super::completion_admission::CompletionAdmission::new(
        CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled,
    );
    admission.native_input = Some(false);
    admission.note_mailbox_released();
    admission.note_terminal_projection_settled(true);
    admission.note_terminal_disposition_settled(true);
    admission.operator_released = true;
    assert!(
        !admission.claim_queue_eligible(),
        "delivery or operator permission cannot skip native row cleanup"
    );
    admission.native_input = Some(true);
    admission.terminal_projection_allows_queue = false;
    admission.terminal_disposition_allows_queue = false;
    assert!(
        admission.claim_queue_eligible(),
        "native lifecycle admission does not claim provider delivery"
    );
    assert!(!admission.claim_queue_eligible());
}

async fn s2_primary(
    f: &Fixture,
    shared: &Arc<SharedData>,
) -> (TurnKey, Arc<CancelToken>, NativeCleanup) {
    let (key, actor) = b2b_active(f, shared).await;
    b2b_row(f);
    let prepared = NativePrepared::capture(
        &f.history,
        "native-fixture",
        f.ticket.clone(),
        key,
        Some(actor.clone()),
        shared,
    )
    .await
    .unwrap();
    prepared.any_key(&f.history).unwrap();
    let batch = b2b_batch(f);
    let cleanup = prepared
        .admit(
            &batch,
            &batch,
            0,
            &f.ticket.source.as_ref().unwrap().target,
            &f.history,
            shared,
        )
        .await
        .unwrap();
    (key, actor, cleanup)
}

async fn s2_withdraw(f: &Fixture, shared: &Arc<SharedData>) -> NativeCleanup {
    let prepared = NativePrepared::capture(
        &f.history,
        "native-fixture",
        f.ticket.clone(),
        TurnKey::new(
            ChannelId::new(f.ticket.channel_id),
            f.ticket.channel_id + 1,
            1,
        ),
        None,
        shared,
    )
    .await
    .unwrap();
    prepared.any_key(&f.history).unwrap();
    let mut batch = b2b_batch(f);
    let evidence = &mut batch.evidence[0];
    evidence.disposition = Disposition::WithdrawnNotExecuted;
    evidence.run = None;
    evidence.membership_range = None;
    evidence.membership_row = None;
    evidence.closer_range = None;
    prepared
        .admit(
            &batch,
            &batch,
            0,
            &f.ticket.source.as_ref().unwrap().target,
            &f.history,
            shared,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn native_8148_s2_row_start_and_offset_guard_keep_owned_lease() {
    use super::super::super::inflight;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    for (n, field) in ["started_at", "turn_start_offset"].into_iter().enumerate() {
        let f = Fixture::at(
            runtime.path().join(format!("runtime/row-{n}.json")),
            8148970 + n as u64,
            true,
        );
        let shared = b2b_shared(None);
        let (key, actor, cleanup) = s2_primary(&f, &shared).await;
        let path = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            f.ticket.channel_id,
        );
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if field == "started_at" {
            value[field] = "2000-01-01T00:00:00Z".into();
        } else {
            value[field] = 900.into();
        }
        let bytes = serde_json::to_vec(&value).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            !b2b_submit(&shared, cleanup, &f.history).await.unwrap(),
            "{field}"
        );
        assert_eq!(std::fs::read(path).unwrap(), bytes);
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
        assert_eq!(
            shared
                .restart
                .global_active
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }
}

#[tokio::test]
async fn native_8148_s2_bound_ticket_rejects_episode_extra_and_source_without_writes() {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    for (n, field) in ["episode", "extra", "source"].into_iter().enumerate() {
        let path = runtime.path().join(format!("runtime/ticket-{n}.json"));
        let f = Fixture::at(path.clone(), 8148973 + n as u64, true);
        let shared = b2b_shared(None);
        let (key, actor) = b2b_active(&f, &shared).await;
        b2b_row(&f);
        let prepared = NativePrepared::capture(
            &f.history,
            "native-fixture",
            f.ticket.clone(),
            key,
            Some(actor),
            &shared,
        )
        .await
        .unwrap();
        let batch = b2b_batch(&f);
        let mut value = f.history.json().unwrap();
        let ticket = &mut value["live"][0]["registration"]["native_settlement"]["ticket"];
        match field {
            "episode" => ticket["owning_episode"]["turn_nonce"] = "other-episode".into(),
            "extra" => ticket["future"] = true.into(),
            _ => ticket["source"]["binding_watermark"] = "changed-valid-binding".into(),
        }
        let coherent_source = ticket["source"].clone();
        if field == "source" {
            value["live"][0]["registration"]["source"] = coherent_source;
        }
        let bytes = serde_json::to_vec(&value).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        if field == "source" {
            let updated = f
                .history
                .native_ticket("native-fixture", &f.ticket.key)
                .unwrap();
            updated
                .source
                .as_ref()
                .unwrap()
                .validate(&updated.key)
                .unwrap();
        }
        assert!(prepared.any_key(&f.history).is_err(), "{field}");
        assert!(
            prepared
                .admit(
                    &batch,
                    &batch,
                    0,
                    &f.ticket.source.as_ref().unwrap().target,
                    &f.history,
                    &shared
                )
                .await
                .is_err(),
            "{field}"
        );
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}

#[tokio::test]
async fn native_8148_s2_persistence_error_keeps_row_and_wake_debt() {
    use super::super::super::{inflight, runtime_store};
    use crate::services::turn_orchestrator::{Intervention, InterventionMode};
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::at(
        runtime.path().join("runtime/persistence.json"),
        8148976,
        true,
    );
    let shared = b2b_shared(None);
    let (key, actor, cleanup) = s2_primary(&f, &shared).await;
    let persistence = super::super::super::queue_persistence_context(
        &shared,
        &ProviderKind::Claude,
        key.channel_id,
    );
    let item = Intervention {
        author_id: serenity::model::id::UserId::new(7),
        author_is_bot: false,
        message_id: serenity::model::id::MessageId::new(8149999),
        queued_generation: 1,
        source_message_ids: vec![],
        source_message_queued_generations: vec![],
        source_text_segments: vec![],
        text: "next".into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: vec![],
    };
    assert!(
        shared
            .mailbox(key.channel_id)
            .enqueue(item, persistence.clone())
            .await
            .enqueued
    );
    let queue = runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join(&persistence.token_hash)
        .join(format!("{}.json", f.ticket.channel_id));
    let bytes = std::fs::read(&queue).unwrap();
    std::fs::remove_file(&queue).unwrap();
    std::fs::create_dir(&queue).unwrap();
    let row = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        f.ticket.channel_id,
    );
    let before = std::fs::read(&row).unwrap();
    let mut events =
        super::super::super::turn_completion_events::subscribe_turn_completion_events(&shared);
    assert!(
        !b2b_submit(&shared, cleanup.clone(), &f.history)
            .await
            .unwrap()
    );
    assert!(
        shared
            .mailbox(key.channel_id)
            .snapshot()
            .await
            .cancel_token
            .is_none()
    );
    assert!(actor.is_completion_cleanup());
    assert_eq!(
        shared
            .restart
            .global_active
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(std::fs::read(&row).unwrap(), before);
    assert!(events.try_recv().is_err());
    assert!(
        !f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap()
            .cleanup_complete
    );
    std::fs::remove_dir(&queue).unwrap();
    std::fs::write(&queue, bytes).unwrap();
    assert!(b2b_submit(&shared, cleanup, &f.history).await.unwrap());
    assert!(!row.exists());
    assert!(events.try_recv().unwrap().queue_is_eligible());
}

#[tokio::test]
async fn native_8148_s2_transfer_closes_debt_without_successor_row_or_wake() {
    use super::super::super::inflight;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    for (n, (successor_lease, missing_row, reuse_actor)) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (true, false, true),
    ]
    .into_iter()
    .enumerate()
    {
        let f = Fixture::at(
            runtime.path().join(format!("runtime/transfer-{n}.json")),
            8148991 + n as u64,
            true,
        );
        let shared = b2b_shared(None);
        let (key, actor, cleanup) = s2_primary(&f, &shared).await;
        assert!(
            cleanup.drive(&f.history, &shared).await.is_err(),
            "result must already be durable"
        );
        let released = shared
            .mailbox(key.channel_id)
            .finish_turn(super::super::super::queue_persistence_context(
                &shared,
                &ProviderKind::Claude,
                key.channel_id,
            ))
            .await;
        assert!(released.removed_token.is_some());
        let successor = if reuse_actor {
            actor.clone()
        } else {
            Arc::new(CancelToken::new())
        };
        if successor_lease {
            shared
                .mailbox(key.channel_id)
                .restore_active_turn(
                    successor.clone(),
                    serenity::model::id::UserId::new(7),
                    serenity::model::id::MessageId::new(key.user_msg_id + 1),
                )
                .await;
        }
        let path = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            f.ticket.channel_id,
        );
        let mut foreign = serde_json::to_value(
            inflight::load_inflight_state(&ProviderKind::Claude, f.ticket.channel_id).unwrap(),
        )
        .unwrap();
        std::fs::write(&path, b"{unreadable").unwrap();
        assert!(
            b2b_submit(&shared, cleanup.clone(), &f.history)
                .await
                .is_err()
        );
        assert!(
            !f.history
                .native_settlement("native-fixture", &f.ticket)
                .unwrap()
                .cleanup_complete
        );

        foreign["turn_nonce"] = "successor".into();
        let bytes = serde_json::to_vec(&foreign).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        if missing_row {
            std::fs::remove_file(&path).unwrap();
        }
        let mut events =
            super::super::super::turn_completion_events::subscribe_turn_completion_events(&shared);
        let active = shared
            .restart
            .global_active
            .load(std::sync::atomic::Ordering::SeqCst);
        for _ in 0..2 {
            assert!(
                b2b_submit(&shared, cleanup.clone(), &f.history)
                    .await
                    .unwrap()
            );
        }
        if missing_row {
            assert!(!path.exists());
        } else {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
        assert!(events.try_recv().is_err());
        assert_eq!(
            shared
                .restart
                .global_active
                .load(std::sync::atomic::Ordering::SeqCst),
            active
        );
        assert!(!actor.is_completion_cleanup());
        let snapshot = shared.mailbox(key.channel_id).snapshot().await;
        if successor_lease {
            assert!(Arc::ptr_eq(
                snapshot.cancel_token.as_ref().unwrap(),
                &successor
            ));
        } else {
            assert!(snapshot.cancel_token.is_none());
        }
        assert!(
            f.history
                .native_settlement("native-fixture", &f.ticket)
                .unwrap()
                .cleanup_complete
        );
        assert!(
            crate::services::tui_input::human_frame::history::v2::native::boot_records(
                &f.ticket
                    .source
                    .as_ref()
                    .unwrap()
                    .transcript
                    .with_extension("json"),
                "claude"
            )
            .unwrap()
            .is_empty()
        );
    }
}

#[tokio::test]
async fn native_8148_s2_panic_payload_is_contained_and_native_actor_still_replies() {
    for owned in [false, true] {
        let result: Result<(), String> = super::super::actor_state::contain_panic(async move {
            if owned {
                std::panic::panic_any(String::from("native-owned-payload"));
            } else {
                panic!("native-borrowed-payload");
            }
        })
        .await;
        assert_eq!(
            result.unwrap_err(),
            if owned {
                "native-owned-payload"
            } else {
                "native-borrowed-payload"
            }
        );
    }
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::at(runtime.path().join("runtime/panic.json"), 8148980, true);
    let shared = b2b_shared(None);
    let (_, _, cleanup) = s2_primary(&f, &shared).await;
    assert!(b2b_submit(&shared, cleanup, &f.history).await.unwrap());
}

#[tokio::test]
async fn native_8148_s2_withdrawal_preseed_same_and_different_ticket_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::message_outbox::{
        OutboxMessage, enqueue_outbox_pg_returning_id_with_persistent_dedupe,
    };
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared = b2b_shared(Some(pool.clone()));
    for (n, same) in [true, false].into_iter().enumerate() {
        let f = Fixture::at(
            runtime.path().join(format!("runtime/preseed-{n}.json")),
            8148981 + n as u64,
            false,
        );
        let cleanup = s2_withdraw(&f, &shared).await;
        let mut ticket = f.ticket.clone();
        if !same {
            ticket.extra.insert("other-ticket".into(), true.into());
        }
        let stable = serde_json::to_vec(&("native-fixture", &ticket)).unwrap();
        let dedupe = format!("native-withdrawal:{}", blake3::hash(&stable));
        let target = format!("channel:{}", f.ticket.channel_id);
        enqueue_outbox_pg_returning_id_with_persistent_dedupe(
            &pool,
            OutboxMessage {
                target: &target,
                content: "입력이 실행 전에 철회되었습니다.",
                bot: "claude",
                source: "retained_input_notice",
                reason_code: None,
                session_key: Some(&dedupe),
            },
        )
        .await
        .unwrap();
        assert!(b2b_submit(&shared, cleanup, &f.history).await.unwrap());
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM message_outbox WHERE target=$1 AND source='retained_input_notice'").bind(&target).fetch_one(&pool).await.unwrap();
        assert_eq!(count, if same { 1 } else { 2 });
        assert!(
            f.history
                .native_settlement("native-fixture", &f.ticket)
                .unwrap()
                .withdrawal_notice
        );
    }
    drop(shared);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_8148_s2_blocked_pg_does_not_hold_finalizer_actor() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(1).await;
    let shared = b2b_shared(Some(pool.clone()));
    let f = Fixture::at(runtime.path().join("runtime/blocked.json"), 8148983, false);
    let cleanup = s2_withdraw(&f, &shared).await;
    let held = pool.acquire().await.unwrap();
    let submit = tokio::spawn({
        let shared = shared.clone();
        let history = f.history.clone();
        async move { b2b_submit(&shared, cleanup, &history).await }
    });
    crate::test_support::wait::await_within(
        async {
            while f
                .history
                .native_settlement("native-fixture", &f.ticket)
                .unwrap()
                .outcome
                .is_none()
            {
                tokio::task::yield_now().await;
            }
        },
        crate::test_support::wait::WAIT_CAP,
    )
    .await;
    let key = TurnKey::new(ChannelId::new(8148984), 8148985, 1);
    shared
        .turn_finalizer
        .register_start(key, ProviderKind::Claude, RelayOwnerKind::None, &shared);
    let actor = CancelToken::new();
    assert!(
        !crate::test_support::wait::await_within(
            shared.turn_finalizer.native_scope(key, &actor),
            std::time::Duration::from_millis(500)
        )
        .await
        .unwrap()
    );
    assert!(
        !submit.is_finished(),
        "Start must be answered before PG timeout"
    );
    let error = crate::test_support::wait::await_within(submit, std::time::Duration::from_secs(5))
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.contains("timed out"), "{error}");
    let pending = f
        .history
        .native_settlement("native-fixture", &f.ticket)
        .unwrap();
    assert!(pending.outcome.is_some());
    assert!(!pending.withdrawal_notice && !pending.cleanup_complete);
    drop(held);
    drop(shared);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_8148_p2_transfer_ledger_settlement_is_a_neutral_bridge_exit() {
    use super::super::super::{inflight, turn_bridge::native_input};
    use super::super::native_prepare::Handle;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::at(runtime.path().join("runtime/frames.json"), 81489951, true);
    let shared = b2b_shared(None);
    let (key, actor) = b2b_active(&f, &shared).await;
    let row = b2b_row(&f);
    shared.turn_finalizer.register_start(
        key,
        ProviderKind::Claude,
        super::super::RelayOwnerKind::None,
        &shared,
    );
    let handle = Handle::prepare(
        &shared,
        key,
        Some(actor.clone()),
        f.history.clone(),
        "native-fixture".into(),
        f.ticket.clone(),
    )
    .await
    .unwrap();
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
    let cleanup = prepared
        .admit(
            &batch,
            &batch,
            0,
            &f.ticket.source.as_ref().unwrap().target,
            &f.history,
            &shared,
        )
        .await
        .unwrap();
    let successor = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        "next-episode".into(),
    )));
    shared
        .mailbox(key.channel_id)
        .restore_active_turn(
            successor.clone(),
            serenity::model::id::UserId::new(7),
            serenity::model::id::MessageId::new(key.user_msg_id + 7),
        )
        .await;
    shared
        .restart
        .global_active
        .store(77, std::sync::atomic::Ordering::SeqCst);
    let mut next = row.clone();
    next.user_msg_id = key.user_msg_id + 7;
    next.turn_nonce = Some("next-episode".into());
    next.hosted_execution_nonce = Some("next-runtime".into());
    let path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        row.channel_id,
    );
    let bytes = serde_json::to_vec(&next).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let mut events =
        super::super::super::turn_completion_events::subscribe_turn_completion_events(&shared);
    assert!(b2b_submit(&shared, cleanup, &f.history).await.unwrap());
    assert!(!actor.is_completion_cleanup());
    assert_eq!(
        native_input::state(&shared, &ProviderKind::Claude, &row, &actor).await,
        native_input::State::Settled
    );
    assert_eq!(
        native_input::state(&shared, &ProviderKind::Claude, &row, &successor).await,
        native_input::State::Legacy
    );
    for mut body in [
        "external-after-close-1".to_string(),
        "external-after-close-2".into(),
        "external-after-close-3".into(),
    ] {
        assert!(
            native_input::reject_legacy_body(
                &shared,
                &ProviderKind::Claude,
                &row,
                &actor,
                &mut body
            )
            .await
        );
        assert!(body.is_empty());
    }
    assert!(native_input::neutral_exit(&shared, &ProviderKind::Claude, &row, &actor).await);
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
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    assert_eq!(
        shared
            .restart
            .global_active
            .load(std::sync::atomic::Ordering::SeqCst),
        77
    );
    assert!(events.try_recv().is_err());
    drop(handle);
}

async fn control_next_head(
    mailbox: crate::services::turn_orchestrator::ChannelMailboxHandle,
    channel: u64,
    root: &std::path::Path,
) {
    use crate::services::discord::queue_io::input_owner::drive::{self, Drive, Front};
    use crate::services::turn_orchestrator::PendingQueueItem;
    use crate::test_support::wait::{WAIT_CAP, await_within};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    mailbox
        .v2_adopt(
            root.join("runtime").join(format!("next-{channel}.json")),
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

async fn control_case(kind: &str) {
    use super::super::native_prepare::Handle;
    use crate::test_support::wait::{WAIT_CAP, await_within};
    let runtime = crate::config::TestRuntimeRootGuard::new();
    if kind == "cancel" {
        let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
        let outside = registry.handle(ChannelId::new(81489790));
        let error = await_within(
            outside.v2_adopt(runtime.path().join("outside-runtime.json"), "probe".into()),
            WAIT_CAP,
        )
        .await
        .unwrap_err();
        assert!(error.contains("directory sync unconfirmed"), "{error}");
        let inside = registry.handle(ChannelId::new(81489791));
        await_within(
            inside.v2_adopt(
                runtime.path().join("runtime/inside-runtime.json"),
                "probe".into(),
            ),
            WAIT_CAP,
        )
        .await
        .unwrap();
    }
    let channel = 81489701
        + ["cancel", "terminate", "clear", "stop"]
            .iter()
            .position(|v| *v == kind)
            .unwrap() as u64;
    let f = Fixture::at(
        runtime.path().join(format!("runtime/{kind}.json")),
        channel,
        true,
    );
    let shared = b2b_shared(None);
    let (key, actor) = b2b_active(&f, &shared).await;
    b2b_row(&f);
    shared.turn_finalizer.register_start(
        key,
        ProviderKind::Claude,
        super::super::RelayOwnerKind::None,
        &shared,
    );
    if kind == "clear" {
        shared
            .mailbox(key.channel_id)
            .v2_adopt(
                runtime.path().join(format!("runtime/next-{channel}.json")),
                format!("next-{channel}"),
            )
            .await
            .unwrap();
        let input = serde_json::from_value(serde_json::json!({"author_id":7,"message_id":channel+9,"channel_id":channel,"source_message_ids":[channel+9],"queued_generation":1,"text":"V2 clear pending"})).unwrap();
        shared
            .mailbox(key.channel_id)
            .v2_admit_ticket(input, "clear-pending".into())
            .await
            .unwrap();
        assert!(
            shared
                .mailbox(key.channel_id)
                .snapshot()
                .await
                .v2_queue
                .is_some()
        );
    }
    let custody = await_within(
        Handle::prepare(
            &shared,
            key,
            Some(actor.clone()),
            f.history.clone(),
            "native-fixture".into(),
            f.ticket.clone(),
        ),
        WAIT_CAP,
    )
    .await
    .unwrap();
    if kind == "clear" {
        let row_path = super::super::super::inflight::inflight_state_path(
            &super::super::super::inflight::inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            channel,
        );
        let row_before = std::fs::read(&row_path).unwrap();
        let before =
            std::fs::read(runtime.path().join(format!("runtime/next-{channel}.json"))).unwrap();
        let rejected = super::super::super::mailbox_clear_channel(
            &shared,
            &ProviderKind::Claude,
            key.channel_id,
        )
        .await;
        assert!(rejected.removed_token.is_none());
        assert_eq!(std::fs::read(row_path).unwrap(), row_before);
        assert!(rejected.persistence.failed.contains(&"native_input"));
        assert_eq!(
            std::fs::read(runtime.path().join(format!("runtime/next-{channel}.json"))).unwrap(),
            before
        );
        assert!(
            f.history
                .native_settlement("native-fixture", &f.ticket)
                .unwrap()
                .outcome
                .is_none()
        );
        assert_eq!(
            shared
                .restart
                .global_active
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }
    f.history
        .note_native_any_key("native-fixture", &f.ticket)
        .unwrap();
    let mut events =
        super::super::super::turn_completion_events::subscribe_turn_completion_events(&shared);
    match kind {
        "cancel" | "terminate" => {
            let event = if kind == "cancel" {
                super::super::TerminalEvent::Cancel
            } else {
                super::super::TerminalEvent::HostTerminated
            };
            let result = await_within(
                shared.turn_finalizer.submit_terminal(
                    key,
                    ProviderKind::Claude,
                    event,
                    super::super::FinalizeContext::host_terminated(),
                    shared.clone(),
                ),
                WAIT_CAP,
            )
            .await;
            assert!(matches!(
                result,
                super::super::FinalizeOutcome::Finalized { .. }
            ));
        }
        "clear" => {
            let result = await_within(
                super::super::super::mailbox_clear_channel(
                    &shared,
                    &ProviderKind::Claude,
                    key.channel_id,
                ),
                WAIT_CAP,
            )
            .await;
            assert!(result.removed_token.is_some());
            assert!(result.persistence.failed.is_empty());
        }
        "stop" => {
            let result = await_within(
                super::super::super::mailbox_finish::mailbox_finish_stopped_turn(
                    &shared,
                    &ProviderKind::Claude,
                    key.channel_id,
                    Some(&actor),
                    super::super::super::mailbox_finish::MailboxLookup::Peek,
                ),
                WAIT_CAP,
            )
            .await;
            assert!(matches!(
                result,
                crate::services::turn_orchestrator::TokenFinish::Finished(_)
            ));
        }
        _ => unreachable!(),
    }
    let settlement = f
        .history
        .native_settlement("native-fixture", &f.ticket)
        .unwrap();
    assert_eq!(
        settlement.outcome.as_ref().map(|o| &o.disposition),
        Some(&Disposition::Cancelled),
        "{kind}: control outcome must precede release"
    );
    await_within(
        async {
            while !f
                .history
                .native_settlement("native-fixture", &f.ticket)
                .unwrap()
                .cleanup_complete
            {
                tokio::task::yield_now().await;
            }
        },
        WAIT_CAP,
    )
    .await;
    assert!(
        shared
            .mailbox(key.channel_id)
            .snapshot()
            .await
            .cancel_token
            .is_none()
    );
    assert!(
        super::super::super::inflight::load_inflight_state_read_only(
            &ProviderKind::Claude,
            channel
        )
        .is_none()
    );
    assert_eq!(
        shared
            .restart
            .global_active
            .load(std::sync::atomic::Ordering::SeqCst),
        if matches!(kind, "clear" | "stop") {
            1
        } else {
            0
        }
    );
    let before = f
        .history
        .native_settlement("native-fixture", &f.ticket)
        .unwrap();
    let stale = await_within(
        shared.mailbox(key.channel_id).finish_turn_if_token(
            Some(actor),
            crate::services::discord::queue_persistence_context(
                &shared,
                &ProviderKind::Claude,
                key.channel_id,
            ),
        ),
        WAIT_CAP,
    )
    .await;
    assert!(!matches!(
        stale,
        crate::services::turn_orchestrator::TokenFinish::Finished(_)
    ));
    assert_eq!(
        f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap(),
        before
    );
    await_within(async {
        loop {
            let event = events.recv().await.unwrap();
            if event.phase == super::super::super::turn_completion_events::TurnCompletionPhase::QueueEligible { break; }
        }
    }, WAIT_CAP).await;
    control_next_head(shared.mailbox(key.channel_id), channel, runtime.path()).await;
    drop(custody);
}

#[tokio::test]
async fn native_8148_control_cancel_lease_row_and_head_progress() {
    control_case("cancel").await;
}
#[tokio::test]
async fn native_8148_control_terminate_lease_row_and_head_progress() {
    control_case("terminate").await;
}
#[tokio::test]
async fn native_8148_control_clear_lease_row_and_head_progress() {
    control_case("clear").await;
}
#[tokio::test]
async fn native_8148_control_stop_lease_row_and_head_progress() {
    control_case("stop").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_8148_control_failed_admission_and_successor_preserve_all_axes() {
    use super::super::native_prepare::Handle;
    use crate::test_support::wait::{WAIT_CAP, await_within};
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::at(runtime.path().join("runtime/refusal.json"), 81489720, true);
    let shared = b2b_shared(None);
    let (key, actor) = b2b_active(&f, &shared).await;
    let row = b2b_row(&f);
    shared.turn_finalizer.register_start(
        key,
        ProviderKind::Claude,
        super::super::RelayOwnerKind::None,
        &shared,
    );
    let custody = await_within(
        Handle::prepare(
            &shared,
            key,
            Some(actor.clone()),
            f.history.clone(),
            "native-fixture".into(),
            f.ticket.clone(),
        ),
        WAIT_CAP,
    )
    .await
    .unwrap();
    let bytes = std::fs::read(runtime.path().join("runtime/refusal.json")).unwrap();
    for keyed in [false, true] {
        if keyed {
            f.history
                .note_native_any_key("native-fixture", &f.ticket)
                .unwrap();
        }
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(
                runtime
                    .path()
                    .join("runtime/refusal.json")
                    .with_extension("lock"),
            )
            .unwrap();
        if keyed {
            lock.lock().unwrap();
        }
        let result = await_within(
            shared.mailbox(key.channel_id).finish_turn_if_token(
                Some(actor.clone()),
                crate::services::discord::queue_persistence_context(
                    &shared,
                    &ProviderKind::Claude,
                    key.channel_id,
                ),
            ),
            WAIT_CAP,
        )
        .await;
        assert!(matches!(
            result,
            crate::services::turn_orchestrator::TokenFinish::Unavailable
        ));
        drop(lock);
        assert!(
            f.history
                .native_settlement("native-fixture", &f.ticket)
                .unwrap()
                .outcome
                .is_none()
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
        assert_eq!(
            shared
                .restart
                .global_active
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }
    let successor = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        "successor".into(),
    )));
    await_within(
        shared.mailbox(key.channel_id).restore_active_turn(
            successor.clone(),
            serenity::model::id::UserId::new(8),
            serenity::model::id::MessageId::new(key.user_msg_id + 10),
        ),
        WAIT_CAP,
    )
    .await;
    let result = await_within(
        super::super::super::mailbox_clear_channel(&shared, &ProviderKind::Claude, key.channel_id),
        WAIT_CAP,
    )
    .await;
    assert!(result.removed_token.is_none());
    assert!(result.persistence.failed.contains(&"native_input"));
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
    let after = super::super::super::inflight::load_inflight_state_read_only(
        &ProviderKind::Claude,
        row.channel_id,
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(after).unwrap(),
        serde_json::to_value(row).unwrap()
    );
    assert!(
        f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap()
            .outcome
            .is_none()
    );
    assert!(!bytes.is_empty());
    drop(custody);
}

#[tokio::test]
async fn native_8148_control_native_result_and_control_outcome_have_one_winner() {
    use super::super::native_prepare::Handle;
    use crate::test_support::wait::{WAIT_CAP, await_within};
    let runtime = crate::config::TestRuntimeRootGuard::new();
    for native_first in [true, false] {
        let f = Fixture::at(
            runtime
                .path()
                .join(format!("runtime/race-{native_first}.json")),
            81489730 + u64::from(native_first),
            true,
        );
        let shared = b2b_shared(None);
        let (key, actor) = b2b_active(&f, &shared).await;
        b2b_row(&f);
        shared.turn_finalizer.register_start(
            key,
            ProviderKind::Claude,
            super::super::RelayOwnerKind::None,
            &shared,
        );
        let custody = await_within(
            Handle::prepare(
                &shared,
                key,
                Some(actor.clone()),
                f.history.clone(),
                "native-fixture".into(),
                f.ticket.clone(),
            ),
            WAIT_CAP,
        )
        .await
        .unwrap();
        let prepared = NativePrepared::capture(
            &f.history,
            "native-fixture",
            f.ticket.clone(),
            key,
            Some(actor),
            &shared,
        )
        .await
        .unwrap();
        prepared.any_key(&f.history).unwrap();
        let batch = b2b_batch(&f);
        let work = prepared
            .admit(
                &batch,
                &batch,
                0,
                &f.ticket.source.as_ref().unwrap().target,
                &f.history,
                &shared,
            )
            .await
            .unwrap();
        if native_first {
            assert!(b2b_submit(&shared, work, &f.history).await.unwrap());
        } else {
            await_within(
                shared.turn_finalizer.submit_terminal(
                    key,
                    ProviderKind::Claude,
                    super::super::TerminalEvent::Cancel,
                    super::super::FinalizeContext::host_terminated(),
                    shared.clone(),
                ),
                WAIT_CAP,
            )
            .await;
            assert!(b2b_submit(&shared, work, &f.history).await.is_err());
        }
        if native_first {
            await_within(
                shared.turn_finalizer.submit_terminal(
                    key,
                    ProviderKind::Claude,
                    super::super::TerminalEvent::Cancel,
                    super::super::FinalizeContext::host_terminated(),
                    shared.clone(),
                ),
                WAIT_CAP,
            )
            .await;
        }
        await_within(
            async {
                while !f
                    .history
                    .native_settlement("native-fixture", &f.ticket)
                    .unwrap()
                    .cleanup_complete
                {
                    tokio::task::yield_now().await;
                }
            },
            WAIT_CAP,
        )
        .await;
        let result = f
            .history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap();
        assert_eq!(
            result.outcome.unwrap().disposition,
            if native_first {
                Disposition::Executed
            } else {
                Disposition::Cancelled
            }
        );
        assert_eq!(
            shared
                .restart
                .global_active
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert!(
            shared
                .mailbox(key.channel_id)
                .snapshot()
                .await
                .cancel_token
                .is_none()
        );
        control_next_head(
            shared.mailbox(key.channel_id),
            f.ticket.channel_id,
            runtime.path(),
        )
        .await;
        drop(custody);
    }
}

fn review_soft_input(id: u64) -> crate::services::turn_orchestrator::Intervention {
    crate::services::turn_orchestrator::pending_queue_item_to_intervention(
        serde_json::from_value(serde_json::json!({"author_id":7,"message_id":id,
            "source_message_ids":[id],"queued_generation":1,"text":"retained V1 input"}))
        .unwrap(),
        std::time::SystemTime::now(),
        std::time::Instant::now(),
    )
}

#[tokio::test]
async fn native_8148_review_v1_partial_release_keeps_terminal_accounting() {
    use super::super::super::{inflight, turn_completion_events};
    use crate::services::turn_orchestrator::test_support::{fail_queue_saves, queue_save_faults};
    use crate::test_support::wait::{WAIT_CAP, await_within};
    use std::sync::atomic::Ordering;
    let _root = crate::config::TestRuntimeRootGuard::new();
    for capture_only in [false, true] {
        let channel = ChannelId::new(81489850 + u64::from(capture_only));
        let shared = b2b_shared(None);
        let provider = ProviderKind::Gemini;
        let finish_context = super::super::FinalizeContext {
            clear_inflight: true,
            ..super::super::FinalizeContext::bridge()
        };
        let actor = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
            "v1-exact".into(),
        )));
        shared
            .mailbox(channel)
            .restore_active_turn(
                actor.clone(),
                serenity::model::id::UserId::new(7),
                serenity::model::id::MessageId::new(channel.get() + 1),
            )
            .await;
        shared.restart.global_active.store(1, Ordering::SeqCst);
        let key = TurnKey::new(channel, channel.get() + 1, 1).with_episode_nonce(Some("v1-exact"));
        let mut row = inflight::InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            7,
            key.user_msg_id,
            key.user_msg_id,
            "pipe input".into(),
            None,
            None,
            None,
            None,
            0,
        );
        row.turn_nonce = Some("v1-exact".into());
        row.born_generation = 1;
        inflight::save_inflight_state(&row).unwrap();
        let context = super::super::super::queue_persistence_context(&shared, &provider, channel);
        assert!(
            shared
                .mailbox(channel)
                .enqueue(review_soft_input(channel.get() + 2), context)
                .await
                .enqueued
        );
        assert!(shared.mailbox(channel).snapshot().await.v2_queue.is_none());
        let mut events = turn_completion_events::subscribe_turn_completion_events(&shared);
        fail_queue_saves(channel, 1);
        if capture_only {
            let captured = await_within(
                super::super::episode::claim_normal_episode(
                    &shared,
                    &provider,
                    key,
                    true,
                    Some(actor.clone()),
                ),
                WAIT_CAP,
            )
            .await
            .unwrap()
            .unwrap();
            assert!(
                captured
                    .finish
                    .removed_token
                    .as_ref()
                    .is_some_and(|t| Arc::ptr_eq(t, &actor))
            );
            assert!(captured.finish.persistence_error.is_some());
            let snapshot = captured.snapshot;
            let finish = captured.finish;
            let result = super::super::finalize::do_finalize_with_release(
                key,
                provider,
                &super::super::TerminalEvent::Complete,
                finish_context,
                snapshot.as_ref(),
                &shared,
                Some(finish),
                false,
            )
            .await;
            assert!(matches!(
                result,
                super::super::FinalizeOutcome::Finalized {
                    removed_token: Some(_),
                    ..
                }
            ));
        } else {
            let mut ledger = HashMap::from([(
                key.exact_key(),
                LedgerEntry::registered(
                    key,
                    provider.clone(),
                    super::super::RelayOwnerKind::None,
                    None,
                    super::super::completion_admission::CompletionAdmissionPlan::Immediate,
                    None,
                ),
            )]);
            let mut pending = HashMap::new();
            let evidence = || super::super::episode::TerminalEvidence {
                episode_captured: true,
                turn_nonce: Some("v1-exact".into()),
                claim_snapshot: None,
                probed_tmux_session: None,
            };
            let result = await_within(
                super::handle_terminal(
                    &mut ledger,
                    &mut pending,
                    key,
                    provider.clone(),
                    super::super::TerminalEvent::Complete,
                    finish_context,
                    evidence(),
                    &shared,
                ),
                WAIT_CAP,
            )
            .await;
            assert!(matches!(
                result,
                super::super::FinalizeOutcome::Finalized {
                    removed_token: Some(_),
                    ..
                }
            ));
            assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 0);
            let mut released = 0;
            let mut eligible = 0;
            while let Ok(event) = events.try_recv() {
                released += usize::from(
                    event.phase == turn_completion_events::TurnCompletionPhase::MailboxReleased,
                );
                eligible += usize::from(event.queue_is_eligible());
            }
            assert_eq!((released, eligible), (1, 1));
            let duplicate = super::handle_terminal(
                &mut ledger,
                &mut pending,
                key,
                provider,
                super::super::TerminalEvent::Complete,
                finish_context,
                evidence(),
                &shared,
            )
            .await;
            assert!(matches!(
                duplicate,
                super::super::FinalizeOutcome::AlreadyFinalized
            ));
            super::super::reconcile::reconcile(&mut ledger, &mut pending, &shared).await;
            assert!(events.try_recv().is_err());
        }
        assert_eq!(queue_save_faults(channel, true), 0);
        assert!(actor.cancelled.load(Ordering::SeqCst));
        assert!(actor.is_completion_cleanup());
        assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 0);
        let snapshot = shared.mailbox(channel).snapshot().await;
        assert!(snapshot.cancel_token.is_none());
        assert_eq!(snapshot.intervention_queue.len(), 1);
        assert!(
            inflight::load_inflight_state_read_only(&row.provider_kind().unwrap(), channel.get())
                .is_none()
        );
    }
}

#[tokio::test]
async fn native_8148_review_control_partial_release_debits_before_retry() {
    use crate::services::turn_orchestrator::test_support::{fail_queue_saves, queue_save_faults};
    use std::sync::atomic::Ordering;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::at(
        runtime.path().join("runtime/control-partial.json"),
        81489852,
        true,
    );
    let shared = b2b_shared(None);
    let (key, actor) = b2b_active(&f, &shared).await;
    let row = b2b_row(&f);
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
    prepared.install_control(&f.history, &shared).await.unwrap();
    prepared.any_key(&f.history).unwrap();
    let context = super::super::super::queue_persistence_context(
        &shared,
        &ProviderKind::Claude,
        key.channel_id,
    );
    assert!(
        shared
            .mailbox(key.channel_id)
            .enqueue(review_soft_input(81489854), context)
            .await
            .enqueued
    );
    f.history
        .try_native_control(
            "native-fixture",
            &f.ticket,
            Disposition::Cancelled,
            Some(serde_json::to_value(row).unwrap()),
        )
        .unwrap();
    let mut entry = LedgerEntry::registered(
        key,
        ProviderKind::Claude,
        super::super::RelayOwnerKind::None,
        None,
        super::super::completion_admission::CompletionAdmissionPlan::Immediate,
        None,
    );
    entry.completion_admission.native_input = Some(false);
    entry.native_inputs.insert("partial".into(), prepared);
    let mut events =
        super::super::super::turn_completion_events::subscribe_turn_completion_events(&shared);
    fail_queue_saves(key.channel_id, 1);
    super::native_policy::reconcile(&mut entry, &shared).await;
    assert_eq!(queue_save_faults(key.channel_id, true), 0);
    assert!(
        shared
            .mailbox(key.channel_id)
            .snapshot()
            .await
            .cancel_token
            .is_none()
    );
    assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 0);
    assert!(
        !f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap()
            .cleanup_complete
    );
    assert!(events.try_recv().is_err());
    super::native_policy::reconcile(&mut entry, &shared).await;
    assert!(
        f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap()
            .cleanup_complete
    );
    assert!(events.try_recv().unwrap().queue_is_eligible());
    super::native_policy::reconcile(&mut entry, &shared).await;
    assert!(events.try_recv().is_err());
    assert_eq!(shared.restart.global_active.load(Ordering::SeqCst), 0);
    assert!(!actor.is_completion_cleanup());
}
