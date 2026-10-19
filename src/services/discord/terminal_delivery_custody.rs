//! #5521: retain a detached terminal episode until the existing worker settles it.
//! The sibling directory is outside old inflight/receipt writers and reapers.
//! Payload interpretation, receipt checks and publication remain in turn_bridge.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use super::{
    outbound::delivery_record::{DeliveryRecordLock, lock_record_path},
    runtime_store,
};

const DIRECTORY: &str = "discord_terminal_delivery_custody";
const BATCH_SIZE: usize = 10;
static NEXT_ENTRY: AtomicUsize = AtomicUsize::new(0);

mod codex_absence {
    //! Pending terminal custody keeps its captured source available before a fresh launch.

    use super::*;
    use crate::services::discord::inflight::{CodexRange, InflightTurnState};
    use crate::services::tmux_common as tc;
    use crate::services::tui_prompt_dedupe::{binding_events, clear_relaunch::Captured};

    #[derive(Deserialize)]
    struct Identity {
        version: u8,
        provider: String,
        channel_id: u64,
        watcher_owner_channel_id: u64,
        local: InflightTurnState,
        admitted: Option<CodexRange>,
    }

    pub(crate) fn codex_absent_launch_clear(captured: &Captured) -> bool {
        if captured.context.owner_runtime_root != tc::current_tmux_owner_marker() {
            return false;
        }
        let Ok(root) = root() else { return false };
        let source = binding_events::codex::read_ownership(&captured.context)
            .ok()
            .filter(|fold| !fold.conflicted && fold.pending.is_empty())
            .and_then(|fold| fold.verified)
            .map(|proof| proof.source);
        let generation = generation(&captured.context.tmux_session);
        observes_retained_at(&root, true, |payload| {
            holds(payload, captured, source.as_ref(), generation)
        })
        .is_ok_and(|pending| !pending)
    }

    fn holds(
        payload: &Value,
        captured: &Captured,
        source: Option<&binding_events::SourceId>,
        generation: Option<i64>,
    ) -> bool {
        let Ok(identity) = serde_json::from_value::<Identity>(payload.clone()) else {
            return true;
        };
        let local = &identity.local;
        if identity.version != 1
            || identity.provider != local.provider
            || identity.channel_id != local.channel_id
            || identity.channel_id == 0
            || identity.watcher_owner_channel_id == 0
        {
            return true;
        }
        if let Some(range) = &identity.admitted {
            if range.source.provider != identity.provider
                || range.source.delivery_channel_id != identity.channel_id
                || range.source.offset_authority_channel_id != identity.watcher_owner_channel_id
                || !range.identity.matches_state(local)
                || Some(range.source.tmux_session_name.as_str())
                    != local.tmux_session_name.as_deref()
                || Some(range.source.turn_nonce.as_str()) != local.turn_nonce.as_deref()
                || Some(range.rollout_path.as_str()) != local.output_path.as_deref()
                || Some(range.session_id.as_str()) != local.session_id.as_deref()
                || Some(range.source.range.0) != local.turn_start_offset
                || range.source.range.1 != local.last_offset
                || local
                    .tui_terminal_source_file_identity
                    .is_some_and(|file| Some(file) != range.source_file_identity)
                || local
                    .tui_terminal_generation_mtime_ns
                    .is_some_and(|stamp| stamp != range.source.generation_mtime_ns)
            {
                return true;
            }
        }
        if identity.provider != "codex" {
            return false;
        }
        let channel = captured.context.channel_id.unwrap_or_default();
        if identity.channel_id != channel && identity.watcher_owner_channel_id != channel {
            return false;
        }
        let Some(tmux) = local
            .tmux_session_name
            .as_deref()
            .filter(|tmux| !tmux.trim().is_empty())
        else {
            return true;
        };
        if tmux != captured.context.tmux_session {
            return false;
        }
        let (Some(range), Some(source), Some(generation)) =
            (&identity.admitted, source, generation)
        else {
            return true;
        };
        let Some(file) = range.source_file_identity else {
            return true;
        };
        if local.runtime_kind != Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui)
            || range.source.range.0 >= range.source.range.1
            || range.session_id.is_empty()
            || !Path::new(&range.rollout_path).is_absolute()
            || range.source.turn_nonce.is_empty()
            || range.source.generation_mtime_ns <= 0
        {
            return true;
        }
        let matches = [
            Path::new(&range.rollout_path) == source.path.as_path(),
            range.session_id == source.session_id,
            file == (source.dev, source.ino),
            range.source.generation_mtime_ns == generation,
        ];
        // Mixed identities cannot establish an unrelated incarnation; an exact one is pending.
        matches.into_iter().any(|same| same)
    }

    fn generation(tmux: &str) -> Option<i64> {
        let legacy = PathBuf::from(tc::legacy_tmp_session_path(tmux, "generation"));
        let canonical = tc::persistent_sessions_dir()?.join(legacy.file_name()?);
        let mut seen = None;
        for path in [canonical, legacy] {
            let meta = match fs::symlink_metadata(path) {
                Ok(meta) if meta.is_file() => meta,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                _ => return None,
            };
            let stamp = i64::try_from(
                meta.modified()
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_nanos(),
            )
            .ok()?;
            if stamp <= 0 || seen.is_some_and(|previous| previous != stamp) {
                return None;
            }
            seen = Some(stamp);
        }
        seen
    }
}
pub(crate) use codex_absence::codex_absent_launch_clear;

