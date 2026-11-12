//! Certified clear adoption uses the existing binding publisher and leaves O cursors to O.
use super::super::{changed, ledger};
use super::{Source, binding_context, nonce};
use crate::services::tmux_common::{self, TmuxSourceAuthority};
use crate::services::tui_prompt_dedupe::{self as dedupe, TuiRuntimeBinding, binding_events};
use binding_context::BindingContext;
use binding_events::{BindingEvent, BindingTarget};
use chrono::{DateTime, Utc};
use std::io::Read;
use std::path::{Path, PathBuf};

#[cfg(test)]
thread_local! { pub(crate) static AFTER_PUBLICATION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) }; }

#[derive(serde::Serialize, serde::Deserialize)]
struct Certified {
    context: BindingContext,
    pending: BindingEvent,
    published: DateTime<Utc>,
    consumed: bool,
}
fn location(session: &str) -> PathBuf {
    tmux_common::session_temp_path(session, "claude_clear_ready").into()
}
fn load(session: &str) -> Option<Certified> {
    let file = std::fs::File::open(location(session)).ok()?;
    let mut bytes = Vec::new();
    file.take(64 * 1024 + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 64 * 1024 {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}
fn save(session: &str, proof: &Certified) -> Option<()> {
    let path = location(session);
    let text = serde_json::to_string(proof).ok()?;
    crate::domain::atomic_write::prepare(&path, &text)
        .ok()?
        .publish()
        .ok()?;
    crate::domain::file_sync::fsync_parent_dir(&path).ok()
}
fn pending(ctx: &BindingContext, native: &str, path: &Path) -> Option<BindingEvent> {
    let channel = ctx.channel_id?;
    let awaited = crate::services::claude::herdr_turn::awaited_clear(
        channel,
        &ctx.tmux_session,
        &ctx.execution_nonce,
    )?;
    if awaited.session_id != native || awaited.transcript.as_deref().map(Path::new) != Some(path) {
        return None;
    }
    binding_events::binding_events_since(channel, 0)
        .ok()?
        .into_iter()
        .rev()
        .find(|event| {
            event.tmux_session == ctx.tmux_session
                && !matches!(event.new, BindingTarget::Rejected { .. })
        })
}
impl Certified {
    fn source(&self) -> Option<Source> {
        let BindingTarget::Pending {
            payload_session_id,
            payload_transcript_path,
        } = &self.pending.new
        else {
            return None;
        };
        Some(Source {
            session: self.context.tmux_session.clone(),
            nonce: Some(self.context.execution_nonce.clone()),
            native: payload_session_id.clone(),
            path: payload_transcript_path.as_ref()?.into(),
        })
    }
    fn current(&self, session: &str, epoch: &str, path: &Path) -> bool {
        let Some(source) = self.source() else {
            return false;
        };
        self.context.provider == "claude"
            && self.context.tmux_session == session
            && self.context.execution_nonce == epoch
            && source.path == path
            && nonce(session) == Some(Some(epoch.into()))
            && binding_context::execution_context("claude", epoch)
                .is_ok_and(|ctx| ctx == self.context)
            && self.published >= self.context.created_at
            && self.context.provider_root.as_deref().is_some_and(|root| {
                crate::services::claude_tui::source_verify::is_top_level_transcript(
                    root,
                    path,
                    &source.native,
                )
            })
            && pending(&self.context, &source.native, path).as_ref() == Some(&self.pending)
    }
}

pub(crate) fn note(
    context: Option<&BindingContext>,
    published: Option<DateTime<Utc>>,
    native: &str,
    path: &Path,
) {
    let (Some(ctx), Some(at)) = (context, published) else {
        return;
    };
    if at < *super::super::STARTED {
        return;
    }
    tmux_common::with_tmux_source_authority(&ctx.tmux_session, |_| {
        let Some(event) = pending(ctx, native, path) else {
            return;
        };
        let mut proof = Certified {
            context: ctx.clone(),
            pending: event,
            published: at,
            consumed: false,
        };
        if !proof.current(&ctx.tmux_session, &ctx.execution_nonce, path) {
            return;
        }
        if let Some(previous) = load(&ctx.tmux_session)
            && previous.context == *ctx
            && previous.pending == proof.pending
        {
            if previous.published >= at {
                return;
            }
            proof.consumed = previous.consumed;
        }
        if save(&ctx.tmux_session, &proof).is_none() {
            tracing::warn!(
                session = ctx.tmux_session,
                "clear readiness certification could not be persisted"
            );
        }
    });
}

/// Retirement and pre-source publication share source authority; a consumed receipt never bootstraps twice.
pub(crate) fn adopt(session: &str, epoch: &str, path: &Path) -> bool {
    tmux_common::with_tmux_source_authority(session, |authority| {
        adopt_under(authority, epoch, path)
    })
}
fn adopt_under(authority: &TmuxSourceAuthority<'_>, epoch: &str, path: &Path) -> bool {
    let session = authority.session();
    if resolved(authority, epoch, path) {
        return true;
    }
    let Some(mut proof) = load(session) else {
        return false;
    };
    if !proof.current(session, epoch, path) {
        return false;
    }
    let Some(source) = proof.source() else {
        return false;
    };
    let Some(channel) = proof.context.channel_id else {
        return false;
    };
    let mut state = ledger();
    if proof.consumed {
        return observed(authority).is_some_and(|binding| projected(&binding, &source))
            && local(&state, &source, proof.published);
    }
    if state
        .entries
        .get(&source)
        .is_some_and(|entry| entry.submitted || entry.pending.is_some())
    {
        return false;
    }
    let key = (session.to_owned(), epoch.to_owned());
    if state
        .starts
        .get(&key)
        .is_some_and(|(at, _)| *at > proof.published)
    {
        return false;
    }
    let Some(old) = observed(authority) else {
        return false;
    };
    if let Some(old) = &old {
        let same_new = old.session_id.as_deref() == Some(source.native.as_str())
            && Path::new(old.relay_output_path()) == source.path;
        let same_old = proof.pending.old.as_ref().is_some_and(|from| {
            old.session_id.as_deref() == Some(from.session_id.as_str())
                && Path::new(old.relay_output_path()) == from.path
        });
        if old.runtime_kind != crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
            || (!same_new && !same_old)
        {
            return false;
        }
    }
    // Persist the one-use fence before publication; a crash in between stays Held.
    proof.consumed = true;
    if save(session, &proof).is_none() {
        return false;
    }
    let binding = TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        output_path: source.path.display().to_string(),
        session_id: Some(source.native.clone()),
        relay_output_path: None,
        input_fifo_path: None,
        last_offset: 0,
        relay_last_offset: None,
    };
    let published = dedupe::pane_registration::register_claude_pane_under_source_authority(
        authority,
        channel,
        binding,
        dedupe::Record::AwaitFirst,
    )
    .is_some_and(dedupe::Persisted::published);
    if !published {
        return false;
    }
    #[cfg(test)]
    if let Some(hook) = AFTER_PUBLICATION.with_borrow_mut(Option::take) {
        hook();
    }
    state.entries.retain(|old, _| {
        old.session != source.session || old.nonce != source.nonce || old == &source
    });
    state.starts.insert(key, (proof.published, source.clone()));
    let entry = state.entries.entry(source).or_default();
    entry.start = true;
    entry.recovery = None;
    entry.revision += 1;
    changed(&mut state);
    true
}

fn observed(authority: &TmuxSourceAuthority<'_>) -> Option<Option<TuiRuntimeBinding>> {
    use dedupe::RuntimeObservation;
    match dedupe::runtime_observation_under_source_authority(authority) {
        RuntimeObservation::Missing => Some(None),
        RuntimeObservation::Present(binding) | RuntimeObservation::Expired(binding) => {
            Some(Some(binding))
        }
        RuntimeObservation::Withheld(_) => None,
    }
}
fn projected(binding: &Option<TuiRuntimeBinding>, source: &Source) -> bool {
    binding.as_ref().is_some_and(|binding| {
        binding.runtime_kind == crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
            && binding.session_id.as_deref() == Some(source.native.as_str())
            && Path::new(binding.relay_output_path()) == source.path
    })
}
fn local(state: &super::super::Ledger, source: &Source, published: DateTime<Utc>) -> bool {
    let Some(epoch) = &source.nonce else {
        return false;
    };
    state
        .starts
        .get(&(source.session.clone(), epoch.clone()))
        .is_some_and(|(at, current)| *at == published && current == source)
        && state
            .entries
            .get(source)
            .is_some_and(|entry| entry.start && !entry.submitted && entry.pending.is_none())
}
fn resolved(authority: &TmuxSourceAuthority<'_>, epoch: &str, path: &Path) -> bool {
    let session = authority.session();
    if nonce(session) != Some(Some(epoch.into())) {
        return false;
    }
    let Ok(ctx) = binding_context::execution_context("claude", epoch) else {
        return false;
    };
    let Some(channel) = ctx.channel_id else {
        return false;
    };
    if ctx.tmux_session != session {
        return false;
    }
    binding_events::claude_history(channel, session, Some(epoch)).is_ok_and(|(pin, history)| {
        history.complete
            && history.awaiting.is_none()
            && pin.is_some_and(|pin| {
                pin.path == path && binding_events::codex::source_file_matches(&pin)
            })
    })
}

/// A wake only asks the owner to retry; the input boundary revalidates and consumes again.
pub(crate) fn wakeable(session: &str, epoch: &str, path: &Path) -> bool {
    let stage = tmux_common::with_tmux_source_authority(session, |authority| {
        if resolved(authority, epoch, path) {
            return Some(false);
        }
        let proof = load(session)?;
        if !proof.current(session, epoch, path) {
            return None;
        }
        let source = proof.source()?;
        let state = ledger();
        if state
            .starts
            .get(&(session.into(), epoch.into()))
            .is_some_and(|(at, _)| *at > proof.published)
        {
            return None;
        }
        if proof.consumed {
            return (observed(authority).is_some_and(|binding| projected(&binding, &source))
                && local(&state, &source, proof.published))
            .then_some(false);
        }
        let old = observed(authority)?;
        if let Some(binding) = &old {
            let from = proof.pending.old.as_ref().is_some_and(|from| {
                binding.session_id.as_deref() == Some(from.session_id.as_str())
                    && Path::new(binding.relay_output_path()) == from.path
            });
            if !projected(&old, &source) && !from {
                return None;
            }
        }
        Some(true)
    });
    match stage {
        Some(true) => true,
        Some(false) => super::super::observe(session, Some(path)).is_some(),
        None => false,
    }
}
