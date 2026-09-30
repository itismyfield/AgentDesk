//! The Claude source check a hook's continuation candidate passes before the pane follows it.
//! The candidate is the payload's own transcript; its first record and file identity decide.

use std::io;
use std::path::Path;

use super::*;
use crate::services::claude_tui::source_verify::{
    self, ClaudeHookSource, ClaudeSource, OpenedTranscript, SourceRejection, SourceVerdict,
};
use binding_events::SourceId;

/// What a registration writes to the binding event log before it publishes the binding.
#[derive(Clone, Debug)]
pub(crate) enum Record {
    /// The file on disk decides between Source and Pending.
    Stat,
    /// A source the Claude check verified; recorded as this identity if the path still names it.
    Verified(SourceId),
    /// A restored exact path whose transcript is not verified yet: published, nothing logged.
    AwaitFirstRecord,
}

/// What a registration's record left in the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Persisted {
    /// The log names the published source: a stat's record or the verified identity.
    Logged,
    /// Nothing verified was logged; the pane waits on its exact path and the next pass checks again.
    AwaitingExact,
}

impl Record {
    pub(crate) fn persist(&self, proposal: &Proposal) -> io::Result<Persisted> {
        match self {
            Self::Stat => binding_events::record_source(proposal).map(|()| Persisted::Logged),
            // A file replaced since the check is left unlogged for the next check to judge.
            Self::Verified(source) if binding_events::codex::source_file_matches(source) => {
                binding_events::record_verified(proposal, source).map(|()| Persisted::Logged)
            }
            Self::Verified(_) | Self::AwaitFirstRecord => Ok(Persisted::AwaitingExact),
        }
    }

    /// What publishing without a log leaves: only a stat has nothing further to wait for.
    pub(crate) fn unlogged(&self) -> Persisted {
        match self {
            Self::Stat => Persisted::Logged,
            Self::Verified(_) | Self::AwaitFirstRecord => Persisted::AwaitingExact,
        }
    }
}

/// What the check decided for a candidate.
enum Checked {
    /// The bound source itself, already pinned or its file momentarily unreadable.
    Bound,
    /// A verified source: the bound one on its first check, a corrected path, or a new session.
    Verified(SourceId),
    /// Named correctly but without a verified transcript yet; it waits as a Pending.
    Waiting,
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
        let source = match self.check() {
            Err(error) => {
                tracing::warn!(tmux, %error, "binding event log unreadable; hook retried");
                return false;
            }
            Ok(Checked::Bound) => {
                *skip = unlogged;
                return true;
            }
            Ok(Checked::Verified(source)) => source,
            Ok(Checked::Waiting) => {
                *skip = unlogged;
                return wait(failure);
            }
            Ok(Checked::Refused(refusal, reason)) => {
                *skip = Some(refusal);
                if reject(reason) {
                    tracing::warn!(
                        tmux,
                        payload,
                        candidate,
                        reason,
                        "Claude hook source refused"
                    );
                }
                return false;
            }
        };
        *skip = unlogged;
        if let Some(current) = self.bound.session_id.as_deref()
            && current != self.command_session_id
            && current != payload
        {
            *skip = Some(AdoptSkip::MtimeUnreadable);
            let Some(current_mtime) =
                super::super::pending::bound_transcript_mtime(tmux, self.bound)
            else {
                return false;
            };
            let candidate_mtime = std::fs::metadata(candidate).and_then(|m| m.modified());
            let Ok(candidate_mtime) = candidate_mtime else {
                return false;
            };
            *skip = unlogged;
            if current_mtime.is_some_and(|current| candidate_mtime <= current) {
                *skip = Some(AdoptSkip::OlderThanBound);
                reject("older_than_bound_transcript");
                return false;
            }
        }
        #[cfg(test)]
        after_check();
        // The path must still name the file the check read; a replaced one waits for the next check,
        // the bound source too, which keeps its binding and cursor meanwhile.
        if !binding_events::codex::source_file_matches(&source) {
            return wait(failure);
        }
        let recorded = proposal.map(|p| binding_events::record_verified(p, &source));
        if let Some(Err(error)) = recorded {
            tracing::error!(
                tmux,
                payload,
                %error,
                "binding event log append failed; Claude continuation not adopted"
            );
            *failure = Some(persist_error(error));
            return false;
        }
        true
    }

    /// Judges the candidate against the pane's bound source; `Err` when its log cannot be loaded.
    fn check(&self) -> io::Result<Checked> {
        let (bound, payload) = (self.bound, self.payload_session_id);
        let candidate = self.hook.transcript_path.as_deref().unwrap_or_default();
        let bound_session = bound.session_id.clone().unwrap_or_default();
        let file = match &self.proposal {
            Some(p) => binding_events::pinned_file(
                p.channel_id,
                p.tmux_session,
                &bound_session,
                &bound.output_path,
            )?,
            None => None,
        };
        let history = [ClaudeSource {
            session_id: bound_session,
            path: bound.output_path.clone().into(),
            file,
        }];
        let source = ClaudeHookSource::from_signal(payload, self.hook, candidate.into());
        let same_bound = history[0].session_id == payload && candidate == bound.output_path;
        let verdict = match self.opened {
            Ok(opened) => source_verify::verify_claude_source(&source, self.root, opened, &history),
            Err(error) => {
                tracing::warn!(candidate, %error, "Claude transcript unreadable; the candidate waits");
                match source_verify::precheck(&source, self.root) {
                    Some(rejection) => SourceVerdict::Rejected(rejection),
                    None if same_bound => SourceVerdict::Current,
                    None => SourceVerdict::Pending,
                }
            }
        };
        Ok(match verdict {
            SourceVerdict::Current => Checked::Bound,
            SourceVerdict::Confirm(source) | SourceVerdict::Rotate(source) => {
                match source.source_id() {
                    Some(id) => Checked::Verified(id),
                    None => refused(SourceRejection::IdentityUnavailable),
                }
            }
            SourceVerdict::Pending => Checked::Waiting,
            // A one-entry history names no left session, so a conflict is refused like a regression.
            SourceVerdict::PendingConflict => refused(SourceRejection::Regression),
            SourceVerdict::Rejected(rejection) => refused(rejection),
            SourceVerdict::Anomaly => Checked::Refused(AdoptSkip::SourceAnomaly, "source_anomaly"),
        })
    }
}

fn refused(rejection: SourceRejection) -> Checked {
    let reason = match rejection {
        SourceRejection::InvalidSessionId => "invalid_session_id",
        SourceRejection::NotTopLevelTranscript => "not_top_level_transcript",
        SourceRejection::FirstRecordMismatch => "first_record_mismatch",
        SourceRejection::IdentityUnavailable => "identity_unavailable",
        SourceRejection::Regression => "regression",
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
