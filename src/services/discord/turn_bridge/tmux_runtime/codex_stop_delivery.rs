//! Herdr user cancellation reads the bound source, fences its turn, then sends one Escape.

use std::io::{BufRead, Seek};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::claude_stop_delivery::{
    ClaudeStopDeliveryReservation, ClaudeStopTurnIdentity, ClaudeTuiInterruptPhase,
    classify_tui_interrupt_phase,
};

use crate::db::dispatched_sessions::hosted_execution::{HostedLookup, HostedRecord, HostedState};
use crate::services::provider::cancel_token_claude_interrupt::{
    ClaudeInterruptDeliveryGuard, HerdrSubmission, HerdrTurnStart, LateStop, herdr_cancel_enabled,
    herdr_stop_settlement_available,
};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::session_host::{HerdrMutation, HerdrTarget, HostKey, HostMutation};

#[derive(Clone, Debug, PartialEq, Eq)]
struct CodexStopTurnIdentity {
    path: PathBuf,
    file: (u64, u64),
    started_at: u64,
    turn_id: String,
}

impl CodexStopTurnIdentity {
    /// The active turn; `Ok(None)` only once the whole file shows none, and a read that fails or
    /// ends mid-record is `Err`, never an idle turn.
    fn observe(path: &Path) -> Result<Option<Self>, ()> {
        match Self::scan(path, true) {
            Some(None) if !mutant("identity_tail_only") => Self::scan(path, false).ok_or(()),
            scanned => scanned.ok_or(()),
        }
    }

    /// `None` when the file could not be read; `tail` reads only its last 256 KiB.
    fn scan(path: &Path, tail: bool) -> Option<Option<Self>> {
        let mut file = std::fs::File::open(path).ok()?;
        let meta = file.metadata().ok()?;
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            (meta.dev(), meta.ino())
        };
        #[cfg(not(unix))]
        let identity = (
            meta.created()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_nanos()
                .try_into()
                .ok()?,
            0,
        );
        let start = match tail {
            true => meta.len().saturating_sub(256 * 1024),
            false => 0,
        };
        file.seek(std::io::SeekFrom::Start(start)).ok()?;
        let mut reader = std::io::BufReader::new(file);
        let (mut line, mut offset, mut active) = (String::new(), start, None::<Self>);
        if start > 0 {
            offset += reader.read_line(&mut line).ok()? as u64;
        }
        loop {
            line.clear();
            let position = offset;
            let read = reader.read_line(&mut line).ok()?;
            if read == 0 {
                return Some(active);
            }
            offset += read as u64;
            if !line.ends_with('\n') {
                return None;
            }
            let record: serde_json::Value = serde_json::from_str(&line).ok()?;
            if record["type"].as_str() != Some("event_msg") {
                continue;
            }
            let payload = &record["payload"];
            match payload["type"].as_str() {
                Some("task_started") => {
                    let id = payload["turn_id"].as_str().filter(|id| !id.is_empty())?;
                    active = Some(Self {
                        path: path.to_owned(),
                        file: identity,
                        started_at: position,
                        turn_id: id.to_owned(),
                    });
                }
                Some("task_complete" | "turn_aborted") => {
                    if payload["turn_id"].as_str().is_none()
                        || active.as_ref().is_some_and(|turn| {
                            payload["turn_id"].as_str() == Some(turn.turn_id.as_str())
                        })
                    {
                        active = None;
                    }
                }
                _ => {}
            }
        }
    }

    /// Whether this active turn is the one `start` began: the first start at or after its offset,
    /// with no other turn's record before it. An unreadable record is `Unobserved`.
    fn is_own(&self, start: &HerdrTurnStart) -> Result<(), HerdrNotSent> {
        if self.path != start.source
            || start.file.is_some_and(|file| file != self.file)
            || self.started_at < start.offset
        {
            return Err(HerdrNotSent::Identity);
        }
        let mut file = std::fs::File::open(&self.path).map_err(|_| unobserved())?;
        file.seek(std::io::SeekFrom::Start(start.offset))
            .map_err(|_| unobserved())?;
        let mut reader = std::io::BufReader::new(file);
        let (mut line, mut offset) = (String::new(), start.offset);
        loop {
            line.clear();
            let position = offset;
            match reader.read_line(&mut line) {
                Ok(read) if read > 0 && line.ends_with('\n') => offset += read as u64,
                _ => return Err(unobserved()),
            }
            let record: serde_json::Value =
                serde_json::from_str(&line).map_err(|_| unobserved())?;
            let payload = &record["payload"];
            let kind = payload["type"].as_str().unwrap_or("");
            let foreign = match record["type"].as_str() {
                Some("event_msg") if kind == "task_started" => {
                    return match position == self.started_at {
                        true => Ok(()),
                        false => Err(HerdrNotSent::Identity),
                    };
                }
                Some("event_msg") => matches!(kind, "task_complete" | "turn_aborted"),
                Some("response_item") => match kind {
                    "message" => payload["role"].as_str() == Some("assistant"),
                    "function_call" | "custom_tool_call" | "tool_search_call" | "reasoning" => true,
                    "function_call_output" | "custom_tool_call_output" | "tool_search_output" => {
                        true
                    }
                    _ => false,
                },
                _ => false,
            };
            if foreign {
                return Err(HerdrNotSent::Identity);
            }
        }
    }
}

