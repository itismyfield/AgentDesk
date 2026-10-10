use std::collections::BTreeSet;
use std::sync::Arc;

use super::super::{AdoptionRuntime, install, installed};
use super::*;
use crate::services::discord::{SharedData, make_shared_data_for_tests};
use crate::services::provider::ProviderKind;
use crate::services::tui_o::shadow::tap::TuiOConfig;

fn on() -> TuiOConfig {
    TuiOConfig {
        codex_history_adoption: true,
        ..TuiOConfig::default()
    }
}

fn runtime(shared: &Arc<SharedData>) -> Arc<AdoptionRuntime> {
    install(shared, &ProviderKind::Codex, "bot", Some(&on())).expect("flag on installs")
}

fn source(key: &str, channel: Option<u64>, bound: bool) -> LiveSource {
    LiveSource {
        key: key.to_owned(),
        channel,
        bound,
    }
}

fn channels(ids: &[u64]) -> BTreeSet<u64> {
    ids.iter().copied().collect()
}

async fn read(runtime: &AdoptionRuntime, ids: &[u64]) -> Result<CodexBootWitness, BootReadError> {
    runtime
        .wait_boot(channels(ids), std::future::pending())
        .await
}

/// A Gateway boot whose recovery completed; only discovery is left to decide.
fn recovered_gateway(shared: &Arc<SharedData>) -> Arc<AdoptionRuntime> {
    let runtime = runtime(shared);
    runtime
        .boot
        .progress
        .send_modify(|p| p.role = Some(BootRole::Gateway));
    runtime.record_recovery_for_tests();
    runtime
}

#[test]
fn codex_history_flag_defaults_off() {
    for yaml in [
        "{}",
        "writer:\n  channels: [1]\n",
        "codex_history_adoption: false\n",
    ] {
        let config: TuiOConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(!config.codex_history_adoption, "{yaml}");
    }
    let config: TuiOConfig = serde_yaml::from_str("codex_history_adoption: true\n").unwrap();
    assert!(config.codex_history_adoption);
    // A whole agentdesk.yaml whose tui_o section never names the flag.
    let mut yaml = serde_yaml::to_value(crate::config::Config::default()).unwrap();
    yaml["tui_o"] = serde_yaml::from_str("writer:\n  channels: [7]\n").unwrap();
    let config: crate::config::Config = serde_yaml::from_value(yaml).unwrap();
    assert!(!config.tui_o.unwrap().codex_history_adoption);
    // Off serializes nothing new, so a rewritten config is unchanged.
    let written = serde_yaml::to_string(&TuiOConfig::default()).unwrap();
    assert!(!written.contains("codex_history_adoption"), "{written}");
}

#[test]
fn off_or_non_codex_boot_installs_nothing() {
    let shared = make_shared_data_for_tests();
    let off = TuiOConfig::default();
    for (provider, config) in [
        (ProviderKind::Codex, None),
        (ProviderKind::Codex, Some(&off)),
        (ProviderKind::Claude, Some(&on())),
    ] {
        assert!(install(&shared, &provider, "bot", config).is_none());
        assert!(installed(&shared).is_none(), "{provider:?}");
    }
    let runtime = runtime(&shared);
    assert!(Arc::ptr_eq(&installed(&shared).unwrap(), &runtime));
    // Without a runtime the role record is a no-op: no task, no state.
    record_role(None, BootRole::Gateway);
}

#[tokio::test(start_paused = true)]
async fn evidence_never_crosses_runtimes() {
    let (a, b) = (make_shared_data_for_tests(), make_shared_data_for_tests());
    let (runtime_a, runtime_b) = (recovered_gateway(&a), recovered_gateway(&b));
    assert_ne!(runtime_a.id, runtime_b.id);
    // A producer finds its own SharedData's runtime, never another live one.
    installed(&a)
        .unwrap()
        .record_discovery(Ok(vec![source("a", Some(1), true)]));
    let witness = read(&runtime_a, &[1]).await.expect("a's own pass");
    assert_eq!((witness.runtime, witness.pass), (runtime_a.id, 1));
    assert_eq!(
        read(&runtime_b, &[1]).await.unwrap_err(),
        BootReadError::TimedOut
    );
    assert!(Arc::ptr_eq(&installed(&b).unwrap(), &runtime_b));
}

