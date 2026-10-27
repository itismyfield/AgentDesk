//! Verified rotated sources drain their retained descriptor under historical ownership.

use super::*;

impl<B: BindingEvents> Sources<B> {
    pub(super) fn capture_one<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
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
        let verified = self.source_proven(index);
        let retained = verified && self.readers[index].rotated_at.is_some();
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
            None => match if verified {
                capture.poll_verified(MAX_READ_BYTES, retained)
            } else {
                capture.poll(MAX_READ_BYTES)
            } {
                CaptureOutcome::Batch(batch) => batch,
                CaptureOutcome::Anomaly(anomaly) => {
                    let (kind, detail) = (anomaly.kind, anomaly.detail);
                    if verified
                        && matches!(
                            kind,
                            SourceAnomalyKind::Missing | SourceAnomalyKind::Replaced
                        )
                    {
                        return Ok(());
                    }
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
            self.owe(
                deriver,
                owed,
                &batch.source,
                record,
                crate::services::tui_o::shadow::IDENTITY_VERSION,
            );
        }
        if at_end {
            self.pend_at_end(&key, batch.captured_through);
        }
        Ok(())
    }

    fn source_proven(&self, index: usize) -> bool {
        let reader = &self.readers[index];
        self.provider == ShadowProvider::Codex
            && crate::services::tui_prompt_dedupe::codex_verified_channel_requires_proof(
                self.channel,
            )
            && crate::services::tui_prompt_dedupe::codex_verified_o_source_allowed(
                self.channel,
                &reader.source,
            )
    }
}
