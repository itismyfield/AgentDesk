use super::*;

fn prepared() -> Landing {
    Landing {
        target: LandingTarget {
            repository: "owner/project".into(),
            head_sha: "pinned".into(),
            head_ref: "campaign/fixed".into(),
            base_ref: "main".into(),
            pin_hash: "pin".into(),
        },
        epoch: 4,
        control_version: 2,
        action_id: "effect".into(),
        deadline: Utc::now(),
        host: HostGate {
            host: "host".into(),
            gate: "gate".into(),
            incarnation: "boot".into(),
        },
        preflight: Some(LandingPreflight {
            head_sha: "pinned".into(),
            mode: PreflightMode::PreTlc,
            transcript_digest: "digest".into(),
        }),
        status: LandingStatus::Preparing,
    }
}

#[test]
fn m6_current_ci_hold_and_unknown_observation_prevent_push() {
    let job = prepared();
    for ci in [RemoteCi::Active, RemoteCi::Unknown] {
        let remote = RemoteLanding {
            branch_head: None,
            pull_request: None,
            ci,
        };
        assert_eq!(job.publication(&remote), Publication::Wait);
    }
}

#[test]
fn m6_publication_binds_preflight_and_remote_ref() {
    let mut job = prepared();
    let mut remote = RemoteLanding {
        branch_head: None,
        pull_request: None,
        ci: RemoteCi::Idle,
    };
    assert_eq!(job.publication(&remote), Publication::PushThenOpen);
    job.preflight.as_mut().unwrap().head_sha = "old".into();
    assert_eq!(job.publication(&remote), Publication::Refused);
    job.preflight.as_mut().unwrap().head_sha = "pinned".into();
    remote.branch_head = Some("old".into());
    assert_eq!(job.publication(&remote), Publication::Refused);
    remote.branch_head = Some("pinned".into());
    assert_eq!(job.publication(&remote), Publication::Open);
}

#[test]
fn m6_publication_refuses_an_existing_pr() {
    let job = prepared();
    let mut pr = OpenedPr {
        number: 7,
        repository: job.target.repository.clone(),
        head_sha: job.target.head_sha.clone(),
        head_ref: job.target.head_ref.clone(),
        base_ref: job.target.base_ref.clone(),
    };
    assert!(job.matches_pr(&pr));
    pr.head_sha = "other".into();
    assert!(!job.matches_pr(&pr));
    pr.head_sha = job.target.head_sha.clone();
    let remote = RemoteLanding {
        branch_head: Some(job.target.head_sha.clone()),
        pull_request: Some(pr),
        ci: RemoteCi::Idle,
    };
    assert_eq!(job.publication(&remote), Publication::Refused);
}
