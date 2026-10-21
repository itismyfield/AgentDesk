use super::*;
use sha2::Digest;
use std::sync::atomic::{AtomicUsize, Ordering};

const ORIGINAL: &[u8] = b"# retained\r\n[session]\r\nresume_agents_on_restore=true # keep\r\n";
fn check(path: &Path) -> Result<(), String> {
    let bytes = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    if editor::is_false(&bytes) {
        Ok(())
    } else {
        Err("full checker refuses candidate".into())
    }
}
struct Fixture {
    root: tempfile::TempDir,
    target: PathBuf,
    logical: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("actual");
        std::fs::create_dir(&directory).unwrap();
        let target = directory.join("config.toml");
        std::fs::write(&target, ORIGINAL).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        let logical = root.path().join("config.toml");
        std::os::unix::fs::symlink("actual/config.toml", &logical).unwrap();
        Self {
            root,
            target,
            logical,
        }
    }
}

#[test]
fn apply_preserves_symlink_metadata_and_backup_in_the_target_directory() {
    let fixture = Fixture::new();
    let link = std::fs::symlink_metadata(&fixture.logical).unwrap();
    let metadata = std::fs::metadata(&fixture.target).unwrap();
    let plan = preview(&fixture.logical).unwrap();
    assert!(plan.requires_write());
    assert_eq!(plan.target(), fixture.target.canonicalize().unwrap());
    let calls = AtomicUsize::new(0);
    let checker = |path: &Path| {
        calls.fetch_add(1, Ordering::SeqCst);
        if path != fixture.logical {
            assert_eq!(
                path.parent(),
                fixture.target.canonicalize().unwrap().parent(),
                "temporary_target_parent_contract"
            );
        }
        check(path)
    };
    let result = apply(&plan, &checker);
    assert!(
        result.is_ok(),
        "logical_symlink_survives_contract: {result:?}"
    );
    let result = result.unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "candidate_and_final_full_checker_contract"
    );
    let after = std::fs::symlink_metadata(&fixture.logical).unwrap();
    assert_eq!(
        (link.dev(), link.ino()),
        (after.dev(), after.ino()),
        "logical_symlink_survives_contract"
    );
    assert_eq!(
        std::fs::read_link(&fixture.logical).unwrap(),
        Path::new("actual/config.toml")
    );
    let after = std::fs::metadata(&fixture.target).unwrap();
    assert_eq!(
        (after.uid(), after.gid(), after.mode() & 0o7777),
        (metadata.uid(), metadata.gid(), metadata.mode() & 0o7777),
        "uid_gid_mode_preservation_contract"
    );
    assert_eq!(
        std::fs::read(&fixture.target).unwrap(),
        plan.candidate().as_bytes()
    );
    let backup = result.backup.unwrap();
    assert_eq!(
        backup.parent(),
        fixture.target.canonicalize().unwrap().parent()
    );
    assert_eq!(
        std::fs::read(&backup).unwrap(),
        ORIGINAL,
        "durable_backup_matches_original_contract"
    );
    assert_eq!(std::fs::metadata(backup).unwrap().mode() & 0o777, 0o600);
    assert_eq!(result.logical, fixture.logical);
    assert_eq!(result.target, fixture.target.canonicalize().unwrap());
    assert_eq!(
        result.before_hash,
        Some(<[u8; 32]>::from(sha2::Sha256::digest(ORIGINAL)))
    );
    assert_eq!(
        result.after_hash,
        file::read(&fixture.target).unwrap().token.digest()
    );
    assert!(!result.original_absent);
}

#[test]
fn apply_absent_config_records_absence_and_uses_owner_only_mode() {
    let root = tempfile::tempdir().unwrap();
    let logical = root.path().join("missing.toml");
    let plan = preview(&logical).unwrap();
    let result = apply(&plan, &check).unwrap();
    assert!(result.original_absent);
    assert_eq!(result.before_hash, None);
    assert_eq!(result.backup, None);
    assert_eq!(
        std::fs::metadata(&logical).unwrap().mode() & 0o777,
        0o600,
        "absent_config_mode_contract"
    );
    assert!(editor::is_false(
        &std::fs::read_to_string(&logical).unwrap()
    ));
    assert!(!preview(&logical).unwrap().requires_write());
}

