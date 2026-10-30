use super::*;
use crate::services::tui_o::writer::codex_snapshot::{HostPath, Pane, PaneState, Target};
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};
use crate::services::tui_prompt_dedupe::binding_events::{BindingCause, BindingEvidence};
use chrono::Utc;

const PANE: &str = "AgentDesk-codex-ba-factory";
const CURRENT: &str = "11111111111111111111111111111111";
const PRIOR: &str = "22222222222222222222222222222222";
fn source(number: u64) -> SourceId {
    SourceId {
        session_id: format!("source-{number}"),
        path: format!("/native/rollout-{number}.jsonl").into(),
        dev: 1,
        ino: number,
    }
}
fn event(seq: u64, nonce: &str, old: Option<SourceId>, new: BindingTarget) -> BindingEvent {
    BindingEvent {
        seq,
        channel_id: 7,
        provider: "codex".into(),
        tmux_session: PANE.into(),
        execution_nonce: Some(nonce.into()),
        old,
        new,
        cause: BindingCause::Startup,
        parent_hint: None,
        evidence: BindingEvidence {
            hook_event: Some("session_start".into()),
            received_at: Utc::now(),
        },
        committed_at: Utc::now(),
    }
}
fn fixture() -> (Snapshot, Vec<BindingEvent>) {
    let target = Target {
        channel: 7,
        tmux: PANE.into(),
        nonce: CURRENT.into(),
        host: "mac-book".into(),
        owner_runtime_root: "/owner".into(),
        host_path: HostPath::Legacy,
    };
    let context = BindingContext {
        schema: 1,
        provider: "codex".into(),
        created_at: Utc::now(),
        execution_nonce: CURRENT.into(),
        tmux_session: PANE.into(),
        channel_id: Some(7),
        owner_runtime_root: target.owner_runtime_root.clone(),
        host: Some(target.host.clone()),
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: Some("/native".into()),
        first_prompt_digest: None,
        source_policy: Some("shadow".into()),
    };
    let events = vec![
        event(1, PRIOR, None, BindingTarget::Source(source(1))),
        event(
            2,
            CURRENT,
            Some(source(1)),
            BindingTarget::Source(source(2)),
        ),
    ];
    (
        Snapshot {
            target,
            context,
            current: source(2),
            held: vec![source(2)],
            tail: 2,
            log_digest: "whole-log".into(),
            census_digest: "whole-census".into(),
            panes: vec![Pane {
                name: PANE.into(),
                state: PaneState::Live,
            }],
        },
        events,
        &SettledCompanions::default(),
    )
}
fn requested(
    snapshot: &Snapshot,
    events: &[BindingEvent],
    seq: u64,
) -> Result<RetentionClaimV1, &'static str> {
    claim(
        snapshot,
        &source_key(&source(1)),
        &source(1),
        seq,
        &events[0].tmux_session,
        events,
    )
}

#[test]
fn codex_retention_same_pane_uses_last_binder_without_old_context() {
    let (snapshot, events) = fixture();
    let claim = requested(&snapshot, &events, 2).unwrap();
    assert_eq!(claim.version, 1);
    assert_eq!(claim.binder_nonce, PRIOR);
    assert_eq!(
        (claim.binding_seq, claim.binder_seq, claim.hop_seq),
        (1, 1, 2)
    );
    assert_eq!(claim.successor, source(2));
    assert_eq!(claim.basis, RetentionBasisV1::SamePane);
    assert_eq!(
        claim.observation,
        identity(&Observation::Complete(Box::new(snapshot))).unwrap()
    );
}

#[test]
fn codex_retention_pending_keeps_original_binding_and_resolution_sequences_distinct() {
    let (mut snapshot, mut events) = fixture();
    events[0].new = BindingTarget::Pending {
        payload_session_id: source(1).session_id,
        payload_transcript_path: Some(source(1).path.display().to_string()),
    };
    events.insert(
        1,
        event(
            2,
            PRIOR,
            None,
            BindingTarget::Resolved {
                pending_seq: 1,
                source: source(1),
            },
        ),
    );
    events[2].seq = 3;
    snapshot.tail = 3;
    let accepted = requested(&snapshot, &events, 3).unwrap();
    assert_eq!(
        (accepted.binding_seq, accepted.binder_seq, accepted.hop_seq),
        (1, 2, 3)
    );
    events[1].execution_nonce = Some(CURRENT.into());
    assert_eq!(
        requested(&snapshot, &events, 3).unwrap_err(),
        "codex_retention_resolution_ambiguous"
    );
}