/// Read the existing canonical binding log, never promote a diagnostic runtime binding.
fn source_matches(
    channel: u64,
    logical: &str,
    nonce: &str,
    provider: &ProviderKind,
    path: &str,
) -> bool {
    use crate::services::tui_prompt_dedupe::binding_events::{self, BindingTarget};
    let Ok(events) = binding_events::binding_events_since(channel, 0) else {
        return false;
    };
    let Some(event) = events.iter().rev().find(|event| {
        event.tmux_session == logical && !matches!(event.new, BindingTarget::Rejected { .. })
    }) else {
        return false;
    };
    if event.execution_nonce.as_deref() != Some(nonce) || event.provider != provider.as_str() {
        return false;
    }
    let source = match &event.new {
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => source,
        _ => return false,
    };
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    source.path == Path::new(path)
        && crate::services::tui_o::shadow::capture::file_identity(&meta) == (source.dev, source.ino)
}

#[cfg(test)]
struct TestBindingRoot(Option<PathBuf>);
#[cfg(test)]
impl TestBindingRoot {
    fn enter(root: Option<&Path>) -> Self {
        use crate::services::tui_prompt_dedupe::binding_events as events;
        let old = events::test_root();
        events::set_test_root(root);
        Self(old)
    }
}
#[cfg(test)]
impl Drop for TestBindingRoot {
    fn drop(&mut self) {
        crate::services::tui_prompt_dedupe::binding_events::set_test_root(self.0.as_deref());
    }
}

#[cfg(test)]
#[path = "codex_stop_delivery_tests.rs"]
mod tests;

enum TurnIdentity {
    Claude(ClaudeStopTurnIdentity),
    Codex(CodexStopTurnIdentity),
}

impl TurnIdentity {
    /// `Err` when the turn could not be read; only Codex can prove there is no active turn.
    fn capture(provider: &ProviderKind, path: &str) -> Result<Option<Self>, ()> {
        match provider {
            ProviderKind::Claude => ClaudeStopTurnIdentity::capture(path)
                .map(|identity| Some(Self::Claude(identity)))
                .ok_or(()),
            ProviderKind::Codex => {
                CodexStopTurnIdentity::observe(Path::new(path)).map(|turn| turn.map(Self::Codex))
            }
            _ => Ok(None),
        }
    }

    fn current(&self) -> bool {
        match self {
            Self::Claude(identity) => identity.still_current(),
            Self::Codex(identity) => {
                matches!(CodexStopTurnIdentity::observe(&identity.path), Ok(Some(now)) if now == *identity)
            }
        }
    }
}

/// Test-only effect mutations run against one binary; production always keeps every fence.
fn mutant(name: &str) -> bool {
    #[cfg(test)]
    {
        std::env::var("ADK_P10_3_MUTANT").ok().as_deref() == Some(name)
    }
    #[cfg(not(test))]
    {
        let _ = name;
        false
    }
}

