//! The Claude source check a hook's continuation candidate passes before the pane follows it.
//! The candidate is the payload's own transcript; its first record and file identity decide, and the
//! pane's history in the binding log decides whether a session it left may come back.

use std::io;
use std::path::Path;

use super::*;
use crate::services::claude_tui::source_verify::{
    self, ClaudeHookSource, OpenedTranscript, SourceHistory, SourceRejection, SourceVerdict,
};
use crate::services::tui_prompt_dedupe::binding_context::{
    SpawnNonceMarker, observe_spawn_nonce_marker,
};
use binding_events::{Committed, SourceId};

/// What a registration writes to the binding event log before it publishes the binding.
#[derive(Clone, Debug)]
pub(crate) enum Record {
    /// The file on disk decides between Source and Pending.
    Stat,
    /// A source the Claude check verified; recorded as this identity if the path still names it.
    Verified(SourceId),
    /// A restored exact path whose transcript is not verified yet: published, nothing logged.
    AwaitFirstRecord,
    /// A logged source the path must still name; anything else publishes nothing.
    Exact(SourceId),
}

/// What a registration's record left in the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Persisted {
    /// The log names the published source: a stat's record or the verified identity.
    Logged,
    /// Nothing verified was logged; the pane waits on its exact path and the next pass checks again.
    AwaitingExact,
    /// The pinned file was replaced or is gone: nothing was logged or published.
    Anomaly,
    /// The pinned file could not be looked at: nothing was logged or published, judged again later.
    Recheck,
}

impl Persisted {
    /// Whether the registration may publish its binding.
    pub(crate) fn published(self) -> bool {
        matches!(self, Self::Logged | Self::AwaitingExact)
    }
}

impl Record {
    pub(crate) fn persist(&self, proposal: &Proposal) -> io::Result<Persisted> {
        let persisted = |committed| match committed {
            Committed::Appended | Committed::Unchanged => Persisted::Logged,
            Committed::Stale => Persisted::AwaitingExact,
            Committed::Anomaly => Persisted::Anomaly,
            Committed::Recheck => Persisted::Recheck,
        };
        match self {
            Self::Stat => binding_events::record_source(proposal).map(persisted),
            Self::Verified(source) if binding_events::codex::source_file_matches(source) => {
                binding_events::record_verified(proposal, source).map(persisted)
            }
            // A file replaced since the check is judged against the pin, or left for the next check.
            Self::Verified(_) => binding_events::judge_moved(proposal).map(persisted),
            Self::AwaitFirstRecord => Ok(Persisted::AwaitingExact),
            Self::Exact(source) if binding_events::codex::source_file_matches(source) => {
                binding_events::record_verified(proposal, source).map(|committed| match committed {
                    Committed::Appended | Committed::Unchanged => Persisted::Logged,
                    Committed::Anomaly => Persisted::Anomaly,
                    Committed::Stale | Committed::Recheck => Persisted::Recheck,
                })
            }
            Self::Exact(_) => Ok(Persisted::Recheck),
        }
    }

    /// What publishing without a log leaves: only a stat has nothing further to wait for.
    pub(crate) fn unlogged(&self) -> Persisted {
        match self {
            Self::Stat => Persisted::Logged,
            Self::Verified(_) | Self::AwaitFirstRecord => Persisted::AwaitingExact,
            Self::Exact(_) => Persisted::Recheck,
        }
    }
}

/// What the check decided for a candidate.
enum Checked {
    /// The bound source itself, its pinned file read again; its pin when it has one.
    Bound(Option<SourceId>),
    /// A verified source: the bound one on its first check, a corrected path, or a new session.
    Verified(SourceId),
    /// Named correctly but without a verified transcript yet; it waits as a Pending.
    Waiting,
    /// The bound source's pinned file could not be read; the hook is refused until it can be.
    Recheck,
    Refused(AdoptSkip, &'static str),
}

/// A hook's continuation candidate on one pane; `hook` names the payload path spelled under `root`.
pub(super) struct Candidate<'a> {
    pub proposal: Option<Proposal<'a>>,
    pub tmux_session: &'a str,
    pub bound: &'a TuiRuntimeBinding,
    pub root: &'a Path,
    pub command_session_id: &'a str,
    pub payload_session_id: &'a str,
    pub hook: &'a HookSignal,
    pub opened: &'a io::Result<OpenedTranscript>,
}

