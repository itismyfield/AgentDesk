use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    FAILURE_MARKER_WORKER_ENV, HookRelayFailureMarker, HookRelayFailureMarkerWriteRequest,
    NON_WAIT_RELAY_WORKER_ENV, failure_marker_dir, marker_component,
    relay_hook_event_response_with_request_timeout, relay_hook_event_with_request,
    write_hook_relay_failure_marker,
};
use crate::services::claude_tui::hook_server::relay_receipts::{DELIVERY_TTL, LEDGER_RETENTION};
#[cfg(test)]
use crate::services::claude_tui::hook_server::relay_receipts::{
    RELAY_DEADLINE_HEADER, RELAY_PUBLISHED_AT_HEADER, RELAY_REQUEST_ID_HEADER,
};

const RELAY_QUEUE_IDLE_GRACE: Duration = Duration::from_millis(250);
const RELAY_RESPONSE_POLL_INTERVAL: Duration = Duration::from_millis(5);
const RELAY_IN_FLIGHT_RETRY_INTERVAL: Duration = Duration::from_millis(25);
const RELAY_RECOVERY_SCAN_INTERVAL: Duration = Duration::from_millis(100);
const RELAY_RECOVERY_REPROBE: Duration = Duration::from_millis(250);
const MAX_ACTIVE_QUEUE_FILES: usize = 4096;
const MAX_QUARANTINE_FILES: usize = 256;
const MAX_ORPHAN_RESPONSE_FILES: usize = 1024;

#[cfg(test)]
fn behavior_mutation_enabled(name: &str) -> bool {
    std::env::var("AGENTDESK_HOOK_RELAY_TEST_MUTATION").is_ok_and(|value| value == name)
}

#[derive(Debug, Serialize, Deserialize)]
struct OrderedHookRelayRequest {
    request_id: String,
    published_at: DateTime<Utc>,
    delivery_deadline: DateTime<Utc>,
    endpoint: String,
    provider: String,
    event: String,
    session_id: String,
    payload: Value,
    marker_dir: PathBuf,
    response: Option<OrderedHookRelayResponseTarget>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OrderedHookRelayResponseTarget {
    path: PathBuf,
    timeout_millis: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct OrderedHookRelayResponse {
    result: Result<Value, String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OrderedHookRelayWorkerRequest {
    queue_dir: PathBuf,
}

#[derive(Debug, Serialize)]
struct OrderedHookRelayQuarantineEvidence {
    original_path: PathBuf,
    quarantined_path: PathBuf,
    error: String,
    recorded_at: DateTime<Utc>,
}

fn relay_queue_subdir(provider: &str) -> String {
    let provider = marker_component(&provider.trim().to_ascii_lowercase());
    format!("runtime/{provider}_tui_hook_relay_queue")
}

pub(super) fn relay_queue_dir(provider: &str, session_id: &str) -> Option<PathBuf> {
    let session_key = blake3::hash(session_id.as_bytes()).to_hex().to_string();
    crate::config::runtime_root()
        .map(|root| root.join(relay_queue_subdir(provider)).join(session_key))
}

struct RelayQueueFileLock {
    file: std::fs::File,
}

impl Drop for RelayQueueFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn lock_relay_queue_file(
    path: &Path,
    nonblocking: bool,
) -> Result<Option<RelayQueueFileLock>, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("create hook relay queue dir {}: {err}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|err| format!("open hook relay queue lock {}: {err}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let operation = libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 };
        if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 {
            let error = std::io::Error::last_os_error();
            if nonblocking
                && matches!(
                    error.raw_os_error(),
                    Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
                )
            {
                return Ok(None);
            }
            return Err(format!("lock hook relay queue {}: {error}", path.display()));
        }
        Ok(Some(RelayQueueFileLock { file }))
    }
    #[cfg(not(unix))]
    {
        let _ = (file, nonblocking);
        Err("ordered hook relay queue locking is unsupported on this platform".to_string())
    }
}

fn publish_atomic_file(path: &Path, bytes: &[u8], label: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{label} path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|err| format!("create {label} dir {}: {err}", parent.display()))?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("relay");
    let temp_path = parent.join(format!(".{filename}.tmp.{}", uuid::Uuid::new_v4().simple()));
    std::fs::write(&temp_path, bytes)
        .map_err(|err| format!("write {label} temp {}: {err}", temp_path.display()))?;
    std::fs::rename(&temp_path, path).map_err(|err| {
        let _ = std::fs::remove_file(&temp_path);
        format!("publish {label} {}: {err}", path.display())
    })
}

fn queue_request_paths(queue_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = std::fs::read_dir(queue_dir)
        .map_err(|err| format!("read hook relay queue {}: {err}", queue_dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".request.json"))
        })
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn queue_ingress_paths(queue_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let ingress_dir = queue_dir.join("ingress");
    let entries = match std::fs::read_dir(&ingress_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "read hook relay ingress {}: {error}",
                ingress_dir.display()
            ));
        }
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".ingress.json"))
        })
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn sequence_from_embedded_filename(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    name.as_bytes()
        .windows(21)
        .filter_map(|window| {
            (window[20] == b'-' && window[..20].iter().all(u8::is_ascii_digit))
                .then(|| std::str::from_utf8(&window[..20]).ok()?.parse::<u64>().ok())
                .flatten()
        })
        .max()
}

fn read_high_water(path: &Path) -> Result<Option<u64>, String> {
    match std::fs::read_to_string(path) {
        Ok(value) => value
            .trim()
            .parse::<u64>()
            .map(Some)
            .map_err(|error| format!("parse hook relay high-water {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "read hook relay high-water {}: {error}",
            path.display()
        )),
    }
}