#[tokio::test(start_paused = true)]
async fn ended_runtime_is_not_found_and_reads_gone() {
    let shared = make_shared_data_for_tests();
    let runtime = recovered_gateway(&shared);
    drop(shared);
    let restarted = make_shared_data_for_tests();
    assert!(installed(&restarted).is_none());
    runtime.record_discovery(Ok(Vec::new()));
    assert_eq!(
        read(&runtime, &[]).await.unwrap_err(),
        BootReadError::RuntimeGone
    );
}

#[tokio::test(start_paused = true)]
async fn boot_wait_is_bounded_and_cancel_safe() {
    let shared = make_shared_data_for_tests();
    let runtime = recovered_gateway(&shared);
    let started = tokio::time::Instant::now();
    assert_eq!(
        read(&runtime, &[1]).await.unwrap_err(),
        BootReadError::TimedOut
    );
    assert_eq!(
        started.elapsed(),
        Duration::from_secs(60),
        "no early give-up, no longer wait"
    );

    let stop = tokio::time::sleep(Duration::from_secs(5));
    let cancelled = runtime.wait_boot(channels(&[1]), stop).await;
    assert_eq!(cancelled.unwrap_err(), BootReadError::Cancelled);
    // A dropped wait leaves nothing: a later pass still makes the witness at once.
    let dropped = tokio::time::timeout(Duration::from_secs(1), read(&runtime, &[1])).await;
    assert!(dropped.is_err());
    runtime.record_discovery(Ok(vec![source("a", Some(1), true)]));
    let started = tokio::time::Instant::now();
    assert!(read(&runtime, &[1]).await.is_ok());
    assert_eq!(started.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn witness_needs_role_discovery_and_recovery() {
    let shared = make_shared_data_for_tests();
    let runtime = runtime(&shared);
    runtime.record_discovery(Ok(vec![source("a", Some(1), true)]));
    assert_eq!(
        read(&runtime, &[1]).await.unwrap_err(),
        BootReadError::TimedOut,
        "no role"
    );
    runtime
        .boot
        .progress
        .send_modify(|p| p.role = Some(BootRole::RestWorker));
    assert_eq!(
        read(&runtime, &[1]).await.unwrap_err(),
        BootReadError::TimedOut,
        "no recovery"
    );
    // The witness arrives while waiting, once recovery is recorded.
    let wait = read(&runtime, &[1]);
    let record = async {
        tokio::time::sleep(Duration::from_secs(30)).await;
        runtime.record_recovery_for_tests();
    };
    let (witness, ()) = tokio::join!(wait, record);
    let witness = witness.expect("both parts recorded");
    assert_eq!(
        (witness.role, witness.channels.clone()),
        (BootRole::RestWorker, channels(&[1]))
    );
    assert_eq!(
        (witness.provider, witness.bot.as_str()),
        (ProviderKind::Codex, "bot")
    );
}

#[tokio::test(start_paused = true)]
async fn failed_listing_is_not_an_empty_success() {
    let shared = make_shared_data_for_tests();
    let runtime = recovered_gateway(&shared);
    runtime.record_discovery(Err("tmux sessions: unreadable".into()));
    assert_eq!(
        read(&runtime, &[1]).await.unwrap_err(),
        BootReadError::TimedOut
    );
    runtime.record_discovery(Ok(Vec::new()));
    assert_eq!(read(&runtime, &[1]).await.expect("read, and empty").pass, 2);
    // The latest pass decides: a later failure withdraws the evidence.
    runtime.record_discovery(Err("rehydrate task: panicked".into()));
    assert_eq!(
        read(&runtime, &[1]).await.unwrap_err(),
        BootReadError::TimedOut
    );
}

#[tokio::test(start_paused = true)]
async fn every_live_source_of_the_scope_must_be_bound() {
    let shared = make_shared_data_for_tests();
    let runtime = recovered_gateway(&shared);
    runtime.record_discovery(Ok(vec![
        source("a", Some(1), true),
        source("b", Some(2), false),
    ]));
    assert!(read(&runtime, &[1]).await.is_ok(), "b is outside {{1}}");
    assert_eq!(
        read(&runtime, &[1, 2]).await.unwrap_err(),
        BootReadError::TimedOut
    );
    // The unbound source listed first is not hidden by a bound one after it.
    runtime.record_discovery(Ok(vec![
        source("b", Some(1), false),
        source("a", Some(1), true),
    ]));
    assert_eq!(
        read(&runtime, &[1]).await.unwrap_err(),
        BootReadError::TimedOut
    );
    // A source the pass could not place may be any channel's.
    runtime.record_discovery(Ok(vec![
        source("a", Some(1), true),
        source("x", None, false),
    ]));
    assert_eq!(
        read(&runtime, &[1]).await.unwrap_err(),
        BootReadError::TimedOut
    );
}

#[tokio::test(start_paused = true)]
async fn unsupported_role_never_publishes_success() {
    let shared = make_shared_data_for_tests();
    // Standby is answered by its role alone, before and without any evidence.
    let bare = runtime(&shared);
    record_role(Some(&bare), BootRole::Standby);
    let started = tokio::time::Instant::now();
    let unsupported = BootReadError::Unsupported(BootRole::Standby);
    assert_eq!(read(&bare, &[1]).await.unwrap_err(), unsupported);
    assert_eq!(started.elapsed(), Duration::ZERO);

    let runtime = runtime(&shared);
    runtime.record_discovery(Ok(vec![source("a", Some(1), true)]));
    runtime.record_recovery_for_tests();
    runtime
        .boot
        .progress
        .send_modify(|p| p.role = Some(BootRole::Standby));
    let started = tokio::time::Instant::now();
    let read = read(&runtime, &[1]).await;
    assert_eq!(
        read.unwrap_err(),
        BootReadError::Unsupported(BootRole::Standby)
    );
    assert_eq!(
        started.elapsed(),
        Duration::ZERO,
        "known at once, not by timeout"
    );
}

/// The whole source of `path`; test items in it are scanned too, so nothing is hidden.
fn prod_source(path: &str) -> String {
    std::fs::read_to_string(format!("{}/{path}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

// Dormancy: nothing in production turns the flag on or waits on a boot read outside this module,
// and the module itself reaches no activation, init, anchor, post or Candidate surface.
#[test]
fn u3_2a1_is_dormant_in_production() {
    let mut files = Vec::new();
    rust_files(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    let module = std::path::Path::new("src/services/discord/codex_adoption_runtime");
    let mut flag_reads = Vec::new();
    for file in &files {
        let relative = file.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap();
        let name = relative.to_string_lossy();
        if name.ends_with("_tests.rs") || name.contains("/tests/") {
            continue;
        }
        let text = prod_source(&name);
        assert!(
            !text.contains("codex_history_adoption: true"),
            "{name} turns the flag on"
        );
        assert!(
            !text.contains("codex_history_adoption = true"),
            "{name} turns the flag on"
        );
        if text.contains(".codex_history_adoption") {
            flag_reads.push(name.to_string());
        }
        if !relative.starts_with(module) {
            assert!(!text.contains("wait_boot("), "{name} waits on a boot read");
        }
        if name.contains("tui_o/writer/") || name.ends_with("o_writer_host.rs") {
            assert!(
                !text.contains("codex_adoption_runtime"),
                "{name} hosts Codex adoption"
            );
        }
    }
    assert_eq!(
        flag_reads,
        ["src/services/discord/codex_adoption_runtime.rs"]
    );
    for path in [
        "src/services/discord/codex_adoption_runtime.rs",
        "src/services/discord/codex_adoption_runtime/boot.rs",
    ] {
        let text = prod_source(path);
        for surface in [
            "tui_o::writer",
            "activation",
            "Candidate",
            "send_message",
            "http",
            "anchor",
        ] {
            assert!(!text.contains(surface), "{path} reaches {surface}");
        }
    }
}
