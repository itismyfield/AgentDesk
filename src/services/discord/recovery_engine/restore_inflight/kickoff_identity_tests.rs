#![cfg(unix)]

use super::kickoff_identity::*;
use super::*;
use crate::services::discord::{Intervention, InterventionMode, mailbox_snapshot};
use crate::services::platform::tmux::PaneLiveness;

fn reacquired_row(channel: u64) -> inflight::InflightTurnState {
    crate::services::discord::tmux::tmux_watcher::liveness::build_watcher_reacquire_inflight_state(
        super::tests::recovery_state(ProviderKind::Claude, channel),
    )
}

fn assistant_line(text: &str) -> String {
    format!(
        "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}}}\n"
    )
}

#[test]
fn ownerless_reacquired_row_has_no_kickoff_identity() {
    let state = reacquired_row(6_029_001);
    assert_eq!((state.request_owner_user_id, state.user_msg_id), (0, 0));
    assert!(recovery_kickoff_identity(&state).is_none());
}

#[test]
fn owned_row_kickoff_identity_matches_persisted_ids() {
    for user_msg_id in [2, 0] {
        let mut state = super::tests::recovery_state(ProviderKind::Claude, 6_029_002);
        state.user_msg_id = user_msg_id;
        let identity = recovery_kickoff_identity(&state).expect("owned row kicks off");
        assert_eq!(
            identity.request_owner,
            UserId::new(state.request_owner_user_id)
        );
        assert_eq!(identity.user_message_id, optional_message_id(user_msg_id));
    }
}

#[test]
fn ownerless_plan_maps_pane_liveness_to_disposition_inputs() {
    let dir = tempfile::tempdir().expect("output dir");
    let output = dir.path().join("out.jsonl");
    let earlier = assistant_line("earlier turn");
    std::fs::write(&output, format!("{earlier}{}", assistant_line("this turn"))).expect("output");
    let output = output.to_string_lossy().into_owned();
    let mut state = reacquired_row(6_029_003);
    state.turn_start_offset = Some(earlier.len() as u64);

    assert_eq!(
        plan_ownerless_dead_pane_row(&state, PaneLiveness::Live, &output),
        None
    );
    let dead = plan_ownerless_dead_pane_row(&state, PaneLiveness::DeadOrAbsent, &output)
        .expect("dead pane plan");
    assert_eq!(
        dead,
        OwnerlessDeadPanePlan {
            stop_source: "recovery_ownerless_dead_pane",
            branch: "ownerless_dead_pane",
            tmux_alive: false,
            best_response: "this turn".to_string(),
            notice_text: interrupted_recovery_message(&state, "this turn"),
        }
    );
    let probe_error = plan_ownerless_dead_pane_row(&state, PaneLiveness::ProbeError, &output)
        .expect("probe error plan");
    assert!(probe_error.tmux_alive);
    assert_eq!(probe_error.best_response, "this turn");

    state.turn_start_offset = None;
    let from_zero = plan_ownerless_dead_pane_row(&state, PaneLiveness::DeadOrAbsent, &output)
        .expect("plan from offset 0");
    assert!(from_zero.best_response.contains("earlier turn"));

    state.turn_start_offset = Some(std::fs::metadata(&output).expect("eof").len());
    state.full_response = "saved partial".to_string();
    let fallback = plan_ownerless_dead_pane_row(&state, PaneLiveness::DeadOrAbsent, &output)
        .expect("fallback plan");
    assert_eq!(fallback.best_response, "saved partial");
    assert_eq!(
        fallback.notice_text,
        interrupted_recovery_message(&state, "saved partial")
    );
}

