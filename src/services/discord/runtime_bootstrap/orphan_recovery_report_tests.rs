use super::*;
use futures::FutureExt;
use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};

thread_local! {
static FORCE_PRIMARY_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; static REFUSE_BEFORE_REQUEUE: RefCell<Option<String>> = const { RefCell::new(None) }; }
pub(super) async fn before_requeue(pool: &sqlx::PgPool, dispatch: &str) {
    let refuse = REFUSE_BEFORE_REQUEUE.with(|hook| hook.borrow().as_deref() == Some(dispatch));
    if refuse {
        sqlx::query("UPDATE task_dispatches SET status = 'cancelled' WHERE id = $1")
            .bind(dispatch)
            .execute(pool)
            .await
            .unwrap();
    }
}
fn event(report: &OrphanPass, step: OrphanStep, outcome: &PassOutcome) -> bool {
    report
        .events
        .iter()
        .any(|(seen, _, result)| *seen == step && result == outcome)
}

#[tokio::test]
async fn r0l_orphan_once_followers_return_immediately_and_share_one_terminal_result() {
    let once = OrphanOnce::default();
    let calls = AtomicUsize::new(0);
    let (release, latch) = tokio::sync::oneshot::channel::<()>();
    let mut winner = Box::pin(once.run(async {
        calls.fetch_add(1, Ordering::SeqCst);
        assert!(matches!(*once.state.borrow(), OrphanOnceState::Started));
        latch.await.unwrap();
        OrphanPass::default()
    }));
    assert!(futures::poll!(winner.as_mut()).is_pending());
    let mut follower = Box::pin(once.run(async {
        calls.fetch_add(1, Ordering::SeqCst);
        OrphanPass::default()
    }));
    assert!(matches!(
        futures::poll!(follower.as_mut()),
        std::task::Poll::Ready(OrphanOnceState::Started)
    ));
    drop(follower);
    release.send(()).unwrap();
    let OrphanOnceState::Completed(first) = winner.await else {
        panic!("terminal pass")
    };
    let OrphanOnceState::Completed(second) = once
        .run(async {
            calls.fetch_add(1, Ordering::SeqCst);
            OrphanPass::default()
        })
        .await
    else {
        panic!("shared pass")
    };
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn r0l_orphan_once_drop_abort_and_panic_settle_without_rerun() {
    let once = OrphanOnce::default();
    let mut winner = Box::pin(once.run(std::future::pending()));
    assert!(futures::poll!(winner.as_mut()).is_pending());
    drop(winner);
    assert!(matches!(
        *once.state.borrow(),
        OrphanOnceState::Failed("cancelled")
    ));
    assert!(matches!(
        once.run(async { panic!("must not rerun") }).await,
        OrphanOnceState::Failed("cancelled")
    ));

    let once = Arc::new(OrphanOnce::default());
    let task_once = once.clone();
    let (started, observed) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        task_once
            .run(async {
                started.send(()).unwrap();
                std::future::pending().await
            })
            .await
    });
    observed.await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(matches!(
        *once.state.borrow(),
        OrphanOnceState::Failed("cancelled")
    ));

    let once = OrphanOnce::default();
    assert!(
        std::panic::AssertUnwindSafe(once.run(async { panic!("pass panic") }))
            .catch_unwind()
            .await
            .is_err()
    );
    assert!(matches!(
        *once.state.borrow(),
        OrphanOnceState::Failed("panicked")
    ));
    assert!(matches!(
        once.run(async { panic!("must not rerun") }).await,
        OrphanOnceState::Failed("panicked")
    ));
}

