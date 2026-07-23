//! #4181 item-2: monotonic no-progress grace for the redrive destructive
//! trigger, extracted from `stall_liveness` so the parent module stays under
//! the 1000-prod-line giant threshold (the structural split of the
//! stall/liveness judgment authority is tracked by #4615).
//!
//! The redrive no-progress grace measures how long the committed relay offset
//! has stayed frozen. It gates a *destructive* redrive, so the WHOLE lifecycle —
//! grace judgment AND TTL garbage-collection — runs on a process-monotonic
//! clock with ZERO wall-clock dependence. A forward NTP/wall-clock step must be
//! unable to (a) inflate the frozen-duration and fire a redrive early, or
//! (b) evict a monotonically-recent observation and re-arm the grace. The clock
//! is injected via the `RedriveClock` trait rather than a `#[cfg]`-split free
//! function, so the production `MonotonicRedriveClock` is compiled in every
//! build and the real decision + GC path is what tests exercise (with a fake
//! clock). Observations live in a dedicated map, isolated from the wall-clock
//! liveness observations in the parent module, and are updated under the
//! DashMap entry lock so concurrent watchdog passes for the same key serialize.
//!
//! The production clock uses the OS `CLOCK_MONOTONIC` domain, which remains
//! comparable across a dcserver restart within one machine boot (and excludes
//! suspend time). The current episode baseline is stored outside inflight and
//! restored only when boot/provider/channel/turn/frontier identity matches, so a
//! process restart neither re-arms grace nor imports a stale episode or prior-boot
//! timestamp.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use dashmap::mapref::entry::Entry;
use poise::serenity_prelude::ChannelId;
use serde::{Deserialize, Serialize};

use crate::services::provider::ProviderKind;

use super::super::snapshot::WatcherStateSnapshot;
use super::{
    STALL_LIVENESS_STATE_TTL_SECS, STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS, StallLivenessKey,
    live_undelivered_backlog,
};
use crate::services::discord::{RelayFrontierToken, runtime_store};

/// Monotonic seconds source for the redrive no-progress lifecycle (grace + TTL
/// GC). Injected — rather than `#[cfg]`-splitting a free function — so the
/// production reader (`MonotonicRedriveClock`) is compiled in every build and
/// tests drive the real decision + GC path with deterministic time.
trait RedriveClock {
    /// Monotonic seconds: non-decreasing within a process and immune to
    /// wall-clock/NTP steps. There is deliberately NO wall-clock accessor — the
    /// entire redrive no-progress lifecycle must be free of wall dependence.
    fn mono_secs(&self) -> i64;

    fn durable_mono_secs(&self) -> Option<i64> {
        None
    }
}

/// Process-start `Instant` fallback when `CLOCK_MONOTONIC` is unavailable. It
/// remains wall-clock independent but cannot carry an elapsed baseline across a
/// process restart, so durable restore fails closed to a fresh grace there.
static MONO_ANCHOR: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);

/// Production monotonic clock. Grace and persistence use `CLOCK_MONOTONIC` when
/// available; `mono_secs` stays as the injected/tested in-process reader.
struct MonotonicRedriveClock;

impl RedriveClock for MonotonicRedriveClock {
    fn mono_secs(&self) -> i64 {
        // #4181 item-2 F2: the relay_auto_heal / placeholder_reclaim redrive
        // integration tests drive the grace through the deep production call
        // chain (which threads no clock), so they inject their simulated `now`
        // as the monotonic reading via this override. The real `Instant` reader
        // below stays compiled AND is exercised whenever the override is unset
        // (see `redrive_production_monotonic_clock_path_runs_4181`). The unit
        // tests in this module instead inject a `FakeClock` directly through the
        // `RedriveClock` trait (`*_with_clock`), never touching this override.
        #[cfg(test)]
        {
            if let Some(secs) = TEST_CLOCK_OVERRIDE.with(|cell| cell.get()) {
                return secs;
            }
        }
        MONO_ANCHOR.elapsed().as_secs() as i64
    }

    fn durable_mono_secs(&self) -> Option<i64> {
        #[cfg(test)]
        if let Some(secs) = TEST_DURABLE_CLOCK_OVERRIDE.with(|cell| cell.get()) {
            return Some(secs);
        }
        os_monotonic_secs()
    }
}

