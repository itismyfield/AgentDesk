use std::path::{Path, PathBuf};

pub(crate) fn file_nonce(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().and_then(|request| {
        request
            .lines()
            .find_map(|line| line.strip_prefix("nonce="))
            .map(str::to_owned)
    })
}

/// A restart nonce is a pathname component, so validate the whole string.
/// A line-anchored check could accept `x\n../escape` from its clean first line.
fn nonce_is_path_safe(nonce: &str) -> bool {
    !nonce.is_empty()
        && nonce != "."
        && nonce != ".."
        && nonce.len() <= 128
        && nonce
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Build a request-specific artifact path only for a validated nonce.
/// `None` is fail-closed: callers must not fall back to an unscoped path.
pub(crate) fn request_artifact_path(root: &Path, name: &str, nonce: &str) -> Option<PathBuf> {
    nonce_is_path_safe(nonce).then(|| root.join(format!("{name}.{nonce}")))
}

/// Three-valued read of one restart artifact family. Only the per-request
/// identity name is proof; the fixed-name index is compatibility evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalProof {
    /// The identity artifact name and body both carry the requested nonce.
    Proven,
    /// Only the fixed-name compatibility index carries the nonce. Never green.
    LegacyIndexOnly,
    Absent,
}

pub(crate) fn artifact_proof(root: &Path, name: &str, nonce: &str) -> TerminalProof {
    let Some(identity) = request_artifact_path(root, name, nonce) else {
        // Unsafe input is `Absent`, with no fixed-index fallback.
        tracing::warn!(
            root = %root.display(),
            name = name,
            "restart-nonce-unsafe: refusing to read a terminal artifact for an unvalidated nonce"
        );
        return TerminalProof::Absent;
    };
    if file_nonce(&identity).as_deref() == Some(nonce) {
        return TerminalProof::Proven;
    }
    if file_nonce(&root.join(name)).as_deref() == Some(nonce) {
        return TerminalProof::LegacyIndexOnly;
    }
    TerminalProof::Absent
}

pub(crate) fn persisted_proof(root: &Path, nonce: &str) -> TerminalProof {
    artifact_proof(root, "restart_persisted", nonce)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RestartTerminalProof {
    Persisted,
    Cancelled,
    Pending,
}

fn terminal_proof_with_readers(
    mut read_persisted: impl FnMut() -> TerminalProof,
    mut read_cancelled: impl FnMut() -> TerminalProof,
) -> RestartTerminalProof {
    match read_persisted() {
        TerminalProof::Proven => return RestartTerminalProof::Persisted,
        // A legacy index is neither success proof nor safe cancellation evidence.
        TerminalProof::LegacyIndexOnly => return RestartTerminalProof::Pending,
        TerminalProof::Absent => {}
    }

    if read_cancelled() != TerminalProof::Proven {
        return RestartTerminalProof::Pending;
    }

    // Persisted wins if it lands between the first and final persisted reads.
    match read_persisted() {
        TerminalProof::Proven => RestartTerminalProof::Persisted,
        TerminalProof::LegacyIndexOnly => RestartTerminalProof::Pending,
        TerminalProof::Absent => RestartTerminalProof::Cancelled,
    }
}

pub(crate) fn terminal_proof(root: &Path, nonce: &str) -> RestartTerminalProof {
    terminal_proof_with_readers(
        || persisted_proof(root, nonce),
        || artifact_proof(root, "restart_cancelled", nonce),
    )
}

#[cfg(test)]
pub(crate) fn terminal_proof_with_test_readers(
    read_persisted: impl FnMut() -> TerminalProof,
    read_cancelled: impl FnMut() -> TerminalProof,
) -> RestartTerminalProof {
    terminal_proof_with_readers(read_persisted, read_cancelled)
}
