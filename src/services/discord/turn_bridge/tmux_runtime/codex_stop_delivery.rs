//! Herdr user cancellation reads the bound source, fences its turn, then sends one Escape.

use std::io::{BufRead, Seek};
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::Ordering};

use super::claude_stop_delivery::{
    ClaudeStopDeliveryReservation, ClaudeStopTurnIdentity, ClaudeTuiInterruptPhase,
    classify_tui_interrupt_phase,
};

use crate::db::dispatched_sessions::hosted_execution::{HostedLookup, HostedRecord, HostedState};
use crate::services::provider::cancel_token_claude_interrupt::{
    HerdrSubmission, herdr_cancel_enabled, herdr_stop_settlement_available,
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
    fn capture(path: &Path) -> Option<Self> {
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
        let start = meta.len().saturating_sub(256 * 1024);
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
                return active;
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

#[cfg(all(test, unix))]
#[path = "codex_stop_delivery_tests.rs"]
mod tests;

enum TurnIdentity {
    Claude(ClaudeStopTurnIdentity),
    Codex(CodexStopTurnIdentity),
}

impl TurnIdentity {
    fn capture(provider: &ProviderKind, path: &str) -> Option<Self> {
        match provider {
            ProviderKind::Claude => ClaudeStopTurnIdentity::capture(path).map(Self::Claude),
            ProviderKind::Codex => CodexStopTurnIdentity::capture(Path::new(path)).map(Self::Codex),
            _ => None,
        }
    }

    fn current(&self) -> bool {
        match self {
            Self::Claude(identity) => identity.still_current(),
            Self::Codex(identity) => {
                CodexStopTurnIdentity::capture(&identity.path).as_ref() == Some(identity)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HerdrNotSent {
    Idle,
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
pub(super) enum HerdrDelivery {
    Sent,
    NotSent(HerdrNotSent),
    Indeterminate,
}

fn holder(channel: u64) -> bool {
    use crate::services::cluster::channel_home::{HomeRefusal, refusal};
    channel != 0 && refusal(channel) != Some(HomeRefusal::NotHeld)
}

/// Called within the future mailbox arm, after checking its current token, not by stop handlers.
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
    use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};
    if owner.provider != provider.as_str()
        || owner.channel_id != channel.to_string()
        || owner.discord_token_hash != token_hash
        || token.tmux_session_name().as_deref() != Some(&owner.logical_key)
        || read_host_kind_marker(&owner.logical_key)
            != HostKindMarker::Known(crate::services::session_host::HostKind::Herdr)
    {
        return Err(HerdrNotSent::Identity);
    }
    if state.user_stop.swap(true, Ordering::AcqRel) {
        return Err(HerdrNotSent::Duplicate);
    }
    Ok(())
}

/// Dormant until settlement lands; no production stop or late-attach path calls this executor.
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
            enabled,
        );
        #[cfg(test)]
        crate::services::provider::cancel_token_claude_interrupt::HERDR_CANCEL_OVERRIDE.set(None);
        result
    })
    .await
    .unwrap_or(HerdrDelivery::Indeterminate)
}

fn deliver(
    token: &CancelToken,
    provider: &ProviderKind,
    target: &HerdrTarget,
    channel: u64,
    home: Option<&crate::services::cluster::channel_home::HomeGate>,
    nonce: &str,
    enabled: bool,
) -> HerdrDelivery {
    use HerdrNotSent::*;
    if !enabled {
        return HerdrDelivery::NotSent(SwitchOff);
    }
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
        let identity = TurnIdentity::capture(provider, &binding.output_path).ok_or(Idle)?;
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
            let generation = if mutant("generation") {
                None
            } else {
                Some(
                    token
                        .lock_current_interrupt_session(provider.clone(), logical)
                        .ok_or(Generation)?,
                )
            };
            let current =
                crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(logical)
                    .ok_or(Pending)?;
            if current.output_path != binding.output_path
                || (!mutant("identity") && !identity.current())
            {
                return Err(Identity);
            }
            let screen = target.capture(-160).ok_or(Idle)?;
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
            let result = match target.send_keys(&[HostKey::Escape]) {
                Ok(HostMutation::Confirmed) => Ok(HerdrDelivery::Sent),
                Ok(HostMutation::Indeterminate(_)) => Ok(HerdrDelivery::Indeterminate),
                _ => Err(Gate),
            };
            match generation {
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
