use super::*;

impl BindingEvent {
    pub(crate) fn source_refresh(&self) -> Option<(&SourceId, &SourceId)> {
        match &self.record {
            BindingRecord::Bound {
                old,
                new: BindingTarget::Source(new),
                cause: BindingCause::SameSessionVerified,
                evidence,
                ..
            } if self.provider == ShadowProvider::Claude
                && !self.tmux_session.is_empty()
                && !self.execution_nonce.is_empty()
                && matches!(evidence.hook_event.as_str(), "user_prompt_submit" | "stop") =>
            {
                Some((old.as_ref().unwrap_or(new), new))
            }
            _ => None,
        }
    }
}

impl<B: BindingEvents> Sources<B> {
    pub(super) fn refresh_binding<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        event: &BindingEvent,
    ) -> Result<(), WriterAlarm> {
        let Some((_, new)) = event.source_refresh() else {
            return Ok(());
        };
        let live = Fingerprint::path(new).is_ok_and(|fp| (fp.dev, fp.ino) == (new.dev, new.ino));
        if !live {
            return Ok(());
        }
        let store = writer.store();
        let old = renumbered::canonical(store.cursors(), new).map_err(halt)?;
        let Some(mut cursor) = store.cursor(&old).filter(|_| old != *new).cloned() else {
            return Ok(());
        };
        self.guard_selected_reference(store, &old)?;
        let Some(reader) = self.readers.iter_mut().find(|reader| reader.source == old) else {
            return Err(halt("source refresh reader missing"));
        };
        cursor.source = new.clone();
        let capture = owe::reopen_native(&mut self.native, self.channel, self.provider, &cursor)
            .map_err(|failure| failure.halt_at(new))?;
        store
            .refresh_source(&old, new)
            .map_err(halted("source refresh"))?;
        self.rotation = store
            .rotation()
            .map_err(halted("source refresh rotation"))?;
        if let Some(native) = &mut self.native {
            native.end(&old);
        }
        let (old_key, new_key) = (source_key(&old), source_key(new));
        if let Some(lineage) = self.lineage.remove(&old_key) {
            self.lineage.insert(new_key.clone(), lineage);
        }
        if self.prefix_seen.remove(&old_key) {
            self.prefix_seen.insert(new_key);
        }
        for source in &mut self.refined {
            if *source == old {
                source.clone_from(new);
            }
        }
        if self
            .validation
            .as_ref()
            .is_some_and(|job| job.cursor.source == old)
        {
            self.validation = None;
        }
        reader.source.clone_from(new);
        reader.capture = Some(capture);
        reader.pending = None;
        reader.dormant = None;
        reader.idle = None;
        reader.observation = None;
        reader.persisted_at = None;
        reader.parked = false;
        if matches!(&self.current, Current::Known(source) if *source == old) {
            self.current = Current::Known(new.clone());
        }
        self.replaced = None;
        self.refreshed.push((old, new.clone()));
        Ok(())
    }
}
