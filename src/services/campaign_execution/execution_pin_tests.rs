use super::*;

fn at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-29T10:00:00Z")
        .unwrap()
        .to_utc()
}

fn observation(body: &str, updated: Option<DateTime<Utc>>) -> IssueObservation {
    IssueObservation {
        repository: Repository::Adk,
        repository_identity: "itismyfield/AgentDesk".into(),
        issue_number: 7438,
        issue_url: "https://github.com/itismyfield/AgentDesk/issues/7438".into(),
        observed_at: at(),
        issue_updated_at: updated,
        body: body.into(),
        acceptance: vec![AcceptanceLocation {
            acceptance_id: "ac-1".into(),
            line_start: 2,
            line_end: 2,
            excerpt: "AC line".into(),
        }],
    }
}

fn hashes() -> DefinitionHashes {
    DefinitionHashes {
        requirements: "req".into(),
        acceptance: "ac".into(),
        profile: "profile".into(),
    }
}

#[test]
fn snapshot_needs_an_update_time_and_in_body_acceptance_locations() {
    assert_eq!(
        RequirementsSnapshot::observe(observation("title\nAC line\n", None)).unwrap_err(),
        PinError::IncompleteObservation
    );
    assert!(matches!(
        RequirementsSnapshot::observe(observation("one line only", Some(at()))),
        Err(PinError::LocationOutOfBody(id)) if id == "ac-1"
    ));
    let snapshot =
        RequirementsSnapshot::observe(observation("title\nAC line\n", Some(at()))).unwrap();
    assert_eq!(
        snapshot.body_digest,
        blake3::hash(b"title\nAC line\n").to_hex().to_string()
    );
    let same = RequirementsSnapshot::observe(observation("title\nAC line\n", Some(at()))).unwrap();
    assert_eq!(snapshot.hash().unwrap(), same.hash().unwrap());
}

#[test]
fn resume_revalidates_the_stored_pin_and_reports_what_drifted() {
    let snapshot =
        RequirementsSnapshot::observe(observation("title\nAC line\n", Some(at()))).unwrap();
    let pin = ExecutionPin::pin(
        hashes(),
        &snapshot,
        "profile-hash".into(),
        "spec-hash".into(),
    )
    .unwrap();
    assert_eq!(pin.workflow_version, WORKFLOW_VERSION);
    pin.revalidate(&hashes(), &snapshot, "profile-hash", "spec-hash")
        .unwrap();
    let edited =
        RequirementsSnapshot::observe(observation("title\nAC line changed\n", Some(at()))).unwrap();
    assert_eq!(
        pin.revalidate(&hashes(), &edited, "profile-hash", "spec-hash"),
        Err(PinError::Drift("requirements".into()))
    );
    let later = RequirementsSnapshot::observe(observation(
        "title\nAC line\n",
        Some(at() + chrono::Duration::minutes(1)),
    ))
    .unwrap();
    assert_eq!(
        pin.revalidate(&hashes(), &later, "profile-hash", "spec-hash"),
        Err(PinError::Drift("requirements".into()))
    );
    assert_eq!(
        pin.revalidate(&hashes(), &snapshot, "other-profile", "spec-hash"),
        Err(PinError::Drift("profile".into()))
    );
    assert_eq!(
        pin.revalidate(&hashes(), &snapshot, "profile-hash", "other-spec"),
        Err(PinError::Drift("spec".into()))
    );
    let mut other = hashes();
    other.acceptance = "ac2".into();
    assert_eq!(
        pin.revalidate(&other, &snapshot, "profile-hash", "spec-hash"),
        Err(PinError::Drift("definition".into()))
    );
    let restored: ExecutionPin =
        serde_json::from_slice(&serde_json::to_vec(&pin).unwrap()).unwrap();
    assert_eq!(restored.hash().unwrap(), pin.hash().unwrap());
}
