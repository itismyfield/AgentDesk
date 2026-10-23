//! The claim's config read through the real entry point on a real database: the runtime stays
//! free while the read is held or parsed, the claim uses what `load_graceful` reads, no SQL starts
//! before the read is done, and a read that panics fails the claim.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use super::{claim_after_reading, claim_task_dispatches};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::server::task_dispatch_claims::TaskDispatchClaimRequest;
use crate::services::health_diagnostics::pg_wire_proxy::PgWireProxy;

const WAIT: Duration = Duration::from_secs(10);
const OWNER: &str = "mac-book-release";
/// Ignored padding that makes the YAML parse take long enough to see where it runs.
const PADDING_LINES: usize = 60_000;

/// One online claim owner, a dispatch holding one `ue_editor` slot, and two pending dispatches
/// that need it: capacity 2 claims exactly one of them.
async fn seed(pool: &sqlx::PgPool) {
    for statement in [
        "INSERT INTO worker_nodes (
            instance_id, hostname, role, effective_role, status, labels, capabilities,
            last_heartbeat_at, started_at, updated_at
         )
         VALUES ('mac-book-release', 'mac-book', 'worker', 'worker', 'online',
                 '[\"mac-book\"]'::jsonb, '{\"providers\":[\"codex\"]}'::jsonb,
                 NOW(), NOW(), NOW())",
        "INSERT INTO agents (id, name) VALUES ('agent-1', 'Agent 1')",
        "INSERT INTO kanban_cards (id, title) VALUES ('card-1', 'Card 1')",
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, required_capabilities,
            claim_owner, claimed_at, claim_expires_at, created_at, updated_at
         )
         VALUES ('disp-holder', 'card-1', 'agent-1', 'implementation', 'dispatched', 'Holder',
                 '{\"required\":{\"semaphores\":[\"ue_editor\"]}}'::jsonb,
                 'mac-mini-release', NOW(), NOW() + INTERVAL '10 minutes', NOW(), NOW())",
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, required_capabilities,
            created_at, updated_at
         )
         VALUES ('disp-a', 'card-1', 'agent-1', 'implementation', 'pending', 'A',
                 '{\"required\":{\"semaphores\":[\"ue_editor\"]}}'::jsonb, NOW(), NOW()),
                ('disp-b', 'card-1', 'agent-1', 'implementation', 'pending', 'B',
                 '{\"required\":{\"semaphores\":[\"ue_editor\"]}}'::jsonb,
                 NOW() + INTERVAL '1 second', NOW())",
        "INSERT INTO dispatch_semaphore_holdings (
            semaphore_name, scope, scope_key, slot_index, holder_instance_id, dispatch_id,
            expires_at
         )
         VALUES ('ue_editor', 'per-cluster', 'cluster', 0, 'mac-mini-release', 'disp-holder',
                 NOW() + INTERVAL '10 minutes')",
    ] {
        sqlx::query(statement).execute(pool).await.unwrap();
    }
}

fn request() -> TaskDispatchClaimRequest {
    TaskDispatchClaimRequest {
        claim_owner: OWNER.to_string(),
        ttl_secs: Some(60),
        limit: Some(10),
        to_agent_id: None,
        dispatch_type: None,
        lease_ttl_secs: Some(60),
    }
}

/// A config whose only effective cluster setting is `ue_editor` at capacity 2, its data dir
/// under `root`, and a long ignored list.
fn config_yaml(root: &Path) -> String {
    let mut yaml = format!(
        "server:\n  port: 18799\ndata:\n  dir: {}\ncluster:\n  semaphores:\n    ue_editor:\n      capacity: 2\n      scope: per-cluster\namp_padding:\n",
        root.join("data").display()
    );
    for line in 0..PADDING_LINES {
        yaml += &format!("  - padding entry {line} for the claim config read\n");
    }
    yaml
}

fn status(statuses: &[(String, String)], id: &str) -> String {
    statuses
        .iter()
        .find(|(row, _)| row == id)
        .map(|(_, status)| status.clone())
        .unwrap_or_default()
}

