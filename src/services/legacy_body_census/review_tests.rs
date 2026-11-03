use super::*;
use std::path::Path;

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

fn head(root: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(root.join("legacy_body_census/HEAD")).unwrap()).unwrap()
}

#[test]
fn directory_sync_failure_returns_the_existing_file() {
    let root = tempfile::tempdir().unwrap();
    let boot = boot();
    let state = Arc::new(Mutex::new(State::new(&boot, Some("test-node".into()))));
    let result =
        history::prepare_with_sync(root.path(), &boot, Some("doctor".into()), &state, |_| {
            Err(std::io::Error::other("fixture directory sync failed"))
        });
    assert!(
        result.is_ok(),
        "a created journal must retain its file handle after sync failure"
    );
}

#[tokio::test]
async fn historical_invalid_is_not_republished_across_three_boots() {
    let root = tempfile::tempdir().unwrap();
    let a = boot();
    let first = open(root.path(), &a).await;
    assert!(
        first
            .append(json!({"kind":"observation_invalid","reasons":["WRITE_TIMEOUT"]}))
            .await
    );
    let b = boot();
    let second = open(root.path(), &b).await;
    assert_eq!(second.snapshot()["journal_invalid"], false);
    assert!(header(root.path(), &b)["history_problems"].as_array().unwrap().iter()
        .any(|problem| problem["boot_id"] == a.id && problem["reason"] == "HISTORY_INVALIDATED"));
    assert!(
        !records(root.path(), &b)
            .iter()
            .any(|record| record["kind"] == "observation_invalid")
    );
    let c = boot();
    let _third = open(root.path(), &c).await;
    assert!(
        !header(root.path(), &c)["history_problems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|problem| problem["boot_id"] == b.id)
    );
}

#[test]
fn a_missing_verified_manifest_row_cannot_certify_coverage() {
    let mut data: Value =
        serde_json::from_str(include_str!("../../../scripts/tui_o_body_probes.json")).unwrap();
    for row in data["probes"].as_array_mut().unwrap() {
        row["status"] = json!("VERIFIED");
    }
    for row in data["claim_probes"].as_array_mut().unwrap() {
        row["status"] = json!("VERIFIED");
    }
    assert_eq!(manifest::inspect(&data.to_string())["coverage"], false);
    assert_eq!(
        manifest::inspect(&data.to_string())["instrumentation_complete"],
        true
    );
    data["probes"].as_array_mut().unwrap().pop();
    let inspected = manifest::inspect(&data.to_string());
    assert_eq!(inspected["coverage"], false);
    assert_eq!(inspected["error"], "manifest_invalid");
}

#[test]
fn transport_manifest_requires_complete_verified_rows() {
    let mut data: Value =
        serde_json::from_str(include_str!("../../../scripts/tui_o_body_probes.json")).unwrap();
    for row in data["probes"].as_array_mut().unwrap() {
        row["status"] = json!("VERIFIED");
    }
    for row in data["claim_probes"].as_array_mut().unwrap() {
        row["status"] = json!("VERIFIED");
    }
    for case in ["pending", "verified", "missing", "incomplete"] {
        let mut current = data.clone();
        for row in current["transport_probes"].as_array_mut().unwrap() {
            row["status"] = json!(if case == "pending" {
                "PENDING_0a3"
            } else {
                "VERIFIED"
            });
        }
        if case == "missing" {
            current.as_object_mut().unwrap().remove("transport_probes");
        }
        if case == "incomplete" {
            current["transport_probes"].as_array_mut().unwrap().pop();
        }
        let observed = manifest::inspect(&current.to_string());
        assert_eq!(observed["coverage"], false, "{case}");
        assert_eq!(
            observed["instrumentation_complete"],
            case == "verified",
            "{case}"
        );
        assert_eq!(
            observed["error"].is_string(),
            matches!(case, "missing" | "incomplete"),
            "{case}"
        );
        if case == "pending" {
            assert_eq!(
                observed["pending"],
                data["transport_probes"].as_array().unwrap().len()
            );
        }
    }
}

#[tokio::test]
async fn head_is_the_only_predecessor_authority_even_when_wall_time_moves_backwards() {
    let root = tempfile::tempdir().unwrap();
    let a = boot();
    let _first = open(root.path(), &a).await;
    assert_eq!(head(root.path())["boot_id"], a.id);
    let orphan = boot();
    let mut unwitnessed = header(root.path(), &a);
    unwitnessed["boot_id"] = json!(orphan.id);
    unwitnessed["started_at_ms"] = json!(a.started_at_ms + 10_000);
    let orphan_path = root
        .path()
        .join("legacy_body_census")
        .join(format!("{}.jsonl", orphan.id));
    std::fs::write(&orphan_path, format!("{unwitnessed}\n")).unwrap();
    std::fs::write(
        root.path().join("legacy_body_census/unrelated.jsonl"),
        "damaged",
    )
    .unwrap();
    let mut c = boot();
    c.started_at_ms = a.started_at_ms - 10;
    let current = open(root.path(), &c).await;
    assert_eq!(header(root.path(), &c)["predecessor_boot_id"], a.id);
    assert_eq!(current.snapshot()["journal_invalid"], false);
    assert_eq!(head(root.path())["boot_id"], c.id);
    assert!(orphan_path.exists());
}

#[tokio::test]
async fn a_missing_or_damaged_head_never_creates_a_normal_root() {
    for malformed in [
        None,
        Some("{"),
        Some("{\"boot_id\":\"../../outside\",\"header_digest\":\"wrong\"}"),
    ] {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("legacy_body_census")).unwrap();
        if let Some(raw) = malformed {
            std::fs::write(root.path().join("legacy_body_census/HEAD"), raw).unwrap();
        }
        let id = boot();
        let current = open(root.path(), &id).await;
        let written = header(root.path(), &id);
        assert_eq!(current.snapshot()["status"], "INCONCLUSIVE");
        assert!(
            current.snapshot()["reasons"]
                .as_array()
                .unwrap()
                .contains(&json!("PREDECESSOR_UNKNOWN"))
        );
        assert_eq!(written["root_reason"], "PREDECESSOR_UNKNOWN");
        assert!(written["coverage_start"].is_null());
        assert!(written["predecessor_boot_id"].is_null());
    }
}

