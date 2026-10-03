//! The host a stop acts on, decided once (from the token, or by a force-kill verdict) and shared
//! by the interrupt, the cooperative cancel and the hard stop. Only legacy tmux reaches tmux.

use std::sync::Arc;

use super::TmuxCleanupPolicy;
use super::interrupt_policy::ProviderTurnInterruptOutcome;
#[cfg(test)]
use crate::services::claude_tui::host_input::MutationGate;
use crate::services::provider::cancel_token_cleanup::executor::ExpectedBinding;
use crate::services::provider::{CancelToken, ProviderKind};
#[cfg(test)]
use crate::services::session_host::HostKind;
#[cfg(test)]
use crate::services::session_host::InteractiveSessionHost;
#[cfg(test)]
use crate::services::session_host::ResolvedSessionTarget;
#[cfg(test)]
use crate::services::session_host::TargetHost;

/// A tmux name whose `.host_kind` marker is absent or tmux when the stop began, or that a
/// force-kill verdict `approved` after reading its host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LegacyTmuxName {
    name: String,
    approved: bool,
}

impl LegacyTmuxName {
    pub(super) fn as_str(&self) -> &str {
        &self.name
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StopRefusal {
    /// The token's `.host_kind` marker is not tmux; `local_tmux` logs what it holds.
    Marker,
    /// The token names a session other than the one the force-kill verdict approved.
    NotApproved,
    #[cfg(test)]
    Unknown,
    #[cfg(test)]
    Conflict,
    #[cfg(test)]
    Unsupported { kind: HostKind, op: &'static str },
}

/// A verified Herdr pane; the session name keys the turn's generation and composer fences.
#[cfg(test)]
#[derive(Clone)]
pub(super) struct HerdrStopTarget {
    pub(super) session: String,
    pub(super) pane: String,
    pub(super) host: Arc<dyn InteractiveSessionHost>,
    pub(super) gate: Arc<dyn MutationGate + Send + Sync>,
}

#[derive(Clone)]
pub(super) enum StopTarget {
    /// No tmux name: the process backend, unchanged.
    Process,
    LegacyTmux(LegacyTmuxName),
    /// Test-only until a Herdr turn carries its verified target to the stop.
    #[cfg(test)]
    Herdr(HerdrStopTarget),
    Refused {
        name: String,
        refusal: StopRefusal,
    },
}

/// Who settles a stopped turn: only a legacy stop leaves its inflight row for the stop to clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum StopSettlement {
    Legacy,
    /// The host was not admitted, by the stop's verdict or by the executor's claim.
    Refused,
    /// The host's own source owner still owes the turn's output.
    #[cfg(test)]
    HostOwned,
}

/// What a stop did: whether it recorded a termination, and who settles the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) struct StopOutcome {
    pub(in crate::services::discord) termination_recorded: bool,
    pub(in crate::services::discord) settlement: StopSettlement,
}

impl StopOutcome {
    /// Whether the stopped turn's inflight row may be cleared or its turn finished by fallback.
    pub(in crate::services::discord) fn may_clear_inflight(&self) -> bool {
        matches!(self.settlement, StopSettlement::Legacy)
    }
}

/// Herdr's answer to an Escape the gate admitted; both outcomes keep the claim spent.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HerdrStopWrite {
    Confirmed,
    Indeterminate(String),
}

impl StopTarget {
    /// The production verdict: the marker filter alone admits tmux, and no tmux is probed.
    pub(super) fn for_token(token: &CancelToken) -> Self {
        Self::judge(token.tmux_session_name(), None)
    }

    pub(super) fn for_session(name: &str) -> Self {
        Self::judge(Some(name.to_string()), None)
    }

    /// The verdict for a token naming `name`; `approved` is a force-kill verdict's session
    /// (`Some(None)`: a process turn), whose host evidence, marker included, was read then.
    pub(super) fn judge(name: Option<String>, approved: Option<Option<&str>>) -> Self {
        let Some(name) = name else {
            return Self::Process;
        };
        let legacy = |approved| {
            Self::LegacyTmux(LegacyTmuxName {
                name: name.clone(),
                approved,
            })
        };
        let refusal = match approved {
            Some(approved) if Some(name.as_str()) == approved => return legacy(true),
            Some(_) => StopRefusal::NotApproved,
            None if crate::services::discord::host_liveness::local_tmux(&name, None) => {
                return legacy(false);
            }
            None => StopRefusal::Marker,
        };
        Self::Refused { name, refusal }
    }

