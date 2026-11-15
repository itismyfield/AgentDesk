//! Which spooled records are owed. Live capture and spool resume share this one decision; only
//! the collector that receives the derived work differs.

use std::collections::VecDeque;

use super::fork_lineage::{Class, RowIds};
use super::{
    BindingEvents, BindingRecord, BindingTarget, Boundary, CaptureBatch, CleanupWitness, Derived,
    Fingerprint, Instant, SourceCapture, Sources, SystemTime, UnitDeriver, claude_boundary,
};
use crate::services::tui_o::shadow::{CapturedRecord, SourceId};
use crate::services::tui_o::store::rotation::SourceLink;
use crate::services::tui_o::store::spool::source_key;

/// Receives every owed record with its stored source, even when derivation yields nothing.
pub trait Owe {
    fn owe(&mut self, source: &SourceId, record: &CapturedRecord, derived: Vec<Derived>);
    fn finish_replay(
        &mut self,
        ledger: &crate::services::tui_o::store::ledger::LedgerState,
    ) -> super::super::ReplaySummary;
}

impl Owe for VecDeque<Derived> {
    fn owe(&mut self, _: &SourceId, _: &CapturedRecord, derived: Vec<Derived>) {
        self.extend(derived);
    }

    fn finish_replay(
        &mut self,
        ledger: &crate::services::tui_o::store::ledger::LedgerState,
    ) -> super::super::ReplaySummary {
        super::super::replay::filter(ledger, |keep| self.retain(keep))
    }
}

impl<B: BindingEvents> Sources<B> {
    /// Derives a record unless the source's boundary withholds it or its parent already has it.
    pub(super) fn owe(
        &mut self,
        deriver: &mut UnitDeriver,
        owed: &mut impl Owe,
        source: &SourceId,
        record: &CapturedRecord,
        identity_version: u32,
    ) {
        #[cfg(unix)]
        if let Some(native) = &mut self.native {
            native.append(source, record);
        }
        let key = source_key(source);
        let Some(link) = self.rotation.links.get(&key).cloned() else {
            owed.owe(
                source,
                record,
                self.derive_record(deriver, source, record, identity_version),
            );
            return;
        };
        match link.boundary {
            Boundary::Pending { .. } => {}
            Boundary::Owed { from } => {
                if record.start >= from {
                    match self.masked(&link, record) {
                        Ok(false) => owed.owe(
                            source,
                            record,
                            self.derive_record(deriver, source, record, identity_version),
                        ),
                        Ok(true) => {}
                        Err(reason) => owed.owe(
                            source,
                            record,
                            vec![Derived::Blocked {
                                reason: reason.into(),
                            }],
                        ),
                    }
                }
            }
            Boundary::Undecided => {
                let row = RowIds::of(self.provider, &record.line);
                if row.as_ref().is_some_and(RowIds::is_empty) {
                    return;
                }
                let parent = link.parent.as_ref();
                let lineage = parent.and_then(|parent| self.lineage.get(&source_key(parent)));
                let class = row.zip(lineage).map(|(row, lineage)| lineage.class(&row));
                match class {
                    Some(Class::Inherited) => {
                        self.prefix_seen.insert(key);
                    }
                    Some(Class::New) if self.prefix_seen.contains(&key) => {
                        owed.owe(
                            source,
                            record,
                            self.derive_record(deriver, source, record, identity_version),
                        );
                        self.decide(&key, Boundary::Owed { from: record.start });
                    }
                    _ => self.pend(&key, record.start),
                }
            }
        }
    }

    fn derive_record(
        &self,
        deriver: &mut UnitDeriver,
        source: &SourceId,
        record: &CapturedRecord,
        version: u32,
    ) -> Vec<Derived> {
        deriver.enter_source(source);
        let mut ended = deriver.restart_before(source, record);
        if !deriver.input_notices_enabled() {
            ended.extend(deriver.derive_with_version(record, version));
            return ended;
        }
        let origin = if super::super::input_notices::needs_origin(record) {
            self.notice_origin(source, record)
        } else {
            Err("input notice candidate absent".into())
        };
        ended.extend(deriver.derive_scoped(record, source, version, origin));
        ended
    }

