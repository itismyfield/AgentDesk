// #2441 (H1+H4) — graduate the polling sleeps in `tmux_watcher.rs`
// (200ms/250ms × 6 sites) and the 2s `TMUX_LIVENESS_PROBE_INTERVAL` from
// pure fixed-interval polling onto a `notify`-crate-backed file-system
// watcher. The watcher exposes one `Arc<Notify>` per session jsonl path
// that's pumped whenever the underlying file is created / modified /
// removed / renamed; the sleeping watcher loop uses `tokio::select!` to
// race a `notified()` future against the existing sleep so a real write
// wakes us immediately while the sleep continues to act as the upper
// bound for the wake-up latency (fallback for environments where the
// notify backend silently drops events, e.g. some sandboxed FS layers).
//
// Design notes:
//  * One OS thread per `JsonlWatcher` instance: the `notify` crate uses
//    sync callbacks and we do not want to block the tokio runtime threads
//    on the FS event channel. The forwarding is `mpsc::Sender<()>` →
//    `Notify::notify_waiters` on the tokio side.
//  * We watch the **parent directory** in `NonRecursive` mode and filter
//    by file_name. This matches the pattern already battle-tested in
//    `mcp_credential_watcher.rs:344-410` and survives atomic-rename /
//    inode-replacement (rotation via `RotatingJsonlWriter`), which a
//    direct watch on the file path would not.
//  * Spurious wake-ups are explicitly allowed: a caller that wakes up
//    early and reads no new bytes simply re-arms its `Notify` and sleeps
//    again. This is the same shape as a condition variable's "wait until
//    predicate". The fallback sleep handles the case where the notify
//    backend never delivers.
//
// Cross-platform: `notify = "7"` uses FSEvents on macOS and inotify on
// Linux; both deliver Modify events on plain append writes which is what
// the wrappers do via `RotatingJsonlWriter::write_line` → `writeln!` →
// `flush()`. We do *not* require Modify(Data) specifically — any
// `Modify(_)` / `Create(_)` / `Remove(_)` is treated as a wake-up.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

/// A best-effort filesystem watcher that pumps a `Notify` whenever the
/// jsonl path under watch changes. Drop the handle to stop the watcher
/// thread (the underlying `notify::RecommendedWatcher` is dropped along
/// with the spawn closure).
pub(in crate::services::discord) struct JsonlWatcher {
    notify: Arc<Notify>,
    _stop: Arc<std::sync::atomic::AtomicBool>,
}

impl JsonlWatcher {
    /// Spawn a new watcher for `path`. Returns immediately even if the
    /// path's parent does not yet exist — the OS thread retries every
    /// 5 seconds until the parent appears (this matches the pattern in
    /// `mcp_credential_watcher.rs:411-423`). On any setup failure we log
    /// and return a Notify that simply never fires; the watcher loop's
    /// fallback sleep is the safety net.
    pub(in crate::services::discord) fn spawn(path: PathBuf) -> Arc<Self> {
        let notify = Arc::new(Notify::new());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = Arc::new(Self {
            notify: notify.clone(),
            _stop: stop.clone(),
        });

        let watch_target_path = path.clone();
        let watch_target_filename = path.file_name().map(|s| s.to_os_string());
        let Some(target_parent) = path.parent().map(PathBuf::from) else {
            tracing::warn!(
                "jsonl_watcher: refusing to spawn for path with no parent dir: {}",
                path.display()
            );
            return handle;
        };
        let Some(target_filename) = watch_target_filename else {
            tracing::warn!(
                "jsonl_watcher: refusing to spawn for path with no file name: {}",
                path.display()
            );
            return handle;
        };

        let notify_for_thread = notify.clone();
        let stop_for_thread = stop.clone();
        let result = std::thread::Builder::new()
            .name(format!(
                "jsonl-watcher-{}",
                path.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("anon")
                    .chars()
                    .take(40)
                    .collect::<String>()
            ))
            .spawn(move || {
                run_watcher_thread(
                    target_parent,
                    target_filename,
                    watch_target_path,
                    notify_for_thread,
                    stop_for_thread,
                );
            });
        if let Err(err) = result {
            tracing::warn!("jsonl_watcher: failed to spawn watcher thread: {err}");
        }
        handle
    }

