use std::sync::Arc;

use async_trait::async_trait;
use poise::serenity_prelude as serenity;
use serenity::ChannelId;
use tokio::sync::broadcast;
use tokio::time::{Duration, MissedTickBehavior};

use super::super::SharedData;
use super::super::inflight::{InflightSignal, InflightTurnState};
use super::super::scheduled_turn::ScheduledTurnMark;
use super::super::turn_completion_events::TurnCompletionEvent;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::provider::ProviderKind;
use crate::services::turn_host::TurnHost;

const TYPING_REFRESH_INTERVAL: Duration = Duration::from_secs(8);
const TYPING_MAX_LIFETIME: Duration = Duration::from_secs(6 * 60 * 60);

#[async_trait]
trait TypingTransport: Send + Sync {
    fn supervisor_owns_typing(&self, _channel_id: ChannelId) -> bool {
        false
    }

    async fn broadcast_typing(&self, channel_id: ChannelId) -> Result<(), String>;
}

struct SerenityTypingTransport {
    http: Arc<serenity::Http>,
    presence_provider: Option<ProviderKind>,
}

#[async_trait]
impl TypingTransport for SerenityTypingTransport {
    fn supervisor_owns_typing(&self, channel_id: ChannelId) -> bool {
        self.presence_provider
            .as_ref()
            .is_some_and(|provider| super::supervisor::observes(provider, channel_id.get()))
    }

    async fn broadcast_typing(&self, channel_id: ChannelId) -> Result<(), String> {
        channel_id
            .broadcast_typing(&self.http)
            .await
            .map_err(|error| error.to_string())
    }
}

pub(in crate::services::discord) enum TypingScope {
    Channel,
    HerdrChannel,
    Standalone,
}

impl TypingScope {
    pub(in crate::services::discord) fn for_start(host: &TurnHost, standalone: bool) -> Self {
        match (host, standalone) {
            (_, true) => Self::Standalone,
            (TurnHost::Herdr(_), false) => Self::HerdrChannel,
            (_, false) => Self::Channel,
        }
    }

    fn presence_provider(
        self,
        provider: &ProviderKind,
        state: &InflightTurnState,
    ) -> Option<ProviderKind> {
        if state.routine_presentation.isolated_run()
            || matches!(
                state.scheduled,
                ScheduledTurnMark::Scheduled { isolated: true, .. }
            )
        {
            return None;
        }
        let tui = match self {
            Self::Standalone => false,
            Self::HerdrChannel => matches!(provider, ProviderKind::Claude | ProviderKind::Codex),
            Self::Channel => matches!(
                (provider, state.runtime_kind_for_recovery()),
                (ProviderKind::Claude, RuntimeHandoffKind::ClaudeTui)
                    | (ProviderKind::Codex, RuntimeHandoffKind::CodexTui)
            ),
        };
        tui.then(|| provider.clone())
    }
}

pub(in crate::services::discord) fn spawn_native_typing_indicator(
    shared: &Arc<SharedData>,
    http: Arc<serenity::Http>,
    provider: &ProviderKind,
    state: &InflightTurnState,
    scope: TypingScope,
) {
    let channel_id = ChannelId::new(state.channel_id);
    let transport = SerenityTypingTransport {
        http,
        presence_provider: scope.presence_provider(provider, state),
    };
    if transport.supervisor_owns_typing(channel_id) {
        return;
    }
    #[cfg(all(test, unix))]
    tests::record_spawn(channel_id);
    let finalize_rx =
        super::super::turn_completion_events::subscribe_turn_completion_events(shared);
    let producer_rx = shared.inflight_signals.subscribe();
    spawn_typing_indicator_task(
        transport,
        channel_id,
        state.effective_finalizer_turn_id(),
        TYPING_MAX_LIFETIME,
        finalize_rx,
        producer_rx,
    );
}

fn spawn_typing_indicator_task<T>(
    transport: T,
    channel_id: ChannelId,
    turn_id: u64,
    max_lifetime: Duration,
    finalize_rx: broadcast::Receiver<TurnCompletionEvent>,
    producer_rx: broadcast::Receiver<InflightSignal>,
) -> tokio::task::JoinHandle<()>
where
    T: TypingTransport + 'static,
{
    super::super::task_supervisor::spawn_observed(
        "discord_native_typing_indicator",
        run_native_typing_indicator(
            transport,
            channel_id,
            turn_id,
            max_lifetime,
            finalize_rx,
            producer_rx,
        ),
    )
}

