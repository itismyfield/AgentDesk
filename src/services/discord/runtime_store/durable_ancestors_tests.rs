use std::collections::HashMap;
use std::fs;
use std::time::UNIX_EPOCH;

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

/// Whether this filesystem reports birth times, without which no link is ever reused.
fn reuses(dir: &Path) -> bool {
    fs::metadata(dir).and_then(|meta| meta.created()).is_ok()
}

/// Successful ancestor flushes recorded since `from`.
fn flushed_since(watch: &Watch, from: usize) -> Vec<PathBuf> {
    watch.flushed(Step::Ancestors).split_off(from)
}

/// A pre-created chain is still flushed on first use and reused after; a new process, a replaced
/// directory and racing first publications all end with every needed link flushed.
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
    let reuse = reuses(temp.path());

    fsync_ancestors_with(&witness, &root, &target).unwrap();
    let expected = chain(&root, &parent);
    assert_eq!(expected.len(), 3, "codex, store and the root itself");
    assert_eq!(watch.flushed(Step::Ancestors), expected, "pre-created dirs");

    fsync_ancestors_with(&witness, &root, &target).unwrap();
    let steady = if reuse { 3 } else { 6 };
    assert_eq!(
        watch.count(Step::Ancestors),
        steady,
        "steady state reuses it"
    );

    let restarted = Mutex::new(Witness::default());
    fsync_ancestors_with(&restarted, &root, &target).unwrap();
    assert_eq!(
        watch.count(Step::Ancestors),
        steady + 3,
        "a new process re-flushes"
    );

    // Replacing `codex` changes the identities of the two links through it; the root's entry
    // for `store` is untouched and stays proven.
    fs::rename(root.join("store").join("codex"), temp.path().join("old")).unwrap();
    fs::create_dir_all(&parent).unwrap();
    let before = watch.count(Step::Ancestors);
    fsync_ancestors_with(&witness, &root, &target).unwrap();
    let replaced = flushed_since(&watch, before);
    let store = root.join("store");
    if reuse {
        assert_eq!(
            replaced,
            [store.join("codex"), store.clone()],
            "a replaced dir"
        );
    } else {
        assert_eq!(replaced, expected);
    }
    let before = watch.count(Step::Ancestors);
    fsync_ancestors_with(&witness, &root, &target).unwrap();
    assert_eq!(watch.count(Step::Ancestors) == before, reuse);

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

/// A new token under a proven provider flushes only the provider; a recreated directory, a new
/// process, a pre-created directory and a missing birth time are never trusted.
#[test]
fn ancestor_links_reused_only_with_same_identities() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let temp = tempfile::tempdir().unwrap();
    if !reuses(temp.path()) {
        return; // nothing is reused without birth times; the oracle test still checks safety
    }
    let root = temp.path().join("runtime");
    let (store, provider) = (root.join("store"), root.join("store").join("codex"));
    let file = |token: &str| provider.join(token).join("1.json");
    fs::create_dir_all(provider.join("t1")).unwrap();
    let watch = Watch::new(temp.path(), Plan::default());
    let witness = Mutex::new(Witness::default());
    let publish = |witness: &Mutex<Witness>, target: &Path| {
        let before = watch.count(Step::Ancestors);
        fsync_ancestors_with(witness, &root, target).unwrap();
        flushed_since(&watch, before)
    };
    let whole = [provider.clone(), store.clone(), root.clone()];

    assert_eq!(publish(&witness, &file("t1")), whole, "first use");
    fs::create_dir_all(provider.join("t2")).unwrap();
    assert_eq!(
        publish(&witness, &file("t2")),
        [provider.clone()],
        "new token"
    );
    assert!(publish(&witness, &file("t2")).is_empty(), "steady");

    fs::remove_dir(provider.join("t2")).unwrap();
    fs::create_dir(provider.join("t2")).unwrap();
    assert_eq!(
        publish(&witness, &file("t2")),
        [provider.clone()],
        "recreated token"
    );

    fs::remove_dir_all(&provider).unwrap();
    fs::create_dir_all(provider.join("t1")).unwrap();
    let recreated = publish(&witness, &file("t1"));
    assert_eq!(
        recreated,
        [provider.clone(), store.clone()],
        "recreated provider"
    );

    let restarted = Mutex::new(Witness::default());
    assert_eq!(publish(&restarted, &file("t1")), whole, "a new process");

    let other = store.join("claude").join("t");
    fs::create_dir_all(&other).unwrap();
    let precreated = publish(&witness, &other.join("1.json"));
    assert_eq!(
        precreated,
        [store.join("claude"), store.clone()],
        "pre-created"
    );

    let _unprovable = no_birth_time::Guard::new(temp.path());
    assert_eq!(publish(&witness, &file("t1")), whole, "no birth time");
    assert_eq!(publish(&witness, &file("t1")), whole, "never remembered");
}