    /// Returns an `Arc<Notify>` cloned from the watcher. Callers `await`
    /// `notify.notified()` (typically in a `tokio::select!`) to receive
    /// wake-ups. The notify is fired with `notify_waiters()` so every
    /// active waiter wakes on each FS event; spurious wake-ups are
    /// permitted.
    pub(in crate::services::discord) fn notify(&self) -> Arc<Notify> {
        self.notify.clone()
    }
}

fn run_watcher_thread(
    parent: PathBuf,
    filename: std::ffi::OsString,
    full_path: PathBuf,
    notify: Arc<Notify>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

    let notify_for_callback = notify.clone();
    let filename_for_callback = filename.clone();
    let full_path_for_callback = full_path.clone();
    let mut watcher: RecommendedWatcher =
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            // We treat all Create/Modify/Remove flavors as wake-up
            // signals. notify v7 also emits `Access(_)` and `Other` —
            // those are ignored because they don't change the bytes the
            // watcher loop is waiting on.
            if !matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                return;
            }
            let matched = event.paths.iter().any(|p| {
                p.file_name() == Some(filename_for_callback.as_os_str())
                    || p == &full_path_for_callback
            });
            if matched {
                notify_for_callback.notify_waiters();
            }
        }) {
            Ok(w) => w,
            Err(err) => {
                tracing::warn!(
                    "jsonl_watcher: failed to create RecommendedWatcher for {}: {err}",
                    full_path.display()
                );
                return;
            }
        };

    // Attach the parent dir. Retry every 5s while it does not exist —
    // the watcher might race ahead of the wrapper that creates the
    // sessions directory. The sleep is gated by `stop` so the thread
    // exits promptly when the JsonlWatcher is dropped.
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        if parent.exists() {
            match watcher.watch(&parent, RecursiveMode::NonRecursive) {
                Ok(()) => break,
                Err(err) => {
                    tracing::warn!(
                        "jsonl_watcher: cannot watch {}: {err}",
                        parent.display()
                    );
                    // No retry policy will recover from a hard error
                    // (e.g. permission denied) — fall through to the
                    // park loop so the Notify never fires and the
                    // watcher's fallback sleep is the source of truth.
                    park_forever(stop);
                    return;
                }
            }
        } else {
            std::thread::sleep(Duration::from_secs(5));
        }
    }

    // Park the thread holding the watcher alive. Notifications are
    // dispatched from the watcher's internal callback thread, not from
    // here. Drop semantics: when `JsonlWatcher` is dropped, `stop` flips
    // and this thread exits, which drops the `watcher` and tears down
    // the inotify/FSEvents registration.
    park_forever(stop);
    drop(watcher);
}

fn park_forever(stop: Arc<std::sync::atomic::AtomicBool>) {
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(60));
    }
}

impl Drop for JsonlWatcher {
    fn drop(&mut self) {
        self._stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // Last wake so any pinned futures awaiting the Notify can
        // observe the shutdown and bail out of their select arms.
        self.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn notify_fires_when_file_modified() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        let watcher = JsonlWatcher::spawn(path.clone());
        let notify = watcher.notify();

        // Give the notify backend a beat to register the watch.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let waiter = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(3), notify.notified())
                .await
                .map(|_| ())
        });

        // Append a line — the wrapper's normal write pattern.
        std::fs::write(&path, "{}\n{}\n").unwrap();

        let result = waiter.await.expect("waiter task panicked");
        assert!(
            result.is_ok(),
            "JsonlWatcher Notify should fire on file modify"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn notify_is_inert_when_parent_missing_but_does_not_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let nonexistent = tmp.path().join("nope/session.jsonl");
        let watcher = JsonlWatcher::spawn(nonexistent);
        let _notify = watcher.notify();
        // Just confirm no panic and the watcher object is usable.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
