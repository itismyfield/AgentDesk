use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const QUICK_RESTART_SOURCE: &str = "agentdesk-cli";
const QUICK_RESTART_SCOPE: &str = "dcserver";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RestartMarkerOwner {
    nonce: Option<String>,
    source: Option<String>,
    scope: Option<String>,
}

impl RestartMarkerOwner {
    fn from_path(path: &Path) -> Self {
        let content = fs::read_to_string(path).unwrap_or_default();
        Self {
            nonce: marker_field(&content, "nonce"),
            source: marker_field(&content, "source"),
            scope: marker_field(&content, "scope"),
        }
    }
}

impl fmt::Display for RestartMarkerOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let source = self.source.as_deref().unwrap_or("unknown");
        let scope = self.scope.as_deref().unwrap_or("unknown");
        let nonce = self.nonce.as_deref().unwrap_or("unknown");
        write!(formatter, "source={source}, scope={scope}, nonce={nonce}")
    }
}

#[derive(Debug)]
pub(crate) enum RestartMarkerCreateError {
    AlreadyOwned(RestartMarkerOwner),
    Io(io::Error),
}

impl fmt::Display for RestartMarkerCreateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyOwned(owner) => write!(formatter, "restart already owned ({owner})"),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

pub(crate) struct QuickRestartMarker {
    path: PathBuf,
    nonce: String,
}

impl QuickRestartMarker {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn is_current_owner(&self) -> bool {
        RestartMarkerOwner::from_path(&self.path).nonce.as_deref() == Some(self.nonce.as_str())
    }

    pub(crate) fn remove_if_owned(&self) {
        if self.is_current_owner() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn create_quick_restart_marker(
    runtime_root: &Path,
    version: &str,
) -> Result<QuickRestartMarker, RestartMarkerCreateError> {
    let path = runtime_root.join("restart_pending");
    let nonce = uuid::Uuid::new_v4();
    let body = format!(
        "nonce={nonce}\nsource={QUICK_RESTART_SOURCE}\nscope={QUICK_RESTART_SCOPE}\nversion={version}\nrequested_at={}\n",
        chrono::Utc::now().to_rfc3339()
    );

    let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(RestartMarkerCreateError::AlreadyOwned(
                RestartMarkerOwner::from_path(&path),
            ));
        }
        Err(error) => return Err(RestartMarkerCreateError::Io(error)),
    };

    if let Err(error) = file.write_all(body.as_bytes()) {
        let _ = fs::remove_file(&path);
        return Err(RestartMarkerCreateError::Io(error));
    }

    Ok(QuickRestartMarker {
        path,
        nonce: nonce.to_string(),
    })
}

fn marker_field(content: &str, name: &str) -> Option<String> {
    content.lines().find_map(|line| {
        line.strip_prefix(name)
            .and_then(|value| value.strip_prefix('='))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_restart_marker_contains_nonce_and_shell_protocol_fields() {
        let root = tempfile::tempdir().unwrap();
        let marker = create_quick_restart_marker(root.path(), "1.2.3").unwrap();
        let content = fs::read_to_string(marker.path()).unwrap();

        let nonce = marker_field(&content, "nonce").expect("nonce line");
        assert!(uuid::Uuid::parse_str(&nonce).is_ok());
        assert_eq!(
            marker_field(&content, "source").as_deref(),
            Some("agentdesk-cli")
        );
        assert_eq!(marker_field(&content, "scope").as_deref(), Some("dcserver"));
        assert_eq!(marker_field(&content, "version").as_deref(), Some("1.2.3"));
    }

    #[test]
    fn quick_restart_marker_create_new_preserves_existing_owner() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("restart_pending");
        let existing = "nonce=owner-nonce\nsource=deploy-release\nscope=release\n";
        fs::write(&path, existing).unwrap();

        let result = create_quick_restart_marker(root.path(), "9.9.9");

        assert!(matches!(
            result,
            Err(RestartMarkerCreateError::AlreadyOwned(RestartMarkerOwner {
                nonce: Some(ref nonce),
                source: Some(ref source),
                scope: Some(ref scope),
            })) if nonce == "owner-nonce" && source == "deploy-release" && scope == "release"
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), existing);
    }

    #[test]
    fn quick_restart_cleanup_does_not_remove_replacement_owner() {
        let root = tempfile::tempdir().unwrap();
        let marker = create_quick_restart_marker(root.path(), "1.2.3").unwrap();
        let replacement = "nonce=replacement-owner\nsource=deploy-release\nscope=release\n";
        fs::write(marker.path(), replacement).unwrap();

        marker.remove_if_owned();

        assert_eq!(fs::read_to_string(marker.path()).unwrap(), replacement);
    }
}