#[test]
fn codex_retention_current_held_revisit_and_unresolved_history_stay_unknown() {
    let (snapshot, events) = fixture();
    let mut held = snapshot.clone();
    held.held.push(source(1));
    assert_eq!(
        requested(&held, &events, 2).unwrap_err(),
        "codex_retention_source_held"
    );
    let mut own = events.clone();
    own[0].execution_nonce = Some(CURRENT.into());
    assert_eq!(
        requested(&snapshot, &own, 2).unwrap_err(),
        "codex_retention_current_execution"
    );
    let mut revisited = events.clone();
    revisited.push(event(
        3,
        CURRENT,
        Some(source(2)),
        BindingTarget::Source(source(1)),
    ));
    let mut snapshot = snapshot;
    snapshot.tail = 3;
    assert_eq!(
        requested(&snapshot, &revisited, 2).unwrap_err(),
        "codex_retention_source_revisited"
    );
    revisited[2].new = BindingTarget::Pending {
        payload_session_id: "unknown".into(),
        payload_transcript_path: None,
    };
    assert_eq!(
        requested(&snapshot, &revisited, 2).unwrap_err(),
        "codex_retention_pending"
    );
}

#[test]
fn codex_retention_rejects_full_tail_change_beyond_candidate_cutoff() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = crate::config::runtime_root().unwrap();
    crate::services::tui_prompt_dedupe::binding_events::set_test_root(Some(&root));
    let (mut snapshot, mut events) = fixture();
    let path = root.join("binding_events/7.log");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let write = |events: &[BindingEvent]| {
        std::fs::write(
            &path,
            events
                .iter()
                .map(|event| format!("{}\n", serde_json::to_string(event).unwrap()))
                .collect::<String>(),
        )
        .unwrap()
    };
    write(&events);
    let log = observe_channel(7, 65_536).unwrap();
    snapshot.log_digest = log.digest;
    assert!(for_hop(&snapshot, &source_key(&source(1)), &source(1), 2, PANE).is_ok());
    events.push(event(
        3,
        CURRENT,
        None,
        BindingTarget::Rejected {
            payload_session_id: "other".into(),
            payload_transcript_path: None,
            reason: "diagnostic".into(),
        },
    ));
    write(&events);
    assert_eq!(
        for_hop(&snapshot, &source_key(&source(1)), &source(1), 2, PANE).unwrap_err(),
        "codex_retention_log_changed"
    );
    crate::services::tui_prompt_dedupe::binding_events::forget_channel_for_tests(7);
    crate::services::tui_prompt_dedupe::binding_events::set_test_root(None);
}

#[test]
fn codex_retention_other_pane_requires_exact_local_context_and_known_host_death() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (mut snapshot, mut events) = fixture();
    snapshot.target.owner_runtime_root =
        crate::config::runtime_root().unwrap().display().to_string();
    let pane = "AgentDesk-codex-ba-historical-absent";
    for event in &mut events {
        event.tmux_session = pane.into();
    }
    assert_eq!(
        requested(&snapshot, &events, 2).unwrap_err(),
        "codex_retention_old_context_unavailable"
    );
    let mut old = snapshot.context.clone();
    old.owner_runtime_root = snapshot.target.owner_runtime_root.clone();
    old.execution_nonce = PRIOR.into();
    old.tmux_session = pane.into();
    let prepared = PreparedIncarnation::create(old.clone()).unwrap();
    let marker = crate::services::tmux_common::session_temp_path(pane, "host_kind");
    assert_eq!(
        requested(&snapshot, &events, 2).unwrap_err(),
        "codex_retention_historical_not_local"
    );
    for value in ["", "future-host", "herdr"] {
        std::fs::write(&marker, value).unwrap();
        assert_eq!(
            requested(&snapshot, &events, 2).unwrap_err(),
            "codex_retention_historical_not_local"
        );
    }
    std::fs::write(&marker, "process").unwrap();
    assert!(matches!(
        requested(&snapshot, &events, 2).unwrap().basis,
        RetentionBasisV1::LocalHistorical { .. }
    ));
    std::fs::write(&marker, "herdr").unwrap();
    assert_eq!(
        requested(&snapshot, &events, 2).unwrap_err(),
        "codex_retention_historical_not_local"
    );
    std::fs::write(&marker, "process").unwrap();
    old.host = Some("remote".into());
    std::fs::write(&prepared.path, serde_json::to_vec(&old).unwrap()).unwrap();
    assert!(
        requested(&snapshot, &events, 2).is_err(),
        "remote historical context cannot authorize local retirement"
    );
    assert_eq!(
        requested(&snapshot, &events, 2).unwrap_err(),
        "codex_retention_old_context_not_local"
    );
}