/// A turn that could not be read is never reported idle.
fn unobserved() -> HerdrNotSent {
    match mutant("unobserved_is_idle") {
        true => HerdrNotSent::Idle,
        false => HerdrNotSent::Unobserved,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum HerdrNotSent {
    Idle,
    /// The turn or the pane could not be read, so whether it runs is unknown.
    Unobserved,
    Pending,
    Generation,
    Identity,
    Holder,
    Gate,
    SwitchOff,
    Duplicate,
    NotAdmitted,
    SettlementUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum HerdrDelivery {
    Sent,
    NotSent(HerdrNotSent),
    Indeterminate,
}

fn holder(channel: u64) -> bool {
    use crate::services::cluster::channel_home::{HomeRefusal, refusal};
    channel != 0 && refusal(channel) != Some(HomeRefusal::NotHeld)
}

/// Whether `token`'s Herdr turn may take this user stop. It writes nothing: the mailbox records
/// the intent only while `token` is still the channel's turn.
fn herdr_command_eligible(
    token: &Arc<CancelToken>,
    provider: &ProviderKind,
    channel: u64,
    token_hash: &str,
    reason: &str,
) -> Result<(), HerdrNotSent> {
    if !herdr_stop_settlement_available() {
        return Err(HerdrNotSent::SettlementUnavailable);
    }
    if !herdr_cancel_enabled() {
        return Err(HerdrNotSent::SwitchOff);
    }
    if !holder(channel) {
        return Err(HerdrNotSent::Holder);
    }
    if !matches!(
        reason,
        "/stop" | "!stop" | "!cc stop" | "!skill stop" | "/skill stop" | "/cc stop"
    ) {
        return Err(HerdrNotSent::NotAdmitted);
    }
    let state = token.herdr_interrupt_state().ok_or(HerdrNotSent::Pending)?;
    let owner = &state.owner;
    if owner.provider != provider.as_str()
        || owner.channel_id != channel.to_string()
        || owner.discord_token_hash != token_hash
        || token.tmux_session_name().as_deref() != Some(&owner.logical_key)
        || !herdr_marked(&owner.logical_key)
    {
        return Err(HerdrNotSent::Identity);
    }
    Ok(())
}

/// Whether the host marker beside tmux name `name` names Herdr.
pub(in crate::services::discord) fn herdr_marked(name: &str) -> bool {
    use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};
    read_host_kind_marker(name)
        == HostKindMarker::Known(crate::services::session_host::HostKind::Herdr)
}

/// The eligibility check and the intent write in one call, for executor tests without a mailbox.
#[cfg(test)]
pub(super) fn admit_herdr_command(
    token: &Arc<CancelToken>,
    current: Option<&Arc<CancelToken>>,
    provider: &ProviderKind,
    channel: u64,
    token_hash: &str,
    reason: &str,
) -> Result<(), HerdrNotSent> {
    if !current.is_some_and(|current| Arc::ptr_eq(current, token)) {
        return Err(HerdrNotSent::Generation);
    }
    herdr_command_eligible(token, provider, channel, token_hash, reason)?;
    let state = token.herdr_interrupt_state().ok_or(HerdrNotSent::Pending)?;
    if state.user_stop.swap(true, Ordering::AcqRel) {
        return Err(HerdrNotSent::Duplicate);
    }
    Ok(())
}

/// What a Herdr user stop did. Whatever the outcome the turn stays with its provider: the stop
/// never cancels the token, and only the provider's own terminal record ends the turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum HerdrStop {
    /// This stop recorded the turn's intent; the delivery is its one Escape's outcome.
    Requested(HerdrDelivery),
    /// An earlier stop recorded the intent, or the token is already cancelling.
    AlreadyRequested,
    /// Nothing was recorded or sent.
    Refused(HerdrNotSent),
}

impl HerdrStop {
    /// The reply names what was delivered; none says the turn has stopped.
    pub(in crate::services::discord) fn reply(self) -> &'static str {
        use HerdrNotSent::*;
        let not_sent = match self {
            Self::Requested(HerdrDelivery::Sent) => {
                return "중지 키를 보냈어요. 작업 종료 기록이 오면 정리해요.";
            }
            Self::Requested(HerdrDelivery::Indeterminate) => {
                return "중지 키 전달을 확인하지 못했어요. 다시 보내지 않고 작업 종료 기록을 기다려요.";
            }
            Self::AlreadyRequested => {
                return "이미 중지 요청을 받았어요. 중지 키를 더 보내지 않아요.";
            }
            Self::Requested(HerdrDelivery::NotSent(reason)) | Self::Refused(reason) => reason,
        };
        match not_sent {
            Pending => "작업 연결을 기다리는 중이라 아직 중지 키를 보내지 않았어요.",
            Idle => {
                "실행 중임을 확인하지 못해 중지 키를 보내지 않았어요. 작업 종료 여부는 계속 확인해요."
            }
            Unobserved => {
                "작업 상태를 읽지 못해 중지 키를 보내지 않았어요. 작업 종료 여부는 계속 확인해요."
            }
            Generation | Identity | Holder | Gate => {
                "작업 대상이 바뀌어 중지 키를 보내지 않았어요."
            }
            SwitchOff | SettlementUnavailable => {
                "Herdr 중지 키 전달이 꺼져 있어요. 작업은 그대로 계속돼요."
            }
            Duplicate => "이미 중지 요청을 받았어요. 중지 키를 더 보내지 않아요.",
            NotAdmitted => "이 명령으로는 Herdr 작업을 중지할 수 없어요.",
        }
    }
}