impl Candidate<'_> {
    /// Checks the candidate and logs the outcome; `true` when the binding may follow it.
    /// `skip` and `failure` are set as `adopt_continuation` documents.
    pub(super) fn judge(
        &self,
        failure: &mut Option<BindingPersistError>,
        skip: &mut Option<AdoptSkip>,
    ) -> bool {
        let (tmux, payload, proposal) = (
            self.tmux_session,
            self.payload_session_id,
            self.proposal.as_ref(),
        );
        let candidate = self.hook.transcript_path.as_deref().unwrap_or_default();
        let unlogged = AdoptSkip::unlogged(proposal.is_none(), tmux, candidate);
        let persist_error = |error| BindingPersistError {
            tmux_session: tmux.to_owned(),
            error,
        };
        let reject = |reason: &str| match proposal
            .map_or(Ok(true), |p| binding_events::record_rejected(p, reason))
        {
            Err(error) => {
                tracing::warn!(tmux, kind = "rejected", %error, "binding event audit record not persisted");
                false
            }
            Ok(logged) => logged,
        };
        // The candidate stays Pending in the log until it is verified; that record is the hook's ACK.
        let wait = |failure: &mut Option<BindingPersistError>| {
            *failure = proposal
                .map(binding_events::record_pending)
                .and_then(Result::err)
                .map(persist_error);
            false
        };
        *skip = unlogged;
        if unlogged == Some(AdoptSkip::ChannelNotRestored) {
            return false;
        }
        *skip = Some(AdoptSkip::HistoryUnreadable);
        let (checked, reclaimable) = match self.check() {
            Err(error) => {
                tracing::warn!(tmux, %error, "binding event log unreadable; hook retried");
                return false;
            }
            Ok(judged) => judged,
        };
        let source = match checked {
            Checked::Bound(pin) => {
                *skip = unlogged;
                #[cfg(test)]
                let reclaimable = reclaimable && !source_verify::n2b_mutant("r5-reclaim-bound-off");
                if let (true, Some(pin), Some(p)) = (reclaimable, pin, proposal) {
                    self.log_reclaim(p, &pin);
                }
                return true;
            }
            Checked::Verified(source) => source,
            Checked::Waiting => {
                *skip = unlogged;
                return wait(failure);
            }
            Checked::Recheck => {
                *skip = Some(AdoptSkip::SourceUnreadable);
                return false;
            }
            Checked::Refused(refusal, reason) => {
                *skip = Some(refusal);
                let published_at = self.hook.published_at.map(|t| t.to_rfc3339());
                match (reject(reason), refusal) {
                    (true, AdoptSkip::ResumeConflict) => tracing::error!(
                        tmux,
                        payload,
                        candidate,
                        ?published_at,
                        source_unresolved = true,
                        "Claude return to a left session not proven; nothing adopted"
                    ),
                    (true, _) => tracing::warn!(
                        tmux,
                        payload,
                        candidate,
                        reason,
                        "Claude hook source refused"
                    ),
                    (false, _) => {}
                }
                return false;
            }
        };
        *skip = unlogged;
        #[cfg(test)]
        if source_verify::n2b_mutant("mtime") && self.older_than_bound(candidate) {
            *skip = Some(AdoptSkip::SourceRejected(SourceRejection::Regression));
            reject("older_than_bound_transcript");
            return false;
        }
        #[cfg(test)]
        after_check();
        // The path must still name the file the check read; a replaced one waits for the next check,
        // the bound source too, which keeps its binding and cursor meanwhile.
        let recorded = match (
            proposal,
            binding_events::codex::source_file_matches(&source),
        ) {
            (None, true) => None,
            (None, false) => return wait(failure),
            (Some(p), true) => Some(binding_events::record_verified(p, &source)),
            (Some(p), false) => Some(binding_events::judge_moved(p)),
        };
        match recorded {
            Some(Err(error)) => {
                tracing::error!(
                    tmux,
                    payload,
                    %error,
                    "binding event log append failed; Claude continuation not adopted"
                );
                *failure = Some(persist_error(error));
                false
            }
            // The log's pin refused the identity: the pane stays on its pinned file.
            Some(Ok(Committed::Anomaly)) => {
                *skip = Some(AdoptSkip::SourceAnomaly);
                false
            }
            Some(Ok(Committed::Recheck)) => {
                *skip = Some(AdoptSkip::SourceUnreadable);
                false
            }
            Some(Ok(Committed::Stale)) => wait(failure),
            _ => true,
        }
    }

    /// Logs the bound source again only while the prompt outlived the pane's waiting Pending; a
    /// failure leaves that Pending waiting as before.
    fn log_reclaim(&self, proposal: &Proposal, source: &SourceId) -> bool {
        match binding_events::record_reclaim(proposal, source) {
            Ok(appended) => appended,
            Err(error) => {
                tracing::warn!(tmux = self.tmux_session, %error, "Claude prompt reclaim not logged");
                false
            }
        }
    }

    /// Supersedes the pane's waiting Pending when this prompt of the bound session outlived it;
    /// nothing else is recorded and the binding is not touched.
    pub(super) fn reclaim(&self) -> bool {
        let (Some(proposal), Ok((checked, true))) = (self.proposal.as_ref(), self.check()) else {
            return false;
        };
        match checked {
            Checked::Bound(Some(source)) | Checked::Verified(source) => {
                self.log_reclaim(proposal, &source)
            }
            _ => false,
        }
    }

    /// A prompt on a complete history whose waiting Pending names another session may reclaim.
    fn reclaimable(&self, history: &SourceHistory) -> bool {
        use crate::services::claude_tui::hook_server::HookEventKind;
        let prompt = HookEventKind::from_path(&self.hook.event) == HookEventKind::UserPromptSubmit;
        #[cfg(test)]
        let prompt = prompt || source_verify::n2b_mutant("r5-reclaim-any-event");
        let complete = history.complete;
        #[cfg(test)]
        let complete = complete || source_verify::n2b_mutant("r5-reclaim-incomplete");
        let waiting =
            (history.awaiting.as_ref()).is_some_and(|w| w.session != self.payload_session_id);
        prompt && complete && waiting
    }

    /// Judges the candidate against the pane's bound source and the history its log holds for this
    /// execution, and whether it may reclaim; `Err` when the log cannot be loaded. A pane without a
    /// channel has no history.
    fn check(&self) -> io::Result<(Checked, bool)> {
        let (bound, payload) = (self.bound, self.payload_session_id);
        let candidate = self.hook.transcript_path.as_deref().unwrap_or_default();
        let bound_session = bound.session_id.clone().unwrap_or_default();
        let nonce = match observe_spawn_nonce_marker(self.tmux_session) {
            SpawnNonceMarker::Known(nonce) => Some(nonce),
            _ => None,
        };
        let (pin, history) = match &self.proposal {
            Some(p) => {
                binding_events::claude_history(p.channel_id, p.tmux_session, nonce.as_deref())?
            }
            None => (None, SourceHistory::default()),
        };
        #[cfg(test)]
        let (pin, history) = n2b_seams::checked((pin, history));
        let pin = pin.filter(|pin| {
            pin.session_id == bound_session && pin.path == Path::new(&bound.output_path)
        });
        let source = ClaudeHookSource::from_signal(payload, self.hook, candidate.into());
        if let Err(error) = self.opened {
            tracing::warn!(candidate, %error, "Claude transcript unreadable; judged against its pin");
        }
        let opened = self.opened.as_ref();
        let verdict = source_verify::verify_claude_source(
            &source,
            self.root,
            opened,
            &bound_session,
            pin.as_ref(),
            &history,
        );
        let reclaimable = self.reclaimable(&history);
        let checked = match verdict {
            SourceVerdict::Current => Checked::Bound(pin),
            SourceVerdict::Confirm(source) | SourceVerdict::Rotate(source) => {
                match source.source_id() {
                    Some(id) => Checked::Verified(id),
                    None => refused(SourceRejection::IdentityUnavailable),
                }
            }
            SourceVerdict::Pending => Checked::Waiting,
            SourceVerdict::Recheck => Checked::Recheck,
            SourceVerdict::PendingConflict => {
                Checked::Refused(AdoptSkip::ResumeConflict, "resume_conflict")
            }
            SourceVerdict::Rejected(rejection) => refused(rejection),
            SourceVerdict::Anomaly => Checked::Refused(AdoptSkip::SourceAnomaly, "source_anomaly"),
        };
        Ok((checked, reclaimable))
    }
}

