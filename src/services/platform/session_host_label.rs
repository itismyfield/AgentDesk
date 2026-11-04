//! A runtime's session-key spelling survives case and DNS suffix changes.
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::OnceLock;

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
                        sync_parent(parent)?;
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
    sync_parent(parent)?;
    Ok(current.to_owned())
}

fn sync_parent(parent: &Path) -> io::Result<()> {
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = parent;
    Ok(())
}

#[cfg(test)]
mod tests;