    /// Resolved host evidence for a Herdr turn; Unknown and Conflict never become tmux.
    #[cfg(test)]
    pub(super) fn from_session_target(
        provider: &ProviderKind,
        session: &str,
        target: &ResolvedSessionTarget,
        host: Arc<dyn InteractiveSessionHost>,
        gate: Arc<dyn MutationGate + Send + Sync>,
    ) -> Self {
        let refused = |refusal| Self::Refused {
            name: session.to_string(),
            refusal,
        };
        match &target.host {
            TargetHost::Known {
                kind: HostKind::Herdr,
                name,
                ..
            } if matches!(provider, ProviderKind::Claude) => Self::Herdr(HerdrStopTarget {
                session: session.to_string(),
                pane: name.clone(),
                host,
                gate,
            }),
            TargetHost::Known { kind, .. } => refused(StopRefusal::Unsupported {
                kind: *kind,
                op: "interrupt_until_p10",
            }),
            TargetHost::Unknown(_) => refused(StopRefusal::Unknown),
            TargetHost::Conflict { .. } => refused(StopRefusal::Conflict),
        }
    }

    pub(super) fn legacy_name(&self) -> Option<&LegacyTmuxName> {
        match self {
            Self::LegacyTmux(name) => Some(name),
            _ => None,
        }
    }

    /// Only the process backend and a legacy tmux name keep the PID and session paths.
    pub(super) fn reaches_legacy_host(&self) -> bool {
        matches!(self, Self::Process | Self::LegacyTmux(_))
    }

    /// The binding the executor must still hold for a destructive cleanup of this stop; an
    /// approved name carries its verdict so the executor does not judge its host again.
    pub(super) fn expected_binding(&self) -> ExpectedBinding<'_> {
        match self {
            Self::Process => ExpectedBinding::Decided(None),
            Self::LegacyTmux(name) if name.approved => ExpectedBinding::Approved(Some(&name.name)),
            Self::LegacyTmux(name) => ExpectedBinding::Decided(Some(&name.name)),
            #[cfg(test)]
            Self::Herdr(target) => ExpectedBinding::Decided(Some(&target.session)),
            Self::Refused { name, .. } => ExpectedBinding::Decided(Some(name)),
        }
    }

    /// How a stop on this target settles, given whether the executor refused its cleanup.
    pub(super) fn settlement(&self, host_refused: bool) -> StopSettlement {
        match self {
            Self::Process | Self::LegacyTmux(_) if !host_refused => StopSettlement::Legacy,
            #[cfg(test)]
            Self::Herdr(_) => StopSettlement::HostOwned,
            _ => StopSettlement::Refused,
        }
    }

    /// Any other host keeps its session: a session cleanup becomes a preserve.
    pub(super) fn effective_policy(&self, policy: TmuxCleanupPolicy) -> TmuxCleanupPolicy {
        match policy {
            TmuxCleanupPolicy::CleanupSession { .. } if !self.reaches_legacy_host() => {
                TmuxCleanupPolicy::PreserveSession
            }
            policy => policy,
        }
    }
}

/// The interrupt for a target outside tmux: no I/O, except a Claude Herdr pane's Escape.
pub(super) async fn interrupt_unhosted(
    target: &StopTarget,
    provider: &ProviderKind,
    token: &Arc<CancelToken>,
    reason: &str,
) -> ProviderTurnInterruptOutcome {
    #[cfg(not(test))]
    let _ = token;
    match target {
        #[cfg(test)]
        StopTarget::Herdr(herdr) if matches!(provider, ProviderKind::Claude) => {
            super::claude_stop_delivery::herdr::interrupt_claude_turn_on_herdr(token, herdr, reason)
                .await
        }
        StopTarget::Refused { name, refusal } => {
            let provider = provider.as_str();
            let message = "stop interrupt refused: host is not a confirmed legacy tmux";
            tracing::warn!(%name, ?refusal, provider, reason, "{message}");
            not_sent()
        }
        _ => {
            tracing::warn!(
                provider = provider.as_str(),
                reason,
                "stop interrupt unsupported on this host"
            );
            not_sent()
        }
    }
}

pub(super) fn not_sent() -> ProviderTurnInterruptOutcome {
    ProviderTurnInterruptOutcome {
        tmux_session: None,
        sent_keys: false,
        fallback_sigint_pid: None,
        missing_tmux_session: false,
        sigint_target_missing: false,
    }
}

#[cfg(test)]
#[path = "stop_host_tests.rs"]
pub(super) mod tests;
