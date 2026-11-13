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