#[cfg(test)]
thread_local! {
    static TEST_CLOCK_OVERRIDE: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
    static TEST_DURABLE_CLOCK_OVERRIDE: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
    static TEST_CANONICAL_EPISODE_OVERRIDE: std::cell::RefCell<Option<crate::services::discord::inflight::InflightTurnIdentity>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(in crate::services::discord::health) struct RedriveGraceTestClockGuard {
    previous: Option<i64>,
    previous_durable: Option<i64>,
    previous_episode: Option<crate::services::discord::inflight::InflightTurnIdentity>,
}

#[cfg(test)]
impl Drop for RedriveGraceTestClockGuard {
    fn drop(&mut self) {
        TEST_CLOCK_OVERRIDE.with(|cell| cell.set(self.previous));
        TEST_DURABLE_CLOCK_OVERRIDE.with(|cell| cell.set(self.previous_durable));
        TEST_CANONICAL_EPISODE_OVERRIDE
            .with(|episode| *episode.borrow_mut() = self.previous_episode.clone());
    }
}

/// Scoped monotonic override for deep redrive integration tests. Restores the
/// prior thread-local value on normal return or panic and is absent in production.
#[cfg(test)]
pub(in crate::services::discord::health) fn set_redrive_grace_test_clock(
    mono_secs: i64,
    episode_identity: Option<crate::services::discord::inflight::InflightTurnIdentity>,
) -> RedriveGraceTestClockGuard {
    let previous = TEST_CLOCK_OVERRIDE.with(|cell| cell.replace(Some(mono_secs)));
    let previous_durable = TEST_DURABLE_CLOCK_OVERRIDE.with(|cell| cell.replace(Some(mono_secs)));
    let previous_episode =
        TEST_CANONICAL_EPISODE_OVERRIDE.with(|episode| episode.replace(episode_identity));
    RedriveGraceTestClockGuard {
        previous,
        previous_durable,
        previous_episode,
    }
}

/// #4181 item-2 P3-2: clear the override so the production `Instant` reader runs.
/// Required under `--test-threads=1`, where a prior deep-chain test's override
/// would otherwise leak into the production-clock coverage test and silently
/// bypass the real `MONO_ANCHOR.elapsed()` reader.
#[cfg(test)]
fn clear_redrive_grace_test_clock() {
    TEST_CLOCK_OVERRIDE.with(|cell| cell.set(None));
    TEST_DURABLE_CLOCK_OVERRIDE.with(|cell| cell.set(None));
    TEST_CANONICAL_EPISODE_OVERRIDE.with(|episode| *episode.borrow_mut() = None);
}

/// Dedicated no-progress tracker, kept separate from the wall-clock
/// `OFFSET_OBSERVATIONS` the liveness path uses. Every timestamp here is
/// monotonic: the redrive lifecycle has zero wall dependence (#4181 item-2).
#[derive(Clone, Debug)]
struct NoProgressObservation {
    episode_identity: Option<crate::services::discord::inflight::InflightTurnIdentity>,
    turn_nonce: Option<String>,
    /// Highest committed relay offset observed for this key. A later snapshot
    /// reporting a LOWER offset is treated as stale and rejected, so a stale
    /// concurrent watchdog pass cannot rewind the freeze anchor (#4181 P3).
    offset: u64,
    /// Monotonic seconds at which `offset` last advanced — the grace anchor.
    unchanged_since_mono_secs: i64,
    /// Monotonic seconds of the last observation — the TTL-GC freshness anchor.
    /// Monotonic (not wall) so a forward wall-clock jump cannot evict a
    /// monotonically-recent observation and re-arm the grace (#4181 item-2 P2).
    last_seen_mono_secs: i64,
}

static NO_PROGRESS_OBSERVATIONS: LazyLock<
    dashmap::DashMap<(StallLivenessKey, u64), NoProgressObservation>,
> = LazyLock::new(dashmap::DashMap::new);
static DURABLE_BASELINE_LOCKS: LazyLock<dashmap::DashMap<(String, u64), Arc<Mutex<()>>>> =
    LazyLock::new(dashmap::DashMap::new);

const REDRIVE_BASELINES_DIR: &str = "discord_redrive_baselines";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct DurableNoProgressBaseline {
    boot_id: String,
    offset: u64,
    reset_incarnation: u64,
    identity: Option<crate::services::discord::inflight::InflightTurnIdentity>,
    turn_nonce: Option<String>,
    unchanged_since_monotonic_secs: i64,
    last_seen_monotonic_secs: i64,
}

impl DurableNoProgressBaseline {
    fn matches(
        &self,
        identity: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
        turn_nonce: Option<&str>,
        token: RelayFrontierToken,
        boot_id: &str,
    ) -> bool {
        self.boot_id == boot_id
            && self.offset == token.committed_offset
            && self.reset_incarnation == token.reset_incarnation
            && self.identity.as_ref() == identity
            && self.turn_nonce.as_deref() == turn_nonce
    }

    fn observation(&self, mono_now: i64) -> NoProgressObservation {
        NoProgressObservation {
            episode_identity: self.identity.clone(),
            turn_nonce: self.turn_nonce.clone(),
            offset: self.offset,
            unchanged_since_mono_secs: self.unchanged_since_monotonic_secs.min(mono_now),
            last_seen_mono_secs: mono_now,
        }
    }
}

fn durable_baseline_path(provider: &str, channel_id: ChannelId) -> Option<PathBuf> {
    runtime_store::runtime_root().map(|root| {
        root.join(REDRIVE_BASELINES_DIR)
            .join(provider)
            .join(format!("{}.json", channel_id.get()))
    })
}

fn load_durable_baseline_at(path: &Path) -> Option<DurableNoProgressBaseline> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn persist_durable_baseline_at(path: &Path, baseline: &DurableNoProgressBaseline) {
    let result = serde_json::to_string(baseline)
        .map_err(|error| error.to_string())
        .and_then(|json| runtime_store::atomic_write(path, &json));
    if let Err(error) = result {
        tracing::warn!(
            redrive_baseline_path = %path.display(),
            error = %error,
            "redrive no-progress baseline persistence failed; keeping process-local grace"
        );
    }
}

fn remove_durable_baseline_at(path: &Path) {
    if let Err(error) = fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            redrive_baseline_path = %path.display(),
            error = %error,
            "redrive no-progress baseline cleanup failed"
        );
    }
}

