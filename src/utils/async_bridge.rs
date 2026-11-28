use sqlx::PgPool;
use std::{
    any::Any,
    future::Future,
    pin::Pin,
    sync::mpsc,
    time::{Duration, Instant},
};

fn panic_payload_to_string(payload: &(dyn Any + Send + 'static)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "unknown panic payload".to_string()
}

fn build_runtime<E>(map_runtime_error: impl Fn(String) -> E) -> Result<tokio::runtime::Runtime, E> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|error| map_runtime_error(format!("tokio runtime: {error}")))
}

fn current_bridge_deadline_budget<E, M>(map_runtime_error: M) -> Result<Option<Duration>, E>
where
    M: Fn(String) -> E + Copy,
{
    match super::bridge_deadline::remaining() {
        Some(remaining) if remaining.is_zero() => Err(map_runtime_error(
            "bridge deadline passed before async bridge started".to_string(),
        )),
        Some(remaining) => Ok(Some(remaining)),
        None => Ok(None),
    }
}

fn apply_bridge_deadline<F, T, E, M>(
    future: F,
    deadline: Option<Duration>,
    map_runtime_error: M,
) -> Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'static>>
where
    F: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
    M: Fn(String) -> E + Copy + Send + 'static,
{
    Box::pin(async move {
        match deadline {
            Some(budget) => match tokio::time::timeout(budget, future).await {
                Ok(result) => result,
                Err(_) => Err(map_runtime_error(
                    "bridge deadline exceeded during async bridge operation".to_string(),
                )),
            },
            None => future.await,
        }
    })
}

async fn apply_bridge_deadline_then_cleanup<R, T, E, M, F, C, CF>(
    future: F,
    deadline: Option<Duration>,
    map_runtime_error: M,
    cleanup_factory: CF,
) -> Result<T, E>
where
    F: Future<Output = Result<(R, T), E>> + Send + 'static,
    R: Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
    M: Fn(String) -> E + Copy + Send + 'static,
    C: Future<Output = ()> + Send + 'static,
    CF: FnOnce(R) -> C + Send + 'static,
{
    let (resource, result) = apply_bridge_deadline(future, deadline, map_runtime_error).await?;
    cleanup_factory(resource).await;
    Ok(result)
}

fn block_on_runtime_thread<F, T, E, M>(future: F, map_runtime_error: M) -> Result<T, E>
where
    F: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
    M: Fn(String) -> E + Copy + Send + 'static,
{
    let timing = crate::logging::intake_timing::carry();
    match std::thread::spawn(move || {
        let _timing = timing.entered();
        build_runtime(map_runtime_error)?.block_on(future)
    })
    .join()
    {
        Ok(result) => result,
        Err(payload) => Err(map_runtime_error(format!(
            "tokio bridge thread panicked: {}",
            panic_payload_to_string(payload.as_ref())
        ))),
    }
}

async fn build_bridge_pg_pool<E>(
    source_pool: &PgPool,
    map_runtime_error: impl Fn(String) -> E,
) -> Result<PgPool, E> {
    let connect_options = (*source_pool.connect_options()).clone();
    source_pool
        .options()
        .clone()
        .connect_with(connect_options)
        .await
        .map_err(|error| map_runtime_error(format!("connect postgres bridge pool: {error}")))
}

pub fn block_on_result<F, T, E, M>(future: F, map_runtime_error: M) -> Result<T, E>
where
    F: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
    M: Fn(String) -> E + Copy + Send + 'static,
{
    let deadline = current_bridge_deadline_budget(map_runtime_error)?;
    let future = apply_bridge_deadline(future, deadline, map_runtime_error);
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        if tokio::task::try_id().is_none() {
            return block_on_runtime_thread(future, map_runtime_error);
        }
        return match handle.runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| handle.block_on(future))
            }
            tokio::runtime::RuntimeFlavor::CurrentThread => {
                block_on_runtime_thread(future, map_runtime_error)
            }
            _ => block_on_runtime_thread(future, map_runtime_error),
        };
    }

    build_runtime(map_runtime_error)?.block_on(future)
}

