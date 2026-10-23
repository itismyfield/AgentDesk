//! The `s3` profile's claim load: dispatch claims through the real router at a fixed rate against
//! a copy of `agentdesk.example.yaml`, and optional watcher-like reads on the server's blocking pool.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::amplification_verdict_tests::{
    BlockingRead, CLAIM_RPS_KEY, CONFIG_BYTES_KEY, CONFIG_SHA_KEY, ClaimAttempt, ClaimOutcome,
    Plan, WATCHERS_KEY,
};

const CLAIM_RPS: u64 = 20;
/// Synthetic watcher count, `0` unless set; each reads about five times a second like a poll.
const WATCHERS_ENV: &str = "ADK_HEALTH_AMP_WATCHERS";
const WATCHER_PAUSE: Duration = Duration::from_millis(200);
const WATCHER_FILE_BYTES: usize = 4_096;
/// Claimable rows; each claim takes one for a second, so the same rows come round again.
const CLAIM_ROWS: usize = 400;
const CLAIM_TTL_SECS: u64 = 1;
const CLAIM_OWNER: &str = "amp-claim-node";
/// The semaphore the copied config adds: a claim that did not parse the file skips every row.
const SEMAPHORE: &str = "amp_claim";
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn watchers() -> u64 {
    std::env::var(WATCHERS_ENV).map_or(0, |value| {
        value
            .parse()
            .unwrap_or_else(|_| panic!("{WATCHERS_ENV}={value}"))
    })
}

/// The example config with every home path moved under `root` and the claim semaphore added,
/// written to `root/agentdesk.yaml`; returns its path, sha256 and size.
pub(super) fn write_config(root: &Path) -> (PathBuf, String, usize) {
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("agentdesk.example.yaml");
    let example = std::fs::read_to_string(&example).expect("read agentdesk.example.yaml");
    let home = |name: &str| root.join(name).display().to_string();
    let yaml = example
        .replace("~/.adk", &home("adk-home"))
        .replace("~/.claude", &home("claude-home"))
        .replacen(
            "\ncluster:\n",
            &format!("\ncluster:\n  semaphores:\n    {SEMAPHORE}:\n      capacity: 1024\n      scope: per-cluster\n"),
            1,
        );
    assert!(!yaml.contains("~/"), "every home path moved under the root");
    let path = root.join("agentdesk.yaml");
    std::fs::write(&path, &yaml).expect("write the config copy");
    let parsed: crate::config::Config = serde_yaml::from_str(&yaml).expect("the copy parses");
    assert!(
        parsed.data.dir.starts_with(root) && parsed.cluster.semaphores.contains_key(SEMAPHORE),
        "the copy keeps its data under the root and carries the claim semaphore"
    );
    (path, format!("{:x}", Sha256::digest(&yaml)), yaml.len())
}

/// The claim owner online for the whole run and `CLAIM_ROWS` pending dispatches needing the
/// semaphore, seeded by the claim module's own fixtures.
pub(super) async fn seed(pool: &sqlx::PgPool) {
    use crate::server::task_dispatch_claims::task_dispatch_claims_pg_tests as fixture;
    fixture::seed_worker_node(pool, CLAIM_OWNER, "amp-claim", 0).await;
    fixture::seed_agent_and_card(pool).await;
    let needs = serde_json::json!({"required": {"semaphores": [SEMAPHORE]}});
    for row in 0..CLAIM_ROWS {
        let id = format!("amp-claim-{row}");
        fixture::seed_dispatch_with_required_capabilities(
            pool,
            &id,
            "amp",
            needs.clone(),
            "pending",
        )
        .await;
    }
}

pub(super) fn conditions(config_sha: &str, config_bytes: usize) -> Vec<(&'static str, String)> {
    vec![
        (CLAIM_RPS_KEY, CLAIM_RPS.to_string()),
        ("claim_rows", CLAIM_ROWS.to_string()),
        ("claim_ttl_secs", CLAIM_TTL_SECS.to_string()),
        ("claim_limit", "1".to_string()),
        (WATCHERS_KEY, watchers().to_string()),
        ("watcher_pause_ms", WATCHER_PAUSE.as_millis().to_string()),
        ("watcher_file_bytes", WATCHER_FILE_BYTES.to_string()),
        (CONFIG_SHA_KEY, config_sha.to_string()),
        (CONFIG_BYTES_KEY, config_bytes.to_string()),
    ]
}