#[test]
fn apply_refuses_dangling_cycle_nonregular_and_incompatible_without_writes() {
    let fixture = Fixture::new();
    std::os::unix::fs::symlink("missing", fixture.root.path().join("dangling")).unwrap();
    std::os::unix::fs::symlink("cycle", fixture.root.path().join("cycle")).unwrap();
    for path in [
        fixture.root.path().join("dangling"),
        fixture.root.path().join("cycle"),
        fixture.root.path().join("actual"),
    ] {
        assert!(preview(&path).is_err(), "invalid_target_refusal_contract");
    }
    std::fs::write(
        &fixture.target,
        b"session={resume_agents_on_restore=true}\n",
    )
    .unwrap();
    let before = std::fs::read(&fixture.target).unwrap();
    assert!(
        preview(&fixture.logical).is_err(),
        "writer_refusal_prevents_writes_contract"
    );
    assert_eq!(std::fs::read(&fixture.target).unwrap(), before);
    assert!(
        !fixture
            .target
            .parent()
            .unwrap()
            .read_dir()
            .unwrap()
            .any(|p| p
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".agentdesk-"))
    );
}

#[test]
fn apply_stale_target_chain_owner_or_mode_never_publishes() {
    for change in ["target", "chain", "mode", "appeared"] {
        let fixture = Fixture::new();
        let logical = if change == "appeared" {
            fixture.root.path().join("absent")
        } else {
            fixture.logical.clone()
        };
        let plan = preview(&logical).unwrap();
        let checker = |_path: &Path| {
            match change {
                "target" => {
                    let replacement = fixture.root.path().join("replacement");
                    std::fs::write(&replacement, ORIGINAL).unwrap();
                    std::fs::rename(replacement, &fixture.target).unwrap();
                }
                "chain" => {
                    let replacement = fixture.root.path().join("replacement");
                    std::os::unix::fs::symlink("./actual/config.toml", &replacement).unwrap();
                    std::fs::rename(replacement, &fixture.logical).unwrap();
                }
                "mode" => std::fs::set_permissions(
                    &fixture.target,
                    std::fs::Permissions::from_mode(0o600),
                )
                .unwrap(),
                _ => std::fs::write(&logical, ORIGINAL).unwrap(),
            }
            Ok(())
        };
        let result = apply(&plan, &checker);
        assert!(
            matches!(&result, Err(error) if error.stage == Stage::Revalidate && !error.published),
            "rename_revalidation_contract: {change}: {result:?}"
        );
        let result = result.unwrap_err();
        assert_eq!(
            result.stage,
            Stage::Revalidate,
            "rename_revalidation_contract: {change}"
        );
        assert!(!result.published, "stale_preview_never_publishes_contract");
        assert_eq!(std::fs::read(&logical).unwrap(), ORIGINAL);
    }
}

#[test]
fn apply_checker_rejection_or_candidate_mutation_never_publishes() {
    for mutate in [false, true] {
        let fixture = Fixture::new();
        let plan = preview(&fixture.logical).unwrap();
        let checker = |path: &Path| {
            if mutate {
                std::fs::write(path, b"[session]\nresume_agents_on_restore=true\n").unwrap();
                Ok(())
            } else {
                Err("whole config diagnostic".into())
            }
        };
        let failure = apply(&plan, &checker);
        assert!(
            matches!(&failure, Err(error) if error.stage == Stage::Checker && !error.published),
            "checker_failure_prevents_publication_contract: {failure:?}"
        );
        let failure = failure.unwrap_err();
        assert_eq!(failure.stage, Stage::Checker);
        assert!(
            !failure.published,
            "checker_failure_prevents_publication_contract"
        );
        assert_eq!(std::fs::read(&fixture.target).unwrap(), ORIGINAL);
    }
}