/// Lets a prompt of the session a pane is bound to, named by the pane's own launch command,
/// supersede the Pending it outlived; the binding and its hook routing stay as they are.
pub(crate) fn reclaim_with_current_prompt(session_id: &str, hook: &HookSignal) -> bool {
    #[cfg(test)]
    if source_verify::n2b_mutant("r5-reclaim-inner-off") {
        return false;
    }
    let session_id = session_id.trim();
    let payload_path = hook.transcript_path.as_deref().map(str::trim);
    let Some(payload_path) = payload_path
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
    else {
        return false;
    };
    let opened = source_verify::observe_transcript(&payload_path);
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let mut skip = None;
    let key = PromptKey::new("claude", session_id);
    let Some((tmux, binding)) = AdoptSkip::bound_pane(&state, &key, &mut skip) else {
        return false;
    };
    let bound = binding.value.clone();
    let claude = bound.runtime_kind == RuntimeHandoffKind::ClaudeTui;
    let output = PathBuf::from(&bound.output_path);
    let (true, Some(true), Some(root)) = (
        claude,
        bound.session_id.as_deref().map(|bound| bound == session_id),
        output.parent().and_then(Path::parent),
    ) else {
        return false;
    };
    let candidate = source_verify::normalize_payload_path(root, &payload_path);
    let candidate = candidate.display().to_string();
    let hook = &HookSignal {
        transcript_path: Some(candidate.clone()),
        ..hook.clone()
    };
    let channel_id = state.channel_by_tmux.get(&tmux).map(|e| e.value);
    let Some(channel_id) = channel_id.filter(|id| *id != 0) else {
        return false;
    };
    let proposal = Proposal {
        channel_id,
        provider: "claude",
        tmux_session: &tmux,
        session_id: Some(session_id),
        path: &candidate,
        replaced: Some((&bound.output_path, bound.session_id.as_deref())),
        cause: CauseSource::Hook(hook.cause()),
        hook: Some(hook),
    };
    let candidate = Candidate {
        proposal: Some(proposal),
        tmux_session: &tmux,
        bound: &bound,
        root,
        command_session_id: session_id,
        payload_session_id: session_id,
        hook,
        opened: &opened,
    };
    candidate.reclaim()
}

