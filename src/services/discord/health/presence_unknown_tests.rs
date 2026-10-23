use super::*;

fn snapshot(channel_id: u64, agent_id: Option<&str>, elapsed_secs: u64) -> UnknownSnapshot {
    UnknownSnapshot {
        channel_id,
        provider: "claude".to_string(),
        agent_id: agent_id.map(str::to_string),
        reason: "session_identity_unresolved".to_string(),
        hold_leaf: Some("hosted_active_or_unknown".to_string()),
        unknown_since: "2026-10-20T00:00:00Z".parse().unwrap(),
        elapsed_secs,
        observed_reason: "catching_up".to_string(),
        observation_progress: "rebuilding".to_string(),
        notice_state: "sent".to_string(),
        notice_store_error: Some("private receipt path".to_string()),
    }
}

#[test]
fn groups_distinct_channels_and_projects_the_same_oldest_age() {
    let mut status = HealthStatus::Healthy;
    let mut reasons = vec![json!("preexisting")];
    let mut body = json!({"fully_recovered": true});
    let mut other = snapshot(103, Some("role-b"), 67);
    other.provider = "codex".to_string();
    other.hold_leaf = None;
    project(
        vec![
            snapshot(101, Some("role-a"), 61),
            snapshot(102, Some("role-a"), 89),
            snapshot(101, Some("role-a"), 99),
            other,
        ],
        &mut status,
        &mut reasons,
        &mut body,
        true,
    );
    assert_eq!(status, HealthStatus::Degraded);
    assert_eq!(body["fully_recovered"], true);
    assert_eq!(
        reasons,
        vec![
            json!("preexisting"),
            json!(
                "turn_presence_unknown:claude:role-a:session_identity_unresolved:hosted_active_or_unknown:2:99"
            ),
            json!("turn_presence_unknown:codex:role-b:session_identity_unresolved:none:1:67"),
        ]
    );
    let rows = body["turn_presence_unknown"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["channel_id"], 101);
    assert_eq!(rows[0]["elapsed_secs"], 99);
    assert_eq!(rows[1]["channel_id"], 102);
    assert_eq!(rows[1]["elapsed_secs"], 89);
    assert_eq!(rows[2]["provider"], "codex");
    assert_eq!(rows[2]["hold_leaf"], Value::Null);
    assert_eq!(rows[0]["observed_reason"], "catching_up");
    assert_eq!(rows[0]["observation_progress"], "rebuilding");
    assert_eq!(rows[0]["notice_state"], "sent");
    assert_eq!(rows[0]["notice_store_error"], "private receipt path");
    assert_eq!(rows[0]["unknown_since"], "2026-10-20T00:00:00Z");
}

#[test]
fn agent_components_escape_utf8_and_preserve_unknown_identity() {
    assert_eq!(agent_component(None), "~");
    assert_eq!(agent_component(Some("~")), "%7E");
    assert_eq!(agent_component(Some("Role_01.x-y")), "Role_01.x-y");
    assert_eq!(
        agent_component(Some("a:b,c\n한/%")),
        "a%3Ab%2Cc%0A%ED%95%9C%2F%25"
    );
    let mut status = HealthStatus::Unhealthy;
    let mut reasons = Vec::new();
    let mut body = json!({});
    project(
        vec![snapshot(104, None, 61), snapshot(105, Some("~"), 62)],
        &mut status,
        &mut reasons,
        &mut body,
        true,
    );
    assert_eq!(status, HealthStatus::Unhealthy);
    assert_eq!(body["turn_presence_unknown"][0]["agent_id"], "~");
    assert_eq!(body["turn_presence_unknown"][1]["agent_id"], Value::Null);
    assert!(
        reasons
            .iter()
            .any(|reason| reason.as_str().unwrap().contains(":%7E:"))
    );
    assert!(
        reasons
            .iter()
            .any(|reason| reason.as_str().unwrap().contains(":~:"))
    );
}

#[test]
fn public_projection_adds_only_the_group_and_recovery_keeps_other_causes() {
    let mut status = HealthStatus::Healthy;
    let mut reasons = Vec::new();
    let mut body = json!({"fully_recovered": true});
    project(
        vec![snapshot(106, Some("role"), 3601)],
        &mut status,
        &mut reasons,
        &mut body,
        false,
    );
    assert_eq!(status, HealthStatus::Degraded);
    assert_eq!(reasons.len(), 1);
    assert!(body.get("turn_presence_unknown").is_none());
    let mut status = HealthStatus::Unhealthy;
    let mut reasons = vec![json!("db_unavailable")];
    project(Vec::new(), &mut status, &mut reasons, &mut body, true);
    assert_eq!(status, HealthStatus::Unhealthy);
    assert_eq!(reasons, vec![json!("db_unavailable")]);
    assert_eq!(body["turn_presence_unknown"], json!([]));
    assert_eq!(body["fully_recovered"], true);
}
