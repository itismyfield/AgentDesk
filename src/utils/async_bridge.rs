use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::{any::Any, future::Future, pin::Pin, time::Duration};

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
    match crate::engine::loader::bridge_op_deadline_remaining() {
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

fn block_on_runtime_thread<F, T, E, M>(future: F, map_runtime_error: M) -> Result<T, E>
where
    F: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
    M: Fn(String) -> E + Copy + Send + 'static,
{
    match std::thread::spawn(move || build_runtime(map_runtime_error)?.block_on(future)).join() {
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
    let pool_options = source_pool.options();
    PgPoolOptions::new()
        .max_connections(pool_options.get_max_connections().max(1))
        .min_connections(pool_options.get_min_connections())
        .acquire_timeout(pool_options.get_acquire_timeout())
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
    match std::thread::spawn(move || {
        let runtime = build_runtime(map_runtime_error)?;
        let operation = async move {
            let bridge_pool = build_bridge_pg_pool(&source_pool, map_runtime_error).await?;
            let result = future_factory(bridge_pool.clone()).await;
            bridge_pool.close().await;
            result
        };
        runtime.block_on(apply_bridge_deadline(
            operation,
            deadline,
            map_runtime_error,
        ))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::loader::ScopedBridgeDeadline;
    use sqlx::postgres::PgConnectOptions;
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

    #[test]
    fn block_on_pg_result_fails_fast_when_bridge_deadline_already_passed() {
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
}
