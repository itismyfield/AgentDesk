//! A fresh admission's delivery hold and immutable launch proof share the adoption lock.

use super::*;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_prompt_dedupe::binding_context::{self, BindingContext};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub(crate) struct FirstTurn {
    channel: u64,
    tmux: String,
    mark: TurnMark,
    nonce: String,
    digest: String,
    context: Option<BindingContext>,
    valid: bool,
    pending: bool,
    ended: bool,
    ready: bool,
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
