//! The health request's database reads under one absolute deadline: reads that need only the
//! pool start together, and the reads that need the provider snapshot get only the time left.

use std::future::Future;
use std::time::Duration;

use tokio::time::{Instant, error::Elapsed, timeout_at};

use crate::services::health_active_session_audit::ActiveSessionAuditReport;
use crate::services::health_diagnostics::{
    self, AutoQueueCleanupBacklog, DispatchOutboxStats, HealthKvMeta, MailboxSessions,
    SessionLookups,
};

use super::AppState;
use super::runtime_profile::cluster_standby_without_gateway;

/// Every database read of one health request ends by the request start plus this.
pub(super) const HEALTH_DB_BUDGET: Duration = Duration::from_millis(1_500);

pub(super) fn deadline() -> Instant {
    Instant::now() + HEALTH_DB_BUDGET
}

/// A read that waits on the snapshot: never started once the deadline has passed.
enum Late<T> {
    Done(T),
    Elapsed,
    NotStarted,
}

async fn late<T>(deadline: Instant, read: impl Future<Output = T>) -> Late<T> {
    if Instant::now() >= deadline {
        return Late::NotStarted;
    }
    match timeout_at(deadline, read).await {
        Ok(value) => Late::Done(value),
        Err(_) => Late::Elapsed,
    }
}

/// The reads that need only the pool, each cut at the shared deadline.
pub(super) struct Initial {
    pub(super) server_up: Result<bool, Elapsed>,
    pub(super) outbox: Result<Option<DispatchOutboxStats>, Elapsed>,
    pub(super) cleanup: Result<Option<AutoQueueCleanupBacklog>, Elapsed>,
    pub(super) kv: Result<HealthKvMeta, Elapsed>,
    /// `None` on the public body.
    pub(super) audit: Option<Result<ActiveSessionAuditReport, Elapsed>>,
}

impl Initial {
    /// Whether any read was still pending when the deadline passed.
    pub(super) fn timed_out(&self) -> bool {
        self.server_up.is_err()
            || self.outbox.is_err()
            || self.cleanup.is_err()
            || self.kv.is_err()
            || matches!(self.audit, Some(Err(_)))
    }
}

/// Runs the initial reads concurrently with `alongside`, the snapshot and its late reads.
pub(super) async fn read_initial_alongside<T>(
    state: &AppState,
    detailed: bool,
    deadline: Instant,
    alongside: impl Future<Output = T>,
) -> (Initial, T) {
    let pool = state.pg_pool_ref();
    let audit = async {
        if !detailed {
            return None;
        }
        let audit = health_diagnostics::build_active_session_audit(
            pool,
            state.cluster_instance_id.as_deref(),
        );
        Some(timeout_at(deadline, audit).await)
    };
    let (server_up, outbox, cleanup, kv, audit, alongside) = tokio::join!(
        timeout_at(deadline, health_diagnostics::probe_server_up(pool)),
        timeout_at(
            deadline,
            health_diagnostics::load_dispatch_outbox_stats(pool)
        ),
        timeout_at(
            deadline,
            health_diagnostics::load_auto_queue_cleanup_backlog(pool)
        ),
        timeout_at(deadline, health_diagnostics::load_health_kv_meta(pool)),
        audit,
        alongside,
    );
    let initial = Initial {
        server_up,
        outbox,
        cleanup,
        kv,
        audit,
    };
    (initial, alongside)
}

/// The reads that need the snapshot body.
pub(super) struct LateReads {
    /// Detail only: one lookup result per mailbox channel.
    pub(super) sessions: Option<MailboxSessions>,
    /// Whether this node is a gateway standby, before the `server_up` check.
    pub(super) standby_without_gateway: bool,
    timed_out: bool,
}

impl LateReads {
    pub(super) fn timed_out(&self) -> bool {
        self.timed_out
    }
}

/// The bulk session lookup and the standby worker lookup, in the time left before `deadline`.
/// A lookup cut or never started marks every mailbox row unmeasured, never absent.
pub(super) async fn read_late(
    state: &AppState,
    detailed: bool,
    json: &serde_json::Value,
    deadline: Instant,
) -> LateReads {
    let pool = state.pg_pool_ref();
    let channels = if detailed {
        health_diagnostics::mailbox_session_channels(json)
    } else {
        Vec::new()
    };
    let reasons = json["degraded_reasons"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let sessions = async {
        if channels.is_empty() {
            return Late::Done(SessionLookups::new());
        }
        late(
            deadline,
            health_diagnostics::load_mailbox_session_states(pool, &channels),
        )
        .await
    };
    // `server_up` is applied by the caller, so this lookup need not wait on SELECT 1.
    let standby = late(
        deadline,
        cluster_standby_without_gateway(state, true, &reasons),
    );
    let (sessions, standby) = tokio::join!(sessions, standby);
    let unmeasured = |message: &str| {
        channels
            .iter()
            .map(|channel| (*channel, Err(message.to_string())))
            .collect::<SessionLookups>()
    };
    let mut timed_out = false;
    // Only a lookup that never started is marked: one cut after it started stays a failure.
    let not_started = matches!(sessions, Late::NotStarted);
    let lookups = match sessions {
        Late::Done(lookups) => lookups,
        Late::Elapsed => {
            timed_out = true;
            unmeasured("health deadline elapsed during session lookup")
        }
        Late::NotStarted => unmeasured("health deadline elapsed before session lookup"),
    };
    let sessions = MailboxSessions {
        lookups,
        not_started,
    };
    let standby_without_gateway = match standby {
        Late::Done(standby) => standby,
        Late::Elapsed => {
            timed_out = true;
            false
        }
        Late::NotStarted => false,
    };
    LateReads {
        sessions: detailed.then_some(sessions),
        standby_without_gateway,
        timed_out,
    }
}
