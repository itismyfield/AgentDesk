use super::{InflightTurnIdentity, InflightTurnState};
use crate::services::discord::inflight::save_store::identity_gate::runtime_stamp::stamp_runtime_handoff_if_matches_identity_in_root;
use crate::services::discord::inflight::{
    GuardedSaveOutcome, inflight_state_path, save_inflight_state_if_matches_identity_in_root,
    save_inflight_state_in_root,
};
use crate::services::provider::ProviderKind;

#[derive(Clone, Copy, Debug)]
enum Writer {
    Completion,
    RuntimeStamp,
}

impl Writer {
    const ALL: [Self; 2] = [Self::Completion, Self::RuntimeStamp];

    fn save(
        self,
        root: &std::path::Path,
        state: &mut InflightTurnState,
        expected: &InflightTurnIdentity,
    ) -> GuardedSaveOutcome {
        match self {
            Self::Completion => save_inflight_state_if_matches_identity_in_root(
                root,
                state,
                expected,
                expected.turn_start_offset,
            ),
            Self::RuntimeStamp => stamp_runtime_handoff_if_matches_identity_in_root(
                root,
                state,
                expected,
                "replay_projection_tests",
            ),
        }
    }
}

struct Fixture {
    root: tempfile::TempDir,
    path: std::path::PathBuf,
    baseline: InflightTurnState,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("isolated inflight root");
        let mut seed = InflightTurnState::new(
            ProviderKind::Codex,
            6_008_001_200,
            Some("projection-session".into()),
            6_008_001_201,
            6_008_001_202,
            6_008_001_203,
            "original request".into(),
            Some("provider-session".into()),
            Some("AgentDesk-codex-replay-projection".into()),
            Some("/tmp/replay-projection-output.jsonl".into()),
            None,
            32,
        );
        seed.full_response = "보존할 본문 ".into();
        seed.streaming_rollover_frozen_msg_ids = vec![6_008_001_204];
        save_inflight_state_in_root(root.path(), &seed).expect("seed durable row");
        let path = inflight_state_path(root.path(), &ProviderKind::Codex, seed.channel_id);
        let baseline = Self::read(&path);
        Self {
            root,
            path,
            baseline,
        }
    }

    fn read(path: &std::path::Path) -> InflightTurnState {
        serde_json::from_slice(&std::fs::read(path).expect("read durable row"))
            .expect("decode durable row")
    }

    fn load(&self) -> InflightTurnState {
        Self::read(&self.path)
    }

    fn bytes(&self) -> Vec<u8> {
        std::fs::read(&self.path).expect("read durable bytes")
    }
}

#[test]
fn guarded_projection_merge_preserves_hold_body_and_frozen_debt_in_both_writer_orders() {
    for writer in Writer::ALL {
        for hold_first in [false, true] {
            let fixture = Fixture::new();
            let expected = InflightTurnIdentity::from_state(&fixture.baseline);
            let mut held = fixture.baseline.clone();
            held.replay_receipt_id = Some(6_008);
            held.replay_hold_reasons = vec!["output observed".into()];
            held.streaming_rollover_frozen_msg_ids.push(6_008_001_205);
            let mut progress = fixture.baseline.clone();
            progress.replay_receipt_id = Some(6_008);
            progress.replay_hold_reasons = vec!["tool observed".into()];
            progress.full_response.push_str("이어진 결과");
            progress.response_sent_offset = fixture.baseline.full_response.len();
            progress.last_offset = 96;
            progress
                .streaming_rollover_frozen_msg_ids
                .push(6_008_001_206);
            let wanted_body = progress.full_response.clone();
            let (first, second) = if hold_first {
                (&mut held, &mut progress)
            } else {
                (&mut progress, &mut held)
            };
            assert_eq!(
                writer.save(fixture.root.path(), first, &expected),
                GuardedSaveOutcome::Saved
            );
            assert!(fixture.load().save_generation > second.save_generation);
            assert_eq!(
                writer.save(fixture.root.path(), second, &expected),
                GuardedSaveOutcome::Saved
            );

            let mut stale_unheld = fixture.baseline.clone();
            assert_eq!(
                writer.save(fixture.root.path(), &mut stale_unheld, &expected),
                GuardedSaveOutcome::Saved
            );
            let durable = fixture.load();
            assert_eq!(
                durable.replay_receipt_id,
                Some(6_008),
                "{writer:?}, hold_first={hold_first}"
            );
            assert!(durable.replay_rerun_blocked());
            assert_eq!(durable.replay_hold_reasons.len(), 2);
            assert!(
                durable
                    .replay_hold_reasons
                    .iter()
                    .any(|r| r == "output observed")
            );
            assert!(
                durable
                    .replay_hold_reasons
                    .iter()
                    .any(|r| r == "tool observed")
            );
            assert_eq!(durable.full_response, wanted_body);
            assert_eq!(
                durable.response_sent_offset,
                fixture.baseline.full_response.len()
            );
            assert_eq!(durable.last_offset, 96);
            let mut frozen = durable.streaming_rollover_frozen_msg_ids.clone();
            frozen.sort_unstable();
            assert_eq!(frozen, vec![6_008_001_204, 6_008_001_205, 6_008_001_206]);
            assert_eq!(
                serde_json::to_value(&stale_unheld).unwrap(),
                serde_json::to_value(&durable).unwrap()
            );
        }
    }
}

