use super::discovery::{
    RoutineDiscoveryHooks, bind_routine_root_authority, collect_routine_script_paths,
    require_nonempty_routine_tree, script_ref,
};
use super::evaluator::{
    MAX_ROUTINE_RETAINED_OUTPUT_BYTES, RetainedOutputBudget,
    load_single_routine_script_from_source_with_budget,
};
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
    validate_routine_tree_with_retained_output_limit(
        configured_root,
        configured_runtime_root,
        MAX_ROUTINE_RETAINED_OUTPUT_BYTES,
    )
}

fn validate_routine_tree_with_retained_output_limit(
    configured_root: &Path,
    configured_runtime_root: &Path,
    maximum_retained_output_bytes: usize,
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
    let mut retained_output_budget = RetainedOutputBudget::new(maximum_retained_output_bytes);
    for snapshot in snapshots {
        let candidate_output_budget = retained_output_budget.fork();
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
        let evaluation = load_single_routine_script_from_source_with_budget(
            &root.canonical,
            &path,
            source,
            &candidate_output_budget,
        );
        authority_check()?;
        if candidate_output_budget.is_exhausted() {
            return Err(candidate_output_budget.limit_error());
        }
        match evaluation {
            Ok(validated) => {
                retained_output_budget = candidate_output_budget;
                validated_files.push(RoutineValidationFile {
                    path,
                    script_ref,
                    name: validated.name,
                });
            }
            Err(error) => failures.push(RoutineValidationFailure {
                path,
                script_ref,
                message: error.to_string(),
            }),
        }
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
    fn validation_and_runtime_share_exact_multiscript_name_and_metadata_budget() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_root = temp.path().join("release-root");
        let root = runtime_root.join("routines");
        fs::create_dir_all(runtime_root.join("routine-helpers")).unwrap();
        write_routine(
            &root,
            "first.js",
            r#"agentdesk.routines.register({ name: "one", metadata: { key: "value" }, tick() { return {}; } });"#,
        );
        write_routine(
            &root,
            "second.js",
            r#"agentdesk.routines.register({ name: "two", metadata: { "é": "💣" }, tick() { return {}; } });"#,
        );

        // first: 3 + 3 + 5 bytes; second: 3 + 2 + 4 bytes.
        let report =
            validate_routine_tree_with_retained_output_limit(&root, &runtime_root, 20).unwrap();
        assert!(report.valid);
        assert_eq!(report.validated_files.len(), 2);

        let runtime_loader = super::super::RoutineScriptLoader::new().unwrap();
        assert_eq!(
            runtime_loader
                .load_dirs_with_retained_output_limit(&[root.clone()], 20)
                .unwrap(),
            2
        );

        let error =
            validate_routine_tree_with_retained_output_limit(&root, &runtime_root, 19).unwrap_err();
        assert_eq!(
            error.to_string(),
            "routine retained output exceeds maximum 19 bytes"
        );

        let constrained_runtime_loader = super::super::RoutineScriptLoader::new().unwrap();
        let runtime_error = constrained_runtime_loader
            .load_dirs_with_retained_output_limit(&[root], 19)
            .unwrap_err();
        assert_eq!(runtime_error.to_string(), error.to_string());
        assert!(constrained_runtime_loader.script_refs().unwrap().is_empty());
    }

    #[test]
    fn failed_script_retained_budget_is_rolled_back_before_next_script() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_root = temp.path().join("release-root");
        let root = runtime_root.join("routines");
        fs::create_dir_all(runtime_root.join("routine-helpers")).unwrap();
        write_routine(
            &root,
            "a-failed.js",
            r#"agentdesk.routines.register({ name: "fail", tick: 1 });"#,
        );
        write_routine(
            &root,
            "b-valid.js",
            r#"agentdesk.routines.register({ name: "ok", metadata: { k: "v" }, tick() { return {}; } });"#,
        );

        let report =
            validate_routine_tree_with_retained_output_limit(&root, &runtime_root, 4).unwrap();

        assert!(!report.valid);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].script_ref, "a-failed.js");
        assert_eq!(report.validated_files.len(), 1);
        assert_eq!(report.validated_files[0].name, "ok");
    }

    #[test]
    fn validation_report_schema_does_not_expose_metadata_or_budget_state() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_root = temp.path().join("release-root");
        let root = runtime_root.join("routines");
        fs::create_dir_all(runtime_root.join("routine-helpers")).unwrap();
        write_routine(
            &root,
            "valid.js",
            r#"agentdesk.routines.register({ name: "valid", metadata: { hidden: "payload" }, tick() { return {}; } });"#,
        );

        let report = validate_routine_tree(&root, &runtime_root).unwrap();
        let serialized = serde_json::to_value(report).unwrap();
        let object = serialized.as_object().unwrap();
        assert_eq!(object.len(), 5);
        for key in ["valid", "root", "runtimeRoot", "validatedFiles", "failures"] {
            assert!(object.contains_key(key), "missing report field {key}");
        }
        assert!(!object.contains_key("metadata"));
        assert!(!object.contains_key("retainedOutputBudget"));

        let validated_file = object["validatedFiles"][0].as_object().unwrap();
        assert_eq!(validated_file.len(), 3);
        assert!(validated_file.contains_key("path"));
        assert!(validated_file.contains_key("scriptRef"));
        assert!(validated_file.contains_key("name"));
        assert!(!validated_file.contains_key("metadata"));
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