    pub(crate) fn notice_origin(
        &self,
        source: &SourceId,
        record: &CapturedRecord,
    ) -> Result<super::super::input_notices::NoticeOrigin, String> {
        let events = self.bindings.binding_events_since(self.channel, 0)?;
        let matches_source = |event: &super::BindingEvent| match &event.record {
            BindingRecord::Bound {
                new: BindingTarget::Source(bound),
                ..
            } => bound == source,
            BindingRecord::Bound {
                new: BindingTarget::Pending { .. },
                ..
            } => events.iter().any(|resolved| {
                matches!(&resolved.record, BindingRecord::Resolved { resolves_seq, source: bound }
                    if *resolves_seq == event.seq && bound == source)
            }),
            _ => false,
        };
        // Root sources predate rotation links; their first exact binding is the attachment proof.
        let event = match self.rotation.link(source) {
            Some(link) => events.iter().find(|event| event.seq == link.seq),
            None => events.iter().find(|event| matches_source(event)),
        }
        .ok_or("input notice original binding is missing")?;
        if !matches_source(event)
            || event.channel_id != self.channel
            || event.provider != self.provider
            || event.tmux_session.trim().is_empty()
            || event.execution_nonce.trim().is_empty()
            || event.seq == 0
        {
            return Err("input notice binding belongs to another owner".into());
        }
        Ok(super::super::input_notices::NoticeOrigin {
            record: crate::services::tui_o::shadow::SourceRange {
                source: source.clone(),
                start: record.start,
                end: record.end,
            },
            logical_session: event.tmux_session.clone(),
            binding_seq: event.seq,
            execution_nonce: event.execution_nonce.clone(),
        })
    }

    /// Masks only identities consumed within the parent's publishing responsibility.
    /// Root/adopted parents retain their existing from-zero policy.
    fn masked(&self, link: &SourceLink, record: &CapturedRecord) -> Result<bool, &'static str> {
        if link.hot_candidate.is_some() {
            let compare = || {
                let parent = link.parent.as_ref()?;
                let from = claude_boundary::parent_from(&self.rotation, parent)?;
                let lineage = self.lineage.get(&source_key(parent))?;
                let row = RowIds::of(self.provider, &record.line)?;
                if row.is_empty() {
                    return Some(false);
                }
                match lineage.native_class(self.provider, &record.line, from) {
                    Class::Inherited => Some(true),
                    Class::New => Some(false),
                    Class::Unidentified => None,
                }
            };
            return compare().ok_or("automatic parent content comparison failed");
        }
        let Some(parent) = link.parent.as_ref() else {
            return Ok(false);
        };
        let parent_link = self.rotation.link(parent).map(|l| &l.boundary);
        let from = match parent_link {
            None => 0,
            Some(Boundary::Owed { from }) => *from,
            _ => return Ok(false),
        };
        let Some(lineage) = self.lineage.get(&source_key(parent)) else {
            return Ok(false);
        };
        Ok(RowIds::of(self.provider, &record.line).is_some_and(|row| {
            !row.is_empty() && lineage.class_from(&row, from) == Class::Inherited
        }))
    }
}

