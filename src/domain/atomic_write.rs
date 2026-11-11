//! File preparation and atomic publication; observation policy stays with the caller.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    CreateDir,
    CreateTemp,
    Write,
    FileSync,
    Rename,
}

impl Stage {
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Self::CreateDir => "create_dir_all",
            Self::CreateTemp => "create_tmp",
            Self::Write => "write_all",
            Self::FileSync => "sync_all",
            Self::Rename => "rename",
        }
    }

    fn failed(self, error: io::Error) -> NotPublished {
        NotPublished { stage: self, error }
    }
}

/// A failed stage never published the prepared bytes; the original I/O error is owned.
#[derive(Debug)]
pub(crate) struct NotPublished {
    pub(crate) stage: Stage,
    pub(crate) error: io::Error,
}

/// Rename succeeded. This says nothing about parent-directory durability.
#[derive(Debug)]
pub(crate) struct Published;

/// Holds the synced temp file open across the caller's observation and rename.
#[derive(Debug)]
pub(crate) struct PreparedWrite<'a> {
    target: &'a Path,
    temp: PathBuf,
    file: fs::File,
}

pub(crate) fn prepare<'a>(path: &'a Path, data: &str) -> Result<PreparedWrite<'a>, NotPublished> {
    if let Some(parent) = path.parent() {
        #[cfg(test)]
        tests::inject(path, Stage::CreateDir).map_err(|error| Stage::CreateDir.failed(error))?;
        fs::create_dir_all(parent).map_err(|error| Stage::CreateDir.failed(error))?;
    }
    let unique = uuid::Uuid::new_v4().simple();
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let temp = path.with_file_name(format!(".{file_name}.{unique}.tmp"));
    #[cfg(test)]
    tests::inject(path, Stage::CreateTemp).map_err(|error| Stage::CreateTemp.failed(error))?;
    let file = fs::File::create(&temp).map_err(|error| Stage::CreateTemp.failed(error))?;
    let mut prepared = PreparedWrite {
        target: path,
        temp,
        file,
    };
    #[cfg(test)]
    tests::inject(path, Stage::Write).map_err(|error| Stage::Write.failed(error))?;
    prepared
        .file
        .write_all(data.as_bytes())
        .map_err(|error| Stage::Write.failed(error))?;
    #[cfg(test)]
    tests::inject(path, Stage::FileSync).map_err(|error| Stage::FileSync.failed(error))?;
    prepared
        .file
        .sync_all()
        .map_err(|error| Stage::FileSync.failed(error))?;
    Ok(prepared)
}

impl PreparedWrite<'_> {
    /// Only rename publishes the whole file; parent sync is deliberately separate.
    pub(crate) fn publish(self) -> Result<Published, NotPublished> {
        #[cfg(test)]
        tests::inject(self.target, Stage::Rename).map_err(|error| Stage::Rename.failed(error))?;
        #[cfg(test)]
        rename_fault::before(self.target, &self.temp);
        let renamed = fs::rename(&self.temp, self.target);
        #[cfg(test)]
        rename_fault::after(self.target, &renamed);
        renamed
            .map(|()| Published)
            .map_err(|error| Stage::Rename.failed(error))
    }
}

#[cfg(test)]
pub(crate) mod rename_fault {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// (path, fault still pending, rename attempts, their OS errors)
    type Entry = (PathBuf, bool, usize, Vec<String>);
    static ARMED: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

    /// Fails the next rename onto its path; records that path's renames until dropped.
    pub(crate) struct Armed(PathBuf);

    impl Armed {
        pub(crate) fn new(path: &Path) -> Self {
            let mut armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
            armed.push((path.to_path_buf(), true, 0, Vec::new()));
            Self(path.to_path_buf())
        }
        /// Renames attempted onto the path since arming and the errors they returned.
        pub(crate) fn evidence(&self) -> (usize, Vec<String>) {
            let armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
            armed
                .iter()
                .find(|entry| entry.0 == self.0)
                .map(|entry| (entry.2, entry.3.clone()))
                .unwrap_or_default()
        }
    }

    impl Drop for Armed {
        fn drop(&mut self) {
            let mut armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(index) = armed.iter().position(|entry| entry.0 == self.0) {
                armed.remove(index);
            }
        }
    }

    pub(crate) fn before(path: &Path, tmp: &Path) {
        let mut armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = armed.iter_mut().find(|entry| entry.0 == path) {
            entry.2 += 1;
            if std::mem::take(&mut entry.1) {
                let _ = std::fs::remove_file(tmp);
            }
        }
    }

    pub(crate) fn after(path: &Path, renamed: &std::io::Result<()>) {
        let mut armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
        if let (Some(entry), Err(error)) = (armed.iter_mut().find(|entry| entry.0 == path), renamed)
        {
            entry.3.push(error.to_string());
        }
    }
}
#[cfg(test)]
pub(crate) mod tests;
