use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub(super) fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(super) fn supported() -> io::Result<()> {
    if cfg!(any(target_os = "macos", target_os = "linux")) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "platform_unsupported",
        ))
    }
}

pub(super) fn step(kind: &str, path: &Path) -> io::Result<()> {
    #[cfg(test)]
    super::durability_tests::observe(kind, path)?;
    let _ = (kind, path);
    Ok(())
}

pub(super) fn sync_dir(path: &Path) -> io::Result<()> {
    crate::services::discord::runtime_store::fsync_parent_dir(&path.join(".sync"))?;
    step("dir_sync", path)
}

pub(super) fn ensure_dir(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        ensure_dir(parent)?;
    }
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => return sync_dir(path),
        Ok(_) => return Err(invalid("ledger directory is not a directory")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error),
    }
    let parent = path.parent().ok_or_else(|| invalid("missing parent"))?;
    fs::create_dir(path)?;
    step("mkdir", path)?;
    sync_dir(path)?;
    sync_dir(parent)
}

pub(super) fn open_file(path: &Path, create_new: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(create_new);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

pub(super) fn sync_file(file: &File, path: &Path) -> io::Result<()> {
    file.sync_all()?;
    step("file_sync", path)
}

pub(super) fn sync_wal(file: &File, path: &Path) -> io::Result<()> {
    file.sync_data()?;
    step("file_sync", path)
}

pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| invalid("missing parent"))?;
    let tmp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let mut file = open_file(&tmp, true)?;
    file.write_all(bytes)?;
    step("write", &tmp)?;
    sync_file(&file, &tmp)?;
    fs::rename(&tmp, path)?;
    step("rename", path)?;
    sync_dir(parent)
}

pub(super) fn crc(bytes: &[u8]) -> u32 {
    let mut value = !0u32;
    for byte in bytes {
        value ^= u32::from(*byte);
        for _ in 0..8 {
            value = (value >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(value & 1)));
        }
    }
    !value
}
