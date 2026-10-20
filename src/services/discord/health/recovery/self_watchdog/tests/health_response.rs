//! The real `/api/health` answers a stalled database or a stalled mailbox actor with a degraded
//! or 503 body in time, and the watchdog reducer reads that body as a response.

use std::time::{Duration, Instant};

use super::super::policy::{
    Decision, Reason, WatchdogEvent, WatchdogPolicy, WatchdogState, reduce,
};
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::health::snapshot::health_observation::pg_tests::{
    Db, gateway, http_get, runtime, serve,
};
use crate::services::discord::host_teardown_gate::test_support::nameless_turn;
use crate::services::hang_forensics::{self, RuntimeLiveness};

#[test]
fn db_slow_http_response_resets_watchdog_streak() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let main = runtime();
    let parked = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let registry = std::sync::Arc::new(HealthRegistry::new());
    let shared = main.block_on(gateway(&registry));
    // Nothing drives `parked`, so this actor never answers.
    parked.block_on(nameless_turn(
        &shared,
        poise::serenity_prelude::ChannelId::new(6_871_370_001),
    ));
    main.block_on(async {
        let db = Db::start(Duration::ZERO, 4).await;
        let addr = serve(&db.pool, registry.clone()).await;
        for (stalled_query, http_status) in [("FROM dispatch_outbox", 200), ("SELECT 1", 503)] {
            db.proxy.stall(stalled_query);
            let (status, body, _) = http_get(addr, "/api/health").await;
            assert_eq!(status, http_status, "{body}");
            for reason in ["db_slow", "mailbox_snapshot_unavailable"] {
                assert!(
                    body["degraded_reasons"].to_string().contains(reason),
                    "{body}"
                );
            }

            let probe_addr = addr.to_string();
            let observation = tokio::task::spawn_blocking(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                hang_forensics::probe_health_once(&probe_addr, "127.0.0.1", deadline)
            })
            .await
            .unwrap();
            db.proxy.release();

            let policy = WatchdogPolicy::DEFAULT;
            let started = observation.observed_at - Duration::from_secs(60);
            let streak = WatchdogState {
                observed_failures: policy.max_failures - 1,
                fast_failures: policy.max_failures - 1,
                ..WatchdogState::new(started)
            };
            let (state, decision) = reduce(
                &policy,
                streak,
                WatchdogEvent::ProbeFinished {
                    outcome: observation.outcome,
                    observed_at: observation.observed_at,
                    current: hang_forensics::snapshot(),
                    liveness: RuntimeLiveness::Scheduling { age_ms: 0 },
                },
            );
            assert_eq!(
                decision,
                Decision::Continue(Reason::Responsive),
                "{stalled_query}"
            );
            assert_eq!((state.observed_failures, state.fast_failures), (0, 0));
            assert_eq!(state.last_response_at, Some(observation.observed_at));
            assert_eq!(
                state.deadline(&policy),
                observation.observed_at + policy.blackout_limit
            );
        }
        db.finish().await;
    });
}