/// Directories (re)created by the test, with the probe event count at the time.
struct Born(HashMap<PathBuf, usize>);

impl Born {
    fn create(&mut self, watch: &Watch, dir: &Path) {
        let missing: Vec<PathBuf> = dir
            .ancestors()
            .take_while(|dir| !dir.exists())
            .map(Path::to_path_buf)
            .collect();
        fs::create_dir_all(dir).unwrap();
        let now = watch.events().len();
        self.0.extend(missing.into_iter().map(|dir| (dir, now)));
    }

    /// Every entry between `target`'s parent and `root` was flushed by a sync of its directory
    /// that ran after both ends of the entry were created, as a crash right after the publication
    /// would need; a flush from before a directory's latest creation proves nothing.
    fn assert_durable(&self, watch: &Watch, root: &Path, target: &Path) {
        let events = watch.events();
        let mut child = target.parent().unwrap();
        while child != root {
            let parent = child.parent().unwrap();
            let since = [parent, child].map(|dir| self.0.get(dir).copied().unwrap_or(0));
            let since = since.into_iter().max().unwrap();
            let flushed = events[since..]
                .iter()
                .any(|e| e.step == Step::Ancestors && e.ok && e.dir == parent);
            assert!(
                flushed,
                "{} -> {} not durable",
                parent.display(),
                child.display()
            );
            child = parent;
        }
    }
}

/// After every publication each link up to the root is backed by a flush that ran after the
/// link's directories last appeared, across new tokens, providers, replacements, failures and
/// concurrent first publications.
#[test]
fn every_chain_link_flushed_after_its_identity() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime");
    fs::create_dir_all(&root).unwrap();
    let watch = Watch::new(temp.path(), Plan::default());
    let witness = Mutex::new(Witness::default());
    let mut born = Born(HashMap::new());
    let tokens = |provider: &str, token: &str| root.join("store").join(provider).join(token);
    let mut publish = |born: &mut Born, dir: PathBuf| {
        born.create(&watch, &dir);
        let target = dir.join("1.json");
        fsync_ancestors_with(&witness, &root, &target).unwrap();
        born.assert_durable(&watch, &root, &target);
    };

    publish(&mut born, tokens("codex", "a"));
    publish(&mut born, tokens("codex", "b"));
    publish(&mut born, tokens("claude", "a"));
    publish(&mut born, tokens("codex", "a"));
    fs::remove_dir_all(tokens("codex", "b")).unwrap();
    publish(&mut born, tokens("codex", "b"));
    fs::remove_dir_all(root.join("store").join("codex")).unwrap();
    publish(&mut born, tokens("codex", "a"));
    publish(&mut born, root.join("other").join("x").join("y"));

    // A failed flush leaves its link unproven, so the next publication still flushes it.
    born.create(&watch, &tokens("codex", "c"));
    let failing = Plan {
        fail: vec![Step::Ancestors],
        ..Plan::default()
    };
    {
        let _failing = Watch::new(temp.path(), failing);
        let target = tokens("codex", "c").join("1.json");
        fsync_ancestors_with(&witness, &root, &target).unwrap_err();
    }
    publish(&mut born, tokens("codex", "c"));

    let fresh: Vec<PathBuf> = (0..4)
        .map(|i| tokens(&format!("race{}", i % 2), "t"))
        .collect();
    for dir in &fresh {
        born.create(&watch, dir);
    }
    std::thread::scope(|scope| {
        for (i, dir) in fresh.iter().enumerate() {
            let (witness, root) = (&witness, &root);
            scope.spawn(move || {
                let target = dir.join(format!("{i}.json"));
                fsync_ancestors_with(witness, root, &target).unwrap();
            });
        }
    });
    for (i, dir) in fresh.iter().enumerate() {
        born.assert_durable(&watch, &root, &dir.join(format!("{i}.json")));
    }
}

