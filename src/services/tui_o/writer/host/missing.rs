//! A missing-source host keeps its claim until stable evidence changes or it is stopped.
use super::super::binding::{BindingRecord, BindingTarget};
use super::super::{ProducerObservation, ProducerPresence};
use super::*;
use crate::services::tui_o::shadow::SourceId;

type Binding = Option<(SourceId, String)>;
pub(super) struct Observation {
    producer: Option<ProducerObservation>,
    binding: Option<Binding>,
}
impl Observation {
    pub(super) async fn read<I: HostIo>(host: &Hosted<'_, I>) -> Self {
        Self {
            binding: binding(host),
            producer: host
                .io
                .producer_observation(host.channel, host.provider)
                .await,
        }
    }
    pub(super) fn absent(&self) -> bool {
        self.producer
            .as_ref()
            .is_some_and(|p| p.presence == ProducerPresence::Absent)
    }
    pub(super) async fn resume<I: HostIo>(
        mut self,
        host: &Hosted<'_, I>,
        cause: &StopCause,
        known: &[SourceId],
    ) -> Option<ChannelStore> {
        let WriterAlarm::HaltedMissingSource { source, .. } = &cause.alarm else {
            return None;
        };
        if cause.io.is_some() {
            return None;
        }
        let wait = async {
            loop {
                if std::fs::metadata(&source.path).is_ok() {
                    break;
                }
                let next = Self::read(host).await;
                let presence_changed = match (&self.producer, &next.producer) {
                    (Some(old), Some(new)) => {
                        old.presence != new.presence
                            || (new.incarnation.is_some() && old.incarnation != new.incarnation)
                    }
                    (None, Some(new)) => new.presence == ProducerPresence::Absent,
                    _ => false,
                };
                let binding_changed = next.binding.as_ref().and_then(|b| b.as_ref()).is_some_and(
                    |(source, nonce)| match &self.binding {
                        Some(old) => old.as_ref().is_none_or(|(old_source, old_nonce)| {
                            old_source != source || (!nonce.is_empty() && old_nonce != nonce)
                        }),
                        None => !known.contains(source),
                    },
                );
                if presence_changed || binding_changed {
                    break;
                }
                // Failed reads carry no evidence; the first valid reading establishes a baseline.
                if self.producer.is_none() {
                    self.producer = next.producer;
                }
                if self.binding.is_none() {
                    self.binding = next.binding;
                }
                tokio::time::sleep(actor::POLL_INTERVAL).await;
            }
        };
        unless_stopped(&host.stopping, wait).await?;
        match recover(host.runtime_root, host.channel, cause.unsent.as_ref()) {
            Ok(Recovered::Store(store)) => Some(store),
            Ok(Recovered::Fresh(_)) => {
                hold(&host.alarms, host.channel, "init missing on resume");
                None
            }
            Err(error) => {
                hold(&host.alarms, host.channel, &error.detail);
                None
            }
        }
    }
}
fn binding<I: HostIo>(host: &Hosted<'_, I>) -> Option<Binding> {
    let events = host.bindings.binding_events_since(host.channel, 0).ok()?;
    if events.iter().enumerate().any(|(index, event)| {
        event.seq != index as u64 + 1
            || event.channel_id != host.channel
            || event.provider != host.provider
    }) {
        return None;
    }
    Some(
        events
            .into_iter()
            .filter_map(|event| match event.record {
                BindingRecord::Bound {
                    new: BindingTarget::Source(source),
                    ..
                }
                | BindingRecord::Resolved { source, .. } => Some((source, event.execution_nonce)),
                _ => None,
            })
            .next_back(),
    )
}
