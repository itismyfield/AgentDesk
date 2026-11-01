use super::*;
use crate::services::process_boot::{self, Boot};
use std::sync::{
    Barrier,
    atomic::{AtomicBool, Ordering},
};

fn boot() -> Boot {
    Boot {
        id: uuid::Uuid::new_v4().to_string(),
        started_at_ms: chrono::Utc::now().timestamp_millis(),
        monotonic: std::time::Instant::now(),
    }
}

async fn open(root: &std::path::Path, boot: &Boot) -> Journal {
    Journal::open(
        root,
        boot,
        Some("doctor-123-456".into()),
        Duration::from_secs(2),
        Arc::new(|| {}),
    )
    .await
}

fn header(root: &std::path::Path, boot: &Boot) -> Value {
    let raw = std::fs::read_to_string(
        root.join("legacy_body_census")
            .join(format!("{}.jsonl", boot.id)),
    )
    .unwrap();
    serde_json::from_str(raw.lines().next().unwrap()).unwrap()
}

#[tokio::test]
async fn eager_identity_is_shared_with_header_before_any_sample() {
    let root = tempfile::tempdir().unwrap();
    let boot = process_boot::initialize().clone();
    let before = chrono::Utc::now().timestamp_millis();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let journal = open(root.path(), &boot).await;
    let written = header(root.path(), &boot);
    assert_eq!(written["boot_id"], boot.id);
    assert_eq!(written["doctor_boot_id"], "doctor-123-456");
    assert_eq!(written["started_at_ms"], boot.started_at_ms);
    assert!(written["started_at_ms"].as_i64().unwrap() <= before);
    assert_eq!(written["census_hash"].as_str().unwrap().len(), 64);
    assert_eq!(journal.snapshot()["status"], "INCONCLUSIVE");
    assert_eq!(written["probe_coverage"], false);
}

#[tokio::test]
async fn predecessor_links_a_boot_that_had_no_samples_or_end() {
    let root = tempfile::tempdir().unwrap();
    let first = boot();
    let old = open(root.path(), &first).await;
    assert_eq!(old.snapshot()["status"], "INCONCLUSIVE");
    assert!(
        old.snapshot()["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("PREDECESSOR_MISSING"))
    );
    let mut second = boot();
    second.started_at_ms = first.started_at_ms + 1;
    let new = open(root.path(), &second).await;
    assert_eq!(
        header(root.path(), &second)["predecessor_boot_id"],
        first.id
    );
    assert_eq!(new.snapshot()["journal_ready"], true);
    assert!(
        !new.snapshot()["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("PREDECESSOR_MISSING"))
    );
}

#[tokio::test]
async fn headerless_or_truncated_history_never_becomes_empty_success() {
    for malformed in [
        b"{\"kind\":\"checkpoint\"}\n".as_slice(),
        b"{\"kind\":\"header\"",
    ] {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("legacy_body_census");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{}.jsonl", uuid::Uuid::new_v4())),
            malformed,
        )
        .unwrap();
        let journal = open(root.path(), &boot()).await;
        let state = journal.snapshot();
        assert_eq!(state["status"], "INCONCLUSIVE");
        assert_eq!(state["journal_invalid"], true);
        assert!(
            state["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r.as_str().unwrap().starts_with("HISTORY_"))
        );
    }
}

#[tokio::test]
async fn collection_failure_is_inconclusive_and_does_not_block_continuation() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("legacy_body_census"), "not a directory").unwrap();
    let journal = open(root.path(), &boot()).await;
    assert!(!journal.append(json!({"kind":"checkpoint"})).await);
    assert_eq!(journal.snapshot()["status"], "INCONCLUSIVE");
    assert_eq!(journal.snapshot()["journal_invalid"], true);
}

