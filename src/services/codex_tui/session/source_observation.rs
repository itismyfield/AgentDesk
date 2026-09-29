//! Metadata comparison only: native IDs are not ADK turn/lease or launch proof.
use super::*;
use crate::services::cluster::stream_relay::SourceFileIdentity;
use std::io::{BufRead, BufReader, Read};

// Install holds source authority: bounded bytes/lines, no waiting or retries.
const HEADER_BYTES: u64 = 64 * 1024;
const HEADER_LINES: usize = 16;

pub(super) fn observe(path: &Path, supplied: Option<&str>) -> &'static str {
    let root = default_codex_sessions_dir().and_then(|p| p.canonicalize().ok());
    let canonical = path.canonicalize().ok();
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).ok();
    let identity = file.as_ref().map(SourceFileIdentity::from_open_file);
    let verdict = match (root.as_ref(), canonical.as_ref(), file) {
        (Some(root), Some(path), Some(file)) if path.starts_with(root) => compare(file, supplied),
        _ => "pending",
    };
    // This install log is the consumer, not durable source selection evidence.
    tracing::info!(
        scope = "source_metadata_only",
        launch_verified = false,
        verdict,
        ?root,
        ?canonical,
        ?identity,
        "Codex rollout metadata observation"
    );
    verdict
}

pub(super) fn compare(file: std::fs::File, supplied: Option<&str>) -> &'static str {
    let Some(supplied) = supplied.and_then(|id| uuid::Uuid::parse_str(id.trim()).ok()) else {
        return "pending";
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return "pending";
    }
    let mut reader = BufReader::new(file.take(HEADER_BYTES));
    let mut line = String::new();
    for _ in 0..HEADER_LINES {
        line.clear();
        if reader.read_line(&mut line).is_err() || !line.ends_with('\n') {
            return "pending";
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            return "pending";
        };
        if value["type"] != "session_meta" {
            continue;
        }
        let meta = &value["payload"];
        let Some(id) = meta["id"]
            .as_str()
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
        else {
            return "pending";
        };
        if id != supplied {
            return "rejected";
        }
        let source = &meta["source"];
        if source.get("subagent").is_some() || meta["parent_thread_id"].as_str().is_some() {
            return "child";
        }
        return match source.as_str() {
            Some("cli" | "vscode" | "exec") => "matched",
            _ => "pending",
        };
    }
    "pending"
}

/// Which Codex front end a launch expects the rollout header to name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexRolloutSource {
    Cli,
    Exec,
}

impl CodexRolloutSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Exec => "exec",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexHookSourceRoute {
    PayloadPath,
    RolloutIndex,
}

