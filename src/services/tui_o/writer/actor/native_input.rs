//! The host supplies one source-bound input port to the existing writer poll.
use super::*;

#[derive(Clone)]
pub struct NativePort {
    #[cfg(unix)]
    pub inner: Arc<dyn EvidencePort>,
}

#[cfg(unix)]
pub struct Snapshot {
    pub generation: u64,
    pub registrations: Vec<super::super::input_facts::native::Registration>,
    pub registry_revisions: Vec<u64>,
    pub has_open_barrier: bool,
}

#[cfg(unix)]
pub type Terminal = Result<super::super::input_facts::native::terminal::RawWitness, String>;

#[cfg(unix)]
pub trait EvidencePort: Send + Sync {
    fn snapshot(
        &self,
        origin: super::super::input_notices::NoticeOrigin,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Snapshot, String>> + Send + '_>>;
    /// Only a quiescent source may reuse its snapshot; effect rechecks still use `snapshot`.
    fn snapshot_since<'a>(
        &'a self,
        origin: super::super::input_notices::NoticeOrigin,
        _known: Option<&'a [u64]>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<Snapshot>, String>> + Send + 'a>,
    > {
        Box::pin(async move { self.snapshot(origin).await.map(Some) })
    }
    fn offer(
        &self,
        observed: super::super::input_facts::native::Observation,
        current: super::super::input_facts::native::Observation,
        index: usize,
        origin: super::super::input_notices::NoticeOrigin,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send + '_>>;
    fn abstain(
        &self,
        _observed: super::super::input_facts::native::Observation,
        _current: super::super::input_facts::native::Observation,
        _proofs: (
            Vec<super::super::input_facts::native::Barrier>,
            Vec<super::super::input_facts::native::Barrier>,
        ),
        _idle: bool,
        _origin: super::super::input_notices::NoticeOrigin,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async { Err("native abstention unavailable".into()) })
    }
    fn receipt(
        &self,
        _observed: super::super::input_facts::native::Observation,
        _current: super::super::input_facts::native::Observation,
        _index: usize,
        _origin: super::super::input_notices::NoticeOrigin,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async { Err("native receipt unavailable".into()) })
    }
    /// Proves the unterminated `tail` is the ended execution's whole strict closer for `ticket`.
    fn terminal(
        &self,
        _observed: super::super::input_facts::native::Observation,
        _ticket: super::super::input_facts::native::Registration,
        _tail: super::super::input_facts::native::terminal::Tail,
        _origin: super::super::input_notices::NoticeOrigin,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Terminal> + Send + '_>> {
        Box::pin(async { Err("native terminal proof unavailable".into()) })
    }
    fn retry(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;
}

/// Publishes [`Unsettled`] and [`Owing`]; `None` keeps legacy readers without cleanup policy.
pub fn spawn_projecting<P, L, A, B>(
    config: &WriterConfig,
    readers: Option<ReaderConfig>,
    writer: ChannelWriter<P, L, A>,
    provider: ShadowProvider,
    bindings: Arc<B>,
    (stop, resumed, unsettled, owing): (
        watch::Receiver<bool>,
        watch::Sender<bool>,
        Unsettled,
        Owing,
    ),
) -> Option<JoinHandle<Option<StopCause>>>
where
    P: DiscordPort,
    L: DeliveryLease + 'static,
    A: AlarmSink + 'static,
    B: BindingEvents,
{
    spawn_native(
        config,
        readers,
        writer,
        provider,
        bindings,
        (stop, resumed, unsettled, owing),
        None,
    )
}

pub fn spawn_native<P, L, A, B>(
    config: &WriterConfig,
    readers: Option<ReaderConfig>,
    writer: ChannelWriter<P, L, A>,
    provider: ShadowProvider,
    bindings: Arc<B>,
    (stop, resumed, unsettled, owing): (
        watch::Receiver<bool>,
        watch::Sender<bool>,
        Unsettled,
        Owing,
    ),
    native: Option<NativePort>,
) -> Option<JoinHandle<Option<StopCause>>>
where
    P: DiscordPort,
    L: DeliveryLease + 'static,
    A: AlarmSink + 'static,
    B: BindingEvents,
{
    let run = || {
        let commands = codex_submission::take_spawn_receiver();
        let transitions = transition_port::take_spawn_receiver();
        let projections = (unsettled, owing);
        tokio::spawn(run_publishing(
            writer,
            provider,
            bindings,
            (stop, resumed),
            projections,
            (None, readers, config.producer_absent, native),
            (commands, transitions),
        ))
    };
    config.enabled.then(run)
}

use super::super::host::HostIo;
/// A host's re-post turn for one channel writer.
pub type RepostTurn<I> = Box<
    dyn crate::services::tui_o::repost::runner::RepostHook<
            ChannelWriter<<I as HostIo>::Port, <I as HostIo>::Lease, <I as HostIo>::Alarms>,
        >,
>;

pub(in crate::services::tui_o::writer) fn hold(
    alarms: &impl AlarmSink,
    channel: u64,
    detail: &str,
) {
    tracing::error!(channel, detail, "[tui_o] writer host held the channel");
    let detail = format!("writer host: {detail}");
    alarms.raise(channel, WriterAlarm::Halted { detail });
}

pub(in crate::services::tui_o::writer) async fn stopped(mut stop: watch::Receiver<bool>) {
    if stop.wait_for(|stop| *stop).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// `step`'s output, or `None` once the writer was told to stop first.
pub(in crate::services::tui_o::writer) async fn unless_stopped<T>(
    stop: &watch::Receiver<bool>,
    step: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        () = stopped(stop.clone()) => None,
        output = step => Some(output),
    }
}
