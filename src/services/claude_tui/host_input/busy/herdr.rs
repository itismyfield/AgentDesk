//! The existing bound Herdr transaction, with no fallback to another host.
use super::{BusyObservation, BusyOutcome, BusyReport, BusyState};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::claude_tui::busy_inject::{self, Request, Screen, Veto};
use crate::services::claude_tui::composer_lock::DraftSighting;
use crate::services::claude_tui::host_input::{InputRun, run_herdr};
use crate::services::claude_tui::input::TuiInputAction;
use crate::services::claude_tui::owner_input;
use crate::services::claude_tui::source_verify::{
    self, Observation as SourceObservation, PinJudgment,
};
use crate::services::session_host::{HerdrMutation, HerdrTarget, HostKey, HostMutation};
use crate::services::tui_input::human_frame::history;
use crate::services::tui_prompt_dedupe::binding_events::SourceId;
use crate::services::tui_prompt_dedupe::{self, binding_events};
use std::sync::Arc;

pub(super) struct Adapter {
    pub(super) target: HerdrTarget,
    pub(super) writer: Option<String>,
    pub(super) observation: Option<BusyObservation>,
}

impl Adapter {
    pub(super) fn submit(&self, request: &Request<'_>) -> BusyReport {
        let Some(writer) = self.writer.as_deref() else {
            return BusyReport::refused("owner Herdr input without a writer".into());
        };
        if let Err(reason) = owner_input::bind_writer(writer) {
            return BusyReport::refused(reason);
        }
        let Some(capture) = self.target.capture_ansi(-80) else {
            return BusyReport::not_sent(
                Veto::PaneUnavailable,
                self.observation.as_ref().map(BusyObservation::probe),
            );
        };
        if busy_inject::modal(&capture) {
            if let Some(observation) = self.observation.as_ref() {
                return BusyReport::not_sent(Veto::Modal, Some(observation.probe()));
            }
            let target = self.target.clone();
            let probe = move || match target.capture_ansi(-80) {
                Some(capture) if !busy_inject::modal(&capture) => Screen::Ready,
                Some(_) => Screen::Blocked,
                None => Screen::Unknown,
            };
            return BusyReport::not_sent(Veto::Modal, Some(Box::new(probe)));
        }
        let Ok(offset) = std::fs::metadata(request.transcript).map(|meta| meta.len()) else {
            return BusyReport::not_sent(
                Veto::TranscriptUnavailable,
                self.observation.as_ref().map(BusyObservation::probe),
            );
        };
        let frame = busy_inject::frame(
            request.source,
            request.author,
            request.name,
            request.nonce,
            request.text,
        );
        let who = ("claude", request.session, request.nonce, request.author);
        if history::register(who, request.transcript, offset, &frame).is_err() {
            return BusyReport::not_sent(Veto::LoadFailed, None);
        }
        let withdrawn = || {
            history::withdraw(request.nonce, request.transcript);
            BusyReport::refused("owner_refused".into())
        };
        if owner_input::ready_payload().is_err() {
            return withdrawn();
        }
        let park = busy_inject::draft_sighting(&capture) == DraftSighting::PersonDraft;
        if let Some(observation) = self.observation.as_ref()
            && !observation.admits(request)
        {
            history::withdraw(request.nonce, request.transcript);
            owner_input::first_key_held("busy_source_unavailable");
            return observation.held();
        }
        if park {
            owner_input::keyed();
            let parked = (self.target.pin(HerdrMutation::Input).ok())
                .and_then(|()| self.target.send_keys(&[HostKey::CtrlS]).ok());
            if parked != Some(HostMutation::Confirmed) {
                return BusyReport::outcome(BusyOutcome::Failed("stash_failed"));
            }
        }
        if owner_input::start_payload().is_err() {
            return match park {
                true => BusyReport::outcome(BusyOutcome::Failed("stash_failed")),
                false => withdrawn(),
            };
        }
        if let Some(observation) = self.observation.as_ref()
            && !observation.admits(request)
        {
            if park {
                return BusyReport::outcome(BusyOutcome::Uncertain("source_changed"));
            }
            history::withdraw(request.nonce, request.transcript);
            owner_input::first_key_held("busy_source_unavailable");
            return observation.held();
        }
        owner_input::keyed();
        let plan = [
            TuiInputAction::PasteBuffer(frame.clone()),
            TuiInputAction::Enter,
        ];
        BusyReport::outcome(match run_herdr(&self.target, &plan, None) {
            InputRun::Applied => {
                receipt(request, frame, offset);
                BusyOutcome::Submitted
            }
            InputRun::Refused(_) if !park => {
                history::withdraw(request.nonce, request.transcript);
                owner_input::first_key_held("PaneUnavailable");
                return BusyReport::not_sent(
                    Veto::PaneUnavailable,
                    self.observation.as_ref().map(BusyObservation::probe),
                );
            }
            InputRun::Refused(_) => BusyOutcome::Failed("stash_failed"),
            InputRun::Indeterminate { confirmed: 0, .. } | InputRun::Cancelled { confirmed: 0 } => {
                BusyOutcome::Uncertain("paste_failed")
            }
            _ => BusyOutcome::Uncertain("enter_failed"),
        })
    }
}

