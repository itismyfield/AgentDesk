#![cfg(unix)]

use std::cell::RefCell;
use std::rc::Rc;

use super::fault::{FenceWriteFault, arm};
use super::*;
use crate::services::claude_tui::host_input::{SpyGuard, SpyState};
use crate::services::claude_tui::input::{
    send_followup_prompt_or_idle_transcript, send_fresh_prompt,
};
use crate::services::claude_tui::submission_fence;
use crate::services::provider::CancelToken;
use crate::services::turn_host::HerdrRefusal;

const RULE: &str =
    "────────────────────────────────────────────────────────────────────────────────";

fn ready_pane() -> String {
    format!(
        " ▐▛███▛█   Claude Code v2.1.289\n~/work\n\n{RULE}\n\u{276f} \n{RULE}\n  \
         ⏱ 0m │ ░░░░░░░░░░ │ 0% │ 0/1.0M │ $0.00\n  MCP: 2"
    )
}

fn managed_row(channel: u64) -> (InflightTurnState, ManagedSubmission) {
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        Some("zp1".into()),
        7,
        channel + 10,
        channel + 11,
        "managed prompt".into(),
        None,
        Some(format!("AgentDesk-claude-zp1-{channel}")),
        None,
        None,
        0,
    );
    state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    let submission =
        install_managed_submission_boundary(&mut state, &TurnHost::Tmux).expect("managed route");
    super::super::save_inflight_state_create_new(&state).expect("create row");
    (state, submission)
}

fn durable_phase(channel: u64) -> Option<SubmissionPhase> {
    super::super::load_inflight_state(&ProviderKind::Claude, channel)
        .expect("row survives")
        .managed_submission
        .map(|record| record.phase)
}

#[derive(Clone, Copy, Debug)]
enum Entry {
    Fresh,
    Warm,
}

fn submit(
    entry: Entry,
    channel: u64,
    prompt: &str,
    transcript: &std::path::Path,
) -> Result<(), String> {
    let session = format!("AgentDesk-claude-zp1-{channel}");
    let token = CancelToken::new();
    match entry {
        Entry::Fresh => send_fresh_prompt(&session, prompt, Some(&token)),
        Entry::Warm => {
            send_followup_prompt_or_idle_transcript(&session, prompt, Some(&token), transcript)
        }
    }
}

struct Run {
    result: Result<(), String>,
    /// Each pane write with the durable phase read just before it.
    sends: Vec<(String, Option<SubmissionPhase>)>,
}

/// Drives the real input entry through the host spy inside `submission`'s scope.
fn run(
    entry: Entry,
    channel: u64,
    prompt: &str,
    submission: &ManagedSubmission,
    fail_send: Option<usize>,
) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("session.jsonl");
    std::fs::write(&transcript, "{\"type\":\"result\"}\n").unwrap();
    let sends = Rc::new(RefCell::new(Vec::new()));
    let seen = sends.clone();
    let _spy = SpyGuard::install(SpyState {
        captures: std::iter::repeat_n(Some(ready_pane()), 24).collect(),
        fail_send: fail_send.map(|at| (at, Err("cut: provider process lost".to_string()))),
        on_send: Some(Box::new(move |call: &str| {
            seen.borrow_mut()
                .push((call.to_string(), durable_phase(channel)));
        })),
        ..SpyState::default()
    });
    let result = submission_fence::with_scope(Some(submission.fence()), || {
        submit(entry, channel, prompt, &transcript)
    });
    let sends = sends.borrow().clone();
    Run { result, sends }
}

fn is_payload_or_enter(call: &str) -> bool {
    ["literal:", "load:", "paste:", "keys:Enter"]
        .iter()
        .any(|prefix| call.starts_with(prefix))
}

