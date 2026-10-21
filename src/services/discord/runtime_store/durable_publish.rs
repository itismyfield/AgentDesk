//! Opt-in durable publication: the `atomic_write` sequence, then, only after a successful rename,
//! a flush of the parent and of the directories up to a trusted root.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use super::durable_ancestors::fsync_created_ancestors;
use super::{AtomicWriteContext, PARENT_DIR_FSYNC_FLUSHES, classify_io_error};

/// Step that failed before the rename made the new bytes visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrePublishStage {
    CreateDir,
    CreateTemp,
    Write,
    FileSync,
    Rename,
}

/// Nothing was published; the target still holds its previous bytes (or is still absent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrePublishError {
    pub(crate) stage: PrePublishStage,
    /// The same text `atomic_write` returns for this failure, ENOSPC classification included.
    pub(crate) message: String,
}

impl std::fmt::Display for PrePublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Directory flush that failed after the rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncStage {
    ParentOpen,
    ParentSync,
    Ancestors,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FlushStatus {
    /// The parent and every directory up to the trusted root were flushed.
    Flushed,
    /// The rename is visible, but this platform offers no directory flush.
    UnflushedPlatform,
    /// The rename is visible, but a directory flush failed: durability is unconfirmed.
    SyncFailed { stage: SyncStage, error: String },
}

/// The new bytes are visible at the target path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Published {
    pub(crate) flush_status: FlushStatus,
}

/// Publishes `data` at `path`, then flushes its parent and the directories up to `root`, the
/// deepest directory whose own entry the caller already trusts to be durable.
pub(crate) fn publish_durable(
    root: &Path,
    path: &Path,
    data: &str,
) -> Result<Published, PrePublishError> {
    write_and_rename(path, data)?;
    Ok(Published {
        flush_status: flush_published(root, path),
    })
}

fn write_and_rename(path: &Path, data: &str) -> Result<(), PrePublishError> {
    let failed = |stage, prefix| {
        move |error| PrePublishError {
            stage,
            message: classify_io_error(prefix, error),
        }
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(failed(PrePublishStage::CreateDir, "create_dir_all"))?;
    }
    let unique = uuid::Uuid::new_v4().simple();
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!(".{file_name}.{unique}.tmp"));
    #[cfg(test)]
    probe::inject(path, probe::Step::CreateTemp, &tmp)
        .map_err(failed(PrePublishStage::CreateTemp, "create_tmp"))?;
    let mut file =
        fs::File::create(&tmp).map_err(failed(PrePublishStage::CreateTemp, "create_tmp"))?;
    #[cfg(test)]
    probe::inject(path, probe::Step::Write, &tmp)
        .map_err(failed(PrePublishStage::Write, "write_all"))?;
    file.write_all(data.as_bytes())
        .map_err(failed(PrePublishStage::Write, "write_all"))?;
    #[cfg(test)]
    probe::inject(path, probe::Step::FileSync, &tmp)
        .map_err(failed(PrePublishStage::FileSync, "sync_all"))?;
    file.sync_all()
        .map_err(failed(PrePublishStage::FileSync, "sync_all"))?;
    #[cfg(test)]
    super::rename_fault::before(path, &tmp);
    let renamed = fs::rename(&tmp, path);
    #[cfg(test)]
    super::rename_fault::after(path, &renamed);
    renamed.map_err(failed(PrePublishStage::Rename, "rename"))
}

fn flush_published(root: &Path, path: &Path) -> FlushStatus {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return FlushStatus::UnflushedPlatform;
    }
    if let Err((stage, error)) = flush_parent(path) {
        return FlushStatus::SyncFailed {
            stage,
            error: error.to_string(),
        };
    }
    match fsync_created_ancestors(root, path) {
        Ok(()) => FlushStatus::Flushed,
        Err(error) => FlushStatus::SyncFailed {
            stage: SyncStage::Ancestors,
            error: error.to_string(),
        },
    }
}

/// The `fsync_parent_dir` contract (read-only open, then `sync_all`), reporting which half failed.
fn flush_parent(path: &Path) -> Result<(), (SyncStage, io::Error)> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    flush_dir(path, parent, SyncStage::ParentOpen, SyncStage::ParentSync)
}

/// Read-only open of `dir` (opening a directory for writing fails with `EISDIR`), then `sync_all`.
pub(super) fn flush_dir(
    target: &Path,
    dir: &Path,
    open_stage: SyncStage,
    sync_stage: SyncStage,
) -> Result<(), (SyncStage, io::Error)> {
    #[cfg(test)]
    match probe::dir_decision(target, open_stage, dir) {
        probe::Decision::Proceed => {}
        probe::Decision::Fail(stage, error) => return Err((stage, error)),
    }
    #[cfg(not(test))]
    let _ = target;
    let opened = fs::File::open(dir).map_err(|error| (open_stage, error));
    let synced = opened.and_then(|file| file.sync_all().map_err(|error| (sync_stage, error)));
    #[cfg(test)]
    probe::record(
        target,
        synced.as_ref().err().map_or(sync_stage, |e| e.0),
        dir,
        synced.is_ok(),
    );
    synced
}