pub fn block_on_pg_result<F, T, E, M, B>(
    pool: &PgPool,
    future_factory: B,
    map_runtime_error: M,
) -> Result<T, E>
where
    F: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
    M: Fn(String) -> E + Copy + Send + 'static,
    B: FnOnce(PgPool) -> F + Send + 'static,
{
    if let Some(deadline) = super::bridge_deadline::current_work_deadline() {
        return block_on_pg_until(pool, deadline, future_factory, map_runtime_error).map_err(
            |error| match error {
                PgWorkError::Deadline => {
                    map_runtime_error("postgres bridge work deadline exceeded".to_string())
                }
                PgWorkError::Other(error) => error,
            },
        );
    }
    let deadline = current_bridge_deadline_budget(map_runtime_error)?;
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        if tokio::task::try_id().is_none() {
            return run_pg_bridge_thread(pool, future_factory, map_runtime_error, deadline);
        }
        return match handle.runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => {
                let pool = pool.clone();
                tokio::task::block_in_place(|| {
                    handle.block_on(apply_bridge_deadline(
                        future_factory(pool),
                        deadline,
                        map_runtime_error,
                    ))
                })
            }
            _ => run_pg_bridge_thread(pool, future_factory, map_runtime_error, deadline),
        };
    }

    run_pg_bridge_thread(pool, future_factory, map_runtime_error, deadline)
}

fn run_pg_bridge_thread<F, T, E, M, B>(
    pool: &PgPool,
    future_factory: B,
    map_runtime_error: M,
    deadline: Option<Duration>,
) -> Result<T, E>
where
    F: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
    M: Fn(String) -> E + Copy + Send + 'static,
    B: FnOnce(PgPool) -> F + Send + 'static,
{
    let source_pool = pool.clone();
    let timing = crate::logging::intake_timing::carry();
    match std::thread::spawn(move || {
        let _timing = timing.entered();
        let runtime = build_runtime(map_runtime_error)?;
        let operation = async move {
            let bridge_pool = build_bridge_pg_pool(&source_pool, map_runtime_error).await?;
            let result = future_factory(bridge_pool.clone()).await;
            Ok((bridge_pool, result))
        };
        let result = runtime.block_on(apply_bridge_deadline_then_cleanup(
            operation,
            deadline,
            map_runtime_error,
            |bridge_pool: PgPool| async move {
                bridge_pool.close().await;
            },
        ))?;
        result
    })
    .join()
    {
        Ok(result) => result,
        Err(payload) => Err(map_runtime_error(format!(
            "tokio bridge thread panicked: {}",
            panic_payload_to_string(payload.as_ref())
        ))),
    }
}

pub enum PgWorkError<E = String> {
    Deadline,
    Other(E),
}

pub fn block_on_pg_result_before<F, T>(
    pool: &PgPool,
    reserve: Duration,
    future_factory: impl FnOnce(PgPool) -> F + Send + 'static,
) -> Result<T, PgWorkError>
where
    F: Future<Output = Result<T, String>> + Send + 'static,
    T: Send + 'static,
{
    let deadline = match super::bridge_deadline::work_deadline(reserve) {
        Some(deadline) if Instant::now() < deadline => deadline,
        Some(_) => return Err(PgWorkError::Deadline),
        None => {
            return block_on_pg_result(pool, future_factory, PgWorkError::Other);
        }
    };
    block_on_pg_until(pool, deadline, future_factory, |error| error)
}