#[test]
fn apply_faults_distinguish_prepublication_from_published_unverified() {
    for stage in [
        Stage::Backup,
        Stage::BackupSync,
        Stage::Metadata,
        Stage::Checker,
        Stage::CandidateSync,
        Stage::Revalidate,
        Stage::Rename,
        Stage::Publish,
        Stage::ParentSync,
        Stage::PostCheck,
    ] {
        let fixture = Fixture::new();
        let plan = preview(&fixture.logical).unwrap();
        FAULT.with(|fault| fault.set(Some(stage)));
        let result = apply(&plan, &check);
        FAULT.with(|fault| fault.set(None));
        assert!(
            result.is_err(),
            "apply_failure_does_not_claim_success_contract: {stage:?}: {result:?}"
        );
        let error = result.unwrap_err();
        assert_eq!(error.stage, stage);
        assert!(!error.detail.is_empty());
        let published = matches!(stage, Stage::ParentSync | Stage::PostCheck);
        assert_eq!(
            error.published, published,
            "apply_failure_does_not_claim_success_contract"
        );
        let bytes = std::fs::read(&fixture.target).unwrap();
        if published {
            assert_eq!(
                bytes,
                plan.candidate().as_bytes(),
                "postrename_failure_never_rolls_back_contract"
            );
            assert_eq!(std::fs::read(error.backup.unwrap()).unwrap(), ORIGINAL);
        } else {
            assert_eq!(
                bytes, ORIGINAL,
                "prepublish_fault_preserves_original_contract"
            );
        }
    }
}

#[test]
fn apply_postcheck_external_edit_is_retained_and_not_rolled_back() {
    let fixture = Fixture::new();
    let plan = preview(&fixture.logical).unwrap();
    let external = b"# external writer\n[session]\nresume_agents_on_restore=false\n";
    let checker = |path: &Path| {
        if path == fixture.logical {
            std::fs::write(path, external).unwrap();
        }
        check(path)
    };
    let error = apply(&plan, &checker);
    assert!(
        matches!(&error, Err(error) if error.stage == Stage::PostCheck && error.published),
        "postrename_external_edit_preserved_contract: {error:?}"
    );
    let error = error.unwrap_err();
    assert_eq!(error.stage, Stage::PostCheck);
    assert!(error.published);
    assert_eq!(
        std::fs::read(&fixture.target).unwrap(),
        external,
        "postrename_external_edit_preserved_contract"
    );
    assert_eq!(std::fs::read(error.backup.unwrap()).unwrap(), ORIGINAL);
}

#[test]
fn apply_aliases_resolve_to_one_target_and_reject_a_second_stale_plan() {
    let fixture = Fixture::new();
    let alias = fixture.root.path().join("alias");
    std::os::unix::fs::symlink(&fixture.target, &alias).unwrap();
    let first = preview(&fixture.logical).unwrap();
    let second = preview(&alias).unwrap();
    assert_eq!(
        first.target(),
        second.target(),
        "resolved_target_serialization_key_contract"
    );
    apply(&first, &check).unwrap();
    let calls = AtomicUsize::new(0);
    let checker = |path: &Path| {
        calls.fetch_add(1, Ordering::SeqCst);
        check(path)
    };
    let error = apply(&second, &checker).unwrap_err();
    assert!(!error.published);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "stale_alias_is_rejected_before_checker_contract"
    );
}

#[test]
fn apply_checker_timeout_refuses_before_publication() {
    let fixture = Fixture::new();
    let plan = preview(&fixture.logical).unwrap();
    let checker = |_path: &Path| Err("bounded checker timeout".to_owned());
    let result = apply(&plan, &checker);
    assert!(
        matches!(&result, Err(error) if error.stage == Stage::Checker && !error.published
        && error.detail.contains("timeout")),
        "checker_timeout_never_publishes_contract: {result:?}"
    );
    assert_eq!(std::fs::read(&fixture.target).unwrap(), ORIGINAL);
}

#[test]
fn apply_parent_directory_swap_refuses_without_overwriting_replacement() {
    let fixture = Fixture::new();
    let plan = preview(&fixture.logical).unwrap();
    let checker = |_path: &Path| {
        let parent = fixture.target.parent().unwrap();
        std::fs::rename(parent, fixture.root.path().join("old-parent")).unwrap();
        std::fs::create_dir(parent).unwrap();
        std::fs::write(&fixture.target, ORIGINAL).unwrap();
        Ok(())
    };
    let result = apply(&plan, &checker);
    assert!(
        matches!(&result, Err(error) if !error.published),
        "parent_directory_replacement_never_publishes_contract: {result:?}"
    );
    assert_eq!(std::fs::read(&fixture.target).unwrap(), ORIGINAL);
}

