//! The pinned test incarnation reads only its current proof and preserves held bytes.

use crate::services::{
    cluster::stream_relay::SourceFileIdentity,
    provider::{CancelToken, cancel_requested},
    tmux_common as tc,
    tui_prompt_dedupe::{
        self as dedupe,
        binding_context::{self, SpawnNonceMarker},
    },
};
use std::{
    fs::File,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(crate) fn is_verified(tmux: Option<&str>) -> bool {
    tmux.is_some_and(|tmux| {
        tmux == super::canary::CANARY_TMUX && dedupe::codex_verified_requires_proof(tmux)
    })
}

pub(crate) fn wait_for_binding(
    tmux: &str,
    cancel: Option<&CancelToken>,
) -> Result<dedupe::TuiRuntimeBinding, String> {
    let nonce = binding_context::observe_spawn_nonce_marker(tmux);
    let mut polling = HoldPoll::new(tmux);
    loop {
        if cancel_requested(cancel) {
            return Err("cancelled waiting for verified Codex source".into());
        }
        if matches!(&nonce, SpawnNonceMarker::Known(_))
            && binding_context::observe_spawn_nonce_marker(tmux) == nonce
            && let Some(binding) = dedupe::runtime_binding_for_tmux_session(tmux)
            && binding.runtime_kind == crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
            && SourcePin::new(
                Some(tmux),
                Path::new(&binding.output_path),
                binding.session_id.as_deref(),
            )
            .is_some_and(|pin| pin.allowed(None))
        {
            return Ok(binding);
        }
        polling.pause(None);
    }
}

pub(super) enum PinState {
    Current,
    Held,
    Advanced(dedupe::TuiRuntimeBinding),
}

pub(super) struct SourcePin {
    tmux: String,
    path: PathBuf,
    session_id: Option<String>,
    nonce: SpawnNonceMarker,
    identity: SourceFileIdentity,
}

pub(super) struct OpenedSource {
    pub file: File,
    pub pin: SourcePin,
    pub path: PathBuf,
    pub offset: u64,
    pub session_id: Option<String>,
}

impl Clone for SourcePin {
    fn clone(&self) -> Self {
        Self {
            tmux: self.tmux.clone(),
            path: self.path.clone(),
            session_id: self.session_id.clone(),
            nonce: match &self.nonce {
                SpawnNonceMarker::Known(nonce) => SpawnNonceMarker::Known(nonce.clone()),
                SpawnNonceMarker::Absent => SpawnNonceMarker::Absent,
                SpawnNonceMarker::Unreadable => SpawnNonceMarker::Unreadable,
            },
            identity: self.identity,
        }
    }
}

impl SourcePin {
    pub(super) fn new(tmux: Option<&str>, path: &Path, session_id: Option<&str>) -> Option<Self> {
        let tmux = tmux.filter(|_| is_verified(tmux))?;
        Some(Self {
            tmux: tmux.into(),
            path: path.to_owned(),
            session_id: session_id.map(str::to_owned),
            nonce: binding_context::observe_spawn_nonce_marker(tmux),
            identity: File::open(path)
                .ok()
                .map(|file| SourceFileIdentity::from_open_file(&file))
                .unwrap_or(SourceFileIdentity::Unavailable),
        })
    }

    fn allowed(&self, identity: Option<SourceFileIdentity>) -> bool {
        tc::with_tmux_source_authority(&self.tmux, |authority| {
            self.allowed_under(authority, identity)
        })
    }

    fn allowed_under(
        &self,
        authority: &tc::TmuxSourceAuthority<'_>,
        identity: Option<SourceFileIdentity>,
    ) -> bool {
        let SpawnNonceMarker::Known(nonce) = &self.nonce else {
            return false;
        };
        if binding_context::observe_spawn_nonce_marker(&self.tmux) != self.nonce
            || !dedupe::codex_verified_source_allowed_under_source_authority(
                authority,
                &self.path.display().to_string(),
                self.session_id.as_deref(),
            )
        {
            return false;
        }
        let Ok(context) = binding_context::execution_context("codex", nonce) else {
            return false;
        };
        if context.channel_id != Some(super::canary::CANARY_CHANNEL) {
            return false;
        }
        let Ok(fold) = dedupe::binding_events::codex::read_ownership(&context) else {
            return false;
        };
        let Some(proof) = fold.verified else {
            return false;
        };
        identity.is_none_or(|identity| {
            #[cfg(unix)]
            {
                identity
                    == SourceFileIdentity::Unix {
                        dev: proof.source.dev,
                        ino: proof.source.ino,
                    }
            }
            #[cfg(not(unix))]
            {
                let _ = (identity, proof);
                false
            }
        })
    }

    fn identity_hold_reason(&self, identity: Option<SourceFileIdentity>) -> Option<&'static str> {
        let identity = identity.or_else(|| {
            File::open(&self.path)
                .ok()
                .map(|file| SourceFileIdentity::from_open_file(&file))
        })?;
        tc::with_tmux_source_authority(&self.tmux, |_| {
            let SpawnNonceMarker::Known(nonce) = &self.nonce else {
                return None;
            };
            let context = binding_context::execution_context("codex", nonce).ok()?;
            let proof = dedupe::binding_events::codex::read_ownership(&context)
                .ok()?
                .verified?;
            #[cfg(unix)]
            let matches = identity
                == SourceFileIdentity::Unix {
                    dev: proof.source.dev,
                    ino: proof.source.ino,
                };
            #[cfg(not(unix))]
            let matches = {
                let _ = (identity, proof);
                false
            };
            (!matches).then_some("source_identity_mismatch")
        })
    }

    pub(super) fn state(&self, identity: SourceFileIdentity) -> PinState {
        tc::with_tmux_source_authority(&self.tmux, |authority| {
            if self.allowed_under(authority, Some(identity)) {
                return PinState::Current;
            }
            let SpawnNonceMarker::Known(nonce) = &self.nonce else {
                return PinState::Held;
            };
            if binding_context::observe_spawn_nonce_marker(&self.tmux) != self.nonce
                || !binding_context::execution_context("codex", nonce)
                    .is_ok_and(|context| context.channel_id == Some(super::canary::CANARY_CHANNEL))
            {
                return PinState::Held;
            }
            match dedupe::runtime_binding_for_tmux_session_under_source_authority(authority) {
                Some(binding)
                    if binding.runtime_kind
                        == crate::services::agent_protocol::RuntimeHandoffKind::CodexTui =>
                {
                    PinState::Advanced(binding)
                }
                Some(_) => PinState::Held,
                None => PinState::Held,
            }
        })
    }

    pub(super) fn wait(&self, cancel: Option<&CancelToken>, identity: SourceFileIdentity) -> bool {
        let mut polling = HoldPoll::new(&self.tmux);
        loop {
            if cancel_requested(cancel) {
                return false;
            }
            match self.state(identity) {
                PinState::Current => return true,
                PinState::Advanced(_) => return false,
                PinState::Held => polling.pause(self.identity_hold_reason(Some(identity))),
            }
        }
    }

    pub(super) fn open(
        mut self,
        cancel: Option<&CancelToken>,
        mut start: u64,
    ) -> Option<OpenedSource> {
        let tmux = self.tmux.clone();
        let mut polling = HoldPoll::new(&tmux);
        loop {
            if cancel_requested(cancel) {
                return None;
            }
            match self.state(self.identity) {
                PinState::Advanced(binding) => {
                    start = binding.last_offset;
                    self = Self::new(
                        Some(&self.tmux),
                        Path::new(&binding.output_path),
                        binding.session_id.as_deref(),
                    )?;
                    continue;
                }
                PinState::Current => {
                    if let Ok(file) = File::open(&self.path)
                        && file
                            .metadata()
                            .is_ok_and(|metadata| metadata.len() >= start)
                        && self.allowed(Some(SourceFileIdentity::from_open_file(&file)))
                    {
                        return Some(OpenedSource {
                            file,
                            path: self.path.clone(),
                            session_id: self.session_id.clone(),
                            offset: start,
                            pin: self,
                        });
                    }
                }
                PinState::Held => {}
            }
            polling.pause(self.identity_hold_reason(None));
        }
    }
}

struct HoldPoll<'a> {
    tmux: &'a str,
    last_warning: Option<Instant>,
}

impl<'a> HoldPoll<'a> {
    fn new(tmux: &'a str) -> Self {
        Self {
            tmux,
            last_warning: None,
        }
    }

    fn diagnose(&mut self, reason: Option<&'static str>, now: Instant) {
        if self
            .last_warning
            .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(60))
        {
            let reason = reason.unwrap_or_else(|| dedupe::codex_verified_hold_reason(self.tmux));
            tracing::warn!(
                tmux_session = self.tmux,
                hold_reason = reason,
                "Codex verified output is held; preserving its source and cursors"
            );
            self.last_warning = Some(now);
        }
    }

    fn pause(&mut self, reason: Option<&'static str>) {
        self.diagnose(reason, Instant::now());
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
#[cfg(unix)]
mod diagnostic_tests;
