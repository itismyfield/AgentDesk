use super::*;
use crate::db::postgres;
use std::time::Instant;

const LABEL: &str = "advisory lock idle expiry";
const EXPIRY: Duration = Duration::from_secs(2);

struct Db {
    pool: PgPool,
    admin: String,
    database: String,
    _lifecycle: postgres::PostgresTestLifecycleGuard,
}

impl Db {
    async fn new() -> Option<Self> {
        let lifecycle = postgres::lock_test_lifecycle();
        let Some(base) = postgres::postgres_test_database_url_base() else {
            eprintln!("SKIP: isolated PostgreSQL fixture is not configured");
            return None;
        };
        let admin = format!(
            "{base}/{}",
            std::env::var("POSTGRES_TEST_ADMIN_DB").unwrap_or_else(|_| "postgres".into())
        );
        let database = format!("agentdesk_lease_expiry_{}", uuid::Uuid::new_v4().simple());
        postgres::create_test_database(&admin, &database, LABEL)
            .await
            .unwrap();
        let pool = postgres::connect_test_pool(&format!("{base}/{database}"), LABEL)
            .await
            .unwrap();
        Some(Self {
            pool,
            admin,
            database,
            _lifecycle: lifecycle,
        })
    }

    async fn expiring(&self, lock_id: i64, expiry: Option<Duration>) -> Option<AdvisoryLockLease> {
        AdvisoryLockLease::try_acquire_named_expiring(&self.pool, lock_id, LABEL, LABEL, expiry)
            .await
            .unwrap()
    }

    async fn competitor(&self, lock_id: i64) -> Option<AdvisoryLockLease> {
        AdvisoryLockLease::try_acquire(&self.pool, lock_id, LABEL)
            .await
            .unwrap()
    }

    /// Polls a plain competitor until it wins `lock_id`, or `None` once `within` passes.
    async fn competitor_wins_within(
        &self,
        lock_id: i64,
        within: Duration,
    ) -> Option<AdvisoryLockLease> {
        tokio::time::timeout(within, async {
            loop {
                if let Some(lease) = self.competitor(lock_id).await {
                    break lease;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .ok()
    }

    async fn granted(&self, lock_id: i64) -> bool {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND granted AND objid=$1::BIGINT::OID AND objsubid=1)",
        )
        .bind(lock_id)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn close(self) {
        postgres::close_test_pool(self.pool, LABEL).await.unwrap();
        postgres::drop_test_database(&self.admin, &self.database, LABEL)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn keepalive_holds_an_expiring_lease_and_silence_releases_it_pg() {
    let Some(db) = Db::new().await else {
        return;
    };
    let mut lease = db.expiring(5_631_001, Some(EXPIRY)).await.unwrap();
    let kept_until = Instant::now() + EXPIRY * 3;
    while Instant::now() < kept_until {
        lease.keepalive().await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    lease.keepalive().await.unwrap();
    assert!(
        db.competitor(5_631_001).await.is_none(),
        "a lease kept alive past its expiry stays held"
    );
    let silent_since = Instant::now();
    let winner = db
        .competitor_wins_within(5_631_001, EXPIRY * 4)
        .await
        .expect("a silent lease session expires and frees the lock");
    assert!(silent_since.elapsed() >= EXPIRY / 2);
    assert!(
        lease.keepalive().await.is_err(),
        "the expired session is gone"
    );
    winner.unlock().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn expiry_is_in_force_before_the_lock_is_granted_pg() {
    let Some(db) = Db::new().await else {
        return;
    };
    test_seam::stall_after_lock_for(5_631_002);
    let pool = db.pool.clone();
    let stalled = tokio::spawn(async move {
        AdvisoryLockLease::try_acquire_named_expiring(&pool, 5_631_002, LABEL, LABEL, Some(EXPIRY))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !db.granted(5_631_002).await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the stalled acquisition holds the lock with its session open");
    test_seam::release_stall_for(5_631_002);
    let winner = db
        .competitor_wins_within(5_631_002, EXPIRY * 4)
        .await
        .expect("a session frozen right after the grant still expires");
    assert!(!stalled.is_finished(), "the stalled client never resumed");
    stalled.abort();
    winner.unlock().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn idle_expiry_ends_only_the_expiring_lease_session_pg() {
    let Some(db) = Db::new().await else {
        return;
    };
    let mut pooled = db.pool.acquire().await.unwrap();
    let pooled_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *pooled)
        .await
        .unwrap();
    let mut plain = db.competitor(5_631_003).await.unwrap();
    let mut disabled = db.expiring(5_631_004, Some(Duration::ZERO)).await.unwrap();
    let expiring = db.expiring(5_631_005, Some(EXPIRY)).await.unwrap();
    let winner = db
        .competitor_wins_within(5_631_005, EXPIRY * 4)
        .await
        .expect("the expiring lease session ends");
    tokio::time::sleep(EXPIRY).await;
    assert!(db.competitor(5_631_003).await.is_none(), "plain lease kept");
    assert!(db.competitor(5_631_004).await.is_none(), "zero expiry kept");
    assert_eq!(plain.session_idle_timeout().await.unwrap(), "0");
    assert_eq!(disabled.session_idle_timeout().await.unwrap(), "0");
    let pid_now: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *pooled)
        .await
        .unwrap();
    assert_eq!(pid_now, pooled_pid, "pooled session survives untouched");
    let pooled_timeout: String = sqlx::query_scalar("SHOW idle_session_timeout")
        .fetch_one(&mut *pooled)
        .await
        .unwrap();
    assert_eq!(pooled_timeout, "0");
    drop((pooled, expiring));
    winner.unlock().await.unwrap();
    plain.unlock().await.unwrap();
    disabled.unlock().await.unwrap();
    db.close().await;
}