fn queued(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(id),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: crate::services::discord::runtime_store::process_generation(),
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("queued {id}"),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

struct ApplyCase {
    outcome: RecoveryRelayOutcome,
    liveness: PaneLiveness,
    budget_exhausted: bool,
    row_kept: bool,
}

async fn run_apply_case(channel: u64, case: ApplyCase) {
    let root = tempfile::tempdir().expect("runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let shared = super::super::make_shared_data_for_tests_with_storage(None);
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(channel);
    let output = root.path().join("out.jsonl");
    std::fs::write(&output, assistant_line("partial")).expect("output");
    let mut state = reacquired_row(channel);
    state.born_generation = 0;
    state.output_path = Some(output.to_string_lossy().into_owned());
    state.turn_start_offset = Some(0);
    if case.budget_exhausted {
        state.recovery_relay_attempts = inflight::RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET - 1;
    }
    inflight::save_inflight_state(&state).expect("persist ownerless row");
    let state = inflight::load_inflight_state(&provider, channel).expect("persisted row");
    let queued_ids = [6_029_901, 6_029_902];
    for id in queued_ids {
        crate::services::discord::mailbox_enqueue_intervention(
            &shared,
            &provider,
            channel_id,
            queued(id),
        )
        .await;
    }

    let plan = plan_ownerless_dead_pane_row(
        &state,
        case.liveness,
        state.output_path.as_deref().expect("output"),
    )
    .expect("non-live plan");
    apply_ownerless_dead_pane_outcome(&shared, &provider, &state, &plan, case.outcome).await;

    let row = inflight::load_inflight_state(&provider, channel);
    assert_eq!(row.is_some(), case.row_kept, "{:?}", case.outcome);
    if let Some(row) = row {
        assert_eq!(
            row.recovery_relay_attempts,
            state.recovery_relay_attempts + 1
        );
    }
    let snapshot = mailbox_snapshot(&shared, channel_id).await;
    assert!(snapshot.cancel_token.is_none(), "no recovery anchor minted");
    assert!(snapshot.active_request_owner.is_none());
    let remaining: Vec<u64> = snapshot
        .intervention_queue
        .iter()
        .map(|item| item.message_id.get())
        .collect();
    assert_eq!(
        remaining, queued_ids,
        "queue is neither dropped nor superseded"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn ownerless_apply_disposes_row_per_relay_outcome_without_anchor_or_queue_loss() {
    let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let cases = [
        // Delivered notice finishes and clears the row.
        (
            RecoveryRelayOutcome::Delivered,
            PaneLiveness::DeadOrAbsent,
            false,
            false,
        ),
        // A permanent Discord verdict force-clears the row.
        (
            RecoveryRelayOutcome::PermanentFailure,
            PaneLiveness::DeadOrAbsent,
            false,
            false,
        ),
        // A transient failure within budget keeps the row and counts the attempt.
        (
            RecoveryRelayOutcome::TransientFailure,
            PaneLiveness::DeadOrAbsent,
            false,
            true,
        ),
        // A confirmed-dead pane lets budget exhaustion clear the row.
        (
            RecoveryRelayOutcome::TransientFailure,
            PaneLiveness::DeadOrAbsent,
            true,
            false,
        ),
        // A failed probe never budget-clears.
        (
            RecoveryRelayOutcome::TransientFailure,
            PaneLiveness::ProbeError,
            true,
            true,
        ),
    ];
    for (index, (outcome, liveness, budget_exhausted, row_kept)) in cases.into_iter().enumerate() {
        run_apply_case(
            6_029_100 + index as u64,
            ApplyCase {
                outcome,
                liveness,
                budget_exhausted,
                row_kept,
            },
        )
        .await;
    }
}

#[test]
fn ownerless_guard_precedes_recovery_marker_and_kickoff() {
    let source = include_str!("../restore_inflight.rs");
    let production = source.split("#[cfg(test)]\nmod tests").next().unwrap();
    assert_eq!(
        production
            .matches("UserId::new(state.request_owner_user_id)")
            .count(),
        0
    );
    assert_eq!(
        production
            .matches("recovery_kickoff_identity(&state)")
            .count(),
        1
    );
    assert_eq!(production.matches("UserId::new(").count(), 0);
    let guard = production
        .split("        let Some(kickoff_identity) = kickoff_identity::recovery_kickoff_identity(&state) else {")
        .collect::<Vec<_>>();
    assert_eq!(guard.len(), 2);
    assert!(guard[0].ends_with("            continue;\n        }\n\n"));
    assert!(guard[1].contains(
        "            continue;\n        };\n\n        shared\n            .restart\n            .recovering_channels\n            .insert("
    ));
    assert!(guard[1].contains("            kickoff_identity.request_owner,\n"));

    let helper = include_str!("kickoff_identity.rs");
    assert_eq!(helper.matches("UserId::new(").count(), 1);
    assert_eq!(helper.matches("UserId::new(NonZeroU64::new(").count(), 1);
    assert_eq!(helper.matches("MessageId::new(").count(), 0);
}