fn read_high_water_or_quarantine(queue_dir: &Path, path: &Path) -> Result<Option<u64>, String> {
    match read_high_water(path) {
        Ok(value) => Ok(value),
        Err(error) if path.exists() => {
            quarantine_path(queue_dir, path, &error)?;
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn quarantine_path(queue_dir: &Path, path: &Path, error: &str) -> Result<PathBuf, String> {
    let quarantine_dir = queue_dir.join("quarantine");
    std::fs::create_dir_all(&quarantine_dir).map_err(|err| {
        format!(
            "create hook relay quarantine {}: {err}",
            quarantine_dir.display()
        )
    })?;
    let original_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown");
    #[cfg(test)]
    let original_name = if behavior_mutation_enabled("quarantine-filename-loss") {
        "unknown"
    } else {
        original_name
    };
    let quarantined_path = quarantine_dir.join(format!(
        "{}-{original_name}.corrupt",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::rename(path, &quarantined_path).map_err(|err| {
        format!(
            "quarantine hook relay file {} -> {}: {err}",
            path.display(),
            quarantined_path.display()
        )
    })?;
    let evidence = OrderedHookRelayQuarantineEvidence {
        original_path: path.to_path_buf(),
        quarantined_path: quarantined_path.clone(),
        error: error.to_string(),
        recorded_at: Utc::now(),
    };
    let evidence_path = quarantine_dir.join(format!(
        "{}.evidence.json",
        quarantined_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("quarantine")
    ));
    let encoded = serde_json::to_vec(&evidence)
        .map_err(|err| format!("serialize hook relay quarantine evidence: {err}"))?;
    publish_atomic_file(&evidence_path, &encoded, "hook relay quarantine evidence")?;
    Ok(quarantined_path)
}

fn scan_sequence_high_water(queue_dir: &Path) -> Result<u64, String> {
    let completed_path = queue_dir.join("completed-high-water");
    let mut high_water = read_high_water_or_quarantine(queue_dir, &completed_path)?.unwrap_or(0);
    for path in queue_request_paths(queue_dir)? {
        high_water = high_water.max(sequence_from_embedded_filename(&path).unwrap_or(0));
    }
    let quarantine_dir = queue_dir.join("quarantine");
    match std::fs::read_dir(&quarantine_dir) {
        Ok(entries) => {
            for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
                high_water = high_water.max(sequence_from_embedded_filename(&path).unwrap_or(0));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "read hook relay quarantine {}: {error}",
                quarantine_dir.display()
            ));
        }
    }
    Ok(high_water)
}

fn next_relay_queue_sequence(queue_dir: &Path) -> Result<u64, String> {
    let counter_path = queue_dir.join("next-sequence");
    let current = match std::fs::read_to_string(&counter_path) {
        Ok(value) => match value.trim().parse::<u64>() {
            Ok(value) => value.max(scan_sequence_high_water(queue_dir)?),
            Err(error) => {
                let message = format!(
                    "parse hook relay queue sequence {}: {error}",
                    counter_path.display()
                );
                quarantine_path(queue_dir, &counter_path, &message)?;
                scan_sequence_high_water(queue_dir)?
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            scan_sequence_high_water(queue_dir)?
        }
        Err(error) => {
            return Err(format!(
                "read hook relay queue sequence {}: {error}",
                counter_path.display()
            ));
        }
    };
    let next = current
        .checked_add(1)
        .ok_or_else(|| "hook relay queue sequence exhausted".to_string())?;
    publish_atomic_file(
        &counter_path,
        next.to_string().as_bytes(),
        "hook relay queue sequence",
    )?;
    Ok(next)
}

fn record_completed_high_water(queue_dir: &Path, sequence: u64) -> Result<(), String> {
    let path = queue_dir.join("completed-high-water");
    let current = read_high_water_or_quarantine(queue_dir, &path)?.unwrap_or(0);
    if sequence <= current {
        return Ok(());
    }
    publish_atomic_file(
        &path,
        sequence.to_string().as_bytes(),
        "hook relay completed high-water",
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn enqueue_ordered_hook_relay_request(
    endpoint: &str,
    provider: &str,
    event: &str,
    session_id: &str,
    payload: Value,
    response_timeout: Option<Duration>,
) -> Result<(PathBuf, Option<PathBuf>), String> {
    let marker_dir =
        failure_marker_dir(provider).ok_or_else(|| "runtime root is unavailable".to_string())?;
    let queue_dir = relay_queue_dir(provider, session_id)
        .ok_or_else(|| "runtime root is unavailable".to_string())?;
    std::fs::create_dir_all(&queue_dir).map_err(|err| {
        format!(
            "create ordered hook relay queue {}: {err}",
            queue_dir.display()
        )
    })?;
    let request_id = uuid::Uuid::new_v4().to_string();
    let published_at = Utc::now();
    let delivery_timeout = response_timeout.unwrap_or(DELIVERY_TTL).min(DELIVERY_TTL);
    let delivery_deadline = published_at
        + chrono::Duration::from_std(delivery_timeout)
            .map_err(|err| format!("convert hook relay delivery TTL: {err}"))?;
    let response = response_timeout
        .map(|timeout| {
            Ok::<_, String>(OrderedHookRelayResponseTarget {
                path: queue_dir.join("responses").join(format!(
                    "{}-{}.response.json",
                    request_id,
                    uuid::Uuid::new_v4().simple(),
                )),
                timeout_millis: timeout.as_millis().try_into().unwrap_or(u64::MAX),
            })
        })
        .transpose()?;
    let response_path = response.as_ref().map(|response| response.path.clone());
    let request = OrderedHookRelayRequest {
        request_id: request_id.clone(),
        published_at,
        delivery_deadline,
        endpoint: endpoint.to_string(),
        provider: provider.to_string(),
        event: event.to_string(),
        session_id: session_id.to_string(),
        payload,
        marker_dir,
        response,
    };
    let published_nanos = published_at.timestamp_nanos_opt().unwrap_or(i64::MAX);
    let request_path = queue_dir
        .join("ingress")
        .join(format!("{published_nanos:020}-{request_id}.ingress.json"));
    let encoded = serde_json::to_vec(&request)
        .map_err(|err| format!("serialize ordered hook relay request: {err}"))?;
    publish_atomic_file(&request_path, &encoded, "ordered hook relay request")?;
    let active_count = match (
        queue_ingress_paths(&queue_dir),
        queue_request_paths(&queue_dir),
    ) {
        (Ok(ingress), Ok(requests)) => ingress.len() + requests.len(),
        (Err(error), _) | (_, Err(error)) => {
            let _ = std::fs::remove_file(&request_path);
            return Err(error);
        }
    };
    if active_count > MAX_ACTIVE_QUEUE_FILES {
        let _ = std::fs::remove_file(&request_path);
        return Err(format!(
            "ordered hook relay queue is at its active-file limit ({MAX_ACTIVE_QUEUE_FILES})"
        ));
    }
    Ok((queue_dir, response_path))
}

pub(super) fn start_ordered_hook_relay_worker(queue_dir: &Path) -> Result<(), String> {
    let Some(worker_probe) = lock_relay_queue_file(&queue_dir.join("worker.lock"), true)? else {
        // The active helper normally observes this request; the endpoint recovery
        // owner and synchronous response reprobe cover its narrow idle-exit race.
        return Ok(());
    };
    drop(worker_probe);
    let request = OrderedHookRelayWorkerRequest {
        queue_dir: queue_dir.to_path_buf(),
    };
    let encoded = serde_json::to_string(&request)
        .map_err(|err| format!("serialize non-wait hook relay worker handoff: {err}"))?;
    let executable = std::env::current_exe()
        .map_err(|err| format!("resolve non-wait hook relay worker: {err}"))?;
    let mut command = Command::new(executable);
    #[cfg(test)]
    command.args([
        "--ignored",
        "--exact",
        "services::claude_tui::hook_relay::tests::non_wait_relay_worker_subprocess_entry",
    ]);
    let child = command
        .env(NON_WAIT_RELAY_WORKER_ENV, encoded)
        .env_remove(FAILURE_MARKER_WORKER_ENV)
        .env_remove("AGENTDESK_ROOT_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("start non-wait hook relay worker: {err}"))?;
    drop(child);
    Ok(())
}

pub(super) fn handoff_non_wait_hook_event(
    endpoint: &str,
    provider: &str,
    event: &str,
    session_id: &str,
    payload: Value,
) -> Result<(), String> {
    let (queue_dir, _) =
        enqueue_ordered_hook_relay_request(endpoint, provider, event, session_id, payload, None)?;
    start_ordered_hook_relay_worker(&queue_dir)
}

pub(super) fn handoff_ordered_hook_event_response_with_timeout(
    endpoint: &str,
    provider: &str,
    event: &str,
    session_id: &str,
    payload: Value,
    timeout: Duration,
) -> Result<Value, String> {
    let started = Instant::now();
    let (queue_dir, response_path) = enqueue_ordered_hook_relay_request(
        endpoint,
        provider,
        event,
        session_id,
        payload,
        Some(timeout),
    )?;
    let response_path = response_path
        .ok_or_else(|| "ordered hook relay response path was not allocated".to_string())?;
    start_ordered_hook_relay_worker(&queue_dir)?;
    let mut recovery_reprobed = false;
    loop {
        match std::fs::read(&response_path) {
            Ok(encoded) => {
                let _ = std::fs::remove_file(&response_path);
                return serde_json::from_slice::<OrderedHookRelayResponse>(&encoded)
                    .map_err(|err| format!("parse ordered hook relay response: {err}"))?
                    .result;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "read ordered hook relay response {}: {error}",
                    response_path.display()
                ));
            }
        }
        if started.elapsed() >= timeout {
            let _ = std::fs::remove_file(&response_path);
            return Err(format!(
                "ordered hook relay response timed out after {}ms",
                timeout.as_millis()
            ));
        }
        if !recovery_reprobed && started.elapsed() >= RELAY_RECOVERY_REPROBE {
            recovery_reprobed = true;
            let _ = start_ordered_hook_relay_worker(&queue_dir);
        }
        std::thread::sleep(RELAY_RESPONSE_POLL_INTERVAL);
    }
}

pub(super) fn run_ordered_hook_relay_worker_from_env(encoded: OsString) -> Result<(), String> {
    encoded
        .into_string()
        .map_err(|_| "non-wait hook relay handoff is not UTF-8".to_string())
        .and_then(|encoded| {
            serde_json::from_str::<OrderedHookRelayWorkerRequest>(&encoded)
                .map_err(|err| format!("parse non-wait hook relay handoff: {err}"))
        })
        .and_then(run_ordered_hook_relay_worker)
}

fn run_ordered_hook_relay_worker(request: OrderedHookRelayWorkerRequest) -> Result<(), String> {
    let queue_dir = request.queue_dir;
    let Some(worker_lock) = lock_relay_queue_file(&queue_dir.join("worker.lock"), true)? else {
        return Ok(());
    };
    let mut worker_lock = Some(worker_lock);
    loop {
        promote_ordered_hook_relay_ingress(&queue_dir)?;
        let request_paths = queue_request_paths(&queue_dir)?;
        if request_paths.is_empty() {
            std::thread::sleep(RELAY_QUEUE_IDLE_GRACE);
            promote_ordered_hook_relay_ingress(&queue_dir)?;
            if queue_request_paths(&queue_dir)?.is_empty() {
                drop(worker_lock.take());
                return Ok(());
            }
            continue;
        }
        let mut retry_pending = false;
        for request_path in request_paths {
            match process_ordered_hook_relay_request(&request_path)? {
                OrderedHookRelayProcessOutcome::Completed => {
                    let sequence = sequence_from_embedded_filename(&request_path).unwrap_or(0);
                    record_completed_high_water(&queue_dir, sequence)?;
                    if let Err(error) = std::fs::remove_file(&request_path) {
                        let message = format!(
                            "remove completed hook relay request {}: {error}",
                            request_path.display()
                        );
                        quarantine_path(&queue_dir, &request_path, &message)?;
                    }
                }
                OrderedHookRelayProcessOutcome::Quarantine(error) => {
                    quarantine_path(&queue_dir, &request_path, &error)?;
                }
                OrderedHookRelayProcessOutcome::Retry => {
                    retry_pending = true;
                    break;
                }
            }
        }
        if retry_pending {
            std::thread::sleep(RELAY_IN_FLIGHT_RETRY_INTERVAL);
        }
    }
}

fn promote_ordered_hook_relay_ingress(queue_dir: &Path) -> Result<(), String> {
    #[cfg(test)]
    if behavior_mutation_enabled("promotion-omission") {
        return Ok(());
    }
    for ingress_path in queue_ingress_paths(queue_dir)? {
        let encoded = match std::fs::read(&ingress_path) {
            Ok(encoded) => encoded,
            Err(error) => {
                let message = format!(
                    "read ordered hook relay ingress {}: {error}",
                    ingress_path.display()
                );
                quarantine_path(queue_dir, &ingress_path, &message)?;
                continue;
            }
        };
        let request = match serde_json::from_slice::<OrderedHookRelayRequest>(&encoded) {
            Ok(request) => request,
            Err(error) => {
                let message = format!(
                    "parse ordered hook relay ingress {}: {error}",
                    ingress_path.display()
                );
                quarantine_path(queue_dir, &ingress_path, &message)?;
                continue;
            }
        };
        let sequence = next_relay_queue_sequence(queue_dir)?;
        let request_path = queue_dir.join(format!(
            "{sequence:020}-{}.request.json",
            request.request_id
        ));
        std::fs::rename(&ingress_path, &request_path).map_err(|error| {
            format!(
                "promote ordered hook relay ingress {} -> {}: {error}",
                ingress_path.display(),
                request_path.display()
            )
        })?;
    }
    Ok(())
}

enum OrderedHookRelayProcessOutcome {
    Completed,
    Quarantine(String),
    Retry,
}

fn process_ordered_hook_relay_request(
    request_path: &Path,
) -> Result<OrderedHookRelayProcessOutcome, String> {
    let encoded = std::fs::read(request_path).map_err(|err| {
        format!(
            "read ordered hook relay request {}: {err}",
            request_path.display()
        )
    })?;
    let request = match serde_json::from_slice::<OrderedHookRelayRequest>(&encoded) {
        Ok(request) => request,
        Err(error) => {
            return Ok(OrderedHookRelayProcessOutcome::Quarantine(format!(
                "parse ordered hook relay request {}: {error}",
                request_path.display()
            )));
        }
    };
    let expired = Utc::now() >= request.delivery_deadline;
    #[cfg(test)]
    let expired = expired && !behavior_mutation_enabled("post-transport-ttl");
    if expired {
        let error = format!(
            "ordered hook relay delivery expired at {} before transport",
            request.delivery_deadline.to_rfc3339()
        );
        let _ = record_request_failure(&request, &error);
        return Ok(OrderedHookRelayProcessOutcome::Quarantine(error));
    }
    if let Some(response) = request.response.as_ref() {
        let result = relay_hook_event_response_with_request_timeout(
            &request.endpoint,
            &request.provider,
            &request.event,
            &request.session_id,
            request.payload.clone(),
            &request.request_id,
            request.published_at,
            request.delivery_deadline,
            Duration::from_millis(response.timeout_millis),
        );
        let pin_mismatch = result
            .as_ref()
            .err()
            .is_some_and(|error| error.contains("HTTP 409"));
        if result
            .as_ref()
            .err()
            .is_some_and(|error| error.contains("HTTP 425"))
        {
            return Ok(OrderedHookRelayProcessOutcome::Retry);
        }
        let encoded = serde_json::to_vec(&OrderedHookRelayResponse { result })
            .map_err(|err| format!("serialize ordered hook relay response: {err}"))?;
        publish_atomic_file(&response.path, &encoded, "ordered hook relay response")?;
        if pin_mismatch {
            return Ok(OrderedHookRelayProcessOutcome::Quarantine(
                "receiver rejected relay request id pin with HTTP 409".to_string(),
            ));
        }
        return Ok(OrderedHookRelayProcessOutcome::Completed);
    }
    match relay_hook_event_with_request(
        &request.endpoint,
        &request.provider,
        &request.event,
        &request.session_id,
        request.payload.clone(),
        &request.request_id,
        request.published_at,
        request.delivery_deadline,
    ) {
        Ok(()) => Ok(OrderedHookRelayProcessOutcome::Completed),
        Err(error) => {
            if error.contains("HTTP 425") {
                return Ok(OrderedHookRelayProcessOutcome::Retry);
            }
            record_request_failure(&request, &error)?;
            if error.contains("HTTP 409") {
                Ok(OrderedHookRelayProcessOutcome::Quarantine(error))
            } else {
                Ok(OrderedHookRelayProcessOutcome::Completed)
            }
        }
    }
}

fn record_request_failure(request: &OrderedHookRelayRequest, error: &str) -> Result<(), String> {
    write_hook_relay_failure_marker(HookRelayFailureMarkerWriteRequest {
        marker_dir: request.marker_dir.clone(),
        marker: HookRelayFailureMarker {
            provider: request.provider.trim().to_ascii_lowercase(),
            event: request.event.clone(),
            session_id: request.session_id.clone(),
            endpoint: request.endpoint.clone(),
            error: error.to_string(),
            recorded_at: Utc::now(),
        },
    })
}

pub(crate) struct OrderedHookRelayRecoveryOwner {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for OrderedHookRelayRecoveryOwner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let join_started = Instant::now();
            tracing::info!(
                thread_finished = thread.is_finished(),
                "ordered hook relay recovery owner join begin"
            );
            match thread.join() {
                Ok(()) => tracing::info!(
                    join_latency_ms = join_started.elapsed().as_millis(),
                    "ordered hook relay recovery owner join end"
                ),
                Err(_) => tracing::warn!(
                    join_latency_ms = join_started.elapsed().as_millis(),
                    "ordered hook relay recovery owner join end after thread panic"
                ),
            }
        }
    }
}

pub(crate) fn start_ordered_hook_relay_recovery_owner() -> Option<OrderedHookRelayRecoveryOwner> {
    #[cfg(test)]
    if std::env::var_os("AGENTDESK_ROOT_DIR").is_none() {
        return None;
    }
    let runtime_root = crate::config::runtime_root()?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("hook-relay-recovery".to_string())
        .spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                let scan_started = Instant::now();
                match scan_ordered_hook_relay_queues_once(&runtime_root) {
                    Ok(stats) => tracing::debug!(
                        scan_latency_ms = scan_started.elapsed().as_millis(),
                        queue_count = stats.queue_count,
                        active_queue_count = stats.active_queue_count,
                        "ordered hook relay recovery scan completed"
                    ),
                    Err(error) => tracing::warn!(
                        scan_latency_ms = scan_started.elapsed().as_millis(),
                        error,
                        "ordered hook relay recovery scan failed"
                    ),
                }
                let deadline = Instant::now() + RELAY_RECOVERY_SCAN_INTERVAL;
                while !thread_stop.load(Ordering::Acquire) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        })
        .ok()?;
    Some(OrderedHookRelayRecoveryOwner {
        stop,
        thread: Some(thread),
    })
}

#[derive(Debug, Default, PartialEq, Eq)]
struct OrderedHookRelayScanStats {
    queue_count: usize,
    active_queue_count: usize,
}

fn scan_ordered_hook_relay_queues_once(
    runtime_root: &Path,
) -> Result<OrderedHookRelayScanStats, String> {
    let mut stats = OrderedHookRelayScanStats::default();
    for provider in ["claude", "codex"] {
        let provider_root = runtime_root.join(relay_queue_subdir(provider));
        if std::fs::symlink_metadata(&provider_root)
            .ok()
            .is_some_and(|metadata| metadata.file_type().is_symlink())
        {
            continue;
        }
        let entries = match std::fs::read_dir(&provider_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "read ordered relay provider root {}: {error}",
                    provider_root.display()
                ));
            }
        };
        for entry in entries.filter_map(Result::ok) {
            let queue_dir = entry.path();
            let metadata = match std::fs::symlink_metadata(&queue_dir) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                continue;
            }
            stats.queue_count += 1;
            gc_ordered_hook_relay_queue(&queue_dir);
            let has_work = queue_ingress_paths(&queue_dir).is_ok_and(|paths| !paths.is_empty())
                || queue_request_paths(&queue_dir).is_ok_and(|paths| !paths.is_empty());
            if has_work {
                stats.active_queue_count += 1;
            }
            if has_work && let Err(error) = start_ordered_hook_relay_worker(&queue_dir) {
                tracing::warn!(
                    queue_dir = %queue_dir.display(),
                    error,
                    "failed to restart stranded ordered hook relay worker"
                );
            }
        }
    }
    Ok(stats)
}

