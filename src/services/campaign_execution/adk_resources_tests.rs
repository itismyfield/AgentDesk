use super::*;

fn at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-29T09:30:00Z")
        .unwrap()
        .to_utc()
}

#[test]
fn build_token_waits_are_resource_outcomes_not_quality_failures() {
    assert_eq!(
        classify_build_token_exit(Some(75)),
        Some(BuildTokenOutcome::Timeout)
    );
    assert_eq!(
        classify_build_token_exit(Some(69)),
        Some(BuildTokenOutcome::Unusable)
    );
    assert_eq!(
        classify_build_token_exit(Some(73)),
        Some(BuildTokenOutcome::Nested)
    );
    assert_eq!(
        classify_build_token_exit(Some(64)),
        Some(BuildTokenOutcome::Usage)
    );
    assert_eq!(
        classify_build_token_exit(Some(0)),
        Some(BuildTokenOutcome::Held)
    );
    assert_eq!(
        classify_build_token_exit(Some(101)),
        Some(BuildTokenOutcome::Held)
    );
    assert_eq!(classify_build_token_exit(None), None);
}

#[test]
fn host_tool_target_token_and_pg_observations_never_report_absence_as_observed() {
    let temp = tempfile::tempdir().unwrap();
    let sh = observe_tool("host", "sh", at());
    assert!(sh.is_available());
    assert_eq!(sh.resource.kind, ResourceKind::Host);
    let missing = observe_tool("host", "adk-tool-that-does-not-exist-7438", at());
    assert!(!missing.is_available());
    assert!(matches!(missing.observation, Observation::NotYet { .. }));
    let target = observe_target_dir("host", &temp.path().join("target"), at());
    assert!(target.is_available() && temp.path().join("target").is_dir());
    assert!(
        !temp
            .path()
            .join("target")
            .join(".adk-campaign-probe")
            .exists()
    );
    let file = temp.path().join("file");
    std::fs::write(&file, b"x").unwrap();
    let not_dir = observe_target_dir("host", &file.join("target"), at());
    assert!(matches!(not_dir.observation, Observation::NotYet { .. }));
    let token = observe_build_token("host", &temp.path().join("absent.lock"), at());
    assert!(matches!(token.observation, Observation::NotYet { .. }));
    let present = observe_build_token("host", &file, at());
    assert!(matches!(present.observation, Observation::Unknown { .. }) && !present.is_available());
    assert!(matches!(
        observe_pg("host", None, at()).observation,
        Observation::NotYet { .. }
    ));
    assert!(matches!(
        observe_pg("host", Some("mysql://x"), at()).observation,
        Observation::NotYet { .. }
    ));
    let pg = observe_pg("host", Some("postgres://localhost:5/throwaway"), at());
    assert!(matches!(pg.observation, Observation::Unknown { .. }) && !pg.is_available());
}
