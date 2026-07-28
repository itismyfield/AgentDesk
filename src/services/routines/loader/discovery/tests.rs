use super::super::{
    RoutineScriptFailure, RoutineScriptLoader, full_source_version, test_runtime_root,
};
use super::{
    PathResolutionError, RoutineRootValidationError, bind_routine_root_authority,
    candidate_failure_key, routine_roots_identity,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PREFLIGHT_EVALUATION_SENTINEL: usize = 7;

fn isolated_release_surfaces() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let release = tempfile::tempdir().unwrap();
    let routines = release.path().join("routines");
    let helpers = release.path().join("routine-helpers");
    std::fs::create_dir_all(&routines).unwrap();
    std::fs::create_dir_all(&helpers).unwrap();
    std::fs::write(
        routines.join("tracked.js"),
        "agentdesk.routines.register({ name: 'Tracked', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();
    std::fs::write(helpers.join("helper.js"), "module.exports = {};").unwrap();
    (release, routines, helpers)
}

#[cfg(unix)]
fn collect_with_limits(
    runtime_root: &Path,
    routines: &Path,
    limits: super::RoutineTreeLimits,
) -> std::io::Result<Vec<super::DiscoveredRoutineScript>> {
    let (_, roots, _) =
        bind_routine_root_authority(&[routines.to_path_buf()], runtime_root).unwrap();
    super::collect_routine_script_paths_with_limits(
        &roots[0],
        super::RoutineDiscoveryHooks::default(),
        limits,
    )
}

#[cfg(unix)]
fn test_limits(
    max_entries: usize,
    max_files: usize,
    max_depth: usize,
    max_source_bytes: u64,
) -> super::RoutineTreeLimits {
    super::RoutineTreeLimits {
        max_entries,
        max_files,
        max_depth,
        max_source_bytes,
    }
}

#[cfg(unix)]
#[test]
fn routine_tree_limits_accept_exact_boundaries() {
    let release = tempfile::tempdir().unwrap();
    let routines = release.path().join("routines");
    let nested = routines.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir_all(release.path().join("routine-helpers")).unwrap();
    std::fs::write(nested.join("a.js"), "aa").unwrap();
    std::fs::write(nested.join("b.js"), "bb").unwrap();

    let snapshots =
        collect_with_limits(release.path(), &routines, test_limits(3, 2, 1, 4)).unwrap();

    assert_eq!(snapshots.len(), 2);
}

#[cfg(unix)]
#[test]
fn routine_tree_entry_limit_rejects_before_unbounded_retention() {
    let release = tempfile::tempdir().unwrap();
    let routines = release.path().join("routines");
    std::fs::create_dir_all(&routines).unwrap();
    std::fs::create_dir_all(release.path().join("routine-helpers")).unwrap();
    std::fs::write(routines.join("a.js"), "a").unwrap();
    std::fs::write(routines.join("b.js"), "b").unwrap();

    let error =
        collect_with_limits(release.path(), &routines, test_limits(1, 2, 1, 2)).unwrap_err();

    assert!(error.to_string().contains("maximum entry count 1"));
}

#[cfg(unix)]
#[test]
fn routine_tree_file_limit_rejects_overflow() {
    let release = tempfile::tempdir().unwrap();
    let routines = release.path().join("routines");
    std::fs::create_dir_all(&routines).unwrap();
    std::fs::create_dir_all(release.path().join("routine-helpers")).unwrap();
    std::fs::write(routines.join("a.js"), "a").unwrap();
    std::fs::write(routines.join("b.js"), "b").unwrap();

    let error =
        collect_with_limits(release.path(), &routines, test_limits(2, 1, 1, 2)).unwrap_err();

    assert!(error.to_string().contains("maximum file count 1"));
}

#[cfg(unix)]
#[test]
fn routine_tree_depth_limit_rejects_overflow() {
    let release = tempfile::tempdir().unwrap();
    let routines = release.path().join("routines");
    let too_deep = routines.join("a").join("b");
    std::fs::create_dir_all(&too_deep).unwrap();
    std::fs::create_dir_all(release.path().join("routine-helpers")).unwrap();
    std::fs::write(too_deep.join("deep.js"), "x").unwrap();

    let error =
        collect_with_limits(release.path(), &routines, test_limits(3, 1, 1, 1)).unwrap_err();

    assert!(error.to_string().contains("maximum depth 1"));
}

#[cfg(unix)]
#[test]
fn routine_tree_total_source_limit_rejects_overflow() {
    let release = tempfile::tempdir().unwrap();
    let routines = release.path().join("routines");
    std::fs::create_dir_all(&routines).unwrap();
    std::fs::create_dir_all(release.path().join("routine-helpers")).unwrap();
    std::fs::write(routines.join("a.js"), "aa").unwrap();
    std::fs::write(routines.join("b.js"), "bb").unwrap();

    let error =
        collect_with_limits(release.path(), &routines, test_limits(2, 2, 1, 3)).unwrap_err();

    assert!(error.to_string().contains("source bytes"));
    assert!(error.to_string().contains("maximum 3"));
}

#[test]
fn shared_nonempty_contract_rejects_empty_snapshot() {
    let error = super::require_nonempty_routine_tree(&[]).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
        error.to_string(),
        "routine root contains no JavaScript entrypoints"
    );
}

