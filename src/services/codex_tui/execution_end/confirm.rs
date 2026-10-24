use std::time::Duration;

use super::capture::CapturedOldExecution;
use crate::services::platform::tmux::{self, SessionPresence};
use crate::services::process::{ProcessGroupProbe, ProcessIdentityProbe, process_group_probe};
use crate::services::tmux_common as tc;
use crate::services::tui_o::store::codex_end::{self, Appended, CodexEnd, ContextSnapshot};
use crate::services::tui_prompt_dedupe::{binding_context, binding_events};

const VACANT_WAIT: Duration = Duration::from_secs(2);
const VACANT_POLL: Duration = Duration::from_millis(100);

/// Can only be constructed after independent pane, PID and process-group confirmation.
struct ConfirmedOldEnd(CodexEnd);

#[derive(Debug)]
struct Vacancy {
    pane: SessionPresence,
    managed_pid: Option<u32>,
    pid: ProcessIdentityProbe,
    pgid: ProcessGroupProbe,
}

impl Vacancy {
    fn observe(capture: &CapturedOldExecution) -> Self {
        Self {
            pane: tmux::session_presence(&capture.record.tmux_session),
            managed_pid: crate::services::session_backend::process_session_pid(
                &capture.record.tmux_session,
            ),
            pid: capture.identity.probe(capture.pane_pid),
            pgid: process_group_probe(capture.pgid),
        }
    }
    fn vacant(&self) -> bool {
        self.pane == SessionPresence::Missing
            && self.managed_pid.is_none()
            && self.pid == ProcessIdentityProbe::GoneOrReused
            && (self.pgid == ProcessGroupProbe::Gone || super::mutant("group_unchecked"))
    }
}

pub(crate) async fn record_clear(capture: CapturedOldExecution, reset: bool, unconfirmed: bool) {
    if !reset || unconfirmed {
        warn(&capture, "clear effect unconfirmed");
        return;
    }
    let deadline = tokio::time::Instant::now() + VACANT_WAIT;
    loop {
        if Vacancy::observe(&capture).vacant() {
            break;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        tokio::time::sleep(VACANT_POLL.min(deadline - now)).await;
    }
    let result = tc::with_tmux_source_authority(&capture.record.tmux_session, |authority| {
        let confirmed = confirm(&capture)?;
        let root = crate::config::runtime_root().ok_or("runtime root unavailable")?;
        match codex_end::append(&root, &confirmed.0)? {
            Appended::Durable => {
                let ticket = super::attempt::install_ticket(
                    confirmed.0.channel,
                    authority,
                    capture.epoch,
                    &confirmed.0.end_id,
                );
                if !ticket {
                    warn(
                        &capture,
                        "attempt changed or may still spawn; End has no ticket",
                    );
                }
                tracing::debug!(
                    end_id = confirmed.0.end_id,
                    ticket,
                    "Codex clear End durable"
                );
                Ok(())
            }
            Appended::PublishedUnsynced(error) => Err(format!("End published unsynced: {error}")),
        }
    });
    if let Err(reason) = result {
        warn(&capture, &reason);
    }
}

fn confirm(capture: &CapturedOldExecution) -> Result<ConfirmedOldEnd, String> {
    if !Vacancy::observe(capture).vacant() {
        return Err("pane/pid/pgid not confirmed vacant".into());
    }
    use binding_context::SpawnNonceMarker;
    match binding_context::observe_spawn_nonce_marker(&capture.record.tmux_session) {
        SpawnNonceMarker::Absent if !super::mutant("absent_rejected") => {}
        SpawnNonceMarker::Known(nonce) if nonce == capture.record.execution_nonce => {}
        _ => return Err("spawn marker changed or unreadable".into()),
    }
    let old = &capture.record;
    match std::fs::read_to_string(tc::tmux_owner_path(&old.tmux_session)) {
        Ok(owner) if owner.trim() == old.owner_runtime_root => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        _ => return Err("pane owner marker changed or unreadable".into()),
    }
    let context = binding_context::execution_context("codex", &old.execution_nonce)?;
    if ContextSnapshot::from(&context) != old.context
        || binding_context::stable_host_identity().as_deref() != Some(old.host.as_str())
        || tc::current_tmux_owner_marker() != old.owner_runtime_root
    {
        return Err("execution context/host/owner changed".into());
    }
    if super::identity::verify_source(&context, &old.source)? != old.provider_root {
        return Err("provider root identity changed".into());
    }
    let events = binding_events::records_strict(old.channel)
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("binding log corrupt: {e:?}"))?;
    if events.last().map_or(0, |e| e.seq) < old.binding_seq
        || events.iter().any(|e| {
            e.channel_id != old.channel
                || (e.seq > old.binding_seq && e.tmux_session == old.tmux_session)
        })
    {
        return Err("pane binding moved after capture".into());
    }
    let mut record = old.clone();
    if super::mutant("snapshot_normalized") {
        record.context.provider_root = Some(record.provider_root.clone());
    }
    record.ended_at = chrono::Utc::now();
    Ok(ConfirmedOldEnd(record))
}

/// A successful same-name create replaced the old pane; its captured processes must still be gone.
pub(super) fn replacement(
    capture: &CapturedOldExecution,
    successor_nonce: &str,
    first_seq: u64,
) -> Result<Option<CodexEnd>, String> {
    let old = &capture.record;
    let vacancy = Vacancy::observe(capture);
    if vacancy.managed_pid.is_some()
        || vacancy.pid != ProcessIdentityProbe::GoneOrReused
        || vacancy.pgid != ProcessGroupProbe::Gone
    {
        return Ok(None);
    }
    if binding_context::observe_spawn_nonce_marker(&old.tmux_session)
        != binding_context::SpawnNonceMarker::Known(successor_nonce.to_owned())
    {
        return Err("replacement marker changed".into());
    }
    let context = binding_context::execution_context("codex", &old.execution_nonce)?;
    if ContextSnapshot::from(&context) != old.context
        || binding_context::stable_host_identity().as_deref() != Some(old.host.as_str())
        || tc::current_tmux_owner_marker() != old.owner_runtime_root
        || super::identity::verify_source(&context, &old.source)? != old.provider_root
    {
        return Err("old replacement context/host/owner/source changed".into());
    }
    let events = binding_events::records_strict(old.channel)
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("binding log corrupt: {e:?}"))?;
    if events.iter().any(|event| {
        event.channel_id != old.channel
            || (event.seq > old.binding_seq
                && event.seq < first_seq
                && event.tmux_session == old.tmux_session)
    }) {
        return Err("old binding changed before replacement".into());
    }
    let mut record = old.clone();
    record.basis = codex_end::EndBasis::DirectReplacement;
    record.ended_at = chrono::Utc::now();
    Ok(Some(record))
}

fn warn(capture: &CapturedOldExecution, reason: &str) {
    if super::mutant("reason_removed") {
        tracing::warn!("Codex clear end not recorded");
        return;
    }
    let vacancy = Vacancy::observe(capture);
    tracing::warn!(channel = capture.record.channel, pane = capture.record.tmux_session,
        pid = capture.pane_pid, pgid = capture.pgid, reason,
        pane_state = ?vacancy.pane, managed_pid = ?vacancy.managed_pid,
        pid_state = ?vacancy.pid, pgid_state = ?vacancy.pgid, attempt_state = capture.epoch, "Codex clear end not recorded");
}
