//! One channel's O actor: replays the spool, spools newly captured transcript bytes, and delivers
//! owed pieces in order. Capture goes on while the gateway is not Owned; delivery waits for it.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::deliver::{ChannelWriter, Step};
use super::pieces::{Derived, UnitDeriver};
use super::{AlarmSink, DeliveryLease, DiscordPort, WriterAlarm, WriterConfig};
use crate::services::tui_o::shadow::capture::SourceCapture;
use crate::services::tui_o::shadow::{
    CaptureBatch, CaptureOutcome, CaptureSource, MAX_READ_BYTES, ShadowProvider,
};
use crate::services::tui_o::store::StoreError;
use crate::services::tui_o::store::spool::SpoolFrame;

pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Runs the channel's actor only when `tui_o.writer.enabled` is set.
pub fn spawn_if_enabled<P, L, A>(
    config: &WriterConfig,
    writer: ChannelWriter<P, L, A>,
    provider: ShadowProvider,
    stop: watch::Receiver<bool>,
) -> Option<JoinHandle<()>>
where
    P: DiscordPort,
    L: DeliveryLease + 'static,
    A: AlarmSink + 'static,
{
    let run = || tokio::spawn(run_channel(writer, provider, stop));
    config.enabled.then(run)
}

struct Actor<P, L, A> {
    writer: ChannelWriter<P, L, A>,
    deriver: UnitDeriver,
    owed: VecDeque<Derived>,
    capture: Option<SourceCapture>,
    /// A batch the full spool refused; it is retried before the source is read again.
    pending: Option<CaptureBatch>,
}

/// Returns when the channel stops or `stop` turns true or closes.
pub async fn run_channel<P, L, A>(
    writer: ChannelWriter<P, L, A>,
    provider: ShadowProvider,
    mut stop: watch::Receiver<bool>,
) where
    P: DiscordPort,
    L: DeliveryLease,
    A: AlarmSink,
{
    let deriver = UnitDeriver::new(writer.channel(), provider);
    let (owed, capture, pending) = (VecDeque::new(), None, None);
    let mut actor = Actor {
        writer,
        deriver,
        owed,
        capture,
        pending,
    };
    if let Err(detail) = actor.resume() {
        actor.writer.stop(WriterAlarm::Halted { detail });
    }
    while !actor.writer.is_stopped() && !*stop.borrow() {
        actor.deliver_owed().await;
        actor.collect_settled();
        actor.capture();
        tokio::select! {
            () = tokio::time::sleep(POLL_INTERVAL) => {}
            changed = stop.changed() => if changed.is_err() { return },
        }
    }
}

impl<P: DiscordPort, L: DeliveryLease, A: AlarmSink> Actor<P, L, A> {
    /// Re-derives the retained spool, then reopens the source at its cursor.
    fn resume(&mut self) -> Result<(), String> {
        let cursors: Vec<_> = self.writer.store().cursors().cloned().collect();
        let cursor = match cursors.as_slice() {
            [] => return Ok(()),
            [one] if !one.retired => one.clone(),
            _ => return Err("several or retired sources need source rotation".into()),
        };
        let (deriver, owed) = (&mut self.deriver, &mut self.owed);
        let replay = self.writer.store().for_each_frame(&cursor.source, |frame| {
            if let SpoolFrame::Record(record) = frame {
                owed.extend(deriver.derive(&record));
            }
        });
        replay.map_err(|error| format!("spool replay: {error:?}"))?;
        let opened = SourceCapture::open(cursor.source.clone(), cursor.captured_through);
        let capture = opened.map_err(|error| format!("source reopen: {error}"))?;
        if capture.prefix_hash() != cursor.prefix_hash {
            return Err("source bytes before the cursor changed".into());
        }
        self.capture = Some(capture);
        Ok(())
    }

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

    /// With nothing owed, unsealed or open, every retained segment is settled. The open segment
    /// is kept unless the spool is full, so a busy transcript does not churn segment files.
    fn collect_settled(&mut self) {
        let Some(source) = self.capture.as_ref().map(|c| c.source().clone()) else {
            return;
        };
        let open = self.writer.store().ledger().unresolved().is_some();
        if self.writer.is_stopped() || open || !self.owed.is_empty() || self.deriver.has_unsealed()
        {
            return;
        }
        let keep = usize::from(self.pending.is_none());
        while self.writer.store().retained_segments(&source) > keep {
            if let Err(error) = self.writer.store().gc_oldest_segment(&source) {
                let detail = format!("spool gc: {error:?}");
                self.writer.stop(WriterAlarm::Halted { detail });
                return;
            }
        }
    }

    fn capture(&mut self) {
        let Some(capture) = self.capture.as_mut() else {
            return;
        };
        if self.writer.is_stopped() {
            return;
        }
        let retried = self.pending.is_some();
        let batch = match self.pending.take() {
            Some(batch) => batch,
            None => match capture.poll(MAX_READ_BYTES) {
                CaptureOutcome::Batch(batch) => batch,
                CaptureOutcome::Anomaly(anomaly) => {
                    let detail = format!("source {:?}: {}", anomaly.kind, anomaly.detail);
                    self.writer.stop(WriterAlarm::Halted { detail });
                    return;
                }
            },
        };
        let prefix_hash = capture.prefix_hash();
        match self.writer.store().append_spool(&batch, &prefix_hash) {
            Ok(()) => {
                let derived = batch.records.iter().flat_map(|r| self.deriver.derive(r));
                self.owed.extend(derived.collect::<Vec<_>>());
            }
            Err(StoreError::SpoolFull) => {
                if !retried {
                    self.writer.alarm(WriterAlarm::SpoolFull);
                }
                self.pending = Some(batch);
            }
            Err(error) => {
                let detail = format!("spool append: {error:?}");
                self.writer.stop(WriterAlarm::Halted { detail });
            }
        }
    }
}