fn durable_baseline_lock(provider: &str, channel_id: u64) -> Arc<Mutex<()>> {
    DURABLE_BASELINE_LOCKS
        .entry((provider.to_string(), channel_id))
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn load_canonical_episode(
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Option<crate::services::discord::inflight::InflightTurnIdentity> {
    crate::services::discord::inflight::load_inflight_state_read_only(provider, channel_id.get())
        .map(|state| crate::services::discord::inflight::InflightTurnIdentity::from_state(&state))
}

#[cfg(not(test))]
fn canonical_episode(
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Option<crate::services::discord::inflight::InflightTurnIdentity> {
    load_canonical_episode(provider, channel_id)
}

#[cfg(test)]
fn canonical_episode(
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> Option<crate::services::discord::inflight::InflightTurnIdentity> {
    TEST_CANONICAL_EPISODE_OVERRIDE.with(|override_episode| {
        override_episode
            .borrow()
            .clone()
            .or_else(|| load_canonical_episode(provider, channel_id))
    })
}

fn canonical_episode_matches(
    provider: &ProviderKind,
    channel_id: ChannelId,
    identity: &crate::services::discord::inflight::InflightTurnIdentity,
) -> bool {
    canonical_episode(provider, channel_id).as_ref() == Some(identity)
}

fn clear_for_session_at(
    probe: &StallLivenessKey,
    requested_identity: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
    canonical: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
    path: Option<&Path>,
) {
    NO_PROGRESS_OBSERVATIONS.retain(|(key, _), observation| {
        if !key.matches_session(probe) {
            return true;
        }
        canonical.is_some_and(|identity| observation.episode_identity.as_ref() == Some(identity))
    });
    let Some(path) = path else {
        return;
    };
    let Some(baseline) = load_durable_baseline_at(path) else {
        return;
    };
    let owned_by_canonical =
        canonical.is_some_and(|identity| baseline.identity.as_ref() == Some(identity));
    let matches_request =
        requested_identity.is_none_or(|requested| baseline.identity.as_ref() == Some(requested));
    let matches_requested_session = baseline.identity.as_ref().is_some_and(|identity| {
        identity.tmux_session_name.as_deref() == probe.tmux_session.as_deref()
    });
    if matches_request && matches_requested_session && !owned_by_canonical {
        remove_durable_baseline_at(path);
    }
}

fn os_monotonic_secs() -> Option<i64> {
    #[cfg(unix)]
    {
        let mut value = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) } == 0 {
            return Some(value.tv_sec);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn boot_id() -> Option<String> {
    let mut size = 0usize;
    let name = c"kern.bootsessionuuid";
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size <= 1
    {
        return None;
    }
    let mut buffer = vec![0u8; size];
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    let end = buffer.iter().position(|byte| *byte == 0).unwrap_or(size);
    std::str::from_utf8(&buffer[..end])
        .ok()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(target_os = "linux")]
fn boot_id() -> Option<String> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn boot_id() -> Option<String> {
    None
}

/// Returns `true` iff the observed committed relay offset has stayed UNCHANGED
/// for at least `STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS` of MONOTONIC time.
///
/// The read-modify-write runs under the DashMap entry lock so concurrent
/// watchdog passes for the same key serialize (#4181 P3).
///
/// The map key includes the frontier reset incarnation. Every legitimate
/// downward reset publishes a distinct incarnation, so it automatically re-arms
/// at the new coordinate. A lower value inside one incarnation is necessarily a
/// stale snapshot and cannot rewind its monotonic freeze anchor.
fn relay_offset_stalled_past_grace(
    key: &StallLivenessKey,
    token: RelayFrontierToken,
    mono_now: i64,
) -> bool {
    let observed_offset = token.committed_offset;
    match NO_PROGRESS_OBSERVATIONS.entry((key.clone(), token.reset_incarnation)) {
        Entry::Occupied(mut occupied) => {
            let observation = occupied.get_mut();
            observation.last_seen_mono_secs = mono_now;
            if observed_offset > observation.offset {
                observation.offset = observed_offset;
                observation.unchanged_since_mono_secs = mono_now;
                false
            } else if observed_offset < observation.offset {
                false
            } else {
                mono_now.saturating_sub(observation.unchanged_since_mono_secs)
                    >= STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64
            }
        }
        Entry::Vacant(vacant) => {
            vacant.insert(NoProgressObservation {
                episode_identity: None,
                turn_nonce: None,
                offset: observed_offset,
                unchanged_since_mono_secs: mono_now,
                last_seen_mono_secs: mono_now,
            });
            false
        }
    }
}

fn relay_offset_stalled_past_grace_durable(
    provider: &ProviderKind,
    channel_id: ChannelId,
    key: &StallLivenessKey,
    snapshot: &WatcherStateSnapshot,
    token: RelayFrontierToken,
    mono_now: i64,
) -> bool {
    let Some(path) = durable_baseline_path(provider.as_str(), channel_id) else {
        return relay_offset_stalled_past_grace(key, token, mono_now);
    };
    let Some(boot_id) = boot_id() else {
        return relay_offset_stalled_past_grace(key, token, mono_now);
    };
    let channel_lock = durable_baseline_lock(provider.as_str(), channel_id.get());
    let _guard = channel_lock
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let Some(identity) = snapshot.inflight_identity.as_ref() else {
        return relay_offset_stalled_past_grace(key, token, mono_now);
    };
    let turn_nonce = snapshot.mailbox_active_turn_nonce.as_deref();
    if !canonical_episode_matches(provider, channel_id, identity) {
        return false;
    }
    let map_key = (key.clone(), token.reset_incarnation);
    let (stalled, observation) = match NO_PROGRESS_OBSERVATIONS.entry(map_key) {
        Entry::Occupied(mut occupied) => {
            let observation = occupied.get_mut();
            if observation.episode_identity.as_ref() != Some(identity)
                || observation.turn_nonce.as_deref() != turn_nonce
            {
                *observation = NoProgressObservation {
                    episode_identity: Some(identity.clone()),
                    turn_nonce: turn_nonce.map(str::to_string),
                    offset: token.committed_offset,
                    unchanged_since_mono_secs: mono_now,
                    last_seen_mono_secs: mono_now,
                };
            } else {
                observation.last_seen_mono_secs = mono_now;
                if token.committed_offset > observation.offset {
                    observation.offset = token.committed_offset;
                    observation.unchanged_since_mono_secs = mono_now;
                }
            }
            (
                token.committed_offset == observation.offset
                    && mono_now.saturating_sub(observation.unchanged_since_mono_secs)
                        >= STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64,
                observation.clone(),
            )
        }
        Entry::Vacant(vacant) => {
            let observation = load_durable_baseline_at(&path)
                .filter(|baseline| baseline.matches(Some(identity), turn_nonce, token, &boot_id))
                .map(|baseline| baseline.observation(mono_now))
                .unwrap_or(NoProgressObservation {
                    episode_identity: Some(identity.clone()),
                    turn_nonce: turn_nonce.map(str::to_string),
                    offset: token.committed_offset,
                    unchanged_since_mono_secs: mono_now,
                    last_seen_mono_secs: mono_now,
                });
            let stalled = mono_now.saturating_sub(observation.unchanged_since_mono_secs)
                >= STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64;
            vacant.insert(observation.clone());
            (stalled, observation)
        }
    };

    if !canonical_episode_matches(provider, channel_id, identity) {
        let map_key = (key.clone(), token.reset_incarnation);
        if NO_PROGRESS_OBSERVATIONS
            .get(&map_key)
            .is_some_and(|current| {
                current.episode_identity.as_ref() == Some(identity)
                    && current.turn_nonce.as_deref() == turn_nonce
            })
        {
            NO_PROGRESS_OBSERVATIONS.remove(&map_key);
        }
        return false;
    }
    persist_durable_baseline_at(
        &path,
        &DurableNoProgressBaseline {
            boot_id,
            offset: observation.offset,
            reset_incarnation: token.reset_incarnation,
            identity: Some(identity.clone()),
            turn_nonce: turn_nonce.map(str::to_string),
            unchanged_since_monotonic_secs: observation.unchanged_since_mono_secs,
            last_seen_monotonic_secs: observation.last_seen_mono_secs,
        },
    );
    stalled
}

/// #4181 item-2: a live undelivered backlog whose committed relay offset has
/// been frozen past the no-progress grace (monotonic), so the redrive
/// destructive trigger is eligible to fire. The production path measures time
/// with the process `MonotonicRedriveClock`.
///
/// `token` carries the current authoritative `confirmed_end_offset` together
/// with its reset incarnation, so a JSONL-rotation coordinate reset can be told
/// apart from a stale lagging snapshot (see `relay_offset_stalled_past_grace`).
pub(in crate::services::discord::health) fn stalled_undelivered_backlog_for_redrive(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    token: RelayFrontierToken,
) -> bool {
    stalled_undelivered_backlog_for_redrive_with_token_and_clock(
        provider,
        channel_id,
        snapshot,
        token,
        &MonotonicRedriveClock,
    )
}

fn stalled_undelivered_backlog_for_redrive_with_token_and_clock(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    token: RelayFrontierToken,
    clock: &dyn RedriveClock,
) -> bool {
    if !live_undelivered_backlog(snapshot) || snapshot.last_relay_offset != token.committed_offset {
        return false;
    }
    let key = StallLivenessKey::from_snapshot(provider, channel_id, snapshot);
    match (
        clock.durable_mono_secs(),
        snapshot.inflight_identity.as_ref(),
    ) {
        (Some(mono_now), Some(_)) => relay_offset_stalled_past_grace_durable(
            provider, channel_id, &key, snapshot, token, mono_now,
        ),
        _ => relay_offset_stalled_past_grace(&key, token, clock.mono_secs()),
    }
}

#[cfg(test)]
fn stalled_undelivered_backlog_for_redrive_with_clock(
    provider: &ProviderKind,
    channel_id: ChannelId,
    snapshot: &WatcherStateSnapshot,
    live_committed_offset: u64,
    clock: &dyn RedriveClock,
) -> bool {
    stalled_undelivered_backlog_for_redrive_with_token_and_clock(
        provider,
        channel_id,
        snapshot,
        RelayFrontierToken {
            reset_incarnation: 0,
            committed_offset: live_committed_offset,
        },
        clock,
    )
}

/// Drop redrive no-progress observations for a cleared session. Called by the
/// parent's `clear_stall_watchdog_liveness_state`.
pub(super) fn clear_for_session(probe: &StallLivenessKey) {
    let Some(provider) = ProviderKind::from_str(&probe.provider) else {
        return;
    };
    let channel_id = ChannelId::new(probe.channel_id);
    let channel_lock = durable_baseline_lock(&probe.provider, probe.channel_id);
    let _guard = channel_lock
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let canonical = canonical_episode(&provider, channel_id);
    let path = durable_baseline_path(&probe.provider, channel_id);
    clear_for_session_at(probe, None, canonical.as_ref(), path.as_deref());
}

/// TTL-GC on the MONOTONIC freshness anchor (production clock). Called by the
/// parent's `gc_stall_watchdog_liveness_state`. Using monotonic age (not wall)
/// means a forward wall-clock jump cannot evict a monotonically-recent
/// observation and re-arm the grace (#4181 item-2 P2).
pub(super) fn gc() {
    gc_with_clock(&MonotonicRedriveClock);
}

fn gc_with_clock(clock: &dyn RedriveClock) {
    let durable_now = clock.durable_mono_secs();
    let mono_now = durable_now.unwrap_or_else(|| clock.mono_secs());
    NO_PROGRESS_OBSERVATIONS.retain(|_, observation| {
        mono_now.saturating_sub(observation.last_seen_mono_secs)
            <= STALL_LIVENESS_STATE_TTL_SECS as i64
    });
    let Some(boot_id) = boot_id() else {
        return;
    };
    let Some(root) = runtime_store::runtime_root().map(|root| root.join(REDRIVE_BASELINES_DIR))
    else {
        return;
    };
    gc_durable_baselines_at(&root, &boot_id, mono_now);
}

fn gc_durable_baselines_at(root: &Path, boot_id: &str, mono_now: i64) {
    let Ok(providers) = fs::read_dir(root) else {
        return;
    };
    for provider in providers.flatten() {
        let Some(provider_name) = provider.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(files) = fs::read_dir(provider.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let Some(channel_id) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.parse::<u64>().ok())
            else {
                continue;
            };
            let channel_lock = durable_baseline_lock(&provider_name, channel_id);
            let _guard = channel_lock
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let keep = load_durable_baseline_at(&path).is_some_and(|baseline| {
                baseline.boot_id == boot_id
                    && mono_now.saturating_sub(baseline.last_seen_monotonic_secs)
                        <= STALL_LIVENESS_STATE_TTL_SECS as i64
            });
            if !keep {
                remove_durable_baseline_at(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use poise::serenity_prelude::ChannelId;

    use crate::services::discord::relay_health::{
        RelayActiveTurn, RelayHealthSnapshot, RelayStallState,
    };
    use crate::services::provider::ProviderKind;

    use super::*;

    #[must_use]
    fn isolated_runtime_root() -> (
        crate::config::TestEnvVarGuard,
        tempfile::TempDir,
        crate::config::test_env_lock::SharedTestEnvLockGuard,
    ) {
        let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let dir = tempfile::tempdir().expect("temp runtime root");
        let env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            dir.path(),
        );
        (env, dir, lock)
    }

    /// Deterministic monotonic clock for tests, injected through the same
    /// `RedriveClock` trait the production `MonotonicRedriveClock` implements —
    /// so tests exercise the real decision + GC code path (no `#[cfg]` split).
    struct FakeClock {
        mono: Cell<i64>,
    }

    impl FakeClock {
        fn new(mono: i64) -> Self {
            Self {
                mono: Cell::new(mono),
            }
        }

        fn set(&self, mono: i64) {
            self.mono.set(mono);
        }
    }

    impl RedriveClock for FakeClock {
        fn mono_secs(&self) -> i64 {
            self.mono.get()
        }
    }

    struct FakeDurableClock {
        mono: Cell<i64>,
    }

    impl FakeDurableClock {
        fn new(mono: i64) -> Self {
            Self {
                mono: Cell::new(mono),
            }
        }
    }

    impl RedriveClock for FakeDurableClock {
        fn mono_secs(&self) -> i64 {
            self.mono.get()
        }

        fn durable_mono_secs(&self) -> Option<i64> {
            Some(self.mono.get())
        }
    }

    fn save_canonical_inflight(snapshot: &WatcherStateSnapshot, provider: &ProviderKind) {
        let identity = snapshot
            .inflight_identity
            .as_ref()
            .expect("snapshot identity");
        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            provider.clone(),
            snapshot.relay_health.channel_id,
            None,
            0,
            identity.user_msg_id,
            snapshot.inflight_current_msg_id.unwrap_or(0),
            "redrive test".to_string(),
            None,
            identity.tmux_session_name.clone(),
            snapshot.inflight_output_path.clone(),
            None,
            identity.turn_start_offset.unwrap_or(0),
        );
        state.started_at = identity.started_at.clone();
        state.turn_start_offset = identity.turn_start_offset;
        state.turn_nonce = snapshot.mailbox_active_turn_nonce.clone();
        let root = runtime_store::runtime_root().expect("runtime root");
        let path = root
            .join("discord_inflight")
            .join(provider.as_str())
            .join(format!("{}.json", snapshot.relay_health.channel_id));
        let json = serde_json::to_string_pretty(&state).expect("serialize canonical inflight");
        runtime_store::atomic_write(&path, &json).expect("save canonical inflight");
    }

    /// A frozen, still-live undelivered backlog: unread bytes present, pane
    /// alive, terminal delivery not committed, and `last_relay_offset` fixed at
    /// `relay_offset` so the relay offset reads as frozen across observations.
    fn frozen_backlog_snapshot(
        channel_id: u64,
        tmux_session: &str,
        relay_offset: u64,
        capture_offset: u64,
    ) -> WatcherStateSnapshot {
        let unread = capture_offset.saturating_sub(relay_offset);
        WatcherStateSnapshot {
            provider: ProviderKind::Codex.as_str().to_string(),
            attached: true,
            tmux_session: Some(tmux_session.to_string()),
            watcher_owner_channel_id: Some(channel_id),
            last_relay_offset: relay_offset,
            inflight_state_present: true,
            last_relay_ts_ms: 1_700_000_000_000,
            last_capture_offset: Some(capture_offset),
            capture_coordinate: crate::services::discord::health::liveness_authority::CaptureCoordinateObservation {
                offset: Some(capture_offset),
                path_hash: 0,
                file_id: None,
                status: crate::services::discord::health::liveness_authority::CoordinateStatus::Observed,
            },
            unread_bytes: Some(unread),
            desynced: true,
            reconnect_count: 0,
            inflight_started_at: Some("2026-06-12 00:00:00".to_string()),
            inflight_updated_at: Some("2026-06-12 00:00:00".to_string()),
            inflight_user_msg_id: Some(9001),
            inflight_current_msg_id: Some(9002),
            tmux_session_alive: Some(true),
            has_pending_queue: false,
            mailbox_active_user_msg_id: Some(9001),
            mailbox_active_turn_nonce: None,
            bound_output_path: None,
            bound_session_id: None,
            inflight_terminal_delivery_committed: false,
            inflight_identity: Some(
                crate::services::discord::inflight::InflightTurnIdentity {
                    user_msg_id: 9001,
                    started_at: "2026-06-12 00:00:00".to_string(),
                    tmux_session_name: Some(tmux_session.to_string()),
                    turn_start_offset: Some(relay_offset),
                },
            ),
            inflight_finalizer_turn_id: None,
            inflight_output_path: Some(format!("/tmp/{tmux_session}.jsonl")),
            relay_stall_state: RelayStallState::TmuxAliveRelayDead,
            relay_health: RelayHealthSnapshot {
                provider: ProviderKind::Codex.as_str().to_string(),
                channel_id,
                active_turn: RelayActiveTurn::Foreground,
                tmux_session: Some(tmux_session.to_string()),
                tmux_alive: Some(true),
                watcher_attached: true,
                watcher_attached_stale: false,
                watcher_owner_channel_id: Some(channel_id),
                watcher_owns_live_relay: true,
                bridge_inflight_present: true,
                bridge_current_msg_id: Some(9002),
                mailbox_has_cancel_token: true,
                mailbox_active_user_msg_id: Some(9001),
                mailbox_turn_started_at_ms: None,
                queue_depth: 0,
                pending_discord_callback_msg_id: Some(9002),
                pending_thread_proof: false,
                parent_channel_id: None,
                thread_channel_id: None,
                last_relay_ts_ms: Some(1_700_000_000_000),
                last_outbound_activity_ms: None,
                last_capture_offset: Some(capture_offset),
                last_relay_offset: relay_offset,
                unread_bytes: Some(unread),
                desynced: true,
                stale_thread_proof: false,
            },
        }
    }

    /// #4181 item-2 (grace): the no-progress grace measures elapsed time on the
    /// MONOTONIC clock. Only genuine monotonic elapsed past the grace fires;
    /// wall-clock/NTP steps are irrelevant because the path takes no wall input.
    ///
    /// Mutation proof: making `relay_offset_stalled_past_grace` gate on anything
    /// other than the injected monotonic seconds (e.g. resetting the anchor when
    /// frozen, or comparing against a different clock) flips step (2) or (3).
    #[test]
    fn redrive_no_progress_grace_is_monotonic_4181() {
        let _runtime = isolated_runtime_root();
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4_181_002);
        let tmux_session = "AgentDesk-codex-4181-mono-grace";
        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));

        let snap = frozen_backlog_snapshot(channel.get(), tmux_session, 10, 301_613);
        assert!(
            live_undelivered_backlog(&snap),
            "precondition: the frozen backlog must be live"
        );
        let grace = STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64;
        let clock = FakeClock::new(0);
        // The frozen offset equals the live committed frontier throughout.
        let live = 10;

        // (1) Prime at monotonic t=0.
        assert!(
            !stalled_undelivered_backlog_for_redrive_with_clock(
                &provider, channel, &snap, live, &clock,
            ),
            "the first observation can never be past the grace"
        );

        // (2) Only 10s of monotonic time elapsed: must NOT fire.
        clock.set(10);
        assert!(
            !stalled_undelivered_backlog_for_redrive_with_clock(
                &provider, channel, &snap, live, &clock,
            ),
            "10s of monotonic elapsed is inside the grace"
        );

        // (3) Genuine monotonic elapsed past the grace: MUST fire.
        clock.set(grace);
        assert!(
            stalled_undelivered_backlog_for_redrive_with_clock(
                &provider, channel, &snap, live, &clock,
            ),
            "monotonic elapsed past the grace must trip the redrive"
        );

        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    /// #4181 item-2 P2 (TTL GC is monotonic): a monotonically-recent observation
    /// survives GC (so the grace is NOT re-armed), while a monotonically-stale
    /// one is evicted. Under the pre-fix wall-clock GC, a forward wall jump > TTL
    /// between prime and check could evict the fresh observation and re-arm the
    /// grace; monotonic GC makes that impossible because the path takes no wall
    /// input. This test crosses the TTL boundary, which the earlier test did not.
    ///
    /// Mutation proof: reverting GC to a wall-clock freshness anchor cannot even
    /// compile (the observation carries only monotonic fields); reverting the GC
    /// comparison so it evicts inside the TTL flips the "survives" assertion.
    #[test]
    fn redrive_ttl_gc_is_monotonic_and_survives_wall_jumps_4181() {
        let _runtime = isolated_runtime_root();
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4_181_003);
        let tmux_session = "AgentDesk-codex-4181-mono-gc";
        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));

        let snap = frozen_backlog_snapshot(channel.get(), tmux_session, 10, 301_613);
        let grace = STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64;
        let ttl = STALL_LIVENESS_STATE_TTL_SECS as i64;
        let clock = FakeClock::new(1_000);
        let live = 10;

        // Prime the frozen observation at mono=1000.
        assert!(!stalled_undelivered_backlog_for_redrive_with_clock(
            &provider, channel, &snap, live, &clock,
        ));

        // GC one second before the grace boundary. Monotonic age is grace-1
        // (far below the TTL), so the observation MUST survive — no matter how
        // far an (unread) wall clock jumped, since GC never reads wall.
        clock.set(1_000 + grace - 1);
        gc_with_clock(&clock);

        // The freeze anchor survived: crossing the grace fires WITHOUT re-priming.
        clock.set(1_000 + grace);
        assert!(
            stalled_undelivered_backlog_for_redrive_with_clock(
                &provider, channel, &snap, live, &clock,
            ),
            "monotonic GC must not evict a monotonically-recent observation"
        );

        // GC past the monotonic TTL DOES evict genuine staleness, so the next
        // observation re-primes and is not immediately past the grace.
        clock.set(1_000 + grace + ttl + 1);
        gc_with_clock(&clock);
        assert!(
            !stalled_undelivered_backlog_for_redrive_with_clock(
                &provider, channel, &snap, live, &clock,
            ),
            "monotonic GC must evict a monotonically-stale observation (re-prime)"
        );

        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    /// #4181 P3 (atomic + stale-offset rejection): a stale concurrent snapshot
    /// reporting a LOWER committed offset than already recorded must not rewind
    /// the freeze anchor and re-arm the grace.
    ///
    /// Mutation proof: dropping the stale sub-branch (so a lower offset always
    /// re-arms `unchanged_since_mono_secs`) makes the final grace-boundary check
    /// measure ~1s of freeze and return `false`, failing.
    #[test]
    fn redrive_rejects_stale_lower_offset_4181() {
        let _runtime = isolated_runtime_root();
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4_181_004);
        let tmux_session = "AgentDesk-codex-4181-stale-offset";
        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));

        let grace = STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64;
        let clock = FakeClock::new(0);
        // The live committed frontier stays at 100 the whole time — the lower
        // snapshot below is stale (lagging), NOT an authoritative rotation.
        let live = 100;

        // Frozen backlog at a HIGH committed offset (100). Prime at mono=0.
        let high = frozen_backlog_snapshot(channel.get(), tmux_session, 100, 301_613);
        assert!(!stalled_undelivered_backlog_for_redrive_with_clock(
            &provider, channel, &high, live, &clock,
        ));

        // A stale concurrent snapshot reporting a LOWER offset (50) arrives near
        // the grace boundary while the live frontier is still 100. It must NOT
        // rewind the freeze anchor: the recorded high-offset freeze keeps aging.
        clock.set(grace - 1);
        let stale_low = frozen_backlog_snapshot(channel.get(), tmux_session, 50, 301_613);
        assert!(!stalled_undelivered_backlog_for_redrive_with_clock(
            &provider, channel, &stale_low, live, &clock,
        ));

        // Crossing the grace with the true high offset fires — the stale lower
        // snapshot did not re-arm the grace.
        clock.set(grace);
        assert!(
            stalled_undelivered_backlog_for_redrive_with_clock(
                &provider, channel, &high, live, &clock,
            ),
            "a stale lower-offset snapshot must not rewind the monotonic freeze anchor"
        );

        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    /// #4181 (rotation re-arm): a JSONL size-cap rotation lowers the authoritative
    /// `confirmed_end_offset` H->L on the SAME session/turn (same key). The grace
    /// must RE-ARM at the new coordinate L — not treat L as a stale snapshot
    /// forever — so a real stall at L still fires. This is the failure the
    /// unconditional stale-rejection introduced: the observed lower offset equals
    /// the live frontier (rotation reset), unlike the stale case above where it
    /// is below the live frontier.
    ///
    /// Mutation proof: removing the `observed_offset >= live_offset` re-arm arm
    /// (so a rotation-lowered offset is rejected like a stale snapshot) leaves the
    /// grace anchored at H forever; the post-rotation stall then never fires and
    /// the final assertion fails.
    #[test]
    fn redrive_rotation_reset_rearms_grace_4181() {
        let _runtime = isolated_runtime_root();
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4_181_006);
        let tmux_session = "AgentDesk-codex-4181-rotation";
        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));

        let grace = STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64;
        let clock = FakeClock::new(0);

        // Frozen at a HIGH offset H=100 (live frontier 100). Prime at mono=0 and
        // let it age right up to the grace boundary.
        let high = frozen_backlog_snapshot(channel.get(), tmux_session, 100, 301_613);
        let high_token = RelayFrontierToken {
            reset_incarnation: 0,
            committed_offset: 100,
        };
        assert!(
            !stalled_undelivered_backlog_for_redrive_with_token_and_clock(
                &provider, channel, &high, high_token, &clock,
            )
        );
        clock.set(grace - 1);
        assert!(
            !stalled_undelivered_backlog_for_redrive_with_token_and_clock(
                &provider, channel, &high, high_token, &clock,
            )
        );

        // Resetting H→L advances the incarnation, so L receives a fresh grace
        // namespace even though the session and turn identity did not change.
        let low = frozen_backlog_snapshot(channel.get(), tmux_session, 40, 301_613);
        let low_token = RelayFrontierToken {
            reset_incarnation: 1,
            committed_offset: 40,
        };
        clock.set(grace);
        assert!(
            !stalled_undelivered_backlog_for_redrive_with_token_and_clock(
                &provider, channel, &low, low_token, &clock,
            ),
            "a new reset incarnation must re-arm the grace at L"
        );

        clock.set(grace + grace);
        assert!(
            stalled_undelivered_backlog_for_redrive_with_token_and_clock(
                &provider, channel, &low, low_token, &clock,
            ),
            "a real stall at the post-reset frontier must fire after the grace"
        );

        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }

    /// #4181 item-3: a fresh process-local map restores the same episode's freeze
    /// anchor from the durable sidecar, so restart does not grant another grace.
    #[test]
    fn redrive_durable_baseline_survives_process_restart_4181() {
        let _runtime = isolated_runtime_root();
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4_181_007);
        let tmux_session = "AgentDesk-codex-4181-durable-baseline";
        let snap = frozen_backlog_snapshot(channel.get(), tmux_session, 10, 301_613);
        let token = RelayFrontierToken {
            reset_incarnation: 0,
            committed_offset: 10,
        };
        let grace = STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64;
        let clock = FakeDurableClock::new(10_000);
        let key = StallLivenessKey::from_snapshot(&provider, channel, &snap);
        let root = tempfile::tempdir().expect("temp runtime root");
        let path = root.path().join("baseline.json");
        let baseline = DurableNoProgressBaseline {
            boot_id: "boot-a".to_string(),
            offset: token.committed_offset,
            reset_incarnation: token.reset_incarnation,
            identity: snap.inflight_identity.clone(),
            turn_nonce: None,
            unchanged_since_monotonic_secs: clock.mono.get(),
            last_seen_monotonic_secs: clock.mono.get(),
        };
        persist_durable_baseline_at(&path, &baseline);
        assert_eq!(load_durable_baseline_at(&path), Some(baseline));

        NO_PROGRESS_OBSERVATIONS.clear();
        clock.mono.set(10_000 + grace);
        let restored = load_durable_baseline_at(&path).expect("durable baseline");
        assert!(restored.matches(snap.inflight_identity.as_ref(), None, token, "boot-a"));
        assert!(
            !restored.matches(snap.inflight_identity.as_ref(), None, token, "boot-b"),
            "a machine reboot must reject the prior boot's monotonic baseline"
        );
        NO_PROGRESS_OBSERVATIONS.insert(
            (key.clone(), token.reset_incarnation),
            restored.observation(clock.mono.get()),
        );
        assert!(relay_offset_stalled_past_grace(
            &key,
            token,
            clock.mono.get(),
        ));

        remove_durable_baseline_at(&path);
        assert!(
            !path.exists(),
            "session cleanup removes the durable baseline"
        );
    }

    /// #4181 item-3 identity gate: a prior episode's durable baseline must not
    /// shorten grace for a successor turn on the same provider/channel.
    #[test]
    fn redrive_durable_baseline_rejects_successor_episode_4181() {
        let _runtime = isolated_runtime_root();
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4_181_008);
        let old = frozen_backlog_snapshot(
            channel.get(),
            "AgentDesk-codex-4181-old-episode",
            10,
            301_613,
        );
        let new = frozen_backlog_snapshot(
            channel.get(),
            "AgentDesk-codex-4181-new-episode",
            10,
            301_613,
        );
        let token = RelayFrontierToken {
            reset_incarnation: 0,
            committed_offset: 10,
        };
        let new_key = StallLivenessKey::from_snapshot(&provider, channel, &new);
        let baseline = DurableNoProgressBaseline {
            boot_id: "boot-a".to_string(),
            offset: token.committed_offset,
            reset_incarnation: token.reset_incarnation,
            identity: old.inflight_identity.clone(),
            turn_nonce: None,
            unchanged_since_monotonic_secs: 20_000,
            last_seen_monotonic_secs: 20_000,
        };

        assert!(!baseline.matches(new.inflight_identity.as_ref(), None, token, "boot-a"));
        assert!(!relay_offset_stalled_past_grace(&new_key, token, 20_180));
        NO_PROGRESS_OBSERVATIONS.remove(&(new_key, token.reset_incarnation));
    }

    #[test]
    fn redrive_durable_baseline_rejects_same_second_synthetic_successor_4181() {
        let _runtime = isolated_runtime_root();
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4_181_009);
        let snap = frozen_backlog_snapshot(channel.get(), "AgentDesk-codex-4181-synthetic", 10, 20);
        let mut successor = snap.clone();
        successor.inflight_identity.as_mut().unwrap().user_msg_id = 0;
        successor
            .inflight_identity
            .as_mut()
            .unwrap()
            .turn_start_offset = Some(11);
        let mut predecessor = successor.inflight_identity.clone().unwrap();
        predecessor.turn_start_offset = Some(10);
        let token = RelayFrontierToken {
            reset_incarnation: 0,
            committed_offset: 10,
        };
        let baseline = DurableNoProgressBaseline {
            boot_id: "boot-a".to_string(),
            offset: 10,
            reset_incarnation: 0,
            identity: Some(predecessor),
            turn_nonce: Some("prior-turn".to_string()),
            unchanged_since_monotonic_secs: 1,
            last_seen_monotonic_secs: 1,
        };

        assert!(!baseline.matches(
            successor.inflight_identity.as_ref(),
            Some("successor-turn"),
            token,
            "boot-a",
        ));
    }

    #[test]
    fn stale_clear_preserves_successor_episode_baseline_4181() {
        let (_env, _root, _lock) = isolated_runtime_root();
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4_181_010);
        let old = frozen_backlog_snapshot(channel.get(), "AgentDesk-codex-4181-old-clear", 10, 20);
        let mut successor =
            frozen_backlog_snapshot(channel.get(), "AgentDesk-codex-4181-successor", 10, 20);
        successor.inflight_identity.as_mut().unwrap().user_msg_id = 9003;
        successor.mailbox_active_turn_nonce = Some("successor-nonce".to_string());
        save_canonical_inflight(&successor, &provider);
        let successor_identity = successor.inflight_identity.clone().unwrap();
        let path = durable_baseline_path(provider.as_str(), channel).unwrap();
        persist_durable_baseline_at(
            &path,
            &DurableNoProgressBaseline {
                boot_id: "boot-a".to_string(),
                offset: 10,
                reset_incarnation: 0,
                identity: Some(successor_identity.clone()),
                turn_nonce: successor.mailbox_active_turn_nonce.clone(),
                unchanged_since_monotonic_secs: 10,
                last_seen_monotonic_secs: 10,
            },
        );
        let successor_key = StallLivenessKey::from_snapshot(&provider, channel, &successor);
        NO_PROGRESS_OBSERVATIONS.insert(
            (successor_key.clone(), 0),
            NoProgressObservation {
                episode_identity: Some(successor_identity.clone()),
                turn_nonce: successor.mailbox_active_turn_nonce.clone(),
                offset: 10,
                unchanged_since_mono_secs: 10,
                last_seen_mono_secs: 10,
            },
        );

        let stale_probe = StallLivenessKey::from_snapshot(&provider, channel, &old);
        let channel_lock = durable_baseline_lock(provider.as_str(), channel.get());
        let _guard = channel_lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let canonical = canonical_episode(&provider, channel);
        clear_for_session_at(
            &stale_probe,
            old.inflight_identity.as_ref(),
            canonical.as_ref(),
            Some(&path),
        );

        assert_eq!(
            load_durable_baseline_at(&path).and_then(|baseline| baseline.identity),
            Some(successor_identity)
        );
        assert!(NO_PROGRESS_OBSERVATIONS.contains_key(&(successor_key, 0)));
    }

    #[test]
    fn stale_identity_persist_cannot_overwrite_successor_baseline_4181() {
        let (_env, _root, _lock) = isolated_runtime_root();
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4_181_011);
        let old = frozen_backlog_snapshot(channel.get(), "AgentDesk-codex-4181-shared", 10, 20);
        let mut successor = old.clone();
        successor.inflight_identity.as_mut().unwrap().user_msg_id = 9003;
        successor
            .inflight_identity
            .as_mut()
            .unwrap()
            .turn_start_offset = Some(11);
        successor.mailbox_active_turn_nonce = Some("successor-nonce".to_string());
        save_canonical_inflight(&successor, &provider);
        let token = RelayFrontierToken {
            reset_incarnation: 0,
            committed_offset: 10,
        };
        let path = durable_baseline_path(provider.as_str(), channel).unwrap();
        let successor_baseline = DurableNoProgressBaseline {
            boot_id: boot_id().expect("boot id"),
            offset: 10,
            reset_incarnation: 0,
            identity: successor.inflight_identity.clone(),
            turn_nonce: successor.mailbox_active_turn_nonce.clone(),
            unchanged_since_monotonic_secs: 10,
            last_seen_monotonic_secs: 10,
        };
        persist_durable_baseline_at(&path, &successor_baseline);

        let old_key = StallLivenessKey::from_snapshot(&provider, channel, &old);
        relay_offset_stalled_past_grace_durable(&provider, channel, &old_key, &old, token, 20);

        assert_eq!(load_durable_baseline_at(&path), Some(successor_baseline));
    }

    #[test]
    fn redrive_durable_gc_serializes_with_persist_and_ignores_staging_files_4181() {
        let (_env, root, _lock) = isolated_runtime_root();
        let records = root.path().join("records").join("codex");
        fs::create_dir_all(&records).unwrap();
        let channel_id = 4_181_012;
        let path = records.join(format!("{channel_id}.json"));
        let staging = records.join(format!(".{channel_id}.json.writer.tmp"));
        fs::write(&staging, "staging").unwrap();
        let stale = DurableNoProgressBaseline {
            boot_id: "boot-b".to_string(),
            offset: 10,
            reset_incarnation: 0,
            identity: None,
            turn_nonce: None,
            unchanged_since_monotonic_secs: 1,
            last_seen_monotonic_secs: 1,
        };
        let fresh = DurableNoProgressBaseline {
            boot_id: "boot-a".to_string(),
            last_seen_monotonic_secs: 10_000,
            unchanged_since_monotonic_secs: 10_000,
            ..stale.clone()
        };
        persist_durable_baseline_at(&path, &stale);
        let channel_lock = durable_baseline_lock("codex", channel_id);
        let held = channel_lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let records_root = root.path().join("records");
        let gc_thread =
            std::thread::spawn(move || gc_durable_baselines_at(&records_root, "boot-a", 10_000));
        std::thread::sleep(std::time::Duration::from_millis(20));
        persist_durable_baseline_at(&path, &fresh);
        drop(held);
        gc_thread.join().unwrap();

        assert_eq!(load_durable_baseline_at(&path), Some(fresh));
        assert!(
            staging.exists(),
            "GC must not touch atomic-write staging files"
        );
    }

    #[test]
    fn redrive_durable_gc_removes_invalid_boot_malformed_and_stale_4181() {
        let (_env, root, _lock) = isolated_runtime_root();
        let records = root.path().join("records").join("codex");
        fs::create_dir_all(&records).unwrap();
        let sample = |boot_id: &str, last_seen: i64| DurableNoProgressBaseline {
            boot_id: boot_id.to_string(),
            offset: 10,
            reset_incarnation: 0,
            identity: None,
            turn_nonce: None,
            unchanged_since_monotonic_secs: last_seen,
            last_seen_monotonic_secs: last_seen,
        };
        persist_durable_baseline_at(&records.join("101.json"), &sample("boot-a", 9_000));
        persist_durable_baseline_at(&records.join("102.json"), &sample("boot-b", 9_000));
        persist_durable_baseline_at(&records.join("103.json"), &sample("boot-a", 1));
        fs::write(records.join("104.json"), "{").unwrap();

        gc_durable_baselines_at(&root.path().join("records"), "boot-a", 10_000);

        assert!(records.join("101.json").exists());
        assert!(!records.join("102.json").exists());
        assert!(!records.join("103.json").exists());
        assert!(!records.join("104.json").exists());
    }

    #[test]
    fn scoped_test_clock_restores_nested_and_panicking_overrides_4181() {
        let _runtime = isolated_runtime_root();
        clear_redrive_grace_test_clock();
        let outer = set_redrive_grace_test_clock(11, None);
        assert_eq!(MonotonicRedriveClock.mono_secs(), 11);
        {
            let _inner = set_redrive_grace_test_clock(22, None);
            assert_eq!(MonotonicRedriveClock.mono_secs(), 22);
        }
        assert_eq!(MonotonicRedriveClock.mono_secs(), 11);

        let panic = std::panic::catch_unwind(|| {
            let _panicking = set_redrive_grace_test_clock(33, None);
            assert_eq!(MonotonicRedriveClock.mono_secs(), 33);
            panic!("exercise panic-safe clock restoration");
        });
        assert!(panic.is_err());
        assert_eq!(MonotonicRedriveClock.mono_secs(), 11);
        drop(outer);
        assert!(TEST_CLOCK_OVERRIDE.with(|cell| cell.get()).is_none());
    }

    /// #4181 F2 (production clock coverage): exercise the real
    /// `MonotonicRedriveClock` (process `Instant`) through the production entry
    /// points, so the clock reader is compiled AND executed under test rather
    /// than hidden behind a `#[cfg]`. A first observation is deterministically
    /// not past the grace regardless of the clock's absolute value.
    #[test]
    fn redrive_production_monotonic_clock_path_runs_4181() {
        let _runtime = isolated_runtime_root();
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(4_181_005);
        let tmux_session = "AgentDesk-codex-4181-prod-clock";
        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
        // #4181 P3-2: under `--test-threads=1` a prior deep-chain test may have
        // left the override set on this thread; clear it so the REAL `Instant`
        // reader runs (otherwise this coverage assertion is silently bypassed).
        clear_redrive_grace_test_clock();

        let snap = frozen_backlog_snapshot(channel.get(), tmux_session, 10, 301_613);
        assert!(
            !stalled_undelivered_backlog_for_redrive(
                &provider,
                channel,
                &snap,
                RelayFrontierToken {
                    reset_incarnation: 0,
                    committed_offset: 10,
                },
            ),
            "first observation on the real monotonic clock is not past the grace"
        );
        // The production GC runs on the real clock without panicking.
        gc();

        super::super::clear_stall_watchdog_liveness_state(&provider, channel, Some(tmux_session));
    }
}
