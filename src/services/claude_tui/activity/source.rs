//! A source visit is the binding owner's verified pin within one immutable execution.
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::claude_tui::source_verify::{self, FirstRecord, OpenedTranscript};
use crate::services::cluster::stream_relay::SourceFileIdentity;
use crate::services::tui_prompt_dedupe::{
    self as dedupe, binding_context as context, binding_events,
};
use chrono::{DateTime, Utc};
use std::{fs, path::PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Source {
    pub session: String,
    pub channel: u64,
    pub owner: String,
    pub host: Option<String>,
    pub nonce: String,
    pub native: String,
    pub path: PathBuf,
    pub file: (u64, u64),
    pub visit: DateTime<Utc>,
}
impl Source {
    pub(super) fn current(session: &str) -> Option<Self> {
        Self::read(session, || {
            dedupe::runtime_binding_for_tmux_session(session)
        })
    }
    pub(super) fn current_under(
        authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    ) -> Option<Self> {
        Self::read(authority.session(), || {
            dedupe::runtime_binding_for_tmux_session_under_source_authority(authority)
        })
    }
    fn read(
        session: &str,
        binding: impl FnOnce() -> Option<dedupe::TuiRuntimeBinding>,
    ) -> Option<Self> {
        let context::SpawnNonceMarker::Known(nonce) = context::observe_spawn_nonce_marker(session)
        else {
            return None;
        };
        let ctx = context::execution_context("claude", &nonce).ok()?;
        if ctx.tmux_session != session {
            return None;
        }
        let binding = binding()?;
        if binding.runtime_kind != RuntimeHandoffKind::ClaudeTui {
            return None;
        }
        let channel = ctx.channel_id?;
        let (pin, history) = binding_events::claude_history(channel, session, Some(&nonce)).ok()?;
        let pin = pin?;
        let visit = history.current?;
        if !history.complete
            || history.awaiting.is_some()
            || visit.pin.as_ref() != Some(&pin)
            || binding.session_id.as_deref() != Some(&pin.session_id)
            || std::path::Path::new(binding.relay_output_path()) != pin.path
            || !source_verify::is_top_level_transcript(
                ctx.provider_root.as_deref()?,
                &pin.path,
                &pin.session_id,
            )
        {
            return None;
        }
        let source = Self {
            session: session.into(),
            channel,
            owner: ctx.owner_runtime_root,
            host: ctx.host,
            nonce,
            native: pin.session_id,
            path: pin.path,
            file: (pin.dev, pin.ino),
            visit: visit.since.unwrap_or(visit.seen).max(ctx.created_at),
        };
        source.stamp()?;
        match source_verify::observe_transcript(&source.path).ok()? {
            OpenedTranscript::Opened {
                file,
                first: FirstRecord::Session(native),
            } if identity(file) == Some(source.file) && native == source.native => Some(source),
            _ => None,
        }
    }
    pub(super) fn stamp(&self) -> Option<Stamp> {
        let meta = fs::metadata(&self.path).ok()?;
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::MetadataExt;
            Some((meta.dev(), meta.ino()))
        };
        #[cfg(not(unix))]
        let file: Option<(u64, u64)> = None;
        if !meta.is_file() || file != Some(self.file) {
            return None;
        }
        Some(Stamp {
            len: meta.len(),
            modified: meta.modified().ok()?,
        })
    }
}
fn identity(file: SourceFileIdentity) -> Option<(u64, u64)> {
    match file {
        #[cfg(unix)]
        SourceFileIdentity::Unix { dev, ino } => Some((dev, ino)),
        SourceFileIdentity::Unavailable => None,
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Stamp {
    pub len: u64,
    pub modified: std::time::SystemTime,
}
