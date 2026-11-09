use super::*;

// Keep the previous reader executable to pin its handling of a newer persisted format.
impl TurnViewReconciler {
    pub(super) fn load_persisted_target_v3(
        &self,
        target: TurnViewTarget,
        shared: &SharedData,
        source: &'static str,
    ) -> Option<AppliedTarget> {
        let path = Self::persisted_target_path(target)?;
        let text = fs::read_to_string(&path).ok()?;
        let record = match serde_json::from_str::<PersistedTargetState>(&text) {
            Ok(record) => record,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    source,
                    "turn view persisted reaction state was malformed; deleting"
                );
                let _ = fs::remove_file(&path);
                return None;
            }
        };
        if !matches!(
            record.version,
            PERSISTED_STATE_VERSION
                | LEGACY_QUEUED_HOURGLASS_STATE_VERSION
                | QUEUED_MARKER_ONLY_STATE_VERSION
        ) || record.provider != shared.provider.as_str()
            || TurnViewTargetKind::from_str(&record.kind) != Some(target.kind)
            || record.channel_id != target.channel_id.get()
            || record.message_id != target.message_id.get()
        {
            tracing::warn!(
                path = %path.display(),
                version = record.version,
                provider = %record.provider,
                kind = %record.kind,
                channel_id = record.channel_id,
                message = record.message_id,
                source,
                "turn view persisted reaction state did not match target; deleting"
            );
            let _ = fs::remove_file(&path);
            return None;
        }
        let Some(recorded_applied) = TurnViewState::from_str(&record.applied) else {
            tracing::warn!(
                path = %path.display(),
                applied = %record.applied,
                source,
                "turn view persisted reaction state had unknown applied value; deleting"
            );
            let _ = fs::remove_file(&path);
            return None;
        };
        if recorded_applied == TurnViewState::None {
            let _ = fs::remove_file(&path);
            return None;
        }
        let identity = self.resolve_persisted_identity(&record, shared, source)?;
        let legacy_queue_reactions = match record.version {
            PERSISTED_STATE_VERSION if recorded_applied.is_queue_marker() => {
                vec![reaction_set::for_state(recorded_applied)[0]]
            }
            LEGACY_QUEUED_HOURGLASS_STATE_VERSION if recorded_applied.is_queue_marker() => vec![
                reaction_set::for_state(recorded_applied)[0],
                reaction_set::for_state(TurnViewState::Pending)[0],
            ],
            _ => Vec::new(),
        };
        let applied = if legacy_queue_reactions.is_empty() {
            recorded_applied
        } else {
            TurnViewState::None
        };
        let mut target = Self::applied_target(
            TurnViewOwner::new(record.owner_generation, record.owner_turn_id),
            applied,
            identity,
            record.start_attempt_id.map(TurnStartAttempt),
        );
        target.legacy_queue_reactions = legacy_queue_reactions;
        Some(target)
    }
}

#[tokio::test]
async fn previous_v3_reader_drops_v4_state_without_removing_its_mailbox() {
    let _root = scoped_runtime_root();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let target = target_with(100_000_000_078_540, 100_000_000_078_541);
    let record =
        persisted_record_with_version(&shared, target, shared.provider.as_str(), "queued", 4);
    write_persisted(&record, target);
    let reconciler = TurnViewReconciler::default();
    reg_7832_seed_reactions(&reconciler, target, target.kind.identity_label(), &['📬']);
    let before = reconciler.ops();
    assert!(
        reconciler
            .load_persisted_target_v3(target, &shared, "test_previous_v3")
            .is_none()
    );
    assert!(!persisted_exists(target));
    assert_eq!(
        reconciler.ops(),
        before,
        "the old loader only deletes its state file"
    );
    assert_eq!(
        snapshot_reactions(&reconciler, target),
        vec![expected('📬', target.kind.identity_label())]
    );
}

#[tokio::test]
async fn persisted_queue_versions_distinguish_live_mailbox_from_legacy_mailbox() {
    let _root = scoped_runtime_root();
    let shared = crate::services::discord::make_shared_data_for_tests();
    for version in [3, 4] {
        let target = target_with(
            100_000_000_078_530,
            100_000_000_078_530 + u64::from(version),
        );
        let record = persisted_record_with_version(
            &shared,
            target,
            shared.provider.as_str(),
            "queued",
            version,
        );
        write_persisted(&record, target);
        let reconciler = TurnViewReconciler::default();
        let loaded = reconciler
            .load_persisted_target(target, &shared, "test_queue_version")
            .unwrap();
        assert_eq!(
            loaded.applied,
            if version == 4 {
                TurnViewState::Queued
            } else {
                TurnViewState::None
            }
        );
        assert_eq!(
            loaded.legacy_queue_reactions,
            if version == 4 { vec![] } else { vec!['📬'] }
        );
        reg_7832_seed_reactions(&reconciler, target, target.kind.identity_label(), &['📬']);
        reconciler
            .note_queue_marker_removed(
                &shared,
                target,
                TurnViewOwner::new(record.owner_generation, record.owner_turn_id.clone()),
                TurnViewIdentity::Test("unused"),
                '📬',
                "test_queue_version_exit",
            )
            .await;
        assert!(
            snapshot_reactions(&reconciler, target).is_empty(),
            "version {version} exits cleanly"
        );
        assert!(!persisted_exists(target));
    }
}
