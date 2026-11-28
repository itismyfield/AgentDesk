use same_file::Handle;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind},
    path::Path,
};

pub struct CheckpointLock {
    handle: Handle,
    pub created: bool,
}

impl CheckpointLock {
    pub fn acquire(path: &Path, blocking: bool) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        loop {
            let (file, created) = match options().create_new(true).open(path) {
                Ok(file) => (file, true),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    match open_existing(path) {
                        Ok(file) => (file, false),
                        Err(error) if blocking && error.kind() == ErrorKind::NotFound => continue,
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            };
            let handle = Handle::from_file(file)?;
            if blocking {
                handle.as_file().lock()?;
            } else {
                handle.as_file().try_lock().map_err(io::Error::from)?;
            }

            // A waiter may have opened a lock inode that retention unlinked.
            match open_existing(path).and_then(Handle::from_file) {
                Ok(current) if current == handle => return Ok(Self { handle, created }),
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            if !blocking {
                return Err(ErrorKind::WouldBlock.into());
            }
        }
    }
}

impl Drop for CheckpointLock {
    fn drop(&mut self) {
        let _ = self.handle.as_file().unlock();
    }
}

fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options
}

fn open_existing(path: &Path) -> io::Result<File> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(ErrorKind::InvalidInput.into());
    }
    options().open(path)
}