/// A user stop on a Herdr turn: judged eligible without a write, admitted by the mailbox only
/// while `token` is the channel's turn, then at most one Escape. It never cancels or cleans up.
pub(super) async fn herdr_command_stop(
    shared: &Arc<crate::services::discord::SharedData>,
    provider: &ProviderKind,
    channel: poise::serenity_prelude::ChannelId,
    token: &Arc<CancelToken>,
    reason: &str,
) -> HerdrStop {
    let eligible =
        herdr_command_eligible(token, provider, channel.get(), &shared.token_hash, reason);
    if let Err(refusal) = eligible {
        return HerdrStop::Refused(refusal);
    }
    let mailbox = shared.mailbox(channel);
    let admitted = mailbox
        .admit_herdr_user_stop_if_current(token.clone(), reason.to_string())
        .await;
    let stop = match admitted.token {
        None => HerdrStop::Refused(HerdrNotSent::Generation),
        Some(_) if admitted.already_stopping => HerdrStop::AlreadyRequested,
        Some(token) => HerdrStop::Requested(match shared.pg_pool.as_ref() {
            Some(pool) => match interrupt_herdr(pool, &token, provider).await {
                HerdrDelivery::NotSent(HerdrNotSent::Pending) => {
                    #[cfg(all(test, unix))]
                    tests::before_late_stop().await;
                    late_stop(shared, pool, &token, provider, channel).await
                }
                delivery => delivery,
            },
            None => HerdrDelivery::NotSent(HerdrNotSent::Pending),
        }),
    };
    tracing::info!(channel_id = channel.get(), reason, ?stop, "herdr user stop");
    stop
}

