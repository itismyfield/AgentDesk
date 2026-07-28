use super::*;

fn write_budgeted_routine(path: &Path, name: &str, metadata: &str) -> usize {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let source = format!(
        "agentdesk.routines.register({{ name: {name:?}, metadata: {metadata}, tick() {{ return {{ action: 'skip' }}; }} }});"
    );
    let source_bytes = source.len();
    std::fs::write(path, source).unwrap();
    source_bytes
}

#[test]
fn load_dir_recurses_into_nested_script_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("ops").join("daily");
    std::fs::create_dir_all(&nested).unwrap();
    let path = nested.join("summary.js");
    std::fs::write(
        &path,
        "agentdesk.routines.register({ name: 'Nested', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    assert_eq!(loader.load_dir(dir.path()).unwrap(), 1);
    assert_eq!(loader.script_refs().unwrap(), vec!["ops/daily/summary.js"]);
    assert!(loader.has_script("ops/daily/summary.js").unwrap());
}

#[test]
fn load_dir_ignores_sibling_node_helpers_and_preserves_quickjs_refs() {
    let parent = tempfile::tempdir().unwrap();
    let routines = parent.path().join("routines");
    let nested = routines.join("monitoring");
    let helpers = parent.path().join("routine-helpers");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir_all(&helpers).unwrap();

    let nested_routine = nested.join("inventory.js");
    let root_routine = routines.join("tracked.js");
    let node_helper = helpers.join("inventory.js");
    std::fs::write(
        &node_helper,
        "throw new Error('sibling Node helper must never be evaluated by QuickJS');",
    )
    .unwrap();
    std::fs::write(
        &nested_routine,
        "agentdesk.routines.register({ name: 'Nested Inventory', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();
    std::fs::write(
        &root_routine,
        "agentdesk.routines.register({ name: 'Tracked Root', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    let source_reads = Arc::new(Mutex::new(Vec::new()));
    let observed_source_reads = Arc::clone(&source_reads);
    *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |path| {
        observed_source_reads
            .lock()
            .unwrap()
            .push(path.to_path_buf());
    }));

    assert_eq!(loader.load_dir(&routines).unwrap(), 2);
    assert_eq!(
        loader.script_refs().unwrap(),
        vec!["monitoring/inventory.js", "tracked.js"]
    );
    assert!(loader.has_script("monitoring/inventory.js").unwrap());
    assert!(loader.has_script("tracked.js").unwrap());
    assert_eq!(
        loader
            .state
            .evaluation_attempts
            .load(std::sync::atomic::Ordering::Relaxed),
        2
    );
    let source_reads = source_reads.lock().unwrap();
    let nested_routine = nested_routine.canonicalize().unwrap();
    let root_routine = root_routine.canonicalize().unwrap();
    let node_helper = node_helper.canonicalize().unwrap();
    assert_eq!(source_reads.len(), 2);
    assert!(source_reads.contains(&nested_routine));
    assert!(source_reads.contains(&root_routine));
    assert!(
        !source_reads.contains(&node_helper),
        "sibling Node helper was source-read"
    );
    assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
}

