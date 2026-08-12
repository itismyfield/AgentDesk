//! Preserve the distinction between confirmed release-source facts and observation failures:
//! each fact stays typed as observed or unobserved, and missing evidence must never be simplified
//! into a string sentinel that consumers could mistake for a confirmed value.

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReleaseSourceObservation {
    Manifest {
        generated_at: Option<String>,
        repo_head: Result<String, ReleaseSourceUnobservedReason>,
        latest_postgres_migration: Result<String, ReleaseSourceUnobservedReason>,
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
    RepoHeadInvalid,
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
            Self::RepoHeadInvalid => "repo_head_invalid",
            Self::LatestPostgresMigrationMissing => "latest_postgres_migration_missing",
        }
    }
}

#[derive(Debug, Deserialize)]
struct ReleaseSourceManifest {
    generated_at: Option<String>,
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

    let generated_at = nonempty(manifest.generated_at);
    let repo_head = match nonempty(manifest.repo_head) {
        Some(value) if is_git_object_id(&value) => Ok(value),
        Some(_) => Err(ReleaseSourceUnobservedReason::RepoHeadInvalid),
        None => Err(ReleaseSourceUnobservedReason::RepoHeadMissing),
    };
    // `_write_release_source_manifest` currently emits a basename selected by its
    // migration glob, but this reader cannot establish filesystem provenance. Keep
    // the filename opaque beyond non-empty trimming so future valid naming schemes
    // are not rejected while still refusing to turn absence into a sentinel value.
    let latest_postgres_migration = match nonempty(manifest.latest_postgres_migration) {
        Some(value) => Ok(value),
        None => Err(ReleaseSourceUnobservedReason::LatestPostgresMigrationMissing),
    };

    ReleaseSourceObservation::Manifest {
        // This is the manifest writer's timestamp, not proof that the manifest
        // describes the currently executing binary. `deploy-release.sh` starts and
        // health-checks the promoted binary before `_write_release_source_manifest`,
        // so a health response can temporarily expose the preceding manifest even
        // when every included fact is well formed. Consumers must judge freshness
        // from this timestamp when that distinction matters.
        generated_at,
        repo_head,
        latest_postgres_migration,
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn is_git_object_id(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn health_json(include_node_hostname: bool) -> serde_json::Value {
    let mut health = match observe() {
        ReleaseSourceObservation::Manifest {
            generated_at,
            repo_head,
            latest_postgres_migration,
        } => {
            let mut health = serde_json::json!({ "observation_status": "observed" });
            let mut failure = None;
            if let Some(value) = generated_at {
                health["generated_at"] = serde_json::json!(value);
            }
            match repo_head {
                Ok(value) => {
                    health["deployed_repo_head"] = serde_json::json!(value);
                }
                Err(reason) => {
                    failure.get_or_insert(reason);
                }
            }
            match latest_postgres_migration {
                Ok(value) => {
                    health["deployed_latest_postgres_migration"] = serde_json::json!(value);
                }
                Err(reason) => {
                    failure.get_or_insert(reason);
                }
            }
            if let Some(reason) = failure {
                health["observation_status"] = serde_json::json!("unobserved");
                health["observation_failure"] = serde_json::json!(reason.as_str());
            }
            health
        }
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

    const REPO_HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

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
            format!(
                r#"{{"generated_at":"2026-08-12T00:00:00Z","repo_head":"{REPO_HEAD}","latest_postgres_migration":"0104_example.sql"}}"#
            ),
        )
        .expect("write manifest");

        assert_eq!(
            read(path),
            ReleaseSourceObservation::Manifest {
                generated_at: Some("2026-08-12T00:00:00Z".to_string()),
                repo_head: Ok(REPO_HEAD.to_string()),
                latest_postgres_migration: Ok("0104_example.sql".to_string()),
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
    fn release_source_reports_each_missing_field_without_discarding_the_other() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing_head = temp.path().join("missing-head.json");
        std::fs::write(
            &missing_head,
            r#"{"generated_at":"2026-08-12T00:00:00Z","latest_postgres_migration":"0104_example.sql"}"#,
        )
        .expect("write manifest");
        let ReleaseSourceObservation::Manifest {
            repo_head,
            latest_postgres_migration,
            ..
        } = read(&missing_head)
        else {
            panic!("valid manifest must retain field-level observations");
        };
        assert_eq!(
            repo_head,
            Err(ReleaseSourceUnobservedReason::RepoHeadMissing)
        );
        assert_eq!(latest_postgres_migration.as_deref(), Ok("0104_example.sql"));

        let missing_migration = temp.path().join("missing-migration.json");
        std::fs::write(
            &missing_migration,
            format!(r#"{{"generated_at":"2026-08-12T00:00:00Z","repo_head":"{REPO_HEAD}"}}"#),
        )
        .expect("write manifest");
        let ReleaseSourceObservation::Manifest {
            repo_head,
            latest_postgres_migration,
            ..
        } = read(&missing_migration)
        else {
            panic!("valid manifest must retain field-level observations");
        };
        assert_eq!(repo_head.as_deref(), Ok(REPO_HEAD));
        assert_eq!(
            latest_postgres_migration,
            Err(ReleaseSourceUnobservedReason::LatestPostgresMigrationMissing)
        );
    }

    #[test]
    fn release_source_rejects_non_sha_repo_heads_without_discarding_migration() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("invalid-head.json");
        std::fs::write(
            &path,
            r#"{"repo_head":"unknown","latest_postgres_migration":"0104_example.sql"}"#,
        )
        .expect("write manifest");
        let ReleaseSourceObservation::Manifest {
            repo_head,
            latest_postgres_migration,
            ..
        } = read(path)
        else {
            panic!("valid manifest must retain field-level observations");
        };
        assert_eq!(
            repo_head,
            Err(ReleaseSourceUnobservedReason::RepoHeadInvalid)
        );
        assert_eq!(latest_postgres_migration.as_deref(), Ok("0104_example.sql"));
        assert!(!is_git_object_id("0123456789abcdef0123456789abcdef0123456"));
        assert!(!is_git_object_id(
            "0123456789ABCDEF0123456789ABCDEF01234567"
        ));
    }

    #[test]
    fn release_source_read_distinguishes_observed_from_unobserved() {
        let temp = tempfile::tempdir().expect("tempdir");
        let observed_path = temp.path().join("observed.json");
        std::fs::write(
            &observed_path,
            format!(
                r#"{{"generated_at":"2026-08-12T00:00:00Z","repo_head":"{REPO_HEAD}","latest_postgres_migration":"0104_example.sql"}}"#
            ),
        )
        .expect("write manifest");

        assert!(matches!(
            read(observed_path),
            ReleaseSourceObservation::Manifest {
                repo_head: Ok(_),
                latest_postgres_migration: Ok(_),
                ..
            }
        ));
        assert!(matches!(
            read(temp.path().join("absent.json")),
            ReleaseSourceObservation::Unobserved { .. }
        ));
    }

    #[test]
    fn release_source_module_docs_forbid_string_sentinels() {
        let source = include_str!("release_source.rs");
        assert!(source.starts_with("//! Preserve the distinction"));
        assert!(source.contains("string sentinel"));
    }
}