#[tokio::test]
async fn r0l_orphan_missing_pid_observation_failure_and_missing_pg_are_distinct() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let missing = recover_orphan_pending_dispatches_once(None).await;
    assert!(event(&missing, OrphanStep::Pid, &PassOutcome::Missing));
    assert!(event(
        &missing,
        OrphanStep::Postgres,
        &PassOutcome::Unavailable
    ));
    let pid = crate::cli::agentdesk_runtime_root()
        .unwrap()
        .join("runtime/dcserver.pid");
    std::fs::create_dir_all(pid.parent().unwrap()).unwrap();
    std::fs::write(&pid, "1").unwrap();
    let no_pg = recover_orphan_pending_dispatches_once(None).await;
    assert!(event(
        &no_pg,
        OrphanStep::Postgres,
        &PassOutcome::Unavailable
    ));
    assert!(!event(&no_pg, OrphanStep::Query, &PassOutcome::Empty));
    let runtime_dir = pid.parent().unwrap();
    std::fs::rename(runtime_dir, runtime_dir.with_file_name("runtime-saved")).unwrap();
    std::fs::write(runtime_dir, "not a directory").unwrap();
    let failed = recover_orphan_pending_dispatches_once(None).await;
    assert!(
        failed
            .events
            .iter()
            .any(|(step, _, result)| *step == OrphanStep::Pid
                && matches!(result, PassOutcome::Failed(_)))
    );
}

async fn seed(pool: &sqlx::PgPool, agent: &str, dispatch: &str) {
    crate::db::agents::insert_agent_channels_for_tests(pool, agent, Some("632551"), None).await;
    let card = format!("card-{dispatch}");
    sqlx::query("INSERT INTO kanban_cards (id, title, status, assigned_agent_id) VALUES ($1, $1, 'in_progress', $2)").bind(&card).bind(agent).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at) VALUES ($1, $2, $3, 'implementation', 'pending', $1, NOW() - INTERVAL '2 days')")
        .bind(dispatch).bind(card).bind(agent).execute(pool).await.unwrap();
}
fn pid() {
    let path = crate::cli::agentdesk_runtime_root()
        .unwrap()
        .join("runtime/dcserver.pid");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "1").unwrap();
}