#[tokio::test]
async fn concurrent_boots_follow_head_publication_order_without_losing_a_boot() {
    let root = tempfile::tempdir().unwrap();
    let a = boot();
    let _first = open(root.path(), &a).await;
    let mut b = boot();
    b.started_at_ms = a.started_at_ms - 10;
    let mut c = boot();
    c.started_at_ms = a.started_at_ms + 10;
    let (_b, _c) = tokio::join!(open(root.path(), &b), open(root.path(), &c));
    let current = head(root.path());
    let (last, before) = if current["boot_id"] == b.id {
        (&b, &c)
    } else {
        assert_eq!(current["boot_id"], c.id);
        (&c, &b)
    };
    assert_eq!(header(root.path(), last)["predecessor_boot_id"], before.id);
    assert_eq!(header(root.path(), before)["predecessor_boot_id"], a.id);
    use sha2::{Digest, Sha256};
    let line = serde_json::to_vec(&header(root.path(), last)).unwrap();
    let mut encoded = line;
    encoded.push(b'\n');
    assert_eq!(
        current["header_digest"],
        hex::encode(Sha256::digest(&encoded))
    );
}

#[tokio::test]
async fn own_invalid_reasons_are_persisted_once_until_a_new_reason_arrives() {
    let root = tempfile::tempdir().unwrap();
    let id = boot();
    let current = open(root.path(), &id).await;
    invalidate(&current.0.state, "WRITE_TIMEOUT");
    assert!(current.append(json!({"kind":"checkpoint"})).await);
    assert!(current.append(json!({"kind":"checkpoint"})).await);
    assert_eq!(
        records(root.path(), &id)
            .iter()
            .filter(|r| r["kind"] == "observation_invalid")
            .count(),
        1
    );
    invalidate(&current.0.state, "WRITE_FAILED");
    assert!(current.append(json!({"kind":"checkpoint"})).await);
    assert_eq!(
        records(root.path(), &id)
            .iter()
            .filter(|r| r["kind"] == "observation_invalid")
            .count(),
        2
    );
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
            .trim_start_matches("agentdesk::"),
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
                .trim_start_matches("agentdesk::"),
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
async fn a_foreign_host_header_cannot_become_the_predecessor() {
    use sha2::{Digest, Sha256};
    let root = tempfile::tempdir().unwrap();
    let a = boot();
    let _first = open(root.path(), &a).await;
    let mut altered = records(root.path(), &a);
    altered[0]["host"] = json!("other-node");
    let raw = altered
        .iter()
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    std::fs::write(
        root.path()
            .join("legacy_body_census")
            .join(format!("{}.jsonl", a.id)),
        raw,
    )
    .unwrap();
    let pointer = json!({"boot_id":a.id,"header_digest":hex::encode(Sha256::digest(format!("{}\n", altered[0]).as_bytes()))});
    std::fs::write(
        root.path().join("legacy_body_census/HEAD"),
        pointer.to_string(),
    )
    .unwrap();
    let b = boot();
    let current = open(root.path(), &b).await;
    assert!(header(root.path(), &b)["predecessor_boot_id"].is_null());
    assert!(
        current.snapshot()["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("HISTORY_HOST_OR_ROLE_MISMATCH"))
    );
    assert_eq!(
        header(root.path(), &b)["root_reason"],
        "PREDECESSOR_UNKNOWN"
    );
}

#[tokio::test]
async fn head_digest_mismatch_does_not_reset_coverage_to_a_fresh_root() {
    let root = tempfile::tempdir().unwrap();
    let a = boot();
    let _first = open(root.path(), &a).await;
    let mut pointer = head(root.path());
    pointer["header_digest"] = json!("0".repeat(64));
    std::fs::write(
        root.path().join("legacy_body_census/HEAD"),
        pointer.to_string(),
    )
    .unwrap();
    let b = boot();
    let current = open(root.path(), &b).await;
    assert_eq!(current.snapshot()["status"], "INCONCLUSIVE");
    assert!(
        current.snapshot()["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("HISTORY_HEADER_DIGEST_MISMATCH"))
    );
    assert_eq!(
        header(root.path(), &b)["root_reason"],
        "PREDECESSOR_UNKNOWN"
    );
    assert!(header(root.path(), &b)["coverage_start"].is_null());
}

#[tokio::test]
async fn a_failed_head_publish_keeps_the_boot_and_its_own_invalid_record() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("legacy_body_census/HEAD")).unwrap();
    let id = boot();
    let current = open(root.path(), &id).await;
    assert_eq!(current.snapshot()["journal_ready"], false);
    assert_eq!(current.snapshot()["journal_invalid"], true);
    assert!(records(root.path(), &id).iter().any(|row| {
        row["kind"] == "observation_invalid"
            && row["reasons"]
                .as_array()
                .unwrap()
                .contains(&json!("HEAD_WRITE_FAILED"))
    }));
}

#[tokio::test]
async fn a_directory_sync_failure_is_written_to_the_created_journal() {
    let root = tempfile::tempdir().unwrap();
    let id = boot();
    let current = Journal(writer::Writer::spawn(
        Some(root.path().into()),
        id.clone(),
        writer::Options {
            doctor: Some("doctor".into()),
            host: Some("test-node".into()),
            sync_parent: Some(|_| Err(std::io::Error::other("fixture sync failed"))),
            ..Default::default()
        },
    ));
    assert!(current.append(json!({"kind":"boot_ready"})).await);
    assert_eq!(current.snapshot()["journal_invalid"], true);
    assert!(records(root.path(), &id).iter().any(|row| {
        row["kind"] == "observation_invalid"
            && row["reasons"]
                .as_array()
                .unwrap()
                .contains(&json!("DIRECTORY_SYNC_FAILED"))
    }));
}

#[test]
fn a_record_is_encoded_before_one_write_all() {
    #[derive(Default)]
    struct Counting {
        bytes: Vec<u8>,
        writes: usize,
    }
    impl std::io::Write for Counting {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.writes += 1;
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Counting::default();
    history::append_line(&mut output, &json!({"kind":"checkpoint","value":"a\nb"})).unwrap();
    assert_eq!(output.writes, 1);
    assert_eq!(output.bytes.last(), Some(&b'\n'));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.bytes).unwrap()["value"],
        "a\nb"
    );
}

