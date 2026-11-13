//! Durable Pending certificates restore readiness without replacing runtime bindings.
use super::super::{changed, ledger};
use super::{Source, binding_context, nonce};
use crate::services::tmux_common::{self, TmuxSourceAuthority};
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events};
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

fn verified_with(
    session: &str,
    binding: impl FnOnce() -> Option<dedupe::TuiRuntimeBinding>,
) -> Option<(Certified, Source)> {
    let proof = load(session)?;
    let source = proof.source()?;
    let epoch = source.nonce.as_deref()?;
    if !proof.current(session, epoch, &source.path) {
        return None;
    }
    let binding = binding()?;
    let old = proof.pending.old.as_ref()?;
    if binding.runtime_kind != crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
        || binding.session_id.as_deref() != Some(old.session_id.as_str())
        || Path::new(binding.relay_output_path()) != old.path
    {
        return None;
    }
    Some((proof, source))
}

fn verified(session: &str) -> Option<(Certified, Source)> {
    verified_with(session, || {
        dedupe::runtime_binding_for_tmux_session(session)
    })
}
fn verified_under(authority: &TmuxSourceAuthority<'_>) -> Option<(Certified, Source)> {
    verified_with(authority.session(), || {
        dedupe::runtime_binding_for_tmux_session_under_source_authority(authority)
    })
}
/// The durable identity fences old output even when a consumed proof cannot restore local readiness.
pub(in crate::services::claude_tui::readiness) fn restore(session: &str) -> Option<Source> {
    let (proof, source) = verified(session)?;
    restore_verified(proof, source)
}
fn restore_verified(proof: Certified, source: Source) -> Option<Source> {
    let session = source.session.as_str();
    let key = (session.to_owned(), source.nonce.clone()?);
    let mut state = ledger();
    let newer = state
        .starts
        .get(&key)
        .is_some_and(|(at, _)| *at > proof.published);
    if !proof.consumed
        && !newer
        && state
            .entries
            .get(&source)
            .is_none_or(|entry| !entry.submitted && entry.pending.is_none())
        && !local(&state, &source, proof.published)
    {
        state.entries.retain(|old, _| {
            old.session != source.session || old.nonce != source.nonce || old == &source
        });
        state.starts.insert(key, (proof.published, source.clone()));
        let entry = state.entries.entry(source.clone()).or_default();
        entry.start = true;
        entry.clear_start = true;
        entry.recovery = None;
        entry.revision += 1;
        changed(&mut state);
    }
    Some(source)
}

fn local(state: &super::super::Ledger, source: &Source, at: DateTime<Utc>) -> bool {
    let Some(epoch) = &source.nonce else {
        return false;
    };
    state
        .starts
        .get(&(source.session.clone(), epoch.clone()))
        .is_some_and(|(seen, current)| *seen == at && current == source)
        && state
            .entries
            .get(source)
            .is_some_and(|entry| entry.start && entry.clear_start)
}

pub(in crate::services::claude_tui::readiness) fn usable(source: &Source) -> bool {
    eligible(source, verified(&source.session))
}
pub(in crate::services::claude_tui::readiness) fn usable_under(
    source: &Source,
    authority: &TmuxSourceAuthority<'_>,
) -> bool {
    eligible(source, verified_under(authority))
}
fn eligible(source: &Source, current: Option<(Certified, Source)>) -> bool {
    let Some((proof, current)) = current else {
        return false;
    };
    if current != *source {
        return false;
    }
    let state = ledger();
    local(&state, source, proof.published)
        && (!proof.consumed
            || state
                .entries
                .get(source)
                .is_some_and(|entry| entry.pending.is_some() && !entry.submitted))
}

/// Consuming a reservation fences a crash before the first key; known non-delivery may release it.
pub(in crate::services::claude_tui::readiness) fn claim(
    source: &Source,
    expected: &super::super::clear::Evidence,
) -> bool {
    tmux_common::with_tmux_source_authority(&source.session, |authority| {
        if super::super::clear::evidence_under(source, authority).as_ref() != Some(expected) {
            return false;
        }
        let Some((mut proof, current)) = verified_under(authority) else {
            return false;
        };
        if current != *source || proof.consumed {
            return false;
        }
        proof.consumed = true;
        save(&source.session, &proof).is_some()
    })
}

pub(in crate::services::claude_tui::readiness) fn release(
    source: &Source,
    expected: &super::super::clear::Evidence,
) {
    tmux_common::with_tmux_source_authority(&source.session, |authority| {
        let Some((mut proof, current)) = verified_under(authority) else {
            return;
        };
        if current == *source
            && proof.consumed
            && expected.same_pending(proof.pending.channel_id, proof.pending.seq)
        {
            proof.consumed = false;
            if save(&source.session, &proof).is_none() {
                tracing::warn!(
                    session = source.session,
                    "clear reservation release could not be persisted"
                );
            }
        }
    });
}

/// Adoption publishes only local pre-source evidence; the runtime binding and cursors stay old.
pub(crate) fn adopt(session: &str, epoch: &str, path: &Path) -> bool {
    tmux_common::with_tmux_source_authority(session, |authority| {
        if resolved(authority, epoch, path) {
            return true;
        }
        let Some((proof, source)) = verified_under(authority) else {
            return false;
        };
        let Some(source) = restore_verified(proof, source) else {
            return false;
        };
        if source.nonce.as_deref() != Some(epoch)
            || source.path != path
            || !usable_under(&source, authority)
        {
            return false;
        }
        #[cfg(test)]
        if let Some(hook) = AFTER_PUBLICATION.with_borrow_mut(Option::take) {
            hook();
        }
        true
    })
}

pub(crate) fn wakeable(session: &str, epoch: &str, path: &Path) -> bool {
    nonce(session) == Some(Some(epoch.into()))
        && super::super::observe(session, Some(path)).is_some()
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
