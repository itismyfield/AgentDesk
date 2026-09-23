use super::*;
use serde_json::json;

pub(crate) fn ready_node(id: &str, os: &str) -> Value {
    let now = chrono::Utc::now().timestamp_millis();
    json!({"instance_id":id,"status":"online","labels":[],"capabilities":{
        "intake_worker":{"enabled":true,"providers":["claude"],"features":["execution_requirements_v1","preserve_on_cancel_v1"]},
        "intake_poller":{"claude":now},
        "execution_readiness":{
            "schema":1,"boot_id":"fixture","observed_at_ms":now,"expires_at_ms":now+120_000,
            "os":os,"arch":"x86_64","runtime_profile":"worker","release":{},
            "providers":{"claude":{"cli_usable":true,"version":"1.0","failure":null,
                "credential_profiles":{"default":true},"authentication_verified":false,"quota_verified":false}},
            "tools":{"git":true},"repositories":{"example-org/example-repo":true},
            "backends":["process"],"disk_free_bytes":null,
        }
    }})
}

#[test]
fn execution_requirements_are_strict_and_share_dispatch_matching() {
    let policy = json!({"os":["windows"],"arch":["x86_64"],"nodes":["win"],
        "tools":["git"],"repositories":["example-org/example-repo"],"backends":["process"]});
    let requirements = ExecutionRequirements::parse(policy.clone()).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    assert!(
        requirements
            .explain(&ready_node("win", "windows"), now)
            .is_empty()
    );
    for (node, reason) in [
        (ready_node("win", "macos"), "required_os_mismatch"),
        (ready_node("other", "windows"), "required_node_mismatch"),
        (
            json!({"instance_id":"win","status":"online","labels":["windows"]}),
            "execution_evidence_missing",
        ),
    ] {
        assert!(requirements.explain(&node, now).iter().any(|r| r == reason));
        assert!(
            !super::super::capability_routing::explain_capability_match(
                &node,
                &json!({"execution":policy})
            )
            .eligible
        );
    }
    let mut node = ready_node("win", "windows");
    node["capabilities"]["execution_readiness"]["repositories"]["example-org/example-repo"] =
        json!(false);
    assert!(
        requirements
            .explain(&node, now)
            .iter()
            .any(|r| r.starts_with("required_repository_unavailable"))
    );
    node["capabilities"]["execution_readiness"]["expires_at_ms"] = json!(0);
    assert!(
        requirements
            .explain(&node, now)
            .contains(&"execution_evidence_stale".into())
    );
    for value in [
        json!({"os":["Windowz"]}),
        json!({"os":"windows"}),
        json!({"oops":true}),
        json!({"tools":["../../sh"]}),
        json!({"repositories":["C:\\repo"]}),
    ] {
        assert!(ExecutionRequirements::parse(value).is_err());
    }
    assert!(
        ExecutionRequirements::default()
            .explain(&json!({}), now)
            .is_empty()
    );
}
