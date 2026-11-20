//! A fresh admission's delivery hold and immutable launch proof share the adoption lock.

use super::*;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_prompt_dedupe::binding_context::{self, BindingContext};
use sha2::{Digest, Sha256};

#[cfg(any(test, unix))]
mod output;
#[cfg(any(test, unix))]
pub(crate) use output::FirstHold;

#[derive(Clone, Debug)]
pub(crate) struct FirstTurn {
    channel: u64,
    tmux: String,
    mark: TurnMark,
    nonce: String,
    digest: String,
    context: Option<BindingContext>,
    #[cfg(test)]
    blocked_source_digest: Option<String>,
    #[cfg(any(test, unix))]
    normalizing: output::Normalizing,
    valid: bool,
    pending: bool,
    ended: bool,
    ready: bool,
}

#[cfg(any(test, unix))]
impl FirstTurn {
    fn names(&self, proof: &FirstProof) -> bool {
        self.channel == proof.channel
            && self.mark == proof.mark
            && self.nonce == proof.nonce
            && self.digest == proof.digest
            && self.context.as_ref() == Some(&proof.context)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FirstProof {
    pub(crate) channel: u64,
    pub(crate) mark: TurnMark,
    pub(crate) nonce: String,
    pub(crate) digest: String,
    pub(crate) context: BindingContext,
    pub(crate) source: SourceId,
}

pub(crate) fn digest(prompt: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(prompt.as_bytes()))
}

pub(crate) fn nonce_id(nonce: &str) -> [u8; 32] {
    Sha256::digest(nonce.as_bytes()).into()
}

impl FirstProof {
    pub(crate) fn names_binding(
        &self,
        event: &crate::services::tui_o::writer::binding::BindingEvent,
    ) -> bool {
        event.channel_id == self.channel
            && event.provider == crate::services::tui_o::shadow::ShadowProvider::Codex
            && event.tmux_session == self.context.tmux_session
            && event.execution_nonce == self.context.execution_nonce
    }

    pub(crate) fn with_current<T>(
        &self,
        action: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        use crate::services::{tmux_common as tc, tui_prompt_dedupe as dedupe};
        tc::with_tmux_source_authority(&self.context.tmux_session, |authority| {
            let nonce = binding_context::observe_spawn_nonce_marker(authority.session());
            if nonce
                != binding_context::SpawnNonceMarker::Known(self.context.execution_nonce.clone())
                || self.context.owner_runtime_root != tc::current_tmux_owner_marker()
                || binding_context::execution_context("codex", &self.context.execution_nonce)?
                    != self.context
            {
                return Err("first admission no longer names the current execution".into());
            }
            let binding =
                dedupe::runtime_binding_for_tmux_session_under_source_authority(authority)
                    .ok_or("first admission source mapping is missing")?;
            let path = std::path::Path::new(&binding.output_path)
                .canonicalize()
                .map_err(|e| e.to_string())?;
            let source = self.source.path.canonicalize().map_err(|e| e.to_string())?;
            if binding.runtime_kind != crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
                || dedupe::owner_channel_for_tmux_session(authority.session()) != Some(self.channel)
                || binding.session_id.as_deref() != Some(self.source.session_id.as_str())
                || !dedupe::codex_verified_source_allowed_under_source_authority(
                    authority,
                    &binding.output_path,
                    binding.session_id.as_deref(),
                )
                || path != source
                || self
                    .context
                    .provider_root
                    .as_ref()
                    .is_none_or(|root| !source.starts_with(root))
            {
                return Err("first admission source mapping changed".into());
            }
            action()
        })
    }
}

/// A clear invalidates once and excludes fresh admission until its final effect has finished.
pub(crate) struct FirstPause(Candidate);

impl Drop for FirstPause {
    fn drop(&mut self) {
        let guard = self.0.lock();
        self.0.first_pauses.fetch_sub(1, Ordering::Relaxed);
        self.0.first_turn.send_modify(|_| {});
        drop(guard);
    }
}

pub(crate) struct FirstAdmission {
    candidate: Candidate,
    nonce: String,
    completed: bool,
    order: u64,
}

impl FirstAdmission {
    pub(crate) fn completed(&mut self) {
        self.completed = true;
    }
}

impl Drop for FirstAdmission {
    fn drop(&mut self) {
        let guard = self.candidate.lock();
        self.candidate.first_turn.send_modify(|first| {
            if let Some(first) = first
                .as_mut()
                .filter(|first| first.nonce == self.nonce && first.mark.order == self.order)
            {
                first.ended = true;
                if !self.completed {
                    first.valid = false;
                    first.pending = false;
                }
            }
        });
        drop(guard);
    }
}

impl Candidate {
    pub(crate) fn pause_first(&self) -> FirstPause {
        let guard = self.lock();
        self.first_pauses.fetch_add(1, Ordering::Relaxed);
        self.first_turn.send_modify(|first| {
            if let Some(first) = first {
                first.valid = false;
                first.pending = false;
            }
        });
        drop(guard);
        FirstPause(self.clone())
    }

