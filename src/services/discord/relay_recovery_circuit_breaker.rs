//! Durable, non-destructive circuit breaker for relay-dead watcher reattach.
//!
//! The ordinary auto-heal limiter is intentionally process-local and windowed.
//! That is useful for burst control, but it cannot bound a dead relay frontier
//! across dcserver restarts or successive windows. This sidecar records a
//! lifetime budget for one exact inflight episode without writing the inflight
//! row itself: circuit bookkeeping must not advance `updated_at` or
//! `save_generation` and masquerade as producer liveness.

use std::fs;
use std::path::{Path, PathBuf};

use poise::serenity_prelude::ChannelId;
use serde::{Deserialize, Serialize};

use super::{RelayRecoveryActionKind, RelayRecoveryApplySource, RelayRecoveryDecision, SharedData};
use crate::services::provider::ProviderKind;

const CIRCUIT_VERSION: u32 = 1;
const CIRCUIT_ALERT_DEDUPE_TTL_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RelayReattachEpisode {
    key: String,
    owner_user_id: u64,
    pin: super::super::inflight::InflightEpisodePin,
}

impl RelayReattachEpisode {
    fn from_state(state: &super::super::inflight::InflightTurnState) -> Self {
        let mut hasher = blake3::Hasher::new();
        for part in [
            state.provider.as_bytes(),
            &state.channel_id.to_be_bytes(),
            state.channel_name.as_deref().unwrap_or("").as_bytes(),
            &state.request_owner_user_id.to_be_bytes(),
            &state.user_msg_id.to_be_bytes(),
            &state.current_msg_id.to_be_bytes(),
            &state.finalizer_turn_id.to_be_bytes(),
            state.started_at.as_bytes(),
            state.tmux_session_name.as_deref().unwrap_or("").as_bytes(),
            state.session_id.as_deref().unwrap_or("").as_bytes(),
            state.output_path.as_deref().unwrap_or("").as_bytes(),
            state.input_fifo_path.as_deref().unwrap_or("").as_bytes(),
            state
                .runtime_kind
                .map(|kind| kind.as_str())
                .unwrap_or("")
                .as_bytes(),
            state.effective_relay_owner_kind().as_str().as_bytes(),
            &state.turn_start_offset.unwrap_or(u64::MAX).to_be_bytes(),
            &state.born_generation.to_be_bytes(),
            state.turn_nonce.as_deref().unwrap_or("").as_bytes(),
        ] {
            hasher.update(&(part.len() as u64).to_be_bytes());
            hasher.update(part);
        }
        Self {
            key: hasher.finalize().to_hex().to_string(),
            owner_user_id: state.request_owner_user_id,
            pin: super::super::inflight::InflightEpisodePin::from_state(state),
        }
    }

    pub(super) fn short_key(&self) -> &str {
        self.key.get(..16).unwrap_or(self.key.as_str())
    }

