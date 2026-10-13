//! Shadow observation of channel ingress ownership: the gateway hook queues an allowlisted event's
//! ids and one worker judges them and only logs; no verdict returns, drops, forwards or acks input.

mod telemetry;
mod worker;

use std::collections::BTreeSet;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use poise::serenity_prelude as serenity;
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::time::Instant;

use crate::db::o_channel_homes::HomeProvider;
use crate::services::provider::ProviderKind;
use telemetry::{Counts, Sink};
use worker::{HomeSource, Scope};

/// Descriptors the worker has not taken yet; past this the newest is dropped and counted.
const QUEUE: usize = 256;

/// The boot snapshot that turns the shadow on: the switch and a non-empty channel allowlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ShadowConfig {
    channels: BTreeSet<u64>,
    instance_id: String,
}

impl ShadowConfig {
    /// `None` unless the switch is `true` and the allowlist names a channel; no wildcard exists.
    pub(super) fn of(config: &crate::config::Config) -> Option<Self> {
        let runtime = &config.runtime;
        if runtime.channel_ingress_shadow_enabled != Some(true) {
            return None;
        }
        let listed = runtime.channel_ingress_shadow_channels.iter().flatten();
        let channels: BTreeSet<u64> = listed.copied().filter(|channel| *channel != 0).collect();
        let instance_id = config.cluster.instance_id.as_deref().unwrap_or_default();
        (!channels.is_empty()).then(|| Self {
            channels,
            instance_id: instance_id.trim().to_owned(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum EventKind {
    Message,
    Command,
    Autocomplete,
    Component,
    Modal,
}

/// The ids of one raw event, copied before any branch of the event handler runs.
#[derive(Clone, Copy, Debug)]
pub(super) struct Descriptor {
    sequence: u64,
    kind: EventKind,
    channel: NonZeroU64,
    source_id: u64,
    application_id: Option<u64>,
    received: Instant,
}

/// An ingress event's kind, actual channel, source id and application id; `None` for events that
/// carry no channel ingress.
fn describe(event: &serenity::FullEvent) -> Option<(EventKind, NonZeroU64, u64, Option<u64>)> {
    use serenity::Interaction as I;
    let (kind, channel, source, application) = match event {
        serenity::FullEvent::Message { new_message: m } => {
            (EventKind::Message, m.channel_id.get(), m.id.get(), None)
        }
        serenity::FullEvent::InteractionCreate { interaction } => {
            let (kind, channel, id, application) = match interaction {
                I::Command(c) => (EventKind::Command, c.channel_id, c.id, c.application_id),
                I::Autocomplete(c) => (
                    EventKind::Autocomplete,
                    c.channel_id,
                    c.id,
                    c.application_id,
                ),
                I::Component(c) => (EventKind::Component, c.channel_id, c.id, c.application_id),
                I::Modal(m) => (EventKind::Modal, m.channel_id, m.id, m.application_id),
                _ => return None,
            };
            (kind, channel.get(), id.get(), Some(application.get()))
        }
        _ => return None,
    };
    Some((kind, NonZeroU64::new(channel)?, source, application))
}

/// One provider gateway's live shadow, owned by its `Data`; dropping it ends the worker.
pub(super) struct IngressShadow {
    tx: mpsc::Sender<Descriptor>,
    channels: BTreeSet<u64>,
    counts: Arc<Counts>,
}

impl IngressShadow {
    /// Never waits: a full or closed queue drops the descriptor and counts it.
    fn submit(&self, event: &serenity::FullEvent) {
        let Some((kind, channel, source_id, application_id)) = describe(event) else {
            self.counts.excluded.fetch_add(1, Ordering::AcqRel);
            return;
        };
        if !self.channels.contains(&channel.get()) {
            return;
        }
        let sequence = self.counts.submitted.fetch_add(1, Ordering::AcqRel) + 1;
        let descriptor = Descriptor {
            sequence,
            kind,
            channel,
            source_id,
            application_id,
            received: Instant::now(),
        };
        let dropped = match self.tx.try_send(descriptor) {
            Ok(()) => return,
            Err(TrySendError::Full(_)) => &self.counts.dropped_full,
            Err(TrySendError::Closed(_)) => &self.counts.dropped_closed,
        };
        dropped.fetch_add(1, Ordering::AcqRel);
    }
}

/// The gateway hook, called before any branch of the event handler; a no-op while off.
pub(super) fn observe(shadow: Option<&IngressShadow>, event: &serenity::FullEvent) {
    if let Some(shadow) = shadow {
        shadow.submit(event);
    }
}

/// What a started shadow needs from the runtime; built only once the switch is on.
pub(super) struct Begin {
    pub(super) bot: String,
    pub(super) generation: u64,
    pub(super) pool: Option<sqlx::PgPool>,
}

/// Starts the shadow when the boot config turns it on for a home provider; otherwise returns
/// before `begin` runs or any queue, pool or task exists.
pub(super) fn start_if_enabled(
    config: Option<&ShadowConfig>,
    provider: &ProviderKind,
    begin: impl FnOnce() -> Begin,
) -> Option<IngressShadow> {
    let config = config?;
    let Some(provider) = HomeProvider::parse(provider.as_str()) else {
        tracing::warn!(
            provider = provider.as_str(),
            "ingress shadow is for claude and codex only"
        );
        return None;
    };
    let Begin {
        bot,
        generation,
        pool,
    } = begin();
    let scope = Scope {
        provider,
        bot,
        generation,
        instance_id: config.instance_id.clone(),
        allowlist: config.channels.iter().copied().collect(),
        build: None,
        sink: Sink::Log,
    };
    Some(spawn(
        &config.channels,
        scope,
        HomeSource::of(pool.as_ref()),
    ))
}

fn spawn(channels: &BTreeSet<u64>, scope: Scope, source: HomeSource) -> IngressShadow {
    let (tx, rx) = mpsc::channel(QUEUE);
    let counts = Arc::new(Counts::default());
    tokio::spawn(worker::run(scope, source, rx, Arc::clone(&counts)));
    IngressShadow {
        tx,
        channels: channels.clone(),
        counts,
    }
}

#[cfg(test)]
pub(super) mod shadow_tests;