fn micros_since(origin: Instant) -> u64 {
    Instant::now().saturating_duration_since(origin).as_micros() as u64
}

/// Open loop like the health dispatcher: each claim waits only for its own scheduled time.
pub(super) async fn drive_claims(
    addr: SocketAddr,
    origin: Instant,
    plan: Plan,
) -> Vec<ClaimAttempt> {
    let mut attempts = Vec::new();
    let mut in_flight = Vec::new();
    for (seq, scheduled) in plan.claim_schedule(CLAIM_RPS).into_iter().enumerate() {
        tokio::time::sleep_until((origin + Duration::from_micros(scheduled)).into()).await;
        let attempt = ClaimAttempt {
            seq,
            measured: scheduled >= plan.warmup_end(),
            scheduled,
            sent: None,
            last_byte: None,
            outcome: ClaimOutcome::Unsent,
        };
        if micros_since(origin) >= plan.end() {
            attempts.push(attempt);
        } else {
            in_flight.push(tokio::spawn(claim(addr, origin, attempt)));
        }
    }
    for request in in_flight {
        attempts.push(request.await.expect("claim task"));
    }
    attempts
}

/// One claim POST on a new loopback connection, bounded by the client timeout from the send.
async fn claim(addr: SocketAddr, origin: Instant, mut attempt: ClaimAttempt) -> ClaimAttempt {
    let body = format!(
        r#"{{"claim_owner":"{CLAIM_OWNER}","ttl_secs":{CLAIM_TTL_SECS},"limit":1,"lease_ttl_secs":3600}}"#
    );
    let request = format!(
        "POST /cluster/task-dispatches/claim HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    attempt.sent = Some(micros_since(origin));
    let read = tokio::time::timeout(CLIENT_TIMEOUT, async {
        let mut stream = tokio::net::TcpStream::connect(addr).await?;
        stream.write_all(request.as_bytes()).await?;
        let (mut raw, mut last, mut chunk) = (Vec::new(), None, [0u8; 8192]);
        loop {
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                return Ok::<_, std::io::Error>((raw, last));
            }
            raw.extend_from_slice(&chunk[..read]);
            last = Some(micros_since(origin));
        }
    })
    .await;
    attempt.outcome = match read {
        Err(_) => ClaimOutcome::Timeout,
        Ok(Err(error)) => ClaimOutcome::Failed(format!("{:?}", error.kind())),
        Ok(Ok((raw, last))) => {
            attempt.last_byte = last;
            let raw = String::from_utf8_lossy(&raw);
            match raw.get(9..12).and_then(|status| status.parse().ok()) {
                Some(status) => {
                    let body = raw.split_once("\r\n\r\n").map_or("", |(_, body)| body);
                    let claimed = serde_json::from_str::<serde_json::Value>(body)
                        .ok()
                        .and_then(|json| json["claimed"].as_array().map(Vec::len));
                    ClaimOutcome::Answered { status, claimed }
                }
                None => ClaimOutcome::Failed("malformed".to_string()),
            }
        }
    };
    attempt
}

/// `watchers()` loops on the calling runtime, each reading its own small file on the blocking
/// pool, waiting for the read, then pausing, as a watcher's outer poll does.
pub(super) async fn drive_blocking(
    root: PathBuf,
    origin: Instant,
    plan: Plan,
) -> Vec<BlockingRead> {
    let count = watchers() as usize;
    let mut lanes = Vec::new();
    for watcher in 0..count {
        let file = root.join(format!("watcher-{watcher}.log"));
        std::fs::write(&file, vec![b'x'; WATCHER_FILE_BYTES]).expect("write a watcher file");
        let first = origin + WATCHER_PAUSE * watcher as u32 / count as u32;
        lanes.push(tokio::spawn(async move {
            let mut reads = Vec::new();
            tokio::time::sleep_until(first.into()).await;
            while micros_since(origin) < plan.end() {
                let submitted = micros_since(origin);
                let path = file.clone();
                let ran = tokio::task::spawn_blocking(move || {
                    let started = micros_since(origin);
                    let _ = std::fs::read(&path);
                    started
                })
                .await
                .ok();
                reads.push(BlockingRead {
                    watcher,
                    submitted,
                    started: ran,
                    finished: ran.map(|_| micros_since(origin)),
                });
                tokio::time::sleep(WATCHER_PAUSE).await;
            }
            reads
        }));
    }
    let mut reads = Vec::new();
    for lane in lanes {
        reads.extend(lane.await.expect("watcher lane"));
    }
    reads
}
