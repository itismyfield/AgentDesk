//! The per-Context reader behind policy `config.get` and `kv.get`: one thread, one current-thread
//! runtime and at most one PostgreSQL connection, started on the first read, kept until drop.

use std::any::Any;
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use sqlx::postgres::{PgConnectOptions, PgConnection};
use sqlx::{Connection, PgPool};

use super::config_ops::ConfigRead;
use super::kv_ops::KvRead;

const DEADLINE_PASSED: &str = "bridge deadline passed before async bridge started";
const DEADLINE_EXCEEDED: &str = "bridge deadline exceeded during async bridge operation";

/// The only two statements the bridge runs; each is a fixed SELECT owned by its op module.
pub(super) enum ReadCommand {
    Config(ConfigRead),
    Kv(KvRead),
}

impl ReadCommand {
    fn select<'c>(
        &'c self,
        connection: &'c mut PgConnection,
    ) -> BoxFuture<'c, Result<String, String>> {
        match self {
            ReadCommand::Config(read) => read.select(connection),
            ReadCommand::Kv(read) => read.select(connection),
        }
    }
}

#[derive(Debug)]
pub(super) enum ReadError {
    /// The bridge itself failed: deadline, connection or worker.
    Bridge(String),
    /// The SELECT ran and PostgreSQL returned an error.
    Query(String),
}

struct Request {
    command: ReadCommand,
    deadline: Option<Instant>,
    reply: mpsc::SyncSender<Result<String, ReadError>>,
}

struct Worker {
    requests: tokio::sync::mpsc::UnboundedSender<Request>,
    thread: JoinHandle<()>,
}

/// Where the dedicated connection comes from: the source pool's options and acquire bound.
#[derive(Clone)]
struct Source {
    connect_options: PgConnectOptions,
    acquire_timeout: Duration,
}

/// One owner per JS Context; the worker holds only its request receiver, never the owner.
pub(super) struct ReadBridge {
    source: Source,
    worker: Mutex<Option<Worker>>,
}

impl ReadBridge {
    pub(super) fn new(source_pool: &PgPool) -> Self {
        Self {
            source: Source {
                connect_options: (*source_pool.connect_options()).clone(),
                acquire_timeout: source_pool.options().get_acquire_timeout(),
            },
            worker: Mutex::new(None),
        }
    }

    /// Runs one read and waits for its own reply; the caller's absolute deadline travels with it.
    pub(super) fn read(&self, command: ReadCommand) -> Result<String, ReadError> {
        let deadline = crate::engine::loader::bridge_op_deadline();
        if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
            return Err(ReadError::Bridge(DEADLINE_PASSED.to_string()));
        }
        let mut slot = self.worker.lock().unwrap_or_else(PoisonError::into_inner);
        let worker = match slot.take() {
            Some(worker) => worker,
            None => self.spawn()?,
        };
        let (reply, answer) = mpsc::sync_channel(1);
        let request = Request {
            command,
            deadline,
            reply,
        };
        if worker.requests.send(request).is_err() {
            return Err(ReadError::Bridge(stopped(worker)));
        }
        match wait(&answer) {
            Ok(result) => {
                *slot = Some(worker);
                result
            }
            Err(mpsc::RecvError) => Err(ReadError::Bridge(stopped(worker))),
        }
    }

    fn spawn(&self) -> Result<Worker, ReadError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| ReadError::Bridge(format!("tokio runtime: {error}")))?;
        let (requests, inbox) = tokio::sync::mpsc::unbounded_channel();
        let source = self.source.clone();
        let thread = std::thread::Builder::new()
            .name("policy-read-bridge".to_string())
            .spawn(move || runtime.block_on(serve(source, inbox)))
            .map_err(|error| ReadError::Bridge(format!("spawn read bridge thread: {error}")))?;
        Ok(Worker { requests, thread })
    }
}