#[cfg(unix)]
#[test]
fn shared_state_identity_includes_each_mount_authority() {
    let (release, routines, _helpers) = isolated_release_surfaces();
    let (runtime, roots, helper) =
        bind_routine_root_authority(&[routines], release.path()).unwrap();
    let baseline = routine_roots_identity(&runtime, &roots, &helper);
    let changed = |mount_id: Option<u64>| Some(mount_id.unwrap_or(0).wrapping_add(1));

    let mut changed_runtime = runtime.clone();
    changed_runtime.mount_id = changed(changed_runtime.mount_id);
    assert_ne!(
        baseline,
        routine_roots_identity(&changed_runtime, &roots, &helper)
    );

    let mut changed_roots = roots.clone();
    changed_roots[0].mount_id = changed(changed_roots[0].mount_id);
    assert_ne!(
        baseline,
        routine_roots_identity(&runtime, &changed_roots, &helper)
    );

    let mut changed_helper = helper.clone();
    changed_helper.mount_id = changed(changed_helper.mount_id);
    assert_ne!(
        baseline,
        routine_roots_identity(&runtime, &roots, &changed_helper)
    );
}

#[cfg(unix)]
#[test]
fn regular_file_mount_transition_is_rejected() {
    let error = super::verify_mount_authority(Some(12), Some(11), Path::new("routine.js"), "file")
        .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(error.to_string().contains("crosses mount authority"));
}

#[cfg(target_os = "linux")]
#[test]
fn parses_proc_fdinfo_mount_authority_fallback() {
    assert_eq!(
        super::parse_proc_fdinfo_mount_id("pos:\t0\nflags:\t0100000\nmnt_id:\t42\n").unwrap(),
        42
    );
    assert!(super::parse_proc_fdinfo_mount_id("pos:\t0\n").is_err());
}

#[cfg(unix)]
#[test]
fn non_utf8_js_name_fails_injective_namespace_validation() {
    use std::os::unix::ffi::OsStringExt as _;

    let invalid_name = std::ffi::OsString::from_vec(vec![0xff, b'.', b'j', b's']);
    let error = super::validate_routine_namespace_name(
        &invalid_name,
        super::PinnedEntryKind::RegularFile,
        Path::new("routines"),
    )
    .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("non-UTF-8 name"));
}