#[test]
fn valid_disjoint_operator_root_loads_and_overrides_nested_routine() {
    let bundled = tempfile::tempdir().unwrap();
    let operator = tempfile::tempdir().unwrap();
    let bundled_monitoring = bundled.path().join("monitoring");
    let operator_monitoring = operator.path().join("monitoring");
    std::fs::create_dir_all(&bundled_monitoring).unwrap();
    std::fs::create_dir_all(&operator_monitoring).unwrap();
    std::fs::write(
        bundled_monitoring.join("inventory.js"),
        "agentdesk.routines.register({ name: 'Bundled Inventory', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();
    std::fs::write(
        operator_monitoring.join("inventory.js"),
        "agentdesk.routines.register({ name: 'Operator Inventory', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    assert_eq!(
        loader
            .load_dirs(&[bundled.path().to_path_buf(), operator.path().to_path_buf()])
            .unwrap(),
        1
    );
    assert_eq!(
        loader
            .get_script("monitoring/inventory.js")
            .unwrap()
            .unwrap()
            .name,
        "Operator Inventory"
    );
}

#[test]
fn load_dirs_supports_operator_override_dirs() {
    let bundled = tempfile::tempdir().unwrap();
    let operator = tempfile::tempdir().unwrap();
    let bundled_nested = bundled.path().join("ops");
    let operator_nested = operator.path().join("ops");
    std::fs::create_dir_all(&bundled_nested).unwrap();
    std::fs::create_dir_all(&operator_nested).unwrap();
    std::fs::write(
        bundled.path().join("bundled-only.js"),
        "agentdesk.routines.register({ name: 'Bundled Only', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();
    std::fs::write(
        bundled_nested.join("shared.js"),
        "agentdesk.routines.register({ name: 'Bundled Shared', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();
    std::fs::write(
        operator.path().join("operator-only.js"),
        "agentdesk.routines.register({ name: 'Operator Only', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();
    std::fs::write(
        operator_nested.join("shared.js"),
        "agentdesk.routines.register({ name: 'Operator Shared', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    assert_eq!(
        loader
            .load_dirs(&[bundled.path().to_path_buf(), operator.path().to_path_buf()])
            .unwrap(),
        3
    );
    assert_eq!(
        loader.script_refs().unwrap(),
        vec![
            "bundled-only.js".to_string(),
            "operator-only.js".to_string(),
            "ops/shared.js".to_string()
        ]
    );
    let shared = loader.get_script("ops/shared.js").unwrap().unwrap();
    assert_eq!(shared.name, "Operator Shared");
    assert!(
        shared
            .file
            .starts_with(operator.path().canonicalize().unwrap())
    );
}

#[test]
fn load_dirs_rejects_invalid_root_without_partially_loading_healthy_roots() {
    let temp = tempfile::tempdir().unwrap();
    let invalid_root = temp.path().join("not-a-directory");
    std::fs::write(&invalid_root, "not a directory").unwrap();
    let healthy_root = temp.path().join("healthy");
    std::fs::create_dir_all(&healthy_root).unwrap();
    std::fs::write(
        healthy_root.join("healthy.js"),
        "agentdesk.routines.register({ name: 'Healthy', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    let error = loader.load_dirs(&[invalid_root, healthy_root]).unwrap_err();
    assert!(error.to_string().contains("not a directory"), "{error}");
    assert!(loader.script_refs().unwrap().is_empty());
    assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
}

#[test]
fn retained_output_budget_is_shared_across_roots_and_accepts_exact_boundary() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let first_source_bytes =
        write_budgeted_routine(&first.path().join("a.js"), "é", "{ 'é': '💣' }");
    let second_source_bytes =
        write_budgeted_routine(&second.path().join("b.js"), "é", "{ 'é': '💣' }");
    // Each retained script also owns an 8-byte name/metadata payload.
    let exact_budget = first_source_bytes + second_source_bytes + 16;
    let roots = [first.path().to_path_buf(), second.path().to_path_buf()];
    let loader = RoutineScriptLoader::new().unwrap();

    let error = loader
        .load_dirs_with_retained_output_limit(&roots, exact_budget - 1)
        .unwrap_err();
    assert!(error.to_string().contains("retained output"), "{error}");
    assert!(loader.script_refs().unwrap().is_empty());
    assert!(loader.state.failed_scripts.lock().unwrap().is_empty());

    assert_eq!(
        loader
            .load_dirs_with_retained_output_limit(&roots, exact_budget)
            .unwrap(),
        2
    );
    assert_eq!(loader.script_refs().unwrap(), vec!["a.js", "b.js"]);
}

#[test]
fn retained_output_budget_includes_implicit_lkg_and_commits_atomically() {
    let root = tempfile::tempdir().unwrap();
    let retained = root.path().join("a.js");
    let retained_source_bytes = write_budgeted_routine(&retained, "A", "{ k: '12' }");
    let loader = RoutineScriptLoader::new().unwrap();
    assert_eq!(loader.load_dir(root.path()).unwrap(), 1);
    let retained_version = loader.get_script("a.js").unwrap().unwrap().script_version;

    std::fs::write(
        &retained,
        "agentdesk.routines.register({ name: 'Broken' });",
    )
    .unwrap();
    let fresh_source_bytes = write_budgeted_routine(&root.path().join("b.js"), "B", "{ k: '12' }");
    // Each retained script also owns a 4-byte name/metadata payload.
    let exact_budget = retained_source_bytes + fresh_source_bytes + 8;

    let error = loader
        .load_dirs_with_retained_output_limit(&[root.path().to_path_buf()], exact_budget - 1)
        .unwrap_err();
    assert!(error.to_string().contains("retained output"), "{error}");
    assert_eq!(loader.script_refs().unwrap(), vec!["a.js"]);
    assert_eq!(
        loader.get_script("a.js").unwrap().unwrap().script_version,
        retained_version
    );
    assert!(loader.state.failed_scripts.lock().unwrap().is_empty());

    assert_eq!(
        loader
            .load_dirs_with_retained_output_limit(&[root.path().to_path_buf()], exact_budget)
            .unwrap(),
        1
    );
    assert_eq!(loader.script_refs().unwrap(), vec!["a.js", "b.js"]);
    assert_eq!(
        loader.get_script("a.js").unwrap().unwrap().script_version,
        retained_version
    );
}

#[test]
fn retained_output_budget_includes_explicit_cached_scan_fallback() {
    let parent = tempfile::tempdir().unwrap();
    let retained_root = parent.path().join("retained");
    let fresh_root = parent.path().join("fresh");
    let retained_source_bytes =
        write_budgeted_routine(&retained_root.join("a.js"), "A", "{ k: '12' }");
    let loader = RoutineScriptLoader::new().unwrap();
    assert_eq!(loader.load_dir(&retained_root).unwrap(), 1);
    std::fs::remove_dir_all(&retained_root).unwrap();
    let fresh_source_bytes = write_budgeted_routine(&fresh_root.join("b.js"), "B", "{ k: '12' }");
    let exact_budget = retained_source_bytes + fresh_source_bytes + 8;
    let roots = [retained_root, fresh_root];

    let error = loader
        .load_dirs_with_retained_output_limit(&roots, exact_budget - 1)
        .unwrap_err();
    assert!(error.to_string().contains("retained output"), "{error}");
    assert_eq!(loader.script_refs().unwrap(), vec!["a.js"]);

    assert_eq!(
        loader
            .load_dirs_with_retained_output_limit(&roots, exact_budget)
            .unwrap(),
        1
    );
    assert_eq!(loader.script_refs().unwrap(), vec!["a.js", "b.js"]);
}

#[test]
fn aggregate_discovery_exhaustion_is_fatal_and_registry_atomic() {
    let retained_root = tempfile::tempdir().unwrap();
    let retained_path = retained_root.path().join("retained.js");
    write_budgeted_routine(&retained_path, "Retained", "{}");
    let loader = RoutineScriptLoader::new().unwrap();
    assert_eq!(loader.load_dir(retained_root.path()).unwrap(), 1);
    let retained_version = loader
        .get_script("retained.js")
        .unwrap()
        .unwrap()
        .script_version;
    let failure_sentinel = retained_root.path().join("failure-sentinel.js");
    loader.record_failure(
        &failure_sentinel,
        Some("sentinel-version".to_string()),
        std::time::Instant::now(),
    );
    let failures_before = loader
        .state
        .failed_scripts
        .lock()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let evaluations_before = loader
        .state
        .evaluation_attempts
        .load(std::sync::atomic::Ordering::Relaxed);

    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let first_source_bytes = write_budgeted_routine(&first.path().join("a.js"), "A", "{}");
    let second_source_bytes = write_budgeted_routine(&second.path().join("b.js"), "B", "{}");
    let roots = [first.path().to_path_buf(), second.path().to_path_buf()];
    let error = loader
        .load_dirs_with_limits(
            &roots,
            MAX_ROUTINE_RETAINED_OUTPUT_BYTES,
            RoutineTreeLimits {
                max_entries: 2,
                max_files: 2,
                max_depth: 1,
                max_source_bytes: (first_source_bytes + second_source_bytes - 1) as u64,
            },
        )
        .unwrap_err();

    assert!(error.to_string().contains("source bytes"), "{error}");
    assert_eq!(loader.script_refs().unwrap(), vec!["retained.js"]);
    assert_eq!(
        loader
            .get_script("retained.js")
            .unwrap()
            .unwrap()
            .script_version,
        retained_version
    );
    assert_eq!(
        loader
            .state
            .failed_scripts
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        failures_before
    );
    assert_eq!(
        loader
            .state
            .evaluation_attempts
            .load(std::sync::atomic::Ordering::Relaxed),
        evaluations_before
    );
}