/// A stop that met its turn unbound runs from the turn's reader at its own start, or now when that
/// start was already read; either way only while `token` is still the channel's turn.
async fn late_stop(
    shared: &Arc<crate::services::discord::SharedData>,
    pool: &sqlx::PgPool,
    token: &Arc<CancelToken>,
    provider: &ProviderKind,
    channel: poise::serenity_prelude::ChannelId,
) -> HerdrDelivery {
    let Some(state) = token.herdr_interrupt_state() else {
        return HerdrDelivery::NotSent(HerdrNotSent::Pending);
    };
    let run = LateRun {
        data: Arc::downgrade(shared),
        actor: Arc::downgrade(token),
        pool: pool.clone(),
        provider: provider.clone(),
        channel,
        handle: tokio::runtime::Handle::current(),
        #[cfg(test)]
        enabled: crate::services::provider::cancel_token_claude_interrupt::HERDR_CANCEL_OVERRIDE
            .with(std::cell::Cell::get),
    };
    if state.arm_late_stop(run.clone().into_stop()) {
        return HerdrDelivery::NotSent(HerdrNotSent::Pending);
    }
    let after = state.seen_progress().unwrap_or(0);
    let delivery = match mutant("immediate_branch_skips_token_check") {
        true => Some(interrupt_herdr(pool, token, provider).await),
        false => late_attempt(shared, pool, token, provider, channel).await,
    };
    if retries(delivery) {
        state.retry_late_stop(run.into_stop(), after);
    }
    delivery.unwrap_or(HerdrDelivery::NotSent(HerdrNotSent::Generation))
}

/// The executor for a late stop, run only while `token` is still the channel's turn.
async fn late_attempt(
    shared: &crate::services::discord::SharedData,
    pool: &sqlx::PgPool,
    token: &Arc<CancelToken>,
    provider: &ProviderKind,
    channel: poise::serenity_prelude::ChannelId,
) -> Option<HerdrDelivery> {
    let current = shared.mailbox_peek(channel)?.cancel_token().await.ok()??;
    match Arc::ptr_eq(&current, token) {
        true => Some(interrupt_herdr(pool, token, provider).await),
        false => None,
    }
}

/// Only a refusal before any send keeps a late stop for the reader's next record.
fn retries(delivery: Option<HerdrDelivery>) -> bool {
    matches!(
        delivery,
        Some(HerdrDelivery::NotSent(
            HerdrNotSent::Pending | HerdrNotSent::Unobserved
        ))
    ) && !mutant("late_intent_dropped_on_notsent")
}

/// What a late stop needs on the turn's reader; it holds the channel state and token only weakly.
#[derive(Clone)]
struct LateRun {
    data: std::sync::Weak<crate::services::discord::SharedData>,
    actor: std::sync::Weak<CancelToken>,
    pool: sqlx::PgPool,
    provider: ProviderKind,
    channel: poise::serenity_prelude::ChannelId,
    handle: tokio::runtime::Handle,
    #[cfg(test)]
    enabled: Option<bool>,
}

impl LateRun {
    fn into_stop(self) -> LateStop {
        Box::new(move || {
            let (Some(shared), Some(token)) = (self.data.upgrade(), self.actor.upgrade()) else {
                return;
            };
            #[cfg(test)]
            let previous =
                crate::services::provider::cancel_token_claude_interrupt::HERDR_CANCEL_OVERRIDE
                    .replace(self.enabled);
            // The reader is a blocking thread, as the tail's own waits are.
            let attempt = late_attempt(&shared, &self.pool, &token, &self.provider, self.channel);
            let delivery = self.handle.block_on(attempt);
            #[cfg(test)]
            crate::services::provider::cancel_token_claude_interrupt::HERDR_CANCEL_OVERRIDE
                .set(previous);
            tracing::info!(
                channel_id = self.channel.get(),
                ?delivery,
                "herdr late user stop"
            );
            if retries(delivery)
                && let Some(state) = token.herdr_interrupt_state()
            {
                let after = state.seen_progress().unwrap_or(0);
                state.retry_late_stop(self.clone().into_stop(), after);
            }
        })
    }
}