#[tokio::test]
async fn header_schema_and_manifest_error_are_explicit() {
    let root = tempfile::tempdir().unwrap();
    let id = boot();
    let _current = open(root.path(), &id).await;
    let written = header(root.path(), &id);
    assert_eq!(written["observation_policy_version"], 1);
    assert_eq!(written["host"], "test-node");
    assert_eq!(written["process_role"], "server");
    assert!(written.get("manifest_error").is_some());
    assert!(written["manifest_error"].is_null());
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_head_is_replaced_without_reading_its_target() {
    let root = tempfile::tempdir().unwrap();
    let external = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(external.path(), "not evidence").unwrap();
    std::fs::create_dir(root.path().join("legacy_body_census")).unwrap();
    std::os::unix::fs::symlink(external.path(), root.path().join("legacy_body_census/HEAD"))
        .unwrap();
    let id = boot();
    let current = open(root.path(), &id).await;
    assert_eq!(current.snapshot()["status"], "INCONCLUSIVE");
    assert_eq!(
        header(root.path(), &id)["root_reason"],
        "PREDECESSOR_UNKNOWN"
    );
    assert_eq!(
        std::fs::read_to_string(external.path()).unwrap(),
        "not evidence"
    );
    assert_eq!(head(root.path())["boot_id"], id.id);
}

struct SyncBlock {
    path: PathBuf,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<Barrier>,
}
static SYNC_BLOCK: Mutex<Option<SyncBlock>> = Mutex::new(None);

fn held_sync(path: &Path) -> std::io::Result<()> {
    let barrier = SYNC_BLOCK
        .lock()
        .unwrap()
        .as_ref()
        .filter(|block| block.path == path)
        .map(|block| (block.entered.clone(), block.release.clone()));
    if let Some((entered, release)) = barrier {
        entered.notify_one();
        release.wait();
    }
    crate::services::discord::runtime_store::fsync_parent_dir(path)
}

#[tokio::test]
async fn head_publication_holds_the_boot_lock() {
    let root = tempfile::tempdir().unwrap();
    let id = boot();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(Barrier::new(2));
    *SYNC_BLOCK.lock().unwrap() = Some(SyncBlock {
        path: root
            .path()
            .join("legacy_body_census")
            .join(format!("{}.jsonl", id.id)),
        entered: entered.clone(),
        release: release.clone(),
    });
    let current = Journal(writer::Writer::spawn(
        Some(root.path().into()),
        id,
        writer::Options {
            doctor: Some("doctor".into()),
            host: Some("test-node".into()),
            sync_parent: Some(held_sync),
            ..Default::default()
        },
    ));
    current.0.try_append(json!({"kind":"boot_ready"}));
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("writer reached header sync");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.path().join("legacy_body_census/writer.lock"))
        .unwrap();
    let unavailable = lock.try_lock().is_err();
    drop(lock);
    release.wait();
    *SYNC_BLOCK.lock().unwrap() = None;
    assert!(current.append(json!({"kind":"boot_ready"})).await);
    assert!(
        unavailable,
        "HEAD publication ran without its cross-process boot lock"
    );
    assert_eq!(current.snapshot()["journal_ready"], true);
}

