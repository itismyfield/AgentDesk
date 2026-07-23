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
        self.resolve_ownership_inner(|| {}, || {}, append_force_kill_phase, on_removed_owned)
    }

    fn resolve_ownership_inner(
        &self,
        after_claim: impl FnOnce(),
        after_reservation: impl FnOnce(),
        append_phase: impl FnOnce(&Path) -> io::Result<()>,
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

        match fs::hard_link(&claimed_path, &self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let replacement =
                    RestartMarkerOwner::from_path(&self.path).unwrap_or(RestartMarkerOwner {
                        nonce: None,
                        source: None,
                        scope: None,
                    });
                fs::remove_file(claimed_path)?;
                return Ok(MarkerOwnership::Replaced(replacement));
            }
            Err(error) => {
                restore_claimed_marker(&claimed_path, &self.path);
                return Err(error);
            }
        }

        if let Err(error) = append_phase(&claimed_path) {
            identity_safe_remove(&self.path, &self.nonce)?;
            restore_claimed_marker(&claimed_path, &self.path);
            return Err(error);
        }
        after_reservation();
        // Keep the canonical hard link occupied through the destructive callback.
        // If this process dies during force-kill, the nonce-bound marker remains
        // for the next runtime to consume, so the interrupted fallback converges.
        on_removed_owned();
        identity_safe_remove(&self.path, &self.nonce)?;
        fs::remove_file(claimed_path)?;
        Ok(MarkerOwnership::RemovedOwned)
    }
}

fn restore_claimed_marker(claimed_path: &Path, marker_path: &Path) {
    if fs::hard_link(claimed_path, marker_path).is_ok() {
        let _ = fs::remove_file(claimed_path);
    }
}

fn identity_safe_remove(marker_path: &Path, expected_nonce: &str) -> io::Result<()> {
    let cleanup_path =
        marker_path.with_file_name(format!(".restart_pending.cleanup.{}", uuid::Uuid::new_v4()));
    match fs::rename(marker_path, &cleanup_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }

    let owner = match RestartMarkerOwner::from_path(&cleanup_path) {
        Ok(owner) => owner,
        Err(error) => {
            restore_claimed_marker(&cleanup_path, marker_path);
            return Err(error);
        }
    };
    if owner.nonce.as_deref() == Some(expected_nonce) {
        return fs::remove_file(cleanup_path);
    }

    restore_claimed_marker(&cleanup_path, marker_path);
    Ok(())
}

fn append_force_kill_phase(path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().append(true).open(path)?;
    file.write_all(b"phase=force_kill\n")
}

pub(crate) fn create_quick_restart_marker(
    runtime_root: &Path,
    version: &str,
) -> Result<QuickRestartMarker, RestartMarkerCreateError> {
    create_quick_restart_marker_inner(runtime_root, version, |file, body| {
        file.write_all(body.as_bytes())?;
        file.flush()
    })
}

