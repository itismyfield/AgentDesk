use crate::domain::file_sync::{PARENT_DIR_FSYNC_FLUSHES, fsync_parent_dir};
use crate::services::durable_publish::atomic_write;
use crate::services::runtime_paths::agentdesk_root;
use std::fs;
use std::path::{Path, PathBuf};

/// Path to the generation counter file.
pub fn generation_path() -> Option<PathBuf> {
    agentdesk_root().map(|root| root.join("runtime").join("generation"))
}

// <epoch-provenance-surface>

/// How this process's generation was selected; variants describe observed calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GenerationAllocationRoute {
    /// `read_generation_counter` returned `Parsed` or `Absent`, `atomic_write`
    /// returned `Ok`, `next != current`, and `fsync_parent_dir` returned `Ok`
    /// after flushing the parent directory.
    AdvancedWithSyncedRename,
    /// As `AdvancedWithSyncedRename`, but `fsync_parent_dir` returned `Ok`
    /// without flushing (`PARENT_DIR_FSYNC_FLUSHES` is false).
    AdvancedWithUnflushedRename,
    /// `atomic_write` returned `Ok`, but `fsync_parent_dir` returned `Err`.
    ParentSyncFailed,
    /// `atomic_write` and `fsync_parent_dir` returned `Ok`, but
    /// `read_generation_counter` returned `Failed`.
    CounterReadFailed,
    /// `atomic_write` returned `Ok`, but `next == current`; parent sync skipped.
    Saturated,
    /// `atomic_write` returned `Err`.
    WriteFailed,
    /// `lock_generation_path` returned `Err`; an unsynchronized
    /// `read_generation_counter` followed and no write was attempted.
    LockFailed,
    /// No generation path was available; no filesystem call was attempted.
    PathUnavailable,
    /// No transaction is bound, or a test pin supplied no provenance.
    Unwitnessed,
}

/// Generation and allocation route are published as one value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProcessGenerationAllocation {
    pub(crate) generation: u64,
    route: GenerationAllocationRoute,
}

/// Counter reads retain establishment and bounded failure detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CounterReadFailure {
    Io(std::io::ErrorKind),
    Parse,
}

impl CounterReadFailure {
    fn as_str(self) -> &'static str {
        match self {
            Self::Io(_) => "io",
            Self::Parse => "parse",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CounterRead {
    Parsed(u64),
    Absent,
    Failed(CounterReadFailure),
}

impl CounterRead {
    fn value(self) -> u64 {
        match self {
            Self::Parsed(value) => value,
            Self::Absent | Self::Failed(_) => 0,
        }
    }

    fn established(self) -> bool {
        !matches!(self, Self::Failed(_))
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Parsed(_) => "parsed",
            Self::Absent => "absent",
            Self::Failed(CounterReadFailure::Io(_)) => "io_error",
            Self::Failed(CounterReadFailure::Parse) => "parse_error",
        }
    }

    fn failure(self) -> Option<CounterReadFailure> {
        match self {
            Self::Failed(failure) => Some(failure),
            Self::Parsed(_) | Self::Absent => None,
        }
    }
}

impl GenerationAllocationRoute {
    fn advanced(self) -> bool {
        matches!(self, Self::AdvancedWithSyncedRename)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::AdvancedWithSyncedRename => "advanced_with_synced_rename",
            Self::AdvancedWithUnflushedRename => "advanced_with_unflushed_rename",
            Self::ParentSyncFailed => "parent_sync_failed",
            Self::CounterReadFailed => "counter_read_failed",
            Self::Saturated => "saturated",
            Self::WriteFailed => "write_failed",
            Self::LockFailed => "lock_failed",
            Self::PathUnavailable => "path_unavailable",
            Self::Unwitnessed => "unwitnessed",
        }
    }
}

impl ProcessGenerationAllocation {
    pub(crate) fn epoch_advanced(self) -> bool {
        self.route.advanced()
    }

    pub(crate) fn epoch_route(self) -> &'static str {
        self.route.as_str()
    }
}

#[cfg(test)]
mod test_generation_publication {
    use super::*;

    std::thread_local! {
        static BINDING: std::cell::Cell<Option<ProcessGenerationAllocation>> =
            const { std::cell::Cell::new(None) };
    }