/// The durable phase is MayHaveSubmitted when the first payload reaches the host, and every
/// cut after that leaves it there, for both entries and both payload shapes.
#[test]
fn the_first_host_payload_already_sees_may_have_submitted_at_every_cut() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let chunked = "x".repeat(2_000);
    // (label, prompt, index of Enter among the sends)
    let plans = [
        ("literal", "single line prompt".to_string(), 1),
        ("chunked literal", chunked, 2),
        ("paste", "first line\nsecond line".to_string(), 2),
    ];
    let mut channel = 6_801_000;
    for entry in [Entry::Fresh, Entry::Warm] {
        for (label, prompt, enter_at) in &plans {
            for cut in [Some(0), Some(1), Some(*enter_at), None] {
                channel += 1;
                let (_, submission) = managed_row(channel);
                assert_eq!(durable_phase(channel), Some(SubmissionPhase::Waiting));
                let run = run(entry, channel, prompt, &submission, cut);
                let case = format!("{entry:?} {label} cut={cut:?}");
                let first = run
                    .sends
                    .iter()
                    .find(|(call, _)| is_payload_or_enter(call))
                    .unwrap_or_else(|| panic!("{case}: no payload reached the host"));
                assert!(
                    first.0.starts_with("literal:") || first.0.starts_with("load:"),
                    "{case}: first host write {:?}",
                    first.0
                );
                assert_eq!(
                    first.1,
                    Some(SubmissionPhase::MayHaveSubmitted),
                    "{case}: the fence must be durable before the first payload"
                );
                assert_eq!(
                    cut.is_none(),
                    run.result.is_ok(),
                    "{case}: {:?}",
                    run.result
                );
                assert_eq!(
                    durable_phase(channel),
                    Some(SubmissionPhase::MayHaveSubmitted),
                    "{case}: a cut never lowers the phase back to Waiting"
                );
            }
        }
    }
}

/// A fence write or rename failure holds every payload and Enter, keeps the request in its
/// Waiting row, reports the typed refusal, and keeps later input of the same attempt shut.
#[test]
fn a_refused_fence_write_holds_every_payload_and_late_input() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let mut channel = 6_802_000;
    for entry in [Entry::Fresh, Entry::Warm] {
        for prompt in ["single line prompt", "first line\nsecond line"] {
            channel += 1;
            let (_, control) = managed_row(channel);
            let ran = run(entry, channel, prompt, &control, None);
            assert_eq!(ran.result, Ok(()), "{entry:?} control");
            assert!(ran.sends.iter().any(|(call, _)| is_payload_or_enter(call)));
            assert!(matches!(
                control.classify_failure(),
                SubmissionFailure::SubmissionIndeterminate
            ));

            for fault in [FenceWriteFault::Write, FenceWriteFault::Rename] {
                channel += 1;
                let case = format!("{entry:?} {prompt:?} {fault:?}");
                let (born, submission) = managed_row(channel);
                arm(channel, fault);
                let refused = run(entry, channel, prompt, &submission, None);
                let error = refused.result.expect_err(&case);
                assert!(error.contains("held before submit"), "{case}: {error}");
                assert!(
                    refused.sends.is_empty(),
                    "{case}: no load, paste, literal or Enter may reach the pane: {:?}",
                    refused.sends
                );
                assert!(matches!(
                    submission.classify_failure(),
                    SubmissionFailure::PreSubmitPersistenceRefused(_)
                ));
                let row = super::super::load_inflight_state(&ProviderKind::Claude, channel)
                    .expect("the request stays in its row");
                assert_eq!(
                    row.managed_submission.map(|r| r.phase),
                    Some(SubmissionPhase::Waiting)
                );
                assert_eq!(row.user_text, born.user_text);

                let late = run(entry, channel, prompt, &submission, None);
                assert!(
                    late.result.is_err(),
                    "{case}: late input of a refused attempt"
                );
                assert!(late.sends.is_empty(), "{case}: late sends {:?}", late.sends);
                assert_eq!(durable_phase(channel), Some(SubmissionPhase::Waiting));
            }
        }
    }
}

/// Every row write keeps the stronger record of the same episode; an unmarked row stays
/// LegacyUnknown and a successor episode inherits nothing.
#[test]
fn row_writes_never_lower_promote_or_transfer_the_submission_record() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = super::super::inflight_runtime_root().unwrap();
    let persist = |state: &InflightTurnState| {
        let path = inflight_state_path(&root, &ProviderKind::Claude, state.channel_id);
        let _lock = lock_inflight_state_path(&path).unwrap();
        super::super::store::persist_under_lock(&root, &path, state, "zp1_stale_writer").unwrap();
    };

    let (stale, submission) = managed_row(6_803_001);
    submission.fence().before_first_payload().unwrap();
    persist(&stale);
    assert_eq!(
        durable_phase(6_803_001),
        Some(SubmissionPhase::MayHaveSubmitted),
        "a stale Waiting snapshot must not lower the fence"
    );

    let mut legacy = InflightTurnState::new(
        ProviderKind::Claude,
        6_803_002,
        None,
        7,
        6_803_012,
        6_803_013,
        "legacy".into(),
        None,
        None,
        None,
        None,
        0,
    );
    super::super::save_inflight_state_create_new(&legacy).unwrap();
    legacy.managed_submission = Some(ManagedSubmissionRecord::at(SubmissionPhase::Waiting));
    persist(&legacy);
    assert_eq!(
        durable_phase(6_803_002),
        None,
        "an unmarked row stays LegacyUnknown"
    );

    let (_, _) = managed_row(6_803_003);
    let mut successor =
        super::super::load_inflight_state(&ProviderKind::Claude, 6_803_003).unwrap();
    successor.turn_nonce = Some("successor-nonce".into());
    successor.managed_submission = None;
    persist(&successor);
    assert_eq!(
        durable_phase(6_803_003),
        None,
        "a successor episode inherits nothing"
    );
}