/// Dormant until settlement lands; only the Herdr user stop calls this executor.
pub(super) async fn interrupt_herdr(
    pool: &sqlx::PgPool,
    token: &Arc<CancelToken>,
    provider: &ProviderKind,
) -> HerdrDelivery {
    use HerdrNotSent::*;
    if !herdr_stop_settlement_available() {
        return HerdrDelivery::NotSent(SettlementUnavailable);
    }
    if !herdr_cancel_enabled() {
        return HerdrDelivery::NotSent(SwitchOff);
    }
    let Some(state) = token.herdr_interrupt_state() else {
        return HerdrDelivery::NotSent(Pending);
    };
    let Some(channel) = state.owner.channel_id.parse::<u64>().ok() else {
        return HerdrDelivery::NotSent(Identity);
    };
    if !state.user_stop.load(Ordering::Acquire) {
        return HerdrDelivery::NotSent(NotAdmitted);
    }
    if !holder(channel) {
        return HerdrDelivery::NotSent(Holder);
    }
    let owner = &state.owner;
    if owner.provider != provider.as_str()
        || token.tmux_session_name().as_deref() != Some(&owner.logical_key)
    {
        return HerdrDelivery::NotSent(Identity);
    }
    let HostedLookup::Found(found) = crate::services::claude::herdr_turn::load(pool, owner).await
    else {
        return HerdrDelivery::NotSent(Pending);
    };
    let HostedRecord::Known(record) = found.record else {
        return HerdrDelivery::NotSent(Pending);
    };
    if record.state != HostedState::Bound || record.owner != *owner {
        return HerdrDelivery::NotSent(Pending);
    }
    let Some(target) = crate::services::session_host::herdr_endpoints().target(&record) else {
        return HerdrDelivery::NotSent(Pending);
    };
    if !holder(channel) {
        return HerdrDelivery::NotSent(Holder);
    }
    let (token, provider) = (token.clone(), provider.clone());
    let home = crate::services::cluster::channel_home::registered_channel(channel);
    #[cfg(test)]
    let binding_root = crate::services::tui_prompt_dedupe::binding_events::test_root();
    let enabled = herdr_cancel_enabled();
    if !enabled {
        return HerdrDelivery::NotSent(SwitchOff);
    }
    let attempted = Arc::new(AtomicBool::new(false));
    let writer_attempted = attempted.clone();
    tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        let _root = TestBindingRoot::enter(binding_root.as_deref());
        #[cfg(test)]
        crate::services::provider::cancel_token_claude_interrupt::HERDR_CANCEL_OVERRIDE
            .set(Some(enabled));
        let result = deliver(
            &token,
            &provider,
            &target,
            channel,
            home.as_deref(),
            &record.execution_nonce,
            &writer_attempted,
        );
        #[cfg(test)]
        crate::services::provider::cancel_token_claude_interrupt::HERDR_CANCEL_OVERRIDE.set(None);
        result
    })
    .await
    .unwrap_or_else(|_| match attempted.load(Ordering::Acquire) {
        true => HerdrDelivery::Indeterminate,
        false => HerdrDelivery::NotSent(HerdrNotSent::Gate),
    })
}

/// A writer that unwinds once its Escape may have left keeps the claim spent, so its uncertain
/// reply is never followed by a second Escape.
struct SpentOnUnwind<'a> {
    generation: Option<ClaudeInterruptDeliveryGuard<'a>>,
    attempted: &'a AtomicBool,
}

impl Drop for SpentOnUnwind<'_> {
    fn drop(&mut self) {
        let spend = std::thread::panicking() && self.attempted.load(Ordering::Acquire);
        if spend
            && !mutant("unwind_releases")
            && let Some(generation) = self.generation.take()
        {
            let _ = generation.commit_success::<(), ()>(Ok(()));
        }
    }
}

