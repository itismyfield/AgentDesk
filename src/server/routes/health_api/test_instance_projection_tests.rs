use super::*;
use serde_json::json;

fn witness() -> serde_json::Value {
    let identities: Vec<_> = [
        "claude_bot_token",
        "codex_bot_token",
        "announce_bot_token",
        "notify_bot_token",
    ]
    .into_iter()
    .enumerate()
    .map(|(index, name)| {
        json!({
            "name":name,"application_id":format!("{}",100+index),"device":1,"inode":20+index,
            "size":30,"mtime_ns":40,"ctime_ns":50,"token":"synthetic-secret"
        })
    })
    .collect();
    json!({"schema":1,"instance_id":"test-instance","head":"a".repeat(40),"pid":42,
        "binary_sha256":"b".repeat(64),"nonce":"test-nonce","port":18791,
        "identities":identities,"live_ready":false,"token":"synthetic-secret","root":"/private/fixture"})
}

#[test]
fn test_instance_projection_never_trusts_an_unadmitted_input_block() {
    let body = public_health_json(json!({
        "status":"healthy", "test_instance":witness(),
        "opencode":{"warm_server_count":1,"warm_servers":[{"pid":42,"port":1234}]}
    }));
    assert!(body.get("test_instance").is_none());
    assert_eq!(body["opencode"]["warm_server_count"], 1);
    assert!(body["opencode"].get("warm_servers").is_none());
    assert!(!body.to_string().contains("synthetic-secret"));
}

#[test]
fn test_instance_projection_allows_only_the_nine_and_seven_public_fields() {
    let original = witness();
    let projected = project_test_instance(&json!({"test_instance":original}), true).unwrap();
    assert_eq!(projected.as_object().unwrap().len(), 9);
    assert_eq!(projected["live_ready"], false);
    for identity in projected["identities"].as_array().unwrap() {
        assert_eq!(identity.as_object().unwrap().len(), 7);
    }
    assert!(projected.get("root").is_none());
    assert!(!projected.to_string().contains("synthetic-secret"));
    let mut invalid = witness();
    invalid["live_ready"] = json!(true);
    assert!(project_test_instance(&json!({"test_instance":invalid}), true).is_none());
}
