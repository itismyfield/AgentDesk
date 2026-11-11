//! Channel custody is held by the caller's existing PostgreSQL transaction.
use crate::domain::intake_identity::OwnerIdentity;
use sqlx::{Postgres, Transaction};

pub(crate) async fn lock(
    tx: &mut Transaction<'_, Postgres>,
    id: &OwnerIdentity,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(id.advisory_key())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use std::time::Duration;

    #[tokio::test]
    async fn channel_custody_lock_preserves_identity_and_transaction_lifetime_pg() {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate_with_max_connections(4).await;
        let identity = OwnerIdentity::new(" CLAUDE ", " custody-lock-fixture ");
        for commit in [false, true] {
            let mut holder = pool.begin().await.unwrap();
            let mut waiter = pool.begin().await.unwrap();
            let mut observer = pool.begin().await.unwrap();
            let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *holder)
                .await
                .unwrap();
            let waiter_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *waiter)
                .await
                .unwrap();
            assert_ne!(holder_pid, waiter_pid);
            lock(&mut holder, &identity).await.unwrap();
            let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
                .bind(identity.advisory_key())
                .fetch_one(&mut *observer)
                .await
                .unwrap();
            assert!(
                !free,
                "primitive must hold the exact identity key on return"
            );
            {
                let waiting = lock(&mut waiter, &identity);
                tokio::pin!(waiting);
                let blocked = async {
                    loop {
                        let blockers: Vec<i32> = sqlx::query_scalar("SELECT pg_blocking_pids($1)")
                            .bind(waiter_pid)
                            .fetch_one(&pool)
                            .await
                            .unwrap();
                        if blockers.contains(&holder_pid) {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                };
                tokio::select! {
                    result = &mut waiting => panic!("lock completed before transaction end: {result:?}"),
                    observed = tokio::time::timeout(Duration::from_secs(10), blocked) => {
                        observed.expect("waiter should reach the same advisory lock");
                    }
                }
                if commit {
                    holder.commit().await.unwrap();
                } else {
                    holder.rollback().await.unwrap();
                }
                tokio::time::timeout(Duration::from_secs(10), waiting)
                    .await
                    .unwrap()
                    .unwrap();
            }
            let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
                .bind(identity.advisory_key())
                .fetch_one(&mut *observer)
                .await
                .unwrap();
            assert!(
                !free,
                "returned primitive keeps the waiter's transaction lock"
            );
            waiter.rollback().await.unwrap();
            let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
                .bind(identity.advisory_key())
                .fetch_one(&mut *observer)
                .await
                .unwrap();
            assert!(
                free,
                "transaction end releases custody on both finish paths"
            );
            observer.rollback().await.unwrap();
        }
        pool.close().await;
        db.drop().await;
    }
}