pub(super) struct Reader {
    pub(super) source: SourceId,
    /// `None` once a retired source's watch has ended or its file is gone.
    pub(super) capture: Option<SourceCapture>,
    /// A batch the full spool refused; it is retried before the source is read again.
    pub(super) pending: Option<CaptureBatch>,
    pub(super) captured_any: bool,
    /// Last length seen and since when it held after the successor's first record.
    pub(super) quiet: Option<(u64, Instant)>,
    pub(super) rotated_at: Option<Instant>,
    pub(super) growth_alarmed: bool,
    /// Set while retired: the source is only stat-watched until then.
    pub(super) watch_until: Option<Instant>,
    /// The old source's length at its rotation; its successor waits until it is read and spooled.
    pub(super) drain_to: Option<u64>,
    pub(super) dormant: Option<Instant>,
    pub(super) idle: Option<(Fingerprint, Instant)>,
    pub(super) observation: Option<CleanupWitness>,
    pub(super) persisted_at: Option<SystemTime>,
    pub(super) parked: bool,
}
impl Reader {
    pub(super) fn new(source: SourceId, capture: Option<SourceCapture>) -> Self {
        Self {
            source,
            capture,
            pending: None,
            captured_any: false,
            quiet: None,
            rotated_at: None,
            growth_alarmed: false,
            watch_until: None,
            drain_to: None,
            dormant: None,
            idle: None,
            observation: None,
            persisted_at: None,
            parked: false,
        }
    }

    pub(super) fn reading(&self) -> bool {
        self.capture.is_some() && self.watch_until.is_none()
    }

    pub(super) fn backlog_end(&self) -> Option<u64> {
        let capture = self.capture.as_ref().filter(|_| self.watch_until.is_none());
        capture.and_then(|capture| capture.file_len().ok())
    }
}

#[derive(Default)]
pub(super) struct NativeSources {
    #[cfg(unix)]
    seeds: std::collections::HashMap<
        SourceId,
        Result<super::super::input_facts::native::Projection, String>,
    >,
    #[cfg(unix)]
    revision: u64,
}

pub(super) fn open_native(
    native: &mut Option<NativeSources>,
    channel: u64,
    provider: crate::services::tui_o::shadow::ShadowProvider,
    cursor: &crate::services::tui_o::store::spool::Cursor,
) -> std::io::Result<SourceCapture> {
    #[cfg(unix)]
    if let Some(native) = native {
        let binding = crate::services::tui_o::shadow::SourceBinding {
            channel_id: channel,
            provider,
            source: cursor.source.clone(),
        };
        let (capture, seed) = super::super::input_facts::native::Projection::reopen(
            binding,
            cursor.captured_through,
            &cursor.prefix_hash,
        )?;
        native.seeds.insert(cursor.source.clone(), seed);
        return Ok(capture);
    }
    let _ = (native, channel, provider);
    SourceCapture::reopen(
        cursor.source.clone(),
        cursor.captured_through,
        &cursor.prefix_hash,
    )
}

pub(super) fn reopen_native(
    native: &mut Option<NativeSources>,
    channel: u64,
    provider: crate::services::tui_o::shadow::ShadowProvider,
    cursor: &crate::services::tui_o::store::spool::Cursor,
) -> Result<SourceCapture, super::Reopen> {
    open_native(native, channel, provider, cursor).map_err(|error| match error.kind() {
        std::io::ErrorKind::InvalidData => super::Reopen::Changed,
        _ => super::Reopen::Unread(error),
    })
}

#[cfg(unix)]
impl NativeSources {
    fn append(&mut self, source: &SourceId, record: &CapturedRecord) {
        if let Some(seed) = self.seeds.get_mut(source) {
            if let Ok(projection) = seed
                && let Err(error) = projection.append(record)
            {
                *seed = Err(error);
            }
        }
    }

