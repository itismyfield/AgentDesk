use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub reads: usize,
    pub stats: usize,
}

thread_local! {
    static COUNTS: RefCell<HashMap<PathBuf, Counts>> = RefCell::new(HashMap::new());
}

pub(crate) fn read(path: &Path) {
    COUNTS.with_borrow_mut(|counts| counts.entry(path.to_path_buf()).or_default().reads += 1);
}

pub(crate) fn stat(path: &Path) {
    COUNTS.with_borrow_mut(|counts| counts.entry(path.to_path_buf()).or_default().stats += 1);
}

pub fn take(path: &Path) -> Counts {
    COUNTS.with_borrow_mut(|counts| counts.remove(path).unwrap_or_default())
}

#[test]
fn replacement_fingerprint_uses_identity_when_available() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("context.json");
    std::fs::write(&path, b"before").unwrap();
    let before = super::super::file_stamp(&path).unwrap();
    let replacement = dir.path().join("replacement.json");
    std::fs::write(&replacement, b"after!").unwrap();
    filetime::set_file_mtime(
        &replacement,
        filetime::FileTime::from_system_time(before.modified),
    )
    .unwrap();
    std::fs::rename(replacement, &path).unwrap();
    let after = super::super::file_stamp(&path).unwrap();
    assert_eq!(before.len, after.len);
    assert_eq!(before.modified, after.modified);
    assert_eq!(before == after, before.ino == 0);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    AfterRead,
    BeforeValidate,
}

struct Mutation {
    stage: Stage,
    change: Box<dyn FnOnce(&Path)>,
}

thread_local! {
    static NEXT: RefCell<Option<Mutation>> = const { RefCell::new(None) };
}

pub struct Injection;

impl Injection {
    pub fn fired(&self) -> bool {
        NEXT.with_borrow(Option::is_none)
    }
}

impl Drop for Injection {
    fn drop(&mut self) {
        let _ = NEXT.with_borrow_mut(Option::take);
    }
}

pub fn mutate_once(stage: Stage, change: impl FnOnce(&Path) + 'static) -> Injection {
    NEXT.with_borrow_mut(|next| {
        assert!(
            next.replace(Mutation {
                stage,
                change: Box::new(change)
            })
            .is_none()
        );
    });
    Injection
}

pub(super) fn point(stage: Stage, path: &Path) {
    let change = NEXT.with_borrow_mut(|next| {
        if next.as_ref().is_some_and(|hook| hook.stage == stage) {
            next.take().map(|hook| hook.change)
        } else {
            None
        }
    });
    if let Some(change) = change {
        change(path);
    }
}

#[test]
fn confirmation_rechecks_a_path_that_appeared_after_observation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("context.json");
    let mut observations = super::Observations::default();
    assert!(observations.stamp(&path).is_none());
    let hook = mutate_once(Stage::BeforeValidate, |path| {
        std::fs::write(path, b"new context").unwrap();
    });
    assert!(observations.changed().contains(&path));
    assert!(hook.fired());
}