/// `fsync_parent_dir`, then the directories up to the runtime root, for writers that classify the
/// post-rename outcome themselves; outside the runtime root only the parent is synced.
pub(crate) fn fsync_parent_and_ancestors(path: &Path) -> io::Result<()> {
    super::fsync_parent_dir(path)?;
    match super::runtime_root() {
        Some(root) if path.starts_with(&root) => fsync_created_ancestors(&root, path),
        _ => Ok(()),
    }
}

/// `fs::remove_file`, then a parent flush so a crash cannot bring the removed file back. A flush
/// failure is logged and the removal, which already happened, is still reported.
pub(crate) fn remove_file_synced(path: &Path) -> io::Result<()> {
    fs::remove_file(path)?;
    if PARENT_DIR_FSYNC_FLUSHES && let Err((stage, error)) = flush_parent(path) {
        tracing::warn!(
            path = %path.display(),
            stage = ?stage,
            error = %error,
            removed = true,
            "recovery-critical file removed; directory sync failed (removal durability unconfirmed)"
        );
    }
    Ok(())
}

/// Trusted root of a context: the one it names, else the runtime root every store hangs from.
fn trusted_root(context: &AtomicWriteContext<'_>) -> std::path::PathBuf {
    match context.durable_root {
        Some(root) => root.to_path_buf(),
        // Without a runtime root no chain can be bounded, so the ancestor flush reports failure.
        None => super::runtime_root().unwrap_or_default(),
    }
}

fn log_flush_status(path: &Path, context: &AtomicWriteContext<'_>, status: &FlushStatus) {
    match status {
        FlushStatus::Flushed => {}
        FlushStatus::UnflushedPlatform => tracing::debug!(
            store = context.store,
            path = %path.display(),
            provider = ?context.provider,
            channel_id = ?context.channel_id,
            published = true,
            "snapshot published without a directory flush on this platform (durability unconfirmed)"
        ),
        FlushStatus::SyncFailed { stage, error } => tracing::warn!(
            store = context.store,
            path = %path.display(),
            provider = ?context.provider,
            token_hash = ?context.token_hash,
            channel_id = ?context.channel_id,
            session_key = ?context.session_key,
            turn_id = ?context.turn_id,
            stage = ?stage,
            error = %error,
            published = true,
            "recovery-critical snapshot published; directory sync failed (durability unconfirmed)"
        ),
    }
}

/// Recovery-critical publication: only a pre-rename failure is an `Err` (same text and log as
/// before); a later flush failure is logged as published so callers never roll memory back.
pub(crate) fn critical_publish(
    path: &Path,
    data: &str,
    context: AtomicWriteContext<'_>,
) -> Result<(), String> {
    match publish_durable(&trusted_root(&context), path, data) {
        Ok(published) => {
            log_flush_status(path, &context, &published.flush_status);
            Ok(())
        }
        Err(error) => {
            tracing::error!(
                store = context.store,
                path = %path.display(),
                provider = ?context.provider,
                token_hash = ?context.token_hash,
                channel_id = ?context.channel_id,
                session_key = ?context.session_key,
                turn_id = ?context.turn_id,
                stage = ?error.stage,
                error = %error.message,
                "recovery-critical atomic write failed"
            );
            Err(error.message)
        }
    }
}

/// Best-effort durable publication: never fails its caller, but every failure is logged.
pub(crate) fn best_effort_publish_logged(path: &Path, data: &str, context: AtomicWriteContext<'_>) {
    match publish_durable(&trusted_root(&context), path, data) {
        Ok(published) => log_flush_status(path, &context, &published.flush_status),
        Err(error) => tracing::warn!(
            store = context.store,
            path = %path.display(),
            provider = ?context.provider,
            token_hash = ?context.token_hash,
            channel_id = ?context.channel_id,
            session_key = ?context.session_key,
            turn_id = ?context.turn_id,
            stage = ?error.stage,
            error = %error.message,
            "best-effort atomic write failed"
        ),
    }
}

/// The plain `atomic_write` with the recovery-critical failure log, kept for test fixtures.
#[cfg(test)]
pub(crate) fn critical_atomic_write(
    path: &Path,
    data: &str,
    context: AtomicWriteContext<'_>,
) -> Result<(), String> {
    super::atomic_write(path, data).map_err(|error| {
        tracing::error!(
            store = context.store,
            path = %path.display(),
            provider = ?context.provider,
            token_hash = ?context.token_hash,
            channel_id = ?context.channel_id,
            error = %error,
            "recovery-critical atomic write failed"
        );
        error
    })
}

/// Test-only view of every directory flush and injected step failure, scoped by target path so
/// parallel tests in separate temp directories never see each other's events.
#[cfg(test)]
#[path = "durable_publish_tests.rs"]
mod tests;

