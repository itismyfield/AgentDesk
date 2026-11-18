//! Exact launch, nonce-scoped pin, native identity and the descriptor behind the reply.
use super::super::{evidence::Stamp, source::Source};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::claude_tui::source_verify::{self, FirstRecord, OpenedTranscript};
use crate::services::cluster::stream_relay::SourceFileIdentity;
use crate::services::tui_prompt_dedupe::{self, binding_context, binding_events};
use chrono::{DateTime, Utc};
use std::{fs::File, path::Path};

pub(super) fn launched(source: &Source, stamp: &Stamp) -> Option<DateTime<Utc>> {
    let nonce = source.nonce.as_deref()?;
    let ctx = binding_context::execution_context("claude", nonce).ok()?;
    let binding = tui_prompt_dedupe::runtime_binding_for_tmux_session(&source.session)?;
    if ctx.tmux_session != source.session
        || binding.runtime_kind != RuntimeHandoffKind::ClaudeTui
        || Path::new(binding.relay_output_path()) != source.path
    {
        return None;
    }
    let (pin, history) =
        binding_events::claude_history_at(ctx.channel_id?, &source.session, Some(nonce)).ok()?;
    let (pin, _) = pin?;
    let nonce_matches = history.complete
        && history.awaiting.is_none()
        && history.current.as_ref().is_some_and(|current| {
            current.pin.as_ref() == Some(&pin) && current.session == pin.session_id
        });
    if !nonce_matches {
        return None;
    }
    if pin.path != source.path {
        return None;
    }
    let OpenedTranscript::Opened {
        file,
        first: FirstRecord::Session(first),
    } = source_verify::observe_transcript(&source.path).ok()?
    else {
        return None;
    };
    let native_matches = pin.session_id == source.native
        && first == source.native
        && ctx.expected_native_session_id.as_deref() == Some(source.native.as_str())
        && binding.session_id.as_deref() == Some(source.native.as_str());
    if !native_matches {
        return None;
    }
    let inode_matches = (pin.dev, pin.ino) == stamp.file && identity(file) == Some(stamp.file);
    if !inode_matches {
        return None;
    }
    Some(ctx.created_at)
}
fn identity(file: SourceFileIdentity) -> Option<(u64, u64)> {
    match file {
        #[cfg(unix)]
        SourceFileIdentity::Unix { dev, ino } => Some((dev, ino)),
        _ => None,
    }
}
pub(super) fn same_file(file: &File, path: &Path, stamp: &Stamp) -> bool {
    let (Ok(opened), Ok(current)) = (file.metadata(), std::fs::metadata(path)) else {
        return false;
    };
    identity(SourceFileIdentity::from_open_file(file)) == Some(stamp.file)
        && opened.len() == stamp.len
        && opened.modified().ok() == current.modified().ok()
        && Stamp::read(path).as_ref() == Some(stamp)
}
