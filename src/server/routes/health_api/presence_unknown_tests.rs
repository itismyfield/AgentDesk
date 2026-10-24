use std::{path::Path, process::Command, sync::Arc};

use serde_json::{Value, json};

use super::{public_health_json, tests::EnvVarGuard, tests::health_body};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::health::{
    HealthRegistry, seed_long_unknown_for_tests, seed_presence_for_tests,
};
use crate::services::tui_o::cutover::test_override;
use crate::services::tui_o::turn_mode::TestConfirmation;

fn unknown_reasons(body: &Value) -> Vec<&str> {
    body["degraded_reasons"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .filter(|reason| reason.starts_with("turn_presence_unknown:"))
        .collect()
}

#[test]
fn health_unknown_reasons_follow_publication_and_recovery() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::health_unknown_reasons_follow_publication_and_recovery"
    )) {
        return;
    }
    const CHANNEL: u64 = 71_870_101;
    let root = tempfile::tempdir().unwrap();
    let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let _channels = test_override::force_channels(&[(CHANNEL, RuntimeHandoffKind::ClaudeTui)]);
    let _confirmed = TestConfirmation::new(CHANNEL);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let registry = Arc::new(HealthRegistry::new());
        seed_long_unknown_for_tests(CHANNEL, "binding_unreadable", 59);
        let before = health_body("/health/detail", Some(registry.clone())).await;
        assert!(unknown_reasons(&before).is_empty());
        seed_long_unknown_for_tests(CHANNEL, "binding_unreadable", 61);
        let detail = health_body("/health/detail", Some(registry.clone())).await;
        let expected = "turn_presence_unknown:claude:~:binding_unreadable:none:1:61";
        assert_eq!(unknown_reasons(&detail), vec![expected]);
        assert_eq!(detail["turn_presence_unknown"][0]["channel_id"], CHANNEL);
        assert_eq!(detail["turn_presence_unknown"][0]["elapsed_secs"], 61);
        assert_eq!(
            detail["turn_presence_unknown"][0]["reason"],
            "binding_unreadable"
        );
        let public = health_body("/health", Some(registry.clone())).await;
        assert_eq!(unknown_reasons(&public), vec![expected]);
        assert!(public.get("turn_presence_unknown").is_none());
        assert_eq!(public["status"], "unhealthy");
        assert_eq!(public["fully_recovered"], false);
        assert!(
            public["degraded_reasons"]
                .as_array()
                .unwrap()
                .contains(&json!("db_unavailable"))
        );
        seed_presence_for_tests(CHANNEL, true);
        let recovered = health_body("/health/detail", Some(registry)).await;
        assert!(unknown_reasons(&recovered).is_empty());
        assert_eq!(recovered["turn_presence_unknown"], json!([]));
        assert!(
            recovered["degraded_reasons"]
                .as_array()
                .unwrap()
                .contains(&json!("db_unavailable"))
        );
    });
}

#[test]
fn public_unknown_diagnostics_keep_groups_and_drop_private_rows() {
    let reason =
        "turn_presence_unknown:claude:role%3Aid%2C%0A%ED%95%9C:binding_unreadable:none:2:3661";
    let public = public_health_json(json!({
        "status": "degraded", "db": true, "fully_recovered": true,
        "degraded_reasons": [reason],
        "turn_presence_unknown": [{
            "channel_id": 71870102, "notice_store_error": "/private/receipt",
            "observed_reason": "catching_up", "observation_progress": "rebuilding"
        }]
    }));
    assert_eq!(public["status"], "degraded");
    assert_eq!(public["ok"], false);
    assert_eq!(public["degraded"], true);
    assert_eq!(public["fully_recovered"], true);
    assert_eq!(public["degraded_reasons"], json!([reason]));
    assert!(public.get("turn_presence_unknown").is_none());
    assert!(!public.to_string().contains("71870102"));
    assert!(!public.to_string().contains("/private/receipt"));
}

fn deploy_ready(body: &Value, jq: bool) -> bool {
    let script = r#"
        . "$DEFAULTS" || exit 90
        if [ "$USE_JQ" = 0 ]; then
            _health_json_has_jq() { return 1; }
        else
            command -v jq >/dev/null 2>&1 || exit 91
        fi
        release=0; health_json_is_ready "$BODY" 1 1 1 1 >/dev/null 2>&1 || release=1
        plain=0; health_json_is_ready "$BODY" 0 1 >/dev/null 2>&1 || plain=1
        [ "$release" = "$plain" ] || exit 92
        exit "$release"
    "#;
    let status = Command::new("bash")
        .args(["-c", script])
        .env(
            "DEFAULTS",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/_defaults.sh"),
        )
        .env("USE_JQ", if jq { "1" } else { "0" })
        .env("BODY", body.to_string())
        .status()
        .unwrap();
    match status.code() {
        Some(0) => true,
        Some(1) => false,
        other => panic!("deploy readiness did not run (exit={other:?}, jq={jq})"),
    }
}

#[test]
fn deploy_unknown_group_is_nonblocking_without_waiving_other_causes() {
    let valid =
        "turn_presence_unknown:claude:role%3Aid%2C%0A%ED%95%9C:binding_unreadable:none:2:3661";
    let body = |reasons: Vec<&str>| {
        json!({
            "status": "degraded", "db": true, "server_up": true, "dashboard": true,
            "fully_recovered": true, "degraded_reasons": reasons,
        })
    };
    for jq in [true, false] {
        assert!(deploy_ready(&body(vec![valid]), jq));
        assert!(deploy_ready(
            &body(vec![valid, "relay_verdict_unknown_codex_88"]),
            jq
        ));
        for blocker in [
            "disk_low_free_bytes:1",
            "db_unavailable",
            "future_unknown_cause",
        ] {
            assert!(!deploy_ready(&body(vec![valid, blocker]), jq), "{blocker}");
        }
        for malformed in [
            "turn_presence_unknown:other:~:binding_unreadable:none:1:61",
            "turn_presence_unknown:claude:role%3a:binding_unreadable:none:1:61",
            "turn_presence_unknown:claude:role%ZZ:binding_unreadable:none:1:61",
            "turn_presence_unknown:claude:a,b:binding_unreadable:none:1:61",
            "turn_presence_unknown:claude:~:binding_unreadable:none:0:61",
            "turn_presence_unknown:claude:~:binding_unreadable:none:1:old",
            "turn_presence_unknown:claude:~:binding_unreadable:none:1:61:extra",
        ] {
            assert!(!deploy_ready(&body(vec![malformed]), jq), "{malformed}");
        }
        let mut unhealthy = body(vec![valid]);
        unhealthy["status"] = json!("unhealthy");
        assert!(!deploy_ready(&unhealthy, jq));
    }
}
