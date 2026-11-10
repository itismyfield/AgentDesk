//! A runtime's session-key spelling survives case and DNS suffix changes.
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
#[cfg(not(test))]
use std::sync::OnceLock;

use crate::domain::file_sync::fsync_parent_dir;

#[cfg(not(test))]
pub(crate) fn session_host_label() -> String {
    static LABEL: OnceLock<String> = OnceLock::new();
    LABEL.get_or_init(|| {
        let current = super::shell::hostname_short();
        let Some(root) = crate::config::runtime_root() else {
            return current;
        };
        match load_label(&root.join("runtime/session_host_label"), &current) {
            Ok(label) => label,
            Err(error) => {
                tracing::warn!(%error, "session host label could not be persisted; using current hostname");
                current
            }
        }
    }).clone()
}

#[cfg(test)]
thread_local! {
    static TEST_LABEL: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn session_host_label() -> String {
    TEST_LABEL
        .with(|label| label.borrow().clone())
        .unwrap_or_else(super::shell::hostname_short)
}

#[cfg(test)]
pub(crate) fn with_session_host_label_path_for_test<R>(path: &Path, run: impl FnOnce() -> R) -> R {
    struct Reset(Option<String>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_LABEL.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let label = load_label(path, &super::shell::hostname_short()).expect("load fixture host label");
    let _reset = Reset(TEST_LABEL.with(|slot| slot.replace(Some(label))));
    run()
}

fn normalized(label: &str) -> String {
    label
        .split('.')
        .next()
        .unwrap_or(label)
        .to_ascii_lowercase()
}

fn stored_label(text: &str) -> Option<&str> {
    let label = text.strip_suffix('\n').unwrap_or(text);
    (!label.is_empty()
        && label.len() <= 255
        && !label.contains(['.', ':', '/', '\\'])
        && !label
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control()))
    .then_some(label)
}

fn load_label(path: &Path, current: &str) -> io::Result<String> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("label has no parent"))?;
    fs::create_dir_all(parent)?;
    loop {
        match fs::read_to_string(path) {
            Ok(text) => {
                if let Some(label) = stored_label(&text)
                    && normalized(label) == normalized(current)
                {
                    return Ok(label.to_owned());
                }
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::InvalidData => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match OpenOptions::new().write(true).create_new(true).open(path) {
                    Ok(mut file) => {
                        writeln!(file, "{current}")?;
                        file.sync_all()?;
                        fsync_parent_dir(path)?;
                        return Ok(current.to_owned());
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
    // Publish a complete replacement so a rename or corrupt file never leaves a partial label.
    let mut replacement = tempfile::NamedTempFile::new_in(parent)?;
    writeln!(replacement, "{current}")?;
    replacement.as_file().sync_all()?;
    replacement.persist(path).map_err(|error| error.error)?;
    fsync_parent_dir(path)?;
    Ok(current.to_owned())
}

#[cfg(test)]
mod production_probe_tests;
#[cfg(test)]
mod tests;
