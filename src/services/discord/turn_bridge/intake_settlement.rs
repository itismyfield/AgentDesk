//! Receipt-backed settlement at the terminal bridge boundary.
//!
//! Honest boundary: settlement requires a `Ready` capability probe, so this
//! path cannot be production-verified while #5245 leaves the required schema
//! migrations unapplied. The done-writer gate is a lexical scan and cannot see
//! direct SQL, glob imports, or generated calls; this slice neither widens nor
//! narrows that declared limitation.

use super::InflightTurnState;
use crate::db::intake_outbox_delivery_proof::{
    IntakeSettlementSource, settle_intake_done_from_receipt,
};
use crate::services::discord::SharedData;
use crate::services::discord::runtime_bootstrap::intake_delivery_capability::SettlementCapabilities;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// The disposition of a bridge turn at its one normal terminal exit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BridgeTurnDisposition {
    /// A retry still owns the inflight row; leave the intake row open.
    PreservedForRetry,
    /// This bridge committed terminal delivery.
    Committed,
    /// A watcher or standby relay owns the terminal delivery.
    RelayOwnerHandoff,
    /// No body was delivered and no retry was retained.
    NoBodyNoRetry,
}

impl BridgeTurnDisposition {
    fn settlement_source(self) -> Option<IntakeSettlementSource> {
        match self {
            Self::PreservedForRetry => None,
            Self::Committed => Some(IntakeSettlementSource::Committed),
            Self::RelayOwnerHandoff => Some(IntakeSettlementSource::RelayOwnerHandoff),
            Self::NoBodyNoRetry => Some(IntakeSettlementSource::NoBodyNoRetry),
        }
    }
}

/// Applies the terminal-outcome precedence contract.
pub(super) fn classify(
    terminal_delivery_committed: bool,
    status_panel_terminal_committed: bool,
    preserve_inflight_for_cleanup_retry: bool,
    bridge_skip_holder_owns_inflight: bool,
    relay_owner_present: bool,
) -> BridgeTurnDisposition {
    if preserve_inflight_for_cleanup_retry || bridge_skip_holder_owns_inflight {
        BridgeTurnDisposition::PreservedForRetry
    } else if terminal_delivery_committed || status_panel_terminal_committed {
        BridgeTurnDisposition::Committed
    } else if relay_owner_present {
        BridgeTurnDisposition::RelayOwnerHandoff
    } else {
        BridgeTurnDisposition::NoBodyNoRetry
    }
}

const SOURCE_COUNT: usize = 4;

struct SettlementCounters {
    cas_won: [AtomicU64; SOURCE_COUNT],
    cas_noop: [AtomicU64; SOURCE_COUNT],
    write_failed: [AtomicU64; SOURCE_COUNT],
}

impl Default for SettlementCounters {
    fn default() -> Self {
        Self {
            cas_won: std::array::from_fn(|_| AtomicU64::new(0)),
            cas_noop: std::array::from_fn(|_| AtomicU64::new(0)),
            write_failed: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

static SETTLEMENT_COUNTERS: OnceLock<SettlementCounters> = OnceLock::new();

fn counters() -> &'static SettlementCounters {
    SETTLEMENT_COUNTERS.get_or_init(SettlementCounters::default)
}

const fn source_index(source: IntakeSettlementSource) -> usize {
    match source {
        IntakeSettlementSource::Committed => 0,
        IntakeSettlementSource::RelayOwnerHandoff => 1,
        IntakeSettlementSource::NoBodyNoRetry => 2,
        IntakeSettlementSource::Sweep => 3,
    }
}

fn record_settlement_result(
    outbox_id: i64,
    source: IntakeSettlementSource,
    result: Result<bool, sqlx::Error>,
) {
    let index = source_index(source);
    match result {
        Ok(true) => {
            counters().cas_won[index].fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                counter = "intake_settlement_cas_won",
                source = source.as_str(),
                outbox_id,
                "intake settlement CAS reached done"
            );
        }
        Ok(false) => {
            counters().cas_noop[index].fetch_add(1, Ordering::Relaxed);
            tracing::info!(
                counter = "intake_settlement_cas_noop",
                source = source.as_str(),
                outbox_id,
                "intake settlement CAS was already terminal or won by another actor"
            );
        }
        Err(error) => {
            counters().write_failed[index].fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                counter = "intake_settlement_write_failed",
                source = source.as_str(),
                outbox_id,
                %error,
                "intake settlement SQL failed; leaving turn outcome unchanged"
            );
        }
    }
}