#[cfg(unix)]
#[test]
fn unix_backslash_and_directory_separator_have_distinct_script_refs() {
    let release = tempfile::tempdir().unwrap();
    let routines = release.path().join("routines");
    std::fs::create_dir_all(routines.join("a")).unwrap();
    std::fs::create_dir_all(release.path().join("routine-helpers")).unwrap();
    std::fs::write(
        routines.join("a").join("b.js"),
        "agentdesk.routines.register({ name: 'Nested', tick() { return {}; } });",
    )
    .unwrap();
    std::fs::write(
        routines.join("a\\b.js"),
        "agentdesk.routines.register({ name: 'Backslash', tick() { return {}; } });",
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());

    assert_eq!(loader.load_dirs(&[routines]).unwrap(), 2);
    assert_eq!(
        loader.script_refs().unwrap(),
        vec!["a/b.js".to_string(), "a\\b.js".to_string()]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn discovery_rejects_non_utf8_routine_names_before_script_ref_indexing() {
    use std::os::unix::ffi::OsStringExt as _;

    let release = tempfile::tempdir().unwrap();
    let routines = release.path().join("routines");
    std::fs::create_dir_all(&routines).unwrap();
    std::fs::create_dir_all(release.path().join("routine-helpers")).unwrap();
    let invalid_name = std::ffi::OsString::from_vec(vec![0xff, b'.', b'j', b's']);
    std::fs::write(
        routines.join(invalid_name),
        "agentdesk.routines.register({ tick() { return {}; } });",
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
    let error = loader.load_dirs(&[routines]).unwrap_err();

    assert!(error.to_string().contains("non-UTF-8 name"));
    assert!(loader.script_refs().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn rejects_routine_file_aliasing_protected_helper_file() {
    let (release, routines, helpers) = isolated_release_surfaces();
    std::fs::hard_link(helpers.join("helper.js"), routines.join("helper-alias.js")).unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
    let loaded = loader.load_dirs(&[routines]).unwrap();

    assert_eq!(loaded, 0);
    assert!(!loader.has_script("helper-alias.js").unwrap());
}

fn preflight_observed_loader() -> (
    RoutineScriptLoader,
    Arc<std::sync::atomic::AtomicUsize>,
    PathBuf,
) {
    let loader = RoutineScriptLoader::new().unwrap();
    loader.state.evaluation_attempts.store(
        PREFLIGHT_EVALUATION_SENTINEL,
        std::sync::atomic::Ordering::Relaxed,
    );
    let failure_sentinel = PathBuf::from("preflight-existing-failure.js");
    loader.state.failed_scripts.lock().unwrap().insert(
        failure_sentinel.clone(),
        RoutineScriptFailure {
            source_version: Some("preflight-sentinel".to_string()),
            consecutive_failures: 1,
            retry_at: Instant::now(),
            warning_emitted: true,
        },
    );
    let source_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_source_reads = Arc::clone(&source_reads);
    *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |_| {
        observed_source_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }));
    (loader, source_reads, failure_sentinel)
}

fn assert_preflight_rejection_has_no_load_side_effects(
    loader: &RoutineScriptLoader,
    source_reads: &std::sync::atomic::AtomicUsize,
    failure_sentinel: &Path,
) {
    assert_eq!(
        loader
            .state
            .evaluation_attempts
            .load(std::sync::atomic::Ordering::Relaxed),
        PREFLIGHT_EVALUATION_SENTINEL
    );
    assert_eq!(source_reads.load(std::sync::atomic::Ordering::Relaxed), 0);
    let failed_scripts = loader.state.failed_scripts.lock().unwrap();
    assert_eq!(failed_scripts.len(), 1);
    assert!(failed_scripts.contains_key(failure_sentinel));
    assert!(loader.script_refs().unwrap().is_empty());
}

#[test]
fn preflight_rejects_sibling_helper_as_additional_root_without_side_effects() {
    let (release, routines, helpers) = isolated_release_surfaces();

    let (loader, source_reads, failure_sentinel) = preflight_observed_loader();
    *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
    let error = loader.load_dirs(&[routines, helpers]).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<RoutineRootValidationError>(),
        Some(RoutineRootValidationError::HelperSurfaceOverlap { root_index: 1, .. })
    ));
    assert!(
        error
            .to_string()
            .contains("overlaps reserved runtime helper surface")
    );
    assert_preflight_rejection_has_no_load_side_effects(
        &loader,
        source_reads.as_ref(),
        &failure_sentinel,
    );
}

#[test]
fn preflight_rejects_dot_root_that_contains_runtime_helper_surface() {
    let (release, _routines, _helpers) = isolated_release_surfaces();
    let (loader, source_reads, failure_sentinel) = preflight_observed_loader();
    *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
    *loader.current_dir_override.lock().unwrap() = Some(release.path().to_path_buf());

    let error = loader.load_dirs(&[PathBuf::from(".")]).unwrap_err();

    assert!(matches!(
        error.downcast_ref::<RoutineRootValidationError>(),
        Some(RoutineRootValidationError::HelperSurfaceOverlap { root_index: 0, .. })
    ));
    assert_preflight_rejection_has_no_load_side_effects(
        &loader,
        source_reads.as_ref(),
        &failure_sentinel,
    );
}

#[test]
fn preflight_rejects_runtime_helper_with_custom_primary_root() {
    let (release, _routines, helpers) = isolated_release_surfaces();
    let custom = tempfile::tempdir().unwrap();
    std::fs::write(
        custom.path().join("custom.js"),
        "agentdesk.routines.register({ name: 'Custom', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();
    let (loader, source_reads, failure_sentinel) = preflight_observed_loader();
    *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());

    let error = loader
        .load_dirs(&[custom.path().to_path_buf(), helpers])
        .unwrap_err();

    assert!(matches!(
        error.downcast_ref::<RoutineRootValidationError>(),
        Some(RoutineRootValidationError::HelperSurfaceOverlap { root_index: 1, .. })
    ));
    assert_preflight_rejection_has_no_load_side_effects(
        &loader,
        source_reads.as_ref(),
        &failure_sentinel,
    );
}

#[test]
fn disjoint_custom_sibling_and_cwd_helper_named_roots_are_allowed() {
    let release = tempfile::tempdir().unwrap();
    let custom = tempfile::tempdir().unwrap();
    let routines = custom.path().join("routines");
    let custom_helpers = custom.path().join("routine-helpers");
    std::fs::create_dir_all(&routines).unwrap();
    std::fs::create_dir_all(&custom_helpers).unwrap();
    std::fs::write(
        routines.join("primary.js"),
        "agentdesk.routines.register({ name: 'Primary', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();
    std::fs::write(
        custom_helpers.join("operator.js"),
        "agentdesk.routines.register({ name: 'Operator', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
    *loader.current_dir_override.lock().unwrap() = Some(custom.path().to_path_buf());

    assert_eq!(
        loader
            .load_dirs(&[PathBuf::from("routines"), PathBuf::from("routine-helpers"),])
            .unwrap(),
        2
    );
    assert_eq!(
        loader.script_refs().unwrap(),
        vec!["operator.js", "primary.js"]
    );
}

#[test]
fn preflight_rejects_release_parent_as_additional_root_without_side_effects() {
    let (release, routines, _helpers) = isolated_release_surfaces();

    let (loader, source_reads, failure_sentinel) = preflight_observed_loader();
    *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
    let error = loader
        .load_dirs(&[routines, release.path().to_path_buf()])
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<RoutineRootValidationError>(),
        Some(RoutineRootValidationError::HelperSurfaceOverlap { root_index: 1, .. })
    ));
    assert_preflight_rejection_has_no_load_side_effects(
        &loader,
        source_reads.as_ref(),
        &failure_sentinel,
    );
}

#[test]
fn preflight_rejects_root_below_sibling_helper_without_side_effects() {
    let (release, routines, helpers) = isolated_release_surfaces();
    let nested_helper_root = helpers.join("nested");
    std::fs::create_dir_all(&nested_helper_root).unwrap();

    let (loader, source_reads, failure_sentinel) = preflight_observed_loader();
    *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
    let error = loader
        .load_dirs(&[routines, nested_helper_root])
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<RoutineRootValidationError>(),
        Some(RoutineRootValidationError::HelperSurfaceOverlap { root_index: 1, .. })
    ));
    assert_preflight_rejection_has_no_load_side_effects(
        &loader,
        source_reads.as_ref(),
        &failure_sentinel,
    );
}