fn refused(rejection: SourceRejection) -> Checked {
    let reason = match rejection {
        SourceRejection::InvalidSessionId => "invalid_session_id",
        SourceRejection::NotTopLevelTranscript => "not_top_level_transcript",
        SourceRejection::FirstRecordMismatch => "first_record_mismatch",
        SourceRejection::IdentityUnavailable => "identity_unavailable",
        SourceRejection::Regression => "regression",
        SourceRejection::UnprovenStart => "unproven_start",
    };
    Checked::Refused(AdoptSkip::SourceRejected(rejection), reason)
}

#[cfg(test)]
thread_local! {
    /// Runs once between a check, a hook's or a restore's, and the record it leads to.
    pub(crate) static AFTER_CHECK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn after_check() {
    if let Some(seam) = AFTER_CHECK.with_borrow_mut(Option::take) {
        seam();
    }
}

#[cfg(test)]
pub(crate) use n2b_seams::{BEFORE_AUTHORITY, before_authority};

#[cfg(test)]
mod n2b_seams {
    use super::*;

    thread_local! {
        /// Runs once before a hook takes its pane's authority.
        pub(crate) static BEFORE_AUTHORITY: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
        static STALE_HISTORY: std::cell::RefCell<Option<(Option<SourceId>, SourceHistory)>> = const { std::cell::RefCell::new(None) };
    }

    /// Before a hook takes its pane's authority: the mutation reads the history here and keeps
    /// that copy for this hook's check, past any hook the seam lands meanwhile.
    pub(crate) fn before_authority(command_session_id: &str) {
        let mut stale = None;
        if source_verify::n2b_mutant("auth") {
            let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
            let key = PromptKey::new("claude", command_session_id);
            let tmux = state
                .tmux_by_provider_session
                .get(&key)
                .map(|e| e.value.clone());
            let channel = tmux.as_ref().and_then(|t| state.channel_by_tmux.get(t));
            let channel = channel.map(|e| e.value);
            drop(state);
            if let (Some(tmux), Some(channel)) = (tmux, channel) {
                let nonce = match observe_spawn_nonce_marker(&tmux) {
                    SpawnNonceMarker::Known(nonce) => Some(nonce),
                    _ => None,
                };
                stale = binding_events::claude_history(channel, &tmux, nonce.as_deref()).ok();
            }
        }
        if let Some(seam) = BEFORE_AUTHORITY.with_borrow_mut(Option::take) {
            seam();
        }
        STALE_HISTORY.set(stale);
    }

    /// The history a mutation read before the authority, or what the check read under it.
    pub(super) fn checked(
        read: (Option<SourceId>, SourceHistory),
    ) -> (Option<SourceId>, SourceHistory) {
        match STALE_HISTORY.with_borrow_mut(Option::take) {
            Some(stale) => stale,
            None if source_verify::n2b_mutant("hist") => (read.0, SourceHistory::default()),
            None => read,
        }
    }

    impl Candidate<'_> {
        /// The newer-transcript rule the log history replaced, for the mutation that brings it back.
        pub(super) fn older_than_bound(&self, candidate: &str) -> bool {
            let current = self.bound.session_id.as_deref();
            let mtime = |path: &str| std::fs::metadata(path).and_then(|m| m.modified()).ok();
            current.is_some_and(|c| c != self.command_session_id && c != self.payload_session_id)
                && mtime(candidate) <= mtime(&self.bound.output_path)
        }
    }
}
