//! Content-proven Claude starts and the capture pass that re-evaluates retained Pending bytes.

use super::fork_lineage::{Class, RowIds};
use super::*;
use crate::services::tui_o::store::rotation::{BoundaryEvidence, HotCandidate};

impl<B: BindingEvents> Sources<B> {
    pub(super) fn refine_provisional<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        event: &BindingEvent,
        source: &SourceId,
        parent_hint: Option<&SourceId>,
    ) -> Result<(), WriterAlarm> {
        let Some(link) = self.rotation.link(source).cloned().filter(|l| {
            l.hot_candidate.is_some() && matches!(l.boundary, Boundary::Pending { .. })
        }) else {
            return Ok(());
        };
        let mut event = event.clone();
        if let BindingRecord::Bound {
            new: BindingTarget::Source(new),
            parent_hint: parent,
            ..
        } = &mut event.record
        {
            *new = source.clone();
            *parent = parent_hint.cloned();
        }
        self.flush(writer)?;
        if let Some(rotation) = writer
            .store()
            .refine_provisional_hot_link(&link, &event)
            .map_err(halted("explicit boundary refinement"))?
        {
            self.rotation = rotation;
            self.prefix_seen.remove(&source_key(source));
            self.refined.push(source.clone());
        }
        Ok(())
    }

    fn replay_refined<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        deriver: &mut UnitDeriver,
        owed: &mut impl Owe,
    ) -> Result<(), WriterAlarm> {
        for source in std::mem::take(&mut self.refined) {
            let store = writer.store();
            self.sync_parent(&source_key(&source), |p| {
                store
                    .cursor(p)
                    .map(|c| (c.captured_through, c.prefix_hash.clone()))
            });
            writer
                .store()
                .for_each_frame(&source, |frame| {
                    if let SpoolFrame::Record(record) = frame {
                        self.owe(deriver, owed, &source, &record);
                    }
                })
                .map_err(halted("explicit boundary replay"))?;
        }
        self.flush(writer)
    }

    /// Runs even when every reader returned an empty batch or was held/dormant.
    pub(super) fn reevaluate_boundaries<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        deriver: &mut UnitDeriver,
        owed: &mut impl Owe,
        replay: bool,
    ) -> Result<(), WriterAlarm> {
        let pending: Vec<_> = self
            .rotation
            .links
            .values()
            .filter(|link| {
                link.hot_candidate.is_some() && matches!(link.boundary, Boundary::Pending { .. })
            })
            .cloned()
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        self.flush(writer)?;
        let Some((_, events)) = self.read_log(writer, 0)? else {
            return Ok(());
        };
        for link in pending {
            let Some((from, evidence)) = self.boundary_proof(writer, &link, &events)? else {
                continue;
            };
            writer
                .store()
                .resolve_automatic_boundary(&link.source, from, evidence)
                .map_err(halted("automatic boundary"))?;
            self.rotation = writer.store().rotation().map_err(halted("rotation"))?;
            let store = writer.store();
            self.sync_parent(&source_key(&link.source), |parent| {
                store
                    .cursor(parent)
                    .map(|c| (c.captured_through, c.prefix_hash.clone()))
            });
            if replay {
                writer
                    .store()
                    .for_each_frame(&link.source, |frame| {
                        if let SpoolFrame::Record(record) = frame {
                            self.owe(deriver, owed, &link.source, &record);
                        }
                    })
                    .map_err(halted("automatic boundary replay"))?;
            }
        }
        Ok(())
    }

    fn boundary_proof<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        link: &SourceLink,
        events: &[BindingEvent],
    ) -> Result<Option<(u64, BoundaryEvidence)>, WriterAlarm> {
        let Some(candidate) = &link.hot_candidate else {
            return Ok(None);
        };
        let valid_event = events
            .iter()
            .find(|event| event.seq == link.seq)
            .is_some_and(|event| {
                event.channel_id == candidate.channel
                    && event.channel_id == self.channel
                    && event.provider == ShadowProvider::Claude
                    && event.provider == self.provider
                    && event.tmux_session == candidate.pane
                    && event.execution_nonce == candidate.nonce
                    && matches!(
                        event.record,
                        BindingRecord::Bound {
                            cause: BindingCause::Unknown,
                            ..
                        }
                    )
                    && hop(event, events)
                        .is_some_and(|(a, b)| *a == candidate.parent && *b == link.source)
            });
        let superseded = events.iter().any(|event| {
            event.seq > link.seq
                && matches!(
                    &event.record,
                    BindingRecord::Bound { new: BindingTarget::Source(source), cause, .. }
                        if *cause != BindingCause::Unknown && *source == link.source
                )
        });
        if !valid_event || superseded || !verified_first(&link.source) {
            return Ok(None);
        }
        let Some(from) = parent_from(&self.rotation, &candidate.parent) else {
            return Ok(None);
        };
        let (Some(parent), Some(child)) = (
            writer.store().cursor(&candidate.parent).cloned(),
            writer.store().cursor(&link.source).cloned(),
        ) else {
            return Ok(None);
        };
        let drained = self
            .rotation
            .successors
            .get(&source_key(&candidate.parent))
            .filter(|next| next.source == link.source && next.seq == Some(link.seq))
            .is_some_and(|next| next.drain_to.is_none_or(|to| parent.captured_through >= to));
        if !drained
            || child.captured_through > LINEAGE_SCAN_CAP_BYTES
            || reopen(&parent).is_err()
            || reopen(&child).is_err()
        {
            return Ok(None);
        }
        let Some(lineage) = fork_lineage::sync(
            None,
            self.provider,
            &candidate.parent,
            parent.captured_through,
        ) else {
            return Ok(None);
        };
        let mut scratch = UnitDeriver::new(self.channel, self.provider);
        let (mut anchor, mut novel, mut invalid) = (false, None, false);
        let ledger = writer.store().ledger().clone();
        writer
            .store()
            .for_each_frame(&link.source, |frame| {
                if novel.is_some() || invalid {
                    return;
                }
                let SpoolFrame::Record(record) = frame else {
                    invalid = true;
                    return;
                };
                let Some(row) = RowIds::of(self.provider, &record.line) else {
                    invalid = true;
                    return;
                };
                if row.is_empty() {
                    return;
                }
                match lineage.native_class(self.provider, &record.line, from) {
                    Class::Inherited => {
                        let derived = scratch.derive(&record);
                        let mut units = HashMap::<_, Vec<_>>::new();
                        for work in derived {
                            let key = match &work {
                                Derived::Piece(p) => p.unit_key.clone(),
                                Derived::Excluded { unit_key, .. } => unit_key.clone(),
                                Derived::Blocked { .. } => {
                                    invalid = true;
                                    return;
                                }
                            };
                            units.entry(key).or_default().push(work);
                        }
                        anchor |= units
                            .values()
                            .any(|work| work.iter().all(|d| super::super::owed::acked(&ledger, d)));
                    }
                    Class::New => {
                        let value = serde_json::from_slice(&record.line).unwrap_or_default();
                        let native = classify(self.provider, &value).iter().any(|fact| {
                            matches!(
                                fact,
                                RecordFact::Unit(..)
                                    | RecordFact::Prompt(..)
                                    | RecordFact::Assistant
                            )
                        });
                        if native {
                            if anchor {
                                novel = Some(record.start);
                            } else {
                                invalid = true;
                            }
                        }
                    }
                    _ => invalid = true,
                }
            })
            .map_err(halted("pending boundary spool"))?;
        Ok(novel.filter(|_| !invalid).map(|from| {
            (
                from,
                BoundaryEvidence {
                    candidate: candidate.clone(),
                    seq: link.seq,
                    through: child.captured_through,
                    prefix_hash: child.prefix_hash,
                    parent_through: parent.captured_through,
                    parent_hash: parent.prefix_hash,
                },
            )
        }))
    }

    /// Spools each read source once. A successor waits while a predecessor is still short of its
    /// rotation-time length, so that backlog is owed first whatever the reader order.
    pub fn capture<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        deriver: &mut UnitDeriver,
        owed: &mut impl Owe,
    ) -> Result<(), WriterAlarm> {
        if self.checkpoint.is_none() {
            return Ok(());
        }
        self.replay_refined(writer, deriver, owed)?;
        let mut left: Vec<usize> = (0..self.readers.len()).collect();
        while let Some(at) = left.iter().position(|&index| !self.held(index)) {
            let index = left.remove(at);
            self.capture_one(writer, deriver, owed, index)?;
        }
        self.reevaluate_boundaries(writer, deriver, owed, true)?;
        self.flush(writer)?;
        if let Some(source) = self.stalled(deriver) {
            return Err(WriterAlarm::RotationStalled { source });
        }
        Ok(())
    }

    fn capture_one<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        deriver: &mut UnitDeriver,
        owed: &mut impl Owe,
        index: usize,
    ) -> Result<(), WriterAlarm> {
        if self.reader_config.is_some() && self.readers[index].watch_until.is_none() {
            let reader = &self.readers[index];
            if reader.capture.is_none() && (reader.drain_to.is_some() || reader.pending.is_some()) {
                return Err(halt(
                    "reader without an FD still holds a capture obligation",
                ));
            }
        }
        let reader = &mut self.readers[index];
        let Some(capture) = reader
            .capture
            .as_mut()
            .filter(|_| reader.watch_until.is_none())
        else {
            if self.reader_config.is_none() {
                reader.drain_to = None;
            }
            return Ok(());
        };
        let retried = reader.pending.is_some();
        let batch = match reader.pending.take() {
            Some(batch) => batch,
            None => match capture.poll(MAX_READ_BYTES) {
                CaptureOutcome::Batch(batch) => batch,
                CaptureOutcome::Anomaly(anomaly) => {
                    let (kind, detail) = (anomaly.kind, anomaly.detail);
                    if self.reader_config.is_some() && kind == SourceAnomalyKind::Missing {
                        return self.cleanup_missing(writer, index, SystemTime::now());
                    }
                    return Err(halt(format!("source {kind:?}: {detail}")));
                }
            },
        };
        let read_through = capture.read_through();
        let at_end = capture.file_len().ok() == Some(capture.captured_through());
        let first_capture = !reader.captured_any && !batch.records.is_empty();
        match writer.store().append_spool(&batch, &capture.prefix_hash()) {
            Ok(()) => reader.captured_any |= !batch.records.is_empty(),
            Err(StoreError::SpoolFull) => {
                if !retried {
                    writer.alarm(WriterAlarm::SpoolFull);
                }
                reader.pending = Some(batch);
                return Ok(());
            }
            Err(error) => return Err(halt(format!("spool append: {error:?}"))),
        }
        if !reader.drain_to.is_some_and(|end| read_through < end) {
            reader.drain_to = None;
        }
        if first_capture && self.reader_config.is_some() {
            for old in &mut self.readers {
                if self
                    .rotation
                    .successors
                    .get(&source_key(&old.source))
                    .is_some_and(|next| next.source == batch.source)
                {
                    old.quiet = None;
                }
            }
        }
        let key = source_key(&batch.source);
        let store = writer.store();
        self.sync_parent(&key, |parent| {
            store
                .cursor(parent)
                .map(|c| (c.captured_through, c.prefix_hash.clone()))
        });
        for record in &batch.records {
            self.owe(deriver, owed, &batch.source, record);
        }
        if at_end {
            self.pend_at_end(&key, batch.captured_through);
        }
        Ok(())
    }

    /// A reading predecessor of this reader has not yet spooled through its rotation-time length.
    fn held(&self, index: usize) -> bool {
        let source = &self.readers[index].source;
        self.readers.iter().any(|reader| {
            reader.drain_to.is_some()
                && (reader.reading() || self.reader_config.is_some())
                && (self.rotation.successors.get(&source_key(&reader.source)))
                    .is_some_and(|next| next.source == *source)
        })
    }
}