#[derive(Serialize, Deserialize)]
struct Record {
    version: u8,
    key: String,
    seed_sha256: String,
    payload_sha256: String,
    payload: Value,
}

fn payload_sha256(payload: &Value) -> String {
    format!("{:x}", Sha256::digest(payload.to_string().as_bytes()))
}

impl Record {
    fn valid(&self) -> bool {
        self.version == 1
            && !self.key.trim().is_empty()
            && self.seed_sha256.len() == 64
            && self
                .seed_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            && self.payload.is_object()
            && self.payload_sha256 == payload_sha256(&self.payload)
    }
}

struct CheckpointState {
    record: Record,
    encoded: String,
    lock: Option<DeliveryRecordLock>,
}

/// The existing per-record flock stays owned by this handle throughout a
/// terminal publication attempt. The adapter can persist each actual chunk
/// receipt before attempting the next POST, without releasing that lock.
pub(in crate::services::discord) struct CustodyCheckpoint {
    path: PathBuf,
    state: Mutex<CheckpointState>,
}

/// This guard belongs to the attempt future, independently of any callback
/// clones. Dropping that future must release custody even if a clone escapes.
struct CustodyAttempt(Arc<CustodyCheckpoint>);

impl Drop for CustodyAttempt {
    fn drop(&mut self) {
        self.0.deactivate();
    }
}

impl CustodyCheckpoint {
    fn deactivate(&self) {
        // Poison must not prevent releasing the operating-system lock during
        // unwinding. Further writes still reject the poisoned state.
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        drop(state.lock.take());
    }

    fn verify_current(&self, state: &CheckpointState) -> Result<(), String> {
        if state.lock.is_none() {
            return Err("terminal custody checkpoint is no longer active".into());
        }
        if fs::read_to_string(&self.path).map_err(|error| error.to_string())? != state.encoded {
            return Err("terminal custody changed during resume; record preserved".into());
        }
        Ok(())
    }

    fn persist_locked(&self, state: &mut CheckpointState, payload: &Value) -> Result<(), String> {
        self.verify_current(state)?;
        if !payload.is_object() {
            return Err("terminal custody resume payload is invalid; record preserved".into());
        }
        if payload != &state.record.payload {
            let updated = Record {
                version: state.record.version,
                key: state.record.key.clone(),
                seed_sha256: state.record.seed_sha256.clone(),
                payload_sha256: payload_sha256(payload),
                payload: payload.clone(),
            };
            let encoded = serde_json::to_string(&updated).map_err(|error| error.to_string())?;
            runtime_store::atomic_write(&self.path, &encoded)?;
            // Track a completed rename even when the subsequent directory
            // fsync fails, so the final attempt compares against those bytes.
            state.record = updated;
            state.encoded = encoded;
        }
        runtime_store::fsync_parent_dir(&self.path).map_err(|error| error.to_string())
    }

    pub(in crate::services::discord) fn persist(&self, payload: &Value) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        self.persist_locked(&mut state, payload)
    }

    fn finish(&self, payload: &Value, result: Result<bool, String>) -> Result<bool, String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if matches!(result, Ok(true)) {
            self.verify_current(&state)?;
            fs::remove_file(&self.path).map_err(|error| error.to_string())?;
            runtime_store::fsync_parent_dir(&self.path).map_err(|error| error.to_string())?;
            Ok(true)
        } else {
            self.persist_locked(&mut state, payload)?;
            result
        }
    }
}

fn root() -> Result<PathBuf, String> {
    runtime_store::runtime_root()
        .map(|root| root.join(DIRECTORY))
        .ok_or_else(|| "terminal custody runtime root unavailable".into())
}

fn record_path(root: &Path, key: &str) -> PathBuf {
    root.join(format!("{:x}.json", Sha256::digest(key.as_bytes())))
}

async fn lock(path: &Path) -> Result<DeliveryRecordLock, String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || lock_record_path(&path))
        .await
        .map_err(|error| error.to_string())?
}

/// A successful return means the exact retry payload reached a file and
/// `fsync_parent_dir` returned `Ok` (no flush on Windows). The initial snapshot cannot be replaced on retry;
/// receipt/cleanup progress recorded by the terminal adapter is preserved.
pub(in crate::services::discord) async fn persist(
    key: &str,
    payload: &Value,
) -> Result<(), String> {
    persist_at(&root()?, key, payload).await
}

