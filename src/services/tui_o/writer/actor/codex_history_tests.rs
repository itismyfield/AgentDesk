use super::*;
use crate::services::tui_o::writer::codex_snapshot::{Pane, Target};
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;
use crate::services::tui_prompt_dedupe::binding_events::{
    BindingCause, BindingEvent, BindingEvidence,
};
use chrono::{DateTime, TimeDelta, Utc};

const NONCE: &str = "11111111111111111111111111111111";
const PRIOR: &str = "22222222222222222222222222222222";
const PANE: &str = "AgentDesk-codex-observation-test";

fn created() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-24T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn source(number: u64) -> SourceId {
    SourceId {
        session_id: format!("session-{number}"),
        path: format!("/native/sessions/source-{number}.jsonl").into(),
        dev: 1,
        ino: number,
    }
}

fn event(seq: u64, nonce: &str, old: Option<SourceId>, new: BindingTarget) -> BindingEvent {
    BindingEvent {
        seq,
        channel_id: 42,
        provider: "codex".into(),
        tmux_session: PANE.into(),
        execution_nonce: Some(nonce.into()),
        old,
        new,
        cause: BindingCause::Startup,
        parent_hint: None,
        evidence: BindingEvidence {
            hook_event: Some("session_start".into()),
            received_at: created(),
        },
        committed_at: created() + TimeDelta::seconds(seq as i64),
    }
}

fn inputs(mode: &str) -> Inputs {
    Inputs {
        target: Target {
            channel: 42,
            tmux: PANE.into(),
            nonce: NONCE.into(),
            host: "mac-book".into(),
            owner_runtime_root: "/owner".into(),
            host_path: HostPath::Legacy,
        },
        context: BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: created(),
            execution_nonce: NONCE.into(),
            tmux_session: PANE.into(),
            channel_id: Some(42),
            owner_runtime_root: "/owner".into(),
            host: Some("mac-book".into()),
            expected_native_session_id: (mode == "resume").then(|| source(1).session_id),
            launch_mode: mode.into(),
            provider_root: Some("/native".into()),
            first_prompt_digest: Some("first prompt".into()),
            source_policy: Some("shadow".into()),
        },
        current: source(1),
        events: vec![event(1, NONCE, None, BindingTarget::Source(source(1)))],
        tail: 1,
        log_digest: "whole-log-1".into(),
        census_digest: "whole-census".into(),
        panes: vec![Pane {
            name: PANE.into(),
            state: PaneState::Live,
        }],
        coverage: Coverage::Ok(()),
    }
}

fn complete(input: Inputs) -> Snapshot {
    match assess(input) {
        Observation::Complete(snapshot) => *snapshot,
        other => panic!("expected Complete, got {other:?}"),
    }
}

fn unknown(input: Inputs) {
    assert!(matches!(assess(input), Observation::Unknown(_)));
}

#[test]
fn codex_observation_fresh_holds_all_visits_but_not_the_initial_predecessor() {
    let mut input = inputs("fresh");
    input.events[0].old = Some(source(9));
    input.events.push(event(
        2,
        NONCE,
        Some(source(1)),
        BindingTarget::Source(source(2)),
    ));
    input.events.push(event(
        3,
        NONCE,
        Some(source(2)),
        BindingTarget::Source(source(1)),
    ));
    input.tail = 3;
    let observed = complete(input);
    assert_eq!(observed.current, source(1));
    assert_eq!(observed.held, vec![source(1), source(2)]);
    assert!(!observed.held.contains(&source(9)));

    let mut input = inputs("fresh");
    input.events[0].seq = 2;
    input
        .events
        .insert(0, event(1, PRIOR, None, BindingTarget::Source(source(9))));
    input.tail = 2;
    let events = input.events.clone();
    assert_eq!(complete(input).held, [source(1)]);
    let mut input = inputs("fresh");
    input.events = events;
    input
        .events
        .push(event(3, NONCE, None, BindingTarget::Source(source(2))));
    input.current = source(2);
    input.tail = 3;
    unknown(input);
}

#[test]
fn codex_observation_same_source_resume_needs_no_current_nonce_record() {
    let mut input = inputs("resume");
    input.events[0].execution_nonce = Some(PRIOR.into());
    input.events[0].committed_at = created() - TimeDelta::days(1);
    let observed = complete(input);
    assert_eq!(observed.held, vec![source(1)]);
    assert_eq!(observed.target.nonce, NONCE);
    let mut input = inputs("resume");
    input.events[0].execution_nonce = Some(PRIOR.into());
    input.context.expected_native_session_id = Some(source(8).session_id);
    unknown(input);
}

#[test]
fn codex_observation_resume_holds_the_expected_source_and_later_visits() {
    let mut input = inputs("resume");
    input.events[0].execution_nonce = Some(PRIOR.into());
    input.events.push(event(
        2,
        NONCE,
        Some(source(1)),
        BindingTarget::Source(source(2)),
    ));
    input.current = source(2);
    input.tail = 2;
    assert_eq!(complete(input).held, vec![source(1), source(2)]);
}

