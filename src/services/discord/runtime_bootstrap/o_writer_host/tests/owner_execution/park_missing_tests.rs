use super::*;
use crate::services::tui_o::channel_policy::Adoption;
use crate::services::tui_o::shadow::{
    CaptureOutcome, CaptureSource, SourceId, capture::SourceCapture,
};
use crate::services::tui_o::store::spool::source_key;
use crate::services::tui_o::store::{InitSource, Initialized, OEra, OStore, StoreConfig};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[path = "park_reply_tests.rs"]
mod park_reply_tests;

fn retained_bytes(root: &Path, channel: u64, source: &SourceId) -> BTreeMap<PathBuf, String> {
    let channel = root.join("o_store").join(channel.to_string());
    let key = source_key(source);
    let mut bytes = BTreeMap::new();
    for dir in ["cursor", "spool"] {
        for entry in std::fs::read_dir(channel.join(dir)).unwrap() {
            let path = entry.unwrap().path();
            if path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(&key)
            {
                bytes.insert(
                    path.clone(),
                    hex::encode(Sha256::digest(std::fs::read(path).unwrap())),
                );
            }
        }
    }
    bytes
}

fn missing_store(root: &Path, channel: u64) -> (OStore, OEra, SourceId) {
    let path = root.join("old.jsonl");
    std::fs::write(&path, b"{}\n{}\n").unwrap();
    let source = crate::services::tui_o::shadow::binding_reader::source_id_for(
        &uuid::Uuid::new_v4().to_string(),
        &path,
    )
    .unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root)
        .unwrap()
        .unwrap();
    let era = store
        .begin_era(&[channel], chrono::Utc::now(), |channel| {
            Ok(Initialized {
                channel,
                sources: vec![InitSource {
                    source_id: source.clone(),
                    delivery_start: 0,
                    prefix_hash: hex::encode(Sha256::digest(b"")),
                }],
                initial_anchor: 100,
                build_digest: "missing-source-fixture".into(),
                at: chrono::Utc::now(),
            })
        })
        .unwrap();
    let mut channel_store = store.open_channel(&era, channel).unwrap().unwrap();
    channel_store.set_binding_checkpoint(0).unwrap();
    channel_store.set_segment_max_for_tests(1);
    let mut capture = SourceCapture::open(source.clone(), 0).unwrap();
    for _ in 0..2 {
        let CaptureOutcome::Batch(batch) = capture.poll(3) else {
            panic!("capture batch")
        };
        assert_eq!(batch.records.len(), 1);
        channel_store
            .append_spool(&batch, &capture.prefix_hash())
            .unwrap();
    }
    assert_eq!(channel_store.retained_segments(&source), 2);
    assert!(!channel_store.cursor(&source).unwrap().retired);
    drop(channel_store);
    std::fs::remove_file(path).unwrap();
    (store, era, source)
}

async fn accepts(channel: u64) {
    let result = tokio::time::timeout(Duration::from_secs(12), async {
        while !host::channel_accepts(channel) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "missing source must park without stopping its host: hosted={:?} ready={} health={:?}",
        host::process_readiness().hosted_by(channel),
        host::process_readiness().is_ready(channel),
        crate::services::tui_o::alarm::health_reasons()
    );
}

async fn scenario(role: &str, cleanup_first: bool, superseded_first: bool, dormant: bool) {
    let root = crate::config::TestRuntimeRootGuard::new();
    let _binding =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let _live = crate::config_live_reload::test_live_guard_after_shared_test_env_lock();
    let _namespace = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "TMUX_TMPDIR",
        &root.path().canonicalize().unwrap(),
    );
    // A tmux client prefers `TMUX` over `TMUX_TMPDIR`, so an inherited pane would expose the
    // operator's sessions to the producer-absence probe and keep a missing source from parking.
    let _pane = crate::config::TestEnvVarGuard::remove_after_shared_test_env_lock("TMUX");
    let channel = 78_450_001
        + match role {
            "worker" => 10,
            "standby" => 20,
            _ => 0,
        }
        + u64::from(cleanup_first)
        + 2 * u64::from(superseded_first)
        + 100 * u64::from(dormant);
    let name = format!("park-missing-{channel}");
    let _hosts = crate::config::session_hosts::force_for_test(Some("mac-a"), &[]);
    let config: crate::config::Config = serde_json::from_value(json!({"server":{},
        "cluster":{"enabled":true,"instance_id":"mac-a","gateway_preferred_instance_id":"mac-a"},
        "agents":[{"id":"park-agent","name":"fixture","provider":"claude",
            "channels":{"claude":{"id":channel.to_string(),"name":name,"runtime":"tui"}}}],
        "tui_o":{"writer":{"channels":[channel], "dormant_channels": if dormant { vec![channel] } else { vec![] }}}})).unwrap();
    crate::config_live_reload::install(config.clone());
    let _selected = cutover::test_override::force_boot(
        BootChannels::validate(&config)
            .unwrap()
            .adopted(Adoption::Committed),
    );
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(channel);
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    if role != "gateway" {
        execution::tests::admission(&pool, "claude", channel, "mac-a").await;
        let permit = execution::read(&pool, "claude", channel, "mac-a", || Ok(false))
            .await
            .unwrap()
            .unwrap();
        cutover::boot_snapshot()
            .unwrap()
            .install_execution(channel, Some(Ok(permit)));
    }
    let (_registry, shared) = runtime(&pool, ProviderKind::Claude, channel, role).await;
    crate::services::discord::host_defer_gate::tests::map_channel(
        &shared,
        serenity::ChannelId::new(channel),
        &name,
    )
    .await;
    let server = gateway_context(&shared, channel).await;
    let _rest = crate::services::discord::shared_state::test_rest::install(
        shared.http.cached_serenity_ctx.get().unwrap().http.clone(),
    );
    let (store, era, source) = missing_store(root.path(), channel);
    crate::services::tui_o::channel_policy::install(&config).unwrap();
    let original = retained_bytes(root.path(), channel, &source);
    let gate = crate::services::tui_o::ownership::gate("claude");
    gate.acquired();
    if superseded_first {
        super::super::super::fence_tests::supersede_producer_once_for_tests(channel);
    }
    let tui_o = TuiOConfig::default();
    let refresh = || {
        super::super::super::super::owner_cleanup::refresh_writers(
            &shared,
            &ProviderKind::Claude,
            Some(&tui_o),
        )
    };
    if cleanup_first {
        refresh().await;
    } else if role == "gateway" {
        spawn(&shared, &ProviderKind::Claude, Some(&tui_o), true);
    } else {
        spawn_execution(&shared, &ProviderKind::Claude, Some(&tui_o));
    }
    accepts(channel).await;
    let readiness = host::process_readiness();
    let generation = readiness.hosted_by(channel).unwrap();
    for _ in 0..5 {
        refresh().await;
        let debt = readiness
            .undelivered_after(channel, Duration::from_secs(5))
            .await
            .expect("a parked source must retain a polling actor and a readable owed view");
        assert_eq!(
            (debt.owed, debt.prepared, debt.unsealed, debt.uncaptured),
            (0, 0, 0, 0)
        );
        assert_eq!(
            readiness.hosted_by(channel),
            Some(generation),
            "refresh must retain the first host generation"
        );
        assert!(host::channel_accepts(channel));
        assert_eq!(
            for_input_placement("claude", channel).await,
            IntakeRoute::Gateway
        );
    }
    assert!(
        !crate::services::tui_o::alarm::health_reasons()
            .contains(&format!("tui_o:halted:{channel}"))
    );
    assert_eq!(retained_bytes(root.path(), channel, &source), original);
    let restored = store.open_channel(&era, channel).unwrap().unwrap();
    assert!(!restored.cursor(&source).unwrap().retired);
    assert_eq!(restored.retained_segments(&source), 2);
    assert!(restored.cleanup_witness(&source).unwrap().is_none());
    assert!(restored.rotation().unwrap().successors.is_empty());
    if let Some(view) = cutover::boot_snapshot().unwrap().execution(channel) {
        view.permit.unwrap().gate().close();
    }
    gate.lost();
    server.abort();
    pool.close().await;
}