#[test]
fn apply_concurrent_aliases_serialize_and_only_the_first_publishes() {
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;
    let fixture = Fixture::new();
    let alias = fixture.root.path().join("alias");
    std::os::unix::fs::symlink(&fixture.target, &alias).unwrap();
    let first = preview(&fixture.logical).unwrap();
    let second = preview(&alias).unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release = Arc::new(Mutex::new(Some(release_rx)));
    let first_thread = std::thread::spawn(move || {
        let checker = |path: &Path| {
            if let Some(receiver) = release.lock().unwrap().take() {
                entered_tx.send(()).unwrap();
                receiver.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            check(path)
        };
        apply(&first, &checker)
    });
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let (second_entered_tx, second_entered_rx) = mpsc::channel();
    let second_thread = std::thread::spawn(move || {
        let checker = |path: &Path| {
            second_entered_tx.send(()).unwrap();
            check(path)
        };
        started_tx.send(()).unwrap();
        apply(&second, &checker)
    });
    started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let entered_early = second_entered_rx
        .recv_timeout(Duration::from_millis(200))
        .is_ok();
    release_tx.send(()).unwrap();
    let first_result = first_thread.join().unwrap();
    let second_result = second_thread.join().unwrap();
    assert!(!entered_early, "same_target_alias_serialization_contract");
    assert!(
        first_result.is_ok(),
        "first_serialized_apply_publishes_contract: {first_result:?}"
    );
    assert!(
        matches!(second_result, Err(error) if !error.published),
        "second_serialized_alias_stale_contract"
    );
}

struct CheckpointHook {
    stage: Stage,
    run: Box<dyn FnOnce()>,
}
thread_local! {
    static HOOK: std::cell::RefCell<Option<CheckpointHook>> = const { std::cell::RefCell::new(None) };
}
pub(super) fn run_hook(stage: Stage) {
    let hook = HOOK.with(|hook| {
        if hook
            .borrow()
            .as_ref()
            .is_some_and(|hook| hook.stage == stage)
        {
            hook.borrow_mut().take()
        } else {
            None
        }
    });
    if let Some(hook) = hook {
        (hook.run)();
    }
}

#[test]
fn apply_postcheck_parent_replacement_with_the_same_inode_is_unverified() {
    let fixture = Fixture::new();
    let replacement = fixture.root.path().join("replacement-parent");
    std::fs::create_dir(&replacement).unwrap();
    let plan = preview(&fixture.logical).unwrap();
    let current = fixture.target.parent().unwrap().to_owned();
    let target = fixture.target.clone();
    let old_parent = fixture.root.path().join("published-parent");
    let hard_link = replacement.join("config.toml");
    HOOK.with(|hook| {
        *hook.borrow_mut() = Some(CheckpointHook {
            stage: Stage::PostCheck,
            run: Box::new(move || {
                // Establish both links before the final snapshot records the inode's ctime.
                std::fs::hard_link(target, hard_link).unwrap();
            }),
        })
    });
    let checker = |path: &Path| {
        if path == fixture.logical {
            std::fs::rename(&current, &old_parent).unwrap();
            std::fs::rename(&replacement, &current).unwrap();
        }
        check(path)
    };
    let result = apply(&plan, &checker);
    assert!(
        matches!(&result, Err(error) if error.stage == Stage::PostCheck && error.published && error.detail.contains("parent changed")),
        "postcheck_parent_identity_fence_contract: {result:?}"
    );
}

fn replacement_at_checkpoint(fixture: &Fixture, stage: Stage) -> PathBuf {
    let current = fixture.target.parent().unwrap().to_owned();
    let old = fixture.root.path().join("held-parent");
    let old_result = old.clone();
    HOOK.with(|hook| {
        *hook.borrow_mut() = Some(CheckpointHook {
            stage,
            run: Box::new(move || {
                std::fs::rename(&current, &old).unwrap();
                std::fs::create_dir(&current).unwrap();
                std::fs::write(current.join("config.toml"), ORIGINAL).unwrap();
                // A same-named decoy makes a path-based rename overwrite the replacement.
                for entry in std::fs::read_dir(&old).unwrap() {
                    let entry = entry.unwrap();
                    if entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".agentdesk-candidate-")
                    {
                        std::fs::copy(entry.path(), current.join(entry.file_name())).unwrap();
                    }
                }
            }),
        });
    });
    old_result
}

