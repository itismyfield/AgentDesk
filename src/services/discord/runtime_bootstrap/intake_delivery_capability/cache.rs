use super::{SchemaReason, SettlementCapabilities, capabilities_for, probe_schema};
use crate::config::IntakeDeliverySettlementStage;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

const STAMP_DISPATCHED: u64 = 1;
const SETTLE_AND_SWEEP: u64 = 1 << 1;
const CAPABILITY_MASK: u64 = STAMP_DISPATCHED | SETTLE_AND_SWEEP;
const GENERATION_SHIFT: u32 = 2;

/// Bootstrap-owned capability snapshot read by the per-turn bridge path.
#[derive(Debug, Default)]
pub(in crate::services::discord) struct SettlementCapabilityCache {
    state: AtomicU64,
    bridge_turn_snapshots: Mutex<HashMap<i64, SettlementCapabilities>>,
    #[cfg(test)]
    stale_results: AtomicU64,
}

impl SettlementCapabilityCache {
    fn capability_bits(capabilities: SettlementCapabilities) -> u64 {
        let mut bits = 0;
        if capabilities.stamp_dispatched {
            bits |= STAMP_DISPATCHED;
        }
        if capabilities.settle_and_sweep {
            bits |= SETTLE_AND_SWEEP;
        }
        bits
    }