fn isolated_scenario(test: &str, scenario: usize, failures: &mut usize) -> bool {
    const CASE: &str = "ADK_PARK_SCENARIO";
    if let Ok(selected) = std::env::var(CASE) {
        return selected == scenario.to_string();
    }
    let qualified = test
        .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
        .unwrap_or(test);
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", qualified, "--nocapture", "--test-threads=1"])
        .env(CASE, scenario.to_string())
        .output();
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            *failures += 1;
            eprintln!("PARK_SCENARIO test={test} scenario={scenario} spawn_failed={error}");
            return false;
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let passed = output.status.success() && stdout.contains("1 passed; 0 failed; 0 ignored;");
    eprintln!(
        "PARK_SCENARIO test={test} scenario={scenario} status={} passed={passed}\n{stdout}\n{stderr}",
        output.status
    );
    if !passed {
        *failures += 1;
    }
    false
}

#[tokio::test(flavor = "current_thread")]
async fn owner_cleanup_parks_missing_source_of_sessionless_channel_gateway_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_cleanup_parks_missing_source_of_sessionless_channel_gateway_pg"
    )) {
        return;
    }
    eprintln!("PARK_CASE_MAP 0=gateway.initial 1=gateway.cleanup");
    let mut failures = 0;
    for (index, cleanup_first) in [false, true].into_iter().enumerate() {
        if isolated_scenario(
            concat!(
                module_path!(),
                "::owner_cleanup_parks_missing_source_of_sessionless_channel_gateway_pg"
            ),
            index,
            &mut failures,
        ) {
            scenario("gateway", cleanup_first, false, false).await;
        }
    }
    assert_eq!(failures, 0, "all gateway subcases must pass");
}

#[tokio::test(flavor = "current_thread")]
async fn owner_cleanup_parks_missing_source_of_sessionless_channel_execution_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_cleanup_parks_missing_source_of_sessionless_channel_execution_pg"
    )) {
        return;
    }
    eprintln!(
        "PARK_CASE_MAP 0=worker.initial 1=worker.cleanup 2=standby.initial 3=standby.cleanup"
    );
    let mut failures = 0;
    for (role_index, role) in ["worker", "standby"].into_iter().enumerate() {
        for (mode_index, cleanup_first) in [false, true].into_iter().enumerate() {
            if isolated_scenario(
                concat!(
                    module_path!(),
                    "::owner_cleanup_parks_missing_source_of_sessionless_channel_execution_pg"
                ),
                role_index * 2 + mode_index,
                &mut failures,
            ) {
                scenario(role, cleanup_first, false, false).await;
            }
        }
    }
    assert_eq!(failures, 0, "all execution subcases must pass");
}

#[tokio::test(flavor = "current_thread")]
async fn presence_superseded_first_sample_then_absent_parks_in_first_host_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::presence_superseded_first_sample_then_absent_parks_in_first_host_pg"
    )) {
        return;
    }
    scenario("gateway", false, true, false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn owner_cleanup_reader_config_parks_missing_source_without_cleanup_evidence_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_cleanup_reader_config_parks_missing_source_without_cleanup_evidence_pg"
    )) {
        return;
    }
    scenario("gateway", true, false, true).await;
}
