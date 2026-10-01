//! Host evidence a Claude turn reads before it probes, kills or launches a tmux session by
//! name, and the output-poll probe that evidence allows.

use crate::services::agent_protocol::RuntimeHandoffKind;
#[cfg(unix)]
use crate::services::provider::session_probe::{
    SessionLiveness, SessionProbeTarget, observe_session_liveness,
};
use crate::services::provider::{ProviderKind, SessionProbe};
#[cfg(unix)]
use crate::services::session_host::HostKind;
#[cfg(unix)]
use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};

/// `Err` before any tmux probe, cleanup or launch when the `.host_kind` marker names another
/// host or cannot be read; an absent or tmux marker keeps the existing path.
#[cfg(unix)]
pub(super) fn tmux_turn_admitted(tmux_session_name: &str) -> Result<(), String> {
    match read_host_kind_marker(tmux_session_name) {
        HostKindMarker::Absent | HostKindMarker::Known(HostKind::Tmux) => Ok(()),
        marker => {
            tracing::warn!(
                tmux_session_name,
                ?marker,
                "claude turn deferred: host is not tmux"
            );
            Err(format!(
                "host check kept session {tmux_session_name} ({marker:?}): no tmux probe, cleanup or launch"
            ))
        }
    }
}

/// The live-pane answer for a startup decision; a present session whose pane probe fails is
/// `Err`, never a stale session to kill and recreate.
#[cfg(unix)]
pub(super) fn live_pane(tmux_session_name: &str, session_exists: bool) -> Result<bool, String> {
    let legacy =
        crate::services::session_host::legacy_collapse::tmux_live_pane_bool(tmux_session_name);
    if legacy || !session_exists {
        return Ok(legacy);
    }
    let target = SessionProbeTarget::Tmux(tmux_session_name.to_string());
    match observe_session_liveness(&target).liveness {
        SessionLiveness::Alive => Ok(true),
        SessionLiveness::Missing => Ok(false),
        liveness => {
            tracing::warn!(
                tmux_session_name,
                ?liveness,
                "claude turn deferred: pane unobserved"
            );
            Err(format!(
                "pane of tmux session {tmux_session_name} is unobserved ({liveness:?}): no cleanup or relaunch"
            ))
        }
    }
}

/// Transcript-poll callbacks: the legacy tmux probe for a local tmux session; for another
/// or unknown host the poll never reads dead and takes readiness from the transcript alone.
pub(crate) fn host_poll_probe(
    tmux_session_name: Option<&str>,
    provider: ProviderKind,
    runtime_kind: Option<RuntimeHandoffKind>,
    transcript: &str,
) -> SessionProbe {
    let Some(name) = tmux_session_name else {
        let transcript = std::path::PathBuf::from(transcript);
        return SessionProbe::new(
            || true,
            move || {
                crate::services::tui_turn_state::jsonl_ready_for_input(
                    &provider,
                    runtime_kind,
                    &transcript,
                    None,
                )
                .is_some_and(crate::services::tui_turn_state::TuiReadyState::is_ready)
            },
        );
    };
    let (name, transcript) = (name.to_string(), transcript.to_string());
    SessionProbe::tmux_with_structured_output(name, provider, runtime_kind, transcript)
}

/// A wrapper follow-up's poll probe: a failed pane probe keeps polling instead of reading
/// dead, so it never asks for the session to be recreated.
#[cfg(unix)]
pub(super) fn tmux_wrapper_poll_probe(tmux_session_name: &str) -> SessionProbe {
    let runtime_kind =
        crate::services::tmux_common::resolve_tmux_runtime_kind_marker(tmux_session_name);
    let target = SessionProbeTarget::Tmux(tmux_session_name.to_string());
    match SessionProbe::for_target(&target, ProviderKind::Claude, runtime_kind) {
        Ok(probe) => probe,
        Err(_) => SessionProbe::tmux(tmux_session_name.to_string(), ProviderKind::Claude),
    }
}

#[cfg(test)]
#[path = "host_gate_tests.rs"]
mod tests;
