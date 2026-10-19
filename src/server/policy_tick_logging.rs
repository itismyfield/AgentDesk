use super::{PolicyTickHookExecution, PolicyTickHookOutcome};

#[derive(Default)]
pub(super) struct TickLogging {
    slow: [bool; 4],
}

impl TickLogging {
    pub(super) fn record(&mut self, label: &str, execution: &PolicyTickHookExecution) {
        match execution.outcome {
            PolicyTickHookOutcome::Ok => {
                let (tier, threshold) = match label {
                    "30s" => (0, 1000),
                    "1min" => (1, 2500),
                    // Preserve the threshold for tiers without a measured duration baseline.
                    "5min" => (2, 500),
                    _ => (3, 500),
                };
                let slow = execution.elapsed.as_millis() > threshold;
                let was_slow = std::mem::replace(&mut self.slow[tier], slow);
                if slow && !was_slow {
                    tracing::warn!(
                        "[policy-tick] {} took {}ms",
                        label,
                        execution.elapsed.as_millis()
                    );
                } else {
                    tracing::debug!(
                        "[policy-tick] {} took {}ms",
                        label,
                        execution.elapsed.as_millis()
                    );
                }
            }
            PolicyTickHookOutcome::Error | PolicyTickHookOutcome::JoinError => {
                tracing::warn!(
                    "[policy-tick] {} hook {}: {}",
                    label,
                    execution.outcome.status(),
                    execution.error.as_deref().unwrap_or("unknown error")
                );
            }
            PolicyTickHookOutcome::Timeout => {
                tracing::warn!(
                    "[policy-tick] {} hook timed out after {}ms",
                    label,
                    execution.elapsed.as_millis()
                );
            }
            PolicyTickHookOutcome::SkippedInFlight => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::server::{
        PolicyTickHookExecution, PolicyTickHookOutcome, record_tick_hook_execution_pg,
    };
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tracing::{Level, Subscriber, instrument::WithSubscriber};
    use tracing_subscriber::{Layer, layer::SubscriberExt};

    #[derive(Clone, Default)]
    struct Levels(Arc<Mutex<Vec<Level>>>);

    impl<S: Subscriber> Layer<S> for Levels {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target().starts_with("agentdesk::server") {
                self.0.lock().unwrap().push(*event.metadata().level());
            }
        }
    }

    #[tokio::test]
    async fn failures_stay_warn_and_do_not_count_as_timing_recovery() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused@127.0.0.1:1/unused")
            .unwrap();
        pool.close().await;
        let levels = Levels::default();
        crate::logging::test_capture::pin_callsite_interest();
        let mut tick_logging = super::TickLogging::default();
        async {
            for (outcome, elapsed) in [
                (PolicyTickHookOutcome::Ok, 1001),
                (PolicyTickHookOutcome::SkippedInFlight, 0),
                (PolicyTickHookOutcome::Ok, 1002),
                (PolicyTickHookOutcome::Error, 1),
                (PolicyTickHookOutcome::JoinError, 1),
                (PolicyTickHookOutcome::Timeout, 5000),
                (PolicyTickHookOutcome::Ok, 1003),
                (PolicyTickHookOutcome::Ok, 1000),
                (PolicyTickHookOutcome::Ok, 1001),
            ] {
                record_tick_hook_execution_pg(
                    &pool,
                    &mut tick_logging,
                    "30s",
                    &PolicyTickHookExecution {
                        outcome,
                        elapsed: Duration::from_millis(elapsed),
                        error: Some("failed".into()),
                    },
                )
                .await;
            }
        }
        .with_subscriber(tracing_subscriber::registry().with(levels.clone()))
        .await;
        assert_eq!(
            *levels.0.lock().unwrap(),
            [
                Level::WARN,
                Level::DEBUG,
                Level::WARN,
                Level::WARN,
                Level::WARN,
                Level::DEBUG,
                Level::DEBUG,
                Level::WARN
            ]
        );
    }

    #[tokio::test]
    async fn successful_tick_logs_are_tiered_and_warn_only_on_slow_transitions() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused@127.0.0.1:1/unused")
            .unwrap();
        pool.close().await;
        let levels = Levels::default();
        crate::logging::test_capture::pin_callsite_interest();
        let mut tick_logging = super::TickLogging::default();
        async {
            for offset in [0, 1, 2, 0, 1] {
                for (label, threshold) in [
                    ("30s", 1000),
                    ("1min", 2500),
                    ("5min", 500),
                    ("legacy", 500),
                ] {
                    let elapsed = threshold + offset;
                    record_tick_hook_execution_pg(
                        &pool,
                        &mut tick_logging,
                        label,
                        &PolicyTickHookExecution {
                            outcome: PolicyTickHookOutcome::Ok,
                            elapsed: Duration::from_millis(elapsed),
                            error: None,
                        },
                    )
                    .await;
                }
            }
        }
        .with_subscriber(tracing_subscriber::registry().with(levels.clone()))
        .await;
        assert_eq!(
            *levels.0.lock().unwrap(),
            [
                Level::DEBUG,
                Level::WARN,
                Level::DEBUG,
                Level::DEBUG,
                Level::WARN
            ]
            .into_iter()
            .flat_map(|level| [level; 4])
            .collect::<Vec<_>>()
        );
    }
}
