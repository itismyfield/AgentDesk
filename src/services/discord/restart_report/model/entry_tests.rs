use super::*;
use crate::services::discord::inflight::{InflightEpisodePin, InflightTurnState};
use crate::services::provider::ProviderKind;

fn row() -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        42,
        None,
        7,
        0,
        0,
        "prompt".into(),
        None,
        None,
        None,
        None,
        42,
    );
    state.turn_nonce = Some("nonce-a".into());
    state.turn_start_offset = Some(42);
    state.terminal_delivery_committed = true;
    state
}

fn capture(state: &InflightTurnState) -> RowBinding<InflightEpisodePin> {
    RowBinding::capture(
        InflightEpisodePin::from_state(state),
        state.save_generation,
        RowContext {
            nonce: state.turn_nonce.clone(),
            actor_cutoff: Some((7, 90)),
            turn_id: state.user_msg_id,
            turn_start_offset: state.turn_start_offset,
        },
    )
    .unwrap()
}

#[test]
fn real_full_pin_protects_successor_without_liveness_input() {
    let state = row();
    assert!(state.terminal_delivery_completed());
    let expected = ReportPin {
        provider: state.provider.clone(),
        channel_id: state.channel_id,
        attempt: RestartAttempt("a".into()),
        association: Association::RowEpisode(capture(&state)),
        state: RestartReportState::Awaiting,
    };
    assert_eq!(
        decide(
            &expected,
            &expected,
            Evidence::RowCompleted(capture(&state))
        ),
        Decision::ConsumeRowReport
    );
    let mut successors = Vec::new();
    let mut next = state.clone();
    next.turn_nonce = Some("nonce-b".into());
    successors.push(next);
    let mut next = state.clone();
    next.save_generation += 1;
    successors.push(next);
    let mut next = state.clone();
    next.turn_start_offset = Some(43);
    successors.push(next);
    let mut next = state.clone();
    next.request_owner_user_id += 1;
    successors.push(next);
    let mut next = state.clone();
    next.born_generation += 1;
    successors.push(next);
    let mut next = state.clone();
    next.started_at.push('x');
    successors.push(next);
    for next in successors {
        assert_eq!(
            decide(&expected, &expected, Evidence::RowCompleted(capture(&next))),
            Decision::Preserve
        );
    }
}

#[test]
fn old_format_and_existing_entry_are_unchanged() {
    use super::super::*;
    let root = tempfile::tempdir().unwrap();
    let scheduling_before = crate::services::recovery_defer::invocation_count();
    let decisions_before = decision_count();
    let ctx = RestartReportContext::from_bridge(ProviderKind::Claude, 42, None, None);
    let path = restart_report_path(root.path(), &ctx.provider, ctx.channel_id);
    let attempt = announce_restart_in_root(root.path(), &ctx).unwrap();
    let report = load_report(&path).unwrap().unwrap();
    let handoff =
        prepare_restart_handoff_in_root(root.path(), root.path(), &ctx, Some(&attempt)).unwrap();
    let handoff_path = restart_attempt_report_path(root.path(), &ctx, &handoff);
    let handoff_before = std::fs::read(&handoff_path).unwrap();
    assert_eq!(
        crate::services::recovery_defer::invocation_count(),
        scheduling_before
    );
    assert_eq!(decision_count(), decisions_before);
    let original = std::fs::read(&path).unwrap();
    let pin: ReportPin<InflightEpisodePin> = ReportPin {
        provider: report.provider,
        channel_id: report.channel_id,
        attempt: report.attempt,
        association: Association::LegacyUnassociated,
        state: report.state,
    };
    assert_eq!(decide(&pin, &pin, Evidence::Unverified), Decision::Preserve);
    assert_eq!(decision_count(), decisions_before + 1);
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(std::fs::read(&handoff_path).unwrap(), handoff_before);
    let wire: serde_json::Value = serde_json::from_slice(&original).unwrap();
    assert_eq!(wire["version"], 2);
    assert!(wire.get("association").is_none());
    assert_eq!(wire["attempt"], attempt.as_str());
    let legacy = br#"{"version":1,"provider":"claude","channel_id":42,"status":"pending","completed_at":"2026-08-01 00:00:00"}"#;
    let legacy_path = root.path().join("old.json");
    std::fs::write(&legacy_path, legacy).unwrap();
    let _: LegacyRestartReport = serde_json::from_slice(legacy).unwrap();
    assert_eq!(std::fs::read(legacy_path).unwrap(), legacy);
}
