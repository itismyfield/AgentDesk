tokio::task_local! {
    pub(in crate::services::discord) static RECORD_REGISTRY_LOCK:
        (std::path::PathBuf, std::sync::Arc<std::sync::atomic::AtomicBool>);
}

pub(super) fn lock_registry() -> Option<std::fs::File> {
    RECORD_REGISTRY_LOCK
        .try_with(|(path, entered)| {
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path.with_extension("lock"))
                .unwrap();
            lock.try_lock().unwrap();
            entered.store(true, std::sync::atomic::Ordering::SeqCst);
            lock
        })
        .ok()
}
