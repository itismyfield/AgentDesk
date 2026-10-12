//! Explicit reader policy, leaving the existing actor on its legacy path.

use super::*;

impl<B: BindingEvents> Sources<B> {
    /// Applies operator-resolved boundaries, re-derives every retained spool with predecessors
    /// first, then reopens each source at its cursor.
    pub fn resume<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        deriver: &mut UnitDeriver,
        owed: &mut impl Owe,
    ) -> Result<(), WriterAlarm> {
        self.resume_at(writer, deriver, owed, SystemTime::now())
    }

    pub fn resume_at<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        deriver: &mut UnitDeriver,
        owed: &mut impl Owe,
        wall_now: SystemTime,
    ) -> Result<(), WriterAlarm> {
        if self.reader_config.is_some() {
            self.observe_clock(wall_now)?;
            writer
                .store()
                .prepare_cleanup_witnesses()
                .map_err(halted("witness directory"))?;
        }
        let resolved = writer.store().apply_resolved_boundaries();
        resolved.map_err(halted("resolved boundary"))?;
        renumbered::unique(writer.store().cursors()).map_err(halt)?;
        self.rotation = writer.store().rotation().map_err(halted("rotation"))?;
        let checkpoint = writer.store().binding_checkpoint();
        self.checkpoint = checkpoint.map_err(halted("binding checkpoint"))?;
        if let Some(logged) = self.read_log(writer, 0)? {
            let checkpoint = self.names.start(writer.store(), &logged, self.checkpoint)?;
            if let Some(checkpoint) = checkpoint {
                self.restore_hops(writer, &logged.1, checkpoint)?;
            }
        }
        let mut cursors: Vec<_> = writer.store().cursors().cloned().collect();
        cursors.sort_by_key(|cursor| {
            let seq = self.rotation.link(&cursor.source).map_or(0, |l| l.seq);
            (predecessors(&self.rotation, &cursor.source), seq)
        });
        let now = Instant::now();
        let consumed: HashMap<String, u64> = (cursors.iter())
            .map(|cursor| (source_key(&cursor.source), cursor.captured_through))
            .collect();
        for cursor in cursors {
            let (key, mut captured_any) = (source_key(&cursor.source), false);
            self.sync_parent(&key, |parent| consumed.get(&source_key(parent)).copied());
            if let Some(SourceLink {
                boundary: Boundary::Pending { .. },
                ..
            }) = self.rotation.link(&cursor.source)
            {
                let source = cursor.source.clone();
                self.raised.push(WriterAlarm::BoundaryPending { source });
            }
            let replay = writer.store().for_each_frame(&cursor.source, |frame| {
                if let SpoolFrame::Record(record) = frame {
                    captured_any = true;
                    self.owe(deriver, owed, &cursor.source, &record);
                }
            });
            replay.map_err(halted("spool replay"))?;
            let missing = self.reader_config.is_some()
                && matches!(
                    Fingerprint::path(&cursor.source),
                    Err((SourceAnomalyKind::Missing, _))
                );
            let capture = match reopen(&cursor) {
                Ok(capture) => Some(capture),
                Err(Reopen::Unread(_)) if cursor.retired => None,
                Err(Reopen::Unread(error))
                    if missing
                        && error.kind() == std::io::ErrorKind::NotFound
                        && matches!(
                            Fingerprint::path(&cursor.source),
                            Err((SourceAnomalyKind::Missing, _))
                        ) =>
                {
                    None
                }
                Err(failed) => return Err(failed.halt()),
            };
            let mut reader = Reader::new(cursor.source.clone(), capture);
            reader.captured_any = captured_any
                || !writer
                    .store()
                    .ledger()
                    .gc_segments(&cursor.source)
                    .is_empty();
            reader.rotated_at = self.rotation.successors.contains_key(&key).then_some(now);
            reader.watch_until = cursor.retired.then_some(now + RETIRED_WATCH);
            reader.drain_to = match self.rotation.successors.get(&key) {
                // A record written before the rotation length was kept holds to the length now.
                Some(next) if next.seq.is_none() => reader.backlog_end(),
                Some(next) => next.drain_to.filter(|end| cursor.captured_through < *end),
                None => None,
            };
            // A retired reader is never captured again, so its prefix-only end is judged here.
            let ended = (reader.capture.as_ref())
                .is_none_or(|capture| capture.file_len().ok() == Some(cursor.captured_through));
            if self.reader_config.is_some() && !cursor.retired {
                reader.observation = writer
                    .store()
                    .cleanup_witness(&cursor.source)
                    .map_err(halted("cleanup witness"))?;
                reader.persisted_at = reader.observation.as_ref().map(|w| w.observed_at);
                if ended && let Some(capture) = reader.capture.as_ref() {
                    let fp = capture
                        .verify_eof(cursor.captured_through, &cursor.prefix_hash)
                        .map_err(|e| halt(format!("restore EOF: {e}")))?;
                    if fp.modified > wall_now {
                        return Err(halt("source mtime is in the future"));
                    }
                }
            }
            if (reader.watch_until.is_some() || self.reader_config.is_some()) && ended {
                self.pend_at_end(&key, cursor.captured_through);
            }
            self.readers.push(reader);
        }
        self.restore_legacy_hops(writer);
        self.flush(writer)?;
        if self.reader_config.is_some() {
            for index in 0..self.readers.len() {
                if missing_reader(&self.readers[index]) {
                    self.cleanup_missing(writer, index, wall_now)?;
                    continue;
                }
                let reader = &mut self.readers[index];
                let cursor = writer
                    .store()
                    .cursor(&reader.source)
                    .cloned()
                    .ok_or_else(|| halt("restore cursor missing"))?;
                if !cursor.retired
                    && self
                        .rotation
                        .successors
                        .contains_key(&source_key(&reader.source))
                    && reader.drain_to.is_none()
                    && let Some(capture) = reader
                        .capture
                        .as_ref()
                        .filter(|c| c.file_len().ok() == Some(cursor.captured_through))
                {
                    let fingerprint = capture
                        .verify_eof(cursor.captured_through, &cursor.prefix_hash)
                        .map_err(|e| halt(format!("restore validation: {e}")))?;
                    if Fingerprint::path(&reader.source).ok().as_ref() != Some(&fingerprint) {
                        return Err(halt("source changed during restore"));
                    }
                    let witness = witness(&cursor, fingerprint, wall_now);
                    writer
                        .store()
                        .write_cleanup_witness(&witness)
                        .map_err(halted("restore witness"))?;
                    reader.persisted_at = Some(wall_now);
                    reader.observation = Some(witness);
                    if self.reader_config.is_some_and(|c| c.dormant) {
                        reader.capture = None;
                        reader.dormant = Some(now + DORMANT_POLL);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn tend<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
    ) -> Result<(), WriterAlarm> {
        self.tend_at(writer, SystemTime::now())
    }

    pub fn tend_at<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        wall_now: SystemTime,
    ) -> Result<(), WriterAlarm> {
        if self.reader_config.is_none() {
            return self.tend_legacy(writer);
        }
        self.observe_clock(wall_now)?;
        let verified = self.validation_result(writer, wall_now)?;
        let now = Instant::now();
        let captured: HashSet<_> = self
            .readers
            .iter()
            .filter(|r| r.captured_any)
            .map(|r| source_key(&r.source))
            .collect();
        for index in 0..self.readers.len() {
            let reader = &self.readers[index];
            if let Some(until) = reader.watch_until {
                if let Ok(fp) = Fingerprint::path(&reader.source) {
                    let through = writer
                        .store()
                        .cursor(&reader.source)
                        .map_or(0, |c| c.captured_through);
                    if fp.len > through {
                        let source = reader.source.clone();
                        self.unretire(writer, &source)?;
                        writer.alarm(WriterAlarm::RetiredSourceGrew { source });
                    } else if now >= until {
                        self.readers[index].capture = None;
                    }
                }
                continue;
            }
            if reader.capture.is_none() && (reader.pending.is_some() || reader.drain_to.is_some()) {
                return Err(halt("closed reader retains a capture obligation"));
            }
            let key = source_key(&reader.source);
            let Some(next) = self.rotation.successors.get(&key).cloned() else {
                continue;
            };
            if reader.dormant.is_some_and(|deadline| now < deadline) && verified != Some(index) {
                continue;
            }
            let fp = match Fingerprint::path(&reader.source) {
                Ok(fp) => fp,
                Err((SourceAnomalyKind::Missing, _)) => {
                    self.cleanup_missing(writer, index, wall_now)?;
                    continue;
                }
                Err((kind, detail)) => return Err(halt(format!("source {kind:?}: {detail}"))),
            };
            let cursor = writer
                .store()
                .cursor(&self.readers[index].source)
                .cloned()
                .ok_or_else(|| halt("reader cursor missing"))?;
            if fp.modified > wall_now {
                return Err(halt("source mtime is in the future"));
            }
            if fp.len < cursor.captured_through {
                return Err(halt("source shrank below durable cursor"));
            }
            if fp.len > cursor.captured_through {
                self.wake(writer, index)?;
                let reader = &mut self.readers[index];
                if reader
                    .rotated_at
                    .is_some_and(|at| now - at > OLD_GROWTH_ALARM)
                    && !reader.growth_alarmed
                {
                    reader.growth_alarmed = true;
                    writer.alarm(WriterAlarm::SourceStillGrowing {
                        source: reader.source.clone(),
                    });
                }
                reader.idle = None;
                reader.quiet = None;
                reader.observation = None;
                continue;
            }
            if !drained(&self.readers[index], &cursor) {
                self.readers[index].idle = None;
                self.readers[index].quiet = None;
                continue;
            }
            let reader = &mut self.readers[index];
            if reader
                .observation
                .as_ref()
                .is_some_and(|w| wall_now < w.observed_at)
            {
                reader.observation = None;
                reader.idle = None;
                reader.quiet = None;
            }
            let unchanged = reader
                .observation
                .as_ref()
                .is_some_and(|w| w.bound_to(&cursor) && w.fingerprint == fp);
            if unchanged && let Some(observation) = &mut reader.observation {
                if wall_now >= observation.observed_at {
                    observation.observed_at = wall_now;
                }
            }
            if reader.idle.as_ref().is_none_or(|(held, _)| *held != fp) {
                reader.quiet = None;
            }
            if !unchanged {
                reader.observation = None;
            }
            let idle_since = match &reader.idle {
                Some((held, since)) if *held == fp => *since,
                _ => now,
            };
            reader.idle = Some((fp.clone(), idle_since));
            let successor_captured = captured.contains(&source_key(&next.source));
            let quiet_since = match reader.quiet {
                Some((len, since)) if len == fp.len && successor_captured => since,
                _ => now,
            };
            reader.quiet = Some((fp.len, quiet_since));
            let retire =
                next.proof.is_some() && successor_captured && now - quiet_since >= RETIRE_QUIET;
            let sleep = self.reader_config.is_some_and(|c| c.dormant)
                && reader.dormant.is_none()
                && now - idle_since >= DORMANT_QUIET;
            let refresh = reader.persisted_at.is_none_or(|at| {
                wall_now
                    .duration_since(at)
                    .is_ok_and(|gap| gap >= WITNESS_REFRESH)
            });
            if retire || sleep || refresh || !unchanged {
                if verified == Some(index) {
                    if retire {
                        writer
                            .store()
                            .set_retired(&reader.source, true)
                            .map_err(halted("retire"))?;
                        reader.watch_until = Some(now + RETIRED_WATCH);
                        reader.dormant = None;
                        reader.capture = None;
                        reader.quiet = None;
                    } else if sleep {
                        reader.capture = None;
                        reader.dormant = Some(now + DORMANT_POLL);
                    }
                } else if self.validation.is_none() {
                    self.start_validation(cursor, fp, wall_now);
                }
            }
            if let Some(deadline) = &mut self.readers[index].dormant {
                *deadline = now + DORMANT_POLL;
            }
        }
        let count = self.readers.iter().filter(|r| r.reading()).count();
        if count > MAX_READERS && !self.readers_alarmed {
            writer.alarm(WriterAlarm::TooManyReaders { count });
        }
        self.readers_alarmed = count > MAX_READERS;
        writer.reconcile_reader_count(count);
        Ok(())
    }

    pub(super) fn wake<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        index: usize,
    ) -> Result<(), WriterAlarm> {
        let reader = &mut self.readers[index];
        if reader.dormant.is_some() {
            let cursor = writer
                .store()
                .cursor(&reader.source)
                .ok_or_else(|| halt("dormant cursor missing"))?;
            reader.capture = Some(reopen(cursor).map_err(Reopen::halt)?);
            reader.dormant = None;
            reader.idle = None;
        }
        Ok(())
    }

    fn observe_clock(&mut self, now: SystemTime) -> Result<(), WriterAlarm> {
        if self.last_wall.replace(now).is_some_and(|last| now < last) {
            for reader in &mut self.readers {
                reader.observation = None;
                reader.idle = None;
                reader.quiet = None;
            }
            self.validation = None;
            return Err(halt("wall clock regressed; cleanup evidence invalidated"));
        }
        Ok(())
    }

    pub(super) fn cleanup_missing<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        index: usize,
        now: SystemTime,
    ) -> Result<(), WriterAlarm> {
        let reader = &mut self.readers[index];
        let cursor = writer
            .store()
            .cursor(&reader.source)
            .ok_or_else(|| halt("cleanup cursor missing"))?;
        let next = self.rotation.successors.get(&source_key(&reader.source));
        let observation = reader
            .observation
            .as_ref()
            .filter(|w| w.bound_to(cursor) && w.eligible(now));
        if !next.is_some_and(|next| next.proof.is_none())
            || !drained(reader, cursor)
            || !matches!(
                Fingerprint::path(&reader.source),
                Err((SourceAnomalyKind::Missing, _))
            )
        {
            return Err(halt("missing source has no fresh drained cleanup evidence"));
        }
        let observation =
            observation.ok_or_else(|| halt("missing source has no fresh cleanup observation"))?;
        if let Some(capture) = &reader.capture {
            let fp = capture
                .verify_eof(cursor.captured_through, &cursor.prefix_hash)
                .map_err(|e| halt(format!("missing open FD: {e}")))?;
            if fp.modified != observation.fingerprint.modified {
                return Err(halt("missing open FD was modified after observation"));
            }
        }
        let alarm = WriterAlarm::CleanupRetired {
            source: reader.source.clone(),
            offset: cursor.captured_through,
            modified: observation.fingerprint.modified,
            age: now
                .duration_since(observation.fingerprint.modified)
                .map_err(|_| halt("cleanup mtime is in the future"))?,
        };
        writer
            .store()
            .set_retired(&reader.source, true)
            .map_err(halted("cleanup retire"))?;
        tracing::warn!(source = %reader.source.path.display(), ?alarm, anomaly = "Missing", "[tui_o] cleanup retired drained source");
        writer.alarm(alarm);
        reader.capture = None;
        reader.dormant = None;
        reader.watch_until = Some(Instant::now() + RETIRED_WATCH);
        Ok(())
    }

    fn start_validation(
        &mut self,
        cursor: Cursor,
        fingerprint: Fingerprint,
        observed_at: SystemTime,
    ) {
        let (send, result) = mpsc::channel();
        let snapshot = cursor.clone();
        #[cfg(test)]
        let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        #[cfg(test)]
        let done = ready.clone();
        tokio::task::spawn_blocking(move || {
            let verify = || {
                let capture = SourceCapture::reopen(
                    snapshot.source,
                    snapshot.captured_through,
                    &snapshot.prefix_hash,
                )?;
                capture.verify_eof(snapshot.captured_through, &snapshot.prefix_hash)
            };
            let _ = send.send(verify());
            #[cfg(test)]
            done.store(true, std::sync::atomic::Ordering::Release);
        });
        #[cfg(test)]
        {
            self.jobs_started += 1;
        }
        self.validation = Some(Validation {
            cursor,
            fingerprint,
            observed_at,
            result,
            #[cfg(test)]
            ready,
        });
    }

    pub(super) fn validation_result<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
        now: SystemTime,
    ) -> Result<Option<usize>, WriterAlarm> {
        #[cfg(test)]
        if self.hold_validation {
            return Ok(None);
        }
        let Some(job) = &self.validation else {
            return Ok(None);
        };
        let result = match job.result.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(halt("prefix validation worker disappeared"));
            }
        };
        let Some(job) = self.validation.take() else {
            return Err(halt("prefix validation job disappeared"));
        };
        let Some(index) = self
            .readers
            .iter()
            .position(|r| r.source == job.cursor.source)
        else {
            return Ok(None);
        };
        let cursor = writer
            .store()
            .cursor(&job.cursor.source)
            .ok_or_else(|| halt("verification cursor missing"))?;
        if cursor != &job.cursor || !drained(&self.readers[index], cursor) || now < job.observed_at
        {
            return Ok(None);
        }
        let current = match Fingerprint::path(&job.cursor.source) {
            Ok(fp) => fp,
            Err((SourceAnomalyKind::Missing, _)) => return Ok(None),
            Err((kind, detail)) => return Err(halt(format!("source {kind:?}: {detail}"))),
        };
        if current != job.fingerprint {
            return Ok(None);
        }
        let fp = result.map_err(|e| halt(format!("prefix validation: {e}")))?;
        if fp != job.fingerprint {
            return Ok(None);
        }
        let observation = witness(cursor, fp, job.observed_at);
        writer
            .store()
            .write_cleanup_witness(&observation)
            .map_err(halted("cleanup witness refresh"))?;
        let reader = &mut self.readers[index];
        reader.persisted_at = Some(job.observed_at);
        if reader.observation.as_ref().is_none_or(|old| {
            old.fingerprint != observation.fingerprint || !old.bound_to(&job.cursor)
        }) {
            reader.observation = Some(observation);
        }
        Ok(Some(index))
    }

    /// Retires quiet drained old sources whose hop is proven, alarms on growth and reader count, and
    /// watches retired ones.
    fn tend_legacy<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
        &mut self,
        writer: &mut ChannelWriter<P, L, A>,
    ) -> Result<(), WriterAlarm> {
        let now = Instant::now();
        let captured = self.readers.iter().filter(|r| r.captured_any);
        let captured: HashSet<String> = captured.map(|r| source_key(&r.source)).collect();
        let mut grew_back = Vec::new();
        for reader in &mut self.readers {
            let Some(capture) = reader.capture.as_ref() else {
                continue;
            };
            let Ok(len) = capture.file_len() else {
                continue;
            };
            let through = capture.captured_through();
            if let Some(until) = reader.watch_until {
                if len > through {
                    grew_back.push(reader.source.clone());
                } else if now >= until {
                    reader.capture = None;
                }
                continue;
            }
            let successor = self.rotation.successors.get(&source_key(&reader.source));
            let Some(successor) = successor else {
                continue;
            };
            let successor_captured = captured.contains(&source_key(&successor.source));
            // Only a provider record shows the old source stopped; a drained quiet one stays read.
            let proven = successor.proof.is_some();
            let drained = through == len && reader.pending.is_none();
            let grew = reader.quiet.is_some_and(|(held, _)| held != len);
            let late = reader
                .rotated_at
                .is_some_and(|at| now - at > OLD_GROWTH_ALARM);
            if grew && late && !reader.growth_alarmed {
                reader.growth_alarmed = true;
                let source = reader.source.clone();
                writer.alarm(WriterAlarm::SourceStillGrowing { source });
            }
            let since = match reader.quiet {
                Some((held, since)) if held == len && successor_captured => since,
                _ => now,
            };
            reader.quiet = Some((len, since));
            if proven && successor_captured && drained && now - since >= RETIRE_QUIET {
                let retired = writer.store().set_retired(&reader.source, true);
                retired.map_err(halted("retire"))?;
                (reader.watch_until, reader.quiet) = (Some(now + RETIRED_WATCH), None);
            }
        }
        for source in grew_back {
            self.unretire(writer, &source)?;
            writer.alarm(WriterAlarm::RetiredSourceGrew { source });
        }
        let count = self.readers.iter().filter(|r| r.reading()).count();
        if count > MAX_READERS && !self.readers_alarmed {
            writer.alarm(WriterAlarm::TooManyReaders { count });
        }
        self.readers_alarmed = count > MAX_READERS;
        writer.reconcile_reader_count(count);
        Ok(())
    }
}

fn witness(cursor: &Cursor, fingerprint: Fingerprint, observed_at: SystemTime) -> CleanupWitness {
    CleanupWitness {
        source: cursor.source.clone(),
        captured_through: cursor.captured_through,
        prefix_hash: cursor.prefix_hash.clone(),
        fingerprint,
        observed_at,
    }
}

fn drained(reader: &Reader, cursor: &Cursor) -> bool {
    !cursor.retired
        && reader.pending.is_none()
        && reader.drain_to.is_none()
        && reader.capture.as_ref().is_none_or(|c| {
            c.captured_through() == cursor.captured_through
                && c.read_through() == cursor.captured_through
        })
}

fn missing_reader(reader: &Reader) -> bool {
    reader.capture.is_none() && reader.watch_until.is_none()
}