/// A row without the field loads as LegacyUnknown, an unknown phase survives a rewrite, and
/// only the Claude TUI on tmux receives a boundary.
#[test]
fn legacy_and_unmanaged_routes_stay_unknown() {
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        6_804_001,
        None,
        7,
        6_804_011,
        6_804_012,
        "legacy".into(),
        None,
        Some("AgentDesk-claude-zp1".into()),
        None,
        None,
        0,
    );
    let mut json = serde_json::to_value(&state).unwrap();
    assert!(json.get("managed_submission").is_none());
    let loaded: InflightTurnState = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(loaded.managed_submission, None);

    json["managed_submission"] = serde_json::json!({"protocol": 1, "phase": "observed"});
    let future: InflightTurnState = serde_json::from_value(json).unwrap();
    let record = future.managed_submission.clone().unwrap();
    assert!(!record.is_waiting());
    assert_eq!(
        serde_json::to_value(&future).unwrap()["managed_submission"]["phase"],
        "observed"
    );
    let mut stale = future.clone();
    stale.managed_submission = Some(ManagedSubmissionRecord::at(SubmissionPhase::Waiting));
    carry_forward(Some(&future), &mut stale);
    assert_eq!(
        stale.managed_submission,
        Some(record),
        "never lowered below a newer phase"
    );

    let refused = TurnHost::Refused(HerdrRefusal::ProviderUnsupported {
        provider: "claude".into(),
    });
    for (provider, runtime, host) in [
        (
            ProviderKind::Claude,
            Some(RuntimeHandoffKind::LegacyTmuxWrapper),
            &TurnHost::Tmux,
        ),
        (ProviderKind::Claude, None, &TurnHost::Tmux),
        (
            ProviderKind::Codex,
            Some(RuntimeHandoffKind::CodexTui),
            &TurnHost::Tmux,
        ),
        (
            ProviderKind::Claude,
            Some(RuntimeHandoffKind::ClaudeTui),
            &refused,
        ),
    ] {
        state.provider = provider.as_str().to_string();
        state.runtime_kind = runtime;
        assert!(install_managed_submission_boundary(&mut state, host).is_none());
        assert_eq!(state.managed_submission, None, "{provider:?} {runtime:?}");
        assert!(state.managed_submission_capability().is_none());
    }
}

/// A composer that stops being ready between the readiness wait and the mutation fails before
/// the fence, so the row stays Waiting: Waiting keeps meaning "no payload was allowed".
#[test]
fn a_composer_change_after_readiness_fails_before_the_fence() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let draft = format!(
        "\n\n{RULE}\n\u{276f} [User: ann (ID: 7)] 남은 초안 한글\n{RULE}\n  ⏵⏵ bypass permissions on"
    );
    for (index, entry) in [Entry::Fresh, Entry::Warm].into_iter().enumerate() {
        let channel = 6_805_001 + index as u64;
        let (_, submission) = managed_row(channel);
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("session.jsonl");
        std::fs::write(&transcript, "{\"type\":\"result\"}\n").unwrap();
        let spy = SpyGuard::install(SpyState {
            captures: [Some(ready_pane()), Some(ready_pane())]
                .into_iter()
                .chain(std::iter::repeat_n(Some(draft.clone()), 8))
                .collect(),
            ..SpyState::default()
        });
        let result = submission_fence::with_scope(Some(submission.fence()), || {
            submit(entry, channel, "hello", &transcript)
        });
        assert!(result.is_err(), "{entry:?}: {result:?}");
        assert!(
            !spy.calls().iter().any(|call| is_payload_or_enter(call)),
            "{entry:?}: {:?}",
            spy.calls()
        );
        assert_eq!(
            durable_phase(channel),
            Some(SubmissionPhase::Waiting),
            "{entry:?}"
        );
        assert!(matches!(
            submission.classify_failure(),
            SubmissionFailure::ProviderExecutionFailed
        ));
    }
}
