//! Preserve the distinction between confirmed release-source facts and observation failures:
//! the repository head and latest PostgreSQL migration stay typed as observed or unobserved, and
//! missing evidence must never become a value that consumers could mistake for a confirmed fact.
//! Field independence applies only after the whole manifest parses against the expected schema.
//! A type mismatch in any recognized field, including optional `generated_at`, intentionally
//! rejects the whole manifest as `manifest_invalid_json` and `unobserved`. After parsing,
//! `generated_at` is optional metadata and does not determine the observation status.

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
    // An absent value and an empty or whitespace-only string both use `repo_head_missing`.
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
        // so a response exposes the preceding manifest temporarily only when the
        // later manifest replacement succeeds. Because `DEPLOY_OK` is set before
        // that replacement, a write failure is outside the EXIT trap's rollback and
        // can leave the preceding manifest indefinitely. This response cannot
        // distinguish either stale case from an older successful deployment.
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

// This accepts only the writer's 40-character lowercase SHA-1 form. Repositories
// using Git's SHA-256 object format produce longer IDs and are not supported here.
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
            // `observed` means only that the parsed manifest supplied values accepted
            // by this reader, not that they are proven to match repository state.
            let mut health = serde_json::json!({ "observation_status": "observed" });
            let mut failures = Vec::new();
            if let Some(value) = generated_at {
                health["generated_at"] = serde_json::json!(value);
            }
            match repo_head {
                Ok(value) => {
                    health["deployed_repo_head"] = serde_json::json!(value);
                }
                Err(reason) => {
                    failures.push(reason.as_str());
                }
            }
            match latest_postgres_migration {
                Ok(value) => {
                    health["deployed_latest_postgres_migration"] = serde_json::json!(value);
                }
                Err(reason) => {
                    failures.push(reason.as_str());
                }
            }
            if !failures.is_empty() {
                health["observation_status"] = serde_json::json!(if failures.len() == 1 {
                    "partial"
                } else {
                    "unobserved"
                });
                health["observation_failures"] = serde_json::json!(failures);
            }
            health
        }
        ReleaseSourceObservation::Unobserved { reason } => serde_json::json!({
            "observation_status": "unobserved",
            "observation_failures": [reason.as_str()],
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
    fn release_source_rejects_wrong_generated_at_type_with_other_facts_intact() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("wrong-generated-at-type.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"generated_at":123,"repo_head":"{REPO_HEAD}","latest_postgres_migration":"0104_example.sql"}}"#
            ),
        )
        .expect("write manifest");

        assert_unobserved(&path, ReleaseSourceUnobservedReason::ManifestInvalidJson);
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
    fn release_source_module_docs_define_release_fact_scope() {
        let source = include_str!("release_source.rs");
        assert!(source.starts_with(
            "//! Preserve the distinction between confirmed release-source facts and observation failures:\n\
             //! the repository head and latest PostgreSQL migration stay typed as observed or unobserved, and\n\
             //! missing evidence must never become a value that consumers could mistake for a confirmed fact.\n\
             //! Field independence applies only after the whole manifest parses against the expected schema.\n\
             //! A type mismatch in any recognized field, including optional `generated_at`, intentionally\n\
             //! rejects the whole manifest as `manifest_invalid_json` and `unobserved`. After parsing,\n\
             //! `generated_at` is optional metadata and does not determine the observation status."
        ));
    }
}