fn block_on_pg_until<F, T, E, M>(
    pool: &PgPool,
    deadline: Instant,
    future_factory: impl FnOnce(PgPool) -> F + Send + 'static,
    map_runtime_error: M,
) -> Result<T, PgWorkError<E>>
where
    F: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
    M: Fn(String) -> E + Copy + Send + 'static,
{
    if Instant::now() >= deadline {
        return Err(PgWorkError::Deadline);
    }
    let pool = pool.clone();
    let (reply, result) = mpsc::channel();
    // Bound the caller even when runtime setup or connection cleanup overruns.
    let wait = move || match result.recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
        Ok(outcome) if Instant::now() < deadline => outcome,
        Ok(_) => Err(PgWorkError::Deadline),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(PgWorkError::Deadline),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(PgWorkError::Other(map_runtime_error(
            "postgres bridge worker stopped".to_string(),
        ))),
    };
    if let Ok(handle) = tokio::runtime::Handle::try_current()
        && handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
        && tokio::task::try_id().is_some()
    {
        let _task = handle.spawn(async move {
            let outcome = run_pg_work_before(deadline, async move {
                future_factory(pool).await.map_err(PgWorkError::Other)
            })
            .await;
            let _ = reply.send(outcome);
        });
        return tokio::task::block_in_place(wait);
    }

    let timing = crate::logging::intake_timing::carry();
    std::thread::spawn(move || {
        let _timing = timing.entered();
        let bridge_error = |error| PgWorkError::Other(map_runtime_error(error));
        let outcome = build_runtime(bridge_error).and_then(|runtime| {
            runtime.block_on(run_pg_work_before(deadline, async move {
                let bridge_pool = build_bridge_pg_pool(&pool, bridge_error).await?;
                let result = future_factory(bridge_pool.clone())
                    .await
                    .map_err(PgWorkError::Other);
                bridge_pool.close().await;
                result
            }))
        });
        let _ = reply.send(outcome);
    });
    wait()
}

async fn run_pg_work_before<T, E>(
    deadline: Instant,
    future: impl Future<Output = Result<T, PgWorkError<E>>>,
) -> Result<T, PgWorkError<E>> {
    if Instant::now() >= deadline {
        return Err(PgWorkError::Deadline);
    }
    match tokio::time::timeout_at(deadline.into(), future).await {
        Ok(outcome) if Instant::now() < deadline => outcome,
        _ => Err(PgWorkError::Deadline),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::bridge_deadline::ScopedBridgeDeadline;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use std::time::Instant;

    #[test]
    fn block_on_result_times_out_at_bridge_deadline() {
        let _deadline = ScopedBridgeDeadline::new(Duration::from_millis(20));
        let start = Instant::now();

        let result: Result<(), String> = block_on_result(
            async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok(())
            },
            |error| error,
        );

        assert!(matches!(
            result,
            Err(ref error) if error == "bridge deadline exceeded during async bridge operation"
        ));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "async bridge deadline should fail promptly"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn block_on_pg_result_fails_fast_when_bridge_deadline_already_passed() {
        let options = PgConnectOptions::new()
            .host("127.0.0.1")
            .port(1)
            .username("postgres")
            .database("postgres");
        let pool = PgPoolOptions::new().connect_lazy_with(options);
        let _deadline = ScopedBridgeDeadline::new(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(10));

        let result: Result<(), String> =
            block_on_pg_result(&pool, |_pool| async { Ok(()) }, |error| error);

        assert!(matches!(
            result,
            Err(ref error) if error == "bridge deadline passed before async bridge started"
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn block_on_pg_result_before_maps_unarmed_success_and_error_pg() {
        use crate::db::auto_queue::test_support::TestPostgresDb;
        use crate::test_support::wait::{WAIT_CAP, await_within};

        let database = TestPostgresDb::create().await;
        let pool = database.connect_and_migrate_with_max_connections(2).await;
        let reserve = Duration::from_millis(200);
        assert!(super::super::bridge_deadline::current().is_none());
        let query = |sql| {
            block_on_pg_result_before(&pool, reserve, move |pool| async move {
                sqlx::query_scalar::<_, i32>(sql)
                    .fetch_one(&pool)
                    .await
                    .map_err(|error| error.to_string())
            })
        };
        assert!(matches!(query("SELECT 42"), Ok(42)));
        assert!(matches!(
            query("SELECT 1 / 0"),
            Err(PgWorkError::Other(error)) if error.contains("division by zero")
        ));
        await_within(pool.close(), WAIT_CAP).await;
        await_within(database.drop(), WAIT_CAP).await;
    }

    #[test]
    fn bridge_deadline_does_not_mask_successful_operation_during_cleanup() {
        let runtime = build_runtime(|error| error).unwrap();
        let start = Instant::now();

        let result: Result<&'static str, String> =
            runtime.block_on(apply_bridge_deadline_then_cleanup(
                async {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    Ok(((), "committed"))
                },
                Some(Duration::from_millis(20)),
                |error| error,
                |_| async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                },
            ));

        assert_eq!(result, Ok("committed"));
        assert!(
            start.elapsed() >= Duration::from_millis(50),
            "cleanup still runs after the operation result is fixed"
        );
    }
}
