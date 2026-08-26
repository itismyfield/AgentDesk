use std::path::{Path, PathBuf};

pub(crate) fn file_nonce(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().and_then(|request| {
        request
            .lines()
            .find_map(|line| line.strip_prefix("nonce="))
            .map(str::to_owned)
    })
}

fn nonce_is_path_safe(nonce: &str) -> bool {
    !nonce.is_empty()
        && nonce != "."
        && nonce != ".."
        && nonce.len() <= 128
        && nonce
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

pub(crate) fn request_artifact_path(root: &Path, name: &str, nonce: &str) -> Option<PathBuf> {
    nonce_is_path_safe(nonce).then(|| root.join(format!("{name}.{nonce}")))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalProof {
    Proven,
    LegacyIndexOnly,
    Absent,
}

pub(crate) fn artifact_proof(root: &Path, name: &str, nonce: &str) -> TerminalProof {
    let Some(identity) = request_artifact_path(root, name, nonce) else {
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

pub(crate) fn terminal_proof(root: &Path, nonce: &str) -> RestartTerminalProof {
    if persisted_proof(root, nonce) == TerminalProof::Proven {
        RestartTerminalProof::Persisted
    } else if artifact_proof(root, "restart_cancelled", nonce) == TerminalProof::Proven {
        RestartTerminalProof::Cancelled
    } else {
        RestartTerminalProof::Pending
    }
}
