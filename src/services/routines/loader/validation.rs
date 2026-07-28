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
    validate_routine_tree_with_limits(
        configured_root,
        configured_runtime_root,
        MAX_ROUTINE_RETAINED_OUTPUT_BYTES,
        MAX_ROUTINE_RETAINED_OUTPUT_BYTES,
    )
}

fn validate_routine_tree_with_retained_output_limit(
    configured_root: &Path,
    configured_runtime_root: &Path,
    maximum_retained_output_bytes: usize,
) -> Result<RoutineValidationReport> {
    validate_routine_tree_with_limits(
        configured_root,
        configured_runtime_root,
        maximum_retained_output_bytes,
        MAX_ROUTINE_RETAINED_OUTPUT_BYTES,
    )
}

fn validate_routine_tree_with_limits(
    configured_root: &Path,
    configured_runtime_root: &Path,
    maximum_retained_output_bytes: usize,
    maximum_report_bytes: usize,
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

    let mut report_budget = RetainedOutputBudget::new(maximum_report_bytes);
    charge_validation_path(&report_budget, &root.canonical)?;
    charge_validation_path(&report_budget, &canonical_runtime_root)?;

    let mut failures = Vec::new();
    if let Err(error) = require_nonempty_routine_tree(&snapshots) {
        let candidate_report_budget = report_budget.fork();
        let script_ref = "<routine-root>";
        charge_validation_file_identity(&candidate_report_budget, &root.canonical, script_ref)?;
        let message = candidate_report_budget.retain_display(&error)?;
        report_budget = candidate_report_budget;
        failures.push(RoutineValidationFailure {
            path: root.canonical.clone(),
            script_ref: script_ref.to_string(),
            message,
        });
    }
    let mut validated_files = Vec::with_capacity(snapshots.len());
    let mut retained_output_budget = RetainedOutputBudget::new(maximum_retained_output_bytes);
    for snapshot in snapshots {
        let candidate_output_budget = retained_output_budget.fork();
        authority_check()?;
        let path = &snapshot.path;
        let script_ref = script_ref(&root.canonical, path);
        let candidate_report_budget = report_budget.fork();
        charge_validation_file_identity(&candidate_report_budget, path, &script_ref)?;
        let source = match snapshot.read_source() {
            Ok(source) => source,
            Err(error) => {
                let message = candidate_report_budget.retain_display(&error)?;
                report_budget = candidate_report_budget;
                failures.push(RoutineValidationFailure {
                    path: path.clone(),
                    script_ref,
                    message,
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
                candidate_report_budget.charge(validated.name.len())?;
                retained_output_budget = candidate_output_budget;
                report_budget = candidate_report_budget;
                validated_files.push(RoutineValidationFile {
                    path: path.clone(),
                    script_ref,
                    name: validated.name,
                });
            }
            Err(error) => {
                let message = candidate_report_budget.retain_display(&error)?;
                report_budget = candidate_report_budget;
                failures.push(RoutineValidationFailure {
                    path: path.clone(),
                    script_ref,
                    message,
                });
            }
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

fn charge_validation_path(budget: &RetainedOutputBudget, path: &Path) -> Result<()> {
    budget.charge(path.as_os_str().as_encoded_bytes().len())
}

fn charge_validation_file_identity(
    budget: &RetainedOutputBudget,
    path: &Path,
    script_ref: &str,
) -> Result<()> {
    charge_validation_path(budget, path)?;
    budget.charge(script_ref.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_routine(root: &Path, name: &str, source: &str) -> usize {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join(name), source).unwrap();
        source.len()
    }

    fn report_payload_bytes(report: &RoutineValidationReport) -> usize {
        let path_bytes = |path: &Path| path.as_os_str().as_encoded_bytes().len();
        path_bytes(&report.root)
            + path_bytes(&report.runtime_root)
            + report
                .validated_files
                .iter()
                .map(|file| path_bytes(&file.path) + file.script_ref.len() + file.name.len())
                .sum::<usize>()
            + report
                .failures
                .iter()
                .map(|failure| {
                    path_bytes(&failure.path) + failure.script_ref.len() + failure.message.len()
                })
                .sum::<usize>()
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
        let first_source_bytes = write_routine(
            &root,
            "first.js",
            r#"agentdesk.routines.register({ name: "one", metadata: { key: "value" }, tick() { return {}; } });"#,
        );
        let second_source_bytes = write_routine(
            &root,
            "second.js",
            r#"agentdesk.routines.register({ name: "two", metadata: { "é": "💣" }, tick() { return {}; } });"#,
        );

        // first payload: 3 + 3 + 5 bytes; second: 3 + 2 + 4 bytes.
        let exact_budget = first_source_bytes + second_source_bytes + 20;
        let report =
            validate_routine_tree_with_retained_output_limit(&root, &runtime_root, exact_budget)
                .unwrap();
        assert!(report.valid);
        assert_eq!(report.validated_files.len(), 2);

        let runtime_loader = super::super::RoutineScriptLoader::new().unwrap();
        assert_eq!(
            runtime_loader
                .load_dirs_with_retained_output_limit(&[root.clone()], exact_budget)
                .unwrap(),
            2
        );

        let error = validate_routine_tree_with_retained_output_limit(
            &root,
            &runtime_root,
            exact_budget - 1,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "routine retained output exceeds maximum {} bytes",
                exact_budget - 1
            )
        );

        let constrained_runtime_loader = super::super::RoutineScriptLoader::new().unwrap();
        let runtime_error = constrained_runtime_loader
            .load_dirs_with_retained_output_limit(&[root], exact_budget - 1)
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
        let valid_source_bytes = write_routine(
            &root,
            "b-valid.js",
            r#"agentdesk.routines.register({ name: "ok", metadata: { k: "v" }, tick() { return {}; } });"#,
        );

        let report = validate_routine_tree_with_retained_output_limit(
            &root,
            &runtime_root,
            valid_source_bytes + 4,
        )
        .unwrap();

        assert!(!report.valid);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].script_ref, "a-failed.js");
        assert_eq!(report.validated_files.len(), 1);
        assert_eq!(report.validated_files[0].name, "ok");
    }

    #[test]
    fn validation_report_budget_accepts_exact_utf8_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_root = temp.path().join("release-root");
        let root = runtime_root.join("routines");
        fs::create_dir_all(runtime_root.join("routine-helpers")).unwrap();
        write_routine(
            &root,
            "é.js",
            r#"agentdesk.routines.register({ name: "é💣", tick() { return {}; } });"#,
        );
        let unrestricted = validate_routine_tree(&root, &runtime_root).unwrap();
        let exact_report_bytes = report_payload_bytes(&unrestricted);

        let exact = validate_routine_tree_with_limits(
            &root,
            &runtime_root,
            MAX_ROUTINE_RETAINED_OUTPUT_BYTES,
            exact_report_bytes,
        )
        .unwrap();
        assert!(exact.valid);
        assert_eq!(exact.validated_files[0].name, "é💣");

        let error = validate_routine_tree_with_limits(
            &root,
            &runtime_root,
            MAX_ROUTINE_RETAINED_OUTPUT_BYTES,
            exact_report_bytes - 1,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "routine retained output exceeds maximum {} bytes",
                exact_report_bytes - 1
            )
        );
    }

    #[test]
    fn validation_report_budget_aggregates_multiple_failure_messages() {
        let temp = tempfile::tempdir().unwrap();
        let runtime_root = temp.path().join("release-root");
        let root = runtime_root.join("routines");
        fs::create_dir_all(runtime_root.join("routine-helpers")).unwrap();
        for name in ["first.js", "second.js"] {
            write_routine(
                &root,
                name,
                r#"agentdesk.routines.register({ name: "fail", tick: 1 });"#,
            );
        }
        let unrestricted = validate_routine_tree(&root, &runtime_root).unwrap();
        assert_eq!(unrestricted.failures.len(), 2);
        let exact_report_bytes = report_payload_bytes(&unrestricted);

        let exact = validate_routine_tree_with_limits(
            &root,
            &runtime_root,
            MAX_ROUTINE_RETAINED_OUTPUT_BYTES,
            exact_report_bytes,
        )
        .unwrap();
        assert_eq!(exact.failures.len(), 2);

        let error = validate_routine_tree_with_limits(
            &root,
            &runtime_root,
            MAX_ROUTINE_RETAINED_OUTPUT_BYTES,
            exact_report_bytes - 1,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "routine retained output exceeds maximum {} bytes",
                exact_report_bytes - 1
            )
        );
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
