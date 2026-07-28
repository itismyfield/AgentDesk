use super::*;

#[cfg(unix)]
#[test]
fn runtime_root_alias_retarget_during_binding_is_rejected() {
    use std::os::unix::fs::symlink;

    let layout = tempfile::tempdir().unwrap();
    let first_runtime = layout.path().join("runtime-one");
    let second_runtime = layout.path().join("runtime-two");
    std::fs::create_dir_all(first_runtime.join("routines")).unwrap();
    std::fs::create_dir_all(second_runtime.join("routine-helpers")).unwrap();
    symlink(
        second_runtime.join("routine-helpers"),
        second_runtime.join("routines"),
    )
    .unwrap();
    let runtime_alias = layout.path().join("current-runtime");
    symlink(&first_runtime, &runtime_alias).unwrap();
    let roots = vec![runtime_alias.join("routines")];
    let alias_to_retarget = runtime_alias.clone();
    let second_target = second_runtime.clone();

    let error = bind_routine_root_authority_with_hook(&roots, &runtime_alias, move || {
        std::fs::remove_file(&alias_to_retarget).unwrap();
        symlink(&second_target, &alias_to_retarget).unwrap();
    })
    .unwrap_err();

    assert!(matches!(
        error,
        RoutineRootValidationError::RuntimeRootAuthorityChanged { .. }
    ));
}

#[cfg(unix)]
#[test]
fn missing_root_resolution_preserves_symlink_parent_semantics() {
    use std::os::unix::fs::symlink;

    let release = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("foo");
    std::fs::create_dir_all(&target).unwrap();
    let link = release.path().join("link");
    symlink(&target, &link).unwrap();
    let configured = link.join("..").join("future-routines");

    let validated =
        validate_routine_roots(&[configured], release.path(), Some(release.path())).unwrap();

    assert_eq!(
        validated[0].canonical,
        outside
            .path()
            .canonicalize()
            .unwrap()
            .join("future-routines")
    );
    assert!(!validated[0].exists);
}

#[cfg(unix)]
#[test]
fn missing_root_symlink_insertion_during_validation_is_rejected() {
    use std::os::unix::fs::symlink;

    let release = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let missing_parent = release.path().join("missing-parent");
    let configured = missing_parent.join("routines");
    let parent_to_create = missing_parent.clone();
    let outside_target = outside.path().to_path_buf();

    let error = validate_routine_authority_with_hook(
        &[configured],
        release.path(),
        Some(release.path()),
        move |root_index| {
            assert_eq!(root_index, 0);
            symlink(&outside_target, &parent_to_create).unwrap();
        },
    )
    .unwrap_err();

    assert!(matches!(
        error,
        RoutineRootValidationError::RootAuthorityChangedDuringValidation { root_index: 0, .. }
    ));
}
