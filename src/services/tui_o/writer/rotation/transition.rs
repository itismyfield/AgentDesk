//! What an operator's transition cleanup reads from the channel's readers and what it changes in
//! them: a copy of one reader, every history candidate of a legacy hop, and the retired watch.

use std::collections::BTreeMap;

use serde::Serialize;

use super::*;
use crate::services::tui_o::writer::transition_cleanup::ReaderView;

/// How a source retired by a transition cleanup is watched for growth from then on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum Watch {
    /// A legacy reader keeps its FD for the retired watch, as a provider retirement keeps it.
    Fd,
    /// A configured reader closes its FD and watches the path.
    Path,
    /// A legacy reader that held no FD: nothing sees the source grow.
    Unwatched,
}

/// The growth alarm of a retired source, naming the cleanup its cursor records, if any.
pub(super) fn regrowth_alarm<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
    writer: &mut ChannelWriter<P, L, A>,
    source: SourceId,
) {
    let audit = (writer.store().cursor(&source)).and_then(|c| c.transition_cleanup.as_ref());
    let cleanup_history = audit.map(|audit| audit.job_id.clone());
    writer.alarm(WriterAlarm::RetiredSourceGrew {
        source,
        cleanup_history,
    });
}

/// Every applied hop from each legacy successor's old source to its new one, in the log as the
/// stored sources name it. A Pending resolved more than once counts once per resolution.
pub(crate) fn legacy_hop_candidates(
    events: &[BindingEvent],
    checkpoint: u64,
    rotation: &Rotation,
    channel: u64,
    provider: ShadowProvider,
) -> Result<BTreeMap<String, Vec<(u64, String)>>, String> {
    let applied = &events[..events.partition_point(|e| e.seq <= checkpoint)];
    for (expected, event) in (1..).zip(applied) {
        if event.seq != expected || event.channel_id != channel || event.provider != provider {
            return Err(format!("binding history breaks at seq {}", event.seq));
        }
    }
    if applied.last().map_or(0, |event| event.seq) != checkpoint {
        return Err(format!(
            "binding history ends before checkpoint {checkpoint}"
        ));
    }
    let legacy = (rotation.successors.iter()).filter(|(_, n)| n.seq.is_none() && n.proof.is_none());
    let mut candidates = BTreeMap::new();
    for (key, next) in legacy {
        let mut found = Vec::new();
        for event in applied {
            let BindingRecord::Bound {
                old: Some(old),
                new,
                ..
            } = &event.record
            else {
                continue;
            };
            if source_key(old) != *key {
                continue;
            }
            let news: Vec<&SourceId> = match new {
                BindingTarget::Source(source) => vec![source],
                BindingTarget::Pending { .. } => (applied.iter())
                    .filter_map(|later| match &later.record {
                        BindingRecord::Resolved {
                            resolves_seq,
                            source,
                        } if *resolves_seq == event.seq => Some(source),
                        _ => None,
                    })
                    .collect(),
            };
            if news.iter().any(|new| **new == next.source && *new != old) {
                found.extend(news.iter().map(|_| (event.seq, event.tmux_session.clone())));
            }
        }
        candidates.insert(key.clone(), found);
    }
    Ok(candidates)
}

impl<B: BindingEvents> Sources<B> {
    pub(crate) fn transition_provider(&self) -> ShadowProvider {
        self.provider
    }

    /// The rotation and binding checkpoint the readers act on.
    pub(crate) fn transition_rotation(&self) -> (&Rotation, Option<u64>) {
        (&self.rotation, self.checkpoint)
    }

    /// One reader's state, and the fingerprint of the FD it holds.
    pub(crate) fn reader_view(
        &self,
        source: &SourceId,
    ) -> Option<(ReaderView, Option<Result<Fingerprint, String>>)> {
        let reader = self.readers.iter().find(|r| r.source == *source)?;
        let capture = reader.capture.as_ref();
        let view = ReaderView {
            capture: capture.map(|c| (c.captured_through(), c.read_through())),
            pending_batch: reader.pending.is_some(),
            drain_to: reader.drain_to,
            captured_any: reader.captured_any,
        };
        let fd = capture.map(|c| c.fingerprint().map_err(|error| error.to_string()));
        Some((view, fd))
    }

    /// [`legacy_hop_candidates`] over the whole log, read again; a log not read yet is an error.
    pub(crate) fn legacy_candidates<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
    ) -> Result<BTreeMap<String, Vec<(u64, String)>>, String> {
        let checkpoint = (self.checkpoint)
            .filter(|_| !self.legacy_hops_unread)
            .ok_or("legacy hop history unread")?;
        let events = self.bindings.binding_events_since(self.channel, 0)?;
        let named = (self.names.read(writer.store(), events, 0, Some(checkpoint)))
            .map_err(|alarm| format!("{alarm:?}"))?
            .ok_or("binding log unread")?
            .1;
        legacy_hop_candidates(
            &named,
            checkpoint,
            &self.rotation,
            self.channel,
            self.provider,
        )
    }

    /// Watches a source its transition cleanup retired as a provider retirement would, keeping
    /// what it captured and dropping only its own pending validation.
    pub(crate) fn arm_transition_watch(&mut self, source: &SourceId) -> Option<Watch> {
        if (self.validation.as_ref()).is_some_and(|job| job.cursor.source == *source) {
            self.validation = None;
        }
        let configured = self.reader_config.is_some();
        let reader = self.readers.iter_mut().find(|r| r.source == *source)?;
        reader.watch_until = Some(Instant::now() + RETIRED_WATCH);
        (reader.quiet, reader.drain_to, reader.dormant, reader.idle) = (None, None, None, None);
        if configured {
            reader.capture = None;
            return Some(Watch::Path);
        }
        Some(match reader.capture {
            Some(_) => Watch::Fd,
            None => Watch::Unwatched,
        })
    }

    /// A file already grown past its cursor un-retires the source the cleanup just retired.
    pub(crate) fn regrown_after_transition<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        source: &SourceId,
    ) -> Result<bool, WriterAlarm> {
        let through = (writer.store().cursor(source)).map_or(0, |c| c.captured_through);
        if !Fingerprint::path(source).is_ok_and(|fp| fp.len > through) {
            return Ok(false);
        }
        self.unretire(writer, source)?;
        regrowth_alarm(writer, source.clone());
        Ok(true)
    }
}