#[tokio::test]
async fn blocked_writer_times_out_without_stalling_the_async_runtime() {
    let root = tempfile::tempdir().unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(Barrier::new(2));
    let reached = Arc::new(AtomicBool::new(false));
    let hook = {
        let (armed, entered, release, reached) = (
            armed.clone(),
            entered.clone(),
            release.clone(),
            reached.clone(),
        );
        Arc::new(move || {
            if armed.load(Ordering::SeqCst) {
                reached.store(true, Ordering::SeqCst);
                entered.notify_one();
                release.wait();
            }
        }) as Arc<dyn Fn() + Send + Sync>
    };
    let journal = Journal::open(
        root.path(),
        &boot(),
        Some("doctor".into()),
        Duration::from_millis(40),
        hook,
    )
    .await;
    armed.store(true, Ordering::SeqCst);
    let continuation = AtomicBool::new(false);
    let append = async {
        let ack = journal.append(json!({"kind":"checkpoint"})).await;
        continuation.store(true, Ordering::SeqCst);
        ack
    };
    let outcome = tokio::time::timeout(Duration::from_secs(1), async {
        let (ack, ()) = tokio::join!(append, async {
            entered.notified().await;
            tokio::time::sleep(Duration::from_millis(1)).await;
        });
        ack
    })
    .await;
    // Release even a timeout-removal mutant before making an assertion.
    armed.store(false, Ordering::SeqCst);
    if reached.load(Ordering::SeqCst) {
        release.wait();
    }
    assert_eq!(outcome, Ok(false));
    assert!(continuation.load(Ordering::SeqCst));
    assert_eq!(journal.snapshot()["journal_invalid"], true);
    assert!(
        journal.snapshot()["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("WRITE_TIMEOUT"))
    );
}

#[test]
fn manifest_coverage_and_hash_follow_the_same_embedded_bytes() {
    let raw = include_str!("../../../scripts/tui_o_body_probes.json");
    let actual = manifest::inspect(raw);
    assert_eq!(actual["coverage"], false);
    let mut data: Value = serde_json::from_str(raw).unwrap();
    for row in data["probes"].as_array_mut().unwrap() {
        row["status"] = json!("VERIFIED");
    }
    let complete = manifest::inspect(&serde_json::to_string(&data).unwrap());
    assert_eq!(complete["coverage"], true);
    assert_ne!(actual["census_hash"], complete["census_hash"]);
    data["probes"][0]["status"] = json!("PENDING_0b1");
    assert_eq!(manifest::inspect(&data.to_string())["coverage"], false);
    assert_eq!(manifest::inspect("{}")["coverage"], false);
    assert_eq!(manifest::inspect("not json")["coverage"], false);
}

#[test]
fn journal_initialization_is_pinned_before_recovery_and_drain() {
    let source = |path: &str, compiled: &str| {
        assert!(!compiled.is_empty());
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
            .unwrap()
    };
    let lib = source("src/lib.rs", include_str!("../../lib.rs"));
    let entry = lib.split("pub fn run_from_args()").nth(1).unwrap();
    assert!(
        entry.find("process_boot::initialize()").unwrap()
            < entry.find("run_failure_marker_worker_from_env").unwrap()
    );
    let dcserver = source("src/cli/dcserver.rs", include_str!("../../cli/dcserver.rs"));
    let pid = dcserver.find("// Write PID/version files").unwrap();
    let later = &dcserver[pid..];
    assert!(
        later.find("dcserver.pid").unwrap() < later.find("legacy_body_census::start()").unwrap()
    );
    assert!(
        later.find("legacy_body_census::start()").unwrap()
            < later.find("tokio::runtime::Runtime::new()").unwrap()
    );
    let bootstrap = source("src/bootstrap.rs", include_str!("../../bootstrap.rs"));
    assert!(
        bootstrap.find("legacy_body_census::start()").unwrap()
            < bootstrap.find("load_runtime_config").unwrap()
    );
    let runtime_source = source(
        "src/services/discord/runtime_bootstrap.rs",
        include_str!("../discord/runtime_bootstrap.rs"),
    );
    let runtime = runtime_source
        .split("pub(crate) async fn run_bot(")
        .nth(1)
        .unwrap();
    let ready = runtime
        .find("legacy_body_census::ensure_ready().await")
        .unwrap();
    for token in [
        "run_bot_build_shared_data(",
        "reap_inflight_rows_at_boot_blocking(",
        "spawn_tui_prompt_relay(",
        "run_bot_spawn_execution_runtime_tasks(",
        "o_writer_host::spawn_execution(",
    ] {
        assert!(ready < runtime.find(token).unwrap(), "{token}");
    }
    let drain_source = source(
        "src/server/outbox_worker.rs",
        include_str!("../../server/outbox_worker.rs"),
    );
    let drain = drain_source
        .split("pub(crate) async fn drain_message_outbox_once(")
        .nth(1)
        .unwrap();
    assert!(
        drain
            .find("legacy_body_census::ensure_ready().await")
            .unwrap()
            < drain.find("drain_message_outbox_batch_once(").unwrap()
    );
}

pub(crate) fn mutant(name: &str) -> bool {
    std::env::var("ADK_I7651_MUTANT").ok().as_deref() == Some(name)
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
    let boot = boot();
    let journal = Journal::open(
        root.path(),
        &boot,
        None,
        Duration::from_secs(2),
        Arc::new(|| {}),
    )
    .await;
    let written = header(root.path(), &boot);
    assert_eq!(written["doctor_boot_id"], doctor);
    assert_ne!(written["doctor_boot_id"], written["boot_id"]);
    assert_eq!(journal.snapshot()["journal_ready"], true);
}

#[tokio::test]
async fn unsampled_boot_child() {
    assert!(uuid::Uuid::parse_str(&process_boot::initialize().id).is_ok());
    let Ok(root) = std::env::var("ADK_I7651_CHILD_ROOT") else {
        return;
    };
    let _journal = open(std::path::Path::new(&root), process_boot::initialize()).await;
    std::process::exit(0);
}

#[test]
fn two_processes_keep_unsampled_crash_boots_and_their_incarnations() {
    let root = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                concat!(module_path!(), "::unsampled_boot_child").trim_start_matches("agentdesk::"),
                "--nocapture",
            ])
            .env("ADK_I7651_CHILD_ROOT", root.path())
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
    let mut headers: Vec<Value> = std::fs::read_dir(root.path().join("legacy_body_census"))
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "jsonl"))
        .map(|entry| {
            let raw = std::fs::read_to_string(entry.path()).unwrap();
            serde_json::from_str(raw.lines().next().unwrap()).unwrap()
        })
        .collect();
    headers.sort_by_key(|h| h["started_at_ms"].as_i64());
    assert_eq!(headers.len(), 2);
    assert_ne!(headers[0]["boot_id"], headers[1]["boot_id"]);
    assert_eq!(headers[1]["predecessor_boot_id"], headers[0]["boot_id"]);
}

#[tokio::test]
async fn dangling_or_cyclic_predecessors_are_inconclusive() {
    for cyclic in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let first = boot();
        let _old = open(root.path(), &first).await;
        let mut old_header = header(root.path(), &first);
        old_header["predecessor_boot_id"] = json!(if cyclic {
            first.id.clone()
        } else {
            uuid::Uuid::new_v4().to_string()
        });
        std::fs::write(
            root.path()
                .join("legacy_body_census")
                .join(format!("{}.jsonl", first.id)),
            format!("{old_header}\n"),
        )
        .unwrap();
        let new = open(root.path(), &boot()).await;
        let state = new.snapshot();
        assert_eq!(state["status"], "INCONCLUSIVE");
        assert_eq!(state["journal_invalid"], true);
        assert!(
            state["reasons"]
                .as_array()
                .unwrap()
                .contains(&json!("HISTORY_CHAIN_INVALID"))
        );
    }
}