async fn run_native_typing_indicator<T: TypingTransport>(
    transport: T,
    channel_id: ChannelId,
    turn_id: u64,
    max_lifetime: Duration,
    mut finalize_rx: broadcast::Receiver<TurnCompletionEvent>,
    mut producer_rx: broadcast::Receiver<InflightSignal>,
) {
    let mut refresh = tokio::time::interval(TYPING_REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let lifetime = tokio::time::sleep(max_lifetime);
    tokio::pin!(lifetime);

    loop {
        tokio::select! {
            biased;
            _ = &mut lifetime => break,
            event = finalize_rx.recv() => match event {
                Ok(event)
                    if event.channel_id == channel_id && event.turn_id == Some(turn_id) => break,
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)
                    | broadcast::error::RecvError::Closed) => break,
            },
            event = producer_rx.recv() => match event {
                Ok(InflightSignal::Completed {
                    channel_id: completed_channel,
                    turn_id: completed_turn,
                }) if completed_channel == channel_id.get() && completed_turn == turn_id => break,
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)
                    | broadcast::error::RecvError::Closed) => break,
            },
            _ = refresh.tick() => {
                // Turn mode can be confirmed after this loop was started.
                if transport.supervisor_owns_typing(channel_id) {
                    break;
                }
                if let Err(error) = transport.broadcast_typing(channel_id).await {
                    tracing::warn!(
                        target: "agentdesk::services::discord::router::message_handler::typing_indicator",
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
pub(in crate::services::discord) mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[cfg(unix)]
    static SPAWNS: std::sync::Mutex<Vec<(ChannelId, usize)>> = std::sync::Mutex::new(Vec::new());

    #[cfg(unix)]
    pub(in crate::services::discord) struct SpawnProbe(ChannelId);

    #[cfg(unix)]
    impl SpawnProbe {
        pub(in crate::services::discord) fn new(channel: ChannelId) -> Self {
            SPAWNS.lock().unwrap().push((channel, 0));
            Self(channel)
        }

        pub(in crate::services::discord) fn count(&self) -> usize {
            SPAWNS
                .lock()
                .unwrap()
                .iter()
                .find(|(channel, _)| *channel == self.0)
                .unwrap()
                .1
        }
    }

    #[cfg(unix)]
    impl Drop for SpawnProbe {
        fn drop(&mut self) {
            SPAWNS
                .lock()
                .unwrap()
                .retain(|(channel, _)| *channel != self.0);
        }
    }

    #[cfg(unix)]
    pub(super) fn record_spawn(channel: ChannelId) {
        if let Some((_, count)) = SPAWNS
            .lock()
            .unwrap()
            .iter_mut()
            .find(|(id, _)| *id == channel)
        {
            *count += 1;
        }
    }

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

    #[cfg(unix)]
    #[tokio::test]
    async fn native_typing_keeps_process_and_standalone_work() {
        use super::super::supervisor::tests::RegisteredForTyping;
        use crate::services::discord::{
            admin_host_guard::tests::Recorder, make_shared_data_for_tests,
        };

        let shared = make_shared_data_for_tests();
        let api = Recorder::start().await;
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(6_201_081);
        let _registration = RegisteredForTyping::new(&provider, channel.get());
        let typing = SpawnProbe::new(channel);
        let mut state = InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            1,
            2,
            3,
            "typing fixture".into(),
            None,
            Some("fixture-pane".into()),
            Some("fixture-transcript".into()),
            None,
            0,
        );
        let spawn = |state: &InflightTurnState, scope| {
            spawn_native_typing_indicator(&shared, api.http.clone(), &provider, state, scope);
        };
        // A pre-runtime-kind Claude row with a transcript and no FIFO is a TUI restore.
        assert_eq!(state.runtime_kind, None);
        spawn(&state, TypingScope::Channel);
        assert_eq!(typing.count(), 0);
        spawn(&state, TypingScope::Standalone);
        assert_eq!(typing.count(), 1);
        state.runtime_kind = Some(RuntimeHandoffKind::ProcessBackend);
        spawn(&state, TypingScope::for_start(&TurnHost::Tmux, false));
        assert_eq!(typing.count(), 2);
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn native_refresh_ends_when_a_running_tui_becomes_supervised() {
        use super::super::supervisor::tests::RegisteredForTyping;

        struct LateConfirmation {
            eligibility: SerenityTypingTransport,
            counter: CountingTransport,
        }

        #[async_trait]
        impl TypingTransport for LateConfirmation {
            fn supervisor_owns_typing(&self, channel: ChannelId) -> bool {
                self.eligibility.supervisor_owns_typing(channel)
            }

            async fn broadcast_typing(&self, channel: ChannelId) -> Result<(), String> {
                self.counter.broadcast_typing(channel).await
            }
        }

        let channel = ChannelId::new(6_201_082);
        let provider = ProviderKind::Codex;
        let calls = Arc::new(AtomicUsize::new(0));
        let (_finalize_tx, finalize_rx, _producer_tx, producer_rx) = test_receivers();
        let task = spawn_typing_indicator_task(
            LateConfirmation {
                eligibility: SerenityTypingTransport {
                    http: Arc::new(serenity::Http::new("test-token")),
                    presence_provider: Some(provider.clone()),
                },
                counter: CountingTransport {
                    calls: calls.clone(),
                },
            },
            channel,
            21,
            TYPING_MAX_LIFETIME,
            finalize_rx,
            producer_rx,
        );
        yield_until(|| calls.load(Ordering::SeqCst) == 1).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let _registration = RegisteredForTyping::new(&provider, channel.get());
        tokio::time::advance(TYPING_REFRESH_INTERVAL).await;
        yield_until(|| task.is_finished()).await;
        assert!(task.is_finished());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn typing_retriggers_every_eight_seconds_until_finalize() {
        let channel_id = ChannelId::new(4571);
        let turn_id = 11;
        let calls = Arc::new(AtomicUsize::new(0));
        let (finalize_tx, finalize_rx, _producer_tx, producer_rx) = test_receivers();
        let task = spawn_typing_indicator_task(
            CountingTransport {
                calls: calls.clone(),
            },
            channel_id,
            turn_id,
            TYPING_MAX_LIFETIME,
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
            .send(TurnCompletionEvent::for_turn(channel_id, turn_id))
            .expect("typing loop must subscribe to finalize events");
        yield_until(|| task.is_finished()).await;
        assert!(task.is_finished());

        tokio::time::advance(Duration::from_secs(16)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn producer_completion_stops_typing_without_cleanup_request() {
        let channel_id = ChannelId::new(4572);
        let turn_id = 12;
        let calls = Arc::new(AtomicUsize::new(0));
        let (_finalize_tx, finalize_rx, producer_tx, producer_rx) = test_receivers();
        let task = spawn_typing_indicator_task(
            CountingTransport {
                calls: calls.clone(),
            },
            channel_id,
            turn_id,
            TYPING_MAX_LIFETIME,
            finalize_rx,
            producer_rx,
        );

        yield_until(|| calls.load(Ordering::SeqCst) == 1).await;
        producer_tx
            .send(InflightSignal::Completed {
                channel_id: channel_id.get(),
                turn_id,
            })
            .expect("typing loop must subscribe to producer completion");
        yield_until(|| task.is_finished()).await;
        assert!(task.is_finished());

        tokio::time::advance(Duration::from_secs(16)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn previous_turn_completion_does_not_stop_next_turn_typing() {
        let channel_id = ChannelId::new(4575);
        let current_turn_id = 22;
        let previous_turn_id = 21;
        let calls = Arc::new(AtomicUsize::new(0));
        let (finalize_tx, finalize_rx, producer_tx, producer_rx) = test_receivers();
        let task = spawn_typing_indicator_task(
            CountingTransport {
                calls: calls.clone(),
            },
            channel_id,
            current_turn_id,
            TYPING_MAX_LIFETIME,
            finalize_rx,
            producer_rx,
        );

        yield_until(|| calls.load(Ordering::SeqCst) == 1).await;
        finalize_tx
            .send(TurnCompletionEvent::for_turn(channel_id, previous_turn_id))
            .expect("next-turn typing must subscribe to finalize events");
        producer_tx
            .send(InflightSignal::Completed {
                channel_id: channel_id.get(),
                turn_id: previous_turn_id,
            })
            .expect("next-turn typing must subscribe to producer completion");
        tokio::time::advance(Duration::from_secs(8)).await;
        yield_until(|| calls.load(Ordering::SeqCst) == 2).await;

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(!task.is_finished());
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn max_lifetime_stops_typing_without_terminal_signal() {
        let channel_id = ChannelId::new(4576);
        let calls = Arc::new(AtomicUsize::new(0));
        let (_finalize_tx, finalize_rx, _producer_tx, producer_rx) = test_receivers();
        let max_lifetime = Duration::from_secs(17);
        let task = spawn_typing_indicator_task(
            CountingTransport {
                calls: calls.clone(),
            },
            channel_id,
            31,
            max_lifetime,
            finalize_rx,
            producer_rx,
        );

        yield_until(|| calls.load(Ordering::SeqCst) >= 1).await;
        assert!(calls.load(Ordering::SeqCst) >= 1);

        tokio::time::advance(max_lifetime).await;
        yield_until(|| task.is_finished()).await;
        assert!(task.is_finished());

        let calls_at_expiry = calls.load(Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(16)).await;
        assert_eq!(calls.load(Ordering::SeqCst), calls_at_expiry);
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
        let turn_id = 13;
        let calls = Arc::new(AtomicUsize::new(0));
        let (_finalize_tx, finalize_rx, _producer_tx, producer_rx) = test_receivers();
        let task = spawn_typing_indicator_task(
            FailingTransport {
                calls: calls.clone(),
            },
            channel_id,
            turn_id,
            TYPING_MAX_LIFETIME,
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
        let turn_id = 14;
        let calls = Arc::new(AtomicUsize::new(0));
        let (finalize_tx, finalize_rx, producer_tx, producer_rx) = test_receivers();
        let task = spawn_typing_indicator_task(
            CountingTransport {
                calls: calls.clone(),
            },
            channel_id,
            turn_id,
            TYPING_MAX_LIFETIME,
            finalize_rx,
            producer_rx,
        );

        yield_until(|| calls.load(Ordering::SeqCst) == 1).await;
        finalize_tx
            .send(TurnCompletionEvent::for_turn(other_channel, turn_id))
            .expect("typing loop must subscribe to finalize events");
        producer_tx
            .send(InflightSignal::Completed {
                channel_id: other_channel.get(),
                turn_id,
            })
            .expect("typing loop must subscribe to producer completion");
        tokio::time::advance(Duration::from_secs(8)).await;
        yield_until(|| calls.load(Ordering::SeqCst) == 2).await;

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(!task.is_finished());
        task.abort();
    }
}
