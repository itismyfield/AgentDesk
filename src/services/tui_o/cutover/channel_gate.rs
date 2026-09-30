use std::time::Instant;

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::{
    alarm::AlarmRouter,
    channel_policy::{self, BootChannels},
    writer::WriterAlarm,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum IdentityError {
    #[error("writer boot snapshot is unavailable")]
    MissingSnapshot,
    #[error("selected TUI destination has no direct Discord gateway")]
    NonDirectGateway,
    #[error("Discord destination channel is unknown")]
    UnknownChannel,
    #[error("selected channel runtime kind is unknown")]
    UnknownKind,
    #[error("selected channel runtime kind {actual:?} differs from boot kind {expected:?}")]
    KindMismatch {
        expected: Option<RuntimeHandoffKind>,
        actual: RuntimeHandoffKind,
    },
}

pub(crate) fn o_owns_tui_output_for_channel(
    channel_id: u64,
    kind: Option<RuntimeHandoffKind>,
) -> Result<bool, IdentityError> {
    decide(channel_id, || kind)
}

pub(crate) fn o_owns_tui_output_for_channel_tmux(
    channel_id: u64,
    session: Option<&str>,
) -> Result<bool, IdentityError> {
    decide(channel_id, || {
        session.and_then(|session| {
            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(session)
                .map(|binding| binding.runtime_kind)
                .or_else(|| crate::services::tmux_common::resolve_tmux_runtime_kind_marker(session))
        })
    })
}

fn decide(
    channel_id: u64,
    resolve_kind: impl FnOnce() -> Option<RuntimeHandoffKind>,
) -> Result<bool, IdentityError> {
    let enabled = super::writer_enabled();
    if !enabled {
        return Ok(false);
    }
    let evaluate = |snapshot: Option<&BootChannels>| {
        decide_with_snapshot(enabled, snapshot, channel_id, resolve_kind)
    };
    #[cfg(test)]
    let result = super::test_override::with_channels(evaluate);
    #[cfg(not(test))]
    let result = evaluate(channel_policy::boot());
    result.map_err(|error| error.hold(channel_id))
}

impl IdentityError {
    pub(crate) fn hold(self, channel_id: u64) -> Self {
        let alarm = WriterAlarm::Halted {
            detail: self.to_string(),
        };
        AlarmRouter::for_process(None, None).raise_at(channel_id, &alarm, Instant::now());
        tracing::error!(channel_id, error = %self, "tui_o output identity held");
        self
    }
}

fn decide_with_snapshot(
    enabled: bool,
    snapshot: Option<&BootChannels>,
    channel_id: u64,
    resolve_kind: impl FnOnce() -> Option<RuntimeHandoffKind>,
) -> Result<bool, IdentityError> {
    let snapshot = snapshot.ok_or(IdentityError::MissingSnapshot)?;
    // A verified empty list is O off: Legacy before any destination or kind is resolved.
    if snapshot.channels().is_empty() {
        return Ok(false);
    }
    if channel_id == 0 {
        return Err(IdentityError::UnknownChannel);
    }
    // Membership comes before runtime lookup so unrelated Legacy channels stay independent.
    if !snapshot.channels().contains(&channel_id) {
        return Ok(false);
    }
    let kind = resolve_kind().ok_or(IdentityError::UnknownKind)?;
    let expected = snapshot.kind(channel_id);
    if expected != Some(kind) {
        return Err(IdentityError::KindMismatch {
            expected,
            actual: kind,
        });
    }
    Ok(channel_policy::owns_output(
        enabled,
        snapshot.channels(),
        channel_id,
        Some(kind),
    ))
}

#[cfg(test)]
mod tests;
