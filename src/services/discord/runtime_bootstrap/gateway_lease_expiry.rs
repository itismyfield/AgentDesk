use std::time::Duration;

use crate::services::provider::ProviderKind;

/// Repeat interval of the preferred node's long-wait warning.
const PREFERRED_WAIT_WARN_INTERVAL: Duration = Duration::from_secs(600);

/// Server-side idle expiry for gateway lease sessions; `None` (configured 0) never expires.
pub(super) fn gateway_lease_idle_expiry(
    cluster: &crate::config::ClusterConfig,
) -> Option<Duration> {
    (cluster.gateway_lease_idle_expiry_secs > 0)
        .then(|| Duration::from_secs(cluster.gateway_lease_idle_expiry_secs))
}

/// Warns, with the holder's identity, once the preferred node has waited longer than
/// the configured lease idle expiry (and grace); it records the wait, not its cause.
pub(super) struct PreferredWaitWarning {
    started: tokio::time::Instant,
    next: tokio::time::Instant,
    idle_expiry_secs: u64,
}

impl PreferredWaitWarning {
    pub(super) fn start() -> Self {
        let cluster = crate::config::load_graceful().cluster;
        let after = cluster
            .gateway_lease_idle_expiry_secs
            .max(cluster.gateway_yield_grace_secs)
            + 60;
        let started = tokio::time::Instant::now();
        Self {
            started,
            next: started + Duration::from_secs(after),
            idle_expiry_secs: cluster.gateway_lease_idle_expiry_secs,
        }
    }

    pub(super) async fn observe(
        &mut self,
        pool: &sqlx::PgPool,
        lock_id: i64,
        provider: &ProviderKind,
    ) {
        if tokio::time::Instant::now() < self.next {
            return;
        }
        self.next += PREFERRED_WAIT_WARN_INTERVAL;
        let holder = describe_lock_holder(pool, lock_id).await;
        tracing::warn!(
            provider = provider.as_str(),
            waited_secs = self.started.elapsed().as_secs(),
            configured_idle_expiry_secs = self.idle_expiry_secs,
            "GATEWAY-LEASE: preferred node has waited longer than the configured lease idle expiry; holder: {holder}"
        );
    }
}

/// Read-only description of the backend holding `lock_id`.
async fn describe_lock_holder(pool: &sqlx::PgPool, lock_id: i64) -> String {
    type Holder = (
        i32,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
    );
    let holder = sqlx::query_as::<_, Holder>(
        r#"
        SELECT a.pid, a.application_name, a.state,
               EXTRACT(EPOCH FROM NOW() - a.state_change)::BIGINT,
               EXTRACT(EPOCH FROM NOW() - a.backend_start)::BIGINT
          FROM pg_locks l
          JOIN pg_stat_activity a ON a.pid = l.pid
         WHERE l.locktype = 'advisory'
           AND l.granted
           AND l.classid = (($1::BIGINT >> 32) & 4294967295)::OID
           AND l.objid = ($1::BIGINT & 4294967295)::OID
           AND l.objsubid = 1
         LIMIT 1
        "#,
    )
    .bind(lock_id)
    .fetch_optional(pool)
    .await;
    match holder {
        Ok(Some((pid, application_name, state, idle_secs, age_secs))) => format!(
            "pid={pid} application_name={} state={} idle_secs={} backend_age_secs={}",
            application_name.unwrap_or_default(),
            state.unwrap_or_default(),
            idle_secs.unwrap_or(-1),
            age_secs.unwrap_or(-1)
        ),
        Ok(None) => "none visible".to_string(),
        Err(error) => format!("unavailable ({error})"),
    }
}