    pub(crate) struct Publication {
        previous_binding: Option<ProcessGenerationAllocation>,
    }

    impl Drop for Publication {
        fn drop(&mut self) {
            BINDING.set(self.previous_binding);
        }
    }

    pub(super) fn published_binding() -> Option<ProcessGenerationAllocation> {
        BINDING.get()
    }

    /// The route a successful advance takes on this platform.
    pub(crate) const ADVANCED: GenerationAllocationRoute = if PARENT_DIR_FSYNC_FLUSHES {
        GenerationAllocationRoute::AdvancedWithSyncedRename
    } else {
        GenerationAllocationRoute::AdvancedWithUnflushedRename
    };

    pub(crate) fn allocation(
        generation: u64,
        route: GenerationAllocationRoute,
    ) -> ProcessGenerationAllocation {
        ProcessGenerationAllocation { generation, route }
    }

    fn parent_sync_failure(_: &Path) -> std::io::Result<()> {
        Err(std::io::Error::other("injected parent sync failure"))
    }

    pub(crate) fn publish(allocated: ProcessGenerationAllocation) -> Publication {
        Publication {
            previous_binding: BINDING.replace(Some(allocated)),
        }
    }

    pub(crate) fn allocate_and_publish(parent_sync_succeeds: bool) -> Publication {
        let _allocation = lock_unpoisoned(&PROCESS_GENERATION_ALLOCATION_TRANSACTION);
        let allocated = allocate_generation_epoch_with_io(GenerationIo {
            path: generation_path(),
            lock: lock_generation_path,
            read: read_generation_counter,
            write: atomic_write,
            fsync: if parent_sync_succeeds {
                fsync_parent_dir
            } else {
                parent_sync_failure
            },
            flushes: PARENT_DIR_FSYNC_FLUSHES,
        });
        let previous_binding = BINDING.replace(Some(allocated));
        Publication { previous_binding }
    }
}

#[cfg(test)]
pub(crate) use test_generation_publication::{
    ADVANCED as ADVANCED_ROUTE_FOR_TESTS,
    allocate_and_publish as allocate_and_publish_process_generation_for_tests,
    allocation as process_generation_allocation_for_tests,
    publish as publish_process_generation_allocation_for_tests,
};

// </epoch-provenance-surface>

/// Load the current generation counter (returns 0 if missing or unreadable).
pub fn load_generation() -> u64 {
    generation_path().map_or(0, |path| read_generation_counter(&path).value())
}

/// Production publishes generation and route in one lock-free read cell.
#[cfg(not(test))]
static PROCESS_GENERATION: std::sync::OnceLock<ProcessGenerationAllocation> =
    std::sync::OnceLock::new();
#[cfg(test)]
static TEST_PROCESS_GENERATION_OVERRIDE: std::sync::Mutex<Option<u64>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_PROCESS_GENERATION_ALLOCATION: std::sync::Mutex<Option<ProcessGenerationAllocation>> =
    std::sync::Mutex::new(None);
static PROCESS_GENERATION_ALLOCATION_TRANSACTION: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
fn lock_unpoisoned<T>(cell: &'static std::sync::Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    cell.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
fn test_generation_override() -> std::sync::MutexGuard<'static, Option<u64>> {
    lock_unpoisoned(&TEST_PROCESS_GENERATION_OVERRIDE)
}

#[cfg(test)]
fn test_allocation_memo() -> std::sync::MutexGuard<'static, Option<ProcessGenerationAllocation>> {
    lock_unpoisoned(&TEST_PROCESS_GENERATION_ALLOCATION)
}

fn allocate_once_with(
    cell: &std::sync::OnceLock<ProcessGenerationAllocation>,
    transaction: &std::sync::Mutex<()>,
    allocate: impl FnOnce() -> ProcessGenerationAllocation,
) -> ProcessGenerationAllocation {
    if let Some(bound) = cell.get() {
        return *bound;
    }
    let _transaction = transaction
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    *cell.get_or_init(allocate)
}

struct GenerationFileLock {
    _file: fs::File,
}

impl Drop for GenerationFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn lock_generation_path(path: &Path) -> Result<GenerationFileLock, String> {
    let lock_path = path.with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(lock_path)
        .map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    Ok(GenerationFileLock { _file: file })
}

