use std::fs;

use super::*;
use crate::services::discord::runtime_store::durable_probe::{Plan, Step, Watch};

fn chain(root: &Path, parent: &Path) -> Vec<PathBuf> {
    parent
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(root))
        .map(Path::to_path_buf)
        .collect()
}

/// A pre-created chain is still flushed on first use and reused after; a new process, a replaced
/// directory and racing first publications all end with the whole chain flushed.
#[test]
fn ancestors_precreated_raced_and_restarted() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime");
    let parent = root.join("store").join("codex").join("token");
    fs::create_dir_all(&parent).unwrap();
    let target = parent.join("1.json");
    let watch = Watch::new(temp.path(), Plan::default());
    let witness = Mutex::new(Witness::default());

    fsync_ancestors_with(&witness, &root, &target).unwrap();
    let expected = chain(&root, &parent);
    assert_eq!(expected.len(), 3, "codex, store and the root itself");
    assert_eq!(watch.flushed(Step::Ancestors), expected, "pre-created dirs");

    fsync_ancestors_with(&witness, &root, &target).unwrap();
    assert_eq!(watch.count(Step::Ancestors), 3, "steady state reuses it");

    let restarted = Mutex::new(Witness::default());
    fsync_ancestors_with(&restarted, &root, &target).unwrap();
    assert_eq!(watch.count(Step::Ancestors), 6, "a new process re-flushes");

    fs::rename(root.join("store").join("codex"), temp.path().join("old")).unwrap();
    fs::create_dir_all(&parent).unwrap();
    fsync_ancestors_with(&witness, &root, &target).unwrap();
    assert_eq!(watch.count(Step::Ancestors), 9, "a replaced dir re-flushes");
    fsync_ancestors_with(&witness, &root, &target).unwrap();
    assert_eq!(watch.count(Step::Ancestors), 9);

    // Racing first publications into a fresh chain: none returns before a full chain flushed.
    let fresh = root.join("store").join("claude").join("token");
    fs::create_dir_all(&fresh).unwrap();
    let raced = Mutex::new(Witness::default());
    std::thread::scope(|scope| {
        for i in 0..4 {
            let (raced, root, fresh) = (&raced, &root, &fresh);
            scope.spawn(move || {
                fsync_ancestors_with(raced, root, &fresh.join(format!("{i}.json"))).unwrap();
            });
        }
    });
    let fresh_chain = chain(&root, &fresh);
    let flushed = watch.flushed(Step::Ancestors);
    for dir in &fresh_chain {
        assert!(flushed.contains(dir), "{} never flushed", dir.display());
    }
}

/// A failed ancestor flush is reported and remembers nothing, so the next publication retries.
#[test]
fn failed_ancestor_flush_is_not_remembered() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime");
    let parent = root.join("store").join("codex");
    fs::create_dir_all(&parent).unwrap();
    let target = parent.join("1.json");
    let witness = Mutex::new(Witness::default());
    {
        let plan = Plan {
            fail: vec![Step::Ancestors],
            ..Plan::default()
        };
        let _failing = Watch::new(temp.path(), plan);
        let error = fsync_ancestors_with(&witness, &root, &target).unwrap_err();
        assert!(error.to_string().contains("sync ancestor"), "{error}");
    }
    let watch = Watch::new(temp.path(), Plan::default());
    fsync_ancestors_with(&witness, &root, &target).unwrap();
    assert_eq!(watch.flushed(Step::Ancestors), chain(&root, &parent));
}

/// The walk never leaves the trusted root: a parent outside it, or reached through `..`, is
/// refused before any directory is opened.
#[test]
fn walk_refuses_paths_outside_the_trusted_root() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime");
    fs::create_dir_all(root.join("store")).unwrap();
    let watch = Watch::new(temp.path(), Plan::default());
    let witness = Mutex::new(Witness::default());
    for target in [
        temp.path().join("elsewhere").join("1.json"),
        root.join("store").join("..").join("..").join("1.json"),
    ] {
        let error = fsync_ancestors_with(&witness, &root, &target).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{target:?}");
    }
    let error = fsync_ancestors_with(&witness, Path::new(""), &root.join("1.json")).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(watch.events().is_empty(), "nothing was opened");
    fsync_ancestors_with(&witness, &root, &root.join("1.json")).unwrap();
    assert!(
        watch.events().is_empty(),
        "a file directly in root has no ancestors"
    );
}

/// Eviction drops the oldest chain first, and an evicted chain no longer matches.
#[test]
fn witness_evicts_the_oldest_chain_at_its_cap() {
    let mut witness = Witness::default();
    let key = |n: u8| (PathBuf::from("/r"), PathBuf::from(format!("/r/{n}")));
    witness.record(key(1), vec![(1, 1)], 2);
    witness.record(key(2), vec![(1, 2)], 2);
    witness.record(key(3), vec![(1, 3)], 2);
    assert!(!witness.matches(&key(1), &[(1, 1)]));
    assert!(witness.matches(&key(2), &[(1, 2)]));
    assert!(witness.matches(&key(3), &[(1, 3)]));
    assert!(
        !witness.matches(&key(3), &[(1, 4)]),
        "identity is part of the match"
    );
}
