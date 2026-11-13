use std::path::Path;

/// `errno` value for ENOSPC on both Linux and macOS.
const ENOSPC: i32 = 28;

/// Preserve the string error while notifying disk monitoring of ENOSPC.
pub(crate) fn classify_io_error(prefix: &str, error: std::io::Error) -> String {
    if error.raw_os_error() == Some(ENOSPC) {
        crate::services::disk_monitor::record_enospc_now();
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] 💾 ENOSPC at runtime_store::atomic_write ({prefix}): {error}");
        format!("ENOSPC: {prefix}: {error}")
    } else {
        format!("{prefix}: {error}")
    }
}

pub(crate) fn atomic_write(path: &Path, data: &str) -> Result<(), String> {
    let failed = |failure: crate::domain::atomic_write::NotPublished| {
        classify_io_error(failure.stage.prefix(), failure.error)
    };
    crate::domain::atomic_write::prepare(path, data)
        .map_err(failed)?
        .publish()
        .map(|_| ())
        .map_err(failed)
}
