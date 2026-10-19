//! Records a Claude pane's confirmed `!clear` end for O: read before the kill, written only once
//! the pane, its process and its group are gone and the pane bound nothing new.

use std::time::Duration;

use chrono::Utc;
use tokio::sync::OwnedMutexGuard;

use crate::services::platform::tmux::{self, SessionPresence};
use crate::services::process::{
    ProcessGroupProbe, ProcessIdentity, ProcessIdentityProbe, process_group_probe,
};
use crate::services::provider::ProviderKind;
use crate::services::tmux_common as tc;
use crate::services::tui_o::store::clear_end::{self, Appended, ClearEnd, mutant};
use crate::services::tui_prompt_dedupe::binding_context::{self, SpawnNonceMarker};
use crate::services::tui_prompt_dedupe::binding_events::{
    self as p5, BindingEvent, BindingTarget, SourceId,
};

const VACANT_WAIT: Duration = Duration::from_secs(2);
const VACANT_POLL: Duration = Duration::from_millis(100);

/// What the pane ran when the clear read it under the pane's source authority.
pub(super) struct Captured {
    channel: u64,
    tmux: String,
    nonce: String,
    source: SourceId,
    binding_seq: u64,
    pane_pid: u32,
    identity: ProcessIdentity,
    pgid: u32,
}

/// Captures a Claude, non-Herdr pane whose clear holds the channel's transition guard.
pub(super) fn capture_for(
    provider: &ProviderKind,
    channel: u64,
    tmux: &str,
    transition: &Option<OwnedMutexGuard<()>>,
) -> Option<Captured> {
    let guarded = transition.is_some() || mutant("transition_unchecked");
    let claude = matches!(provider, ProviderKind::Claude) || mutant("any_provider");
    let herdr = crate::config::session_hosts::herdr_endpoint(channel).is_some() || mutant("off");
    (guarded && claude && !herdr)
        .then(|| capture(channel, tmux))
        .flatten()
}

/// Commits a capture only when the kill confirmed the pane ended.
pub(super) async fn commit_confirmed(
    captured: Option<Captured>,
    (reset, unconfirmed): (bool, bool),
) {
    let confirmed = (reset && !unconfirmed) || mutant("kill_unconfirmed_ok");
    if let Some(captured) = captured.filter(|_| confirmed) {
        commit(captured).await;
    }
}

/// Reads the pane before the kill; a pane whose execution or source is not settled records none.
fn capture(channel: u64, tmux: &str) -> Option<Captured> {
    let read = tc::with_tmux_source_authority(tmux, |authority| {
        capture_under(channel, authority.session())
    });
    read.inspect_err(|reason| {
        tracing::warn!(tmux_session = tmux, reason, "clear end not captured");
    })
    .ok()
}

fn capture_under(channel: u64, tmux: &str) -> Result<Captured, &'static str> {
    if tmux::session_presence(tmux) != SessionPresence::Present {
        return Err("pane not present");
    }
    let SpawnNonceMarker::Known(nonce) = binding_context::observe_spawn_nonce_marker(tmux) else {
        return Err("no spawn nonce");
    };
    let context = binding_context::execution_context("claude", &nonce);
    let context = context.map_err(|_| "no execution context")?;
    if context.tmux_session != tmux || context.channel_id != Some(channel) {
        return Err("execution context names another pane");
    }
    let events = p5::records_strict(channel).ok().and_then(Result::ok);
    let events = events.ok_or("binding log unreadable")?;
    let source = pane_source(&events, tmux).ok_or("pane source not settled")?;
    let binding_seq = events.last().map_or(0, |event| event.seq);
    let pane_pid = tmux::pane_pid(tmux).ok_or("no pane pid")?;
    let pgid = process_group(pane_pid).ok_or("no pane process group")?;
    Ok(Captured {
        channel,
        tmux: tmux.to_owned(),
        nonce,
        source,
        binding_seq,
        pane_pid,
        identity: ProcessIdentity::capture(pane_pid),
        pgid,
    })
}

/// The source the pane's binds settle on; a Pending nothing resolved leaves it unknown.
fn pane_source(events: &[BindingEvent], tmux: &str) -> Option<SourceId> {
    let (mut current, mut open) = (None, None);
    for event in events.iter().filter(|event| event.tmux_session == tmux) {
        match &event.new {
            BindingTarget::Source(source) => (current, open) = (Some(source.clone()), None),
            BindingTarget::Pending { .. } => open = Some(event.seq),
            BindingTarget::Resolved {
                pending_seq,
                source,
            } if open == Some(*pending_seq) => (current, open) = (Some(source.clone()), None),
            BindingTarget::Resolved { .. } => return None,
            BindingTarget::Rejected { .. } => {}
        }
    }
    current.filter(|_| open.is_none())
}

fn process_group(pid: u32) -> Option<u32> {
    let output = std::process::Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let pgid = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    (output.status.success() && pgid > 1).then_some(pgid)
}

fn vacant(captured: &Captured) -> bool {
    if mutant("process_unchecked") {
        return tmux::session_presence(&captured.tmux) == SessionPresence::Missing;
    }
    tmux::session_presence(&captured.tmux) == SessionPresence::Missing
        && crate::services::session_backend::process_session_pid(&captured.tmux).is_none()
        && captured.identity.probe(captured.pane_pid) == ProcessIdentityProbe::GoneOrReused
        && process_group_probe(captured.pgid) == ProcessGroupProbe::Gone
}

/// Writes the record once the pane is vacant and its binds did not move past the capture.
async fn commit(captured: Captured) {
    let deadline = tokio::time::Instant::now() + VACANT_WAIT;
    while !vacant(&captured) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(VACANT_POLL).await;
    }
    let written = tc::with_tmux_source_authority(&captured.tmux, |_| write_under(&captured));
    match written {
        Ok(Appended::Durable) => {}
        Ok(Appended::PublishedUnsynced(error)) => {
            tracing::warn!(
                tmux_session = captured.tmux,
                error,
                "clear end published unsynced"
            );
        }
        Err(reason) => {
            tracing::warn!(
                tmux_session = captured.tmux,
                reason,
                "clear end not recorded"
            );
        }
    }
}

fn write_under(captured: &Captured) -> Result<Appended, String> {
    if mutant("authority_nested") {
        tc::with_tmux_source_authority(&captured.tmux, |_| ());
    }
    if !vacant(captured) {
        return Err("pane not confirmed vacant".into());
    }
    let events = p5::records_strict(captured.channel)
        .ok()
        .and_then(Result::ok);
    let events = events.ok_or("binding log unreadable")?;
    let moved = (events.iter())
        .any(|event| event.seq > captured.binding_seq && event.tmux_session == captured.tmux);
    if moved && !mutant("rebind_unchecked") {
        return Err("pane bound again after the capture".into());
    }
    let root = crate::config::runtime_root().ok_or("runtime root unavailable")?;
    let record = ClearEnd {
        channel: captured.channel,
        provider: "claude".into(),
        tmux_session: captured.tmux.clone(),
        execution_nonce: captured.nonce.clone(),
        source: captured.source.clone(),
        binding_seq: captured.binding_seq,
        host: binding_context::stable_host_identity(),
        ended_at: Utc::now(),
        basis: "confirmed_clear".into(),
    };
    clear_end::append(&root, record)
}