fn create_quick_restart_marker_inner(
    runtime_root: &Path,
    version: &str,
    write_staging: impl FnOnce(&mut fs::File, &str) -> io::Result<()>,
) -> Result<QuickRestartMarker, RestartMarkerCreateError> {
    let path = runtime_root.join("restart_pending");
    let staging_path =
        runtime_root.join(format!(".restart_pending.publish.{}", uuid::Uuid::new_v4()));
    let nonce = uuid::Uuid::new_v4();
    let body = format!(
        "nonce={nonce}\nsource={QUICK_RESTART_SOURCE}\nscope={QUICK_RESTART_SCOPE}\nversion={version}\nrequested_at={}\n",
        chrono::Utc::now().to_rfc3339()
    );

    let mut staging = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging_path)
        .map_err(RestartMarkerCreateError::Io)?;
    if let Err(error) = write_staging(&mut staging, &body) {
        let _ = fs::remove_file(&staging_path);
        return Err(RestartMarkerCreateError::Io(error));
    }
    drop(staging);

    match fs::hard_link(&staging_path, &path) {
        Ok(()) => {
            let _ = fs::remove_file(&staging_path);
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&staging_path);
            let owner = RestartMarkerOwner::from_path(&path).unwrap_or(RestartMarkerOwner {
                nonce: None,
                source: None,
                scope: None,
            });
            return Err(RestartMarkerCreateError::AlreadyOwned(owner));
        }
        Err(error) => {
            let _ = fs::remove_file(&staging_path);
            return Err(RestartMarkerCreateError::Io(error));
        }
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
    fn staging_write_failure_never_touches_canonical_marker() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("restart_pending");
        let existing = "nonce=owner-nonce\nsource=deploy-release\nscope=release\n";
        fs::write(&path, existing).unwrap();

        let result = create_quick_restart_marker_inner(root.path(), "9.9.9", |file, body| {
            file.write_all(&body.as_bytes()[..body.len() / 2])?;
            Err(io::Error::other("injected staging write failure"))
        });

        assert!(matches!(result, Err(RestartMarkerCreateError::Io(_))));
        assert_eq!(fs::read_to_string(path).unwrap(), existing);
        assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".restart_pending.publish.")
        }));
    }

    #[test]
    fn staging_publish_eexist_preserves_existing_owner() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("restart_pending");
        let existing = "nonce=owner-nonce\nsource=deploy-release\nscope=release\n";
        fs::write(&path, existing).unwrap();

        let result = create_quick_restart_marker_inner(root.path(), "9.9.9", |file, body| {
            file.write_all(body.as_bytes())?;
            file.flush()
        });

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
                || {},
                append_force_kill_phase,
                || force_kill_called.set(true),
            )
            .unwrap();

        assert!(matches!(outcome, MarkerOwnership::Replaced(_)));
        assert!(!outcome.permits_force_kill());
        assert!(!force_kill_called.get());
        assert_eq!(fs::read_to_string(marker.path()).unwrap(), replacement);
    }

    #[test]
    fn reservation_blocks_new_exclusive_writer_during_force_kill() {
        let root = tempfile::tempdir().unwrap();
        let marker = create_quick_restart_marker(root.path(), "1.2.3").unwrap();
        let writer_blocked = std::cell::Cell::new(false);
        let phase_visible = std::cell::Cell::new(false);

        let outcome = marker
            .resolve_ownership_inner(
                || {},
                || {
                    phase_visible.set(
                        fs::read_to_string(marker.path())
                            .unwrap()
                            .lines()
                            .any(|line| line == "phase=force_kill"),
                    );
                    writer_blocked.set(matches!(
                        create_quick_restart_marker(root.path(), "2.0.0"),
                        Err(RestartMarkerCreateError::AlreadyOwned(_))
                    ));
                },
                append_force_kill_phase,
                || {},
            )
            .unwrap();

        assert_eq!(outcome, MarkerOwnership::RemovedOwned);
        assert!(phase_visible.get());
        assert!(writer_blocked.get());
    }

    #[test]
    fn reservation_race_returns_replaced_without_force_kill() {
        let root = tempfile::tempdir().unwrap();
        let marker = create_quick_restart_marker(root.path(), "1.2.3").unwrap();
        let replacement = "nonce=replacement-owner\nsource=deploy-release\nscope=release\n";
        let force_kill_called = std::cell::Cell::new(false);

        let outcome = marker
            .resolve_ownership_inner(
                || fs::write(marker.path(), replacement).unwrap(),
                || {},
                append_force_kill_phase,
                || force_kill_called.set(true),
            )
            .unwrap();

        assert!(matches!(outcome, MarkerOwnership::Replaced(_)));
        assert!(!force_kill_called.get());
        assert_eq!(fs::read_to_string(marker.path()).unwrap(), replacement);
    }

    #[test]
    fn cleanup_preserves_replacement_after_runtime_consumes_reservation() {
        let root = tempfile::tempdir().unwrap();
        let marker = create_quick_restart_marker(root.path(), "1.2.3").unwrap();
        let replacement = "nonce=replacement-owner\nsource=deploy-release\nscope=release\n";
        let force_kill_called = std::cell::Cell::new(false);

        let outcome = marker
            .resolve_ownership_inner(
                || {},
                || {
                    fs::remove_file(marker.path()).unwrap();
                    fs::write(marker.path(), replacement).unwrap();
                },
                append_force_kill_phase,
                || force_kill_called.set(true),
            )
            .unwrap();

        assert_eq!(outcome, MarkerOwnership::RemovedOwned);
        assert!(force_kill_called.get());
        assert_eq!(fs::read_to_string(marker.path()).unwrap(), replacement);
    }

    #[test]
    fn append_failure_rollback_preserves_replacement_after_runtime_consumes_reservation() {
        let root = tempfile::tempdir().unwrap();
        let marker = create_quick_restart_marker(root.path(), "1.2.3").unwrap();
        let replacement = "nonce=replacement-owner\nsource=deploy-release\nscope=release\n";
        let force_kill_called = std::cell::Cell::new(false);

        let error = marker
            .resolve_ownership_inner(
                || {},
                || {},
                |_| {
                    fs::remove_file(marker.path()).unwrap();
                    fs::write(marker.path(), replacement).unwrap();
                    Err(io::Error::other("injected append failure"))
                },
                || force_kill_called.set(true),
            )
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
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