    pub(crate) fn admit_first(
        &self,
        channel: u64,
        nonce: &str,
        prompt: &str,
        tmux: &str,
        read_seq: impl FnOnce() -> Result<u64, String>,
    ) -> Option<FirstAdmission> {
        if nonce.is_empty() || tmux.is_empty() {
            return None;
        }
        let (sends, order) = {
            let guard = self.lock();
            if *guard != Adoption::Deferred
                || !self.late_origin
                || self.first_pauses.load(Ordering::Relaxed) > 0
            {
                return None;
            }
            let (sends, order) = self.baseline_sends();
            *self
                .turn_mark
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = None;
            self.first_turn.send_replace(Some(FirstTurn {
                channel,
                tmux: tmux.into(),
                mark: TurnMark {
                    sends,
                    seq: 0,
                    order,
                },
                nonce: nonce.into(),
                digest: digest(prompt),
                context: None,
                #[cfg(test)]
                blocked_source_digest: None,
                #[cfg(any(test, unix))]
                normalizing: output::Normalizing::admitted(),
                valid: true,
                pending: true,
                ended: false,
                ready: false,
            }));
            (sends, order)
        };
        let seq = read_seq();
        let guard = self.lock();
        let mut accepted = None;
        self.first_turn.send_modify(|first| {
            let Some(first) = first
                .as_mut()
                .filter(|first| first.valid && first.mark.order == order)
            else {
                return;
            };
            match seq {
                Ok(seq) if *guard == Adoption::Deferred => {
                    first.mark = TurnMark { sends, seq, order };
                    first.ready = true;
                    accepted = Some(first.mark);
                }
                _ => {
                    first.valid = false;
                    first.pending = false;
                }
            }
        });
        let mark = accepted?;
        *self
            .turn_mark
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(mark);
        Some(FirstAdmission {
            candidate: self.clone(),
            nonce: nonce.into(),
            completed: false,
            order,
        })
    }

    #[cfg(test)]
    pub(crate) fn admit_after_rejection(
        &self,
        previous: &super::output_disposition::Output,
        next: (u64, &str, &str, &BindingContext),
        read_seq: impl FnOnce() -> Result<u64, String>,
    ) -> Option<FirstAdmission> {
        use super::output_disposition::Phase;
        let (input, nonce, prompt, context) = next;
        let tmux = context.tmux_session.as_str();
        if !matches!(
            previous.phase(),
            Phase::RejectPendingNotice(_) | Phase::Rejected(_)
        ) || input == previous.id().input
            || nonce.is_empty()
            || digest(nonce) == previous.id().nonce
            || context.execution_nonce.is_empty()
            || digest(&context.execution_nonce) == previous.id().execution
            || context.provider != "codex"
            || context.launch_mode != "fresh"
            || context.channel_id != Some(previous.id().channel)
        {
            return None;
        }
        {
            let mut guard = self.lock();
            let first = self.first_turn.borrow();
            let old = first.as_ref()?;
            if old.channel != previous.id().channel
                || digest(&old.nonce) != previous.id().nonce
                || old.tmux != tmux
                || !matches!(*guard, Adoption::Deferred | Adoption::Refused)
            {
                return None;
            }
            if *guard == Adoption::Refused {
                *guard = Adoption::Deferred;
            }
        }
        self.admit_first(previous.id().channel, nonce, prompt, tmux, || {
            self.attach_first_execution(context, prompt, Some(nonce));
            {
                let guard = self.lock();
                self.first_turn.send_modify(|first| {
                    if let Some(first) = first.as_mut().filter(|first| {
                        first.nonce == nonce && first.context.as_ref() == Some(context)
                    }) {
                        first.blocked_source_digest = Some(previous.id().source.clone());
                    }
                });
                drop(guard);
            }
            read_seq()
        })
    }

    pub(crate) fn first_mark(&self) -> Option<TurnMark> {
        self.first_turn
            .borrow()
            .as_ref()
            .filter(|first| first.valid && first.ready)
            .map(|first| first.mark)
    }

    pub(crate) fn first_ready(&self) -> bool {
        self.first_pauses.load(Ordering::Relaxed) == 0
            && self
                .first_turn
                .borrow()
                .as_ref()
                .is_none_or(|first| !first.valid || first.ready)
    }

