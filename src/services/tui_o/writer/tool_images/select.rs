//! The next step of each image and omission notice the ledger owes, one per poll. A marker places
//! the first attempt in the owed order; every later step bypasses the ordinary disposition.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, Utc};

use super::{ImageBody, OmitReason, notice};
use crate::services::tui_o::shadow::{ShadowProvider, SourceId, UnitKey};
use crate::services::tui_o::store::ChannelStore;
use crate::services::tui_o::store::ledger::tool_images::{
    ImageEntry, ImageObligation, ImageStep, NoticeStep, OpenObligation, Waiting,
};
use crate::services::tui_o::store::ledger::{LedgerEntry, LedgerState, PieceDisposition};
use crate::services::tui_o::store::spool::{SpoolFrame, source_key};
use crate::services::tui_o::writer::deliver::{Admission, ChannelWriter, Step};
use crate::services::tui_o::writer::local_retry::LOCAL_RETRY_DELAY;
use crate::services::tui_o::writer::pieces::{PieceWork, UnitDeriver};
use crate::services::tui_o::writer::{
    AlarmSink, DeliveryLease, DiscordPort, PostOutcome, WriterAlarm,
};

/// What one poll does for the obligations; `None` when each waits or belongs to its marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Retry(UnitKey),
    Omit(UnitKey),
    Notice(UnitKey),
    Reconfirm(u64),
}

/// The first due retry, omission, notice or re-settlement; Ambiguous, Rejected, Prepared and an
/// exhausted notice stay open as they are.
pub(crate) fn next_action(ledger: &LedgerState, now: DateTime<Utc>) -> Option<Action> {
    let images = ledger.tool_images();
    images
        .open_obligations(ledger)
        .into_iter()
        .find_map(|open| match open {
            // A key without any attempt is its marker's; one whose latest is NotSent retries here.
            OpenObligation::Image {
                key,
                step: ImageStep::Attempt,
            } => (ledger.latest_piece(&key, 0).is_some()).then_some(Action::Retry(key)),
            OpenObligation::Image {
                key,
                step: ImageStep::RetryAfterNotFound { serial },
            } => retry_due(ledger, serial, now).then_some(Action::Retry(key)),
            OpenObligation::Image {
                key,
                step: ImageStep::Omit { serial },
            } => {
                tracing::info!(serial, unit_key = ?key, "[tui_o] image unconfirmed twice; omitted");
                Some(Action::Omit(key))
            }
            OpenObligation::Notice {
                key,
                step: NoticeStep::Attempt,
            } => Some(Action::Notice(key)),
            OpenObligation::Notice {
                key,
                step: NoticeStep::RetryAfterNotFound { serial },
            } => retry_due(ledger, serial, now).then_some(Action::Notice(key)),
            OpenObligation::Image {
                step: ImageStep::Waiting(Waiting::Unresolved { serial }),
                ..
            }
            | OpenObligation::Notice {
                step: NoticeStep::Waiting(Waiting::Unresolved { serial }),
                ..
            } => reconfirm_due(ledger, serial, now).then_some(Action::Reconfirm(serial)),
            OpenObligation::Image {
                key,
                step: ImageStep::Waiting(waiting),
            }
            | OpenObligation::Notice {
                key,
                step: NoticeStep::Waiting(waiting),
            } => {
                held(&key, &waiting);
                None
            }
            OpenObligation::Notice {
                key,
                step: NoticeStep::Exhausted { serial },
            } => {
                tracing::debug!(serial, unit_key = ?key, "[tui_o] omission notice kept open");
                None
            }
        })
}

/// Prepared settles in the ordinary path; Ambiguous and Rejected stay open under their alarms.
fn held(key: &UnitKey, waiting: &Waiting) {
    let (serial, status) = match waiting {
        Waiting::Prepared { serial }
        | Waiting::Unresolved { serial }
        | Waiting::Ambiguous { serial } => (*serial, None),
        Waiting::Rejected { serial, status } => (*serial, Some(*status)),
    };
    tracing::debug!(serial, ?status, unit_key = ?key, "[tui_o] image obligation waits");
}

fn retry_due(ledger: &LedgerState, serial: u64, now: DateTime<Utc>) -> bool {
    let delay = chrono::Duration::from_std(LOCAL_RETRY_DELAY).unwrap_or_default();
    let at = ledger.piece(serial).and_then(|piece| piece.outcome_at);
    at.is_some_and(|at| at + delay <= now)
}

/// Re-settles 30 s after the attempt first went Unresolved, then about as long again as it has
/// waited, up to 5 minutes between reads; no count needs to be stored.
fn reconfirm_due(ledger: &LedgerState, serial: u64, now: DateTime<Utc>) -> bool {
    let Some(piece) = ledger.piece(serial) else {
        return false;
    };
    let Some(at) = piece.outcome_at else {
        return false;
    };
    let waited = (at - piece.prepared_at).max(chrono::Duration::seconds(30));
    at + waited.min(chrono::Duration::minutes(5)) <= now
}

