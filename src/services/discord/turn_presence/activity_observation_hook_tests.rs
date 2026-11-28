use super::lock;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

static READY_PANES: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

/// Live probes read this tmux session's pane as passing the final send check while it lives.
pub(crate) struct ReadyPane(String);

impl ReadyPane {
    pub(crate) fn mark(session: &str) -> Self {
        lock(&READY_PANES).insert(session.into());
        Self(session.into())
    }
}

impl Drop for ReadyPane {
    fn drop(&mut self) {
        lock(&READY_PANES).remove(&self.0);
    }
}

pub(in crate::services::discord) fn pane_ready_for_tests(session: &str) -> bool {
    lock(&READY_PANES).contains(session)
}

type Reads = Vec<(u64, u64, bool)>;

type ReadStages = Arc<Mutex<Vec<(&'static str, Reads)>>>;

static READ_TRACES: LazyLock<Mutex<HashMap<u64, ReadStages>>> = LazyLock::new(Default::default);

pub(in crate::services::discord) struct ReadTrace(u64, ReadStages);

impl ReadTrace {
    pub(in crate::services::discord) fn new(channel: u64) -> Self {
        let stages: ReadStages = Arc::default();
        assert!(lock(&READ_TRACES).insert(channel, stages.clone()).is_none());
        Self(channel, stages)
    }
    pub(in crate::services::discord) fn take(&self) -> Vec<(&'static str, Reads)> {
        std::mem::take(&mut *lock(&self.1))
    }
}

impl Drop for ReadTrace {
    fn drop(&mut self) {
        lock(&READ_TRACES).remove(&self.0);
    }
}

pub(in crate::services::discord) fn record_io(channel: u64, stage: &'static str, reads: Reads) {
    if let Some(stages) = lock(&READ_TRACES).get(&channel) {
        #[cfg(target_os = "linux")]
        let tid = unsafe { libc::syscall(libc::SYS_gettid) };
        #[cfg(not(target_os = "linux"))]
        let tid = format!("{:?}", std::thread::current().id());
        eprintln!("S1_IO_WORKER channel={channel} stage={stage} tid={tid}");
        lock(stages).push((stage, reads));
    }
}