    pub(crate) fn decline_first(&self, order: u64) {
        let guard = self.lock();
        self.first_turn.send_if_modified(|first| {
            if let Some(first) = first
                .as_mut()
                .filter(|first| first.mark.order == order && (first.valid || first.pending))
            {
                first.valid = false;
                first.pending = false;
                return true;
            }
            false
        });
        drop(guard);
    }

    pub(crate) fn attach_first_execution(
        &self,
        context: &BindingContext,
        prompt: &str,
        nonce: Option<&str>,
    ) {
        let guard = self.lock();
        self.first_turn.send_modify(|first| {
            let Some(first) = first.as_mut().filter(|first| {
                first.valid && first.pending && nonce == Some(first.nonce.as_str())
            }) else {
                return;
            };
            if context.channel_id != Some(first.channel)
                || context.tmux_session != first.tmux
                || context.provider != "codex"
                || context.launch_mode != "fresh"
                || context.first_prompt_digest.as_ref() != Some(&first.digest)
                || digest(prompt) != first.digest
                || context.execution_nonce.is_empty()
                || context.provider_root.is_none()
                || first.context.as_ref().is_some_and(|old| old != context)
            {
                first.valid = false;
                first.pending = false;
            } else {
                first.context = Some(context.clone());
            }
        });
        drop(guard);
    }

    /// The caller holds the adoption lock, including when comparing this to an initial proof.
    pub(crate) fn first_proof(&self, source: &SourceId) -> Result<Option<FirstProof>, String> {
        if self.first_pauses.load(Ordering::Relaxed) > 0 {
            return Err("first admission is excluded by clear".into());
        }
        let first = self.first_turn.borrow();
        let Some(first) = first.as_ref() else {
            return Ok(None);
        };
        if !first.valid || !first.ready {
            return Err("first admission was invalidated".into());
        }
        #[cfg(test)]
        if first.blocked_source_digest.as_ref().is_some_and(|blocked| {
            blocked == &super::output_disposition::Identity::source_digest(source)
        }) {
            return Err("a fresh admission cannot reuse the rejected source".into());
        }
        let context = first
            .context
            .clone()
            .ok_or("first admission has no prepared execution")?;
        Ok(Some(FirstProof {
            channel: first.channel,
            mark: first.mark,
            nonce: first.nonce.clone(),
            digest: first.digest.clone(),
            context,
            source: source.clone(),
        }))
    }

    pub(crate) fn invalidate_first(&self, nonce: Option<&str>, tmux: Option<&str>) {
        let guard = self.lock();
        self.first_turn.send_if_modified(|first| {
            if let Some(first) = first.as_mut().filter(|first| {
                nonce.is_none_or(|nonce| first.nonce == nonce)
                    && tmux.is_none_or(|tmux| first.tmux == tmux)
            }) {
                let changed = first.valid || first.pending;
                first.valid = false;
                first.pending = false;
                return changed;
            }
            false
        });
        drop(guard);
    }

    #[cfg(any(test, unix))]
    pub(crate) fn first_names(&self, proof: &FirstProof) -> bool {
        self.first_turn
            .borrow()
            .as_ref()
            .is_some_and(|first| first.names(proof))
    }

    #[cfg(any(test, unix))]
    pub(crate) fn invalidate_first_proved(&self, proof: &FirstProof) -> bool {
        let mut matched = false;
        self.first_turn.send_if_modified(|first| {
            let Some(first) = first.as_mut().filter(|first| first.names(proof)) else {
                return false;
            };
            matched = true;
            let changed = first.valid || first.pending;
            first.valid = false;
            first.pending = false;
            changed
        });
        matched
    }

    pub(crate) fn first_ended(&self) -> bool {
        self.first_turn
            .borrow()
            .as_ref()
            .is_some_and(|first| first.ended)
    }

    pub(crate) fn first_changes(&self) -> tokio::sync::watch::Receiver<Option<FirstTurn>> {
        self.first_turn.subscribe()
    }

    pub(crate) async fn wait_first(&self, nonce: Option<[u8; 32]>) {
        let mut notice = self.first_turn.subscribe();
        while notice
            .borrow_and_update()
            .as_ref()
            .is_some_and(|first| first.pending && nonce == Some(nonce_id(&first.nonce)))
        {
            if notice.changed().await.is_err() {
                break;
            }
        }
    }

    /// Decision and host-exit callers already hold the adoption lock.
    pub(crate) fn settle_first(&self) {
        self.first_turn.send_modify(|first| {
            if let Some(first) = first {
                first.pending = false;
            }
        });
    }
}
