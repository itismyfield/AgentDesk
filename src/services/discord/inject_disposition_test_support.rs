//! Test seams for the injected-input table and ring.

use super::*;

std::thread_local! {
    static READS: std::cell::RefCell<HashMap<PathBuf, usize>> = Default::default();
    static HITS: std::cell::RefCell<HashMap<ChannelId, (usize, usize)>> = Default::default();
}
/// A parked writer's path, the sender that reports it reached, and the receiver that frees it.
type Pause = (
    PathBuf,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);
static PAUSES: Mutex<Vec<Pause>> = Mutex::new(Vec::new());
static LOCK_WATCH: Mutex<Vec<(PathBuf, std::sync::mpsc::Sender<()>)>> = Mutex::new(Vec::new());
/// `(call, message)` for every gate-only table call, so a test reads its own message's calls.
static CALLS: Mutex<Vec<(&'static str, u64)>> = Mutex::new(Vec::new());

pub(super) fn note_read(path: &Path) {
    READS.with(|reads| *reads.borrow_mut().entry(path.to_path_buf()).or_default() += 1);
}

pub(super) fn note_hits(channel: ChannelId, observed: usize, unconfirmed: usize) {
    HITS.with(|hits| {
        let entry = &mut *hits.borrow_mut();
        let slot = entry.entry(channel).or_default();
        slot.0 += observed;
        slot.1 += unconfirmed;
    });
}

/// Ring reads on this thread for `provider`'s file under the current runtime root.
pub(crate) fn reads(provider: &ProviderKind) -> usize {
    let path = ring_path(provider).expect("runtime root");
    READS.with(|reads| reads.borrow().get(&path).copied().unwrap_or(0))
}

/// `(observed, unconfirmed)` hits logged on this thread for `channel`.
pub(crate) fn hits(channel: ChannelId) -> (usize, usize) {
    HITS.with(|hits| hits.borrow().get(&channel).copied().unwrap_or((0, 0)))
}

pub(crate) fn ring_file(provider: &ProviderKind) -> PathBuf {
    ring_path(provider).expect("runtime root")
}

/// Whether any terminal was ever noted in this process.
pub(crate) fn table_built() -> bool {
    TABLE.get().is_some()
}

/// Ages `message`'s memory terminal of `provider` by `by`, as if that much time had passed.
pub(crate) fn age(provider: &ProviderKind, message: u64, by: Duration) {
    let Some(table) = TABLE.get() else {
        return;
    };
    let mut tables = lock_table(table);
    if let Some(table) = tables.get_mut(provider.as_str()) {
        let back = |at: Instant| at.checked_sub(by).expect("aged instant");
        if let Some(entry) = table.terminal.get_mut(&message) {
            entry.2 = back(entry.2);
        }
        for slot in table.order.iter_mut().filter(|slot| slot.0 == message) {
            slot.1 = back(slot.1);
        }
    }
}

/// Parks the next write to `path` after its read, under the flock, until the returned sender
/// fires; the receiver says the writer reached that point.
pub(crate) fn park_before_write(
    path: &Path,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel();
    PAUSES
        .lock()
        .unwrap()
        .push((path.to_path_buf(), reached_tx, go_rx));
    (reached_rx, go_tx)
}

pub(super) fn pause_before_write(path: &Path) {
    let parked = {
        let mut pauses = PAUSES.lock().unwrap();
        let at = pauses.iter().position(|(parked, _, _)| parked == path);
        at.map(|at| pauses.remove(at))
    };
    if let Some((_, reached, go)) = parked {
        let _ = reached.send(());
        let _ = go.recv();
    }
}

/// Signals the next writer to `path` as it is about to take the flock.
pub(crate) fn watch_lock(path: &Path) -> std::sync::mpsc::Receiver<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    LOCK_WATCH.lock().unwrap().push((path.to_path_buf(), tx));
    rx
}

pub(super) fn before_lock(path: &Path) {
    let watched = {
        let mut watches = LOCK_WATCH.lock().unwrap();
        let at = watches.iter().position(|(watched, _)| watched == path);
        at.map(|at| watches.remove(at))
    };
    if let Some((_, reached)) = watched {
        let _ = reached.send(());
    }
}

pub(super) fn note_call(call: &'static str, message: u64) {
    CALLS.lock().unwrap().push((call, message));
}

/// The gate-only table calls made for `message`, in order.
pub(crate) fn calls(message: u64) -> Vec<&'static str> {
    let calls = CALLS.lock().unwrap();
    let mine = calls.iter().filter(|(_, id)| *id == message);
    mine.map(|(call, _)| *call).collect()
}

/// `message`'s raw claim entry, unpruned: `in_progress`, `dead_lease` or `thread_intake`.
pub(crate) fn source_entry(provider: &ProviderKind, message: u64) -> Option<&'static str> {
    let tables = lock_table(TABLE.get()?);
    let source = tables.get(provider.as_str())?.sources.get(&message)?;
    Some(match source {
        Source::InProgress { lease } if lease.strong_count() > 0 => "in_progress",
        Source::InProgress { .. } => "dead_lease",
        Source::ThreadIntake { .. } => "thread_intake",
    })
}

/// Order slots held for `message`.
pub(crate) fn order_slots(provider: &ProviderKind, message: u64) -> usize {
    let Some(table) = TABLE.get() else {
        return 0;
    };
    let tables = lock_table(table);
    let Some(table) = tables.get(provider.as_str()) else {
        return 0;
    };
    table.order.iter().filter(|slot| slot.0 == message).count()
}

/// A claim whose owner vanished without its guard's cleanup, as a leaked guard would leave it.
pub(crate) fn plant_dead_lease(provider: &ProviderKind, message: u64) {
    let lease = Arc::downgrade(&Arc::new(SourceLease));
    let mut tables = lock_table(TABLE.get_or_init(Default::default));
    let table = tables.entry(provider.as_str().to_string()).or_default();
    table.sources.insert(message, Source::InProgress { lease });
}
