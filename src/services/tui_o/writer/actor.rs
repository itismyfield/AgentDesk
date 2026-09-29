//! One channel's O actor: replays the spool, follows source binds, spools captured bytes and
//! delivers owed pieces in order. Capture goes on while the gateway is not Owned.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::binding::BindingEvents;
use super::deliver::{ChannelWriter, Step};
use super::pieces::{Derived, UnitDeriver};
use super::rotation::Sources;
use super::{AlarmSink, DeliveryLease, DiscordPort, WriterAlarm, WriterConfig};
use crate::services::tui_o::shadow::ShadowProvider;

pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Runs the channel's actor only when `tui_o.writer.enabled` is set.
pub fn spawn_if_enabled<P, L, A, B>(
    config: &WriterConfig,
    writer: ChannelWriter<P, L, A>,
    provider: ShadowProvider,
    bindings: Arc<B>,
    stop: watch::Receiver<bool>,
) -> Option<JoinHandle<()>>
where
    P: DiscordPort,
    L: DeliveryLease + 'static,
    A: AlarmSink + 'static,
    B: BindingEvents,
{
    let run = || tokio::spawn(run_channel(writer, provider, bindings, stop));
    config.enabled.then(run)
}

struct Actor<P, L, A, B> {
    writer: ChannelWriter<P, L, A>,
    deriver: UnitDeriver,
    owed: VecDeque<Derived>,
    sources: Sources<B>,
}

/// Returns when the channel stops or `stop` turns true or closes.
pub async fn run_channel<P, L, A, B>(
    writer: ChannelWriter<P, L, A>,
    provider: ShadowProvider,
    bindings: Arc<B>,
    mut stop: watch::Receiver<bool>,
) where
    P: DiscordPort,
    L: DeliveryLease,
    A: AlarmSink,
    B: BindingEvents,
{
    let deriver = UnitDeriver::new(writer.channel(), provider);
    let sources = Sources::new(writer.channel(), provider, bindings);
    let owed = VecDeque::new();
    let mut actor = Actor {
        writer,
        deriver,
        owed,
        sources,
    };
    let (writer, deriver, owed) = (&mut actor.writer, &mut actor.deriver, &mut actor.owed);
    if let Err(alarm) = actor.sources.resume(writer, deriver, owed) {
        actor.writer.stop(alarm);
    }
    while !actor.writer.is_stopped() && !*stop.borrow() {
        actor.deliver_owed().await;
        actor.collect_settled();
        actor.read_sources();
        tokio::select! {
            () = tokio::time::sleep(POLL_INTERVAL) => {}
            changed = stop.changed() => if changed.is_err() { return },
        }
    }
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink, B: BindingEvents> Actor<P, L, A, B> {
    async fn deliver_owed(&mut self) {
        while let Some(item) = self.owed.front() {
            match self.writer.deliver(item).await {
                Step::Done => {
                    self.owed.pop_front();
                }
                Step::LeaseBusy | Step::NoGateway | Step::Stopped => return,
            }
        }
    }

    /// With nothing owed, unsealed or open, every retained segment of a source with a decided start
    /// is settled. The open segment stays unless the spool is full, so segment files do not churn.
    fn collect_settled(&mut self) {
        let open = self.writer.store().ledger().unresolved().is_some();
        if self.writer.is_stopped() || open || !self.owed.is_empty() || self.deriver.has_unsealed()
        {
            return;
        }
        for (source, keep) in self.sources.collectable() {
            while self.writer.store().retained_segments(&source) > keep {
                if let Err(error) = self.writer.store().gc_oldest_segment(&source) {
                    let detail = format!("spool gc: {error:?}");
                    self.writer.stop(WriterAlarm::Halted { detail });
                    return;
                }
            }
        }
    }

    /// Applies new binds first, so a bound source is read in the same poll as its predecessor.
    fn read_sources(&mut self) {
        if self.writer.is_stopped() {
            return;
        }
        let (writer, deriver, owed) = (&mut self.writer, &mut self.deriver, &mut self.owed);
        let read = (self.sources.follow(writer))
            .and_then(|()| self.sources.capture(writer, deriver, owed))
            .and_then(|()| self.sources.tend(writer));
        if let Err(alarm) = read {
            self.writer.stop(alarm);
        }
    }
}
