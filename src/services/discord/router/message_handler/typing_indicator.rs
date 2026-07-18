use std::sync::Arc;

use async_trait::async_trait;
use poise::serenity_prelude as serenity;
use serenity::ChannelId;
use tokio::sync::broadcast;
use tokio::time::{Duration, MissedTickBehavior};

use super::super::super::SharedData;
use super::super::super::inflight::InflightSignal;
use super::super::super::turn_completion_events::TurnCompletionEvent;

const TYPING_REFRESH_INTERVAL: Duration = Duration::from_secs(8);

#[async_trait]
trait TypingTransport: Send + Sync {
    async fn broadcast_typing(&self, channel_id: ChannelId) -> Result<(), String>;
}

struct SerenityTypingTransport {
    http: Arc<serenity::Http>,
}

#[async_trait]
impl TypingTransport for SerenityTypingTransport {
    async fn broadcast_typing(&self, channel_id: ChannelId) -> Result<(), String> {
        channel_id
            .broadcast_typing(&self.http)
            .await
            .map_err(|error| error.to_string())
    }
}

pub(in crate::services::discord) fn spawn_native_typing_indicator(
    shared: &Arc<SharedData>,
    http: Arc<serenity::Http>,
    channel_id: ChannelId,
) {
    let finalize_rx =
        super::super::super::turn_completion_events::subscribe_turn_completion_events(shared);
    let producer_rx = shared.inflight_signals.subscribe();
    spawn_typing_indicator_task(
        SerenityTypingTransport { http },
        channel_id,
        finalize_rx,
        producer_rx,
    );
}

fn spawn_typing_indicator_task<T>(
    transport: T,
    channel_id: ChannelId,
    finalize_rx: broadcast::Receiver<TurnCompletionEvent>,
    producer_rx: broadcast::Receiver<InflightSignal>,
) -> tokio::task::JoinHandle<()>
where
    T: TypingTransport + 'static,
{
    super::super::super::task_supervisor::spawn_observed(
        "discord_native_typing_indicator",
        run_native_typing_indicator(transport, channel_id, finalize_rx, producer_rx),
    )
}

async fn run_native_typing_indicator<T: TypingTransport>(
    transport: T,
    channel_id: ChannelId,
    mut finalize_rx: broadcast::Receiver<TurnCompletionEvent>,
    mut producer_rx: broadcast::Receiver<InflightSignal>,
) {
    let mut refresh = tokio::time::interval(TYPING_REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            event = finalize_rx.recv() => match event {
                Ok(event) if event.channel_id == channel_id => break,
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)
                    | broadcast::error::RecvError::Closed) => break,
            },
            event = producer_rx.recv() => match event {
                Ok(InflightSignal::Completed { channel_id: completed_channel })
                    if completed_channel == channel_id.get() => break,
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)
                    | broadcast::error::RecvError::Closed) => break,
            },
            _ = refresh.tick() => {
                if let Err(error) = transport.broadcast_typing(channel_id).await {
                    tracing::warn!(
                        channel_id = channel_id.get(),
                        error = %error,
                        "Discord typing indicator broadcast failed; stopping refresh loop"
                    );
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct CountingTransport {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl TypingTransport for CountingTransport {
        async fn broadcast_typing(&self, _channel_id: ChannelId) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    async fn yield_until(predicate: impl Fn() -> bool) {
        for _ in 0..16 {
            if predicate() {
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    fn test_receivers() -> (
        broadcast::Sender<TurnCompletionEvent>,
        broadcast::Receiver<TurnCompletionEvent>,
        broadcast::Sender<InflightSignal>,
        broadcast::Receiver<InflightSignal>,
    ) {
        let (finalize_tx, finalize_rx) = broadcast::channel(8);
        let (producer_tx, producer_rx) = broadcast::channel(8);
        (finalize_tx, finalize_rx, producer_tx, producer_rx)
    }

    #[tokio::test(start_paused = true)]
    async fn typing_retriggers_every_eight_seconds_until_finalize() {
        let channel_id = ChannelId::new(4571);
        let calls = Arc::new(AtomicUsize::new(0));
        let (finalize_tx, finalize_rx, _producer_tx, producer_rx) = test_receivers();
        let task = spawn_typing_indicator_task(
            CountingTransport {
                calls: calls.clone(),
            },
            channel_id,
            finalize_rx,
            producer_rx,
        );

        yield_until(|| calls.load(Ordering::SeqCst) == 1).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        tokio::time::advance(Duration::from_secs(7)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        tokio::time::advance(Duration::from_secs(1)).await;
        yield_until(|| calls.load(Ordering::SeqCst) == 2).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        finalize_tx
            .send(TurnCompletionEvent::new(channel_id))
            .expect("typing loop must subscribe to finalize events");
        yield_until(|| task.is_finished()).await;
        assert!(task.is_finished());

        tokio::time::advance(Duration::from_secs(16)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn producer_completion_stops_typing_without_cleanup_request() {
        let channel_id = ChannelId::new(4572);
        let calls = Arc::new(AtomicUsize::new(0));
        let (_finalize_tx, finalize_rx, producer_tx, producer_rx) = test_receivers();
        let task = spawn_typing_indicator_task(
            CountingTransport {
                calls: calls.clone(),
            },
            channel_id,
            finalize_rx,
            producer_rx,
        );

        yield_until(|| calls.load(Ordering::SeqCst) == 1).await;
        producer_tx
            .send(InflightSignal::Completed {
                channel_id: channel_id.get(),
            })
            .expect("typing loop must subscribe to producer completion");
        yield_until(|| task.is_finished()).await;
        assert!(task.is_finished());

        tokio::time::advance(Duration::from_secs(16)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    struct FailingTransport {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl TypingTransport for FailingTransport {
        async fn broadcast_typing(&self, _channel_id: ChannelId) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err("typing denied".to_string())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn broadcast_failure_stops_refresh_loop() {
        let channel_id = ChannelId::new(4573);
        let calls = Arc::new(AtomicUsize::new(0));
        let (_finalize_tx, finalize_rx, _producer_tx, producer_rx) = test_receivers();
        let task = spawn_typing_indicator_task(
            FailingTransport {
                calls: calls.clone(),
            },
            channel_id,
            finalize_rx,
            producer_rx,
        );

        yield_until(|| task.is_finished()).await;
        assert!(task.is_finished());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        tokio::time::advance(Duration::from_secs(16)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn unrelated_channel_signals_do_not_stop_typing() {
        let channel_id = ChannelId::new(4573);
        let other_channel = ChannelId::new(4574);
        let calls = Arc::new(AtomicUsize::new(0));
        let (finalize_tx, finalize_rx, producer_tx, producer_rx) = test_receivers();
        let task = spawn_typing_indicator_task(
            CountingTransport {
                calls: calls.clone(),
            },
            channel_id,
            finalize_rx,
            producer_rx,
        );

        yield_until(|| calls.load(Ordering::SeqCst) == 1).await;
        finalize_tx
            .send(TurnCompletionEvent::new(other_channel))
            .expect("typing loop must subscribe to finalize events");
        producer_tx
            .send(InflightSignal::Completed {
                channel_id: other_channel.get(),
            })
            .expect("typing loop must subscribe to producer completion");
        tokio::time::advance(Duration::from_secs(8)).await;
        yield_until(|| calls.load(Ordering::SeqCst) == 2).await;

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(!task.is_finished());
        task.abort();
    }
}
