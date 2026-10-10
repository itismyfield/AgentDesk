//! Raw canary evidence only; no delivery or turn authority lives here.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use serde::Serialize;
use uuid::Uuid;

use super::shadow::seal::TurnSpan;
use super::shadow::{ShadowProvider, SourceBinding, SourceId};

pub(crate) mod sink;
const CHANNEL_CAP: usize = 256;
const OPEN_CAP: usize = 128;
const ID_CAP: usize = 4096;
static OBSERVER: OnceLock<Arc<Observer>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct Boundary {
    pub seq: u64,
    pub at_us: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PhysicalKey {
    provider: ShadowProvider,
    channel_id: u64,
    source_id: SourceId,
    start_offset: u64,
    end_offset: u64,
    native_turn_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct LogicalKey {
    provider: ShadowProvider,
    channel_id: u64,
    provider_session_id: String,
    native_turn_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Placeholder {
    op_id: (Uuid, u64),
    origin: &'static str,
    operation: &'static str,
    reference: Option<(u64, u64)>,
    target_message_id: Option<u64>,
    input_message_id: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "event")]
pub(crate) enum Kind {
    #[serde(rename = "n1_turn_closed")]
    TurnClosed {
        physical_observation_key: PhysicalKey,
        logical_turn_key: Option<LogicalKey>,
        autonomous: bool,
        outcome: &'static str,
    },
    #[serde(rename = "n1_synthetic_create")]
    SyntheticCreate {
        phase: &'static str,
        site: &'static str,
        turn_source: &'static str,
        synthetic_kind: &'static str,
        turn_nonce: Option<String>,
        user_msg_id: u64,
        request_owner_user_id: u64,
        relay_owner_kind: &'static str,
        rebind_origin: bool,
    },
    #[serde(rename = "n1_placeholder_lifecycle")]
    PlaceholderLifecycle {
        #[serde(flatten)]
        request: Placeholder,
        phase: &'static str,
        message_id: Option<u64>,
        error_class: Option<&'static str>,
    },
    #[serde(rename = "n1_mode_confirmed")]
    ModeConfirmed {
        confirmation_window_start: Option<Boundary>,
        // Confirmation happened inside this window, not at the event's timestamp.
        actual_confirmed_at_us: Option<u64>,
        boundary_rule: &'static str,
    },
}

#[derive(Debug, Serialize)]
pub(crate) struct Event {
    boot_id: Uuid,
    channel_id: u64,
    provider: String,
    seq: u64,
    schema_version: u32,
    observed_at_us: u64,
    #[serde(flatten)]
    kind: Kind,
}

#[derive(Clone, Default, Debug, Serialize)]
pub(crate) struct Counters {
    closed_events: u64,
    aborted_events: u64,
    unknown_outcome_events: u64,
    create_attempt: u64,
    create_committed: u64,
    attempt: u64,
    succeeded_post: u64,
    succeeded_patch: u64,
    failed_or_uncertain: u64,
}

#[derive(Default)]
struct Channel {
    seq: u64,
    counters: Counters,
    open: BTreeSet<u64>,
    first_confirmation: Option<Option<Boundary>>,
}

pub(crate) struct Observer {
    boot_id: Uuid,
    started: Instant,
    next_op: AtomicU64,
    dropped: AtomicU64,
    sink_errors: AtomicU64,
    channels: Mutex<BTreeMap<u64, Channel>>,
    sender: SyncSender<Event>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum Snapshot {
    Unavailable,
    Available {
        schema_version: u32,
        boot_id: Uuid,
        channel_id: u64,
        seq_high_water: u64,
        counters: Counters,
        dropped_events: u64,
        sink_errors: u64,
        observer_unhealthy: bool,
        open_attempts: usize,
        open_attempts_complete: bool,
        first_confirmation: Option<Option<Boundary>>,
        confirmation_observed: bool,
    },
}

impl Observer {
    fn new(sender: SyncSender<Event>) -> Self {
        Self {
            boot_id: Uuid::new_v4(),
            started: Instant::now(),
            next_op: AtomicU64::new(1),
            dropped: AtomicU64::new(0),
            sink_errors: AtomicU64::new(0),
            channels: Mutex::new(BTreeMap::new()),
            sender,
        }
    }

    fn lost(&self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }

    fn health(&self) -> (u64, u64) {
        (
            self.dropped.load(Ordering::SeqCst),
            self.sink_errors.load(Ordering::SeqCst),
        )
    }

    fn at_us(&self) -> u64 {
        self.started.elapsed().as_micros().min(u64::MAX as u128) as u64
    }

    fn channel<'a>(
        &self,
        channels: &'a mut BTreeMap<u64, Channel>,
        id: u64,
    ) -> Option<&'a mut Channel> {
        if !channels.contains_key(&id) && channels.len() >= CHANNEL_CAP {
            self.lost();
            return None;
        }
        Some(channels.entry(id).or_default())
    }

    fn emit(&self, provider: &str, channel_id: u64, kind: Kind) {
        if provider.len() > 64 {
            self.lost();
            return;
        }
        let Ok(mut channels) = self.channels.try_lock() else {
            self.lost();
            return;
        };
        let Some(channel) = self.channel(&mut channels, channel_id) else {
            return;
        };
        match &kind {
            Kind::TurnClosed { outcome, .. } => {
                channel.counters.closed_events += 1;
                channel.counters.aborted_events += u64::from(*outcome == "aborted");
                channel.counters.unknown_outcome_events += u64::from(*outcome == "unknown");
            }
            Kind::SyntheticCreate { phase, .. } => match *phase {
                "attempt" => channel.counters.create_attempt += 1,
                _ => channel.counters.create_committed += 1,
            },
            Kind::PlaceholderLifecycle { request, phase, .. } => {
                if *phase == "attempt" {
                    channel.counters.attempt += 1;
                    if channel.open.len() >= OPEN_CAP {
                        self.lost();
                        return;
                    }
                    channel.open.insert(request.op_id.1);
                } else {
                    if !channel.open.remove(&request.op_id.1) {
                        self.lost();
                    }
                    match (*phase, request.operation) {
                        ("succeeded", "post_placeholder") => channel.counters.succeeded_post += 1,
                        ("succeeded", _) => channel.counters.succeeded_patch += 1,
                        _ => channel.counters.failed_or_uncertain += 1,
                    }
                }
            }
            Kind::ModeConfirmed {
                confirmation_window_start,
                ..
            } => {
                channel
                    .first_confirmation
                    .get_or_insert(*confirmation_window_start);
            }
        }
        channel.seq += 1;
        let event = Event {
            boot_id: self.boot_id,
            channel_id,
            provider: provider.into(),
            seq: channel.seq,
            schema_version: 2,
            observed_at_us: self.at_us(),
            kind,
        };
        if self.sender.try_send(event).is_err() {
            self.lost();
        }
    }

    fn snapshot(&self, channel_id: u64) -> Snapshot {
        let health = self.health();
        let Ok(channels) = self.channels.try_lock() else {
            return Snapshot::Unavailable;
        };
        let Some(channel) = channels.get(&channel_id) else {
            return Snapshot::Unavailable;
        };
        let snapshot = Snapshot::Available {
            schema_version: 2,
            boot_id: self.boot_id,
            channel_id,
            seq_high_water: channel.seq,
            counters: channel.counters.clone(),
            dropped_events: health.0,
            sink_errors: health.1,
            observer_unhealthy: health != (0, 0),
            open_attempts: channel.open.len(),
            open_attempts_complete: health == (0, 0),
            first_confirmation: channel.first_confirmation,
            confirmation_observed: channel.first_confirmation.is_some(),
        };
        #[cfg(test)]
        tests::snapshot_cut(self);
        // Monotone health counters bracket the locked cut; a concurrent loss invalidates it.
        if self.health() != health {
            Snapshot::Unavailable
        } else {
            snapshot
        }
    }
}

fn observer() -> Option<Arc<Observer>> {
    #[cfg(test)]
    if let Some(observer) = tests::local_observer() {
        return observer;
    }
    OBSERVER.get().cloned()
}

pub(crate) fn snapshot(channel: u64) -> Snapshot {
    observer().map_or(Snapshot::Unavailable, |o| o.snapshot(channel))
}

pub(crate) fn confirmation_boundary(channel_id: u64) -> Option<Boundary> {
    let o = observer()?;
    let Ok(mut channels) = o.channels.try_lock() else {
        o.lost();
        return None;
    };
    let channel = o.channel(&mut channels, channel_id)?;
    Some(Boundary {
        seq: channel.seq,
        at_us: o.at_us(),
    })
}

pub(crate) fn mode_confirmed(provider: &str, channel: u64, before: Option<Boundary>) {
    emit(
        provider,
        channel,
        Kind::ModeConfirmed {
            confirmation_window_start: before,
            actual_confirmed_at_us: None,
            boundary_rule: "overlap_or_missing_boundary_is_unknown;never_shrink_first_window",
        },
    );
}

pub(crate) fn emit(provider: &str, channel: u64, kind: Kind) {
    if let Some(o) = observer() {
        o.emit(provider, channel, kind);
    }
}

pub(crate) fn bounded_id(value: Option<&str>) -> Option<Option<String>> {
    if value.is_some_and(|s| s.len() > ID_CAP) {
        if let Some(o) = observer() {
            o.lost();
        }
        return None;
    }
    Some(value.map(String::from))
}

pub(crate) fn turn_closed(binding: &SourceBinding, turn: &TurnSpan, aborted: bool) {
    let Some(o) = observer() else {
        return;
    };
    let Some(path) = binding.source.path.to_str() else {
        o.lost();
        return;
    };
    if path.len() > ID_CAP || binding.source.session_id.len() > ID_CAP {
        o.lost();
        return;
    }
    let Some(native) = bounded_id(turn.native_turn_id.as_deref()) else {
        return;
    };
    let provider = binding.provider;
    let channel_id = binding.channel_id;
    let logical_turn_key = native.as_ref().map(|id| LogicalKey {
        provider,
        channel_id,
        provider_session_id: binding.source.session_id.clone(),
        native_turn_id: id.clone(),
    });
    let physical_observation_key = PhysicalKey {
        provider,
        channel_id,
        source_id: binding.source.clone(),
        start_offset: turn.start,
        end_offset: turn.end,
        native_turn_id: native,
    };
    o.emit(
        match provider {
            ShadowProvider::Claude => "claude",
            ShadowProvider::Codex => "codex",
        },
        channel_id,
        Kind::TurnClosed {
            physical_observation_key,
            logical_turn_key,
            autonomous: turn.autonomous,
            outcome: if aborted { "aborted" } else { "completed" },
        },
    );
}

pub(crate) struct Context<'a> {
    pub provider: &'a str,
    pub origin: &'static str,
    pub input_message_id: Option<u64>,
}

