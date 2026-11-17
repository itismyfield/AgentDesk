//! Explicit Codex restart preparation before a successor's Pending row is installed.

use super::*;
use crate::services::cluster::stream_relay::SourceFileIdentity;
use crate::services::codex_tui::session::source_observation::{
    CodexHookSourceClaim, CodexRolloutSource, verify_codex_hook_source,
};
use crate::services::tui_prompt_dedupe::binding_context::ResumeStart;
use crate::services::tui_prompt_dedupe::binding_events::SourceId;

pub(super) fn prepare(
    owner: &HostedOwner,
    observed: &HostedObservation,
    resume: bool,
    native: Option<&str>,
    provider_root: Option<&Path>,
    start: Option<&ResumeStart>,
) -> Result<Option<ResumeStart>, String> {
    if owner.provider != "codex" {
        return if start.is_none() {
            Ok(None)
        } else {
            Err("restart boundary requires Codex".into())
        };
    }
    if !resume && start.is_none() {
        return Ok(None);
    }
    let start = start
        .filter(|_| resume)
        .ok_or("Codex resume requires explicit restart intent")?;
    let native = native.ok_or("Codex restart has no native session")?;
    if !matches!(&observed.record, HostedRecord::Known(record) if record.state == HostedState::Retired && record.owner == *owner)
    {
        return Err("Codex restart requires its confirmed retired predecessor".into());
    }
    let root = provider_root.ok_or("Codex restart has no provider root")?;
    let verified = verify_codex_hook_source(
        root,
        &CodexHookSourceClaim {
            session_id: native,
            transcript_path: Some(&start.path),
            expected_source: CodexRolloutSource::Cli,
        },
    )
    .map_err(|error| format!("Codex restart source: {error:?}"))?;
    let SourceFileIdentity::Unix { dev, ino } = verified.identity else {
        return Err("Codex restart source has no file identity".into());
    };
    let source = SourceId {
        session_id: verified.session_id,
        path: verified.rollout_path,
        dev,
        ino,
    };
    start.validate(&source)?;
    let channel = owner.channel_id.parse::<u64>().map_err(|e| e.to_string())?;
    let runtime = crate::config::runtime_root().ok_or("Codex restart has no runtime root")?;
    let store = crate::services::tui_o::store::OStore::existing(&runtime)
        .ok_or("Codex restart has no O store")?;
    let measured = ResumeStart::capture(start.operation.clone(), channel, &source, &store)?;
    if measured != *start {
        return Err("Codex restart boundary changed before Pending".into());
    }
    Ok(Some(measured))
}