fn gc_ordered_hook_relay_queue(queue_dir: &Path) {
    let Ok(Some(_worker_lock)) = lock_relay_queue_file(&queue_dir.join("worker.lock"), true) else {
        return;
    };
    let retention = LEDGER_RETENTION;
    prune_artifact_dir(
        &queue_dir.join("quarantine"),
        MAX_QUARANTINE_FILES,
        retention,
    );
    prune_artifact_dir(
        &queue_dir.join("responses"),
        MAX_ORPHAN_RESPONSE_FILES,
        retention,
    );
    for dir in [queue_dir, &queue_dir.join("ingress")] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
            let is_temp = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(".tmp."));
            if is_temp && file_is_older_than(&path, retention) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    for dir in [queue_dir.join("responses"), queue_dir.join("quarantine")] {
        let _ = std::fs::remove_dir(dir);
    }
}

fn prune_artifact_dir(dir: &Path, cap: usize, retention: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    paths.sort_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    let excess = paths.len().saturating_sub(cap);
    for (index, path) in paths.into_iter().enumerate() {
        if index < excess || file_is_older_than(&path, retention) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn file_is_older_than(path: &Path, age: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|elapsed| elapsed >= age)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, mpsc};

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request};
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower::ServiceExt;

    use super::*;

    const LOCK_HOLDER_PATH_ENV: &str = "AGENTDESK_RELAY_TEST_LOCK_HOLDER_PATH";
    const LOCK_HOLDER_READY_ENV: &str = "AGENTDESK_RELAY_TEST_LOCK_HOLDER_READY";
    const LOCK_HOLDER_RELEASE_ENV: &str = "AGENTDESK_RELAY_TEST_LOCK_HOLDER_RELEASE";

    fn request_body(request: &[u8]) -> Value {
        let request = std::str::from_utf8(request).expect("HTTP request is UTF-8");
        let (_, body) = request
            .split_once("\r\n\r\n")
            .expect("HTTP request body separator");
        serde_json::from_str(body).expect("HTTP request JSON body")
    }

    fn http_body_bounds(request: &[u8]) -> Option<(usize, usize)> {
        let headers_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")?;
        let body_start = headers_end + 4;
        let headers = std::str::from_utf8(&request[..headers_end]).ok()?;
        let content_length = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })?;
        Some((body_start, content_length))
    }

    fn spawn_test_receiver(
        expected: usize,
    ) -> (
        String,
        mpsc::Receiver<Value>,
        std::thread::JoinHandle<usize>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test receiver");
        listener
            .set_nonblocking(true)
            .expect("set receiver nonblocking");
        let endpoint = format!(
            "http://{}",
            listener.local_addr().expect("test receiver address")
        );
        let (request_tx, request_rx) = mpsc::sync_channel(expected.max(1));
        let receiver = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(4);
            let mut observed = 0usize;
            while observed < expected && Instant::now() < deadline {
                let (mut socket, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("accept test relay: {error}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .expect("set receiver read timeout");
                let mut encoded = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let read = socket.read(&mut buffer).expect("read test relay request");
                    assert!(read > 0, "relay closed before request body");
                    encoded.extend_from_slice(&buffer[..read]);
                    if let Some((body_start, body_len)) = http_body_bounds(&encoded)
                        && encoded.len() >= body_start + body_len
                    {
                        break;
                    }
                }
                request_tx
                    .send(request_body(&encoded))
                    .expect("publish observed relay request");
                let response = b"HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
                socket.write_all(response).expect("write test response");
                socket.flush().expect("flush test response");
                observed += 1;
            }
            observed
        });
        (endpoint, request_rx, receiver)
    }

    fn spawn_status_receiver(
        statuses: Vec<u16>,
    ) -> (
        String,
        mpsc::Receiver<Value>,
        std::thread::JoinHandle<usize>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind status receiver");
        let endpoint = format!(
            "http://{}",
            listener.local_addr().expect("status receiver address")
        );
        let (request_tx, request_rx) = mpsc::sync_channel(statuses.len().max(1));
        let receiver = std::thread::spawn(move || {
            let mut observed = 0usize;
            for status in statuses {
                let (mut socket, _) = listener.accept().expect("accept status relay");
                socket
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut encoded = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let read = socket.read(&mut buffer).expect("read status relay");
                    assert!(read > 0);
                    encoded.extend_from_slice(&buffer[..read]);
                    if let Some((body_start, body_len)) = http_body_bounds(&encoded)
                        && encoded.len() >= body_start + body_len
                    {
                        break;
                    }
                }
                request_tx.send(request_body(&encoded)).unwrap();
                let reason = if status == 409 {
                    "Conflict"
                } else {
                    "Accepted"
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                );
                socket.write_all(response.as_bytes()).unwrap();
                socket.flush().unwrap();
                observed += 1;
            }
            observed
        });
        (endpoint, request_rx, receiver)
    }

    fn wait_until(mut predicate: impl FnMut() -> bool, label: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !predicate() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(predicate(), "timed out waiting for {label}");
    }

    fn recursively_contains(root: &Path, needle: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(root) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if recursively_contains(&path, needle) {
                    return true;
                }
                continue;
            }
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(needle))
                || std::fs::read(&path).ok().is_some_and(|bytes| {
                    bytes
                        .windows(needle.len())
                        .any(|part| part == needle.as_bytes())
                })
            {
                return true;
            }
        }
        false
    }

    fn spawn_worker_process(queue_dir: &Path) -> Child {
        let encoded = serde_json::to_string(&OrderedHookRelayWorkerRequest {
            queue_dir: queue_dir.to_path_buf(),
        })
        .expect("serialize worker request");
        Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--ignored",
                "--exact",
                "services::claude_tui::hook_relay::tests::non_wait_relay_worker_subprocess_entry",
            ])
            .env(NON_WAIT_RELAY_WORKER_ENV, encoded)
            .env_remove(FAILURE_MARKER_WORKER_ENV)
            .env_remove("AGENTDESK_ROOT_DIR")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ordered relay worker process")
    }

    #[test]
    fn corrupt_counter_recovers_without_reusing_completed_high_water() {
        let temp_dir = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(temp_dir.path());
        let (endpoint, requests, receiver) = spawn_test_receiver(1);
        let session_id = "counter-recovery-session";
        let (queue_dir, _) = enqueue_ordered_hook_relay_request(
            &endpoint,
            "claude",
            "PostToolUse",
            session_id,
            json!({"ordinal": 1}),
            None,
        )
        .unwrap();
        start_ordered_hook_relay_worker(&queue_dir).unwrap();
        assert_eq!(
            requests
                .recv_timeout(Duration::from_secs(2))
                .expect("first request delivered")["ordinal"],
            1
        );
        wait_until(
            || queue_request_paths(&queue_dir).is_ok_and(|paths| paths.is_empty()),
            "first request removal",
        );
        assert_eq!(receiver.join().unwrap(), 1);

        std::fs::write(queue_dir.join("next-sequence"), b"not-a-sequence").unwrap();
        enqueue_ordered_hook_relay_request(
            &endpoint,
            "claude",
            "PostToolUse",
            session_id,
            json!({"ordinal": 2}),
            None,
        )
        .expect("corrupt counter is quarantined and rebuilt");
        promote_ordered_hook_relay_ingress(&queue_dir).unwrap();
        let pending = queue_request_paths(&queue_dir).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(
            pending[0]
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("00000000000000000002-")),
            "completed high-water must prevent sequence 1 reuse: {}",
            pending[0].display()
        );
        assert!(
            recursively_contains(&queue_dir, "next-sequence"),
            "counter corruption must leave visible quarantine evidence"
        );

        let second_path = pending[0].clone();
        quarantine_path(
            &queue_dir,
            &second_path,
            "test preserves original sequence name",
        )
        .unwrap();
        std::fs::write(queue_dir.join("next-sequence"), b"corrupt-again").unwrap();
        enqueue_ordered_hook_relay_request(
            &endpoint,
            "claude",
            "PostToolUse",
            session_id,
            json!({"ordinal": 3}),
            None,
        )
        .unwrap();
        promote_ordered_hook_relay_ingress(&queue_dir).unwrap();
        let pending = queue_request_paths(&queue_dir).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(
            pending[0]
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("00000000000000000003-")),
            "quarantined original sequence name must preserve high-water"
        );
    }

    #[test]
    fn corrupt_completed_high_water_is_quarantined_without_stalling_promotion() {
        let temp_dir = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(temp_dir.path());
        let session_id = "completed-high-water-recovery";
        let (queue_dir, _) = enqueue_ordered_hook_relay_request(
            "http://127.0.0.1:9/hooks",
            "claude",
            "PostToolUse",
            session_id,
            json!({"ordinal": 1}),
            None,
        )
        .unwrap();
        std::fs::write(queue_dir.join("next-sequence"), b"7").unwrap();
        std::fs::write(queue_dir.join("completed-high-water"), b"corrupt").unwrap();

        promote_ordered_hook_relay_ingress(&queue_dir).unwrap();

        let pending = queue_request_paths(&queue_dir).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(
            pending[0]
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("00000000000000000008-"))
        );
        assert!(recursively_contains(&queue_dir, "completed-high-water"));
    }

    #[test]
    fn corrupt_oldest_request_is_quarantined_and_later_request_drains() {
        let temp_dir = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(temp_dir.path());
        let (endpoint, requests, receiver) = spawn_test_receiver(1);
        let session_id = "corrupt-oldest-session";
        let (queue_dir, _) = enqueue_ordered_hook_relay_request(
            &endpoint,
            "claude",
            "PostToolUse",
            session_id,
            json!({"ordinal": 1}),
            None,
        )
        .unwrap();
        enqueue_ordered_hook_relay_request(
            &endpoint,
            "claude",
            "PostToolUse",
            session_id,
            json!({"ordinal": 2}),
            None,
        )
        .unwrap();
        promote_ordered_hook_relay_ingress(&queue_dir).unwrap();
        let pending = queue_request_paths(&queue_dir).unwrap();
        let corrupt_name = pending[0]
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap()
            .to_string();
        std::fs::write(&pending[0], b"{corrupt-json").unwrap();

        start_ordered_hook_relay_worker(&queue_dir).unwrap();
        assert_eq!(
            requests
                .recv_timeout(Duration::from_secs(3))
                .expect("later valid request must drain despite corrupt oldest")["ordinal"],
            2
        );
        wait_until(
            || queue_request_paths(&queue_dir).is_ok_and(|paths| paths.is_empty()),
            "valid request drain after quarantine",
        );
        assert_eq!(receiver.join().unwrap(), 1);
        assert!(
            recursively_contains(&queue_dir, &corrupt_name),
            "quarantine evidence must identify the corrupt request"
        );
    }

    #[test]
    fn recovery_scan_reports_idle_queue_cardinality_with_bounded_latency() {
        let temp_dir = tempfile::tempdir().unwrap();
        let provider_root = temp_dir.path().join(relay_queue_subdir("claude"));
        std::fs::create_dir_all(&provider_root).unwrap();
        for queue_index in 0..1_000 {
            std::fs::create_dir(provider_root.join(format!("queue-{queue_index:04}"))).unwrap();
        }

        let started = Instant::now();
        let stats = scan_ordered_hook_relay_queues_once(temp_dir.path()).unwrap();
        let elapsed = started.elapsed();

        assert_eq!(
            stats,
            OrderedHookRelayScanStats {
                queue_count: 1_000,
                active_queue_count: 0,
            }
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "1,000 idle queue scan exceeded the bounded-time budget: {elapsed:?}"
        );
    }

    #[test]
    fn published_request_is_drained_by_receiver_owner_without_producer_restart() {
        let temp_dir = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(temp_dir.path());
        let (endpoint, requests, receiver) = spawn_test_receiver(1);
        let _endpoint_guard =
            crate::services::claude_tui::hook_server::publish_hook_endpoint(endpoint.clone());
        let (queue_dir, _) = enqueue_ordered_hook_relay_request(
            &endpoint,
            "claude",
            "PostToolUse",
            "publish-before-start-crash",
            json!({"published_before_exit": true}),
            None,
        )
        .unwrap();

        assert_eq!(
            requests
                .recv_timeout(Duration::from_secs(3))
                .expect("standing receiver owner must discover a stranded publication")["published_before_exit"],
            true
        );
        wait_until(
            || queue_request_paths(&queue_dir).is_ok_and(|paths| paths.is_empty()),
            "recovery owner request removal",
        );
        assert_eq!(receiver.join().unwrap(), 1);
    }

    #[test]
    fn contended_producer_returns_bounded_and_earlier_ingress_is_not_overtaken() {
        let temp_dir = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(temp_dir.path());
        let (endpoint, requests, receiver) = spawn_test_receiver(2);
        let session_id = "producer-lock-contention";
        let queue_dir = relay_queue_dir("claude", session_id).unwrap();
        std::fs::create_dir_all(&queue_dir).unwrap();
        let ready_path = temp_dir.path().join("holder-ready");
        let release_path = temp_dir.path().join("holder-release");
        let mut holder = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "services::claude_tui::hook_relay::ordered_queue::tests::producer_lock_holder_subprocess_entry",
            ])
            .env(LOCK_HOLDER_PATH_ENV, queue_dir.join("producer.lock"))
            .env(LOCK_HOLDER_READY_ENV, &ready_path)
            .env(LOCK_HOLDER_RELEASE_ENV, &release_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn producer lock holder");
        wait_until(|| ready_path.exists(), "producer lock holder readiness");

        let started = Instant::now();
        enqueue_ordered_hook_relay_request(
            &endpoint,
            "claude",
            "PostToolUse",
            session_id,
            json!({"ordinal": 1}),
            None,
        )
        .expect("contended producer leaves a durable ingress handoff");
        let elapsed = started.elapsed();
        let ingress_was_durable = recursively_contains(&queue_dir, ".ingress.json");
        std::fs::write(&release_path, b"release").unwrap();
        assert!(holder.wait().unwrap().success());

        enqueue_ordered_hook_relay_request(
            &endpoint,
            "claude",
            "PostToolUse",
            session_id,
            json!({"ordinal": 2}),
            None,
        )
        .unwrap();
        start_ordered_hook_relay_worker(&queue_dir).unwrap();
        let first = requests.recv_timeout(Duration::from_secs(3)).unwrap();
        let second = requests.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(receiver.join().unwrap(), 2);

        assert!(
            elapsed < Duration::from_millis(750),
            "producer flock blocked the hook boundary for {elapsed:?}"
        );
        assert!(
            ingress_was_durable,
            "lock contention must leave durable handoff/failure evidence before returning"
        );
        assert_eq!(
            [first["ordinal"].as_u64(), second["ordinal"].as_u64()],
            [Some(1), Some(2)],
            "a fast-path sequence must not overtake an already-published ingress"
        );
    }

    #[test]
    fn concurrent_ingress_is_promoted_in_published_nanos_uuid_order() {
        let temp_dir = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(temp_dir.path());
        let (endpoint, requests, receiver) = spawn_test_receiver(8);
        let barrier = Arc::new(std::sync::Barrier::new(8));
        std::thread::scope(|scope| {
            for ordinal in 0..8u64 {
                let endpoint = endpoint.clone();
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    enqueue_ordered_hook_relay_request(
                        &endpoint,
                        "claude",
                        "PostToolUse",
                        "concurrent-ingress-order",
                        json!({"ordinal": ordinal}),
                        None,
                    )
                    .unwrap();
                });
            }
        });
        let queue_dir = relay_queue_dir("claude", "concurrent-ingress-order").unwrap();
        let expected = queue_ingress_paths(&queue_dir)
            .unwrap()
            .into_iter()
            .map(|path| {
                serde_json::from_slice::<OrderedHookRelayRequest>(&std::fs::read(path).unwrap())
                    .unwrap()
                    .payload["ordinal"]
                    .as_u64()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(expected.len(), 8);
        start_ordered_hook_relay_worker(&queue_dir).unwrap();
        let observed = (0..8)
            .map(|_| {
                requests.recv_timeout(Duration::from_secs(3)).unwrap()["ordinal"]
                    .as_u64()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(receiver.join().unwrap(), 8);
        assert_eq!(observed, expected);
    }

    #[test]
    fn expired_ingress_is_quarantined_before_transport() {
        let temp_dir = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(temp_dir.path());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (queue_dir, _) = enqueue_ordered_hook_relay_request(
            &endpoint,
            "claude",
            "Stop",
            "expired-ingress",
            json!({}),
            None,
        )
        .unwrap();
        let ingress = queue_ingress_paths(&queue_dir).unwrap().remove(0);
        let mut request: OrderedHookRelayRequest =
            serde_json::from_slice(&std::fs::read(&ingress).unwrap()).unwrap();
        request.published_at = Utc::now() - chrono::Duration::hours(2);
        request.delivery_deadline = Utc::now() - chrono::Duration::hours(1);
        std::fs::write(&ingress, serde_json::to_vec(&request).unwrap()).unwrap();
        start_ordered_hook_relay_worker(&queue_dir).unwrap();
        wait_until(
            || {
                queue_ingress_paths(&queue_dir).is_ok_and(|paths| paths.is_empty())
                    && queue_request_paths(&queue_dir).is_ok_and(|paths| paths.is_empty())
            },
            "expired ingress quarantine",
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "expired boundary must be rejected before opening transport"
        );
        assert!(recursively_contains(&queue_dir, "delivery expired"));
    }

    #[test]
    fn double_recovery_scanner_delivers_one_effect() {
        let temp_dir = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(temp_dir.path());
        let (endpoint, requests, receiver) = spawn_test_receiver(1);
        let first =
            crate::services::claude_tui::hook_server::publish_hook_endpoint(endpoint.clone());
        let second =
            crate::services::claude_tui::hook_server::publish_hook_endpoint(endpoint.clone());
        let (queue_dir, _) = enqueue_ordered_hook_relay_request(
            &endpoint,
            "claude",
            "PostToolUse",
            "double-scanner",
            json!({"single_effect": true}),
            None,
        )
        .unwrap();
        assert_eq!(
            requests.recv_timeout(Duration::from_secs(3)).unwrap()["single_effect"],
            true
        );
        assert_eq!(receiver.join().unwrap(), 1);
        wait_until(
            || queue_request_paths(&queue_dir).is_ok_and(|paths| paths.is_empty()),
            "double-scanner drain",
        );
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            crate::services::claude_tui::hook_relay::drain_hook_relay_failure_markers(
                "claude",
                "double-scanner",
            )
            .is_empty(),
            "a second scanner must not replay the already-completed effect"
        );
        drop(second);
        drop(first);
    }

    #[test]
    fn receiver_pin_mismatch_is_quarantined_and_later_request_continues() {
        let temp_dir = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(temp_dir.path());
        let (endpoint, requests, receiver) = spawn_status_receiver(vec![202, 409, 202]);
        let session_id = "pin-mismatch-forward-progress";
        for ordinal in 1..=3u64 {
            enqueue_ordered_hook_relay_request(
                &endpoint,
                "claude",
                "PostToolUse",
                session_id,
                json!({"ordinal": ordinal}),
                None,
            )
            .unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        let queue_dir = relay_queue_dir("claude", session_id).unwrap();
        let ingress = queue_ingress_paths(&queue_dir).unwrap();
        let first: OrderedHookRelayRequest =
            serde_json::from_slice(&std::fs::read(&ingress[0]).unwrap()).unwrap();
        let mut second: OrderedHookRelayRequest =
            serde_json::from_slice(&std::fs::read(&ingress[1]).unwrap()).unwrap();
        second.request_id = first.request_id;
        std::fs::write(&ingress[1], serde_json::to_vec(&second).unwrap()).unwrap();

        start_ordered_hook_relay_worker(&queue_dir).unwrap();
        let observed = (0..3)
            .map(|_| {
                requests.recv_timeout(Duration::from_secs(3)).unwrap()["ordinal"]
                    .as_u64()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(receiver.join().unwrap(), 3);
        assert_eq!(observed, vec![1, 2, 3]);
        wait_until(
            || queue_request_paths(&queue_dir).is_ok_and(|paths| paths.is_empty()),
            "pin mismatch quarantine and forward progress",
        );
        assert!(
            recursively_contains(&queue_dir, "HTTP 409"),
            "pin mismatch request and evidence must be quarantined"
        );
    }

    #[test]
    fn receiver_in_flight_response_retries_same_request_until_receipt_is_cached() {
        let temp_dir = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(temp_dir.path());
        let (endpoint, requests, receiver) = spawn_status_receiver(vec![425, 202]);
        let session_id = "in-flight-retry";
        let (queue_dir, _) = enqueue_ordered_hook_relay_request(
            &endpoint,
            "claude",
            "PostToolUse",
            session_id,
            json!({"ordinal": 1}),
            None,
        )
        .unwrap();

        start_ordered_hook_relay_worker(&queue_dir).unwrap();
        let observed = (0..2)
            .map(|_| {
                requests.recv_timeout(Duration::from_secs(3)).unwrap()["ordinal"]
                    .as_u64()
                    .unwrap()
            })
            .collect::<Vec<_>>();

        assert_eq!(receiver.join().unwrap(), 2);
        assert_eq!(observed, vec![1, 1]);
        wait_until(
            || queue_request_paths(&queue_dir).is_ok_and(|paths| paths.is_empty()),
            "in-flight receipt retry drain",
        );
    }

    #[test]
    #[ignore = "helper subprocess that holds the real producer flock"]
    fn producer_lock_holder_subprocess_entry() {
        let Some(lock_path) = std::env::var_os(LOCK_HOLDER_PATH_ENV) else {
            return;
        };
        let ready_path = std::env::var_os(LOCK_HOLDER_READY_ENV).unwrap();
        let release_path = PathBuf::from(std::env::var_os(LOCK_HOLDER_RELEASE_ENV).unwrap());
        let _lock = lock_relay_queue_file(Path::new(&lock_path), false)
            .unwrap()
            .expect("acquire producer lock");
        std::fs::write(ready_path, b"ready").unwrap();
        let deadline = Instant::now() + Duration::from_millis(1_200);
        while !release_path.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    async fn read_async_http_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut encoded = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = socket.read(&mut buffer).await.expect("read proxy request");
            assert!(read > 0, "worker closed before proxy request body");
            encoded.extend_from_slice(&buffer[..read]);
            if let Some((body_start, body_len)) = http_body_bounds(&encoded)
                && encoded.len() >= body_start + body_len
            {
                return encoded;
            }
        }
    }

    fn request_path(request: &[u8]) -> String {
        std::str::from_utf8(request)
            .unwrap()
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .expect("HTTP request target")
            .to_string()
    }

    fn request_header(request: &[u8], expected: &str) -> Option<String> {
        let request = std::str::from_utf8(request).ok()?;
        request.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case(expected)
                .then(|| value.trim().to_string())
        })
    }

    async fn spawn_actual_receiver_proxy(
        listener: tokio::net::TcpListener,
        router: Arc<tokio::sync::RwLock<Router>>,
        accepted_tx: tokio::sync::mpsc::Sender<(String, Value)>,
        first_release: tokio::sync::oneshot::Receiver<()>,
    ) {
        let mut first_release = Some(first_release);
        for index in 0..2 {
            let (mut socket, _) = listener.accept().await.expect("accept worker relay");
            let encoded = read_async_http_request(&mut socket).await;
            let path = request_path(&encoded);
            let body_start = http_body_bounds(&encoded).unwrap().0;
            let app = router.read().await.clone();
            let request_id = request_header(&encoded, RELAY_REQUEST_ID_HEADER)
                .expect("worker relay request id header");
            let mut request = Request::builder()
                .method(Method::POST)
                .uri(&path)
                .header("content-type", "application/json");
            for name in [
                RELAY_REQUEST_ID_HEADER,
                RELAY_PUBLISHED_AT_HEADER,
                RELAY_DEADLINE_HEADER,
            ] {
                request = request.header(name, request_header(&encoded, name).unwrap());
            }
            let response = app
                .oneshot(
                    request
                        .body(Body::from(encoded[body_start..].to_vec()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let value: Value = serde_json::from_slice(&body).unwrap();
            accepted_tx.send((request_id, value)).await.unwrap();
            if index == 0 {
                let _ = first_release.take().unwrap().await;
            }
            let reason = status.canonical_reason().unwrap_or("Accepted");
            let headers = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                status.as_u16(),
                reason,
                body.len()
            );
            let _ = socket.write_all(headers.as_bytes()).await;
            let _ = socket.write_all(&body).await;
            let _ = socket.flush().await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worker_crash_after_actual_stop_acceptance_replays_cached_receipt_once() {
        let temp_dir = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(temp_dir.path());
        let session_id = "worker-crash-after-stop-acceptance";
        let state = crate::services::claude_tui::hook_server::HookServerState::new();
        let app = crate::services::claude_tui::hook_server::hook_receiver_router_with_state(state);
        let search = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/hooks/claude/PostToolUse?session_id={session_id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "tool_name":"mcp__memento__recall",
                            "tool_response":{"_meta":{"searchEventId":"4308"}}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(search.status().as_u16(), 202);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let routers = Arc::new(tokio::sync::RwLock::new(app));
        let (accepted_tx, mut accepted_rx) = tokio::sync::mpsc::channel(2);
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let proxy = tokio::spawn(spawn_actual_receiver_proxy(
            listener,
            Arc::clone(&routers),
            accepted_tx,
            release_rx,
        ));

        let (queue_dir, response_path) = enqueue_ordered_hook_relay_request(
            &endpoint,
            "claude",
            "Stop",
            session_id,
            json!({}),
            Some(Duration::from_secs(10)),
        )
        .unwrap();
        let ingress_path = queue_ingress_paths(&queue_dir).unwrap().remove(0);
        let queued: Value = serde_json::from_slice(&std::fs::read(&ingress_path).unwrap()).unwrap();
        let request_id = queued
            .get("request_id")
            .and_then(Value::as_str)
            .expect("ordered request must persist a stable receiver idempotency key")
            .to_string();

        let mut first_worker = spawn_worker_process(&queue_dir);
        let (first_path, first_body) =
            tokio::time::timeout(Duration::from_secs(3), accepted_rx.recv())
                .await
                .expect("first receiver acceptance timeout")
                .expect("first receiver acceptance");
        assert_eq!(first_path, request_id);
        first_worker
            .kill()
            .expect("kill worker after receiver acceptance");
        first_worker.wait().expect("reap killed worker");
        let request_path = queue_request_paths(&queue_dir).unwrap().remove(0);
        assert!(
            request_path.exists(),
            "crash point must precede request removal"
        );
        let _ = release_tx.send(());
        let mut recovery_worker = spawn_worker_process(&queue_dir);
        let (second_path, second_body) =
            tokio::time::timeout(Duration::from_secs(3), accepted_rx.recv())
                .await
                .expect("recovery receiver acceptance timeout")
                .expect("recovery receiver acceptance");
        assert_eq!(second_path, request_id);
        let recovery_status = tokio::task::spawn_blocking(move || recovery_worker.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(recovery_status.success());
        proxy.await.unwrap();

        assert!(first_body.get("memento_tool_feedback_flush").is_some());
        assert!(second_body.get("memento_tool_feedback_flush").is_some());
        assert!(
            !request_path.exists(),
            "recovery worker removes the request only after cached acceptance"
        );
        let response_path = response_path.unwrap();
        let response: OrderedHookRelayResponse =
            serde_json::from_slice(&std::fs::read(response_path).unwrap()).unwrap();
        assert!(
            response
                .result
                .as_ref()
                .is_ok_and(|body| body.get("memento_tool_feedback_flush").is_some()),
            "durable worker response must retain the accepted Stop flush"
        );

        let followup = routers
            .read()
            .await
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/hooks/claude/Stop?session_id={session_id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(json!({}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(followup.status().as_u16(), 202);
        let followup_body: Value =
            serde_json::from_slice(&to_bytes(followup.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert!(
            followup_body.get("memento_tool_feedback_flush").is_some(),
            "worker-crash replay must leave the sole Stop retry available to the next fresh boundary"
        );
    }
}
