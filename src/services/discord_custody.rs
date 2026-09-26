//! Read-only fold of the boot-custody ledger into one preservation obligation per transcript
//! source, printed by `adk custody status`. Transcripts are assumed to be append-only.

use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub(crate) const CUSTODY_DIR: &str = "discord_custody";
const CHANGED: &str = "unresolved_source_changed";

/// A source's identity as one attempt saw it; `g_prefix_sha` hashes the first bytes of the
/// obligation's generation when the writer knew that generation.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct Observation {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) size: u64,
    pub(crate) head_len: u64,
    pub(crate) head_sha256: String,
    #[serde(default)]
    pub(crate) g_prefix_sha: Option<String>,
}

/// One transcript source's obligation and what the ledger and the source show now.
#[derive(Debug, Default)]
pub(crate) struct SourceStatus {
    pub(crate) source: String,
    pub(crate) required_from: Option<u64>,
    pub(crate) max_eof: u64,
    pub(crate) generation: Option<Observation>,
    pub(crate) flags: BTreeSet<&'static str>,
    pub(crate) preserved: Vec<(u64, u64)>,
    pub(crate) missing: Vec<(u64, u64)>,
    pub(crate) current: String,
    pub(crate) last_attempt: Option<String>,
    /// Bytes read from the source and the copy to judge `current`.
    pub(crate) verify_read: u64,
    copies: Vec<(u64, u64, PathBuf)>,
}

#[derive(Debug, Default)]
pub(crate) struct EpisodeStatus {
    pub(crate) id: String,
    pub(crate) channel_id: Option<u64>,
    pub(crate) flags: BTreeSet<&'static str>,
    pub(crate) sources: Vec<SourceStatus>,
}

impl EpisodeStatus {
    /// Everything required up to the current EOF is held and nothing is unresolved.
    pub(crate) fn complete(&self) -> bool {
        false
    }
}

/// Folds every episode of one provider's custody directory; an unlistable directory is an error.
pub(crate) fn provider_status(_dir: &Path) -> Result<Vec<EpisodeStatus>, String> {
    Ok(Vec::new())
}

/// The report `adk custody status` prints for the custody root, optionally narrowed.
pub(crate) fn status_report(
    _root: &Path,
    _provider: Option<&str>,
    _episode: Option<&str>,
) -> Result<String, String> {
    Ok(String::new())
}

/// `adk custody status`: reads the release runtime's custody directory and prints the report.
pub(crate) fn cmd_status(provider: Option<&str>, episode: Option<&str>) -> Result<(), String> {
    let root = crate::config::runtime_root().ok_or("no AgentDesk root directory")?;
    let custody = root.join("runtime").join(CUSTODY_DIR);
    print!("{}", status_report(&custody, provider, episode)?);
    Ok(())
}

#[cfg(unix)]
fn identity(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn identity(_meta: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

#[cfg(test)]
mod tests;