#[cfg(test)]
pub(crate) mod probe {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Condvar, Mutex};

    use super::SyncStage;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Step {
        CreateTemp,
        Write,
        FileSync,
        ParentOpen,
        ParentSync,
        Ancestors,
    }

    impl Step {
        fn of(stage: SyncStage) -> Self {
            match stage {
                SyncStage::ParentOpen => Step::ParentOpen,
                SyncStage::ParentSync => Step::ParentSync,
                SyncStage::Ancestors => Step::Ancestors,
            }
        }
    }

    /// One write or flush step, with the target's bytes as they stood when it ran.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Event {
        pub(crate) target: PathBuf,
        pub(crate) step: Step,
        pub(crate) dir: PathBuf,
        pub(crate) ok: bool,
        pub(crate) target_bytes: Option<String>,
    }

    /// Holds a flush until released; `entered` reports that a flush reached it.
    #[derive(Default)]
    pub(crate) struct Gate {
        state: Mutex<(bool, bool)>,
        changed: Condvar,
    }

    impl Gate {
        pub(crate) fn release(&self) {
            self.state.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
            self.changed.notify_all();
        }

        pub(crate) fn wait_entered(&self, timeout: std::time::Duration) -> bool {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let (guard, _) = self
                .changed
                .wait_timeout_while(guard, timeout, |state| !state.0)
                .unwrap_or_else(|e| e.into_inner());
            guard.0
        }

        fn pass(&self) {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.0 = true;
            self.changed.notify_all();
            while !guard.1 {
                guard = self.changed.wait(guard).unwrap_or_else(|e| e.into_inner());
            }
        }
    }

    /// What a watched scope does to flushes under it.
    #[derive(Clone, Default)]
    pub(crate) struct Plan {
        /// Every attempt of these steps fails with `EIO`.
        pub(crate) fail: Vec<Step>,
        /// Parent flushes wait here before running.
        pub(crate) block: Option<Arc<Gate>>,
    }

    struct Entry {
        scope: PathBuf,
        plan: Plan,
        events: Vec<Event>,
    }

    static WATCHED: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

    /// Applies `plan` to every target under `scope` until dropped.
    pub(crate) struct Watch(PathBuf);

    impl Watch {
        pub(crate) fn new(scope: &Path, plan: Plan) -> Self {
            WATCHED
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Entry {
                    scope: scope.to_path_buf(),
                    plan,
                    events: Vec::new(),
                });
            Self(scope.to_path_buf())
        }

        pub(crate) fn events(&self) -> Vec<Event> {
            WATCHED
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .find(|entry| entry.scope == self.0)
                .map(|entry| entry.events.clone())
                .unwrap_or_default()
        }

        /// Successful `step`s for targets under `scope`, as the directories they ran on.
        pub(crate) fn flushed(&self, step: Step) -> Vec<PathBuf> {
            self.events()
                .into_iter()
                .filter(|event| event.step == step && event.ok)
                .map(|event| event.dir)
                .collect()
        }

        pub(crate) fn count(&self, step: Step) -> usize {
            self.events().iter().filter(|e| e.step == step).count()
        }
    }

    impl Drop for Watch {
        fn drop(&mut self) {
            let mut watched = WATCHED.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(index) = watched.iter().position(|entry| entry.scope == self.0) {
                watched.remove(index);
            }
        }
    }

    fn plan_for(target: &Path) -> Option<Plan> {
        WATCHED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .rev()
            .find(|entry| target.starts_with(&entry.scope))
            .map(|entry| entry.plan.clone())
    }

    fn eio() -> std::io::Error {
        std::io::Error::from_raw_os_error(5)
    }

    /// Injected failure of a pre-publication step; every attempt is recorded.
    pub(super) fn inject(target: &Path, step: Step, tmp: &Path) -> std::io::Result<()> {
        let fail = plan_for(target).is_some_and(|plan| plan.fail.contains(&step));
        push(target, step, tmp, !fail);
        if fail { Err(eio()) } else { Ok(()) }
    }

    pub(super) enum Decision {
        Proceed,
        Fail(SyncStage, std::io::Error),
    }

    pub(super) fn dir_decision(target: &Path, open_stage: SyncStage, dir: &Path) -> Decision {
        let Some(plan) = plan_for(target) else {
            return Decision::Proceed;
        };
        if open_stage == SyncStage::ParentOpen
            && let Some(gate) = &plan.block
        {
            gate.pass();
        }
        let sync_stage = match open_stage {
            SyncStage::ParentOpen => SyncStage::ParentSync,
            other => other,
        };
        for stage in [open_stage, sync_stage] {
            if plan.fail.contains(&Step::of(stage)) {
                record(target, stage, dir, false);
                return Decision::Fail(stage, eio());
            }
        }
        Decision::Proceed
    }

    pub(super) fn record(target: &Path, stage: SyncStage, dir: &Path, ok: bool) {
        push(target, Step::of(stage), dir, ok);
    }

    fn push(target: &Path, step: Step, dir: &Path, ok: bool) {
        let target_bytes = std::fs::read_to_string(target).ok();
        let mut watched = WATCHED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = watched
            .iter_mut()
            .rev()
            .find(|entry| target.starts_with(&entry.scope))
        {
            entry.events.push(Event {
                target: target.to_path_buf(),
                step,
                dir: dir.to_path_buf(),
                ok,
                target_bytes,
            });
        }
    }
}
