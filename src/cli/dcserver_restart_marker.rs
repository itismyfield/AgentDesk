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
    fn from_content(content: &str) -> Self {
        Self {
            nonce: marker_field(content, "nonce"),
            source: marker_field(content, "source"),
            scope: marker_field(content, "scope"),
        }
    }

    fn from_path(path: &Path) -> io::Result<Self> {
        fs::read_to_string(path).map(|content| Self::from_content(&content))
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

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MarkerOwnership {
    RemovedOwned,
    MissingCommitted,
    Replaced(RestartMarkerOwner),
}

impl MarkerOwnership {
    pub(crate) fn permits_force_kill(&self) -> bool {
        matches!(self, Self::RemovedOwned)
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

    pub(crate) fn resolve_ownership(
        &self,
        on_removed_owned: impl FnOnce(),
    ) -> io::Result<MarkerOwnership> {
        self.resolve_ownership_inner(|| {}, on_removed_owned)
    }

    fn resolve_ownership_inner(
        &self,
        after_claim: impl FnOnce(),
        on_removed_owned: impl FnOnce(),
    ) -> io::Result<MarkerOwnership> {
        let claimed_path = self
            .path
            .with_file_name(format!(".restart_pending.resolve.{}", uuid::Uuid::new_v4()));
        match fs::rename(&self.path, &claimed_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(MarkerOwnership::MissingCommitted);
            }
            Err(error) => return Err(error),
        }

        after_claim();
        let claimed_owner = match RestartMarkerOwner::from_path(&claimed_path) {
            Ok(owner) => owner,
            Err(error) => {
                restore_claimed_marker(&claimed_path, &self.path);
                return Err(error);
            }
        };
        if claimed_owner.nonce.as_deref() != Some(self.nonce.as_str()) {
            restore_claimed_marker(&claimed_path, &self.path);
            return Ok(MarkerOwnership::Replaced(claimed_owner));
        }

        if self.path.exists() {
            let replacement = RestartMarkerOwner::from_path(&self.path)?;
            fs::remove_file(claimed_path)?;
            return Ok(MarkerOwnership::Replaced(replacement));
        }

        fs::remove_file(claimed_path)?;
        on_removed_owned();
        Ok(MarkerOwnership::RemovedOwned)
    }
}

fn restore_claimed_marker(claimed_path: &Path, marker_path: &Path) {
    if fs::hard_link(claimed_path, marker_path).is_ok() {
        let _ = fs::remove_file(claimed_path);
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
            let owner = RestartMarkerOwner::from_path(&path).unwrap_or(RestartMarkerOwner {
                nonce: None,
                source: None,
                scope: None,
            });
            return Err(RestartMarkerCreateError::AlreadyOwned(owner));
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
    fn ownership_resolution_preserves_replacement_between_check_and_claim() {
        let root = tempfile::tempdir().unwrap();
        let marker = create_quick_restart_marker(root.path(), "1.2.3").unwrap();
        let replacement = "nonce=replacement-owner\nsource=deploy-release\nscope=release\n";

        let force_kill_called = std::cell::Cell::new(false);
        let outcome = marker
            .resolve_ownership_inner(
                || {
                    fs::write(marker.path(), replacement).unwrap();
                },
                || force_kill_called.set(true),
            )
            .unwrap();

        assert!(matches!(outcome, MarkerOwnership::Replaced(_)));
        assert!(!outcome.permits_force_kill());
        assert!(!force_kill_called.get());
        assert_eq!(fs::read_to_string(marker.path()).unwrap(), replacement);
    }

    #[test]
    fn only_removed_owned_permits_force_kill() {
        let replacement = MarkerOwnership::Replaced(RestartMarkerOwner {
            nonce: Some("other".to_string()),
            source: None,
            scope: None,
        });

        assert!(MarkerOwnership::RemovedOwned.permits_force_kill());
        assert!(!MarkerOwnership::MissingCommitted.permits_force_kill());
        assert!(!replacement.permits_force_kill());
    }

    #[test]
    fn normal_owned_timeout_resolution_removes_marker() {
        let root = tempfile::tempdir().unwrap();
        let marker = create_quick_restart_marker(root.path(), "1.2.3").unwrap();

        let force_kill_called = std::cell::Cell::new(false);
        let outcome = marker
            .resolve_ownership(|| force_kill_called.set(true))
            .unwrap();

        assert_eq!(outcome, MarkerOwnership::RemovedOwned);
        assert!(outcome.permits_force_kill());
        assert!(force_kill_called.get());
        assert!(!marker.path().exists());
    }

    #[test]
    fn normal_ack_resolution_reports_missing_committed() {
        let root = tempfile::tempdir().unwrap();
        let marker = create_quick_restart_marker(root.path(), "1.2.3").unwrap();
        fs::remove_file(marker.path()).unwrap();

        let force_kill_called = std::cell::Cell::new(false);
        let outcome = marker
            .resolve_ownership(|| force_kill_called.set(true))
            .unwrap();

        assert_eq!(outcome, MarkerOwnership::MissingCommitted);
        assert!(!outcome.permits_force_kill());
        assert!(!force_kill_called.get());
    }
}