    pub(super) fn pin(&self) -> &super::super::inflight::InflightEpisodePin {
        &self.pin
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum CircuitReservation {
    Reserved {
        episode: RelayReattachEpisode,
        attempt: u32,
        orphaned_staged_alert_ids: Vec<i64>,
    },
    Open {
        episode: RelayReattachEpisode,
        open: CircuitOpenPin,
        alert_needed: bool,
        staged_alert_id: Option<i64>,
    },
    StaleIdentity,
    MissingInflight,
    IoError,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CircuitOpenPin {
    episode_key: String,
    baseline_relay_offset: u64,
    generation: u64,
}

impl CircuitOpenPin {
    fn from_record(record: &CircuitRecord) -> Self {
        Self {
            episode_key: record.episode_key.clone(),
            baseline_relay_offset: record.baseline_relay_offset,
            generation: record.open_generation,
        }
    }

    fn dedupe_suffix(&self) -> String {
        let short = self.episode_key.get(..16).unwrap_or(&self.episode_key);
        format!("{short}:{}:{}", self.baseline_relay_offset, self.generation)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CircuitRecord {
    version: u32,
    episode_key: String,
    baseline_relay_offset: u64,
    attempts: u32,
    alert_queued: bool,
    /// PG row committed as `held` but not yet confirmed deliverable. Keeping
    /// the id in the sidecar closes the crash window between the local
    /// obligation commit and the asynchronous `held -> pending` transition.
    #[serde(default)]
    staged_alert_id: Option<i64>,
    /// Held rows from an older frontier/episode are never deliverable. Retain
    /// them until PG confirms deletion so a transient cleanup failure cannot
    /// turn into an unbounded orphan leak.
    #[serde(default)]
    orphaned_staged_alert_ids: Vec<i64>,
    #[serde(default)]
    open_generation: u64,
}

struct CircuitFileLock {
    _file: fs::File,
}

impl Drop for CircuitFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn lock_path(path: &Path) -> Result<CircuitFileLock, String> {
    let path = path.with_extension("json.lock");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    Ok(CircuitFileLock { _file: file })
}

fn circuit_path(root: &Path, provider: &ProviderKind, channel_id: u64) -> PathBuf {
    root.join(provider.as_str())
        .join(format!("{channel_id}.json"))
}

fn load_record(path: &Path) -> Result<Option<CircuitRecord>, String> {
    match fs::read_to_string(path) {
        Ok(data) => serde_json::from_str(&data)
            .map(Some)
            .map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn persist_record(path: &Path, record: &CircuitRecord) -> Result<(), String> {
    let json = serde_json::to_string_pretty(record).map_err(|error| error.to_string())?;
    super::super::runtime_store::atomic_write(path, &json)
}

fn effective_frontier(
    state: &super::super::inflight::InflightTurnState,
    observed_relay_offset: u64,
) -> u64 {
    observed_relay_offset.max(state.last_watcher_relayed_offset.unwrap_or(0))
}

fn reserve_in_root(
    root: &Path,
    snapshot: &super::super::inflight::InflightTurnState,
    expected: &RelayReattachEpisode,
    observed_relay_offset: u64,
    max_attempts: u32,
) -> CircuitReservation {
    let current = RelayReattachEpisode::from_state(snapshot);
    if &current != expected {
        return CircuitReservation::StaleIdentity;
    }
    let Some(provider) = snapshot.provider_kind() else {
        return CircuitReservation::IoError;
    };
    let path = circuit_path(root, &provider, snapshot.channel_id);
    let Ok(_lock) = lock_path(&path) else {
        return CircuitReservation::IoError;
    };

    // The pre-lock snapshot pins the decision, but it is not authoritative:
    // another turn can replace inflight while this caller waits on the sidecar
    // flock. Re-read under that flock immediately before any record reset or
    // attempt spend. A replacement must reserve through the same lock, so once
    // B has installed and reserved, stale A can only observe B and fail closed.
    let Some(authoritative) =
        super::super::inflight::load_inflight_state_read_only(&provider, snapshot.channel_id)
    else {
        return CircuitReservation::MissingInflight;
    };
    if authoritative.provider_kind().as_ref() != Some(&provider)
        || authoritative.channel_id != snapshot.channel_id
        || RelayReattachEpisode::from_state(&authoritative) != *expected
    {
        return CircuitReservation::StaleIdentity;
    }

    let frontier = effective_frontier(&authoritative, observed_relay_offset);
    let mut record = match load_record(&path) {
        Ok(Some(record)) => record,
        Ok(None) => CircuitRecord {
            version: CIRCUIT_VERSION,
            episode_key: expected.key.clone(),
            baseline_relay_offset: frontier,
            attempts: 0,
            alert_queued: false,
            staged_alert_id: None,
            orphaned_staged_alert_ids: Vec::new(),
            open_generation: 1,
        },
        Err(_) => return CircuitReservation::IoError,
    };
    if record.version != CIRCUIT_VERSION {
        return CircuitReservation::IoError;
    }
    if record.episode_key != expected.key {
        if let Some(id) = record.staged_alert_id.take()
            && !record.orphaned_staged_alert_ids.contains(&id)
        {
            record.orphaned_staged_alert_ids.push(id);
        }
        record.episode_key = expected.key.clone();
        record.baseline_relay_offset = frontier;
        record.attempts = 0;
        record.alert_queued = false;
        record.open_generation = 1;
    } else if frontier > record.baseline_relay_offset {
        if let Some(id) = record.staged_alert_id.take()
            && !record.orphaned_staged_alert_ids.contains(&id)
        {
            record.orphaned_staged_alert_ids.push(id);
        }
        record.baseline_relay_offset = frontier;
        record.attempts = 0;
        record.alert_queued = false;
        record.open_generation = record.open_generation.saturating_add(1).max(1);
    } else if record.open_generation == 0 {
        record.open_generation = 1;
    }
    if record.attempts >= max_attempts {
        if persist_record(&path, &record).is_err() {
            return CircuitReservation::IoError;
        }
        return CircuitReservation::Open {
            episode: expected.clone(),
            open: CircuitOpenPin::from_record(&record),
            alert_needed: !record.alert_queued,
            staged_alert_id: record.staged_alert_id,
        };
    }
    record.attempts = record.attempts.saturating_add(1);
    if persist_record(&path, &record).is_err() {
        return CircuitReservation::IoError;
    }
    CircuitReservation::Reserved {
        episode: expected.clone(),
        attempt: record.attempts,
        orphaned_staged_alert_ids: record.orphaned_staged_alert_ids.clone(),
    }
}

pub(super) fn should_use_durable_circuit(
    action: RelayRecoveryActionKind,
    source: RelayRecoveryApplySource,
) -> bool {
    action == RelayRecoveryActionKind::ReattachWatcher && source != RelayRecoveryApplySource::Manual
}

pub(super) fn reserve_current_episode(
    provider: &ProviderKind,
    decision: &RelayRecoveryDecision,
    max_attempts: u32,
) -> CircuitReservation {
    let Some(state) =
        super::super::inflight::load_inflight_state_read_only(provider, decision.channel_id)
    else {
        return CircuitReservation::MissingInflight;
    };
    if decision.provider != provider.as_str()
        || decision.affected.provider != provider.as_str()
        || decision.affected.channel_id != decision.channel_id
        || decision.affected.finalizer_turn_id != Some(state.effective_finalizer_turn_id())
        || decision.affected.mailbox_active_user_msg_id != Some(state.user_msg_id)
        || decision.affected.tmux_session != state.tmux_session_name
    {
        return CircuitReservation::StaleIdentity;
    }
    let expected = RelayReattachEpisode::from_state(&state);
    let Some(root) = super::super::runtime_store::runtime_root()
        .map(|root| root.join("discord_relay_recovery_circuit"))
    else {
        return CircuitReservation::IoError;
    };
    reserve_in_root(
        &root,
        &state,
        &expected,
        decision.evidence.last_relay_offset,
        max_attempts,
    )
}

fn open_alert_cas_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &RelayReattachEpisode,
    open: &CircuitOpenPin,
    max_attempts: u32,
    staged_alert_id: i64,
) -> bool {
    let path = circuit_path(root, provider, channel_id);
    let Ok(_lock) = lock_path(&path) else {
        return false;
    };
    let Ok(Some(mut record)) = load_record(&path) else {
        return false;
    };
    let Some(authoritative) =
        super::super::inflight::load_inflight_state_read_only(provider, channel_id)
    else {
        return false;
    };
    if RelayReattachEpisode::from_state(&authoritative) != *expected
        || record.version != CIRCUIT_VERSION
        || record.episode_key != open.episode_key
        || record.baseline_relay_offset != open.baseline_relay_offset
        || record.open_generation != open.generation
        || record.attempts < max_attempts
    {
        return false;
    }
    let frontier = effective_frontier(&authoritative, 0);
    if frontier > open.baseline_relay_offset {
        record.baseline_relay_offset = frontier;
        record.attempts = 0;
        record.alert_queued = false;
        if let Some(id) = record.staged_alert_id.take()
            && !record.orphaned_staged_alert_ids.contains(&id)
        {
            record.orphaned_staged_alert_ids.push(id);
        }
        record.open_generation = record.open_generation.saturating_add(1).max(1);
        let _ = persist_record(&path, &record);
        return false;
    }
    if !record.alert_queued {
        record.alert_queued = true;
        record.staged_alert_id = Some(staged_alert_id);
        return persist_record(&path, &record).is_ok();
    }
    record.staged_alert_id == Some(staged_alert_id)
}

fn complete_staged_alert_if_matches(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    open: &CircuitOpenPin,
    staged_alert_id: i64,
) {
    let path = circuit_path(root, provider, channel_id);
    let Ok(_lock) = lock_path(&path) else {
        return;
    };
    let Ok(Some(mut record)) = load_record(&path) else {
        return;
    };
    if record.episode_key == open.episode_key
        && record.baseline_relay_offset == open.baseline_relay_offset
        && record.open_generation == open.generation
        && record.alert_queued
        && record.staged_alert_id == Some(staged_alert_id)
    {
        record.staged_alert_id = None;
        let _ = persist_record(&path, &record);
    }
}

pub(super) fn acknowledge_orphaned_staged_alert_cleanup(
    provider: &ProviderKind,
    channel_id: u64,
    staged_alert_id: i64,
) {
    let Some(root) = super::super::runtime_store::runtime_root()
        .map(|root| root.join("discord_relay_recovery_circuit"))
    else {
        return;
    };
    let path = circuit_path(&root, provider, channel_id);
    let Ok(_lock) = lock_path(&path) else {
        return;
    };
    let Ok(Some(mut record)) = load_record(&path) else {
        return;
    };
    let before = record.orphaned_staged_alert_ids.len();
    record
        .orphaned_staged_alert_ids
        .retain(|id| *id != staged_alert_id);
    if record.orphaned_staged_alert_ids.len() != before {
        let _ = persist_record(&path, &record);
    }
}

fn retain_orphaned_staged_alert_for_cleanup(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    staged_alert_id: i64,
) {
    let path = circuit_path(root, provider, channel_id);
    let Ok(_lock) = lock_path(&path) else {
        return;
    };
    let Ok(Some(mut record)) = load_record(&path) else {
        return;
    };
    if !record.orphaned_staged_alert_ids.contains(&staged_alert_id) {
        record.orphaned_staged_alert_ids.push(staged_alert_id);
        let _ = persist_record(&path, &record);
    }
}

fn reset_alert_queued_if_matches(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    open: &CircuitOpenPin,
) {
    let path = circuit_path(root, provider, channel_id);
    let Ok(_lock) = lock_path(&path) else {
        return;
    };
    let Ok(Some(mut record)) = load_record(&path) else {
        return;
    };
    if record.episode_key == open.episode_key
        && record.baseline_relay_offset == open.baseline_relay_offset
        && record.open_generation == open.generation
        && record.alert_queued
    {
        record.alert_queued = false;
        record.staged_alert_id = None;
        let _ = persist_record(&path, &record);
    }
}

fn owner_mention(owner_user_id: u64) -> String {
    if owner_user_id == 0
        || owner_user_id == super::super::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID
    {
        String::new()
    } else {
        format!(" <@{owner_user_id}>")
    }
}

pub(super) struct CircuitAlertRequest {
    target: String,
    content: String,
    reason_code: String,
}

#[async_trait::async_trait]
pub(super) trait CircuitAlertEnqueue: Sync {
    async fn enqueue(
        &self,
        pool: Option<&sqlx::PgPool>,
        request: &CircuitAlertRequest,
    ) -> Result<i64, String>;

    /// Returns true when the row was activated or had already reached a
    /// non-held terminal/deliverable state. False means the staged id vanished
    /// and the local obligation must be reopened.
    async fn activate(&self, pool: Option<&sqlx::PgPool>, id: i64) -> Result<bool, String>;

    async fn cancel(&self, pool: Option<&sqlx::PgPool>, id: i64) -> Result<(), String>;
}

pub(super) struct PgCircuitAlertEnqueue;

#[async_trait::async_trait]
impl CircuitAlertEnqueue for PgCircuitAlertEnqueue {
    async fn enqueue(
        &self,
        pool: Option<&sqlx::PgPool>,
        request: &CircuitAlertRequest,
    ) -> Result<i64, String> {
        let pool = pool.ok_or_else(|| "pg_pool unavailable".to_string())?;
        crate::services::message_outbox::stage_outbox_pg_with_ttl(
            pool,
            crate::services::message_outbox::OutboxMessage {
                target: &request.target,
                content: &request.content,
                bot: crate::services::discord::bot_role::UtilityBotRole::Announce.alias(),
                source: "stall_watchdog",
                reason_code: Some(&request.reason_code),
                session_key: None,
            },
            CIRCUIT_ALERT_DEDUPE_TTL_SECS,
        )
        .await
        .map_err(|error| error.to_string())
    }

    async fn activate(&self, pool: Option<&sqlx::PgPool>, id: i64) -> Result<bool, String> {
        let pool = pool.ok_or_else(|| "pg_pool unavailable".to_string())?;
        crate::services::message_outbox::activate_or_confirm_staged_outbox_pg(pool, id)
            .await
            .map_err(|error| error.to_string())
    }

    async fn cancel(&self, pool: Option<&sqlx::PgPool>, id: i64) -> Result<(), String> {
        let pool = pool.ok_or_else(|| "pg_pool unavailable".to_string())?;
        crate::services::message_outbox::cancel_staged_outbox_pg(pool, id)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

pub(super) async fn queue_open_alert_once(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    episode: &RelayReattachEpisode,
    open: &CircuitOpenPin,
    max_attempts: u32,
) {
    queue_open_alert_once_with_enqueue(
        shared,
        provider,
        channel_id,
        episode,
        open,
        max_attempts,
        &PgCircuitAlertEnqueue,
    )
    .await;
}

pub(super) async fn queue_open_alert_once_with_enqueue(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    episode: &RelayReattachEpisode,
    open: &CircuitOpenPin,
    max_attempts: u32,
    enqueue: &dyn CircuitAlertEnqueue,
) {
    queue_or_resume_open_alert_with_enqueue(
        shared,
        provider,
        channel_id,
        episode,
        open,
        max_attempts,
        None,
        enqueue,
    )
    .await;
}

pub(super) async fn queue_or_resume_open_alert_with_enqueue(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    episode: &RelayReattachEpisode,
    open: &CircuitOpenPin,
    max_attempts: u32,
    resume_staged_alert_id: Option<i64>,
    enqueue: &dyn CircuitAlertEnqueue,
) {
    let Some(root) = super::super::runtime_store::runtime_root()
        .map(|root| root.join("discord_relay_recovery_circuit"))
    else {
        return;
    };
    let target = format!("channel:{}", channel_id.get());
    let reason_code = format!("relay_reattach_circuit_open:{}", open.dedupe_suffix());
    let mention = owner_mention(episode.owner_user_id);
    let request = CircuitAlertRequest {
        target,
        reason_code,
        content: format!(
            "⚠️ 릴레이 자동 복구 중단{mention}: 같은 세션 backlog가 {max_attempts}회 reattach 뒤에도 전달 frontier를 전진시키지 못했습니다. 세션과 inflight는 보존했으며 자동 redrive만 차단했습니다. 채널 {channel_id} 상태를 확인해 수동 복구 여부를 결정해 주세요."
        ),
    };
    // Stage first without any filesystem authority. `held` rows are invisible
    // to the outbox worker, so progress while this network await is in flight
    // cannot leak a stale alert.
    let staged_id = if let Some(id) = resume_staged_alert_id {
        id
    } else {
        match enqueue.enqueue(shared.pg_pool.as_ref(), &request).await {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(
                    target: "agentdesk::discord::relay_recovery",
                    provider = provider.as_str(),
                    channel_id = channel_id.get(),
                    episode = episode.short_key(),
                    error = %error,
                    "relay reattach circuit alert stage failed; will retry"
                );
                return;
            }
        }
    };

    // Revalidate under a short, synchronous flock and commit the local alert
    // obligation. No `.await` occurs while this guard is live.
    let validate_root = root.clone();
    let validate_provider = provider.clone();
    let validate_episode = episode.clone();
    let validate_open = open.clone();
    let validate_channel_id = channel_id.get();
    let valid = tokio::task::spawn_blocking(move || {
        super::super::inflight::lock_inflight_episode(
            &validate_provider,
            validate_channel_id,
            validate_episode.pin(),
        )
        .is_ok_and(|locked_episode| {
            debug_assert_eq!(
                RelayReattachEpisode::from_state(locked_episode.state()),
                validate_episode
            );
            open_alert_cas_in_root(
                &validate_root,
                &validate_provider,
                validate_channel_id,
                &validate_episode,
                &validate_open,
                max_attempts,
                staged_id,
            )
        })
    })
    .await
    .unwrap_or(false);
    if valid {
        match enqueue.activate(shared.pg_pool.as_ref(), staged_id).await {
            Ok(true) => {
                complete_staged_alert_if_matches(&root, provider, channel_id.get(), open, staged_id)
            }
            Ok(false) => {
                reset_alert_queued_if_matches(&root, provider, channel_id.get(), open);
                tracing::warn!(
                    target: "agentdesk::discord::relay_recovery",
                    provider = provider.as_str(),
                    channel_id = channel_id.get(),
                    staged_id,
                    "relay reattach circuit staged alert disappeared; reopening obligation"
                );
            }
            Err(error) => {
                tracing::warn!(
                    target: "agentdesk::discord::relay_recovery",
                    provider = provider.as_str(),
                    channel_id = channel_id.get(),
                    staged_id,
                    error = %error,
                    "relay reattach circuit alert activation failed; staged obligation retained"
                );
            }
        }
    } else if let Err(error) = enqueue.cancel(shared.pg_pool.as_ref(), staged_id).await {
        retain_orphaned_staged_alert_for_cleanup(&root, provider, channel_id.get(), staged_id);
        tracing::warn!(
            target: "agentdesk::discord::relay_recovery",
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            staged_id,
            error = %error,
            "stale relay reattach circuit held alert cancellation failed; durable cleanup retained"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    struct FailThenSucceedEnqueue {
        calls: AtomicUsize,
    }

    struct CrashAfterLocalCommitEnqueue {
        enqueue_calls: AtomicUsize,
        activate_calls: AtomicUsize,
    }

    struct ProgressThenCancelFailureEnqueue {
        provider: ProviderKind,
        progressed: super::super::super::inflight::InflightTurnState,
    }

    struct BlockingPgEnqueue {
        pool: sqlx::PgPool,
        reached: Arc<tokio::sync::Barrier>,
        resume: Arc<tokio::sync::Barrier>,
        reasons: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl CircuitAlertEnqueue for BlockingPgEnqueue {
        async fn enqueue(
            &self,
            _pool: Option<&sqlx::PgPool>,
            request: &CircuitAlertRequest,
        ) -> Result<i64, String> {
            self.reasons
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(request.reason_code.clone());
            self.reached.wait().await;
            self.resume.wait().await;
            PgCircuitAlertEnqueue
                .enqueue(Some(&self.pool), request)
                .await
        }

        async fn activate(&self, _pool: Option<&sqlx::PgPool>, id: i64) -> Result<bool, String> {
            PgCircuitAlertEnqueue.activate(Some(&self.pool), id).await
        }

        async fn cancel(&self, _pool: Option<&sqlx::PgPool>, id: i64) -> Result<(), String> {
            PgCircuitAlertEnqueue.cancel(Some(&self.pool), id).await
        }
    }

    #[async_trait::async_trait]
    impl CircuitAlertEnqueue for FailThenSucceedEnqueue {
        async fn enqueue(
            &self,
            _pool: Option<&sqlx::PgPool>,
            request: &CircuitAlertRequest,
        ) -> Result<i64, String> {
            assert!(request.target.starts_with("channel:"));
            assert!(
                request
                    .reason_code
                    .starts_with("relay_reattach_circuit_open:")
            );
            assert!(request.content.contains("자동 redrive만 차단"));
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("injected enqueue failure".to_string())
            } else {
                Ok(1)
            }
        }

        async fn activate(&self, _pool: Option<&sqlx::PgPool>, _id: i64) -> Result<bool, String> {
            Ok(true)
        }

        async fn cancel(&self, _pool: Option<&sqlx::PgPool>, _id: i64) -> Result<(), String> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl CircuitAlertEnqueue for CrashAfterLocalCommitEnqueue {
        async fn enqueue(
            &self,
            _pool: Option<&sqlx::PgPool>,
            _request: &CircuitAlertRequest,
        ) -> Result<i64, String> {
            self.enqueue_calls.fetch_add(1, Ordering::SeqCst);
            Ok(91)
        }

        async fn activate(&self, _pool: Option<&sqlx::PgPool>, id: i64) -> Result<bool, String> {
            assert_eq!(id, 91);
            if self.activate_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("injected process loss after local commit".to_string())
            } else {
                Ok(true)
            }
        }

        async fn cancel(&self, _pool: Option<&sqlx::PgPool>, _id: i64) -> Result<(), String> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl CircuitAlertEnqueue for ProgressThenCancelFailureEnqueue {
        async fn enqueue(
            &self,
            _pool: Option<&sqlx::PgPool>,
            _request: &CircuitAlertRequest,
        ) -> Result<i64, String> {
            super::super::super::inflight::save_inflight_state(&self.progressed)
                .expect("advance frontier while PG stage is in flight");
            Ok(92)
        }

        async fn activate(&self, _pool: Option<&sqlx::PgPool>, _id: i64) -> Result<bool, String> {
            panic!("stale staged alert must never activate")
        }

        async fn cancel(&self, _pool: Option<&sqlx::PgPool>, id: i64) -> Result<(), String> {
            assert_eq!(id, 92);
            assert_eq!(self.provider, ProviderKind::Codex);
            Err("injected PG cancellation outage".to_string())
        }
    }

    fn state(channel_id: u64) -> super::super::super::inflight::InflightTurnState {
        let mut state = super::super::super::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            None,
            343_742_347,
            channel_id + 1,
            channel_id + 2,
            "relay reattach circuit".to_string(),
            Some("provider-session-4465".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/relay-4465.jsonl".to_string()),
            None,
            0,
        );
        state.finalizer_turn_id = channel_id + 1;
        state.turn_nonce = Some(format!("nonce-{channel_id}"));
        state
    }

    fn decision_for_state(
        state: &super::super::super::inflight::InflightTurnState,
    ) -> RelayRecoveryDecision {
        let snapshot = super::super::RelayHealthSnapshot {
            provider: state.provider.clone(),
            channel_id: state.channel_id,
            active_turn: super::super::RelayActiveTurn::Foreground,
            tmux_session: state.tmux_session_name.clone(),
            tmux_alive: Some(true),
            watcher_attached: false,
            watcher_attached_stale: false,
            watcher_owner_channel_id: None,
            watcher_owns_live_relay: false,
            bridge_inflight_present: true,
            bridge_current_msg_id: Some(state.current_msg_id),
            mailbox_has_cancel_token: true,
            mailbox_active_user_msg_id: Some(state.user_msg_id),
            mailbox_turn_started_at_ms: None,
            queue_depth: 0,
            pending_discord_callback_msg_id: None,
            pending_thread_proof: false,
            parent_channel_id: None,
            thread_channel_id: None,
            last_relay_ts_ms: None,
            last_outbound_activity_ms: None,
            last_capture_offset: Some(64),
            last_relay_offset: state.last_watcher_relayed_offset.unwrap_or(0),
            unread_bytes: Some(64),
            desynced: true,
            stale_thread_proof: false,
        };
        let mut decision = super::super::plan_relay_recovery(
            &snapshot,
            super::super::RelayStallState::TmuxAliveRelayDead,
            chrono::Utc::now().timestamp_millis(),
        );
        decision.affected.finalizer_turn_id = Some(state.effective_finalizer_turn_id());
        decision
    }

    #[test]
    fn same_episode_is_bounded_forever_until_confirmed_frontier_progress() {
        let temp = tempfile::tempdir().expect("circuit root");
        let _env = crate::config::set_agentdesk_root_for_test(temp.path());
        let state = state(44_650);
        let episode = RelayReattachEpisode::from_state(&state);
        super::super::super::inflight::save_inflight_state(&state)
            .expect("seed authoritative inflight");
        assert!(matches!(
            reserve_in_root(temp.path(), &state, &episode, 0, 2),
            CircuitReservation::Reserved { attempt: 1, .. }
        ));
        assert!(matches!(
            reserve_in_root(temp.path(), &state, &episode, 0, 2),
            CircuitReservation::Reserved { attempt: 2, .. }
        ));
        assert!(matches!(
            reserve_in_root(temp.path(), &state, &episode, 0, 2),
            CircuitReservation::Open {
                episode: actual,
                alert_needed: true,
                ..
            } if actual == episode
        ));

        assert!(matches!(
            reserve_in_root(temp.path(), &state, &episode, 64, 2),
            CircuitReservation::Reserved { attempt: 1, .. }
        ));
    }

    #[test]
    fn exact_new_identity_resets_while_stale_identity_cannot_poison_it() {
        let temp = tempfile::tempdir().expect("circuit root");
        let _env = crate::config::set_agentdesk_root_for_test(temp.path());
        let old = state(44_651);
        let old_episode = RelayReattachEpisode::from_state(&old);
        super::super::super::inflight::save_inflight_state(&old)
            .expect("seed old authoritative inflight");
        assert!(matches!(
            reserve_in_root(temp.path(), &old, &old_episode, 0, 1),
            CircuitReservation::Reserved { .. }
        ));
        assert!(matches!(
            reserve_in_root(temp.path(), &old, &old_episode, 0, 1),
            CircuitReservation::Open { .. }
        ));

        let mut new = old.clone();
        new.session_id = Some("provider-session-4465-next".to_string());
        new.output_path = Some("/tmp/relay-4465-next.jsonl".to_string());
        new.turn_nonce = Some("nonce-next".to_string());
        let new_episode = RelayReattachEpisode::from_state(&new);
        super::super::super::inflight::save_inflight_state(&new)
            .expect("install new authoritative inflight");
        assert_eq!(
            reserve_in_root(temp.path(), &new, &old_episode, 0, 1),
            CircuitReservation::StaleIdentity
        );
        assert!(matches!(
            reserve_in_root(temp.path(), &new, &new_episode, 0, 1),
            CircuitReservation::Reserved { attempt: 1, .. }
        ));
    }

    #[test]
    fn old_snapshot_cannot_reset_or_spend_after_replacement_reserves() {
        let temp = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(temp.path());
        let provider = ProviderKind::Codex;
        let old = state(44_655);
        let old_episode = RelayReattachEpisode::from_state(&old);

        // A has already snapshotted `old`. B installs a replacement that keeps
        // every external routing id but changes the exact provider episode.
        let mut replacement = old.clone();
        replacement.session_id = Some("provider-session-4465-b".to_string());
        replacement.output_path = Some("/tmp/relay-4465-b.jsonl".to_string());
        replacement.turn_nonce = Some("nonce-b".to_string());
        let replacement_episode = RelayReattachEpisode::from_state(&replacement);
        super::super::super::inflight::save_inflight_state(&replacement)
            .expect("B installs authoritative replacement inflight");
        assert!(matches!(
            reserve_in_root(temp.path(), &replacement, &replacement_episode, 0, 3),
            CircuitReservation::Reserved { attempt: 1, .. }
        ));
        let record_path = circuit_path(temp.path(), &provider, replacement.channel_id);
        let record_before = std::fs::read(&record_path).expect("B circuit record");

        // A resumes from its stale snapshot only after B installed and spent.
        assert_eq!(
            reserve_in_root(temp.path(), &old, &old_episode, 0, 3),
            CircuitReservation::StaleIdentity
        );
        assert_eq!(
            std::fs::read(&record_path).expect("circuit record after stale A"),
            record_before,
            "stale A must not reset B's episode or spend B's attempt budget"
        );
    }

    #[test]
    fn same_external_ids_still_pin_session_output_and_nonce_axes() {
        let temp = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(temp.path());
        let original = state(44_656);
        let original_episode = RelayReattachEpisode::from_state(&original);
        let variants = [
            {
                let mut changed = original.clone();
                changed.session_id = Some("provider-session-4465-session-axis".to_string());
                changed
            },
            {
                let mut changed = original.clone();
                changed.output_path = Some("/tmp/relay-4465-output-axis.jsonl".to_string());
                changed
            },
            {
                let mut changed = original.clone();
                changed.turn_nonce = Some("nonce-axis".to_string());
                changed
            },
            {
                let mut changed = original.clone();
                changed.request_owner_user_id += 1;
                changed
            },
            {
                let mut changed = original.clone();
                changed.current_msg_id += 1;
                changed
            },
            {
                let mut changed = original.clone();
                changed.input_fifo_path = Some("/tmp/relay-4465-input-axis".to_string());
                changed
            },
            {
                let mut changed = original.clone();
                changed.runtime_kind =
                    Some(crate::services::agent_protocol::RuntimeHandoffKind::ProcessBackend);
                changed
            },
            {
                let mut changed = original.clone();
                changed
                    .set_relay_owner_kind(super::super::super::inflight::RelayOwnerKind::Watcher);
                changed
            },
            {
                let mut changed = original.clone();
                changed.channel_name = Some("replacement-channel".to_string());
                changed
            },
        ];

        for changed in variants {
            assert_eq!(changed.channel_id, original.channel_id);
            assert_eq!(changed.user_msg_id, original.user_msg_id);
            assert_eq!(changed.finalizer_turn_id, original.finalizer_turn_id);
            assert_eq!(changed.tmux_session_name, original.tmux_session_name);
            assert_ne!(
                RelayReattachEpisode::from_state(&changed),
                original_episode,
                "each hidden provider episode axis must change the exact fingerprint"
            );
        }
    }

    #[test]
    fn stale_decision_cannot_spend_the_replacement_turn_budget() {
        let temp = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(temp.path());
        let provider = ProviderKind::Codex;
        let old = state(44_654);
        let stale_decision = decision_for_state(&old);

        let mut replacement = old.clone();
        replacement.user_msg_id += 100;
        replacement.finalizer_turn_id = replacement.user_msg_id;
        replacement.session_id = Some("provider-session-4465-replacement".to_string());
        replacement.turn_nonce = Some("nonce-replacement".to_string());
        super::super::super::inflight::save_inflight_state(&replacement)
            .expect("seed replacement inflight");

        assert_eq!(
            reserve_current_episode(&provider, &stale_decision, 1),
            CircuitReservation::StaleIdentity
        );
        let replacement_decision = decision_for_state(&replacement);
        assert!(matches!(
            reserve_current_episode(&provider, &replacement_decision, 1),
            CircuitReservation::Reserved { attempt: 1, .. }
        ));
    }

    #[test]
    fn alert_marker_is_exact_episode_scoped() {
        let temp = tempfile::tempdir().expect("circuit root");
        let _env = crate::config::set_agentdesk_root_for_test(temp.path());
        let old = state(44_652);
        let old_episode = RelayReattachEpisode::from_state(&old);
        super::super::super::inflight::save_inflight_state(&old)
            .expect("seed old authoritative inflight");
        assert!(matches!(
            reserve_in_root(temp.path(), &old, &old_episode, 0, 1),
            CircuitReservation::Reserved { .. }
        ));
        let CircuitReservation::Open { open, .. } =
            reserve_in_root(temp.path(), &old, &old_episode, 0, 1)
        else {
            panic!("old episode must be open");
        };
        assert!(open_alert_cas_in_root(
            temp.path(),
            &ProviderKind::Codex,
            old.channel_id,
            &old_episode,
            &open,
            1,
            77,
        ));
        assert!(matches!(
            reserve_in_root(temp.path(), &old, &old_episode, 0, 1),
            CircuitReservation::Open {
                alert_needed: false,
                ..
            }
        ));

        let mut new = old.clone();
        new.user_msg_id += 100;
        new.finalizer_turn_id = new.user_msg_id;
        let new_episode = RelayReattachEpisode::from_state(&new);
        super::super::super::inflight::save_inflight_state(&new)
            .expect("install new authoritative inflight");
        assert!(!open_alert_cas_in_root(
            temp.path(),
            &ProviderKind::Codex,
            new.channel_id,
            &old_episode,
            &open,
            1,
            77,
        ));
        assert!(matches!(
            reserve_in_root(temp.path(), &new, &new_episode, 0, 1),
            CircuitReservation::Reserved { .. }
        ));
    }

    #[test]
    fn manual_lane_never_uses_durable_circuit() {
        assert!(!should_use_durable_circuit(
            RelayRecoveryActionKind::ReattachWatcher,
            RelayRecoveryApplySource::Manual
        ));
        assert!(should_use_durable_circuit(
            RelayRecoveryActionKind::ReattachWatcher,
            RelayRecoveryApplySource::ProbeAutoHeal
        ));
        assert!(should_use_durable_circuit(
            RelayRecoveryActionKind::ReattachWatcher,
            RelayRecoveryApplySource::StallWatchdog
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pg_alert_is_actionable_and_durably_one_shot_per_episode() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_relay_reattach_circuit_alert",
            "relay reattach circuit alert tests",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        let temp = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(temp.path());
        let shared =
            super::super::super::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(44_653);
        let state = state(channel.get());
        super::super::super::inflight::save_inflight_state(&state).expect("seed inflight");

        let decision = decision_for_state(&state);
        let reserved = reserve_current_episode(&provider, &decision, 1);
        assert!(matches!(
            reserved,
            CircuitReservation::Reserved { attempt: 1, .. }
        ));
        let open = reserve_current_episode(&provider, &decision, 1);
        let CircuitReservation::Open {
            episode,
            open,
            alert_needed: true,
            ..
        } = open
        else {
            panic!("second unchanged-frontier attempt must open the circuit");
        };

        queue_open_alert_once(&shared, &provider, channel, &episode, &open, 1).await;
        assert!(matches!(
            reserve_current_episode(&provider, &decision, 1),
            CircuitReservation::Open {
                alert_needed: false,
                ..
            }
        ));

        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT bot, source, reason_code, content
               FROM message_outbox
              WHERE target = $1",
        )
        .bind(format!("channel:{}", channel.get()))
        .fetch_all(&pool)
        .await
        .expect("load circuit alert row");
        assert_eq!(rows.len(), 1, "one episode must queue one actionable alert");
        assert_eq!(rows[0].0, "announce");
        assert_eq!(rows[0].1, "stall_watchdog");
        assert!(rows[0].2.starts_with("relay_reattach_circuit_open:"));
        assert!(rows[0].3.contains("자동 redrive만 차단"));
        assert!(rows[0].3.contains("<@343742347>"));
        let outbox_id: i64 = sqlx::query_scalar(
            "SELECT id FROM message_outbox WHERE target=$1 AND status='pending'",
        )
        .bind(format!("channel:{}", channel.get()))
        .fetch_one(&pool)
        .await
        .expect("load activated circuit alert id");
        assert!(
            crate::services::message_outbox::activate_or_confirm_staged_outbox_pg(&pool, outbox_id)
                .await
                .expect("confirm already activated row")
        );
        assert!(
            !crate::services::message_outbox::activate_or_confirm_staged_outbox_pg(
                &pool,
                outbox_id + 1_000_000,
            )
            .await
            .expect("missing staged row is not a completed obligation")
        );
    }

    #[tokio::test]
    async fn alert_enqueue_failure_stays_pending_and_retry_marks_only_alert_flag() {
        let temp = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(temp.path());
        let shared = super::super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(44_657);
        let state = state(channel.get());
        super::super::super::inflight::save_inflight_state(&state).expect("seed inflight");
        let decision = decision_for_state(&state);
        let CircuitReservation::Reserved { episode, .. } =
            reserve_current_episode(&provider, &decision, 1)
        else {
            panic!("first attempt must reserve");
        };
        let circuit_root = super::super::super::runtime_store::runtime_root()
            .expect("runtime root")
            .join("discord_relay_recovery_circuit");
        let record_path = circuit_path(&circuit_root, &provider, channel.get());
        let record_before = load_record(&record_path)
            .expect("load record")
            .expect("record exists");
        let inflight_before = serde_json::to_value(
            super::super::super::inflight::load_inflight_state_read_only(&provider, channel.get())
                .expect("inflight before alert retry"),
        )
        .expect("serialize inflight");
        let enqueue = FailThenSucceedEnqueue {
            calls: AtomicUsize::new(0),
        };

        let CircuitReservation::Open { open, .. } =
            reserve_current_episode(&provider, &decision, 1)
        else {
            panic!("episode must be open before alert enqueue");
        };

        queue_open_alert_once_with_enqueue(
            &shared, &provider, channel, &episode, &open, 1, &enqueue,
        )
        .await;
        assert!(matches!(
            reserve_current_episode(&provider, &decision, 1),
            CircuitReservation::Open {
                alert_needed: true,
                ..
            }
        ));
        let after_failure = load_record(&record_path)
            .expect("load after failure")
            .expect("record after failure");
        assert_eq!(after_failure.attempts, record_before.attempts);
        assert_eq!(after_failure.episode_key, record_before.episode_key);
        assert_eq!(
            after_failure.baseline_relay_offset,
            record_before.baseline_relay_offset
        );
        assert!(!after_failure.alert_queued);

        queue_open_alert_once_with_enqueue(
            &shared, &provider, channel, &episode, &open, 1, &enqueue,
        )
        .await;
        assert!(matches!(
            reserve_current_episode(&provider, &decision, 1),
            CircuitReservation::Open {
                alert_needed: false,
                ..
            }
        ));
        let after_success = load_record(&record_path)
            .expect("load after success")
            .expect("record after success");
        assert_eq!(after_success.attempts, record_before.attempts);
        assert_eq!(after_success.episode_key, record_before.episode_key);
        assert_eq!(
            after_success.baseline_relay_offset,
            record_before.baseline_relay_offset
        );
        assert!(after_success.alert_queued);
        assert_eq!(enqueue.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            shared.tmux_watchers.len(),
            0,
            "alert retry must never reattach"
        );
        assert_eq!(
            serde_json::to_value(
                super::super::super::inflight::load_inflight_state_read_only(
                    &provider,
                    channel.get(),
                )
                .expect("inflight survives alert retry"),
            )
            .expect("serialize inflight after"),
            inflight_before,
            "alert failure/retry must not mutate inflight state"
        );
    }

    #[tokio::test]
    async fn crash_after_local_alert_commit_resumes_same_held_row_without_reenqueue() {
        let temp = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(temp.path());
        let shared = super::super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(44_659);
        let state = state(channel.get());
        super::super::super::inflight::save_inflight_state(&state).expect("seed inflight");
        let decision = decision_for_state(&state);
        assert!(matches!(
            reserve_current_episode(&provider, &decision, 1),
            CircuitReservation::Reserved { attempt: 1, .. }
        ));
        let CircuitReservation::Open {
            episode,
            open,
            alert_needed: true,
            staged_alert_id: None,
        } = reserve_current_episode(&provider, &decision, 1)
        else {
            panic!("episode must open before staging");
        };
        let enqueue = CrashAfterLocalCommitEnqueue {
            enqueue_calls: AtomicUsize::new(0),
            activate_calls: AtomicUsize::new(0),
        };

        queue_or_resume_open_alert_with_enqueue(
            &shared, &provider, channel, &episode, &open, 1, None, &enqueue,
        )
        .await;
        let CircuitReservation::Open {
            alert_needed: false,
            staged_alert_id: Some(staged_id),
            ..
        } = reserve_current_episode(&provider, &decision, 1)
        else {
            panic!("failed activation must retain the exact staged obligation");
        };
        assert_eq!(staged_id, 91);

        queue_or_resume_open_alert_with_enqueue(
            &shared,
            &provider,
            channel,
            &episode,
            &open,
            1,
            Some(staged_id),
            &enqueue,
        )
        .await;
        assert!(matches!(
            reserve_current_episode(&provider, &decision, 1),
            CircuitReservation::Open {
                alert_needed: false,
                staged_alert_id: None,
                ..
            }
        ));
        assert_eq!(enqueue.enqueue_calls.load(Ordering::SeqCst), 1);
        assert_eq!(enqueue.activate_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn progressed_episode_retains_held_alert_until_cleanup_is_acknowledged() {
        let temp = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(temp.path());
        let shared = super::super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(44_660);
        let state = state(channel.get());
        super::super::super::inflight::save_inflight_state(&state).expect("seed inflight");
        let decision = decision_for_state(&state);
        assert!(matches!(
            reserve_current_episode(&provider, &decision, 1),
            CircuitReservation::Reserved { .. }
        ));
        let CircuitReservation::Open { episode, open, .. } =
            reserve_current_episode(&provider, &decision, 1)
        else {
            panic!("episode must open");
        };
        let enqueue = CrashAfterLocalCommitEnqueue {
            enqueue_calls: AtomicUsize::new(0),
            activate_calls: AtomicUsize::new(0),
        };
        queue_or_resume_open_alert_with_enqueue(
            &shared, &provider, channel, &episode, &open, 1, None, &enqueue,
        )
        .await;

        let mut progressed = state.clone();
        progressed.last_watcher_relayed_offset = Some(64);
        super::super::super::inflight::save_inflight_state(&progressed)
            .expect("persist frontier progress");
        let progressed_decision = decision_for_state(&progressed);
        let CircuitReservation::Reserved {
            orphaned_staged_alert_ids,
            ..
        } = reserve_current_episode(&provider, &progressed_decision, 1)
        else {
            panic!("progress must reopen the attempt budget");
        };
        assert_eq!(orphaned_staged_alert_ids, vec![91]);

        acknowledge_orphaned_staged_alert_cleanup(&provider, channel.get(), 91);
        let CircuitReservation::Open { .. } =
            reserve_current_episode(&provider, &progressed_decision, 1)
        else {
            panic!("second unchanged attempt must open");
        };
        let path = circuit_path(
            &super::super::super::runtime_store::runtime_root()
                .expect("runtime root")
                .join("discord_relay_recovery_circuit"),
            &provider,
            channel.get(),
        );
        assert!(
            load_record(&path)
                .expect("load circuit")
                .expect("record")
                .orphaned_staged_alert_ids
                .is_empty()
        );
    }

    #[tokio::test]
    async fn stale_stage_cancel_failure_becomes_a_durable_cleanup_obligation() {
        let temp = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(temp.path());
        let shared = super::super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(44_661);
        let state = state(channel.get());
        super::super::super::inflight::save_inflight_state(&state).expect("seed inflight");
        let decision = decision_for_state(&state);
        assert!(matches!(
            reserve_current_episode(&provider, &decision, 1),
            CircuitReservation::Reserved { .. }
        ));
        let CircuitReservation::Open { episode, open, .. } =
            reserve_current_episode(&provider, &decision, 1)
        else {
            panic!("episode must open");
        };
        let mut progressed = state.clone();
        progressed.last_watcher_relayed_offset = Some(128);
        let enqueue = ProgressThenCancelFailureEnqueue {
            provider: provider.clone(),
            progressed: progressed.clone(),
        };

        queue_or_resume_open_alert_with_enqueue(
            &shared, &provider, channel, &episode, &open, 1, None, &enqueue,
        )
        .await;

        let progressed_decision = decision_for_state(&progressed);
        let CircuitReservation::Reserved {
            orphaned_staged_alert_ids,
            ..
        } = reserve_current_episode(&provider, &progressed_decision, 1)
        else {
            panic!("frontier progress must reopen the circuit budget");
        };
        assert_eq!(orphaned_staged_alert_ids, vec![92]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn progress_during_pg_stage_cancels_stale_alert_and_reopen_is_distinct() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_relay_reattach_circuit_linearization",
            "relay reattach circuit linearization test",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        let temp = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(temp.path());
        let shared =
            super::super::super::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(44_658);
        let original = state(channel.get());
        super::super::super::inflight::save_inflight_state(&original).expect("seed inflight");
        let decision = decision_for_state(&original);
        assert!(matches!(
            reserve_current_episode(&provider, &decision, 1),
            CircuitReservation::Reserved { attempt: 1, .. }
        ));
        let CircuitReservation::Open {
            episode,
            open: first_open,
            alert_needed: true,
            ..
        } = reserve_current_episode(&provider, &decision, 1)
        else {
            panic!("first cycle must open");
        };
        let reached = Arc::new(tokio::sync::Barrier::new(2));
        let resume = Arc::new(tokio::sync::Barrier::new(2));
        let old_enqueue = Arc::new(BlockingPgEnqueue {
            pool: pool.clone(),
            reached: reached.clone(),
            resume: resume.clone(),
            reasons: Mutex::new(Vec::new()),
        });
        let old_task = {
            let shared = shared.clone();
            let provider = provider.clone();
            let episode = episode.clone();
            let first_open = first_open.clone();
            let old_enqueue = old_enqueue.clone();
            tokio::spawn(async move {
                queue_open_alert_once_with_enqueue(
                    &shared,
                    &provider,
                    channel,
                    &episode,
                    &first_open,
                    1,
                    old_enqueue.as_ref(),
                )
                .await;
            })
        };

        reached.wait().await;
        let mut progressed =
            super::super::super::inflight::load_inflight_state_read_only(&provider, channel.get())
                .expect("load episode for progress");
        progressed.last_watcher_relayed_offset = Some(64);
        let progress_writer = tokio::task::spawn_blocking(move || {
            super::super::super::inflight::save_inflight_state(&progressed)
                .expect("persist authoritative frontier progress");
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            progress_writer.is_finished(),
            "PG stage await must not hold the canonical inflight flock or block progress"
        );
        progress_writer.await.expect("frontier writer");

        resume.wait().await;
        old_task.await.expect("old enqueue task");
        let stale_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM message_outbox WHERE target=$1 AND status IN ('held','pending')",
        )
        .bind(format!("channel:{}", channel.get()))
        .fetch_one(&pool)
        .await
        .expect("count stale staged alerts");
        assert_eq!(
            stale_rows, 0,
            "progressed cycle must leave no deliverable or held stale alert"
        );
        let progressed =
            super::super::super::inflight::load_inflight_state_read_only(&provider, channel.get())
                .expect("reload progressed episode");
        let progressed_decision = decision_for_state(&progressed);
        assert!(matches!(
            reserve_current_episode(&provider, &progressed_decision, 1),
            CircuitReservation::Reserved { attempt: 1, .. }
        ));
        let CircuitReservation::Open {
            episode: reopened_episode,
            open: second_open,
            alert_needed: true,
            ..
        } = reserve_current_episode(&provider, &progressed_decision, 1)
        else {
            panic!("progressed cycle must legitimately re-open");
        };
        assert_ne!(first_open, second_open);
        assert_ne!(first_open.dedupe_suffix(), second_open.dedupe_suffix());
        queue_open_alert_once(
            &shared,
            &provider,
            channel,
            &reopened_episode,
            &second_open,
            1,
        )
        .await;
        assert!(matches!(
            reserve_current_episode(&provider, &progressed_decision, 1),
            CircuitReservation::Open {
                alert_needed: false,
                ..
            }
        ));
        let old_reason = old_enqueue
            .reasons
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())[0]
            .clone();
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT reason_code,status FROM message_outbox WHERE target = $1 ORDER BY id",
        )
        .bind(format!("channel:{}", channel.get()))
        .fetch_all(&pool)
        .await
        .expect("load linearized circuit alerts");
        assert_eq!(
            rows.len(),
            1,
            "only the valid reopened cycle is deliverable"
        );
        assert_eq!(rows[0].1, "pending");
        let new_reason = rows[0].0.clone();
        assert_ne!(
            old_reason, new_reason,
            "progressed re-open needs a distinct dedupe key"
        );
    }
}