pub(super) fn candidate(event: &BindingEvent, parent: &SourceId) -> HotCandidate {
    HotCandidate {
        channel: event.channel_id,
        pane: event.tmux_session.clone(),
        nonce: event.execution_nonce.clone(),
        parent: parent.clone(),
    }
}

pub(super) fn parent_from(rotation: &Rotation, parent: &SourceId) -> Option<u64> {
    match rotation.link(parent).map(|link| &link.boundary) {
        None => Some(0),
        Some(Boundary::Owed { from }) => Some(*from),
        _ => None,
    }
}

fn verified_first(source: &SourceId) -> bool {
    use crate::services::claude_tui::source_verify::observe_transcript;
    #[cfg(unix)]
    use crate::services::claude_tui::source_verify::{FirstRecord, OpenedTranscript};
    #[cfg(unix)]
    use crate::services::cluster::stream_relay::SourceFileIdentity;
    #[cfg(unix)]
    use crate::services::tui_o::shadow::capture::{FileMatch, file_match};
    if source.session_id.is_empty() {
        return false;
    }
    match observe_transcript(&source.path) {
        #[cfg(unix)]
        Ok(OpenedTranscript::Opened {
            file: SourceFileIdentity::Unix { dev, ino },
            first: FirstRecord::Session(session),
        }) => session == source.session_id && file_match(source, (dev, ino)) != FileMatch::Other,
        _ => false,
    }
}
