use chrono::Utc;

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::platform::tmux::{self, SessionPresence};
use crate::services::process::{ProcessIdentity, ProcessIdentityProbe};
use crate::services::tmux_common::{self as tc, TmuxSourceAuthority};
use crate::services::tui_o::store::codex_end::{CodexEnd, ContextSnapshot, EndBasis};
use crate::services::tui_prompt_dedupe::binding_context::{self, SpawnNonceMarker};
use crate::services::tui_prompt_dedupe::binding_events as p5;

pub(crate) struct CapturedOldExecution {
    pub(super) record: CodexEnd,
    pub(super) pane_pid: u32,
    pub(super) identity: ProcessIdentity,
    pub(super) pgid: u32,
    pub(super) epoch: u64,
}

pub(crate) fn capture(channel: u64, pane: &str) -> Result<CapturedOldExecution, String> {
    tc::with_tmux_source_authority(pane, |authority| capture_under(channel, authority))
}

fn capture_under(
    channel: u64,
    authority: &TmuxSourceAuthority<'_>,
) -> Result<CapturedOldExecution, String> {
    let pane = authority.session();
    if tmux::session_presence(pane) != SessionPresence::Present {
        return Err("pane not present".into());
    }
    let SpawnNonceMarker::Known(nonce) = binding_context::observe_spawn_nonce_marker(pane) else {
        return Err("no current spawn nonce".into());
    };
    let context = binding_context::execution_context("codex", &nonce)?;
    let host = binding_context::stable_host_identity().ok_or("host unavailable")?;
    if context.channel_id != Some(channel)
        || context.tmux_session != pane
        || context.host.as_deref() != Some(host.as_str())
        || context.owner_runtime_root != tc::current_tmux_owner_marker()
        || std::fs::read_to_string(tc::tmux_owner_path(pane))
            .ok()
            .as_deref()
            .map(str::trim)
            != Some(context.owner_runtime_root.as_str())
    {
        return Err("execution context channel/pane/host/owner mismatch".into());
    }
    let settled = p5::codex::settled::read(channel, pane)
        .map_err(|e| format!("binding log corrupt: {e}"))?
        .ok_or("pane source unsettled")?;
    let (source, binding_nonce) = (settled.source, settled.execution_nonce);
    if context.launch_mode == "fresh"
        && binding_nonce.as_deref() != Some(nonce.as_str())
        && !super::mutant("nonce_unchecked")
    {
        return Err("fresh source belongs to another execution".into());
    }
    let provider_root = super::identity::verify_source(&context, &source)?;
    let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session_under_source_authority(authority)
        .ok_or("runtime source unavailable")?;
    if binding.runtime_kind != RuntimeHandoffKind::CodexTui
        || binding.session_id.as_deref() != Some(source.session_id.as_str())
        || std::path::Path::new(&binding.output_path)
            .canonicalize()
            .ok()
            .as_ref()
            != Some(&source.path)
    {
        return Err("runtime and settled source disagree".into());
    }
    let pane_pid = tmux::pane_pid(pane).ok_or("pane PID unavailable")?;
    let identity = ProcessIdentity::capture(pane_pid);
    if identity.probe(pane_pid) != ProcessIdentityProbe::Same {
        return Err("pane PID identity unavailable".into());
    }
    let pgid = process_group(pane_pid).ok_or("pane pgid unavailable")?;
    let epoch = super::attempt::generation(channel, authority);
    let record = CodexEnd {
        end_id: uuid::Uuid::new_v4().simple().to_string(),
        channel,
        provider: "codex".into(),
        tmux_session: pane.to_owned(),
        execution_nonce: nonce,
        host,
        owner_runtime_root: context.owner_runtime_root.clone(),
        provider_root,
        source,
        binding_seq: settled.binding_seq,
        captured_at: Utc::now(),
        ended_at: Utc::now(),
        basis: EndBasis::ExplicitClear,
        context: ContextSnapshot::from(&context),
        successor: None,
    };
    Ok(CapturedOldExecution {
        record,
        pane_pid,
        identity,
        pgid,
        epoch,
    })
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
