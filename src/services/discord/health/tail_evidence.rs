use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum UnmeasuredReason {
    MissingMeasurement,
    UntrustedWire,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "grade", content = "detail", rename_all = "snake_case")]
enum TailGrade {
    Unmeasured(UnmeasuredReason),
    #[cfg(test)]
    MeasuredEmpty,
    #[cfg(test)]
    MeasuredBacklog(std::num::NonZeroU64),
}

/// An observation carrier; only a future source-proof producer can grant a measured grade.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(in crate::services::discord) struct TailObservation {
    #[serde(flatten)]
    grade: TailGrade,
}

impl Default for TailObservation {
    fn default() -> Self {
        Self {
            grade: TailGrade::Unmeasured(UnmeasuredReason::MissingMeasurement),
        }
    }
}

impl TailObservation {
    pub(in crate::services::discord) fn is_unmeasured(&self) -> bool {
        matches!(self.grade, TailGrade::Unmeasured(_))
    }
}

impl<'de> Deserialize<'de> for TailObservation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Wire telemetry cannot restore process-local source proof or a consumption permit.
        serde::de::IgnoredAny::deserialize(deserializer)?;
        Ok(Self {
            grade: TailGrade::Unmeasured(UnmeasuredReason::UntrustedWire),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default, Deserialize)]
    struct CompatibleSnapshot {
        #[serde(default)]
        tail_observation: TailObservation,
    }

    #[test]
    fn old_default_missing_and_invalid_grades_are_unmeasured() {
        assert!(TailObservation::default().is_unmeasured());
        for wire in [
            "{}",
            r#"{"unread_bytes":0}"#,
            r#"{"tail_observation":null}"#,
            r#"{"tail_observation":{"grade":"future_grade"}}"#,
            r#"{"tail_observation":{"grade":0}}"#,
            r#"{"tail_observation":{"grade":"measured_backlog","detail":0}}"#,
            r#"{"tail_observation":false}"#,
            r#"{"tail_observation":[]}"#,
        ] {
            let restored: CompatibleSnapshot = serde_json::from_str(wire).unwrap();
            assert!(restored.tail_observation.is_unmeasured(), "wire={wire}");
        }
    }

    #[test]
    fn measured_wire_round_trip_cannot_restore_authority() {
        for observation in [
            TailObservation {
                grade: TailGrade::MeasuredEmpty,
            },
            TailObservation {
                grade: TailGrade::MeasuredBacklog(std::num::NonZeroU64::new(9).unwrap()),
            },
        ] {
            assert!(
                !observation.is_unmeasured(),
                "trusted control must be measured"
            );
            let wire = serde_json::to_vec(&observation).unwrap();
            let restored: TailObservation = serde_json::from_slice(&wire).unwrap();
            assert!(
                restored.is_unmeasured(),
                "wire cannot carry internal authority"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dormant_carriers_preserve_real_snapshot_and_manual_apply() {
        use crate::services::discord::{self as discord, inflight, relay_recovery};
        use crate::services::provider::{CancelToken, ProviderKind};
        use poise::serenity_prelude::{ChannelId, MessageId, UserId};
        use std::sync::{Arc, atomic::Ordering};

        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let provider = ProviderKind::Codex;
        let shared = discord::make_shared_data_for_tests();
        let registry = super::super::HealthRegistry::new();
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;

        for (index, (contents, frontier, unread)) in [
            (None, 0, None),
            (Some(""), 0, Some(0)),
            (Some(""), 100, Some(0)),
            (Some("abc"), 0, Some(3)),
        ]
        .into_iter()
        .enumerate()
        {
            let channel = ChannelId::new(5_996_100 + index as u64);
            let output = root.path().join(format!("tail-{index}.jsonl"));
            if let Some(contents) = contents {
                std::fs::write(&output, contents).unwrap();
            }
            let row = inflight::InflightTurnState::new(
                provider.clone(),
                channel.get(),
                None,
                1,
                2,
                channel.get() + 1,
                "carrier fixture".into(),
                None,
                None,
                Some(output.to_string_lossy().into_owned()),
                None,
                0,
            );
            inflight::save_inflight_state(&row).unwrap();
            let row_path = inflight::inflight_state_path(
                &discord::runtime_store::discord_inflight_root().unwrap(),
                &provider,
                channel.get(),
            );
            let row_bytes = std::fs::read(&row_path).unwrap();
            let token = Arc::new(CancelToken::new());
            assert!(
                discord::mailbox_try_start_turn(
                    &shared,
                    channel,
                    token.clone(),
                    UserId::new(2),
                    MessageId::new(channel.get() + 1),
                )
                .await
            );
            let coord = discord::TmuxRelayCoord::new(channel);
            coord
                .confirmed_end_offset
                .store(frontier, Ordering::Release);
            shared.tmux_relay_coords.insert(channel, Arc::new(coord));
            shared
                .restart
                .global_active
                .store(index + 1, Ordering::Relaxed);
            let mailbox_before = discord::mailbox_snapshot(&shared, channel).await;

            let snapshot = registry
                .snapshot_watcher_state_for_provider(&provider, channel.get())
                .await
                .unwrap();
            assert_eq!(snapshot.unread_bytes, unread);
            assert_eq!(snapshot.last_relay_offset, frontier);
            assert_eq!(
                snapshot.last_capture_offset,
                contents.map(|value| value.len() as u64)
            );
            assert_eq!(snapshot.tail_observation, TailObservation::default());
            let legacy_bytes = serde_json::to_vec(&snapshot).unwrap();
            assert!(
                !serde_json::to_value(&snapshot)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .contains_key("tail_observation")
            );

            for wire in [
                serde_json::json!({}),
                serde_json::json!({"tail_observation": {"grade": "measured_empty"}}),
                serde_json::json!({"tail_observation": {"grade": "measured_backlog", "detail": 0}}),
            ] {
                let restored: CompatibleSnapshot = serde_json::from_value(wire).unwrap();
                let mut compatible = snapshot.clone();
                compatible.tail_observation = restored.tail_observation;
                assert!(compatible.tail_observation.is_unmeasured());
                assert_eq!(serde_json::to_vec(&compatible).unwrap(), legacy_bytes);
                assert_eq!(
                    relay_recovery::stale_mailbox_idle_tail_admits(&provider, &compatible, false,),
                    unread == Some(0),
                    "S1 must preserve the existing raw-tail decision"
                );
            }

            let response = Box::pin(relay_recovery::run_relay_recovery(
                &registry,
                Some(provider.as_str()),
                channel.get(),
                true,
            ))
            .await
            .unwrap();
            assert!(!response.applied);
            assert!(response.skipped);
            assert_eq!(response.decision.evidence.unread_bytes, unread);
            assert_eq!(
                response.decision.evidence.tail_observation,
                snapshot.tail_observation
            );
            assert!(
                !serde_json::to_value(&response).unwrap()["decision"]["evidence"]
                    .as_object()
                    .unwrap()
                    .contains_key("tail_observation")
            );
            assert_eq!(std::fs::read(&row_path).unwrap(), row_bytes);
            let mailbox_after = discord::mailbox_snapshot(&shared, channel).await;
            assert!(Arc::ptr_eq(
                mailbox_after.cancel_token.as_ref().unwrap(),
                &token
            ));
            assert_eq!(
                mailbox_after.active_turn_nonce,
                mailbox_before.active_turn_nonce
            );
            assert_eq!(
                mailbox_after.intervention_queue.len(),
                mailbox_before.intervention_queue.len()
            );
            assert!(!token.cancelled.load(Ordering::Acquire));
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                index + 1
            );
        }

        let detail =
            serde_json::to_value(super::super::build_health_snapshot(&registry).await).unwrap();
        let mailboxes = detail["mailboxes"].as_array().unwrap();
        assert_eq!(
            mailboxes.len(),
            4,
            "detail entry must actually enumerate the seeded actors"
        );
        assert!(
            mailboxes
                .iter()
                .all(|mailbox| mailbox.get("tail_observation").is_none())
        );
    }
}