/// Open image and notice obligations; any of them refuses a source retirement.
pub(crate) fn open_count(ledger: &LedgerState) -> usize {
    ledger.tool_images().open_obligations(ledger).len()
}

/// Per spool key, the lowest offset GC keeps (open obligations' and notices' records); `None`
/// while derived entries still wait to be appended.
pub(crate) fn kept_sources(
    ledger: &LedgerState,
    deriver: &mut UnitDeriver,
) -> Option<HashMap<String, u64>> {
    if deriver.images().is_some_and(|run| run.has_queued()) {
        return None;
    }
    Some(ledger.tool_images().kept_from(ledger))
}

/// The upload for an image attempt, else the ordinary unit post.
pub(in crate::services::tui_o::writer) fn unit_post<P: DiscordPort>(
    port: &P,
    channel: u64,
    piece: &PieceWork,
    attachment: Option<(Vec<u8>, String)>,
) -> Pin<Box<dyn Future<Output = PostOutcome> + Send>> {
    let (payload, origin, kind) = (
        piece.payload.clone(),
        piece.output_origin.clone(),
        piece.unit_key.kind,
    );
    match attachment {
        Some((bytes, name)) => {
            Box::pin(port.post_unit_attachment(channel, payload, origin, kind, bytes, name))
        }
        None => Box::pin(port.post_unit(channel, payload, origin, kind)),
    }
}

/// 400 and 413 refuse the image alone; any other status refuses the channel.
fn refusal_reason(status: u16) -> Option<OmitReason> {
    match status {
        400 => Some(OmitReason::UnsupportedType),
        413 => Some(OmitReason::TooLarge),
        _ => None,
    }
}

