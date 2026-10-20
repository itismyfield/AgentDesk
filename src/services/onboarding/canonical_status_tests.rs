use super::*;

#[test]
fn new_agents_start_with_core_canonical_idle_status() {
    let migration = include_str!("../../../migrations/postgres/0089_drop_agent_archive.sql");

    assert_eq!(ONBOARDING_NEW_AGENT_STATUS, "idle");
    assert!(
        migration.contains("status IN ('idle', 'working')"),
        "onboarding must use a status allowed by agents_status_known_check"
    );
}
