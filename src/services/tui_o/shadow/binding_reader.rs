//! Transcript source identity for the output track.

use std::io;
use std::path::Path;

use super::SourceId;
use super::capture::file_identity;

/// Stats `path` without opening it.
pub fn source_id_for(session_id: &str, path: &Path) -> io::Result<SourceId> {
    let (dev, ino) = file_identity(&std::fs::metadata(path)?);
    let (session_id, path) = (session_id.to_string(), path.to_path_buf());
    Ok(SourceId {
        session_id,
        path,
        dev,
        ino,
    })
}
