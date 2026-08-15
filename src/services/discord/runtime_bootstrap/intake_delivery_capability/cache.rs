use super::{SchemaReason, SettlementCapabilities, capabilities_for, probe_schema};
use crate::config::IntakeDeliverySettlementStage;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

const STAMP_DISPATCHED: u8 = 1;
const SETTLE_AND_SWEEP: u8 = 1 << 1;

/// Bootstrap-owned capability snapshot read by the per-turn bridge path.
#[derive(Debug, Default)]
pub(in crate::services::discord) struct SettlementCapabilityCache {
    bits: AtomicU8,
}

impl SettlementCapabilityCache {
    fn replace(&self, capabilities: SettlementCapabilities) {
        let mut bits = 0;
        if capabilities.stamp_dispatched {
            bits |= STAMP_DISPATCHED;
        }
        if capabilities.settle_and_sweep {
            bits |= SETTLE_AND_SWEEP;
        }
        self.bits.store(bits, Ordering::Release);
    }

    pub(in crate::services::discord) fn current(&self) -> SettlementCapabilities {
        let bits = self.bits.load(Ordering::Acquire);
        SettlementCapabilities {
            stamp_dispatched: bits & STAMP_DISPATCHED != 0,
            settle_and_sweep: bits & SETTLE_AND_SWEEP != 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolutionTrigger {
    Bootstrap,
    Reload {
        previous: IntakeDeliverySettlementStage,
    },
}

fn stage_from(config: Option<&Arc<crate::config::Config>>) -> IntakeDeliverySettlementStage {
    config
        .map(|config| config.runtime.intake_delivery_settlement)
        .unwrap_or_default()
}

fn should_probe(stage: IntakeDeliverySettlementStage, trigger: ResolutionTrigger) -> bool {
    stage >= IntakeDeliverySettlementStage::Settle
        && match trigger {
            ResolutionTrigger::Bootstrap => true,
            ResolutionTrigger::Reload { previous } => stage >= previous,
        }
}

async fn resolve(
    pool: Option<&sqlx::PgPool>,
    stage: IntakeDeliverySettlementStage,
    previous_schema: Option<SchemaReason>,
    trigger: ResolutionTrigger,
) -> (SettlementCapabilities, Option<SchemaReason>) {
    let schema = if should_probe(stage, trigger) {
        Some(match pool {
            Some(pool) => probe_schema(pool).await,
            None => SchemaReason::Query,
        })
    } else {
        previous_schema
    };
    let capabilities = capabilities_for(stage, schema.unwrap_or(SchemaReason::Query));
    (capabilities, schema)
}

async fn bootstrap_from_updates(
    pool: Option<sqlx::PgPool>,
    mut updates: tokio::sync::watch::Receiver<Option<Arc<crate::config::Config>>>,
) -> Arc<SettlementCapabilityCache> {
    let initial_stage = stage_from(updates.borrow().as_ref());
    let (initial, initial_schema) = resolve(
        pool.as_ref(),
        initial_stage,
        None,
        ResolutionTrigger::Bootstrap,
    )
    .await;
    let cache = Arc::new(SettlementCapabilityCache::default());
    cache.replace(initial);

    let reload_cache = cache.clone();
    tokio::spawn(async move {
        let mut previous_stage = initial_stage;
        let mut previous_schema = initial_schema;
        while updates.changed().await.is_ok() {
            let stage = stage_from(updates.borrow_and_update().as_ref());
            let (capabilities, schema) = resolve(
                pool.as_ref(),
                stage,
                previous_schema,
                ResolutionTrigger::Reload {
                    previous: previous_stage,
                },
            )
            .await;
            reload_cache.replace(capabilities);
            previous_stage = stage;
            previous_schema = schema;
            tracing::debug!(
                ?stage,
                stamp_dispatched = capabilities.stamp_dispatched,
                settle_and_sweep = capabilities.settle_and_sweep,
                "intake delivery capabilities refreshed"
            );
        }
    });
    cache
}

/// Resolves capabilities at Discord bootstrap and refreshes them after every
/// successful live-config install. Off and Observe never touch PostgreSQL.
pub(in crate::services::discord) async fn bootstrap(
    pool: Option<sqlx::PgPool>,
) -> Arc<SettlementCapabilityCache> {
    bootstrap_from_updates(pool, crate::config_live_reload::subscribe()).await
}

#[cfg(test)]
pub(in crate::services::discord) async fn bootstrap_for_test(
    pool: Option<sqlx::PgPool>,
    stage: IntakeDeliverySettlementStage,
) -> Arc<SettlementCapabilityCache> {
    let mut config = crate::config::Config::default();
    config.runtime.intake_delivery_settlement = stage;
    let (sender, updates) = tokio::sync::watch::channel(Some(Arc::new(config)));
    let cache = bootstrap_from_updates(pool, updates).await;
    drop(sender);
    cache
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_policy_is_boot_upward_and_same_only_at_settle_or_higher() {
        use IntakeDeliverySettlementStage::{Enforce, Observe, Off, Settle};

        assert!(!should_probe(Off, ResolutionTrigger::Bootstrap));
        assert!(!should_probe(Observe, ResolutionTrigger::Bootstrap));
        assert!(should_probe(Settle, ResolutionTrigger::Bootstrap));
        assert!(should_probe(Enforce, ResolutionTrigger::Bootstrap));
        assert!(should_probe(
            Settle,
            ResolutionTrigger::Reload { previous: Observe }
        ));
        assert!(should_probe(
            Enforce,
            ResolutionTrigger::Reload { previous: Enforce }
        ));
        assert!(!should_probe(
            Settle,
            ResolutionTrigger::Reload { previous: Enforce }
        ));
        assert!(!should_probe(
            Observe,
            ResolutionTrigger::Reload { previous: Observe }
        ));
    }

    #[test]
    fn cache_round_trips_the_two_capability_bits() {
        let cache = SettlementCapabilityCache::default();
        for capabilities in [
            SettlementCapabilities::default(),
            SettlementCapabilities {
                stamp_dispatched: false,
                settle_and_sweep: true,
            },
            SettlementCapabilities {
                stamp_dispatched: true,
                settle_and_sweep: true,
            },
        ] {
            cache.replace(capabilities);
            assert_eq!(cache.current(), capabilities);
        }
    }
}
