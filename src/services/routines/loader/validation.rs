use super::discovery::{
    RoutineDiscoveryHooks, bind_routine_root_authority, collect_routine_script_paths,
    require_nonempty_routine_tree, script_ref,
};
use super::evaluator::validate_routine_script_source;
use super::{verify_bound_root_set, verify_bound_runtime_surface, verify_bound_scan_surface};
use anyhow::{Result, anyhow};
use serde::Serialize;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RoutineValidationFile {
    pub(crate) path: PathBuf,
    pub(crate) script_ref: String,
    pub(crate) name: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RoutineValidationFailure {
    pub(crate) path: PathBuf,
    pub(crate) script_ref: String,
    pub(crate) message: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RoutineValidationReport {
    pub(crate) valid: bool,
    pub(crate) root: PathBuf,
    pub(crate) runtime_root: PathBuf,
    pub(crate) validated_files: Vec<RoutineValidationFile>,
    pub(crate) failures: Vec<RoutineValidationFailure>,
}

pub(crate) fn validate_routine_tree(
    configured_root: &Path,
    configured_runtime_root: &Path,
) -> Result<RoutineValidationReport> {
    let configured_roots = [configured_root.to_path_buf()];
    let (runtime_authority, roots, helper_authority) =
        bind_routine_root_authority(&configured_roots, configured_runtime_root)?;
    let root = roots
        .first()
        .ok_or_else(|| anyhow!("routine validator did not bind the requested root"))?;
    if !root.exists {
        return Err(anyhow!(
            "routine validation root `{}` does not exist",
            configured_root.display()
        ));
    }

    let canonical_runtime_root = runtime_authority.canonical().to_path_buf();
    verify_bound_runtime_surface(
        &runtime_authority,
        &helper_authority,
        &canonical_runtime_root,
        None,
    )?;
    verify_bound_root_set(
        &roots,
        &helper_authority,
        &configured_roots,
        &canonical_runtime_root,
        None,
    )?;

    let authority_check = || {
        verify_bound_scan_surface(
            Some(&runtime_authority),
            &roots,
            &helper_authority,
            &configured_roots,
            &canonical_runtime_root,
            None,
        )
        .map_err(|error| io::Error::other(error.to_string()))
    };
    let snapshots = collect_routine_script_paths(
        root,
        RoutineDiscoveryHooks {
            before_open: None,
            before_read: None,
            read_observer: None,
            authority_check: Some(&authority_check),
        },
    )?;

    let mut failures = Vec::new();
    if let Err(error) = require_nonempty_routine_tree(&snapshots) {
        failures.push(RoutineValidationFailure {
            path: root.canonical.clone(),
            script_ref: "<routine-root>".to_string(),
            message: error.to_string(),
        });
    }
    let mut validated_files = Vec::with_capacity(snapshots.len());
    for snapshot in snapshots {
        authority_check()?;
        let path = snapshot.path.clone();
        let script_ref = script_ref(&root.canonical, &path);
        let source = match snapshot.read_source() {
            Ok(source) => source,
            Err(error) => {
                failures.push(RoutineValidationFailure {
                    path,
                    script_ref,
                    message: error.to_string(),
                });
                continue;
            }
        };
        let fallback_name = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        match validate_routine_script_source(&source, &fallback_name, &script_ref, &path) {
            Ok(validated) => validated_files.push(RoutineValidationFile {
                path,
                script_ref,
                name: validated.name,
            }),
            Err(error) => failures.push(RoutineValidationFailure {
                path,
                script_ref,
                message: error.to_string(),
            }),
        }
        authority_check()?;
    }

    verify_bound_root_set(
        &roots,
        &helper_authority,
        &configured_roots,
        &canonical_runtime_root,
        None,
    )?;
    Ok(RoutineValidationReport {
        valid: failures.is_empty(),
        root: root.canonical.clone(),
        runtime_root: canonical_runtime_root,
        validated_files,
        failures,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_routine(root: &Path, name: &str, source: &str) {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join(name), source).unwrap();
    }

    #[test]
    fn validates_every_runtime_equivalent_source() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_root = temp.path().join("release-root");
        let root = runtime_root.join("routines");
        fs::create_dir_all(runtime_root.join("routine-helpers")).unwrap();
        write_routine(
            &root,
            "valid.js",
            "agentdesk.routines.register({ name: 'valid', tick() { return {}; } });",
        );
        write_routine(
            &root,
            "invalid.js",
            "agentdesk.routines.register = () => {};",
        );

        let report = validate_routine_tree(&root, &runtime_root).unwrap();

        assert!(!report.valid);
        assert_eq!(report.validated_files.len(), 1);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].script_ref, "invalid.js");
    }

    #[test]
    fn rejects_empty_routine_tree() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_root = temp.path().join("release-root");
        let root = runtime_root.join("routines");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(runtime_root.join("routine-helpers")).unwrap();

        let report = validate_routine_tree(&root, &runtime_root).unwrap();

        assert!(!report.valid);
        assert!(report.validated_files.is_empty());
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, root.canonicalize().unwrap());
        assert_eq!(report.failures[0].script_ref, "<routine-root>");
        assert_eq!(
            report.failures[0].message,
            "routine root contains no JavaScript entrypoints"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_hard_linked_routine_source() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_root = temp.path().join("release-root");
        let root = runtime_root.join("routines");
        let helpers = runtime_root.join("routine-helpers");
        fs::create_dir_all(&helpers).unwrap();
        write_routine(
            &root,
            "linked.js",
            "agentdesk.routines.register({ tick() { return {}; } });",
        );
        fs::hard_link(root.join("linked.js"), helpers.join("linked.js")).unwrap();

        let error = validate_routine_tree(&root, &runtime_root).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("aliases the protected routine helper surface")
        );
    }

    #[test]
    fn rejects_source_larger_than_the_runtime_cap() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_root = temp.path().join("release-root");
        let root = runtime_root.join("routines");
        fs::create_dir_all(runtime_root.join("routine-helpers")).unwrap();
        write_routine(
            &root,
            "large.js",
            &" ".repeat((super::super::discovery::MAX_ROUTINE_SOURCE_BYTES + 1) as usize),
        );

        let report = validate_routine_tree(&root, &runtime_root).unwrap();

        assert!(!report.valid);
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].message.contains("exceeds"));
    }
}
