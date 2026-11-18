use super::*;
use crate::services::{
    agent_protocol::RuntimeHandoffKind::ClaudeTui,
    tui_o::{
        channel_policy::BootChannels,
        shadow::{ShadowProvider, SourceId},
        writer::binding::{BindingCause, BindingEvidence, BindingRecord, BindingTarget},
    },
};

fn fixture() -> (BootChannels, Candidate, LegacyRun) {
    let config=serde_json::from_value(serde_json::json!({"server":{},"agents":[{"id":"fixture","name":"fixture","channels":{"claude":{"id":"41","runtime":"tui"}}}],"tui_o":{"writer":{"channels":[41]}}})).unwrap();
    let boot = BootChannels::validate(&config)
        .unwrap()
        .adopted(Adoption::Committed);
    let thread = serde_json::from_value(
        serde_json::json!({"id":"42","parent_id":"41","type":11,"name":"fixture"}),
    )
    .unwrap();
    assert!(
        boot.resolve_child(
            42,
            crate::services::session_host::surface_policy::VerifiedSurface::from_discord(
                &thread, ClaudeTui
            )
            .unwrap(),
            false
        )
    );
    let candidate = boot.output_candidate(42).unwrap();
    let binding = BindingEvent {
        seq: 2,
        channel_id: 42,
        provider: ShadowProvider::Claude,
        tmux_session: "fixture".into(),
        execution_nonce: "fixture-execution".into(),
        committed_at: chrono::Utc::now(),
        record: BindingRecord::Bound {
            old: None,
            new: BindingTarget::Source(SourceId {
                session_id: "fixture-source".into(),
                path: "fixture.jsonl".into(),
                dev: 1,
                ino: 1,
            }),
            cause: BindingCause::Startup,
            parent_hint: None,
            evidence: BindingEvidence {
                hook_event: "SessionStart".into(),
                received_at: chrono::Utc::now(),
                reclaims: false,
            },
        },
    };
    (
        boot,
        candidate,
        LegacyRun {
            binding,
            turn: Some((
                1,
                "start".into(),
                Some("fixture".into()),
                Some(0),
                Some("turn".into()),
            )),
            observed_end: 0,
            delivery: None,
        },
    )
}
#[test]
fn child_run_owner_and_boot_exclusion_require_a_verified_bound_run() {
    let (boot, candidate, run) = fixture();
    let mut wrong = run.clone();
    wrong.binding.channel_id = 43;
    assert!(!candidate.release_child_run(42, wrong));
    let mut foreign = run.clone();
    foreign.turn.as_mut().unwrap().2 = Some("foreign-execution".into());
    assert!(!candidate.release_child_run(42, foreign));
    assert!(candidate.release_child_run(42, run));
    assert_eq!(boot.child(42).unwrap().unwrap().body_owner(), Some(false));
    assert!(boot.output_candidate(42).is_none());
    let member = Candidate::reserved_late();
    member.release(41);
    assert!(!member.child_parked());
}
#[test]
fn child_run_boundary_preserves_send_fence_and_exact_authority() {
    let (_, candidate, run) = fixture();
    candidate.fix_late_boundary(|| LateBoundary {
        baseline: 0,
        takeover_after: 3,
        sends: (0, 0),
    });
    assert!(candidate.release_child_run(42, run.clone()));
    let mut wrong = run.clone();
    wrong.binding.execution_nonce = "other-execution".into();
    assert!(!candidate.retry_child_at_boundary(&wrong, 4));
    let sending = candidate.claim_body(42);
    assert!(!candidate.retry_child_at_boundary(&run, 4));
    drop(sending);
    assert!(candidate.retry_child_at_boundary(&run, 4));
    assert_eq!(candidate.peek(), Adoption::Deferred);
    assert!(candidate.legacy_run().is_none());
    assert_eq!(
        candidate.late_boundary(),
        Some(LateBoundary {
            baseline: 4,
            takeover_after: 4,
            sends: (1, 1)
        })
    );
}
#[test]
fn child_run_old_host_cleanup_cannot_erase_a_reopened_boundary() {
    let (_, candidate, run) = fixture();
    let guard = candidate.late_cleanup_guard(42, None).unwrap();
    assert!(candidate.release_child_run(42, run.clone()));
    assert!(candidate.retry_child_at_boundary(&run, 4));
    drop(guard);
    assert_eq!(candidate.peek(), Adoption::Deferred);
}
#[test]
fn child_run_arbitrary_refusal_does_not_grant_legacy_body_ownership() {
    let (boot, candidate, _) = fixture();
    candidate.release(42);
    assert_eq!(boot.child(42).unwrap().unwrap().body_owner(), None);
    assert!(boot.output_candidate(42).is_some());
}

#[test]
fn ch3fix_owner_snapshot_remains_legacy_across_the_boundary() {
    use std::sync::{Arc, Barrier};
    let (boot, candidate, run) = fixture();
    assert!(candidate.release_child_run(42, run.clone()));
    let child = boot.child(42).unwrap().unwrap();
    let (read, changed) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
    let (on_read, on_changed) = (read.clone(), changed.clone());
    crate::services::tui_o::channel_policy::children::OWNER_STATE_READ.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            on_read.wait();
            on_changed.wait();
        }));
    });
    let next = candidate.clone();
    let worker = std::thread::spawn(move || {
        read.wait();
        let locked = next.locked_for_test();
        if !locked {
            assert!(next.retry_child_at_boundary(&run, 4));
        }
        changed.wait();
        if locked {
            assert!(next.retry_child_at_boundary(&run, 4));
        }
    });
    let owner = child.body_owner();
    worker.join().unwrap();
    assert_eq!(owner, Some(false));
    assert_eq!(child.body_owner(), Some(false));
    *candidate.lock() = Adoption::Committed;
    assert_eq!(child.body_owner(), Some(true));
}