/// One Codex hook's claim: native session id, optional rollout path, launch mode.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CodexHookSourceClaim<'a> {
    pub session_id: &'a str,
    pub transcript_path: Option<&'a Path>,
    pub expected_source: CodexRolloutSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedCodexHookSource {
    pub session_id: String,
    pub rollout_path: PathBuf,
    pub identity: SourceFileIdentity,
    pub route: CodexHookSourceRoute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CodexHookSourceRejection {
    InvalidSessionId,
    FileNameMismatch,
    OutsideSessionsRoot,
    RolloutUnavailable,
    RolloutReplaced,
    SessionMetaMissing,
    SessionMetaIdMismatch,
    SourceMismatch { found: Option<String> },
    NoCandidate,
    AmbiguousCandidates(usize),
}

impl CodexHookSourceRejection {
    /// A retry hint only; none of these outcomes is ever an accepted source.
    pub(crate) fn may_resolve_later(&self) -> bool {
        matches!(
            self,
            Self::RolloutUnavailable
                | Self::RolloutReplaced
                | Self::SessionMetaMissing
                | Self::NoCandidate
        )
    }
}

/// Accepts a hook's rollout only when its file name, root, header id and source all agree.
pub(crate) fn verify_codex_hook_source(
    sessions_root: &Path,
    claim: &CodexHookSourceClaim<'_>,
) -> Result<VerifiedCodexHookSource, CodexHookSourceRejection> {
    let id = uuid::Uuid::parse_str(claim.session_id)
        .ok()
        .filter(|id| id.hyphenated().to_string() == claim.session_id)
        .ok_or(CodexHookSourceRejection::InvalidSessionId)?;
    let suffix = format!("-{id}.jsonl");
    if let Some(path) = claim.transcript_path {
        return verify_rollout(sessions_root, path, id, &suffix, claim.expected_source)
            .map(|source| source.via(CodexHookSourceRoute::PayloadPath));
    }
    let mut candidates: Vec<PathBuf> = cached_indexed_rollouts(sessions_root)
        .into_iter()
        .filter(|item| {
            rollout_file_name_matches(&item.path, &suffix)
                || item
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.id.as_deref())
                    .and_then(|found| uuid::Uuid::parse_str(found).ok())
                    == Some(id)
        })
        .map(|item| item.path)
        .collect();
    candidates.sort();
    candidates.dedup();
    match candidates.as_slice() {
        [] => Err(CodexHookSourceRejection::NoCandidate),
        [path] => verify_rollout(sessions_root, path, id, &suffix, claim.expected_source)
            .map(|source| source.via(CodexHookSourceRoute::RolloutIndex)),
        many => Err(CodexHookSourceRejection::AmbiguousCandidates(many.len())),
    }
}

impl VerifiedCodexHookSource {
    fn via(mut self, route: CodexHookSourceRoute) -> Self {
        self.route = route;
        self
    }
}

fn rollout_file_name_matches(path: &Path, suffix: &str) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(suffix))
}

fn open_rollout_identity(path: &Path) -> Option<SourceFileIdentity> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).ok()?;
    file.metadata()
        .is_ok_and(|meta| meta.is_file())
        .then(|| SourceFileIdentity::from_open_file(&file))
}

fn verify_rollout(
    sessions_root: &Path,
    path: &Path,
    id: uuid::Uuid,
    suffix: &str,
    expected: CodexRolloutSource,
) -> Result<VerifiedCodexHookSource, CodexHookSourceRejection> {
    use CodexHookSourceRejection as Reject;
    if !rollout_file_name_matches(path, suffix) {
        return Err(Reject::FileNameMismatch);
    }
    let (Ok(root), Ok(canonical)) = (sessions_root.canonicalize(), path.canonicalize()) else {
        return Err(Reject::RolloutUnavailable);
    };
    if !canonical.starts_with(&root) {
        return Err(Reject::OutsideSessionsRoot);
    }
    let identity = open_rollout_identity(&canonical).ok_or(Reject::RolloutUnavailable)?;
    let meta = crate::services::codex_tui::rollout_index::read_rollout_session_meta(&canonical)
        .ok_or(Reject::SessionMetaMissing)?;
    let meta_id = meta
        .id
        .as_deref()
        .and_then(|found| uuid::Uuid::parse_str(found).ok());
    if meta_id != Some(id) {
        return Err(Reject::SessionMetaIdMismatch);
    }
    let source_matches = meta.source.as_deref() == Some(expected.as_str())
        && (expected == CodexRolloutSource::Exec || meta.is_tui_compatible());
    if !source_matches {
        return Err(Reject::SourceMismatch { found: meta.source });
    }
    // The header must have been read from the same inode that was pinned above.
    if open_rollout_identity(&canonical) != Some(identity) {
        return Err(Reject::RolloutReplaced);
    }
    Ok(VerifiedCodexHookSource {
        session_id: id.hyphenated().to_string(),
        rollout_path: canonical,
        identity,
        route: CodexHookSourceRoute::PayloadPath,
    })
}

#[cfg(test)]
#[path = "source_observation_tests.rs"]
mod source_observation_tests;
