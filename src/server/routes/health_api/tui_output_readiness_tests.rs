//! The public health body built from a real snapshot drives the deploy readiness shell predicate.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use serde_json::{Value, json};

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::health::{HealthRegistry, build_public_health_snapshot};
use crate::services::tui_o::alarm::{self, AlarmRouter};
use crate::services::tui_o::cutover::test_override;
use crate::services::tui_o::writer::WriterAlarm;

const WRITER_CHANNEL: u64 = 61_325_001;
const CHANNELS: &[(u64, RuntimeHandoffKind)] = &[(WRITER_CHANNEL, RuntimeHandoffKind::ClaudeTui)];

async fn public_body(standby: bool, channels: Option<&[(u64, RuntimeHandoffKind)]>) -> Value {
    let registry = HealthRegistry::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    if standby {
        registry.register_standby("claude".into(), shared).await;
    } else {
        registry.register_worker("claude".into(), shared).await;
    }
    let _o = channels.map(test_override::force_channels);
    let mut json = serde_json::to_value(build_public_health_snapshot(&registry).await).unwrap();
    // The database, dashboard and standby axes `health_response` sets around the snapshot.
    for axis in ["db", "server_up", "dashboard"] {
        json[axis] = true.into();
    }
    if registry.all_providers_are_standby().await {
        json["cluster_standby"] = true.into();
    }
    super::super::public_health_json(json)
}

/// Every executable on PATH except jq, so the predicate takes its jq-less path.
fn path_without_jq(farm: &Path) -> std::ffi::OsString {
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap()) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let link = farm.join(entry.file_name());
            if entry.file_name() != "jq" && !link.exists() {
                let _ = std::os::unix::fs::symlink(entry.path(), link);
            }
        }
    }
    farm.as_os_str().to_owned()
}

/// Whether the deploy-release and deploy.sh readiness calls both accept the raw `body`; they must agree.
fn ready(body: &str, jq: bool) -> bool {
    let farm = tempfile::tempdir().unwrap();
    let mut command = Command::new("bash");
    if !jq {
        command.env("PATH", path_without_jq(farm.path()));
    }
    let script = r#"
        [ "$(command -v jq >/dev/null 2>&1 && echo 1 || echo 0)" = "$WANT_JQ" ] || exit 90
        . "$DEFAULTS" || exit 91
        release=0; health_json_is_ready "$BODY" 1 1 1 1 >/dev/null 2>&1 || release=1
        plain=0; health_json_is_ready "$BODY" 0 1 >/dev/null 2>&1 || plain=1
        [ "$release" = "$plain" ] || exit 92
        exit "$release""#;
    let status = command
        .args(["-c", script])
        .env("WANT_JQ", if jq { "1" } else { "0" })
        .env(
            "DEFAULTS",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/_defaults.sh"),
        )
        .env("BODY", body)
        .status()
        .unwrap();
    match status.code() {
        Some(0) => true,
        Some(1) => false,
        other => panic!("readiness harness failed ({other:?}, jq={jq}) for {body}"),
    }
}

fn assert_ready(label: &str, body: &str, expected: bool) {
    for jq in [true, false] {
        assert_eq!(ready(body, jq), expected, "{label} (jq={jq}): {body}");
    }
}

fn reasons(body: &Value) -> Vec<&str> {
    let reasons = body["degraded_reasons"].as_array().unwrap();
    reasons.iter().filter_map(Value::as_str).collect()
}

#[tokio::test]
async fn public_health_proves_the_tui_gateway_reason_to_the_readiness_predicate() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::public_health_proves_the_tui_gateway_reason_to_the_readiness_predicate"
    )) {
        return;
    }
    let reason = "provider:claude:tui_output_requires_gateway";
    let dormant = public_body(false, None).await;
    assert!(
        dormant.get("tui_output_gateway_channels").is_none(),
        "{dormant}"
    );
    assert!(!reasons(&dormant).contains(&reason), "{dormant}");

    // With an empty list the intake gate still refuses the provider, which no evidence settles.
    let refused = public_body(false, Some(&[])).await;
    assert!(reasons(&refused).contains(&reason), "{refused}");
    assert!(
        refused.get("tui_output_gateway_channels").is_none(),
        "{refused}"
    );
    assert_ready(
        "intake refused without writer channels",
        &refused.to_string(),
        false,
    );

    // Writer channels prove nothing while the intake gate still refuses the provider.
    for standby in [false, true] {
        let refused = public_body(standby, Some(CHANNELS)).await;
        assert!(reasons(&refused).contains(&reason), "{refused}");
        assert!(
            refused.get("tui_output_gateway_channels").is_none(),
            "{refused}"
        );
        let label = format!("intake refused with writer channels (standby={standby})");
        assert_ready(&label, &refused.to_string(), false);
    }

    let _admitted = test_override::admit_intake();
    let worker = public_body(false, Some(CHANNELS)).await;
    assert!(reasons(&worker).contains(&reason), "{worker}");
    assert_eq!(
        worker["tui_output_gateway_channels"],
        json!(["claude:worker:complete:1"])
    );
    assert_ready("verified worker", &worker.to_string(), true);
    // Served bytes need not be compact: whitespace between array tokens must not change the verdict.
    let pretty = serde_json::to_string_pretty(&worker).unwrap();
    assert_ready("verified worker, pretty-printed", &pretty, true);
    assert_ready(
        "verified standby",
        &public_body(true, Some(CHANNELS)).await.to_string(),
        true,
    );

    for evidence in [
        json!(["claude:gateway:complete:1"]),
        json!(["claude:worker:incomplete:1"]),
        json!(["claude:worker:complete:0"]),
        json!(["codex:worker:complete:1"]),
        json!(["claude::complete:1"]),
        json!(["claude:worker:complete:1", "claude:gateway:complete:1"]),
        Value::Null,
    ] {
        let mut body = worker.clone();
        body["tui_output_gateway_channels"] = evidence.clone();
        assert_ready(&format!("unverified {evidence}"), &body.to_string(), false);
    }

    let router = AlarmRouter::for_process(None, None);
    router.raise_at(
        WRITER_CHANNEL,
        &WriterAlarm::PausedNoGateway,
        Instant::now(),
    );
    assert_ready(
        "paused writer",
        &public_body(false, Some(CHANNELS)).await.to_string(),
        false,
    );
    alarm::gateway_resumed(WRITER_CHANNEL);
    assert_ready(
        "resumed writer",
        &public_body(false, Some(CHANNELS)).await.to_string(),
        true,
    );
    let blocked = WriterAlarm::Blocked { status: 403 };
    router.raise_at(WRITER_CHANNEL, &blocked, Instant::now());
    alarm::gateway_resumed(WRITER_CHANNEL);
    assert_ready(
        "blocked writer",
        &public_body(false, Some(CHANNELS)).await.to_string(),
        false,
    );
}