async fn statuses(pool: &sqlx::PgPool) -> Vec<(String, String)> {
    sqlx::query_as("SELECT id, status FROM task_dispatches ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// What the proxy had seen when the runtime was found free: connections and statements.
type Seen = (usize, usize);

/// Writes `yaml` into the FIFO only after a task on the claim's runtime ran, or after `WAIT`;
/// returns what the proxy had seen by then. Opening never blocks, so a claim that never reads
/// the FIFO ends this thread instead of hanging it.
fn feed_after_runtime_ran(
    fifo: PathBuf,
    yaml: String,
    ran: mpsc::Receiver<()>,
    proxy: Arc<PgWireProxy>,
) -> std::thread::JoinHandle<Option<Seen>> {
    std::thread::spawn(move || {
        let seen = ran
            .recv_timeout(WAIT)
            .ok()
            .map(|()| (proxy.accepted(), proxy.executions().len()));
        let deadline = Instant::now() + WAIT;
        let mut file = loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo)
            {
                Ok(file) => break file,
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "no reader opened the FIFO: {error}"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        };
        let mut rest = yaml.as_bytes();
        while !rest.is_empty() {
            match file.write(rest) {
                Ok(written) => rest = &rest[written..],
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("write the config FIFO: {error}"),
            }
        }
        seen
    })
}

#[tokio::test]
async fn claim_config_read_yields_runtime_without_changing_snapshot() {
    // The env lock comes before the database's lifecycle lock; the FIFO is named only for the claim.
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let db = TestPostgresDb::create().await;
    let direct = db.connect_and_migrate().await;
    seed(&direct).await;
    let proxy = Arc::new(PgWireProxy::start(&db.database_url, Duration::ZERO).await);
    let pool = proxy.pool(2);
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("agentdesk.yaml");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let yaml = config_yaml(root.path());

    // This runtime has one thread: the ticker runs only while the claim's future is pending.
    let (ran_tx, ran_rx) = mpsc::channel();
    let longest = Arc::new(Mutex::new(Duration::ZERO));
    let ticker = tokio::spawn({
        let longest = Arc::clone(&longest);
        async move {
            let _ = ran_tx.send(());
            let mut last = Instant::now();
            loop {
                tokio::time::sleep(Duration::from_millis(1)).await;
                let now = Instant::now();
                let mut longest = longest.lock().unwrap();
                *longest = (*longest).max(now - last);
                last = now;
            }
        }
    });
    let writer = feed_after_runtime_ran(path.clone(), yaml.clone(), ran_rx, Arc::clone(&proxy));
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &path,
    );
    let outcome = claim_task_dispatches(&pool, &request()).await.unwrap();
    ticker.abort();
    let longest = *longest.lock().unwrap();

    // The claim used capacity 2 from the file: one more slot, then exhausted.
    let claimed: Vec<&str> = outcome.claimed.iter().map(|c| c.id.as_str()).collect();
    let skipped: Vec<&str> = outcome.skipped.iter().map(|s| s.id.as_str()).collect();
    assert_eq!((claimed, skipped), (vec!["disp-a"], vec!["disp-b"]));
    assert!(
        outcome.skipped[0]
            .reasons
            .iter()
            .any(|reason| reason.contains("exhausted")),
        "{:?}",
        outcome.skipped[0].reasons
    );
    let rows = statuses(&direct).await;
    assert_eq!(
        (status(&rows, "disp-a"), status(&rows, "disp-b")),
        ("dispatched".to_string(), "pending".to_string())
    );
    assert!(
        root.path().join("data").is_dir(),
        "the claim's read created the data dir"
    );

    let seen = writer.join().expect("the FIFO writer finishes");
    assert_eq!(
        seen,
        Some((0, 0)),
        "another task ran while the read was held, before any connection or statement"
    );

    // The same bytes read directly: the snapshot the claim used, and how long a parse takes.
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, &yaml).unwrap();
    let started = Instant::now();
    let direct_read = tokio::task::spawn_blocking(crate::config::load_graceful)
        .await
        .unwrap();
    let parse = started.elapsed();
    let mut expected = crate::config::ClusterConfig::default();
    expected.semaphores.insert(
        "ue_editor".to_string(),
        crate::config::ClusterSemaphoreConfig {
            capacity: 2,
            scope: crate::config::ClusterSemaphoreScope::PerCluster,
        },
    );
    assert_eq!(direct_read.cluster, expected);
    assert_ne!(
        crate::config_live_reload::current().map(|live| live.cluster.clone()),
        Some(expected),
        "the live snapshot differs from the file, so reading it would claim differently"
    );
    assert!(
        parse >= Duration::from_millis(150),
        "the padding must make a parse long enough to see: {parse:?}"
    );
    assert!(
        longest < parse / 2,
        "the runtime kept running through the parse: longest gap {longest:?}, parse {parse:?}"
    );

    pool.close().await;
    direct.close().await;
    db.drop().await;
}

#[tokio::test]
async fn claim_config_read_panic_fails_the_claim_before_any_sql() {
    let db = TestPostgresDb::create().await;
    let direct = db.connect_and_migrate().await;
    seed(&direct).await;
    let proxy = PgWireProxy::start(&db.database_url, Duration::ZERO).await;
    let pool = proxy.pool(2);

    let error = claim_after_reading(&pool, &request(), || panic!("the config read panicked"))
        .await
        .expect_err("a panicked read fails the claim");
    assert!(
        error.starts_with("read cluster config for task dispatch claims: "),
        "{error}"
    );
    assert_eq!(
        (proxy.accepted(), proxy.executions().len()),
        (0, 0),
        "no worker lookup or transaction started"
    );
    let rows = statuses(&direct).await;
    assert_eq!(
        (status(&rows, "disp-a"), status(&rows, "disp-b")),
        ("pending".to_string(), "pending".to_string())
    );

    pool.close().await;
    direct.close().await;
    db.drop().await;
}
