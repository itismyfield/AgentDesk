use std::sync::{Arc, OnceLock};
use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BootWorkFailure {
    Invalid(&'static str),
    Worker(String),
    Closed,
}
pub struct Completed<T> {
    scope: (u64, String),
    value: T,
}
impl<T> Completed<T> {
    pub fn value(&self) -> &T {
        &self.value
    }
    pub(super) fn matches(&self, epoch: u64, provider: &str) -> bool {
        self.scope.0 == epoch && self.scope.1 == provider
    }
}
pub type BootResult<T> = Result<T, BootWorkFailure>;
type Outcome<T> = BootResult<Arc<Completed<T>>>;
type Sender<T> = watch::Sender<Option<Outcome<T>>>;
pub struct BootWorkOnce<T> {
    scope: (u64, String),
    sender: OnceLock<Sender<T>>,
}
impl<T: Send + Sync + 'static> BootWorkOnce<T> {
    pub(super) fn new(epoch: u64, provider: &str) -> Self {
        Self {
            scope: (epoch, provider.into()),
            sender: OnceLock::new(),
        }
    }
    pub async fn run_once(&self, work: impl FnOnce() -> T + Send + 'static) -> Outcome<T> {
        receive(self.start(work)).await
    }
    pub(super) fn start(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> watch::Receiver<Option<Outcome<T>>> {
        self.sender
            .get_or_init(|| {
                let sender = watch::channel(None).0;
                let observer = sender.clone();
                let scope = self.scope.clone();
                // The observer retains the join when a requesting future disappears.
                let worker = tokio::task::spawn_blocking(work);
                tokio::spawn(async move {
                    observer.send_replace(Some(join_work(scope, worker).await));
                });
                sender
            })
            .subscribe()
    }
}
pub(super) async fn join_work<T: Send + Sync + 'static>(
    scope: (u64, String),
    worker: tokio::task::JoinHandle<T>,
) -> Outcome<T> {
    worker
        .await
        .map(|value| Arc::new(Completed { scope, value }))
        .map_err(|error| BootWorkFailure::Worker(error.to_string()))
}
pub(super) async fn receive<T>(mut rx: watch::Receiver<Option<Outcome<T>>>) -> Outcome<T> {
    loop {
        if let Some(result) = rx.borrow_and_update().as_ref() {
            return result.clone();
        }
        rx.changed().await.map_err(|_| BootWorkFailure::Closed)?;
    }
}
