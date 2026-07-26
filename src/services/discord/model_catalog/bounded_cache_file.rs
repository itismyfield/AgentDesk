use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt as _;

#[cfg(unix)]
const SAFE_OPEN_FLAGS: i32 = libc::O_NONBLOCK | libc::O_NOFOLLOW;
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

pub(super) fn read_regular_file_bounded(path: &Path, max_bytes: usize) -> Option<Vec<u8>> {
    read_regular_file_bounded_after_open(path, max_bytes, || {})
}

fn read_regular_file_bounded_after_open(
    path: &Path,
    max_bytes: usize,
    after_open: impl FnOnce(),
) -> Option<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(SAFE_OPEN_FLAGS);
    #[cfg(windows)]
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);

    let mut file = options.open(path).ok()?;
    after_open();
    let metadata = file.metadata().ok()?;
    if !metadata.file_type().is_file() || metadata.is_symlink() || metadata.len() > max_bytes as u64
    {
        return None;
    }

    let mut body = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut body)
        .ok()?;
    (body.len() <= max_bytes && body.len() as u64 == metadata.len()).then_some(body)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn invariant_bounded_cache_file_accepts_regular_file_within_limit() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("cache.json");
        std::fs::write(&path, b"bounded-cache").unwrap();

        assert_eq!(
            read_regular_file_bounded(&path, 64).as_deref(),
            Some(b"bounded-cache".as_slice())
        );
        assert!(read_regular_file_bounded(&path, 4).is_none());
    }

    #[test]
    fn invariant_bounded_cache_file_reads_the_opened_descriptor_after_path_replacement() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("cache.json");
        let replacement = directory.path().join("replacement.json");
        std::fs::write(&path, b"opened-descriptor").unwrap();
        std::fs::write(&replacement, b"replacement-path").unwrap();

        let body = read_regular_file_bounded_after_open(&path, 64, || {
            std::fs::rename(&replacement, &path).unwrap();
        });

        assert_eq!(body.as_deref(), Some(b"opened-descriptor".as_slice()));
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement-path");
    }

    #[cfg(unix)]
    #[test]
    fn invariant_bounded_cache_file_rejects_symlink_and_fifo_without_blocking() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let target = directory.path().join("target.json");
        let link = directory.path().join("link.json");
        let fifo = directory.path().join("cache.fifo");
        std::fs::write(&target, b"cache").unwrap();
        symlink(&target, &link).unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("spawn mkfifo");
        assert!(status.success(), "mkfifo should succeed");

        let started = std::time::Instant::now();
        assert!(read_regular_file_bounded(&link, 64).is_none());
        assert!(read_regular_file_bounded(&fifo, 64).is_none());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "unsafe cache paths must be rejected without a blocking open"
        );
    }

    #[cfg(unix)]
    #[test]
    fn invariant_bounded_cache_file_replacement_race_never_opens_fifo_blocking() {
        use std::os::unix::fs::symlink;
        use std::sync::atomic::{AtomicBool, Ordering};

        let directory = tempdir().unwrap();
        let path = directory.path().join("cache.json");
        let regular = directory.path().join("regular.json");
        let fifo = directory.path().join("cache.fifo");
        std::fs::write(&regular, b"cache").unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("spawn mkfifo");
        assert!(status.success(), "mkfifo should succeed");
        symlink(&regular, &path).unwrap();

        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let writer_stop = std::sync::Arc::clone(&stop);
        let writer_path = path.clone();
        let writer_regular = regular.clone();
        let writer_fifo = fifo.clone();
        let writer = std::thread::spawn(move || {
            while !writer_stop.load(Ordering::Acquire) {
                let _ = std::fs::remove_file(&writer_path);
                let _ = symlink(&writer_regular, &writer_path);
                let _ = std::fs::remove_file(&writer_path);
                let _ = symlink(&writer_fifo, &writer_path);
            }
            let _ = std::fs::remove_file(&writer_path);
        });

        let started = std::time::Instant::now();
        for _ in 0..2_000 {
            let _ = read_regular_file_bounded(&path, 64);
        }
        stop.store(true, Ordering::Release);
        writer.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "regular-file replacement must not leave a blocked reader"
        );
    }
}
