//! Launch-context observations belong to one notice-owner refresh.
use super::{Stamp, file_stamp};
use crate::services::tui_prompt_dedupe::binding_context::{self, BindingContext};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[cfg(any(test, feature = "test-support"))]
pub mod tests;

pub(super) struct Observed {
    pub(super) stamp: Option<Stamp>,
    pub(super) context: Option<BindingContext>,
    pub(super) unavailable: bool,
}

#[derive(Default)]
pub(super) struct Observations {
    stamps: HashMap<PathBuf, Option<Stamp>>,
    reads: HashMap<PathBuf, Observed>,
}

impl Observations {
    pub(super) fn stamp(&mut self, path: &Path) -> Option<Stamp> {
        if let Some(stamp) = self.stamps.get(path) {
            return stamp.clone();
        }
        let stamp = file_stamp(path);
        // A zero inode cannot detect replacement, so keep the original per-read behavior there.
        if stamp.as_ref().is_none_or(|stamp| stamp.ino != 0) {
            self.stamps.insert(path.to_path_buf(), stamp.clone());
        }
        stamp
    }

    pub(super) fn observe(&mut self, path: &Path, nonce: &str) -> &Observed {
        let before = self.stamp(path);
        if before.as_ref().is_some_and(|stamp| stamp.ino == 0) {
            self.reads.remove(path);
        }
        self.reads.entry(path.to_path_buf()).or_insert_with(|| {
            let context = read(path, nonce).ok();
            #[cfg(any(test, feature = "test-support"))]
            tests::point(tests::Stage::AfterRead, path);
            let after = file_stamp(path);
            let unavailable = context.is_none() || before.is_none() || after != before;
            Observed {
                stamp: before,
                context,
                unavailable,
            }
        })
    }

    pub(super) fn changed(&self) -> HashSet<PathBuf> {
        self.stamps
            .iter()
            .filter_map(|(path, before)| {
                #[cfg(any(test, feature = "test-support"))]
                tests::point(tests::Stage::BeforeValidate, path);
                if file_stamp(path) != *before {
                    Some(path.clone())
                } else {
                    None
                }
            })
            .collect()
    }
}

fn read(_path: &Path, nonce: &str) -> Result<BindingContext, String> {
    #[cfg(any(test, feature = "test-support"))]
    tests::read(_path);
    binding_context::execution_context("claude", nonce)
}