    fn observe(
        &self,
        channel: u64,
        source: &SourceId,
        origin: &super::super::input_notices::NoticeOrigin,
        snapshot: super::super::actor::native_input::Snapshot,
    ) -> Result<
        (
            super::super::input_facts::native::Observation,
            Vec<super::super::input_facts::native::Barrier>,
        ),
        String,
    > {
        use super::super::input_facts::native::{Observation, registrations_fingerprint};
        let projection = self
            .seeds
            .get(source)
            .ok_or("native prefix absent")?
            .as_ref()
            .map_err(Clone::clone)?;
        let binding = crate::services::tui_o::shadow::SourceBinding {
            channel_id: channel,
            provider: crate::services::tui_o::shadow::ShadowProvider::Claude,
            source: source.clone(),
        };
        let registrations: Vec<_> = snapshot
            .registrations
            .into_iter()
            .filter(|r| {
                r.ticket.channel_id == channel && (r.ticket.source.is_none() || r.matches(&binding))
            })
            .collect();
        let analysis = projection.analyze(&registrations)?;
        Ok((
            Observation {
                binding,
                generation: snapshot.generation,
                binding_seq: origin.binding_seq,
                revision: self.revision,
                through: projection.through(),
                registrations_fingerprint: registrations_fingerprint(&registrations)?,
                evidence: analysis.evidence,
            },
            analysis.barriers,
        ))
    }
}

impl<B: BindingEvents> Sources<B> {
    pub(crate) fn with_native_inputs(mut self, enabled: bool) -> Self {
        if enabled && self.provider == crate::services::tui_o::shadow::ShadowProvider::Claude {
            self.native = Some(NativeSources::default());
        }
        self
    }

    pub(super) fn reopen_native(
        &mut self,
        cursor: &crate::services::tui_o::store::spool::Cursor,
    ) -> Result<SourceCapture, super::Reopen> {
        reopen_native(&mut self.native, self.channel, self.provider, cursor)
    }

    #[cfg(unix)]
    fn native_origin(
        &self,
        source: &SourceId,
        record: &CapturedRecord,
        target: Option<&serde_json::Value>,
    ) -> Result<super::super::input_notices::NoticeOrigin, String> {
        let events = self.bindings.binding_events_since(self.channel, 0)?;
        let event = events.iter().find(|event| {
            let bound = match &event.record {
                BindingRecord::Bound { new: BindingTarget::Source(bound), .. } => bound == source,
                BindingRecord::Bound { new: BindingTarget::Pending { .. }, .. } => events.iter().any(|resolved| {
                    resolved.channel_id == self.channel && resolved.provider == self.provider
                        && matches!(&resolved.record, BindingRecord::Resolved { resolves_seq, source: bound }
                            if *resolves_seq == event.seq && bound == source)
                }),
                _ => false,
            };
            bound && event.channel_id == self.channel && event.provider == self.provider
                && event.seq != 0 && !event.tmux_session.trim().is_empty()
                && !event.execution_nonce.trim().is_empty()
                && target.is_none_or(|target| target["owner"] == event.tmux_session
                    && target["execution_nonce"] == event.execution_nonce)
        }).ok_or("native ticket binding is missing")?;
        Ok(super::super::input_notices::NoticeOrigin {
            record: crate::services::tui_o::shadow::SourceRange {
                source: source.clone(),
                start: record.start,
                end: record.end,
            },
            logical_session: event.tmux_session.clone(),
            binding_seq: event.seq,
            execution_nonce: event.execution_nonce.clone(),
        })
    }

    #[cfg(unix)]
    fn native_idle(&self, source: &SourceId) -> bool {
        // The typed port rechecks the latest binding; Current is a resume-time history hint.
        self.readers
            .iter()
            .find(|r| &r.source == source)
            .is_some_and(|reader| {
                reader.pending.is_none()
                    && reader.capture.as_ref().is_some_and(|capture| {
                        self.native
                            .as_ref()
                            .and_then(|n| n.seeds.get(source))
                            .is_some_and(|seed| {
                                seed.as_ref().is_ok_and(|seed| {
                                    seed.idle()
                                        && seed.through() == capture.captured_through()
                                        && capture.read_through() == seed.through()
                                        && capture.file_len().ok() == Some(seed.through())
                                })
                            })
                    })
            })
    }

