use std::path::{Path, PathBuf};

pub(super) async fn provision_off_runtime(dashboard_dir: PathBuf) {
    let dashboard_dir_for_task = dashboard_dir.clone();
    match tokio::task::spawn_blocking(move || provision(&dashboard_dir_for_task)).await {
        Ok(ProvisionResult::AlreadyPresent) => {}
        Ok(ProvisionResult::Copied {
            workspace_dist,
            files,
        }) => tracing::info!(
            dashboard_dir = %dashboard_dir.display(),
            workspace_dist = %workspace_dist.display(),
            files,
            "dashboard dist copied"
        ),
        Ok(ProvisionResult::SourceMissing { workspace_dist }) => tracing::warn!(
            dashboard_dir = %dashboard_dir.display(),
            workspace_dist = %workspace_dist.display(),
            "dashboard dist unavailable"
        ),
        Ok(ProvisionResult::CopyFailed {
            workspace_dist,
            error,
        }) => tracing::warn!(
            dashboard_dir = %dashboard_dir.display(),
            workspace_dist = %workspace_dist.display(),
            error,
            "failed to copy dashboard dist"
        ),
        Err(error) => tracing::warn!(
            dashboard_dir = %dashboard_dir.display(),
            error = %error,
            "dashboard provisioning blocking task failed"
        ),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ProvisionResult {
    AlreadyPresent,
    Copied {
        workspace_dist: PathBuf,
        files: usize,
    },
    SourceMissing {
        workspace_dist: PathBuf,
    },
    CopyFailed {
        workspace_dist: PathBuf,
        error: String,
    },
}

#[cfg(test)]
static PROVISION_THREAD_OBSERVER: std::sync::Mutex<
    Option<std::sync::mpsc::Sender<std::thread::ThreadId>>,
> = std::sync::Mutex::new(None);

fn provision(dashboard_dir: &Path) -> ProvisionResult {
    #[cfg(test)]
    if let Some(observer) = PROVISION_THREAD_OBSERVER
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
    {
        let _ = observer.send(std::thread::current().id());
    }
    if dashboard_dir.join("index.html").exists() {
        return ProvisionResult::AlreadyPresent;
    }
    let workspace_dist = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("dashboard/dist");
    if !workspace_dist.join("index.html").exists() {
        return ProvisionResult::SourceMissing { workspace_dist };
    }
    if let Some(parent) = dashboard_dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_dir_all(dashboard_dir);
    match copy_dir_recursive(&workspace_dist, dashboard_dir) {
        Ok(files) => ProvisionResult::Copied {
            workspace_dist,
            files,
        },
        Err(error) => ProvisionResult::CopyFailed {
            workspace_dist,
            error: error.to_string(),
        },
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<usize> {
    std::fs::create_dir_all(dst)?;
    let mut count = 0;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if ty.is_dir() {
            count += copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)?;
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[tokio::test(flavor = "current_thread")]
    async fn existing_dashboard_is_preserved_off_runtime() {
        let root = tempfile::tempdir().unwrap();
        let dashboard = root.path().join("dashboard/dist");
        std::fs::create_dir_all(&dashboard).unwrap();
        std::fs::write(dashboard.join("index.html"), "existing").unwrap();
        let runtime_thread = std::thread::current().id();
        let (sender, receiver) = mpsc::channel();
        *PROVISION_THREAD_OBSERVER
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(sender);

        provision_off_runtime(dashboard.clone()).await;
        let provision_thread = receiver.recv().unwrap();
        *PROVISION_THREAD_OBSERVER
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;

        assert_ne!(provision_thread, runtime_thread);
        assert_eq!(
            std::fs::read_to_string(dashboard.join("index.html")).unwrap(),
            "existing"
        );
    }
}