struct GenerationIo<L, R, W, F> {
    path: Option<PathBuf>,
    lock: L,
    read: R,
    write: W,
    fsync: F,
    /// Whether an `Ok` from `fsync` flushed the parent; injected so both
    /// advanced routes run on every host.
    flushes: bool,
}

/// Allocate once, publishing generation and route together.
pub fn allocate_process_generation() -> u64 {
    allocate_process_generation_binding().generation
}

pub(crate) fn allocate_process_generation_binding() -> ProcessGenerationAllocation {
    #[cfg(not(test))]
    {
        allocate_once_with(
            &PROCESS_GENERATION,
            &PROCESS_GENERATION_ALLOCATION_TRANSACTION,
            allocate_generation_epoch,
        )
    }
    #[cfg(test)]
    {
        if let Some(generation) = *test_generation_override() {
            return ProcessGenerationAllocation {
                generation,
                route: GenerationAllocationRoute::Unwitnessed,
            };
        }
        if let Some(bound) = *test_allocation_memo() {
            return bound;
        }
        let _allocation = lock_unpoisoned(&PROCESS_GENERATION_ALLOCATION_TRANSACTION);
        if let Some(generation) = *test_generation_override() {
            return ProcessGenerationAllocation {
                generation,
                route: GenerationAllocationRoute::Unwitnessed,
            };
        }
        if let Some(bound) = *test_allocation_memo() {
            return bound;
        }
        let allocated = allocate_generation_epoch();
        *test_allocation_memo() = Some(allocated);
        allocated
    }
}

fn allocate_generation_epoch() -> ProcessGenerationAllocation {
    allocate_generation_epoch_with_io(GenerationIo {
        path: generation_path(),
        lock: lock_generation_path,
        read: read_generation_counter,
        write: atomic_write,
        fsync: fsync_parent_dir,
        flushes: PARENT_DIR_FSYNC_FLUSHES,
    })
}