#[test]
fn preflight_rejects_primary_and_child_roots_without_side_effects() {
    let release = tempfile::tempdir().unwrap();
    let routines = release.path().join("routines");
    let nested = routines.join("monitoring");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        nested.join("tracked.js"),
        "agentdesk.routines.register({ name: 'Tracked', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();

    let (loader, source_reads, failure_sentinel) = preflight_observed_loader();
    let error = loader.load_dirs(&[routines, nested]).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<RoutineRootValidationError>(),
        Some(RoutineRootValidationError::CanonicalRootOverlap {
            first_index: 0,
            second_index: 1,
            ..
        })
    ));
    assert!(error.to_string().contains("overlap after canonicalization"));
    assert_preflight_rejection_has_no_load_side_effects(
        &loader,
        source_reads.as_ref(),
        &failure_sentinel,
    );
}

#[test]
fn preflight_rejects_same_canonical_root_without_side_effects() {
    let release = tempfile::tempdir().unwrap();
    let routines = release.path().join("routines");
    let nested = routines.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let lexical_alias = nested.join("..");

    let (loader, source_reads, failure_sentinel) = preflight_observed_loader();
    let error = loader.load_dirs(&[routines, lexical_alias]).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<RoutineRootValidationError>(),
        Some(RoutineRootValidationError::DuplicateCanonicalRoot {
            first_index: 0,
            second_index: 1,
            ..
        })
    ));
    assert!(error.to_string().contains("same canonical directory"));
    assert_preflight_rejection_has_no_load_side_effects(
        &loader,
        source_reads.as_ref(),
        &failure_sentinel,
    );
}