fn receipt(request: &Request<'_>, text: String, offset: u64) {
    let (path, nonce) = (request.transcript.to_path_buf(), request.nonce.to_string());
    let scope = history::ReceiptScope::capture();
    let timing = &busy_inject::TIMING;
    let _ = std::thread::Builder::new()
        .name("claude-receipt".into())
        .spawn(move || {
            let deadline = std::time::Instant::now() + timing.confirm_window;
            let receipt = (path.as_path(), offset, nonce.as_str(), text.as_str());
            let wait = || busy_inject::await_receipt(receipt, deadline, timing.confirm_poll);
            let result = scope.enter(wait);
            tracing::debug!(%nonce, ?result, "Claude Herdr injection receipt diagnostic");
        });
}

#[derive(Clone)]
pub(super) struct Source {
    pub(super) id: SourceId,
    pub(super) offset: u64,
}

pub(super) struct Context {
    channel: u64,
    pub(super) logical: String,
    nonce: String,
    output_ready: fn(u64) -> bool,
    pub(super) target: HerdrTarget,
    pub(super) writer: String,
    pub(super) attempt_target: serde_json::Value,
}

pub(super) struct Reading {
    pub(super) state: BusyState,
    pub(super) source: Option<Source>,
    pub(super) idle_verified: bool,
}

impl Reading {
    fn idle() -> Self {
        Self {
            state: BusyState::Idle,
            source: None,
            idle_verified: true,
        }
    }

    fn held() -> Self {
        Self {
            state: BusyState::Held,
            source: None,
            idle_verified: false,
        }
    }
}

pub(crate) fn observe(
    (channel, logical, nonce): (u64, &str, &str),
    (target, writer, attempt_target): (HerdrTarget, String, serde_json::Value),
    output_ready: fn(u64) -> bool,
) -> BusyObservation {
    let context = Arc::new(Context {
        channel,
        logical: logical.into(),
        nonce: nonce.into(),
        output_ready,
        target,
        writer,
        attempt_target,
    });
    let read = context.read();
    BusyObservation {
        context: Some(context),
        state: read.state,
        source: read.source,
    }
}

