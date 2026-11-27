use crate::services::legacy_body_census::{
    boot_journal_snapshot_for_test, ensure_ready, health_json, start,
};
use crate::services::process_boot::{self, Boot};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn records(root: &Path, boot: &Boot) -> Vec<Value> {
    std::fs::read_to_string(
        root.join("legacy_body_census")
            .join(format!("{}.jsonl", boot.id)),
    )
    .unwrap()
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect()
}

fn header(root: &Path, boot: &Boot) -> Value {
    let raw = std::fs::read_to_string(
        root.join("legacy_body_census")
            .join(format!("{}.jsonl", boot.id)),
    )
    .unwrap();
    serde_json::from_str(raw.lines().next().unwrap()).unwrap()
}

#[tokio::test]
async fn unsampled_boot_child() {
    assert!(uuid::Uuid::parse_str(&process_boot::initialize().id).is_ok());
    let Ok(root) = std::env::var("ADK_I7651_CHILD_ROOT") else {
        return;
    };
    let root = std::path::Path::new(&root);
    std::fs::create_dir_all(root.join("runtime")).unwrap();
    std::fs::write(
        root.join("runtime/dcserver.pid"),
        std::process::id().to_string(),
    )
    .unwrap();
    assert!(crate::config::snapshot().is_none());
    start();
    let config: crate::config::Config =
        serde_json::from_value(json!({"server":{},"cluster":{"instance_id":"test-node"}})).unwrap();
    crate::bootstrap::prepare_and_install_boot_snapshots(&config, None)
        .await
        .unwrap();
    ensure_ready().await;
    let written = header(root, process_boot::initialize());
    assert_eq!(written["host"], "test-node");
    assert!(
        !health_json()["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("HOST_IDENTITY_UNKNOWN"))
    );
    std::process::exit(0);
}

#[test]
fn two_processes_keep_unsampled_crash_boots_and_their_incarnations() {
    let root = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                concat!(module_path!(), "::unsampled_boot_child")
                    .trim_start_matches(concat!(env!("CARGO_CRATE_NAME"), "::")),
                "--nocapture",
            ])
            .env("ADK_I7651_CHILD_ROOT", root.path())
            .env("AGENTDESK_ROOT_DIR", root.path())
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&child.stdout).contains("running 1 test"),
            "child filter selected no test: {}",
            String::from_utf8_lossy(&child.stdout)
        );
        assert!(
            child.status.success(),
            "{}",
            String::from_utf8_lossy(&child.stderr)
        );
    }
    let headers: Vec<Value> = std::fs::read_dir(root.path().join("legacy_body_census"))
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "jsonl"))
        .map(|entry| {
            let raw = std::fs::read_to_string(entry.path()).unwrap();
            serde_json::from_str(raw.lines().next().unwrap()).unwrap()
        })
        .collect();
    assert_eq!(headers.len(), 2);
    assert_ne!(headers[0]["boot_id"], headers[1]["boot_id"]);
    let head: Value = serde_json::from_slice(
        &std::fs::read(root.path().join("legacy_body_census/HEAD")).unwrap(),
    )
    .unwrap();
    let current = headers
        .iter()
        .find(|header| header["boot_id"] == head["boot_id"])
        .unwrap();
    let predecessor = headers
        .iter()
        .find(|header| header["boot_id"] != head["boot_id"])
        .unwrap();
    assert_eq!(current["predecessor_boot_id"], predecessor["boot_id"]);
}

fn child(case: &str, test: &str) {
    let root = tempfile::tempdir().unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env("ADK_I7651_ENTRY_CASE", case)
        .env("AGENTDESK_ROOT_DIR", root.path())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&result.stdout).contains("running 1 test"));
    assert!(
        result.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

fn production_root() -> PathBuf {
    let root = PathBuf::from(std::env::var("AGENTDESK_ROOT_DIR").unwrap());
    std::fs::create_dir_all(root.join("runtime")).unwrap();
    std::fs::write(
        root.join("runtime/dcserver.pid"),
        std::process::id().to_string(),
    )
    .unwrap();
    root
}

#[tokio::test]
async fn production_entry_writes_header_and_boot_ready() {
    if std::env::var("ADK_I7651_ENTRY_CASE").is_err() {
        child(
            "entry",
            concat!(
                module_path!(),
                "::production_entry_writes_header_and_boot_ready"
            )
            .trim_start_matches(concat!(env!("CARGO_CRATE_NAME"), "::")),
        );
        return;
    }
    let root = production_root();
    let id = process_boot::initialize();
    assert!(
        health_json()["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("JOURNAL_NOT_STARTED"))
    );
    start();
    ensure_ready().await;
    ensure_ready().await;
    let health = health_json();
    assert_eq!(health["journal_ready"], true);
    assert!(
        !health["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("JOURNAL_NOT_STARTED"))
    );
    assert_eq!(health["boot_id"], id.id);
    assert_eq!(
        header(&root, id)["doctor_boot_id"],
        crate::cli::doctor::startup::current_boot_id_at(&root).unwrap()
    );
    assert_eq!(
        records(&root, id)
            .iter()
            .filter(|record| record["kind"] == "boot_ready")
            .count(),
        1
    );
}

#[tokio::test]
async fn production_selection_records_both_installation_outcomes() {
    let Ok(case) = std::env::var("ADK_I7651_ENTRY_CASE") else {
        for case in ["installed", "refused"] {
            child(
                case,
                concat!(
                    module_path!(),
                    "::production_selection_records_both_installation_outcomes"
                )
                .trim_start_matches(concat!(env!("CARGO_CRATE_NAME"), "::")),
            );
        }
        return;
    };
    let root = production_root();
    let mut config: crate::config::Config =
        serde_json::from_value(json!({"server":{},"cluster":{"instance_id":"test-node"}})).unwrap();
    if case == "refused" {
        config.tui_o = Some(
            serde_json::from_value(json!({"writer":{"all_tui":false,"channels":[0]}})).unwrap(),
        );
    }
    assert!(config.session_hosts.herdr.endpoints.is_empty());
    assert!(config.agents.is_empty());
    let result = crate::bootstrap::prepare_and_install_boot_snapshots(&config, None).await;
    assert_eq!(result.is_ok(), case == "installed");
    let all = records(&root, process_boot::initialize());
    assert_eq!(
        all[0]["host"], "test-node",
        "loaded config host survives either install result"
    );
    let selected: Vec<_> = all
        .iter()
        .filter(|record| record["kind"] == "selection")
        .collect();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0]["installed"], case == "installed");
}

#[tokio::test]
async fn doctor_pid_mtime_is_preserved_as_a_distinct_identity() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("runtime")).unwrap();
    std::fs::write(
        root.path().join("runtime/dcserver.pid"),
        std::process::id().to_string(),
    )
    .unwrap();
    let doctor = crate::cli::doctor::startup::current_boot_id_at(root.path()).unwrap();
    let boot = Boot {
        id: uuid::Uuid::new_v4().to_string(),
        started_at_ms: chrono::Utc::now().timestamp_millis(),
        monotonic: std::time::Instant::now(),
    };
    let snapshot = boot_journal_snapshot_for_test(root.path(), &boot).await;
    let written = header(root.path(), &boot);
    assert_eq!(written["doctor_boot_id"], doctor);
    assert_ne!(written["doctor_boot_id"], written["boot_id"]);
    assert_eq!(snapshot["journal_ready"], true);
}