#[test]
fn codex_observation_pending_must_resolve_the_exact_original_claim() {
    let pending = BindingTarget::Pending {
        payload_session_id: source(2).session_id,
        payload_transcript_path: Some(source(2).path.display().to_string()),
    };
    let mut input = inputs("fresh");
    input
        .events
        .push(event(2, NONCE, Some(source(1)), pending.clone()));
    input.tail = 2;
    unknown(input);
    let resolved = |pending_seq| BindingTarget::Resolved {
        pending_seq,
        source: source(2),
    };
    let resolved_input = |pending_seq| {
        let mut input = inputs("fresh");
        input
            .events
            .push(event(2, NONCE, Some(source(1)), pending.clone()));
        input
            .events
            .push(event(3, NONCE, Some(source(1)), resolved(pending_seq)));
        input.current = source(2);
        input.tail = 3;
        input
    };
    assert_eq!(complete(resolved_input(2)).held, vec![source(1), source(2)]);
    unknown(resolved_input(1));
    let mut input = resolved_input(2);
    input.events[2].execution_nonce = Some(PRIOR.into());
    unknown(input);
    let mut input = resolved_input(2);
    if let BindingTarget::Pending {
        payload_session_id, ..
    } = &mut input.events[1].new
    {
        *payload_session_id = "other-session".into();
    }
    unknown(input);
}

#[test]
fn codex_observation_source_chain_and_runtime_tip_must_agree() {
    let mut input = inputs("fresh");
    input.events.push(event(
        2,
        NONCE,
        Some(source(8)),
        BindingTarget::Source(source(2)),
    ));
    input.tail = 2;
    input.current = source(2);
    unknown(input);
    let mut input = inputs("fresh");
    input.current.ino += 1;
    unknown(input);
    let mut input = inputs("fresh");
    input.events.push(event(
        2,
        PRIOR,
        Some(source(1)),
        BindingTarget::Source(source(2)),
    ));
    input.tail = 2;
    input.current = source(2);
    unknown(input);
}

#[test]
fn codex_observation_incomplete_or_additional_live_census_is_unknown() {
    for reason in [
        "partial inventory",
        "budget exhausted",
        "missing owner",
        "TTL miss",
    ] {
        let mut input = inputs("fresh");
        input.coverage = Coverage::Err(reason.into());
        unknown(input);
    }
    for state in [PaneState::Live, PaneState::Unknown] {
        let mut input = inputs("fresh");
        input.panes.push(Pane {
            name: "another-pane".into(),
            state,
        });
        unknown(input);
    }
    let mut input = inputs("fresh");
    input.panes.clear();
    unknown(input);
    let mut input = inputs("fresh");
    input.panes.push(input.panes[0].clone());
    unknown(input);
}

#[test]
fn codex_observation_full_channel_tail_and_census_survive_the_fold() {
    let mut input = inputs("fresh");
    let mut historical = event(2, PRIOR, None, BindingTarget::Source(source(8)));
    historical.tmux_session = "ended-pane".into();
    input.events.push(historical);
    input.panes.push(Pane {
        name: "ended-pane".into(),
        state: PaneState::Dead,
    });
    input.tail = 2;
    input.log_digest = "whole-log-with-other-pane-after-cutoff".into();
    let observed = complete(input);
    assert_eq!(observed.tail, 2);
    assert_eq!(
        observed.log_digest,
        "whole-log-with-other-pane-after-cutoff"
    );
    assert_eq!(observed.census_digest, "whole-census");
    assert_eq!(observed.held, vec![source(1)]);
    let mut input = inputs("fresh");
    input.tail = 2;
    unknown(input);
    let mut input = inputs("fresh");
    input.events[0].seq = 2;
    unknown(input);
    let mut input = inputs("fresh");
    input.events[0].tmux_session = "omitted-pane".into();
    unknown(input);
}

#[test]
fn codex_observation_preserves_the_full_immutable_context() {
    let input = inputs("fresh");
    let expected = input.context.clone();
    assert_eq!(complete(input).context, expected);
    for mismatch in [
        "host", "root", "nonce", "provider", "channel", "pane", "created",
    ] {
        let mut input = inputs("fresh");
        match mismatch {
            "host" => input.context.host = Some("other-host".into()),
            "root" => input.context.provider_root = Some("/other-root".into()),
            "nonce" => input.context.execution_nonce = PRIOR.into(),
            "provider" => input.context.provider = "claude".into(),
            "channel" => input.context.channel_id = Some(99),
            "pane" => input.context.tmux_session = "another-pane".into(),
            "created" => input.context.created_at += TimeDelta::days(1),
            _ => unreachable!(),
        }
        unknown(input);
    }
}

#[test]
fn codex_observation_herdr_and_legacy_keep_separate_context_policies() {
    let mut input = inputs("fresh");
    input.target.host_path = HostPath::Herdr {
        identity: "endpoint:snapshot-witness".into(),
    };
    input.context.source_policy = None;
    assert_eq!(complete(input).held, vec![source(1)]);
    let mut input = inputs("fresh");
    input.target.host_path = HostPath::Herdr {
        identity: "endpoint:snapshot-witness".into(),
    };
    unknown(input);
    let mut input = inputs("fresh");
    input.context.source_policy = None;
    unknown(input);
    let mut input = inputs("fresh");
    input.context.source_policy = Some("verified".into());
    assert_eq!(complete(input).held, vec![source(1)]);
}
