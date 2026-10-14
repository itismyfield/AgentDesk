//! A turn-local receipt owns a finite native range; availability never changes its lifecycle.

use super::CodexEpisodeSpan;
use super::decisions::{EvidenceReason as E, LiveReadDecision, LiveReadReason};
use super::dormant::construction::ActualCodexSubmission;
use crate::services::codex_tui::rollout_tail::provenance::{NativeTurnAnchor, terminal_end};
use crate::services::provider::CancelToken;
use crate::services::tui_o::shadow::capture::SourceCapture;
use crate::services::tui_o::shadow::{CaptureOutcome, CaptureSource, MAX_READ_BYTES};
use crate::services::tui_o::store::codex_provenance::submit_floor::NativeFloor;
use std::{
    ops::Range,
    sync::{Arc, atomic::Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lifecycle {
    Open,
    Closed { final_frontier: u64 },
    Cancelled,
    Denied,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Availability {
    Available,
    Unavailable(LiveReadReason),
}

#[derive(Debug)]
pub(crate) struct GrantedRange<'a> {
    grant: &'a Arc<()>,
    span: &'a CodexEpisodeSpan,
    floor: &'a NativeFloor,
    range: Range<u64>,
}

impl GrantedRange<'_> {
    pub(crate) fn identity(&self) -> &Arc<()> {
        self.grant
    }
    pub(crate) fn span(&self) -> &CodexEpisodeSpan {
        self.span
    }
    pub(crate) fn floor(&self) -> &NativeFloor {
        self.floor
    }
    pub(crate) fn bytes(&self) -> &Range<u64> {
        &self.range
    }
}

pub(crate) struct LiveGrant {
    id: Arc<()>,
    span: CodexEpisodeSpan,
    floor: NativeFloor,
    token: Arc<CancelToken>,
    frontier: u64,
    prefix_hash: String,
    lifecycle: Lifecycle,
    availability: Availability,
}

impl LiveGrant {
    pub(super) fn from_receipt(
        receipt: ActualCodexSubmission,
        span: CodexEpisodeSpan,
        floor: NativeFloor,
        through: u64,
    ) -> Result<Self, LiveReadReason> {
        let (_, frontier, hash) = floor.checkpoint();
        let mut grant = Self {
            id: Arc::new(()),
            span,
            prefix_hash: hash.to_owned(),
            floor,
            token: receipt.into_token(),
            frontier,
            lifecycle: Lifecycle::Open,
            availability: Availability::Available,
        };
        grant.advance(frontier..through)?;
        Ok(grant)
    }

    pub(crate) fn frontier(&self) -> u64 {
        self.frontier
    }
    pub(crate) fn lifecycle(&self) -> Lifecycle {
        self.lifecycle
    }
    pub(crate) fn availability(&self) -> Availability {
        self.availability
    }

    fn active(&self) -> Result<(), LiveReadReason> {
        if self.token.cancelled.load(Ordering::Acquire) || self.lifecycle == Lifecycle::Cancelled {
            return Err(E::Cancelled.into());
        }
        if self.lifecycle == Lifecycle::Denied {
            return Err(E::Denied.into());
        }
        if let Availability::Unavailable(reason) = self.availability {
            return Err(reason);
        }
        Ok(())
    }

    fn capture(&self, through: u64) -> Result<SourceCapture, LiveReadReason> {
        let (source, _, _) = self.floor.checkpoint();
        let mut capture = SourceCapture::reopen(source.clone(), self.frontier, &self.prefix_hash)
            .map_err(|error| {
            LiveReadReason::from(if error.kind() == std::io::ErrorKind::InvalidData {
                E::PrefixChanged
            } else {
                E::SourceChanged
            })
        })?;
        if !capture
            .verify_prefix()
            .map_err(|_| LiveReadReason::from(E::SourceChanged))?
            || matches!(capture.poll(0), CaptureOutcome::Anomaly(_))
        {
            return Err(E::PrefixChanged.into());
        }
        while capture.read_through() < through {
            let before = capture.read_through();
            if matches!(
                capture.poll((through - before).min(MAX_READ_BYTES)),
                CaptureOutcome::Anomaly(_)
            ) || capture.read_through() == before
            {
                return Err(E::SourceChanged.into());
            }
        }
        if capture.captured_through() != through {
            return Err(E::IncompleteRecord.into());
        }
        if !capture
            .verify_prefix()
            .map_err(|_| LiveReadReason::from(E::SourceChanged))?
            || matches!(capture.poll(0), CaptureOutcome::Anomaly(_))
        {
            return Err(E::PrefixChanged.into());
        }
        Ok(capture)
    }

    pub(crate) fn advance(&mut self, range: Range<u64>) -> Result<(), LiveReadReason> {
        self.active()?;
        if range.start != self.frontier || range.end <= range.start {
            return Err(E::RangeGap.into());
        }
        if let Lifecycle::Closed { .. } = self.lifecycle {
            return Err(E::FiniteCloseExceeded.into());
        }
        let capture = self.capture(range.end)?;
        let anchor = NativeTurnAnchor {
            native_turn_id: self.span.episode.native_turn_id.clone(),
            start: self.span.start,
        };
        let terminal = terminal_end(&self.span.execution.source, &anchor, range.end)
            .map_err(|_| LiveReadReason::from(E::ForeignTurn))?;
        if terminal.is_some_and(|end| end != range.end) {
            return Err(E::FiniteCloseExceeded.into());
        }
        self.active()?;
        self.frontier = range.end;
        self.prefix_hash = capture.prefix_hash();
        if let Some(final_frontier) = terminal {
            self.span.end = Some(final_frontier);
            self.lifecycle = Lifecycle::Closed { final_frontier };
        }
        Ok(())
    }

    pub(crate) fn close(&mut self, through: u64) -> Result<(), LiveReadReason> {
        self.active()?;
        if let Lifecycle::Closed { final_frontier } = self.lifecycle {
            if through != final_frontier {
                return Err(E::FiniteCloseExceeded.into());
            }
            self.capture(through)?;
            return Ok(());
        }
        let anchor = NativeTurnAnchor {
            native_turn_id: self.span.episode.native_turn_id.clone(),
            start: self.span.start,
        };
        let terminal = terminal_end(&self.span.execution.source, &anchor, through)
            .map_err(|_| LiveReadReason::from(E::ForeignTurn))?
            .ok_or(LiveReadReason::from(E::MissingEvidence))?;
        if terminal != through {
            return Err(E::FiniteCloseExceeded.into());
        }
        self.advance(self.frontier..through)
    }

    pub(crate) fn range(&self, range: Range<u64>) -> LiveReadDecision<GrantedRange<'_>> {
        let check = || -> Result<(), LiveReadReason> {
            self.active()?;
            if range.start < self.span.start
                || range.start >= range.end
                || range.end > self.frontier
            {
                return Err(E::FiniteCloseExceeded.into());
            }
            self.capture(self.frontier)?;
            Ok(())
        };
        match check() {
            Ok(()) => LiveReadDecision::Granted(GrantedRange {
                grant: &self.id,
                span: &self.span,
                floor: &self.floor,
                range,
            }),
            Err(reason) => LiveReadDecision::Held(reason),
        }
    }

    pub(crate) fn unavailable(&mut self, reason: LiveReadReason) {
        self.availability = Availability::Unavailable(reason);
    }
    pub(crate) fn restore(&mut self) -> Result<(), LiveReadReason> {
        self.capture(self.frontier)?;
        self.availability = Availability::Available;
        self.active()
    }
    pub(crate) fn cancel(&mut self) {
        self.lifecycle = Lifecycle::Cancelled;
    }
    pub(crate) fn deny(&mut self) {
        self.lifecycle = Lifecycle::Denied;
    }
}

#[cfg(all(test, unix))]
#[path = "live_grant_tests.rs"]
mod tests;