impl Context {
    pub(super) fn read(&self) -> Reading {
        if !self.target.execution_alive() {
            return Reading::held();
        }
        if self.cleared() || self.idle_ready() {
            return Reading::idle();
        }
        if !(self.output_ready)(self.channel)
            || !matches!(self.target.capture_ansi(-80), Some(capture) if !busy_inject::modal(&capture))
        {
            return Reading::held();
        }
        crate::services::tmux_common::try_with_tmux_source_authority(&self.logical, |authority| {
            // A clear delegates before a new file exists; no pane socket is read under this lock.
            if self.cleared() {
                return Some(Reading::idle());
            }
            let source = self.current_source(authority)?;
            let state =
                crate::services::tui_turn_state::observe_claude_jsonl_turn_state(&source.id.path);
            if self
                .current_source(authority)
                .is_none_or(|again| again.id != source.id)
            {
                return None;
            }
            let state = match state {
                crate::services::tui_turn_state::TuiTurnState::Idle => BusyState::Idle,
                state if state.is_busy() => BusyState::Busy,
                _ => BusyState::Held,
            };
            Some(Reading {
                state,
                source: Some(source),
                idle_verified: false,
            })
        })
        .flatten()
        .unwrap_or_else(Reading::held)
    }

    // This only delegates; the Idle driver reserves and checks again before its first key.
    fn idle_ready(&self) -> bool {
        use crate::services::claude_tui::readiness;
        use crate::services::tui_prompt_dedupe::binding_context::{
            SpawnNonceMarker, observe_spawn_nonce_marker,
        };
        let current = || {
            self.target.execution_alive()
                && matches!(observe_spawn_nonce_marker(&self.logical),
                    SpawnNonceMarker::Known(nonce) if nonce == self.nonce)
        };
        if !current() {
            return false;
        }
        let Some(binding) = tui_prompt_dedupe::runtime_binding_for_tmux_session(&self.logical)
            .filter(|binding| binding.runtime_kind == RuntimeHandoffKind::ClaudeTui)
        else {
            return false;
        };
        let Some(proof) = readiness::observe(
            &self.logical,
            Some(std::path::Path::new(&binding.output_path)),
        ) else {
            return false;
        };
        let Some(reservation) = readiness::reserve(proof) else {
            return false;
        };
        let valid = reservation.current()
            && current()
            && tui_prompt_dedupe::runtime_binding_for_tmux_session(&self.logical).is_some_and(
                |present| {
                    present.runtime_kind == binding.runtime_kind
                        && present.output_path == binding.output_path
                },
            );
        reservation.not_sent();
        valid
    }

    fn cleared(&self) -> bool {
        crate::services::claude::herdr_turn::awaited_clear(self.channel, &self.logical, &self.nonce)
            .is_some()
    }

    fn current_source(
        &self,
        authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    ) -> Option<Source> {
        let (pin, history) =
            binding_events::claude_history_at(self.channel, &self.logical, Some(&self.nonce))
                .ok()?;
        let (pin, pinned_at) = pin?;
        let current = history
            .current
            .as_ref()
            .filter(|_| history.complete && history.awaiting.is_none())?;
        if current.pin.as_ref() != Some(&pin) || current.session != pin.session_id {
            return None;
        }
        let tui_prompt_dedupe::RuntimeObservation::Present(binding) =
            tui_prompt_dedupe::runtime_observation_under_source_authority(authority)
        else {
            return None;
        };
        if binding.runtime_kind != RuntimeHandoffKind::ClaudeTui
            || binding.session_id.as_deref() != Some(pin.session_id.as_str())
            || std::path::Path::new(&binding.output_path) != pin.path.as_path()
        {
            return None;
        }
        let opened = source_verify::observe_transcript(&pin.path);
        let checked = source_verify::judge_pin(
            Some(&pin),
            pinned_at,
            &pin.session_id,
            &pin.path,
            SourceObservation::Opened(opened.as_ref()),
        );
        if checked != PinJudgment::Verified(pin.clone()) {
            return None;
        }
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(&pin.path).ok()?;
        (meta.dev() == pin.dev && meta.ino() == pin.ino).then_some(Source {
            id: pin,
            offset: meta.len(),
        })
    }
}

#[cfg(test)]
#[path = "herdr_tests.rs"]
mod observation_tests;