fn deliver(
    token: &CancelToken,
    provider: &ProviderKind,
    target: &HerdrTarget,
    channel: u64,
    home: Option<&crate::services::cluster::channel_home::HomeGate>,
    nonce: &str,
    attempted: &AtomicBool,
) -> HerdrDelivery {
    use HerdrNotSent::*;
    let attempt = || -> Result<HerdrDelivery, HerdrNotSent> {
        let held = || {
            holder(channel)
                && home.is_none_or(|home| {
                    home.refusal()
                        != Some(crate::services::cluster::channel_home::HomeRefusal::NotHeld)
                })
        };
        let state = token.herdr_interrupt_state().ok_or(Pending)?;
        // The reservation belongs to the blocking writer, even if its async waiter disappears.
        let _claim = ClaudeStopDeliveryReservation::claim(token).ok_or(Duplicate)?;
        let logical = &state.owner.logical_key;
        let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(logical)
            .ok_or(Pending)?;
        let source_current =
            || source_matches(channel, logical, nonce, provider, &binding.output_path);
        if !source_current() {
            return Err(Identity);
        }
        let identity = TurnIdentity::capture(provider, &binding.output_path)
            .map_err(|()| unobserved())?
            .ok_or(Idle)?;
        // A Codex stop targets the turn its token began, never a later native turn of the pane.
        if let TurnIdentity::Codex(turn) = &identity
            && !mutant("late_escape_retargets_native_turn")
        {
            turn.is_own(state.turn_start.get().ok_or(Pending)?)?;
        }
        #[cfg(all(test, unix))]
        if let Some(action) = tests::AFTER_IDENTITY.lock().unwrap().take() {
            action();
        }
        let write = || {
            let submission = state.submission.lock().unwrap_or_else(|e| e.into_inner());
            if *submission == HerdrSubmission::Unsubmitted {
                return Err(Pending);
            }
            if !held() {
                return Err(Holder);
            }
            let mut generation = SpentOnUnwind {
                generation: if mutant("generation") {
                    None
                } else {
                    Some(
                        token
                            .lock_current_interrupt_session(provider.clone(), logical)
                            .ok_or(Generation)?,
                    )
                },
                attempted,
            };
            let current =
                crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(logical)
                    .ok_or(Pending)?;
            if current.output_path != binding.output_path
                || (!mutant("identity") && !identity.current())
            {
                return Err(Identity);
            }
            let screen = target.capture(-160).ok_or_else(unobserved)?;
            let running = match provider {
                ProviderKind::Claude => {
                    use crate::services::tmux_common as screen_state;
                    let structured = crate::services::tui_turn_state::runtime_binding_turn_state(
                        provider, &current,
                    );
                    let ready =
                        screen_state::tmux_capture_indicates_claude_tui_ready_for_input(&screen)
                            || screen_state::tmux_capture_indicates_claude_tui_prompt_draft(
                                &screen,
                            );
                    let active =
                        screen_state::tmux_capture_indicates_claude_tui_actively_streaming(&screen);
                    !screen_state::tmux_capture_indicates_claude_tui_interactive_modal(&screen)
                        && !ready
                        && classify_tui_interrupt_phase(structured, ready, active)
                            == ClaudeTuiInterruptPhase::ActiveGeneration
                }
                ProviderKind::Codex => {
                    crate::services::codex_tui::input::herdr_turn_in_progress(&screen)
                }
                _ => false,
            };
            if !running && !mutant("running") {
                return Err(Idle);
            }
            target.pin(HerdrMutation::Cancel).map_err(|_| Gate)?;
            if (!mutant("identity") && !identity.current()) || !source_current() {
                target.discard_pin();
                return Err(Identity);
            }
            if !held() {
                target.discard_pin();
                return Err(Holder);
            }
            if !herdr_cancel_enabled() {
                target.discard_pin();
                return Err(SwitchOff);
            }
            attempted.store(true, Ordering::Release);
            let result = match target.send_keys(&[HostKey::Escape]) {
                Ok(HostMutation::Confirmed) => Ok(HerdrDelivery::Sent),
                Ok(HostMutation::Indeterminate(_)) => Ok(HerdrDelivery::Indeterminate),
                _ => Err(Gate),
            };
            #[cfg(all(test, unix))]
            if let Some(action) = tests::take_after_send() {
                action();
            }
            match generation.generation.take() {
                Some(_)
                    if mutant("indeterminate_claim")
                        && result == Ok(HerdrDelivery::Indeterminate) =>
                {
                    result
                }
                Some(generation) => generation.commit_success(result),
                None => result,
            }
        };
        match provider {
            ProviderKind::Claude => {
                crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
                    logical, write,
                )
            }
            ProviderKind::Codex => {
                crate::services::codex_tui::input::try_with_composer_mutation_lock(logical, write)
                    .ok_or(Gate)?
            }
            _ => Err(NotAdmitted),
        }
    };
    attempt().unwrap_or_else(HerdrDelivery::NotSent)
}
