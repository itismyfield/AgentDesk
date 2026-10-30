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

/// Windows needs a drive prefix for these fixture roots to count as absolute.
fn abs(path: &str) -> String {
    if cfg!(windows) {
        format!("C:{path}")
    } else {
        path.into()
    }
}

fn source(number: u64) -> SourceId {
    SourceId {
        session_id: format!("session-{number}"),
        path: abs(&format!("/native/sessions/source-{number}.jsonl")).into(),
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
            owner_runtime_root: abs("/owner"),
            host_path: HostPath::Legacy,
        },
        context: BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: created(),
            execution_nonce: NONCE.into(),
            tmux_session: PANE.into(),
            channel_id: Some(42),
            owner_runtime_root: abs("/owner"),
            host: Some("mac-book".into()),
            expected_native_session_id: (mode == "resume").then(|| source(1).session_id),
            launch_mode: mode.into(),
            provider_root: Some(abs("/native").into()),
            first_prompt_digest: Some("first prompt".into()),
            source_policy: Some("shadow".into()),
        },
        current: source(1),
        events: vec![event(1, NONCE, None, BindingTarget::Source(source(1)))],
        settled_companions: Default::default(),
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
            "root" => input.context.provider_root = Some(abs("/other-root").into()),
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

#[cfg(unix)]
mod companions {
    use super::*;
    use crate::services::tui_prompt_dedupe::binding_events::{self as p5, codex};
    use serde_json::Value;

    const CHANNEL: u64 = 1509350778043895902;

    fn rows() -> Vec<Value> {
        include_str!("fixtures/codex_resume_companions.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    struct Fixture {
        _root: crate::config::TestRuntimeRootGuard,
        path: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = crate::config::TestRuntimeRootGuard::new();
            let runtime = crate::config::runtime_root().unwrap();
            p5::set_test_root(Some(&runtime));
            let path = runtime.join(format!("binding_events/{CHANNEL}.log"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            Self { _root: root, path }
        }

        fn write(&self, rows: &[Value]) -> Vec<u8> {
            let prefix = (1..117).map(|seq| {
                let mut row = rows[0].clone();
                row["seq"] = seq.into();
                row
            });
            let bytes = prefix
                .chain(rows.iter().cloned())
                .map(|row| format!("{}\n", serde_json::to_string(&row).unwrap()))
                .collect::<String>()
                .into_bytes();
            std::fs::write(&self.path, &bytes).unwrap();
            bytes
        }

        fn input(&self) -> Result<Inputs, String> {
            let log = codex::observe_channel(CHANNEL, 65_536)?;
            let last = log.events.last().unwrap();
            let BindingTarget::Source(current) = &last.new else {
                panic!("fixture ends in current source");
            };
            let mut input = inputs("fresh");
            input.target.channel = CHANNEL;
            input.target.tmux = last.tmux_session.clone();
            input.target.nonce = last.execution_nonce.clone().unwrap();
            input.context.channel_id = Some(CHANNEL);
            input.context.tmux_session = input.target.tmux.clone();
            input.context.execution_nonce = input.target.nonce.clone();
            input.context.created_at = last.committed_at;
            input.current = current.clone();
            input.panes[0].name = input.target.tmux.clone();
            input.events = log.events;
            input.settled_companions = log.settled_companions;
            input.tail = log.tail;
            input.log_digest = log.digest;
            Ok(input)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            p5::forget_channel_for_tests(CHANNEL);
            p5::set_test_root(None);
        }
    }

    #[test]
    fn codex_observation_settled_resume_companion_allows_history_and_retention() {
        let fixture = Fixture::new();
        let rows = rows();
        let before = fixture.write(&rows);
        let snapshot = complete(fixture.input().unwrap());
        assert_eq!(snapshot.held, [snapshot.current.clone()]);
        let old: SourceId = serde_json::from_value(rows[0]["new"]["source"].clone()).unwrap();
        let claim = crate::services::tui_o::writer::actor::codex_retention::for_hop(
            &snapshot,
            &crate::services::tui_o::store::spool::source_key(&old),
            &old,
            122,
            &snapshot.target.tmux,
        )
        .unwrap();
        assert_eq!((claim.binding_seq, claim.binder_seq), (118, 121));
        assert_eq!(std::fs::read(&fixture.path).unwrap(), before);
    }

    #[test]
    fn codex_observation_companion_requires_same_nonce_and_full_payload() {
        let fixture = Fixture::new();
        for mismatch in ["nonce", "session", "path"] {
            let mut rows = rows();
            for index in [2, 3] {
                let row = &mut rows[index];
                let kind = if index == 2 { "pending" } else { "rejected" };
                match mismatch {
                    "nonce" => {
                        row["execution_nonce"] = "4".repeat(32).into();
                        row["codex_ownership"]["context"]["execution_nonce"] =
                            row["execution_nonce"].clone();
                    }
                    "session" => {
                        let id = "01a1392e-0000-7000-8000-000000000009";
                        row["new"][kind]["payload_session_id"] = id.into();
                        row["codex_ownership"]["claim"]["session_id"] = id.into();
                    }
                    "path" => {
                        let path = "/native/sessions/other.jsonl";
                        row["new"][kind]["payload_transcript_path"] = path.into();
                        row["codex_ownership"]["claim"]["path"] = path.into();
                    }
                    _ => unreachable!(),
                }
            }
            fixture.write(&rows);
            unknown(fixture.input().unwrap());
        }
    }

    #[test]
    fn codex_observation_companion_without_exact_terminal_evidence_stays_unknown() {
        let fixture = Fixture::new();
        for terminal in [false, true] {
            let mut rows = rows();
            // Bare Rejected is audit-only, even when a matching Resolved follows it.
            for row in &mut rows {
                row.as_object_mut().unwrap().remove("codex_ownership");
            }
            if !terminal {
                rows[3]["new"] = rows[2]["new"].clone();
                rows[4]["new"] = rows[2]["new"].clone();
            }
            fixture.write(&rows);
            unknown(fixture.input().unwrap());
        }
        let mut rows = rows();
        rows[4].as_object_mut().unwrap().remove("codex_ownership");
        rows[4]["new"] = rows[3]["new"].clone();
        fixture.write(&rows);
        unknown(fixture.input().unwrap());
    }
}
