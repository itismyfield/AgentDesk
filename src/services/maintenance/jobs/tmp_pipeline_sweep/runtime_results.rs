use crate::services::maintenance::runtime_retention;
use std::{
    fs::{self, OpenOptions},
    io::{self, ErrorKind},
    path::Path,
    time::SystemTime,
};

pub(super) fn sweep(root: &Path, now: SystemTime) {
    let Some(cutoff) = runtime_retention::cutoff(now) else {
        return;
    };
    let runtime = root.join("runtime");
    if !fs::symlink_metadata(&runtime).is_ok_and(|meta| meta.is_dir()) {
        return;
    }
    let deploy = prune(&runtime.join("self_hosted_deploy"), cutoff, |name| {
        name.starts_with("deploy-release-") && (name.ends_with(".sh") || name.ends_with(".log"))
    });
    let review = prune(
        &runtime.join("review_passed"),
        cutoff,
        crate::runtime_layout::is_valid_commit_sha,
    );
    let doctor = runtime.join("doctor");
    let startup = if fs::symlink_metadata(&doctor).is_ok_and(|meta| meta.is_dir())
        && let Ok(current) = crate::runtime_layout::boot_id::current_boot_id_at(root)
    {
        prune(&doctor.join("startup"), cutoff, |name| {
            let Some((stem, suffix)) = name.split_once('.') else {
                return false;
            };
            matches!(suffix, "json" | "lock") && stem != current && is_boot_id(stem)
        })
    } else {
        0
    };
    tracing::info!(
        deploy,
        review,
        startup,
        "runtime result retention completed"
    );
}

fn is_boot_id(stem: &str) -> bool {
    stem.split_once('-').is_some_and(|(pid, timestamp)| {
        pid.parse::<u32>().is_ok() && timestamp.parse::<u64>().is_ok()
    })
}

fn prune(dir: &Path, cutoff: SystemTime, accepts: impl Fn(&str) -> bool) -> usize {
    let entries = fs::symlink_metadata(dir).and_then(|meta| {
        if meta.is_dir() {
            fs::read_dir(dir).map(Some)
        } else {
            Ok(None)
        }
    });
    let entries = match entries {
        Ok(Some(entries)) => entries,
        Ok(None) => return 0,
        Err(error) if error.kind() == ErrorKind::NotFound => return 0,
        Err(error) => {
            tracing::warn!(dir = %dir.display(), %error, "runtime result retention listing failed");
            return 0;
        }
    };
    let mut removed = 0;
    for entry in entries {
        let result = entry.and_then(|entry| {
            if entry.file_name().to_str().is_some_and(&accepts) {
                remove_expired(&entry.path(), cutoff)
            } else {
                Ok(false)
            }
        });
        match result {
            Ok(true) => removed += 1,
            Ok(false) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(dir = %dir.display(), %error, "runtime result removal failed")
            }
        }
    }
    removed
}

fn remove_expired(path: &Path, cutoff: SystemTime) -> io::Result<bool> {
    if !runtime_retention::regular_file_expired(path, cutoff) {
        return Ok(false);
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Ok(false),
        Err(fs::TryLockError::Error(error)) => return Err(error),
    }
    let locked = same_file::Handle::from_file(file)?;
    let current = same_file::Handle::from_file(options.open(path)?)?;
    if locked != current || !runtime_retention::regular_file_expired(path, cutoff) {
        return Ok(false);
    }
    crate::runtime_layout::remove_file_if_present(path)
}