fn read_generation_counter(path: &Path) -> CounterRead {
    match fs::read_to_string(path) {
        Ok(body) => match body.trim().parse::<u64>() {
            Ok(value) => CounterRead::Parsed(value),
            Err(_) => CounterRead::Failed(CounterReadFailure::Parse),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => CounterRead::Absent,
        Err(error) => CounterRead::Failed(CounterReadFailure::Io(error.kind())),
    }
}

fn allocate_generation_epoch_with_io<L, R, W, F>(
    io: GenerationIo<L, R, W, F>,
) -> ProcessGenerationAllocation
where
    L: FnOnce(&Path) -> Result<GenerationFileLock, String>,
    R: FnOnce(&Path) -> CounterRead,
    W: FnOnce(&Path, &str) -> Result<(), String>,
    F: FnOnce(&Path) -> std::io::Result<()>,
{
    use GenerationAllocationRoute as Route;

    let Some(path) = io.path else {
        tracing::warn!(
            path = "<unavailable>",
            current = 0u64,
            next = 0u64,
            counter_read = "not_attempted",
            counter_detail = "not_attempted",
            route = Route::PathUnavailable.as_str(),
            epoch_advanced = false,
            "runtime generation counter path is unavailable; allocation was not attempted"
        );
        return ProcessGenerationAllocation {
            generation: 0,
            route: Route::PathUnavailable,
        };
    };

    let _lock = match (io.lock)(&path) {
        Ok(lock) => lock,
        Err(error) => {
            let read = (io.read)(&path);
            let generation = read.value();
            tracing::error!(
                path = %path.display(),
                current = generation,
                next = generation,
                counter_read = read.as_str(),
                counter_detail = read.failure().map_or("none", CounterReadFailure::as_str),
                error = %error,
                route = Route::LockFailed.as_str(),
                epoch_advanced = false,
                "failed to lock runtime generation counter"
            );
            return ProcessGenerationAllocation {
                generation,
                route: Route::LockFailed,
            };
        }
    };

    let read = (io.read)(&path);
    let current = read.value();
    let next = current.saturating_add(1);
    match (io.write)(&path, &next.to_string()) {
        Err(error) => {
            tracing::error!(
                path = %path.display(),
                current,
                next,
                counter_read = read.as_str(),
                counter_detail = read.failure().map_or("none", CounterReadFailure::as_str),
                error = %error,
                route = Route::WriteFailed.as_str(),
                epoch_advanced = false,
                "failed to allocate runtime process generation"
            );
            ProcessGenerationAllocation {
                generation: current,
                route: Route::WriteFailed,
            }
        }
        Ok(()) if next == current => {
            tracing::error!(
                path = %path.display(),
                current,
                next,
                counter_read = read.as_str(),
                counter_detail = read.failure().map_or("none", CounterReadFailure::as_str),
                route = Route::Saturated.as_str(),
                epoch_advanced = false,
                "runtime process generation counter is saturated"
            );
            ProcessGenerationAllocation {
                generation: next,
                route: Route::Saturated,
            }
        }
        Ok(()) => match (io.fsync)(&path) {
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    current,
                    next,
                    counter_read = read.as_str(),
                    counter_detail = read.failure().map_or("none", CounterReadFailure::as_str),
                    error = %error,
                    route = Route::ParentSyncFailed.as_str(),
                    epoch_advanced = false,
                    "runtime generation counter was renamed but parent-directory sync returned an error"
                );
                ProcessGenerationAllocation {
                    generation: next,
                    route: Route::ParentSyncFailed,
                }
            }
            Ok(()) if !read.established() => {
                tracing::warn!(
                    path = %path.display(),
                    current,
                    next,
                    counter_read = read.as_str(),
                    counter_detail = read.failure().map_or("none", CounterReadFailure::as_str),
                    route = Route::CounterReadFailed.as_str(),
                    epoch_advanced = false,
                    "runtime generation counter was renamed, but the prior value was not established"
                );
                ProcessGenerationAllocation {
                    generation: next,
                    route: Route::CounterReadFailed,
                }
            }
            Ok(()) if !io.flushes => {
                tracing::info!(
                    path = %path.display(),
                    current,
                    next,
                    counter_read = read.as_str(),
                    counter_detail = read.failure().map_or("none", CounterReadFailure::as_str),
                    route = Route::AdvancedWithUnflushedRename.as_str(),
                    epoch_advanced = false,
                    "allocated runtime process generation without a parent directory flush"
                );
                ProcessGenerationAllocation {
                    generation: next,
                    route: Route::AdvancedWithUnflushedRename,
                }
            }
            Ok(()) => {
                tracing::info!(
                    path = %path.display(),
                    current,
                    next,
                    counter_read = read.as_str(),
                    counter_detail = read.failure().map_or("none", CounterReadFailure::as_str),
                    route = Route::AdvancedWithSyncedRename.as_str(),
                    epoch_advanced = true,
                    "allocated runtime process generation"
                );
                ProcessGenerationAllocation {
                    generation: next,
                    route: Route::AdvancedWithSyncedRename,
                }
            }
        },
    }
}

/// Return the process generation; before boot allocation, use the on-disk
/// counter for constructor-only compatibility.
pub fn process_generation() -> u64 {
    #[cfg(not(test))]
    {
        process_generation_binding().generation
    }
    #[cfg(test)]
    {
        if let Some(generation) = *test_generation_override() {
            return generation;
        }
        load_generation()
    }
}

pub(crate) fn process_generation_binding() -> ProcessGenerationAllocation {
    #[cfg(not(test))]
    if let Some(bound) = PROCESS_GENERATION.get() {
        return *bound;
    }
    #[cfg(test)]
    {
        if let Some(generation) = *test_generation_override() {
            return ProcessGenerationAllocation {
                generation,
                route: GenerationAllocationRoute::Unwitnessed,
            };
        }
        if let Some(bound) = test_generation_publication::published_binding() {
            return bound;
        }
    }
    ProcessGenerationAllocation {
        generation: load_generation(),
        route: GenerationAllocationRoute::Unwitnessed,
    }
}

/// Preview the epoch that the replacement process will allocate without
/// mutating the on-disk counter while the old process is still quiescing.
pub fn next_process_generation() -> u64 {
    load_generation().saturating_add(1)
}

#[cfg(test)]
pub fn set_process_generation_for_tests(generation: Option<u64>) {
    let _allocation = lock_unpoisoned(&PROCESS_GENERATION_ALLOCATION_TRANSACTION);
    *test_generation_override() = generation;
    if generation.is_none() {
        *test_allocation_memo() = None;
    }
}

#[cfg(test)]
mod generation_allocation_tests;
