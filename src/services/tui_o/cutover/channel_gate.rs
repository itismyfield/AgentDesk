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

/// How a caller uses the answer: before a body Legacy would send, or only to read ownership.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Use {
    /// A pending adoption is released: Legacy takes the channel for this process.
    Body,
    /// A pending adoption reads as Legacy and stays pending.
    Peek,
}

/// The raw claim behind [`claim_then_send`]: a pending adoption is released to Legacy. Callers
/// outside it are pinned with their reason in the writer census.
pub(crate) fn o_owns_tui_output_for_channel(
    channel_id: u64,
    kind: Option<RuntimeHandoffKind>,
) -> Result<bool, IdentityError> {
    decide(channel_id, Use::Body, || kind)
}

pub(crate) fn o_owns_tui_output_for_channel_tmux(
    channel_id: u64,
    session: Option<&str>,
) -> Result<bool, IdentityError> {
    decide(channel_id, Use::Body, || session_kind(session))
}

/// For diagnostics and lifecycle checks that send no body: a pending adoption stays pending.
pub(crate) fn peek_o_owns_tui_output_for_channel(
    channel_id: u64,
    kind: Option<RuntimeHandoffKind>,
) -> Result<bool, IdentityError> {
    decide(channel_id, Use::Peek, || kind)
}

pub(crate) fn peek_o_owns_tui_output_for_channel_tmux(
    channel_id: u64,
    session: Option<&str>,
) -> Result<bool, IdentityError> {
    decide(channel_id, Use::Peek, || session_kind(session))
}

/// Where a Legacy body goes, with how the destination's runtime kind is found.
#[derive(Clone, Copy)]
pub(crate) struct BodyClaim<'a> {
    channel_id: u64,
    kind: KindOf<'a>,
    direct: bool,
}

#[derive(Clone, Copy)]
enum KindOf<'a> {
    Known(Option<RuntimeHandoffKind>),
    Tmux(Option<&'a str>),
}

impl<'a> BodyClaim<'a> {
    pub(crate) fn new(channel_id: u64, kind: Option<RuntimeHandoffKind>) -> Self {
        let kind = KindOf::Known(kind);
        Self {
            channel_id,
            kind,
            direct: true,
        }
    }

    pub(crate) fn tmux(channel_id: u64, session: Option<&'a str>) -> Self {
        let kind = KindOf::Tmux(session);
        Self {
            channel_id,
            kind,
            direct: true,
        }
    }

    /// A caller without a direct gateway; see [`o_keeps_body`].
    pub(crate) fn direct(self, direct: bool) -> Self {
        Self { direct, ..self }
    }

    fn claim(self) -> Result<bool, IdentityError> {
        let owned = match self.kind {
            KindOf::Known(kind) => o_owns_tui_output_for_channel(self.channel_id, kind),
            KindOf::Tmux(session) => o_owns_tui_output_for_channel_tmux(self.channel_id, session),
        }?;
        Ok(o_keeps_body(self.channel_id, owned, self.direct))
    }
}

/// An owned channel's body is O's whatever the caller's gateway. A caller without a direct one
/// alarms the channel when this process's writer is not taking it, as that body waits for O.
pub(crate) fn o_keeps_body(channel_id: u64, owned: bool, direct: bool) -> bool {
    if owned && !direct && !super::intake_route::accepts(channel_id) {
        let detail = "writer is not taking a body from a caller without a direct gateway".into();
        let alarm = WriterAlarm::Halted { detail };
        AlarmRouter::for_process(None, None).raise_at(channel_id, &alarm, Instant::now());
    }
    owned
}

/// What became of a body offered to [`claim_then_send`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BodySend<T> {
    /// O owns the channel, so nothing was sent.
    OwnedByO,
    Sent(T),
}

impl<T> BodySend<Result<T, String>> {
    /// The send's own result, with O owning the channel or a held identity as a failed send.
    pub(crate) fn flatten(sent: Result<Self, IdentityError>) -> Result<T, String> {
        match sent {
            Ok(Self::Sent(result)) => result,
            Ok(Self::OwnedByO) => Err("O owns this channel's body".to_string()),
            Err(error) => Err(format!("TUI output identity held: {error}")),
        }
    }
}

/// The one place a Legacy body ends a pending adoption: claim, then send at once unless O owns
/// the channel. Callers settle guards and no-ops on a peek first; `None` sends without a claim.
pub(crate) async fn claim_then_send<T, F: std::future::Future<Output = T>>(
    claim: Option<BodyClaim<'_>>,
    send: impl FnOnce() -> F,
) -> Result<BodySend<T>, IdentityError> {
    if let Some(claim) = claim
        && claim.claim()?
    {
        return Ok(BodySend::OwnedByO);
    }
    Ok(BodySend::Sent(send().await))
}

fn session_kind(session: Option<&str>) -> Option<RuntimeHandoffKind> {
    session.and_then(|session| {
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(session)
            .map(|binding| binding.runtime_kind)
            .or_else(|| crate::services::tmux_common::resolve_tmux_runtime_kind_marker(session))
    })
}

fn decide(
    channel_id: u64,
    usage: Use,
    resolve_kind: impl FnOnce() -> Option<RuntimeHandoffKind>,
) -> Result<bool, IdentityError> {
    let enabled = super::writer_enabled();
    if !enabled {
        return Ok(false);
    }
    let evaluate = |snapshot: Option<&BootChannels>| {
        decide_with_snapshot(enabled, snapshot, channel_id, usage, resolve_kind)
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
    usage: Use,
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
    let selected =
        channel_policy::owns_output(enabled, snapshot.channels(), channel_id, Some(kind));
    // Only a committed (or held) adoption is O's; off the home a selected channel has none.
    let Some(candidate) = snapshot.candidate(channel_id).filter(|_| selected) else {
        return Ok(false);
    };
    Ok(match usage {
        Use::Body => candidate.claim(channel_id),
        Use::Peek => candidate.peek().owned(),
    })
}

#[cfg(test)]
mod tests;
