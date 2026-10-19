//! Restore only the missing normalized producer; the incumbent watcher retains delivery custody.
use super::rebind_runtime::codex_relay_generation as generations;
use crate::services::discord::{
    SharedData,
    inflight::{self, InflightEpisodePin, InflightTurnState, RelayOwnerKind, TurnSource},
};
use crate::services::{
    agent_protocol::RuntimeHandoffKind, provider::ProviderKind, tmux_common as tc,
    tui_prompt_dedupe as dedupe,
};
use poise::serenity_prelude::ChannelId;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::{
    Arc, LazyLock, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
#[path = "producer_resume/custody.rs"]
mod custody;
#[path = "producer_resume/proof.rs"]
mod proof;
#[path = "producer_resume/witness.rs"]
mod witness;
#[path = "producer_resume/writer.rs"]
mod writer;
pub(in crate::services::discord) use custody::waiting_for_receipt;
use witness::{Stamp, Witness};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum ResumeOutcome {
    AlreadyPresent,
    WriterReady,
    Held(String),
    RetryLater,
    NotApplicable,
    DeferredToOwner,
}
struct Cached {
    pin: InflightEpisodePin,
    stamps: (Stamp, Stamp),
    retry: Option<Instant>,
    failures: u32,
}
static PROOFS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(2)));
static CACHE: LazyLock<Mutex<HashMap<String, Cached>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(in crate::services::discord) async fn ensure(
    shared: Arc<SharedData>,
    tmux: String,
    cancel: Arc<AtomicBool>,
) -> ResumeOutcome {
    #[cfg(test)]
    entry_tests::before_ensure().await;
    if shared.provider != ProviderKind::Codex {
        return ResumeOutcome::NotApplicable;
    }
    let path = tc::session_temp_path(&tmux, "jsonl");
    if generations::present(&path) {
        return ResumeOutcome::AlreadyPresent;
    }
    let Some(channel) = shared.tmux_watchers.owner_channel_for_tmux_session(&tmux) else {
        return ResumeOutcome::RetryLater;
    };
    if !current(&shared, channel, &tmux, &cancel) {
        return ResumeOutcome::RetryLater;
    }
    let Some(reservation) = generations::ensure_absent(&path) else {
        return ResumeOutcome::AlreadyPresent;
    };
    if super::super::tmux::watch_host_of(&shared, &ProviderKind::Codex, channel.get(), &tmux).await
        != super::super::tmux::WatchHost::Legacy
    {
        return ResumeOutcome::NotApplicable;
    }
    let Ok(permit) = PROOFS.clone().try_acquire_owned() else {
        return ResumeOutcome::RetryLater;
    };
    let mut permit = permit;
    let mut retries = 0;
    // Startup and the same health pass can briefly hold the episode/source lock.
    // Retain one reservation and proof slot; every lock attempt remains non-blocking.
    let prepared = loop {
        let (shared, tmux, cancel, reservation) = (
            shared.clone(),
            tmux.clone(),
            cancel.clone(),
            reservation.clone(),
        );
        let prepared = tokio::task::spawn_blocking(move || {
            let result = prepare(shared, channel, tmux, cancel, reservation);
            (permit, result)
        })
        .await;
        match prepared {
            Ok((returned, Err(ResumeOutcome::RetryLater))) if retries < 3 => {
                permit = returned;
                retries += 1;
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Ok((_, result)) => break Ok(result),
            Err(error) => break Err(error),
        }
    };
    let outcome = match prepared {
        Ok(Ok((context, plan, witnesses))) => writer::spawn(context, plan, witnesses).await,
        Ok(Err(outcome)) => outcome,
        Err(_) => ResumeOutcome::RetryLater,
    };
    tracing::debug!(wrapper = %path, ?outcome, "Codex producer recovery preflight finished");
    outcome
}

pub(in crate::services::discord) fn schedule(shared: Arc<SharedData>, channel: ChannelId) {
    let watcher = shared
        .tmux_watchers
        .get(&channel)
        .map(|w| (w.tmux_session_name.clone(), w.cancel.clone()));
    if let Some((tmux, cancel)) = watcher {
        super::super::task_supervisor::spawn_observed("codex_producer_resume", async move {
            let outcome = ensure(shared, tmux.clone(), cancel).await;
            tracing::debug!(tmux_session = %tmux, ?outcome, "Codex normalized producer recovery evaluated");
        });
    }
}
fn current(shared: &SharedData, channel: ChannelId, tmux: &str, cancel: &Arc<AtomicBool>) -> bool {
    !cancel.load(Ordering::Acquire)
        && shared.tmux_watchers.get(&channel).is_some_and(|w| {
            w.tmux_session_name == tmux
                && w.output_path == tc::session_temp_path(tmux, "jsonl")
                && Arc::ptr_eq(&w.cancel, cancel)
                && !w.turn_delivered.load(Ordering::Acquire)
        })
        && shared.tmux_watchers.owner_channel_for_tmux_session(tmux) == Some(channel)
}
fn applicable(state: &InflightTurnState, tmux: &str, len: u64) -> bool {
    state.provider == "codex"
        && state.runtime_kind == Some(RuntimeHandoffKind::CodexTui)
        && !state.runtime_kind_unknown_on_disk
        && state.version <= inflight::inflight_state_version()
        && state.turn_source == TurnSource::Managed
        && state.user_msg_id != 0
        && state.current_msg_id != 0
        && state.effective_relay_owner_kind() == RelayOwnerKind::Watcher
        && !state.terminal_delivery_committed
        && !state.rebind_origin
        && state.restart_mode.is_none()
        && state.legacy_replay_coordinate.is_none()
        && state.replay_hold_reasons.is_empty()
        && state.herdr_before_start_stop.is_none()
        && state.tui_terminal_kind.is_none()
        && state.tmux_session_name.as_deref() == Some(tmux)
        && state.output_path.as_deref() == Some(tc::session_temp_path(tmux, "jsonl").as_str())
        && state.turn_start_offset.is_some_and(|offset| offset < len)
}
struct Context {
    shared: Arc<SharedData>,
    channel: ChannelId,
    tmux: String,
    cancel: Arc<AtomicBool>,
    pin: InflightEpisodePin,
    completion_key: super::super::turn_finalizer::TurnKey,
    raw_path: String,
    session_id: String,
    binding_required: bool,
    marker_required: bool,
    reservation: (
        super::rebind_runtime::codex_relay_generation::CodexRebindRelayGenerationGate,
        u64,
    ),
    reset: u64,
    epoch: u64,
}
impl Context {
    fn authority(&self, authority: &tc::TmuxSourceAuthority<'_>) -> bool {
        let binding = dedupe::runtime_binding_for_tmux_session_under_source_authority(authority);
        let marker = crate::services::codex_tui::session::read_codex_tui_rollout_marker(&self.tmux);
        !dedupe::codex_verified_requires_proof_under_source_authority(authority)
            && !dedupe::herdr_execution_listed(&self.tmux)
            && !crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel(
                self.channel.get(),
                Some(RuntimeHandoffKind::CodexTui),
            )
            .unwrap_or(true)
            && (!self.binding_required || binding.is_some())
            && binding.is_none_or(|b| {
                b.runtime_kind == RuntimeHandoffKind::CodexTui
                    && b.output_path == self.raw_path
                    && b.session_id.as_deref() == Some(self.session_id.as_str())
            })
            && (!self.marker_required || marker.is_some())
            && marker.is_none_or(|m| {
                m.rollout_path == Path::new(&self.raw_path)
                    && m.session_id.as_deref() == Some(self.session_id.as_str())
            })
    }
    fn current(&self) -> bool {
        current(&self.shared, self.channel, &self.tmux, &self.cancel)
            && self
                .shared
                .relay_frontier_token(self.channel)
                .reset_incarnation
                == self.reset
            && self
                .shared
                .tmux_watchers
                .get(&self.channel)
                .is_some_and(|w| w.pause_epoch.load(Ordering::Acquire) == self.epoch)
    }
}
fn prepare(
    shared: Arc<SharedData>,
    channel: ChannelId,
    tmux: String,
    cancel: Arc<AtomicBool>,
    reservation: (
        super::rebind_runtime::codex_relay_generation::CodexRebindRelayGenerationGate,
        u64,
    ),
) -> Result<(Context, proof::Proof, (Witness, Witness)), ResumeOutcome> {
    use ResumeOutcome::{DeferredToOwner, Held, NotApplicable, RetryLater};
    let root = inflight::inflight_runtime_root().ok_or(RetryLater)?;
    let state_path = inflight::inflight_state_path(&root, &ProviderKind::Codex, channel.get());
    let (state, binding) = {
        let _lock = inflight::try_lock_inflight_state_path(&state_path).map_err(|_| {
            #[cfg(test)]
            entry_tests::preflight_refused(&tmux);
            RetryLater
        })?;
        let state =
            inflight::load_inflight_state_read_only_result(&ProviderKind::Codex, channel.get())
                .map_err(|_| RetryLater)?
                .ok_or(NotApplicable)?;
        let binding = tc::try_with_tmux_source_authority(&tmux, |authority| {
            if dedupe::codex_verified_requires_proof_under_source_authority(authority)
                || crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel(
                    channel.get(),
                    Some(RuntimeHandoffKind::CodexTui),
                )
                .unwrap_or(true)
            {
                return Err(DeferredToOwner);
            }
            Ok(dedupe::runtime_binding_for_tmux_session_under_source_authority(authority))
        })
        .ok_or(RetryLater)??;
        (state, binding)
    };
    let wrapper_path = tc::session_temp_path(&tmux, "jsonl");
    let wrapper = OpenOptions::new()
        .read(true)
        .append(true)
        .open(&wrapper_path)
        .map_err(|_| RetryLater)?;
    let wrapper_stamp = Stamp::read(&wrapper).map_err(|_| RetryLater)?;
    if !applicable(&state, &tmux, wrapper_stamp.2) {
        return Err(NotApplicable);
    }
    let binding_required = binding.is_some();
    let marker = crate::services::codex_tui::session::read_codex_tui_rollout_marker(&tmux);
    let marker_required = marker.is_some();
    let epoch = shared
        .tmux_watchers
        .get(&channel)
        .ok_or(RetryLater)?
        .pause_epoch
        .load(Ordering::Acquire);
    let reset = shared.relay_frontier_token(channel).reset_incarnation;
    let raw_path = if let Some(binding) = binding {
        if binding.runtime_kind != RuntimeHandoffKind::CodexTui
            || binding.session_id != state.session_id
        {
            return Err(Held("binding_mismatch".into()));
        }
        binding.output_path
    } else if let Some(marker) = marker {
        if marker.session_id != state.session_id {
            return Err(Held("marker_session_mismatch".into()));
        }
        marker.rollout_path.to_string_lossy().into_owned()
    } else {
        crate::services::codex_tui::rollout_tail::find_rollout_by_session_id(
            state.session_id.as_deref().ok_or(NotApplicable)?,
        )
        .ok_or(RetryLater)?
        .to_string_lossy()
        .into_owned()
    };
    let raw = File::open(&raw_path).map_err(|_| RetryLater)?;
    let stamps = (Stamp::read(&raw).map_err(|_| RetryLater)?, wrapper_stamp);
    let pin = InflightEpisodePin::from_state(&state);
    {
        let cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(c) = cache
            .get(&wrapper_path)
            .filter(|c| c.pin == pin && c.stamps == stamps)
            && c.retry.is_none_or(|at| Instant::now() < at)
        {
            return Err(if c.retry.is_none() {
                Held("cached_proof_mismatch".into())
            } else {
                RetryLater
            });
        }
    }
    let witnesses = (
        Witness::capture(Path::new(&raw_path), &raw).map_err(|_| RetryLater)?,
        Witness::capture(Path::new(&wrapper_path), &wrapper).map_err(|_| RetryLater)?,
    );
    let plan = match proof::prove(&state, raw, wrapper) {
        Ok(plan) => plan,
        Err(error) if error == "empty_or_terminal_wrapper" => return Err(NotApplicable),
        Err(error) => {
            let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
            let failures = cache
                .get(&wrapper_path)
                .filter(|c| c.pin == pin)
                .map_or(1, |c| c.failures.saturating_add(1));
            let retry = error
                .starts_with("io:")
                .then(|| Instant::now() + Duration::from_secs(5 * (1_u64 << failures.min(6))));
            cache.insert(
                wrapper_path,
                Cached {
                    pin,
                    stamps,
                    retry,
                    failures,
                },
            );
            return Err(Held(error));
        }
    };
    let completion_key = super::super::turn_finalizer::TurnKey::new(
        channel,
        state.effective_finalizer_turn_id(),
        shared.restart.current_generation,
    )
    .with_episode_nonce(state.turn_nonce.as_deref());
    let context = Context {
        shared,
        channel,
        tmux,
        cancel,
        pin,
        completion_key,
        raw_path,
        binding_required,
        marker_required,
        session_id: state
            .session_id
            .ok_or_else(|| Held("session_unknown".into()))?,
        reservation,
        reset,
        epoch,
    };
    Ok((context, plan, witnesses))
}

fn retry(context: &Context) {
    let path = tc::session_temp_path(&context.tmux, "jsonl");
    let stamps = File::open(&context.raw_path)
        .and_then(|f| Stamp::read(&f))
        .and_then(|raw| {
            File::open(&path)
                .and_then(|f| Stamp::read(&f))
                .map(|wrapper| (raw, wrapper))
        });
    if let Ok(stamps) = stamps {
        let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
        let failures = cache
            .get(&path)
            .filter(|c| c.pin == context.pin)
            .map_or(1, |c| c.failures.saturating_add(1));
        let delay = Duration::from_secs(5 * (1_u64 << failures.min(6)));
        cache.insert(
            path,
            Cached {
                pin: context.pin.clone(),
                stamps,
                retry: Some(Instant::now() + delay),
                failures,
            },
        );
    }
}

#[cfg(test)]
#[path = "producer_resume/entry_tests.rs"]
mod entry_tests;