/// Settles the intake row associated with a bridge at the terminal exit.
///
/// The inflight identity is read here, and nowhere else in the terminal
/// delivery path.  Settlement is gated by the resolved capability snapshot;
/// SQL errors are counted and swallowed because terminal delivery has already
/// completed and turning this error into a worker failure can create a
/// duplicate retry.
pub(super) async fn settle_intake_row_at_bridge_exit(
    shared: &std::sync::Arc<SharedData>,
    inflight_state: &InflightTurnState,
    disposition: BridgeTurnDisposition,
    caps: SettlementCapabilities,
) {
    if !caps.settle_and_sweep {
        return;
    }
    let Some(source) = disposition.settlement_source() else {
        return;
    };
    let Some(outbox_id) = inflight_state.intake_outbox_id() else {
        return;
    };
    let Some(pool) = shared.pg_pool.as_ref() else {
        return;
    };

    let result = match pool.acquire().await {
        Ok(mut connection) => {
            settle_intake_done_from_receipt(&mut connection, outbox_id, source).await
        }
        Err(error) => Err(error),
    };
    record_settlement_result(outbox_id, source, result);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::db::intake_outbox::mark_done;
    use crate::db::intake_outbox_status::IntakeOutboxStatus;
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::provider::ProviderKind;
    use chrono::{DateTime, Utc};
    use std::sync::Arc;
    use tokio::sync::Barrier;

    const READY_CAPABILITIES: SettlementCapabilities = SettlementCapabilities {
        stamp_dispatched: true,
        settle_and_sweep: true,
    };
    const BELOW_SETTLE_CAPABILITIES: SettlementCapabilities = SettlementCapabilities {
        stamp_dispatched: false,
        settle_and_sweep: false,
    };

    async fn seed(
        pool: &sqlx::PgPool,
        key: &str,
        status: IntakeOutboxStatus,
        spawned_at: Option<DateTime<Utc>>,
        dispatched_at: Option<DateTime<Utc>>,
    ) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO public.intake_outbox (
                target_instance_id, forwarded_by_instance_id, channel_id,
                user_msg_id, request_owner_id, user_text, turn_kind, agent_id,
                status, claim_owner, spawned_at, dispatched_at
             ) VALUES (
                'worker', 'leader', $1, $1, 'user', 'hello', 'standard', 'agent',
                $2, 'dispatch-worker', $3, $4
             ) RETURNING id",
        )
        .bind(key)
        .bind(status)
        .bind(spawned_at)
        .bind(dispatched_at)
        .fetch_one(pool)
        .await
        .expect("seed intake outbox row") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
    }

    async fn status(pool: &sqlx::PgPool, id: i64) -> IntakeOutboxStatus {
        sqlx::query_scalar("SELECT status FROM public.intake_outbox WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("read intake outbox status") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
    }

    async fn audit(
        pool: &sqlx::PgPool,
        id: i64,
    ) -> (
        IntakeOutboxStatus,
        Option<DateTime<Utc>>,
        Option<String>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
    ) {
        sqlx::query_as(
            "SELECT status, completed_at, claim_owner, spawned_at, dispatched_at
             FROM public.intake_outbox WHERE id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("read intake outbox audit fields") // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
    }

    fn state_for(id: i64) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            42,
            Some("settlement-test".to_owned()),
            7,
            8,
            9,
            "hello".to_owned(),
            None,
            Some("AgentDesk-claude-adk-settlement-test".to_owned()),
            None,
            None,
            0,
        );
        state.adopt_intake_outbox(Some(id));
        state
    }

    async fn shared_with_pool(pool: sqlx::PgPool) -> Arc<SharedData> {
        crate::services::discord::make_shared_data_for_tests_with_storage_and_intake_capabilities(
            Some(pool),
            crate::services::discord::runtime_bootstrap::intake_delivery_capability::SettlementCapabilityCache::for_test(
                READY_CAPABILITIES,
            ),
        )
    }

    #[test]
    fn classify_preserve_precedes_every_terminal_receipt() {
        assert_eq!(
            classify(true, true, true, false, true),
            BridgeTurnDisposition::PreservedForRetry
        );
        assert_eq!(
            classify(false, false, false, true, true),
            BridgeTurnDisposition::PreservedForRetry
        );
    }

    #[tokio::test]
    async fn settle_from_spawned_and_from_dispatched_both_reach_done_pg() {
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        let now = Utc::now();
        let spawned = seed(
            &pool,
            "settle-spawned",
            IntakeOutboxStatus::Spawned,
            Some(now),
            None,
        )
        .await;
        let dispatched = seed(
            &pool,
            "settle-dispatched",
            IntakeOutboxStatus::Dispatched,
            Some(now),
            Some(now),
        )
        .await;
        let shared = shared_with_pool(pool.clone()).await;
        settle_intake_row_at_bridge_exit(
            &shared,
            &state_for(spawned),
            BridgeTurnDisposition::Committed,
            READY_CAPABILITIES,
        )
        .await;
        settle_intake_row_at_bridge_exit(
            &shared,
            &state_for(dispatched),
            BridgeTurnDisposition::Committed,
            READY_CAPABILITIES,
        )
        .await;
        assert_eq!(status(&pool, spawned).await, IntakeOutboxStatus::Done);
        assert_eq!(status(&pool, dispatched).await, IntakeOutboxStatus::Done);
        pool.close().await;
        database.drop().await;
    }

    #[tokio::test]
    async fn settle_is_idempotent_and_preserves_dispatch_audit_fields_pg() {
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        let spawned_at = Utc::now();
        let dispatched_at = spawned_at + chrono::Duration::seconds(1);
        let id = seed(
            &pool,
            "settle-idempotent",
            IntakeOutboxStatus::Dispatched,
            Some(spawned_at),
            Some(dispatched_at),
        )
        .await;
        let shared = shared_with_pool(pool.clone()).await;
        settle_intake_row_at_bridge_exit(
            &shared,
            &state_for(id),
            BridgeTurnDisposition::Committed,
            READY_CAPABILITIES,
        )
        .await;
        let first = audit(&pool, id).await;
        settle_intake_row_at_bridge_exit(
            &shared,
            &state_for(id),
            BridgeTurnDisposition::Committed,
            READY_CAPABILITIES,
        )
        .await;
        let second = audit(&pool, id).await;
        assert_eq!(first.0, IntakeOutboxStatus::Done);
        assert_eq!(first.2, Some("dispatch-worker".to_owned()));
        assert_eq!(first.3, Some(spawned_at));
        assert_eq!(first.4, Some(dispatched_at));
        assert_eq!(
            (first.0, first.2, first.3, first.4),
            (second.0, second.2, second.3, second.4)
        );
        pool.close().await;
        database.drop().await;
    }

    #[tokio::test]
    async fn settle_does_not_touch_terminal_or_pre_spawn_states_pg() {
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        let now = Utc::now();
        let statuses = [
            IntakeOutboxStatus::Pending,
            IntakeOutboxStatus::Claimed,
            IntakeOutboxStatus::Accepted,
            IntakeOutboxStatus::Done,
            IntakeOutboxStatus::Unknown,
            IntakeOutboxStatus::FailedPreAccept,
            IntakeOutboxStatus::FailedPostAccept,
        ];
        let shared = shared_with_pool(pool.clone()).await;
        for (index, state) in statuses.into_iter().enumerate() {
            let id = seed(
                &pool,
                &format!("settle-noop-{index}"),
                state,
                Some(now),
                None,
            )
            .await;
            settle_intake_row_at_bridge_exit(
                &shared,
                &state_for(id),
                BridgeTurnDisposition::Committed,
                READY_CAPABILITIES,
            )
            .await;
            assert_eq!(status(&pool, id).await, state);
        }
        pool.close().await;
        database.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worker_mark_done_and_settlement_converge_under_either_order_pg() {
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        let now = Utc::now();
        for (index, settlement_first) in [true, false].into_iter().enumerate() {
            let id = seed(
                &pool,
                &format!("settle-converge-{index}"),
                IntakeOutboxStatus::Spawned,
                Some(now),
                None,
            )
            .await;
            let shared = shared_with_pool(pool.clone()).await;
            let state = state_for(id);
            let barrier = Arc::new(Barrier::new(2));
            let settle_barrier = Arc::clone(&barrier);
            let settle_shared = Arc::clone(&shared);
            let settle_state = state.clone();
            let worker_pool = pool.clone();
            let settlement = async move {
                settle_barrier.wait().await;
                settle_intake_row_at_bridge_exit(
                    &settle_shared,
                    &settle_state,
                    BridgeTurnDisposition::Committed,
                    READY_CAPABILITIES,
                )
                .await;
            };
            let worker_barrier = Arc::clone(&barrier);
            let worker = async move {
                worker_barrier.wait().await;
                mark_done(&worker_pool, id, "dispatch-worker").await
            };
            if settlement_first {
                let (_, worker_result) = tokio::join!(settlement, worker);
                worker_result.expect("worker mark_done query");
            } else {
                let (worker_result, _) = tokio::join!(worker, settlement);
                worker_result.expect("worker mark_done query");
            }
            assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Done);
        }
        pool.close().await;
        database.drop().await;
    }

    #[tokio::test]
    async fn settlement_rolls_back_with_caller_transaction_pg() {
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        let now = Utc::now();
        let id = seed(
            &pool,
            "settle-rollback",
            IntakeOutboxStatus::Spawned,
            Some(now),
            None,
        )
        .await;
        let mut transaction = pool.begin().await.expect("begin caller transaction"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        assert!(
            settle_intake_done_from_receipt(
                &mut transaction,
                id,
                IntakeSettlementSource::Committed,
            )
            .await
            .expect("settle in caller transaction")
        ); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Spawned);
        transaction
            .rollback()
            .await
            .expect("rollback caller transaction"); // agentdesk-audit: allow-unwrap — PostgreSQL test assertion
        assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Spawned);
        pool.close().await;
        database.drop().await;
    }

    #[tokio::test]
    async fn preserved_for_retry_leaves_row_open_pg() {
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        let now = Utc::now();
        let id = seed(
            &pool,
            "settle-preserve",
            IntakeOutboxStatus::Dispatched,
            Some(now),
            Some(now),
        )
        .await;
        let shared = shared_with_pool(pool.clone()).await;
        settle_intake_row_at_bridge_exit(
            &shared,
            &state_for(id),
            BridgeTurnDisposition::PreservedForRetry,
            READY_CAPABILITIES,
        )
        .await;
        assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Dispatched);
        pool.close().await;
        database.drop().await;
    }

    #[tokio::test]
    async fn relay_owner_handoff_closes_row_pg() {
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        let now = Utc::now();
        let id = seed(
            &pool,
            "settle-relay-owner",
            IntakeOutboxStatus::Spawned,
            Some(now),
            None,
        )
        .await;
        let shared = shared_with_pool(pool.clone()).await;
        let disposition = classify(false, false, false, false, true);
        assert_eq!(disposition, BridgeTurnDisposition::RelayOwnerHandoff);
        settle_intake_row_at_bridge_exit(&shared, &state_for(id), disposition, READY_CAPABILITIES)
            .await;
        assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Done);
        pool.close().await;
        database.drop().await;
    }

    #[tokio::test]
    async fn cancel_prompt_replace_commit_closes_row_pg() {
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        let now = Utc::now();
        let id = seed(
            &pool,
            "settle-cancel-replace",
            IntakeOutboxStatus::Spawned,
            Some(now),
            None,
        )
        .await;
        let shared = shared_with_pool(pool.clone()).await;
        let disposition = classify(false, true, false, false, false);
        assert_eq!(disposition, BridgeTurnDisposition::Committed);
        settle_intake_row_at_bridge_exit(&shared, &state_for(id), disposition, READY_CAPABILITIES)
            .await;
        assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Done);
        pool.close().await;
        database.drop().await;
    }

    #[test]
    fn settlement_sql_error_is_swallowed_and_counted() {
        let source = IntakeSettlementSource::Committed;
        let before = counters().write_failed[source_index(source)].load(Ordering::Relaxed);
        record_settlement_result(
            99,
            source,
            Err(sqlx::Error::Protocol(
                "injected settlement SQL error".to_owned(),
            )),
        );
        let after = counters().write_failed[source_index(source)].load(Ordering::Relaxed);
        assert_eq!(after, before + 1);
    }

    #[tokio::test]
    async fn stage_below_settle_performs_no_write_pg() {
        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate().await;
        let now = Utc::now();
        let id = seed(
            &pool,
            "settle-stage-off",
            IntakeOutboxStatus::Spawned,
            Some(now),
            None,
        )
        .await;
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage_and_intake_capabilities(
            Some(pool.clone()),
            crate::services::discord::runtime_bootstrap::intake_delivery_capability::SettlementCapabilityCache::for_test(
                BELOW_SETTLE_CAPABILITIES,
            ),
        );
        settle_intake_row_at_bridge_exit(
            &shared,
            &state_for(id),
            BridgeTurnDisposition::Committed,
            BELOW_SETTLE_CAPABILITIES,
        )
        .await;
        assert_eq!(status(&pool, id).await, IntakeOutboxStatus::Spawned);
        pool.close().await;
        database.drop().await;
    }
}
