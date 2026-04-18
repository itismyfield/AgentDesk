use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::{any::Any, future::Future};

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
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        return match handle.runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| handle.block_on(future))
            }
            tokio::runtime::RuntimeFlavor::CurrentThread => {
                match std::thread::spawn(move || build_runtime(map_runtime_error)?.block_on(future))
                    .join()
                {
                    Ok(result) => result,
                    Err(payload) => Err(map_runtime_error(format!(
                        "tokio bridge thread panicked: {}",
                        panic_payload_to_string(payload.as_ref())
                    ))),
                }
            }
            _ => build_runtime(map_runtime_error)?.block_on(future),
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
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        return match handle.runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => {
                let pool = pool.clone();
                tokio::task::block_in_place(|| handle.block_on(future_factory(pool)))
            }
            _ => run_pg_bridge_thread(pool, future_factory, map_runtime_error),
        };
    }

    run_pg_bridge_thread(pool, future_factory, map_runtime_error)
}

fn run_pg_bridge_thread<F, T, E, M, B>(
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
    let source_pool = pool.clone();
    match std::thread::spawn(move || {
        let runtime = build_runtime(map_runtime_error)?;
        runtime.block_on(async move {
            let bridge_pool = build_bridge_pg_pool(&source_pool, map_runtime_error).await?;
            let result = future_factory(bridge_pool.clone()).await;
            bridge_pool.close().await;
            result
        })
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