#[tokio::test]
async fn r0l_orphan_empty_query_decode_and_hygiene_failures_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    pid();
    let empty = recover_orphan_pending_dispatches_once(Some(&pool)).await;
    assert!(event(&empty, OrphanStep::Query, &PassOutcome::Empty));
    assert!(event(
        &empty,
        OrphanStep::HygieneTerminal,
        &PassOutcome::Done(0)
    ));
    assert!(event(
        &empty,
        OrphanStep::HygieneMissing,
        &PassOutcome::Done(0)
    ));
    seed(&pool, "r0l-decode", "r0l-decode-dispatch").await;
    sqlx::query("ALTER TABLE task_dispatches ALTER COLUMN title DROP NOT NULL")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE task_dispatches SET title = NULL WHERE id = 'r0l-decode-dispatch'")
        .execute(&pool)
        .await
        .unwrap();
    let decode = recover_orphan_pending_dispatches_once(Some(&pool)).await;
    assert!(
        decode
            .events
            .iter()
            .any(|(step, _, result)| *step == OrphanStep::Decode
                && matches!(result, PassOutcome::Failed(_)))
    );
    assert!(
        !decode
            .events
            .iter()
            .any(|(step, _, _)| *step == OrphanStep::Requeue)
    );
    assert!(
        !event(&decode, OrphanStep::Query, &PassOutcome::Empty),
        "failed decoding is not a normal empty query"
    );
    seed(&pool, "r0l-hygiene-decode", "r0l-hygiene-decode-dispatch").await;
    sqlx::query(
        "UPDATE task_dispatches SET status = 'completed' WHERE id = 'r0l-hygiene-decode-dispatch'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("ALTER TABLE sessions ALTER COLUMN session_key DROP NOT NULL")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sessions (session_key, agent_id, status, active_dispatch_id) VALUES (NULL, 'r0l-hygiene-decode', 'turn_active', 'r0l-hygiene-decode-dispatch')").execute(&pool).await.unwrap();
    let hygiene_decode = recover_orphan_pending_dispatches_once(Some(&pool)).await;
    assert!(
        hygiene_decode
            .events
            .iter()
            .any(|(step, _, result)| *step == OrphanStep::HygieneDecode
                && matches!(result, PassOutcome::Failed(_))),
        "{hygiene_decode:?}"
    );
    sqlx::query("ALTER TABLE task_dispatches RENAME TO r0l_missing_dispatches")
        .execute(&pool)
        .await
        .unwrap();
    let failed = recover_orphan_pending_dispatches_once(Some(&pool)).await;
    for expected in [
        OrphanStep::Query,
        OrphanStep::HygieneTerminal,
        OrphanStep::HygieneMissing,
    ] {
        assert!(
            failed
                .events
                .iter()
                .any(|(step, _, result)| *step == expected
                    && matches!(result, PassOutcome::Failed(_))),
            "{expected:?}: {failed:?}"
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn r0l_orphan_requeue_success_refusal_failure_and_protection_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    pid();
    seed(&pool, "r0l-success", "r0l-success-dispatch").await;
    let success = recover_orphan_pending_dispatches_once(Some(&pool)).await;
    assert!(
        event(&success, OrphanStep::Requeue, &PassOutcome::Done(1)),
        "{success:?}"
    );
    sqlx::query("UPDATE task_dispatches SET status = 'completed'")
        .execute(&pool)
        .await
        .unwrap();
    seed(&pool, "r0l-refused", "r0l-refused-dispatch").await;
    REFUSE_BEFORE_REQUEUE.with(|hook| *hook.borrow_mut() = Some("r0l-refused-dispatch".into()));
    let refused = recover_orphan_pending_dispatches_once(Some(&pool)).await;
    let _ = REFUSE_BEFORE_REQUEUE.with(|hook| hook.borrow_mut().take());
    assert!(event(&refused, OrphanStep::Requeue, &PassOutcome::Refused));
    seed(&pool, "r0l-error", "r0l-error-dispatch").await;
    sqlx::query("ALTER TABLE dispatch_outbox RENAME TO r0l_missing_outbox")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE kv_meta RENAME TO r0l_missing_kv")
        .execute(&pool)
        .await
        .unwrap();
    FORCE_PRIMARY_FAILURE.set(true);
    let failed = recover_orphan_pending_dispatches_once(Some(&pool)).await;
    FORCE_PRIMARY_FAILURE.set(false);
    assert!(
        failed
            .events
            .iter()
            .any(|(step, _, result)| *step == OrphanStep::Requeue
                && matches!(result, PassOutcome::Failed(_)))
    );
    assert!(
        failed
            .events
            .iter()
            .any(|(step, subject, result)| *step == OrphanStep::Marker
                && subject == "r0l-error-dispatch"
                && matches!(result, PassOutcome::Failed(_))),
        "fallback deletion failure is retained: {failed:?}"
    );
    sqlx::query("ALTER TABLE r0l_missing_kv RENAME TO kv_meta")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sessions (session_key, agent_id, status, active_dispatch_id, channel_id) VALUES ('r0l-hygiene', 'r0l-success', 'turn_active', 'r0l-success-dispatch', '632551')").execute(&pool).await.unwrap();
    let gate =
        crate::services::discord::input_runtime::fence::Gate::protect(ProviderKind::Claude, 632551)
            .unwrap();
    let _health = crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
    let held = recover_orphan_pending_dispatches_once(Some(&pool)).await;
    assert!(event(&held, OrphanStep::Protection, &PassOutcome::Deferred));
    assert!(event(
        &held,
        OrphanStep::HygieneScan,
        &PassOutcome::Deferred
    ));
    assert!(
        !held
            .events
            .iter()
            .any(|(step, _, _)| *step == OrphanStep::Requeue)
    );
    pool.close().await;
    db.drop().await;
}

pub(super) fn mutant(name: &str) -> bool {
    std::env::var("ADK_R0L_MUTANT").ok().as_deref() == Some(name)
}

pub(super) fn force_primary_failure() -> bool {
    FORCE_PRIMARY_FAILURE.get()
}