impl Drop for ReadBridge {
    /// Closing the request channel lets the worker drop its connection and return before the join.
    fn drop(&mut self) {
        let slot = self
            .worker
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(Worker { requests, thread }) = slot.take() {
            drop(requests);
            let _ = thread.join();
        }
    }
}

/// Joins a worker that dropped a reply without answering, so a new one starts only after it ended.
fn stopped(worker: Worker) -> String {
    let Worker { requests, thread } = worker;
    drop(requests);
    match thread.join() {
        Err(payload) => format!(
            "tokio bridge thread panicked: {}",
            panic_message(payload.as_ref())
        ),
        Ok(()) => "tokio bridge thread stopped without a reply".to_string(),
    }
}

fn panic_message(payload: &(dyn Any + Send + 'static)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "unknown panic payload".to_string()
}

/// A multi-thread runtime worker blocks in place, as the shared-pool bridge path does.
fn wait<T>(answer: &mpsc::Receiver<T>) -> Result<T, mpsc::RecvError> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle)
            if tokio::task::try_id().is_some()
                && handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread =>
        {
            tokio::task::block_in_place(|| answer.recv())
        }
        _ => answer.recv(),
    }
}

async fn serve(source: Source, mut inbox: tokio::sync::mpsc::UnboundedReceiver<Request>) {
    let mut connection = None;
    while let Some(request) = inbox.recv().await {
        let result = run(&source, &mut connection, &request.command, request.deadline).await;
        let _ = request.reply.send(result);
    }
    drop(connection);
}

/// Acquire, check or reconnect within the earlier of the deadline and the acquire bound, then
/// one SELECT within the deadline; a failed or late SELECT drops the connection and is not rerun.
async fn run(
    source: &Source,
    slot: &mut Option<PgConnection>,
    command: &ReadCommand,
    deadline: Option<Instant>,
) -> Result<String, ReadError> {
    let acquire_bound = Instant::now() + source.acquire_timeout;
    let acquire_by = deadline.map_or(acquire_bound, |deadline| deadline.min(acquire_bound));
    let mut connection =
        match within(Some(acquire_by), ready(source, slot.take(), acquire_by)).await {
            Some(Ok(connection)) => connection,
            Some(Err(error)) => {
                return Err(ReadError::Bridge(format!(
                    "connect postgres bridge pool: {error}"
                )));
            }
            None if deadline == Some(acquire_by) => {
                return Err(ReadError::Bridge(DEADLINE_EXCEEDED.to_string()));
            }
            None => {
                return Err(ReadError::Bridge(format!(
                    "connect postgres bridge pool: {}",
                    sqlx::Error::PoolTimedOut
                )));
            }
        };
    match within(deadline, command.select(&mut connection)).await {
        Some(Ok(value)) => {
            *slot = Some(connection);
            Ok(value)
        }
        Some(Err(error)) => Err(ReadError::Query(error)),
        None => Err(ReadError::Bridge(DEADLINE_EXCEEDED.to_string())),
    }
}

async fn within<F: std::future::Future>(deadline: Option<Instant>, future: F) -> Option<F::Output> {
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline.into(), future).await.ok(),
        None => Some(future.await),
    }
}

/// A kept connection that answers a ping is reused; otherwise a new one is made, retrying a
/// refused or still-starting server as the source pool's own connect does.
async fn ready(
    source: &Source,
    kept: Option<PgConnection>,
    acquire_by: Instant,
) -> Result<PgConnection, sqlx::Error> {
    if let Some(mut connection) = kept
        && connection.ping().await.is_ok()
    {
        return Ok(connection);
    }
    let mut backoff = Duration::from_millis(10);
    let max_backoff = acquire_by.saturating_duration_since(Instant::now()) / 5;
    loop {
        match PgConnection::connect_with(&source.connect_options).await {
            Err(sqlx::Error::Io(error))
                if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
            Err(sqlx::Error::Database(error)) if error.is_transient_in_connect_phase() => {}
            outcome => return outcome,
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}