#[test]
fn apply_rename_checkpoint_parent_swap_is_refused_before_publication() {
    let fixture = Fixture::new();
    let plan = preview(&fixture.logical).unwrap();
    let held = replacement_at_checkpoint(&fixture, Stage::Rename);
    let result = apply(&plan, &check);
    assert!(
        matches!(&result, Err(error) if error.stage == Stage::Rename && !error.published),
        "rename_checkpoint_parent_swap_prevents_publication_contract: {result:?}"
    );
    assert_eq!(
        std::fs::read(&fixture.target).unwrap(),
        ORIGINAL,
        "replacement_parent_config_unchanged_contract"
    );
    assert!(
        !held.read_dir().unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".agentdesk-candidate-")),
        "scratch_cleanup_stays_in_held_parent_contract"
    );
}

#[test]
fn apply_publish_remains_anchored_after_the_last_parent_check() {
    let fixture = Fixture::new();
    let plan = preview(&fixture.logical).unwrap();
    let held = replacement_at_checkpoint(&fixture, Stage::Publish);
    let result = apply(&plan, &check);
    assert_eq!(
        std::fs::read(&fixture.target).unwrap(),
        ORIGINAL,
        "descriptor_anchored_publish_never_overwrites_replacement_contract"
    );
    assert_eq!(
        std::fs::read(held.join("config.toml")).unwrap(),
        plan.candidate().as_bytes(),
        "publication_uses_held_directory_contract"
    );
    assert!(
        matches!(&result, Err(error) if error.stage == Stage::PostCheck && error.published),
        "detached_parent_publication_is_unverified_contract: {result:?}"
    );
}

#[test]
fn apply_unchanged_second_run_creates_no_backup_and_retains_manual_artifacts() {
    let fixture = Fixture::new();
    let first = apply(&preview(&fixture.logical).unwrap(), &check).unwrap();
    let parent = fixture.target.parent().unwrap();
    let names = || {
        let mut names: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    let before = names();
    let plan = preview(&fixture.logical).unwrap();
    assert!(!plan.requires_write());
    let second = apply(&plan, &check);
    assert!(
        second.is_ok(),
        "unchanged_apply_requires_no_new_backup_contract: {second:?}"
    );
    assert!(
        second.unwrap().backup.is_none(),
        "unchanged_apply_requires_no_new_backup_contract"
    );
    assert_eq!(
        names(),
        before,
        "unchanged_apply_leaves_existing_artifacts_contract"
    );
    assert!(
        first.backup.unwrap().exists(),
        "backups_retained_for_manual_cleanup_contract"
    );
    assert!(
        parent.join(".config.toml.agentdesk-apply.lock").exists(),
        "sidecar_retained_for_manual_cleanup_contract"
    );
}

#[test]
fn apply_backup_and_scratch_creation_stay_in_the_held_parent() {
    let fixture = Fixture::new();
    let plan = preview(&fixture.logical).unwrap();
    let held = replacement_at_checkpoint(&fixture, Stage::Backup);
    let result = apply(&plan, &check);
    assert!(
        matches!(&result, Err(error) if !error.published),
        "parent_swap_before_backup_refuses_contract: {result:?}"
    );
    let replacement_names: Vec<_> = std::fs::read_dir(fixture.target.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        replacement_names,
        vec![std::ffi::OsString::from("config.toml")],
        "relative_openat_never_creates_in_replacement_contract"
    );
    assert!(held.read_dir().unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".agentdesk-original-")
    }));
}
