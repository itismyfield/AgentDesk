//! Existing-effect propagation across tasks and synchronous worker closures.
use super::{Failure, Permit, lookup};
use crate::services::provider::ProviderKind;
use std::cell::RefCell;
use std::future::Future;
use std::sync::Arc;

tokio::task_local! { static TASK: Option<Permit>; }
thread_local! { static WORKER: RefCell<Option<Permit>> = const { RefCell::new(None) }; }

pub(crate) fn current() -> Option<Permit> {
    WORKER
        .with(|held| held.borrow().clone())
        .or_else(|| TASK.try_with(Clone::clone).ok().flatten())
}

pub(crate) fn admit(provider: &ProviderKind, channel: u64) -> Result<Option<Permit>, Failure> {
    if let Some(permit) = current() {
        permit.validate(provider, channel)?;
        return Ok(Some(permit));
    }
    lookup(provider, channel)
        .map(|gate| gate.admit())
        .transpose()
}

pub(crate) async fn scope<F: Future>(permit: Option<Permit>, work: F) -> F::Output {
    match permit {
        Some(permit) => TASK.scope(Some(permit), work).await,
        None => work.await,
    }
}

pub(crate) fn detached<F>(permit: Option<Permit>, work: F) -> impl Future<Output = ()>
where
    F: Future<Output = ()> + Send + 'static,
{
    match permit {
        None => futures::future::Either::Left(work),
        Some(permit) => futures::future::Either::Right(poll_on_worker(WorkerFuture {
            future: Some(Box::pin(work)),
            permit: Some(permit),
            runtime: tokio::runtime::Handle::current(),
        })),
    }
}

async fn poll_on_worker<F: Future<Output = ()> + Send + 'static>(mut owned: WorkerFuture<F>) {
    let runtime = owned.runtime.clone();
    let wake = Arc::new(PollWake(tokio::sync::Notify::new()));
    loop {
        let notified = wake.0.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let poll_wake = wake.clone();
        let worker = runtime.spawn_blocking(move || {
            let waker = futures::task::waker(poll_wake);
            let mut cx = std::task::Context::from_waker(&waker);
            let ready = synchronous(owned.permit.clone(), || {
                super::blocking(|| {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        owned.future.as_mut().unwrap().as_mut().poll(&mut cx)
                    }));
                    match result {
                        Ok(poll) => {
                            if poll.is_ready() {
                                drop(owned.future.take());
                            }
                            poll.is_ready()
                        }
                        Err(payload) => {
                            drop(owned.future.take());
                            std::panic::resume_unwind(payload)
                        }
                    }
                })
            });
            (owned, ready)
        });
        let (next, ready) = match worker.await {
            Ok(result) => result,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => panic!("input effect worker cancelled: {error}"),
        };
        owned = next;
        if ready {
            return;
        }
        notified.await;
    }
}

struct PollWake(tokio::sync::Notify);
impl futures::task::ArcWake for PollWake {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.0.notify_one();
    }
}

// The pinned allocation never moves; cancellation disposes it on a worker too.
struct WorkerFuture<F: Future + Send + 'static> {
    future: Option<std::pin::Pin<Box<F>>>,
    permit: Option<Permit>,
    runtime: tokio::runtime::Handle,
}
impl<F: Future + Send + 'static> Drop for WorkerFuture<F> {
    fn drop(&mut self) {
        if let Some(future) = self.future.take() {
            let cleanup = WorkerCleanup(Some((future, self.permit.take())));
            self.runtime.spawn_blocking(move || cleanup.run());
        }
    }
}

// Tokio can discard a queued blocking closure without executing it at shutdown.
struct WorkerCleanup<F: Future + Send + 'static>(Option<(std::pin::Pin<Box<F>>, Option<Permit>)>);
impl<F: Future + Send + 'static> WorkerCleanup<F> {
    fn run(mut self) {
        if let Some((future, permit)) = self.0.take() {
            synchronous(permit, || super::blocking(|| drop(future)));
        }
    }
}
impl<F: Future + Send + 'static> Drop for WorkerCleanup<F> {
    fn drop(&mut self) {
        let Some(work) = self.0.take() else {
            return;
        };
        let work = Arc::new(std::sync::Mutex::new(Some(work)));
        let worker_work = work.clone();
        let spawned = std::thread::Builder::new()
            .name("input-effect-cleanup".into())
            .spawn(move || {
                let work = worker_work
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some((future, permit)) = work {
                    synchronous(permit, || super::blocking(|| drop(future)));
                }
            });
        if let Err(error) = spawned {
            // Keep the effect closed if no cleanup worker can be created.
            tracing::error!(%error, "input effect cleanup worker unavailable; effect retained");
            std::mem::forget(work);
        }
    }
}

pub(crate) struct WorkerScope(Option<Permit>);
impl Drop for WorkerScope {
    fn drop(&mut self) {
        WORKER.with(|held| *held.borrow_mut() = self.0.take());
    }
}
pub(crate) fn worker_scope(permit: Option<Permit>) -> WorkerScope {
    WorkerScope(WORKER.with(|held| held.replace(permit)))
}
pub(crate) fn synchronous<T>(permit: Option<Permit>, work: impl FnOnce() -> T) -> T {
    let _reset = worker_scope(permit);
    work()
}

#[cfg(test)]
#[path = "effect_tests.rs"]
mod tests;

pub(crate) async fn io<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let permit = current();
    tokio::task::spawn_blocking(move || synchronous(permit, || super::blocking(work)))
        .await
        .expect("input effect worker panicked")
}
