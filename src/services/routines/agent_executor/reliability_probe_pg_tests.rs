//! A fresh run's provider-session probe against PostgreSQL legacy session rows.
use crate::services::discord::health::routine_ports::build_routine_agent_executor;
use crate::services::provider::ProviderKind;
use crate::services::routines::agent_executor::routine_agent_session_name;
use crate::services::routines::agent_executor::tests::running_run;
use crate::services::routines::store::RunningAgentRoutineRun;
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use std::sync::Arc;

fn probe_run(provider: &str, name: &str, channel: Option<Value>) -> RunningAgentRoutineRun {
    let started = Utc::now() - Duration::hours(1);
    let mut result = json!({"provider": provider, "agent_id": "agent-1"});
    if let Some(channel) = channel {
        result["channel_id"] = channel;
    }
    RunningAgentRoutineRun {
        name: name.to_string(),
        started_at: started,
        attempts: json!([{"event": "started", "at": started.to_rfc3339()}]),
        result_json: Some(result),
        ..running_run(None)
    }
}

fn probe_session(provider: &str, name: &str) -> String {
    let provider = ProviderKind::from_str(provider).unwrap();
    provider.build_tmux_session_name(&routine_agent_session_name(name, "agent-1"))
}

// The probe's failure stands only for a found legacy row behind a registered bot hash;
// a missing, failed, Herdr or unregistered answer leaves the run to its timeout.
#[cfg(unix)]
#[tokio::test]
async fn fresh_probe_failure_stands_only_for_a_found_legacy_row_pg() {
    use crate::db::dispatched_sessions::hosted_execution::tests::{TOKEN, owner, pending, wire};
    use crate::services::discord::health::HealthRegistry;
    use crate::services::discord::host_key_derivation::tests::seed_row;
    use crate::services::platform::tmux::PaneLiveness;
    use crate::services::tmux_diagnostics::PaneLivenessOverrideGuard;
    use std::os::unix::fs::PermissionsExt;

    let _root = crate::config::TestRuntimeRootGuard::new();
    let fake = tempfile::tempdir().unwrap();
    let tmux = fake.path().join("tmux");
    let pane = "[sending...]\n[API Error: 410 status code (no body)]\n\n▶ Ready for input (type message + Enter)";
    let body = format!("#!/bin/sh\n[ \"$2\" = capture-pane ] || exit 1\nprintf '%s\\n' '{pane}'\n");
    std::fs::write(&tmux, body).unwrap();
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![fake.path().to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(paths).unwrap();
    let _path = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "PATH",
        std::path::Path::new(&path),
    );

    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let registry = Arc::new(HealthRegistry::new());
    for provider in ["claude", "qwen"] {
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared)
            .unwrap()
            .set_token_hash_for_test(TOKEN);
        registry.register(provider.to_string(), shared).await;
    }
    let channel = |n: u64| 1_479_671_301_387_061_000 + n;
    let seed = |provider: &'static str, hash: &'static str, key: String, n: u64, raw| {
        let pool = pool.clone();
        async move { seed_row(&pool, provider, Some(hash), &key, channel(n), raw).await }
    };
    let current = |provider: &str, hash: &str, name: &str| {
        let host = crate::services::platform::hostname_short();
        format!("{provider}/{hash}/{host}:{}", probe_session(provider, name))
    };
    seed(
        "claude",
        TOKEN,
        current("claude", TOKEN, "w2b legacy"),
        1,
        None,
    )
    .await;
    let moved = format!(
        "claude/{TOKEN}/old-host:{}",
        probe_session("claude", "w2b moved")
    );
    seed("claude", TOKEN, moved, 2, None).await;
    seed(
        "claude",
        "h-old",
        current("claude", "h-old", "w2b rotated"),
        3,
        None,
    )
    .await;
    let herdr = Some(wire(&pending(&owner(&channel(4).to_string()), "n-w2b")));
    seed(
        "claude",
        TOKEN,
        current("claude", TOKEN, "w2b herdr"),
        4,
        herdr,
    )
    .await;
    seed(
        "claude",
        TOKEN,
        current("claude", TOKEN, "w2b marker"),
        5,
        None,
    )
    .await;
    let marker = crate::services::tmux_common::session_temp_path(
        &probe_session("claude", "w2b marker"),
        "host_kind",
    );
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, "herdr").unwrap();
    seed("qwen", TOKEN, current("qwen", TOKEN, "w2b qwen"), 6, None).await;

    let executor = |registry: Option<Arc<HealthRegistry>>| {
        build_routine_agent_executor(Arc::new(pool.clone()), registry, 60)
    };
    let registered = executor(Some(registry.clone()));
    let text = |n: u64| Some(json!(channel(n).to_string()));
    let dead = PaneLiveness::DeadOrAbsent;
    let cases = [
        (
            "found legacy row",
            "claude",
            "w2b legacy",
            text(1),
            dead,
            true,
        ),
        (
            "renamed host, channel as text",
            "claude",
            "w2b moved",
            text(2),
            dead,
            true,
        ),
        (
            "renamed host, channel as number",
            "claude",
            "w2b moved",
            Some(json!(channel(2))),
            dead,
            true,
        ),
        (
            "renamed host, no channel",
            "claude",
            "w2b moved",
            None,
            dead,
            false,
        ),
        ("no row", "claude", "w2b absent", text(7), dead, false),
        (
            "rotated token",
            "claude",
            "w2b rotated",
            text(3),
            dead,
            false,
        ),
        ("Herdr record", "claude", "w2b herdr", text(4), dead, false),
        ("Herdr marker", "claude", "w2b marker", text(5), dead, false),
        (
            "live pane",
            "claude",
            "w2b legacy",
            text(1),
            PaneLiveness::Live,
            false,
        ),
        (
            "failed probe",
            "claude",
            "w2b legacy",
            text(1),
            PaneLiveness::ProbeError,
            false,
        ),
        (
            "Qwen API error, legacy row",
            "qwen",
            "w2b qwen",
            text(6),
            PaneLiveness::Live,
            true,
        ),
        (
            "Qwen API error, no row",
            "qwen",
            "w2b qwen absent",
            text(8),
            PaneLiveness::Live,
            false,
        ),
    ];
    for (label, provider, name, channel, liveness, fails) in cases {
        let _pane = PaneLivenessOverrideGuard::set(&probe_session(provider, name), liveness);
        let run = probe_run(provider, name, channel);
        let failure = registered.fresh_provider_session_failure(&run).await;
        assert_eq!(failure.is_some(), fails, "{label}: {failure:?}");
    }
    let _pane = PaneLivenessOverrideGuard::set(&probe_session("claude", "w2b legacy"), dead);
    let run = probe_run("claude", "w2b legacy", text(1));
    let unregistered = executor(None).fresh_provider_session_failure(&run).await;
    assert_eq!(
        unregistered, None,
        "no registered hash is not a legacy answer"
    );
    pool.close().await;
    let failed = registered.fresh_provider_session_failure(&run).await;
    assert_eq!(failed, None, "a failed lookup is not a legacy answer");
    db.drop().await;
}