/// A failed ancestor flush is reported and remembers nothing, so the next publication retries
/// that link, while links proven before the failure stay proven.
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
    let failing = || {
        let plan = Plan {
            fail: vec![Step::Ancestors],
            ..Plan::default()
        };
        Watch::new(temp.path(), plan)
    };
    {
        let _failing = failing();
        let error = fsync_ancestors_with(&witness, &root, &target).unwrap_err();
        assert!(error.to_string().contains("sync ancestor"), "{error}");
    }
    let watch = Watch::new(temp.path(), Plan::default());
    fsync_ancestors_with(&witness, &root, &target).unwrap();
    assert_eq!(watch.flushed(Step::Ancestors), chain(&root, &parent));
    drop(watch);

    let token = parent.join("t");
    fs::create_dir(&token).unwrap();
    {
        let _failing = failing();
        fsync_ancestors_with(&witness, &root, &token.join("1.json")).unwrap_err();
    }
    let watch = Watch::new(temp.path(), Plan::default());
    fsync_ancestors_with(&witness, &root, &token.join("1.json")).unwrap();
    let retried = watch.flushed(Step::Ancestors);
    if reuses(temp.path()) {
        assert_eq!(retried, [parent.clone()], "only the failed link");
    } else {
        assert_eq!(retried, chain(&root, &token));
    }
}

/// A symlinked directory below the root is flushed on every publication and never reported as
/// flushed, since the entry naming its target lies outside the chain.
#[test]
fn symlinked_directory_is_flushed_but_unconfirmed() {
    if !PARENT_DIR_FSYNC_FLUSHES {
        return; // no directory flush to observe; `windows_contract` covers this platform
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("runtime");
    let outside = temp.path().join("outside");
    fs::create_dir_all(outside.join("token")).unwrap();
    fs::create_dir_all(&root).unwrap();
    // `ln` keeps this file free of a platform cfg; the flush guard above already skips Windows.
    let linked = std::process::Command::new("ln")
        .arg("-s")
        .args([&outside, &root.join("store")])
        .status()
        .unwrap();
    assert!(linked.success());
    let target = root.join("store").join("token").join("1.json");
    let watch = Watch::new(temp.path(), Plan::default());
    let witness = Mutex::new(Witness::default());
    for round in 1..=2 {
        let error = fsync_ancestors_with(&witness, &root, &target).unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error}");
        let expected: Vec<PathBuf> = (0..round)
            .flat_map(|_| [root.join("store"), root.clone()])
            .collect();
        assert_eq!(watch.flushed(Step::Ancestors), expected, "round {round}");
    }
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

/// Eviction drops the least recently used link first, an evicted or re-identified link no
/// longer matches, and an identity-less link is never remembered.
#[test]
fn witness_evicts_the_oldest_chain_at_its_cap() {
    let mut witness = Witness::default();
    let id = |n: u64| Some((1, n, UNIX_EPOCH));
    let dir = |n: u8| PathBuf::from(format!("/r/{n}"));
    witness.record(&dir(1), (id(0), id(1)), 2);
    witness.record(&dir(2), (id(0), id(2)), 2);
    assert!(witness.matches(&dir(1), (id(0), id(1))), "touches 1");
    witness.record(&dir(3), (id(0), id(3)), 2);
    assert!(
        !witness.matches(&dir(2), (id(0), id(2))),
        "2 was least recent"
    );
    assert!(witness.matches(&dir(1), (id(0), id(1))));
    assert!(witness.matches(&dir(3), (id(0), id(3))));
    assert!(
        !witness.matches(&dir(3), (id(0), id(4))),
        "identity is part of the match"
    );
    assert!(
        !witness.matches(&dir(3), (id(0), id(3))),
        "a mismatch drops the stale entry"
    );
    witness.record(&dir(4), (id(0), None), 2);
    assert!(!witness.matches(&dir(4), (id(0), None)));
}