    /// Starts a config resolution. A downgrade below Enforce clears stamping
    /// immediately, while a downgrade below Settle clears both capabilities.
    /// See the four-part snapshot contract on [`SettlementCapabilities`].
    fn begin_resolution(&self, stage: IntakeDeliverySettlementStage) -> u64 {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            let generation = (current >> GENERATION_SHIFT) + 1;
            let retained_bits = if stage >= IntakeDeliverySettlementStage::Enforce {
                current & CAPABILITY_MASK
            } else if stage >= IntakeDeliverySettlementStage::Settle {
                current & SETTLE_AND_SWEEP
            } else {
                0
            };
            let next = (generation << GENERATION_SHIFT) | retained_bits;
            match self.state.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return generation,
                Err(observed) => current = observed,
            }
        }
    }

    /// Publishes a probe result only while its config generation is current.
    fn replace_for_generation(
        &self,
        generation: u64,
        capabilities: SettlementCapabilities,
    ) -> bool {
        let bits = Self::capability_bits(capabilities);
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            if current >> GENERATION_SHIFT != generation {
                return false;
            }
            let next = (generation << GENERATION_SHIFT) | bits;
            match self.state.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    fn replace(&self, capabilities: SettlementCapabilities) {
        let generation = self.begin_resolution(IntakeDeliverySettlementStage::Settle);
        let installed = self.replace_for_generation(generation, capabilities);
        debug_assert!(installed, "new cache generation must still be current");
    }

    pub(in crate::services::discord) fn current(&self) -> SettlementCapabilities {
        let bits = self.state.load(Ordering::Acquire) & CAPABILITY_MASK;
        SettlementCapabilities {
            stamp_dispatched: bits & STAMP_DISPATCHED != 0,
            settle_and_sweep: bits & SETTLE_AND_SWEEP != 0,
        }
    }

    /// Transfers the pre-await stamp snapshot to the adjacent bridge spawn.
    /// See the four-part snapshot contract on [`SettlementCapabilities`].
    pub(in crate::services::discord) fn record_bridge_turn_snapshot(
        &self,
        outbox_id: i64,
        snapshot: SettlementCapabilities,
    ) {
        self.bridge_turn_snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(outbox_id, snapshot);
    }

    /// Consumes the snapshot recorded by dispatched stamping. Non-intake and restored bridges
    /// have no adjacent stamp call and therefore start from the cache's current snapshot.
    /// See the four-part snapshot contract on [`SettlementCapabilities`].
    pub(in crate::services::discord) fn take_bridge_turn_snapshot(
        &self,
        outbox_id: Option<i64>,
    ) -> SettlementCapabilities {
        outbox_id
            .and_then(|outbox_id| {
                self.bridge_turn_snapshots
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&outbox_id)
            })
            .unwrap_or_else(|| self.current())
    }

    #[cfg(test)]
    pub(in crate::services::discord) fn for_test(
        capabilities: SettlementCapabilities,
    ) -> Arc<Self> {
        let cache = Arc::new(Self::default());
        cache.replace(capabilities);
        cache
    }

    #[cfg(test)]
    pub(in crate::services::discord) fn generation_for_test(&self) -> u64 {
        self.state.load(Ordering::Acquire) >> GENERATION_SHIFT
    }

    #[cfg(test)]
    pub(in crate::services::discord) fn stale_results_for_test(&self) -> u64 {
        self.stale_results.load(Ordering::Acquire)
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

#[derive(Clone, Copy, Debug)]
struct Resolution {
    generation: u64,
    stage: IntakeDeliverySettlementStage,
    capabilities: SettlementCapabilities,
    schema: Option<SchemaReason>,
}

fn spawn_resolution(
    pool: Option<sqlx::PgPool>,
    stage: IntakeDeliverySettlementStage,
    previous_schema: Option<SchemaReason>,
    trigger: ResolutionTrigger,
    generation: u64,
    completed: tokio::sync::mpsc::UnboundedSender<Resolution>,
) {
    tokio::spawn(async move {
        let (capabilities, schema) = resolve(pool.as_ref(), stage, previous_schema, trigger).await;
        let _ = completed.send(Resolution {
            generation,
            stage,
            capabilities,
            schema,
        });
    });
}

async fn refresh_from_updates(
    pool: Option<sqlx::PgPool>,
    cache: Arc<SettlementCapabilityCache>,
    mut updates: tokio::sync::watch::Receiver<Option<Arc<crate::config::Config>>>,
    ready: tokio::sync::oneshot::Sender<()>,
) {
    let (completed_tx, mut completed_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut latest_stage = stage_from(updates.borrow().as_ref());
    let mut previous_schema = None;
    let generation = cache.begin_resolution(latest_stage);
    spawn_resolution(
        pool.clone(),
        latest_stage,
        previous_schema,
        ResolutionTrigger::Bootstrap,
        generation,
        completed_tx.clone(),
    );

    let mut ready = Some(ready);
    let mut updates_open = true;
    let mut pending_resolutions = 1_usize;
    loop {
        tokio::select! {
            biased;
            changed = updates.changed(), if updates_open => {
                if changed.is_err() {
                    updates_open = false;
                    if pending_resolutions == 0 {
                        break;
                    }
                    continue;
                }
                let stage = stage_from(updates.borrow_and_update().as_ref());
                let trigger = ResolutionTrigger::Reload { previous: latest_stage };
                latest_stage = stage;
                let generation = cache.begin_resolution(stage);
                spawn_resolution(
                    pool.clone(),
                    stage,
                    previous_schema,
                    trigger,
                    generation,
                    completed_tx.clone(),
                );
                pending_resolutions += 1;
            }
            Some(resolution) = completed_rx.recv() => {
                pending_resolutions -= 1;
                if !cache.replace_for_generation(
                    resolution.generation,
                    resolution.capabilities,
                ) {
                    #[cfg(test)]
                    cache.stale_results.fetch_add(1, Ordering::Release);
                    tracing::debug!(
                        generation = resolution.generation,
                        ?resolution.stage,
                        "discarded stale intake delivery capability probe result"
                    );
                } else {
                    previous_schema = resolution.schema;
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(());
                    }
                    tracing::debug!(
                        stage = ?resolution.stage,
                        stamp_dispatched = resolution.capabilities.stamp_dispatched,
                        settle_and_sweep = resolution.capabilities.settle_and_sweep,
                        "intake delivery settlement stage interpreted and capabilities refreshed"
                    );
                }
                if !updates_open && pending_resolutions == 0 {
                    break;
                }
            }
            else => break,
        }
    }
}

async fn bootstrap_from_updates(
    pool: Option<sqlx::PgPool>,
    updates: tokio::sync::watch::Receiver<Option<Arc<crate::config::Config>>>,
) -> Arc<SettlementCapabilityCache> {
    let cache = Arc::new(SettlementCapabilityCache::default());
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(refresh_from_updates(
        pool,
        Arc::clone(&cache),
        updates,
        ready_tx,
    ));
    let _ = ready_rx.await;
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
pub(in crate::services::discord) async fn bootstrap_from_receiver_for_test(
    pool: Option<sqlx::PgPool>,
    updates: tokio::sync::watch::Receiver<Option<Arc<crate::config::Config>>>,
) -> Arc<SettlementCapabilityCache> {
    bootstrap_from_updates(pool, updates).await
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

    #[test]
    fn stale_probe_generation_cannot_replace_a_newer_downgrade() {
        let cache = SettlementCapabilityCache::default();
        let stale_probe = cache.begin_resolution(IntakeDeliverySettlementStage::Settle);
        let downgrade = cache.begin_resolution(IntakeDeliverySettlementStage::Off);
        assert!(cache.replace_for_generation(downgrade, SettlementCapabilities::default()));
        assert!(!cache.replace_for_generation(
            stale_probe,
            SettlementCapabilities {
                stamp_dispatched: false,
                settle_and_sweep: true,
            }
        ));
        assert_eq!(cache.current(), SettlementCapabilities::default());
    }

    #[test]
    fn downgrade_clears_new_stamps_but_preserves_the_recorded_turn_snapshot() {
        let cache = SettlementCapabilityCache::default();
        let ready = SettlementCapabilities {
            stamp_dispatched: true,
            settle_and_sweep: true,
        };
        cache.replace(ready);
        cache.record_bridge_turn_snapshot(5071, ready);

        cache.begin_resolution(IntakeDeliverySettlementStage::Settle);
        assert_eq!(
            cache.current(),
            SettlementCapabilities {
                stamp_dispatched: false,
                settle_and_sweep: true,
            }
        );
        cache.begin_resolution(IntakeDeliverySettlementStage::Off);
        assert_eq!(cache.current(), SettlementCapabilities::default());
        assert_eq!(cache.take_bridge_turn_snapshot(Some(5071)), ready);
        assert_eq!(
            cache.take_bridge_turn_snapshot(Some(5071)),
            SettlementCapabilities::default()
        );
    }
}