    #[cfg(unix)]
    pub(crate) async fn offer_native(
        &mut self,
        port: &super::super::actor::native_input::NativePort,
    ) {
        port.inner.retry().await;
        let sources: Vec<_> = self
            .readers
            .iter()
            .filter(|r| !self.suspension.contains(&r.source))
            .map(|r| r.source.clone())
            .collect();
        for source in sources {
            let Some(native) = &mut self.native else {
                return;
            };
            native.revision = native.revision.saturating_add(1);
            let through = match native.seeds.get(&source) {
                Some(Ok(seed)) => seed.through(),
                _ => continue,
            };
            let record = CapturedRecord {
                start: 0,
                end: through,
                line: Vec::new(),
            };
            let Ok(origin) = self.native_origin(&source, &record, None) else {
                continue;
            };
            let Ok(snapshot) = port.inner.snapshot(origin.clone()).await else {
                continue;
            };
            let Some(native) = self.native.as_ref() else {
                continue;
            };
            let Ok((observed, barriers)) = native.observe(self.channel, &source, &origin, snapshot)
            else {
                continue;
            };
            for proof in &barriers {
                let Some(target) = proof.ticket.source.as_ref().map(|s| &s.target) else {
                    continue;
                };
                let Ok(origin) = self.native_origin(&source, &record, Some(target)) else {
                    continue;
                };
                let Ok(snapshot) = port.inner.snapshot(origin.clone()).await else {
                    continue;
                };
                let Ok((mut current, mut current_barriers)) =
                    native.observe(self.channel, &source, &origin, snapshot)
                else {
                    continue;
                };
                let mut offered = observed.clone();
                offered.binding_seq = origin.binding_seq;
                offered.evidence.clear();
                current.evidence.clear();
                current_barriers
                    .retain(|p| p.namespace == proof.namespace && p.ticket.key == proof.ticket.key);
                if let Err(error) = port
                    .inner
                    .abstain(
                        offered,
                        current,
                        (vec![proof.clone()], current_barriers),
                        false,
                        origin,
                    )
                    .await
                {
                    tracing::debug!(channel=self.channel, %error, "native barrier held");
                }
            }
            for index in 0..observed.evidence.len() {
                let Some(target) = observed.evidence[index]
                    .ticket
                    .source
                    .as_ref()
                    .map(|s| &s.target)
                else {
                    continue;
                };
                let Ok(origin) = self.native_origin(&source, &record, Some(target)) else {
                    continue;
                };
                let mut offered = observed.clone();
                offered.binding_seq = origin.binding_seq;
                let Ok(snapshot) = port.inner.snapshot(origin.clone()).await else {
                    break;
                };
                let Ok((current, _)) = native.observe(self.channel, &source, &origin, snapshot)
                else {
                    break;
                };
                if let Err(error) = port
                    .inner
                    .offer(offered, current, index, origin.clone())
                    .await
                {
                    tracing::debug!(channel=self.channel, %error, "native input offer held");
                }
            }
            if self.native_idle(&source) {
                let Ok(events) = self.bindings.binding_events_since(self.channel, 0) else {
                    continue;
                };
                let Some(binding) = events.iter().rev().find(|e| {
                    e.channel_id == self.channel
                        && e.provider == self.provider
                        && matches!(e.record, BindingRecord::Bound { .. })
                }) else {
                    continue;
                };
                let target = serde_json::json!({"owner":binding.tmux_session,"execution_nonce":binding.execution_nonce});
                let Ok(origin) = self.native_origin(&source, &record, Some(&target)) else {
                    continue;
                };
                let Ok(snapshot) = port.inner.snapshot(origin.clone()).await else {
                    continue;
                };
                let Some(native) = &self.native else {
                    continue;
                };
                let Ok((mut current, _)) = native.observe(self.channel, &source, &origin, snapshot)
                else {
                    continue;
                };
                current.evidence.clear();
                if let Err(error) = port
                    .inner
                    .abstain(
                        current.clone(),
                        current,
                        (Vec::new(), Vec::new()),
                        self.native_idle(&source),
                        origin,
                    )
                    .await
                {
                    tracing::debug!(channel=self.channel, %error, "native Idle handoff held");
                }
            }
        }
    }
}
