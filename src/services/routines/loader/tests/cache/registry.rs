use super::super::*;
use std::thread;

#[test]
fn failed_load_keeps_last_known_good_registry() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.js");
    let bad = dir.path().join("bad.js");
    std::fs::write(
        &good,
        "agentdesk.routines.register({ name: 'Good', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();
    std::fs::write(&bad, "agentdesk.routines.register({ name: 'Bad' });").unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    loader.load_script(dir.path(), &good).unwrap();
    let err = loader.load_script(dir.path(), &bad).unwrap_err();

    assert!(err.to_string().contains("missing tick"));
    assert_eq!(loader.script_refs().unwrap(), vec!["good.js"]);
}

#[test]
fn load_dirs_keeps_last_known_good_operator_override() {
    let bundled = tempfile::tempdir().unwrap();
    let operator = tempfile::tempdir().unwrap();
    std::fs::write(
        bundled.path().join("shared.js"),
        "agentdesk.routines.register({ name: 'Bundled Shared', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();
    let operator_script = operator.path().join("shared.js");
    std::fs::write(
        &operator_script,
        "agentdesk.routines.register({ name: 'Operator Shared', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    let roots = [bundled.path().to_path_buf(), operator.path().to_path_buf()];
    assert_eq!(loader.load_dirs(&roots).unwrap(), 1);
    assert_eq!(
        loader.get_script("shared.js").unwrap().unwrap().name,
        "Operator Shared"
    );

    std::fs::write(
        &operator_script,
        "agentdesk.routines.register({ name: 'Broken Operator' });",
    )
    .unwrap();

    assert_eq!(loader.load_dirs(&roots).unwrap(), 0);
    assert_eq!(
        loader.get_script("shared.js").unwrap().unwrap().name,
        "Operator Shared"
    );
}

#[test]
fn load_dirs_preserves_cached_override_when_root_scan_fails() {
    let temp = tempfile::tempdir().unwrap();
    let bundled = temp.path().join("bundled");
    let operator = temp.path().join("operator");
    std::fs::create_dir_all(&bundled).unwrap();
    std::fs::create_dir_all(&operator).unwrap();
    std::fs::write(
        bundled.join("shared.js"),
        "agentdesk.routines.register({ name: 'Bundled Shared', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();
    std::fs::write(
        operator.join("shared.js"),
        "agentdesk.routines.register({ name: 'Operator Shared', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    let roots = [bundled.clone(), operator.clone()];
    assert_eq!(loader.load_dirs(&roots).unwrap(), 1);
    assert_eq!(
        loader.get_script("shared.js").unwrap().unwrap().name,
        "Operator Shared"
    );
    let retained_version = loader
        .get_script("shared.js")
        .unwrap()
        .unwrap()
        .script_version;
    let retained_refs = loader.script_refs().unwrap();
    let retained_failure_keys = loader
        .state
        .failed_scripts
        .lock()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();

    std::fs::remove_dir_all(&operator).unwrap();
    std::fs::write(&operator, "not a directory").unwrap();

    let authority_error = loader.load_dirs(&roots).unwrap_err();
    assert!(
        authority_error.to_string().contains("not a directory"),
        "{authority_error}"
    );
    assert_eq!(
        loader.get_script("shared.js").unwrap().unwrap().name,
        "Operator Shared"
    );
    assert_eq!(loader.script_refs().unwrap(), retained_refs);
    assert_eq!(
        loader
            .get_script("shared.js")
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
        retained_failure_keys
    );

    std::fs::remove_file(&operator).unwrap();

    assert_eq!(loader.load_dirs(&roots).unwrap(), 0);
    assert_eq!(
        loader.get_script("shared.js").unwrap().unwrap().name,
        "Operator Shared"
    );
    assert_eq!(
        loader
            .get_script("shared.js")
            .unwrap()
            .unwrap()
            .script_version,
        retained_version
    );
}

#[test]
fn load_dir_prunes_removed_scripts_and_keeps_failed_seen_script() {
    let dir = tempfile::tempdir().unwrap();
    let removed = dir.path().join("removed.js");
    let retained = dir.path().join("retained.js");
    std::fs::write(
        &removed,
        "agentdesk.routines.register({ name: 'Removed', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();
    std::fs::write(
        &retained,
        "agentdesk.routines.register({ name: 'Retained', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    assert_eq!(loader.load_dir(dir.path()).unwrap(), 2);

    std::fs::remove_file(&removed).unwrap();
    std::fs::write(
        &retained,
        "agentdesk.routines.register({ name: 'Broken' });",
    )
    .unwrap();

    assert_eq!(loader.load_dir(dir.path()).unwrap(), 0);
    assert_eq!(loader.script_refs().unwrap(), vec!["retained.js"]);
    assert_eq!(
        loader
            .state
            .failed_scripts
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![candidate_failure_key(&retained)]
    );

    std::fs::remove_file(&retained).unwrap();
    assert_eq!(loader.load_dir(dir.path()).unwrap(), 0);
    assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
}

#[test]
fn loader_recovers_after_lock_poisoning() {
    let loader = Arc::new(RoutineScriptLoader::new().unwrap());

    let loader_clone = Arc::clone(&loader);
    let result = thread::spawn(move || {
        let _lock = loader_clone.state.scripts.lock().unwrap();
        panic!("intentional panic to poison the lock");
    })
    .join();
    assert!(result.is_err(), "thread should have panicked");

    let refs = loader.script_refs();
    assert!(refs.is_ok(), "should recover from poison and not panic");
}
