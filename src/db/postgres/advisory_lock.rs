use std::time::Duration;

use sqlx::Connection;
use sqlx::{PgConnection, PgPool};

/// A pooled session returns to the pool only after its advisory lock was explicitly released.
pub(crate) struct PooledAdvisoryLock {
    conn: Option<sqlx::pool::PoolConnection<sqlx::Postgres>>,
    namespace: String,
    key: String,
}

impl PooledAdvisoryLock {
    pub(crate) async fn acquire(
        pool: &PgPool,
        namespace: &str,
        key: &str,
    ) -> Result<Self, sqlx::Error> {
        let mut guard = Self {
            conn: Some(pool.acquire().await?),
            namespace: namespace.to_owned(),
            key: key.to_owned(),
        };
        if let Some(conn) = guard.conn.as_mut() {
            sqlx::query("SELECT pg_advisory_lock(hashtext($1), hashtext($2))")
                .bind(&guard.namespace)
                .bind(&guard.key)
                .execute(&mut **conn)
                .await?;
        }
        Ok(guard)
    }

    pub(crate) async fn unlock(mut self) -> Result<(), sqlx::Error> {
        if let Some(conn) = self.conn.as_mut() {
            sqlx::query("SELECT pg_advisory_unlock(hashtext($1), hashtext($2))")
                .bind(&self.namespace)
                .bind(&self.key)
                .execute(&mut **conn)
                .await?;
        }
        drop(self.conn.take());
        Ok(())
    }
}

impl Drop for PooledAdvisoryLock {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            // An interrupted acquisition or unlock can still own the server lock; end that session.
            drop(conn.detach());
        }
    }
}

/// Session-scoped advisory lock held on a dedicated connection; the server frees it
/// when it sees that session end (drop, `unlock()`, or an idle expiry it enforces).
pub struct AdvisoryLockLease {
    conn: PgConnection,
    lock_id: i64,
    label: String,
}

impl AdvisoryLockLease {
    pub async fn try_acquire(
        pool: &PgPool,
        lock_id: i64,
        label: impl Into<String>,
    ) -> Result<Option<Self>, String> {
        Self::try_acquire_with_application_name(pool, lock_id, label, None, None, |_| {}).await
    }

    /// Acquire under PostgreSQL `application_name`; with `idle_expiry` the server ends the
    /// session after that long without a statement, so a vanished holder cannot keep the lock.
    #[cfg(test)]
    pub async fn try_acquire_named_expiring(
        pool: &PgPool,
        lock_id: i64,
        label: impl Into<String>,
        application_name: impl Into<String>,
        idle_expiry: Option<Duration>,
    ) -> Result<Option<Self>, String> {
        Self::try_acquire_named_expiring_observed(
            pool,
            lock_id,
            label,
            application_name,
            idle_expiry,
            |_| {},
        )
        .await
    }

    pub(crate) async fn try_acquire_named_expiring_observed(
        pool: &PgPool,
        lock_id: i64,
        label: impl Into<String>,
        application_name: impl Into<String>,
        idle_expiry: Option<Duration>,
        observe: impl FnMut(Option<Result<bool, ()>>),
    ) -> Result<Option<Self>, String> {
        Self::try_acquire_with_application_name(
            pool,
            lock_id,
            label,
            Some(application_name.into()),
            idle_expiry.filter(|expiry| !expiry.is_zero()),
            observe,
        )
        .await
    }

    async fn try_acquire_with_application_name(
        pool: &PgPool,
        lock_id: i64,
        label: impl Into<String>,
        application_name: Option<String>,
        idle_expiry: Option<Duration>,
        mut observe: impl FnMut(Option<Result<bool, ()>>),
    ) -> Result<Option<Self>, String> {
        let label = label.into();
        let mut options = (*pool.connect_options()).clone();
        if let Some(application_name) = application_name {
            options = options.application_name(&application_name);
        }
        let mut conn = PgConnection::connect_with(&options)
            .await
            .map_err(|error| format!("{label} acquire advisory lock connection: {error}"))?;
        // Before the lock: a client lost right after acquiring must still leave an expiring session.
        if let Some(expiry) = idle_expiry {
            set_idle_session_timeout(&mut conn, expiry, &label).await?;
        }
        // None marks the actual SQL attempt; setup failures never invoke the observer.
        observe(None);
        let acquired = sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
            .bind(lock_id)
            .fetch_one(&mut conn)
            .await;
        observe(Some(acquired.as_ref().copied().map_err(|_| ())));
        let acquired = acquired.map_err(|error| format!("{label} try advisory lock: {error}"))?;
        if acquired {
            #[cfg(test)]
            test_seam::stall_after_lock(lock_id).await;
            Ok(Some(Self {
                conn,
                lock_id,
                label,
            }))
        } else {
            Ok(None)
        }
    }

    pub async fn keepalive(&mut self) -> Result<(), String> {
        sqlx::query("SELECT 1")
            .execute(&mut self.conn)
            .await
            .map(|_| ())
            .map_err(|error| format!("{} advisory lock keepalive: {error}", self.label))
    }

    pub async fn unlock(mut self) -> Result<(), String> {
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(self.lock_id)
            .execute(&mut self.conn)
            .await
            .map(|_| ())
            .map_err(|error| format!("{} advisory unlock {}: {error}", self.label, self.lock_id))
    }

    /// The idle timeout this lease's own session runs under, as PostgreSQL reports it.
    #[cfg(test)]
    pub(crate) async fn session_idle_timeout(&mut self) -> Result<String, sqlx::Error> {
        sqlx::query_scalar("SHOW idle_session_timeout")
            .fetch_one(&mut self.conn)
            .await
    }
}

async fn set_idle_session_timeout(
    conn: &mut PgConnection,
    expiry: Duration,
    label: &str,
) -> Result<(), String> {
    let result = sqlx::query("SELECT set_config('idle_session_timeout', $1, false)")
        .bind(format!("{}ms", expiry.as_millis()))
        .execute(&mut *conn)
        .await;
    match result {
        Ok(_) => Ok(()),
        // PostgreSQL before 14 has no such setting; the lease then behaves as before, without expiry.
        Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("42704") => {
            tracing::warn!(
                "{label}: server has no idle_session_timeout; idle lease will not expire"
            );
            Ok(())
        }
        Err(error) => Err(format!("{label} set idle session timeout: {error}")),
    }
}

/// Lets a test freeze an acquisition after the server granted the lock, with the session still open.
#[cfg(test)]
pub(crate) mod test_seam {
    use std::collections::HashSet;
    use std::sync::Mutex;

    static STALLED_LOCK_IDS: Mutex<Option<HashSet<i64>>> = Mutex::new(None);

    pub(crate) fn stall_after_lock_for(lock_id: i64) {
        STALLED_LOCK_IDS
            .lock()
            .unwrap()
            .get_or_insert_with(HashSet::new)
            .insert(lock_id);
    }

    /// Later acquisitions of `lock_id` proceed normally; an already stalled one stays frozen.
    pub(crate) fn release_stall_for(lock_id: i64) {
        if let Some(ids) = STALLED_LOCK_IDS.lock().unwrap().as_mut() {
            ids.remove(&lock_id);
        }
    }

    pub(super) async fn stall_after_lock(lock_id: i64) {
        let stalled = STALLED_LOCK_IDS
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|ids| ids.contains(&lock_id));
        if stalled {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
#[path = "advisory_lock_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "pooled_advisory_lock_pg_tests.rs"]
mod pooled_advisory_lock_pg_tests;
