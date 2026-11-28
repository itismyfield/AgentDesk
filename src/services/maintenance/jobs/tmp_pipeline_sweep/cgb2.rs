use super::{
    ALLOWED_TMP_BASE, MAX_ACTIVITY_WALK_DEPTH, MAX_ACTIVITY_WALK_ENTRIES,
    bounded_recursive_last_activity_with_probe, is_stale,
};
use crate::runtime_layout::{remove_dir_all_if_present, remove_file_if_present};
use socket2::{Domain, SockAddr, Socket, Type};
use std::{
    fs::{self, Metadata},
    io::{self, ErrorKind},
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::Path,
    time::{Duration, SystemTime},
};

const PREFIX: &str = "cgb2-";
const MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

pub(super) fn sweep(configured: &Path, dry_run: bool) -> io::Result<()> {
    let tmp = Path::new("/tmp").canonicalize()?;
    let root = if configured == Path::new(ALLOWED_TMP_BASE) {
        tmp.clone()
    } else {
        configured.canonicalize()?
    };
    if !root.starts_with(&tmp) {
        return Ok(());
    }

    let now = SystemTime::now();
    let uid = unsafe { libc::geteuid() };
    let socket_dir = format!("tmux-{uid}");
    let mut removed = 0;
    let sockets = root.join(&socket_dir);
    match socket_entries(&sockets, uid) {
        Ok(Some(entries)) => {
            for entry in entries.flatten() {
                let path = entry.path();
                match prune_socket(&path, uid, now, dry_run) {
                    Ok(true) => removed += 1,
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(path = %path.display(), %error, "cgb2 socket retention failed")
                    }
                }
            }
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(path = %sockets.display(), %error, "cgb2 socket directory unavailable")
        }
    }
    for entry in fs::read_dir(&root)?.flatten() {
        let path = entry.path();
        match prune_fixture(&path, uid, now, dry_run) {
            Ok(true) => removed += 1,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "cgb2 fixture retention failed")
            }
        }
    }
    tracing::info!(removed, dry_run, "cgb2 retention completed");
    Ok(())
}

fn socket_entries(path: &Path, uid: u32) -> io::Result<Option<fs::ReadDir>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && meta.uid() == uid && meta.mode() & 0o077 == 0 => {
            fs::read_dir(path).map(Some)
        }
        Ok(_) => Err(ErrorKind::PermissionDenied.into()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn is_fixture_path(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.as_encoded_bytes().starts_with(PREFIX.as_bytes()))
}

fn prune_socket(path: &Path, uid: u32, now: SystemTime, dry_run: bool) -> io::Result<bool> {
    if !is_fixture_path(path) || !dead_old_socket(path, uid, now)? || dry_run {
        return Ok(false);
    }
    remove_file_if_present(path)
}

fn prune_fixture(dir: &Path, uid: u32, now: SystemTime, dry_run: bool) -> io::Result<bool> {
    if !is_fixture_path(dir) {
        return Ok(false);
    }
    let meta = fs::symlink_metadata(dir)?;
    if meta.file_type().is_socket() {
        return prune_socket(dir, uid, now, dry_run);
    }
    if !meta.is_dir() || meta.uid() != uid || !is_stale(meta.modified()?, now, MIN_AGE) {
        return Ok(false);
    }
    let activity = bounded_recursive_last_activity_with_probe(
        dir,
        MAX_ACTIVITY_WALK_DEPTH,
        MAX_ACTIVITY_WALK_ENTRIES,
        |path, metadata| {
            !metadata.file_type().is_socket()
                || (is_fixture_path(path) && dead_old_socket(path, uid, now).unwrap_or(false))
        },
    )?;
    if activity.traversal_errors != 0
        || !is_stale(activity.last_activity, now, MIN_AGE)
        || !same_entry(&meta, &fs::symlink_metadata(dir)?)
        || dry_run
    {
        return Ok(false);
    }
    remove_dir_all_if_present(dir)
}

fn dead_old_socket(path: &Path, uid: u32, now: SystemTime) -> io::Result<bool> {
    let before = fs::symlink_metadata(path)?;
    if !before.file_type().is_socket()
        || before.uid() != uid
        || !is_stale(before.modified()?, now, MIN_AGE)
    {
        return Ok(false);
    }
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    socket.set_nonblocking(true)?;
    let address = SockAddr::unix(path)?;
    if !matches!(socket.connect(&address), Err(error) if error.kind() == ErrorKind::ConnectionRefused)
    {
        return Ok(false);
    }
    Ok(same_entry(&before, &fs::symlink_metadata(path)?))
}

fn same_entry(before: &Metadata, after: &Metadata) -> bool {
    (
        before.dev(),
        before.ino(),
        before.mtime(),
        before.mtime_nsec(),
    ) == (after.dev(), after.ino(), after.mtime(), after.mtime_nsec())
}