#[cfg(unix)]
#[test]
fn validated_root_alias_retarget_cannot_redirect_discovery_to_helpers() {
    use std::os::unix::fs::symlink;

    let (release, routines, helpers) = isolated_release_surfaces();
    let aliases = tempfile::tempdir().unwrap();
    let root_alias = aliases.path().join("routines");
    symlink(&routines, &root_alias).unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
    let alias_to_replace = root_alias.clone();
    let helper_target = helpers.clone();
    *loader.before_scan_hook.lock().unwrap() = Some(Arc::new(move || {
        std::fs::remove_file(&alias_to_replace).unwrap();
        symlink(&helper_target, &alias_to_replace).unwrap();
    }));
    let source_reads = Arc::new(Mutex::new(Vec::new()));
    let observed_source_reads = Arc::clone(&source_reads);
    *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |path| {
        observed_source_reads
            .lock()
            .unwrap()
            .push(path.to_path_buf());
    }));

    assert_eq!(loader.load_dirs(&[root_alias]).unwrap(), 1);
    assert_eq!(loader.script_refs().unwrap(), vec!["tracked.js"]);
    let expected_source = routines.canonicalize().unwrap().join("tracked.js");
    assert_eq!(source_reads.lock().unwrap().as_slice(), &[expected_source]);
    assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn validated_canonical_root_replacement_cannot_redirect_discovery_to_helpers() {
    use std::os::unix::fs::symlink;

    let (release, routines, helpers) = isolated_release_surfaces();
    let original_routines = release.path().join("routines-original");
    let loader = RoutineScriptLoader::new().unwrap();
    *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
    let routines_to_replace = routines.clone();
    let original_target = original_routines.clone();
    let helper_target = helpers.clone();
    *loader.before_scan_hook.lock().unwrap() = Some(Arc::new(move || {
        std::fs::rename(&routines_to_replace, &original_target).unwrap();
        symlink(&helper_target, &routines_to_replace).unwrap();
    }));
    let source_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_reads = Arc::clone(&source_reads);
    *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |_| {
        observed_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }));

    assert_eq!(loader.load_dirs(&[routines]).unwrap(), 0);
    assert_eq!(source_reads.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(
        loader
            .state
            .evaluation_attempts
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn runtime_root_alias_retarget_cannot_change_bound_routine_authority() {
    use std::os::unix::fs::symlink;

    let layout = tempfile::tempdir().unwrap();
    let first_runtime = layout.path().join("runtime-one");
    let second_runtime = layout.path().join("runtime-two");
    let first_routines = first_runtime.join("routines");
    let second_helpers = second_runtime.join("routine-helpers");
    std::fs::create_dir_all(&first_routines).unwrap();
    std::fs::create_dir_all(&second_helpers).unwrap();
    std::fs::write(
        first_routines.join("tracked.js"),
        "agentdesk.routines.register({ name: 'Tracked', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();
    std::fs::write(
        second_helpers.join("helper.js"),
        "throw new Error('retargeted runtime helper must never be read');",
    )
    .unwrap();
    symlink(&second_helpers, second_runtime.join("routines")).unwrap();
    let runtime_alias = layout.path().join("current-runtime");
    symlink(&first_runtime, &runtime_alias).unwrap();
    let roots = vec![runtime_alias.join("routines")];
    let loader = RoutineScriptLoader::new_shared(&roots, &runtime_alias).unwrap();

    std::fs::remove_file(&runtime_alias).unwrap();
    symlink(&second_runtime, &runtime_alias).unwrap();
    let source_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_reads = Arc::clone(&source_reads);
    *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |_| {
        observed_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }));

    let error = loader.load_dirs(&roots).unwrap_err();

    assert!(matches!(
        error.downcast_ref::<RoutineRootValidationError>(),
        Some(RoutineRootValidationError::RuntimeRootAuthorityChanged { .. })
    ));
    assert_eq!(source_reads.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn runtime_root_alias_retarget_rejects_new_actual_helper_as_external_root() {
    use std::os::unix::fs::symlink;

    let layout = tempfile::tempdir().unwrap();
    let first_runtime = layout.path().join("runtime-one");
    let second_runtime = layout.path().join("runtime-two");
    let first_helpers = first_runtime.join("routine-helpers");
    let second_helpers = second_runtime.join("routine-helpers");
    std::fs::create_dir_all(&first_helpers).unwrap();
    std::fs::create_dir_all(&second_helpers).unwrap();
    std::fs::write(
            second_helpers.join("helper.js"),
            "agentdesk.routines.register({ name: 'External Before Retarget', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
    let runtime_alias = layout.path().join("current-runtime");
    symlink(&first_runtime, &runtime_alias).unwrap();
    let roots = vec![second_helpers];
    let loader = RoutineScriptLoader::new_shared(&roots, &runtime_alias).unwrap();
    assert_eq!(loader.load_dirs(&roots).unwrap(), 1);

    std::fs::remove_file(&runtime_alias).unwrap();
    symlink(&second_runtime, &runtime_alias).unwrap();
    let source_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_reads = Arc::clone(&source_reads);
    *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |_| {
        observed_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }));

    let lookup_error = loader.get_script("helper.js").unwrap_err();
    let error = loader.load_dirs(&roots).unwrap_err();

    assert!(matches!(
        lookup_error.downcast_ref::<RoutineRootValidationError>(),
        Some(RoutineRootValidationError::RuntimeRootAuthorityChanged { .. })
    ));
    assert!(matches!(
        error.downcast_ref::<RoutineRootValidationError>(),
        Some(RoutineRootValidationError::RuntimeRootAuthorityChanged { .. })
    ));
    assert_eq!(source_reads.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(
        loader
            .state
            .evaluation_attempts
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn missing_helper_cannot_hide_same_path_runtime_root_replacement() {
    let layout = tempfile::tempdir().unwrap();
    let runtime = layout.path().join("runtime");
    let original_runtime = layout.path().join("runtime-original");
    let routines = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::write(
        routines.path().join("tracked.js"),
        "agentdesk.routines.register({ name: 'Tracked', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();
    let roots = vec![routines.path().to_path_buf()];
    let loader = RoutineScriptLoader::new_shared(&roots, &runtime).unwrap();

    std::fs::rename(&runtime, &original_runtime).unwrap();
    std::fs::create_dir_all(&runtime).unwrap();
    let replacement_loader = RoutineScriptLoader::new_shared(&roots, &runtime).unwrap();

    assert!(!Arc::ptr_eq(&loader.state, &replacement_loader.state));
    let error = loader.load_dirs(&roots).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<RoutineRootValidationError>(),
        Some(RoutineRootValidationError::RuntimeRootAuthorityChanged { .. })
    ));
    assert_eq!(
        loader
            .state
            .evaluation_attempts
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn shared_loader_rejects_same_path_root_identity_replacement() {
    let runtime = tempfile::tempdir().unwrap();
    let layout = tempfile::tempdir().unwrap();
    let routines = layout.path().join("routines");
    let original = layout.path().join("routines-original");
    std::fs::create_dir_all(&routines).unwrap();
    std::fs::write(
        routines.join("tracked.js"),
        "agentdesk.routines.register({ name: 'Tracked', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();
    let roots = vec![routines.clone()];
    let loader = RoutineScriptLoader::new_shared(&roots, runtime.path()).unwrap();

    std::fs::rename(&routines, &original).unwrap();
    std::fs::create_dir_all(&routines).unwrap();
    std::fs::write(
        routines.join("tracked.js"),
        "throw new Error('replacement root must not be authorized');",
    )
    .unwrap();
    let replacement_loader = RoutineScriptLoader::new_shared(&roots, runtime.path()).unwrap();
    assert!(!Arc::ptr_eq(&loader.state, &replacement_loader.state));
    let source_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_reads = Arc::clone(&source_reads);
    *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |_| {
        observed_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }));

    let error = loader.load_dirs(&roots).unwrap_err();

    assert!(matches!(
        error.downcast_ref::<RoutineRootValidationError>(),
        Some(RoutineRootValidationError::RootIdentityChanged { root_index: 0, .. })
    ));
    assert_eq!(source_reads.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn opened_candidate_survives_path_swap_to_helper_without_reading_helper() {
    use std::os::unix::fs::symlink;

    let (release, routines, helpers) = isolated_release_surfaces();
    let candidate = routines.join("tracked.js");
    let original = routines.join("tracked-original.js");
    let helper = helpers.join("helper.js");
    std::fs::write(
        &helper,
        "throw new Error('reserved helper must never be read or evaluated');",
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
    let candidate_to_compare = candidate.canonicalize().unwrap();
    let candidate_to_replace = candidate.clone();
    let original_path = original.clone();
    let helper_target = helper.clone();
    *loader.before_source_read_hook.lock().unwrap() = Some(Arc::new(move |path| {
        if path == candidate_to_compare.as_path() {
            std::fs::rename(&candidate_to_replace, &original_path).unwrap();
            symlink(&helper_target, &candidate_to_replace).unwrap();
        }
    }));

    assert_eq!(loader.load_dirs(&[routines]).unwrap(), 1);
    assert_eq!(
        loader.get_script("tracked.js").unwrap().unwrap().name,
        "Tracked"
    );
    assert_eq!(
        loader
            .state
            .evaluation_attempts
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
    assert!(
        std::fs::symlink_metadata(&candidate)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[test]
fn candidate_identity_swap_to_helper_is_rejected_before_source_read() {
    let (release, routines, helpers) = isolated_release_surfaces();
    let candidate = routines.join("tracked.js");
    let original = routines.join("tracked-original.js");
    let helper = helpers.join("helper.js");
    std::fs::write(
        &helper,
        "throw new Error('replacement helper must never be read');",
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
    let candidate_to_compare = candidate.canonicalize().unwrap();
    let candidate_to_replace = candidate.clone();
    let original_target = original.clone();
    let helper_to_move = helper.clone();
    *loader.before_candidate_open_hook.lock().unwrap() = Some(Arc::new(move |path| {
        if path == candidate_to_compare.as_path() {
            std::fs::rename(&candidate_to_replace, &original_target).unwrap();
            std::fs::rename(&helper_to_move, &candidate_to_replace).unwrap();
        }
    }));
    let source_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_reads = Arc::clone(&source_reads);
    *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |_| {
        observed_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }));

    assert_eq!(loader.load_dirs(&[routines]).unwrap(), 0);
    assert_eq!(source_reads.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(
        loader
            .state
            .evaluation_attempts
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
}

#[test]
fn preflight_rejects_parent_after_missing_component_as_ambiguous() {
    let release = tempfile::tempdir().unwrap();
    let configured = release.path().join("missing").join("..").join("routines");
    let loader = RoutineScriptLoader::new().unwrap();
    *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());

    let error = loader.load_dirs(&[configured]).unwrap_err();

    assert!(matches!(
        error.downcast_ref::<RoutineRootValidationError>(),
        Some(RoutineRootValidationError::RootCanonicalization {
            source: PathResolutionError::AmbiguousMissingPath { .. },
            ..
        })
    ));
}

#[cfg(unix)]
#[test]
fn symlink_parent_cannot_lexically_hide_runtime_helper_surface() {
    use std::os::unix::fs::symlink;

    let (release, _routines, helpers) = isolated_release_surfaces();
    let nested = release.path().join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let aliases = tempfile::tempdir().unwrap();
    let link = aliases.path().join("link");
    symlink(&nested, &link).unwrap();
    let configured = link.join("..").join("routine-helpers");
    let (loader, source_reads, failure_sentinel) = preflight_observed_loader();
    *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());

    let error = loader.load_dirs(&[configured]).unwrap_err();

    assert!(matches!(
        error.downcast_ref::<RoutineRootValidationError>(),
        Some(RoutineRootValidationError::HelperSurfaceOverlap { root_index: 0, .. })
    ));
    assert!(helpers.exists());
    assert_preflight_rejection_has_no_load_side_effects(
        &loader,
        source_reads.as_ref(),
        &failure_sentinel,
    );
}

#[cfg(unix)]
#[test]
fn preflight_rejects_dangling_symlink_in_missing_root_prefix() {
    use std::os::unix::fs::symlink;

    let release = tempfile::tempdir().unwrap();
    let dangling = release.path().join("dangling");
    symlink(release.path().join("missing-target"), &dangling).unwrap();
    let loader = RoutineScriptLoader::new().unwrap();
    *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());

    let error = loader
        .load_dirs(&[dangling.join("nested").join("routines")])
        .unwrap_err();

    assert!(matches!(
        error.downcast_ref::<RoutineRootValidationError>(),
        Some(RoutineRootValidationError::RootCanonicalization {
            source: PathResolutionError::DanglingSymlink { .. },
            ..
        })
    ));
}

#[cfg(unix)]
#[test]
fn preflight_reports_typed_current_directory_failure() {
    const CHILD_MARKER: &str = "AGENTDESK_TEST_DELETED_ROUTINE_CWD";
    if std::env::var_os(CHILD_MARKER).is_some() {
        let deleted_cwd = tempfile::tempdir().unwrap();
        std::env::set_current_dir(deleted_cwd.path()).unwrap();
        std::fs::remove_dir(deleted_cwd.path()).unwrap();

        let error = match RoutineScriptLoader::new_shared(
            &[PathBuf::from("routines")],
            Path::new("runtime"),
        ) {
            Ok(_) => panic!("deleted cwd must not authorize relative routine paths"),
            Err(error) => error,
        };
        assert!(matches!(
            error.downcast_ref::<RoutineRootValidationError>(),
            Some(RoutineRootValidationError::CurrentDirectoryUnavailable { .. })
        ));
        std::mem::forget(deleted_cwd);
        return;
    }

    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("preflight_reports_typed_current_directory_failure")
        .arg("--nocapture")
        .env(CHILD_MARKER, "1")
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn load_dir_hashes_and_evaluates_the_same_source_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("atomic-source.js");
    std::fs::write(&path, "throw new Error('broken snapshot');").unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    let replacement_compare = path.canonicalize().unwrap();
    let replacement_path = path.clone();
    *loader.source_read_hook.lock().unwrap() = Some(Arc::new(move |candidate| {
        if candidate == replacement_compare.as_path() {
            std::fs::write(
                    &replacement_path,
                    "agentdesk.routines.register({ name: 'Replacement', tick() { return { action: 'skip' }; } });",
                )
                .unwrap();
        }
    }));

    assert_eq!(loader.load_dir(dir.path()).unwrap(), 0);
    assert_eq!(
        loader
            .state
            .failed_scripts
            .lock()
            .unwrap()
            .get(&candidate_failure_key(&path))
            .unwrap()
            .source_version,
        Some(full_source_version("throw new Error('broken snapshot');"))
    );
    assert!(!loader.has_script("atomic-source.js").unwrap());
}

#[test]
fn shared_loader_identity_includes_runtime_helper_authority() {
    let routines = tempfile::tempdir().unwrap();
    let first_runtime = tempfile::tempdir().unwrap();
    let second_runtime = tempfile::tempdir().unwrap();
    let roots = vec![routines.path().to_path_buf()];

    let first = RoutineScriptLoader::new_shared(&roots, first_runtime.path()).unwrap();
    let second = RoutineScriptLoader::new_shared(&roots, second_runtime.path()).unwrap();

    assert!(!Arc::ptr_eq(&first.state, &second.state));
}

#[cfg(unix)]
#[test]
fn shared_loader_identity_separates_distinct_runtime_alias_authorities() {
    use std::os::unix::fs::symlink;

    let layout = tempfile::tempdir().unwrap();
    let runtime = layout.path().join("runtime");
    let first_alias = layout.path().join("runtime-first");
    let second_alias = layout.path().join("runtime-second");
    let routines = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(&runtime).unwrap();
    symlink(&runtime, &first_alias).unwrap();
    symlink(&runtime, &second_alias).unwrap();
    let roots = vec![routines.path().to_path_buf()];

    let first = RoutineScriptLoader::new_shared(&roots, &first_alias).unwrap();
    let second = RoutineScriptLoader::new_shared(&roots, &second_alias).unwrap();

    assert!(!Arc::ptr_eq(&first.state, &second.state));
}

#[cfg(unix)]
#[test]
fn shared_loader_identity_separates_raw_root_aliases() {
    use std::os::unix::fs::symlink;

    let layout = tempfile::tempdir().unwrap();
    let runtime = layout.path().join("runtime");
    let routines = layout.path().join("routines");
    let routines_alias = layout.path().join("routines-alias");
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::create_dir_all(&routines).unwrap();
    symlink(&routines, &routines_alias).unwrap();

    let canonical = RoutineScriptLoader::new_shared(&[routines], &runtime).unwrap();
    let aliased = RoutineScriptLoader::new_shared(&[routines_alias], &runtime).unwrap();

    assert!(!Arc::ptr_eq(&canonical.state, &aliased.state));
}

#[cfg(unix)]
#[test]
fn helper_entry_move_creates_fresh_shared_state_and_invalidates_old_authority() {
    let release = tempfile::tempdir().unwrap();
    let routines = release.path().join("routines");
    let helpers = release.path().join("routine-helpers");
    std::fs::create_dir_all(&routines).unwrap();
    std::fs::create_dir_all(&helpers).unwrap();
    let helper_script = helpers.join("moved.js");
    std::fs::write(
        &helper_script,
        "agentdesk.routines.register({ name: 'Moved', tick() { return {}; } });",
    )
    .unwrap();
    let roots = vec![routines.clone()];
    let before = RoutineScriptLoader::new_shared(&roots, release.path()).unwrap();

    std::fs::rename(&helper_script, routines.join("moved.js")).unwrap();
    let after = RoutineScriptLoader::new_shared(&roots, release.path()).unwrap();

    assert!(!Arc::ptr_eq(&before.state, &after.state));
    assert_eq!(after.load_dirs(&roots).unwrap(), 1);
    let error = before.load_dirs(&roots).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<RoutineRootValidationError>(),
        Some(RoutineRootValidationError::HelperSurfaceAuthorityChanged { .. })
    ));
}

#[cfg(unix)]
#[test]
fn shared_loader_identity_changes_when_helper_surface_is_replaced() {
    let routines = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let helper = runtime.path().join("routine-helpers");
    let old_helper = runtime.path().join("routine-helpers-old");
    std::fs::create_dir_all(&helper).unwrap();
    let roots = vec![routines.path().to_path_buf()];

    let first = RoutineScriptLoader::new_shared(&roots, runtime.path()).unwrap();
    std::fs::rename(&helper, &old_helper).unwrap();
    std::fs::create_dir_all(&helper).unwrap();
    let second = RoutineScriptLoader::new_shared(&roots, runtime.path()).unwrap();

    assert!(!Arc::ptr_eq(&first.state, &second.state));
}

#[cfg(unix)]
#[test]
fn helper_surface_alias_change_before_candidate_read_aborts_without_side_effects() {
    use std::os::unix::fs::symlink;

    let (release, routines, helper) = isolated_release_surfaces();
    let helper_original = release.path().join("routine-helpers-original");
    let roots = vec![routines.clone()];
    let loader = RoutineScriptLoader::new_shared(&roots, release.path()).unwrap();
    assert_eq!(loader.load_dirs(&roots).unwrap(), 1);
    let loaded_version = loader
        .get_script("tracked.js")
        .unwrap()
        .unwrap()
        .script_version;
    let evaluation_attempts = loader
        .state
        .evaluation_attempts
        .load(std::sync::atomic::Ordering::Relaxed);
    let failure_sentinel = routines.join("failure-sentinel.js");
    let retry_at = Instant::now() + Duration::from_secs(17);
    loader.state.failed_scripts.lock().unwrap().insert(
        failure_sentinel.clone(),
        RoutineScriptFailure {
            source_version: Some("sentinel".to_owned()),
            consecutive_failures: 4,
            retry_at,
            warning_emitted: true,
        },
    );

    let helper_to_replace = helper.clone();
    let helper_backup = helper_original.clone();
    let helper_alias_target = routines.clone();
    *loader.before_source_read_hook.lock().unwrap() = Some(Arc::new(move |_| {
        std::fs::rename(&helper_to_replace, &helper_backup).unwrap();
        symlink(&helper_alias_target, &helper_to_replace).unwrap();
    }));
    let source_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_reads = Arc::clone(&source_reads);
    *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |_| {
        observed_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }));

    let error = loader.load_dirs(&roots).unwrap_err();

    assert!(matches!(
        error.downcast_ref::<RoutineRootValidationError>(),
        Some(RoutineRootValidationError::HelperSurfaceAuthorityChanged { .. })
    ));
    assert_eq!(source_reads.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(
        loader
            .state
            .evaluation_attempts
            .load(std::sync::atomic::Ordering::Relaxed),
        evaluation_attempts
    );
    assert_eq!(
        loader
            .state
            .scripts
            .lock()
            .unwrap()
            .get("tracked.js")
            .unwrap()
            .script_version,
        loaded_version
    );
    let failures = loader.state.failed_scripts.lock().unwrap();
    assert_eq!(failures.len(), 1);
    let sentinel = failures.get(&failure_sentinel).unwrap();
    assert_eq!(sentinel.source_version.as_deref(), Some("sentinel"));
    assert_eq!(sentinel.consecutive_failures, 4);
    assert_eq!(sentinel.retry_at, retry_at);
    assert!(sentinel.warning_emitted);
}

#[test]
fn missing_relative_root_creation_gets_fresh_shared_authority() {
    let relative = PathBuf::from("target").join(format!("routine-root-{}", uuid::Uuid::new_v4()));
    let absolute = std::env::current_dir().unwrap().join(&relative);
    let configured = vec![relative.clone()];
    let runtime_root = test_runtime_root();
    let before = RoutineScriptLoader::new_shared(&configured, &runtime_root).unwrap();

    std::fs::create_dir_all(&absolute).unwrap();
    std::fs::write(
            absolute.join("created.js"),
            "agentdesk.routines.register({ name: 'Created Later', tick() { return { action: 'skip' }; } });",
        )
        .unwrap();
    let after = RoutineScriptLoader::new_shared(&[absolute.clone()], &runtime_root).unwrap();

    assert!(!Arc::ptr_eq(&before.state, &after.state));
    assert_eq!(after.load_dirs(&[absolute.clone()]).unwrap(), 1);
    let error = before.load_dirs(&configured).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<RoutineRootValidationError>(),
        Some(RoutineRootValidationError::RootIdentityChanged { root_index: 0, .. })
    ));
    assert!(!before.has_script("created.js").unwrap());
    std::fs::remove_dir_all(absolute).unwrap();
}

#[cfg(unix)]
#[test]
fn preflight_rejects_symlink_alias_to_sibling_helper_without_side_effects() {
    use std::os::unix::fs::symlink;

    let (release, routines, helpers) = isolated_release_surfaces();
    let alias_parent = tempfile::tempdir().unwrap();
    let helper_alias = alias_parent.path().join("helper-alias");
    symlink(&helpers, &helper_alias).unwrap();

    let (loader, source_reads, failure_sentinel) = preflight_observed_loader();
    *loader.runtime_root_override.lock().unwrap() = Some(release.path().to_path_buf());
    let error = loader.load_dirs(&[routines, helper_alias]).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<RoutineRootValidationError>(),
        Some(RoutineRootValidationError::HelperSurfaceOverlap { root_index: 1, .. })
    ));
    assert_preflight_rejection_has_no_load_side_effects(
        &loader,
        source_reads.as_ref(),
        &failure_sentinel,
    );
}