#[test]
fn claim_manifest_requires_complete_verified_rows() {
    let mut data: Value =
        serde_json::from_str(include_str!("../../../scripts/tui_o_body_probes.json")).unwrap();
    for family in ["probes", "transport_probes"] {
        for row in data[family].as_array_mut().unwrap() {
            row["status"] = json!("VERIFIED");
        }
    }
    for case in [
        "pending",
        "verified",
        "missing",
        "incomplete",
        "duplicate",
        "unknown",
        "extra_gate_field",
        "extra_adoption_field",
        "extra_site_field",
    ] {
        let mut current = data.clone();
        for row in current["claim_probes"].as_array_mut().unwrap() {
            row["status"] = json!(if case == "pending" {
                "PENDING_0a2"
            } else {
                "VERIFIED"
            });
        }
        match case {
            "missing" => {
                current.as_object_mut().unwrap().remove("claim_probes");
            }
            "incomplete" => {
                current["claim_probes"].as_array_mut().unwrap().pop();
            }
            "duplicate" => {
                current["claim_probes"][1] = current["claim_probes"][0].clone();
            }
            "unknown" => {
                current["claim_probes"][0]["status"] = json!("UNRECOGNIZED");
            }
            "extra_gate_field" | "extra_adoption_field" | "extra_site_field" => {
                let index = match case {
                    "extra_gate_field" => 0,
                    "extra_adoption_field" => 1,
                    _ => 2,
                };
                current["claim_probes"][index]["unexpected"] = json!("unreviewed");
            }
            _ => {}
        }
        let result = manifest::inspect(&current.to_string());
        assert_eq!(result["coverage"], false, "{case}");
        assert_eq!(
            result["instrumentation_complete"],
            case == "verified",
            "{case}"
        );
        assert_eq!(
            result["error"].is_string(),
            !matches!(case, "pending" | "verified"),
            "{case}"
        );
        if case == "pending" {
            assert_eq!(result["pending"], 3);
        }
        if case.starts_with("extra_") {
            assert_eq!(result["error"], "manifest_invalid", "{case}");
        }
    }
}