pub(crate) struct Attempt {
    observer: Arc<Observer>,
    provider: String,
    channel: u64,
    request: Placeholder,
}

pub(crate) fn placeholder_attempt(
    context: Context<'_>,
    channel: u64,
    operation: &'static str,
    reference: Option<(u64, u64)>,
    target: Option<u64>,
) -> Option<Attempt> {
    let o = observer()?;
    if context.provider.len() > 64 {
        o.lost();
        return None;
    }
    let request = Placeholder {
        op_id: (o.boot_id, o.next_op.fetch_add(1, Ordering::Relaxed)),
        origin: context.origin,
        operation,
        reference,
        target_message_id: target,
        input_message_id: context.input_message_id,
    };
    o.emit(
        context.provider,
        channel,
        Kind::PlaceholderLifecycle {
            request: request.clone(),
            phase: "attempt",
            message_id: None,
            error_class: None,
        },
    );
    Some(Attempt {
        observer: o,
        provider: context.provider.into(),
        channel,
        request,
    })
}

pub(crate) fn placeholder_result(attempt: Option<Attempt>, result: Result<u64, &'static str>) {
    if let Some(a) = attempt {
        a.observer.emit(
            &a.provider,
            a.channel,
            Kind::PlaceholderLifecycle {
                request: a.request,
                phase: if result.is_ok() {
                    "succeeded"
                } else {
                    "failed_or_uncertain"
                },
                message_id: result.ok(),
                error_class: result.err(),
            },
        );
    }
}

#[cfg(test)]
#[path = "n1_observation_tests.rs"]
pub(crate) mod tests;
