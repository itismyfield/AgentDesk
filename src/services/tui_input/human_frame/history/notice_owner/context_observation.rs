//! Launch-context observations belong to one notice-owner refresh.
use super::{ContextObservation, Contexts, NoticeOrigin, Relevance, Stamp, file_stamp};
use crate::services::tui_prompt_dedupe::binding_context::{self, BindingContext};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[cfg(test)]
pub(crate) mod tests;

struct Observed {
    stamp: Option<Stamp>,
    context: Option<BindingContext>,
    unavailable: bool,
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

    fn observe(&mut self, path: &Path, nonce: &str) -> &Observed {
        let before = self.stamp(path);
        if before.as_ref().is_some_and(|stamp| stamp.ino == 0) {
            self.reads.remove(path);
        }
        self.reads.entry(path.to_path_buf()).or_insert_with(|| {
            let context = read(path, nonce).ok();
            #[cfg(test)]
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
                #[cfg(test)]
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
    #[cfg(test)]
    tests::read(_path);
    binding_context::execution_context("claude", nonce)
}

pub(super) fn resumed_execution(
    target: &serde_json::Value,
    origin: &NoticeOrigin,
    channel: u64,
    executions: &mut Contexts,
    frame: Option<&str>,
    observations: &mut Observations,
) -> Relevance {
    let Some(nonce) = target["execution_nonce"].as_str() else {
        return Relevance::Uncertain;
    };
    let Ok(path) = binding_context::context_path("claude", nonce) else {
        return Relevance::Uncertain;
    };
    let read = observations.observe(&path, nonce);
    let observed = executions
        .entry(path)
        .or_insert_with(|| ContextObservation {
            stamp: read.stamp.clone(),
            unavailable: read.unavailable,
            changed: false,
            frames: HashSet::new(),
        });
    observed.unavailable |= read.unavailable;
    if let Some(frame) = frame {
        observed.frames.insert(frame.into());
    }
    if read.unavailable {
        return Relevance::Uncertain;
    }
    let Some(context) = &read.context else {
        return Relevance::Uncertain;
    };
    // The attachment's nonce stays historical; the immutable launch must name this source.
    if target["host"] == "tmux"
        && context.tmux_session == origin.logical_session
        && context.channel_id == Some(channel)
        && target["root"] == context.owner_runtime_root
        && matches!(context.launch_mode.as_str(), "resume" | "fresh")
        && context.expected_native_session_id.as_deref()
            == Some(origin.record.source.session_id.as_str())
    {
        Relevance::Related
    } else {
        Relevance::Unrelated
    }
}