#[tokio::test]
async fn a_durable_cut_requires_its_boot_and_a_live_owner_or_final_drain() {
    let root = tempfile::tempdir().unwrap();
    let id = boot();
    let current = open(root.path(), &id).await;
    assert!(current.append(json!({"kind":"checkpoint"})).await);
    let shared = current.0.state.clone();
    {
        let state = shared.lock().unwrap();
        for candidate in ["PASS", "FAIL"] {
            assert_eq!(state.window.verdict(Some(&id.id), candidate), candidate);
            assert_eq!(state.window.verdict(None, candidate), "INCONCLUSIVE");
            assert_eq!(
                state.window.verdict(Some("other-boot"), candidate),
                "INCONCLUSIVE"
            );
        }
    }
    drop(current);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !shared.lock().unwrap().window.closed {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let rows = records(root.path(), &id);
    for case in [
        "valid",
        "foreign_boot",
        "wrong_cut",
        "later_record",
        "missing_drain",
    ] {
        let mut altered = rows.clone();
        match case {
            "foreign_boot" => {
                for row in altered.iter_mut().filter(|r| r["kind"] == "drain") {
                    row["boot_id"] = json!(uuid::Uuid::new_v4().to_string());
                }
            }
            "wrong_cut" => altered.last_mut().unwrap()["cut"] = json!(900),
            "later_record" => altered.push(json!({"kind":"checkpoint"})),
            "missing_drain" => altered.retain(|r| r["kind"] != "drain"),
            _ => {}
        }
        let mut window = crate::services::legacy_body_census::window::Window::new(id.id.clone());
        for row in altered.iter().skip(1) {
            window.observe(row);
        }
        for candidate in ["PASS", "FAIL"] {
            assert_eq!(
                window.verdict(None, candidate),
                if case == "valid" {
                    candidate
                } else {
                    "INCONCLUSIVE"
                },
                "{case}"
            );
        }
    }
    let next = open(root.path(), &boot()).await;
    assert_eq!(next.snapshot()["predecessor_window"]["status"], "COMPLETE");
    assert_eq!(next.snapshot()["predecessor_window"]["boot_id"], id.id);
}

#[tokio::test]
async fn a_new_enqueue_cannot_reuse_the_previous_drain() {
    let root = tempfile::tempdir().unwrap();
    let id = boot();
    let current = open(root.path(), &id).await;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.path().join("legacy_body_census/writer.lock"))
        .unwrap();
    lock.lock().unwrap();
    current.0.try_append(json!({"kind":"checkpoint"}));
    {
        let state = current.0.state.lock().unwrap();
        assert_eq!(state.window.verdict(Some(&id.id), "PASS"), "INCONCLUSIVE");
        assert_ne!(Some(state.window.cut), state.window.drained);
    }
    drop(lock);
    assert!(current.append(json!({"kind":"checkpoint"})).await);
    let state = current.0.state.lock().unwrap();
    assert_eq!(state.window.verdict(Some(&id.id), "PASS"), "PASS");
}