/// Whether the oldest segment of `source` lies wholly below the offset GC keeps from.
pub(crate) fn below(store: &ChannelStore, source: &SourceId, floor: Option<u64>) -> bool {
    let end = || store.oldest_segment_end(source);
    floor.is_none_or(|from| end().is_some_and(|end| end <= from))
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink> ChannelWriter<P, L, A> {
    /// Before GC collects `source`'s oldest segment, appends the open turn's span and waiting
    /// calls whose evidence lies there, so replay never needs the collected rows.
    pub(crate) fn persist_before_gc(&mut self, deriver: &mut UnitDeriver, source: &SourceId) {
        let Some(below) = self.store().oldest_segment_end(source) else {
            return;
        };
        let images = self.store().ledger().tool_images();
        if let Some(run) = deriver.images() {
            run.persist_before_gc(&source_key(source), below, images);
        }
        self.record_images(deriver);
    }

    /// Appends what the deriver queued, in order; a failure stops the writer.
    pub(crate) fn record_images(&mut self, deriver: &mut UnitDeriver) {
        let entries = deriver.images().map(|run| run.take_entries());
        for entry in entries.into_iter().flatten() {
            let entry = Box::new(entry);
            if self.record(LedgerEntry::ToolImage { entry }).is_err() {
                return;
            }
        }
    }

    /// An image marker from the owed order: the first attempt, or an operator-approved retry of
    /// a channel-wide refusal; any later step is the selector's.
    pub(in crate::services::tui_o::writer) async fn deliver_image(
        &mut self,
        piece: &PieceWork,
        admission: Option<Admission<'_>>,
    ) -> Step {
        let ledger = self.store().ledger();
        let images = ledger.tool_images();
        let obligation = images.obligation(&piece.unit_key).cloned();
        let omitted = images.omitted(&piece.unit_key).is_some();
        let disposition = ledger.disposition(&piece.unit_key, 0);
        let Some(obligation) = obligation else {
            return self.stop(WriterAlarm::LedgerViolation {
                detail: format!("image {} has no obligation", piece.unit_key.native_key),
            });
        };
        if omitted {
            return Step::Done;
        }
        let approved = match disposition {
            PieceDisposition::Fresh => None,
            PieceDisposition::Authorized { rejected_serial } => Some(rejected_serial),
            _ => return Step::Done,
        };
        let origin = piece.output_origin.clone();
        self.attempt_image(&obligation, origin, admission, approved)
            .await
    }

    /// One poll's image or notice action, after the ordinary open attempt is settled.
    pub(crate) async fn select_images(&mut self, now: DateTime<Utc>) -> Step {
        if self.is_stopped() {
            return Step::Stopped;
        }
        if let Err(step) = self.settle_open().await {
            return step;
        }
        let Some(action) = next_action(self.store().ledger(), now) else {
            return Step::Done;
        };
        match action {
            Action::Retry(key) => {
                let ledger = self.store().ledger();
                let Some(obligation) = ledger.tool_images().obligation(&key).cloned() else {
                    return Step::Done;
                };
                // The same caption keeps the earlier attempt's owner seal.
                let origin = ledger
                    .latest_piece(&key, 0)
                    .and_then(|(serial, _)| ledger.output_origin(serial).cloned());
                self.attempt_image(&obligation, origin, None, None).await
            }
            Action::Omit(unit_key) => {
                let reason = OmitReason::NotConfirmed.as_str().to_owned();
                let entry = Box::new(ImageEntry::ImageOmitted { unit_key, reason });
                self.record(LedgerEntry::ToolImage { entry })
                    .map_or_else(|step| step, |()| Step::Done)
            }
            Action::Notice(unit_key) => {
                let images = self.store().ledger().tool_images();
                let members = images.notice_members(&unit_key).into_iter();
                let omitted: Vec<String> = members.map(|m| m.key.native_key).collect();
                tracing::info!(unit_key = ?unit_key, ?omitted, "[tui_o] omission notice attempt");
                let payload = notice::payload(self.store().ledger().tool_images(), &unit_key);
                let piece = PieceWork {
                    unit_key,
                    index: 0,
                    payload,
                    output_origin: None,
                    notice_origin: None,
                };
                self.send_piece(&piece, None, None, None, None).await
            }
            Action::Reconfirm(serial) => self.reconfirm(serial).await,
        }
    }

    /// 400 (or 413) refuses this image alone: it is omitted and its notice follows. 403/404 refuse
    /// the channel and take the ordinary blocked path, as the channel's body would.
    pub(in crate::services::tui_o::writer) fn image_refused(
        &mut self,
        serial: u64,
        status: u16,
    ) -> Option<Step> {
        let reason = refusal_reason(status)?;
        let ledger = self.store().ledger();
        let unit_key = ledger.piece(serial)?.unit_key.clone();
        ledger.tool_images().obligation(&unit_key)?;
        tracing::warn!(channel = self.channel(), serial, status, unit_key = ?unit_key,
            "[tui_o] image refused; omitted with a notice");
        let reason = reason.as_str().to_owned();
        let entry = Box::new(ImageEntry::ImageOmitted { unit_key, reason });
        let recorded = (self.record(LedgerEntry::Rejected { serial, status }))
            .and_then(|()| self.record(LedgerEntry::ToolImage { entry }));
        Some(recorded.map_or_else(|step| step, |()| Step::Done))
    }

    /// Boot completes an image refusal a crash cut between its Rejected and ImageOmitted, so the
    /// image is omitted with its notice rather than read as a blocked channel.
    pub(in crate::services::tui_o::writer) fn finish_image_refusals(&mut self) {
        let ledger = self.store().ledger();
        if ledger.violation().is_some() {
            return;
        }
        let open = ledger.tool_images().open_obligations(ledger).into_iter();
        let cut: Vec<_> = (open.filter_map(|open| match open {
            OpenObligation::Image {
                key,
                step: ImageStep::Waiting(Waiting::Rejected { status, .. }),
            } => Some((key, refusal_reason(status)?)),
            _ => None,
        }))
        .collect();
        for (unit_key, reason) in cut {
            let reason = reason.as_str().to_owned();
            let entry = Box::new(ImageEntry::ImageOmitted { unit_key, reason });
            if self.record(LedgerEntry::ToolImage { entry }).is_err() {
                return;
            }
        }
    }

    async fn attempt_image(
        &mut self,
        obligation: &ImageObligation,
        output_origin: Option<
            crate::services::cluster::intake_router_hook::owner_record::output::Artifact,
        >,
        admission: Option<Admission<'_>>,
        approved: Option<u64>,
    ) -> Step {
        let bytes = match self.image_bytes(obligation) {
            Ok(bytes) => bytes,
            Err(detail) => return self.stop(WriterAlarm::Halted { detail }),
        };
        let piece = PieceWork {
            unit_key: obligation.unit_key.clone(),
            index: 0,
            payload: notice::caption(&obligation.display_name, &obligation.marker),
            output_origin,
            notice_origin: None,
        };
        let attachment = Some((bytes, obligation.display_name.clone()));
        self.send_piece(&piece, admission, None, approved, attachment)
            .await
    }

    /// The image's bytes, read again from its spooled record and checked against its digest.
    fn image_bytes(&mut self, obligation: &ImageObligation) -> Result<Vec<u8>, String> {
        let provider: ShadowProvider = obligation.unit_key.provider;
        let mut found = None;
        let scan = self.store().for_each_frame(&obligation.source, |frame, _| {
            if let SpoolFrame::Record(record) = frame
                && record.start == obligation.record_start
            {
                found = Some(record.line);
            }
        });
        scan.map_err(|error| format!("image spool read: {error:?}"))?;
        let missing = || format!("image {} bytes are gone", obligation.unit_key.native_key);
        let line = found.ok_or_else(missing)?;
        let value = serde_json::from_slice(&line).map_err(|error| error.to_string())?;
        let image = super::extract(provider, &value).into_iter().find(|image| {
            image.call_id == obligation.call_id
                && u32::try_from(image.index).ok() == Some(obligation.image_index)
        });
        match image.map(|image| image.body) {
            Some(ImageBody::Inline { sha256, bytes, .. })
                if Some(&sha256) == obligation.sha256.as_ref() =>
            {
                Ok(bytes)
            }
            _ => Err(missing()),
        }
    }
}