#[test]
fn guarded_projection_conflicting_receipt_is_denied_without_changing_durable_bytes() {
    for writer in Writer::ALL {
        let fixture = Fixture::new();
        let expected = InflightTurnIdentity::from_state(&fixture.baseline);
        let mut held = fixture.baseline.clone();
        held.replay_receipt_id = Some(6_008);
        held.replay_hold_reasons = vec!["original hold".into()];
        assert_eq!(
            writer.save(fixture.root.path(), &mut held, &expected),
            GuardedSaveOutcome::Saved
        );
        let before = fixture.bytes();
        let mut conflict = fixture.load();
        conflict.replay_receipt_id = Some(6_009);
        conflict
            .replay_hold_reasons
            .push("foreign receipt reason".into());
        conflict.full_response.push_str("must not persist");
        conflict
            .streaming_rollover_frozen_msg_ids
            .push(6_008_001_299);
        assert_eq!(
            writer.save(fixture.root.path(), &mut conflict, &expected),
            GuardedSaveOutcome::AuthorityPinned
        );
        assert_eq!(fixture.bytes(), before, "{writer:?}");
    }
}

#[test]
fn guarded_projection_other_nonce_is_denied_without_changing_durable_bytes() {
    for writer in Writer::ALL {
        let fixture = Fixture::new();
        let expected = InflightTurnIdentity::from_state(&fixture.baseline);
        let mut held = fixture.baseline.clone();
        held.replay_receipt_id = Some(6_008);
        held.replay_hold_reasons = vec!["original hold".into()];
        assert_eq!(
            writer.save(fixture.root.path(), &mut held, &expected),
            GuardedSaveOutcome::Saved
        );
        let before = fixture.bytes();
        let mut other_episode = fixture.load();
        other_episode.turn_nonce = Some("other-episode".into());
        other_episode
            .replay_hold_reasons
            .push("other episode reason".into());
        other_episode.full_response.push_str("must not persist");
        assert_ne!(
            writer.save(fixture.root.path(), &mut other_episode, &expected),
            GuardedSaveOutcome::Saved
        );
        assert_eq!(fixture.bytes(), before, "{writer:?}");
    }
}

#[test]
fn pre_migration_json_without_projection_keeps_both_guarded_writers_normal() {
    for writer in Writer::ALL {
        for legacy_nonce in [false, true] {
            let fixture = Fixture::new();
            let mut legacy = serde_json::to_value(&fixture.baseline).expect("legacy row JSON");
            let object = legacy.as_object_mut().unwrap();
            object.remove("replay_receipt_id");
            object.remove("replay_hold_reasons");
            if legacy_nonce {
                object.remove("turn_nonce");
            }
            std::fs::write(&fixture.path, serde_json::to_vec(&legacy).unwrap())
                .expect("seed old-format row");
            let mut local = fixture.load();
            assert_eq!(local.replay_receipt_id, None);
            assert!(!local.replay_rerun_blocked());
            let expected = InflightTurnIdentity::from_state(&local);
            local.full_response.push_str("normal progress");
            assert_eq!(
                writer.save(fixture.root.path(), &mut local, &expected),
                GuardedSaveOutcome::Saved,
                "{writer:?}, legacy_nonce={legacy_nonce}"
            );
            let durable = fixture.load();
            assert_eq!(durable.full_response, local.full_response);
            assert_eq!(durable.replay_receipt_id, None);
            assert!(durable.replay_hold_reasons.is_empty());
            assert!(!durable.replay_rerun_blocked());
        }
    }
}
