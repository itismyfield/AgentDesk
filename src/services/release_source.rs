use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReleaseSourceObservation {
    Observed {
        deployed_repo_head: String,
        deployed_latest_postgres_migration: String,
    },
    Unobserved {
        reason: ReleaseSourceUnobservedReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReleaseSourceUnobservedReason {
    RuntimeRootUnavailable,
    ManifestMissing,
    ManifestUnreadable,
    ManifestEmpty,
    ManifestInvalidJson,
    RepoHeadMissing,
    LatestPostgresMigrationMissing,
}

impl ReleaseSourceUnobservedReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::RuntimeRootUnavailable => "runtime_root_unavailable",
            Self::ManifestMissing => "manifest_missing",
            Self::ManifestUnreadable => "manifest_unreadable",
            Self::ManifestEmpty => "manifest_empty",
            Self::ManifestInvalidJson => "manifest_invalid_json",
            Self::RepoHeadMissing => "repo_head_missing",
            Self::LatestPostgresMigrationMissing => "latest_postgres_migration_missing",
        }
    }
}

#[derive(Debug, Deserialize)]
struct ReleaseSourceManifest {
    repo_head: Option<String>,
    latest_postgres_migration: Option<String>,
}

pub(crate) fn observe() -> ReleaseSourceObservation {
    let Some(runtime_root) = crate::config::runtime_root() else {
        return ReleaseSourceObservation::Unobserved {
            reason: ReleaseSourceUnobservedReason::RuntimeRootUnavailable,
        };
    };
    read(runtime_root.join("runtime").join("release-source.json"))
}

fn read(path: impl AsRef<Path>) -> ReleaseSourceObservation {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ReleaseSourceObservation::Unobserved {
                reason: ReleaseSourceUnobservedReason::ManifestMissing,
            };
        }
        Err(_) => {
            return ReleaseSourceObservation::Unobserved {
                reason: ReleaseSourceUnobservedReason::ManifestUnreadable,
            };
        }
    };
    if raw.trim().is_empty() {
        return ReleaseSourceObservation::Unobserved {
            reason: ReleaseSourceUnobservedReason::ManifestEmpty,
        };
    }
    let manifest = match serde_json::from_str::<ReleaseSourceManifest>(&raw) {
        Ok(manifest) => manifest,
        Err(_) => {
            return ReleaseSourceObservation::Unobserved {
                reason: ReleaseSourceUnobservedReason::ManifestInvalidJson,
            };
        }
    };
    let Some(deployed_repo_head) = nonempty(manifest.repo_head) else {
        return ReleaseSourceObservation::Unobserved {
            reason: ReleaseSourceUnobservedReason::RepoHeadMissing,
        };
    };
    let Some(deployed_latest_postgres_migration) = nonempty(manifest.latest_postgres_migration)
    else {
        return ReleaseSourceObservation::Unobserved {
            reason: ReleaseSourceUnobservedReason::LatestPostgresMigrationMissing,
        };
    };
    ReleaseSourceObservation::Observed {
        deployed_repo_head,
        deployed_latest_postgres_migration,
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

pub(crate) fn health_json(include_node_hostname: bool) -> serde_json::Value {
    let mut health = match observe() {
        ReleaseSourceObservation::Observed {
            deployed_repo_head,
            deployed_latest_postgres_migration,
        } => serde_json::json!({
            "observation_status": "observed",
            "deployed_repo_head": deployed_repo_head,
            "deployed_latest_postgres_migration": deployed_latest_postgres_migration,
        }),
        ReleaseSourceObservation::Unobserved { reason } => serde_json::json!({
            "observation_status": "unobserved",
            "observation_failure": reason.as_str(),
        }),
    };
    if include_node_hostname {
        health["node_hostname"] = serde_json::json!(crate::services::platform::hostname_short());
    }
    health
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_unobserved(path: &Path, expected: ReleaseSourceUnobservedReason) {
        assert_eq!(
            read(path),
            ReleaseSourceObservation::Unobserved { reason: expected }
        );
    }

    #[test]
    fn release_source_reads_confirmed_identity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("manifest.json");
        std::fs::write(
            &path,
            r#"{"repo_head":"0123456789abcdef0123456789abcdef01234567","latest_postgres_migration":"0104_example.sql"}"#,
        )
        .expect("write manifest");

        assert_eq!(
            read(path),
            ReleaseSourceObservation::Observed {
                deployed_repo_head: "0123456789abcdef0123456789abcdef01234567".to_string(),
                deployed_latest_postgres_migration: "0104_example.sql".to_string(),
            }
        );
    }

    #[test]
    fn release_source_reports_missing_file_as_unobserved() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert_unobserved(
            &temp.path().join("missing.json"),
            ReleaseSourceUnobservedReason::ManifestMissing,
        );
    }

    #[test]
    fn release_source_reports_empty_file_as_unobserved() {
        let file = tempfile::NamedTempFile::new().expect("tempfile");
        assert_unobserved(file.path(), ReleaseSourceUnobservedReason::ManifestEmpty);
    }

    #[test]
    fn release_source_reports_invalid_json_as_unobserved() {
        let mut file = tempfile::NamedTempFile::new().expect("tempfile");
        std::io::Write::write_all(&mut file, b"{").expect("write invalid JSON");
        assert_unobserved(
            file.path(),
            ReleaseSourceUnobservedReason::ManifestInvalidJson,
        );
    }

    #[test]
    fn release_source_reports_each_missing_field_as_unobserved() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing_head = temp.path().join("missing-head.json");
        std::fs::write(
            &missing_head,
            r#"{"latest_postgres_migration":"0104_example.sql"}"#,
        )
        .expect("write manifest");
        assert_unobserved(
            &missing_head,
            ReleaseSourceUnobservedReason::RepoHeadMissing,
        );

        let missing_migration = temp.path().join("missing-migration.json");
        std::fs::write(
            &missing_migration,
            r#"{"repo_head":"0123456789abcdef0123456789abcdef01234567"}"#,
        )
        .expect("write manifest");
        assert_unobserved(
            &missing_migration,
            ReleaseSourceUnobservedReason::LatestPostgresMigrationMissing,
        );
    }

    #[test]
    fn release_source_health_keeps_unobserved_distinct_from_confirmed_values() {
        let temp = tempfile::tempdir().expect("tempdir");
        let observed_path = temp.path().join("observed.json");
        std::fs::write(
            &observed_path,
            r#"{"repo_head":"head","latest_postgres_migration":"migration"}"#,
        )
        .expect("write manifest");

        assert!(matches!(
            read(observed_path),
            ReleaseSourceObservation::Observed { .. }
        ));
        assert!(matches!(
            read(temp.path().join("absent.json")),
            ReleaseSourceObservation::Unobserved { .. }
        ));
    }
}