async fn persist_at(root: &Path, key: &str, payload: &Value) -> Result<(), String> {
    if key.trim().is_empty() || !payload.is_object() {
        return Err("terminal custody requires episode identity and object payload".into());
    }
    let path = record_path(root, key);
    let _lock = lock(&path).await?;
    // lock_record_path creates the directory. Also persist its entry in the
    // runtime root before acknowledging the first record in that directory.
    runtime_store::fsync_parent_dir(root).map_err(|error| error.to_string())?;
    let initial_digest = payload_sha256(payload);
    let record = Record {
        version: 1,
        key: key.into(),
        seed_sha256: initial_digest.clone(),
        payload_sha256: initial_digest,
        payload: payload.clone(),
    };
    match fs::read_to_string(&path) {
        Ok(existing) => {
            let existing: Record =
                serde_json::from_str(&existing).map_err(|error| error.to_string())?;
            if !existing.valid()
                || existing.key != record.key
                || existing.seed_sha256 != record.seed_sha256
            {
                return Err(
                    "terminal custody episode payload conflicts with retained record".into(),
                );
            }
            return runtime_store::fsync_parent_dir(&path).map_err(|error| error.to_string());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    let encoded = serde_json::to_string(&record).map_err(|error| error.to_string())?;
    runtime_store::atomic_write(&path, &encoded)?;
    runtime_store::fsync_parent_dir(&path).map_err(|error| error.to_string())
}

/// Called by message_outbox_loop, including after restart. A failed resume
/// keeps custody; only the terminal adapter can attest that it is settled.
pub(crate) async fn drain(registry: &super::health::HealthRegistry) {
    let result = match root() {
        Ok(root) => {
            drain_with(&root, |mut payload, checkpoint| async move {
                let result = super::turn_bridge::resume_foreign_terminal_custody(
                    registry,
                    &mut payload,
                    &checkpoint,
                )
                .await;
                (payload, result)
            })
            .await
        }
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        tracing::warn!(%error, "detached terminal custody retained for retry");
    }
}

/// Whether a retained episode may belong to `channel`, read without taking record locks; a record
/// that does not parse counts, since its channel cannot be ruled out.
pub(in crate::services::discord) fn retains_channel(channel: u64) -> Result<bool, String> {
    retains_channel_at(&root()?, channel)
}

fn retains_channel_at(root: &Path, channel: u64) -> Result<bool, String> {
    observes_retained_at(root, false, |payload| {
        [
            "/channel_id",
            "/watcher_owner_channel_id",
            "/local/channel_id",
            "/local/watcher_owner_channel_id",
            "/local/logical_channel_id",
        ]
        .iter()
        .any(|id| payload.pointer(id).and_then(Value::as_u64) == Some(channel))
    })
}

fn observes_retained_at(
    root: &Path,
    exact_path: bool,
    mut holds: impl FnMut(&Value) -> bool,
) -> Result<bool, String> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.to_string()),
    };
    for entry in entries {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        if exact_path && !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file()) {
            return Ok(true);
        }
        let encoded = match fs::read_to_string(&path) {
            Ok(encoded) => encoded,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        let record = serde_json::from_str::<Record>(&encoded).ok();
        let Some(record) = record.filter(Record::valid) else {
            return Ok(true);
        };
        if (exact_path && record_path(root, &record.key) != path) || holds(&record.payload) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
pub(in crate::services::discord) async fn drain_for_test<F, Fut>(resume: F) -> Result<usize, String>
where
    F: FnMut(Value, Arc<CustodyCheckpoint>) -> Fut,
    Fut: std::future::Future<Output = (Value, Result<bool, String>)>,
{
    drain_with(&root()?, resume).await
}

async fn drain_with<F, Fut>(root: &Path, mut resume: F) -> Result<usize, String>
where
    F: FnMut(Value, Arc<CustodyCheckpoint>) -> Fut,
    Fut: std::future::Future<Output = (Value, Result<bool, String>)>,
{
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.to_string()),
    };
    // Round-robin batches keep one temporarily undeliverable episode from
    // starving later records. This cursor is scheduling only, never authority.
    let is_record = |entry: &std::io::Result<fs::DirEntry>| {
        entry.as_ref().map_or(true, |entry| {
            entry.path().extension().and_then(|value| value.to_str()) == Some("json")
        })
    };
    let count = entries.filter(is_record).count();
    if count == 0 {
        return Ok(0);
    }
    let start = NEXT_ENTRY.fetch_add(1, Ordering::Relaxed) % count;
    let entries = fs::read_dir(root).map_err(|error| error.to_string())?;
    let mut settled = 0;
    for entry in entries.filter(is_record).skip(start).take(BATCH_SIZE) {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let record_lock = lock(&path).await?;
        let encoded = match fs::read_to_string(&path) {
            Ok(encoded) => encoded,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        let record: Record = serde_json::from_str(&encoded).map_err(|error| error.to_string())?;
        if !record.valid() || record_path(root, &record.key) != path {
            return Err(
                "terminal custody version, identity or payload digest is invalid; record preserved"
                    .into(),
            );
        }
        let payload = record.payload.clone();
        let checkpoint = Arc::new(CustodyCheckpoint {
            path,
            state: Mutex::new(CheckpointState {
                record,
                encoded,
                lock: Some(record_lock),
            }),
        });
        let _attempt = CustodyAttempt(checkpoint.clone());
        let (payload, result) = resume(payload, checkpoint.clone()).await;
        if checkpoint.finish(&payload, result)? {
            settled += 1;
        }
    }
    Ok(settled)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod pg_tests;
