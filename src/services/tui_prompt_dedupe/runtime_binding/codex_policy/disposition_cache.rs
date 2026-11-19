//! A pane's source policy is judged again only after one of its inputs moves.
//!
//! The judgement reads the pane's spawn markers, its execution's canonical context, its
//! channel's binding log and its rollout marker. A kept answer stands while each of those files
//! keeps its fingerprint and the pane keeps its source revision, channel and owner root. Only
//! files whose last change is older than [`SETTLED`] are kept, so a write under one coarse clock
//! tick, or one that restores its mtime, cannot hide behind an equal stamp.

use super::SourcePolicyState;
use crate::services::tmux_common::{self as tc, TmuxSourceAuthority};
use crate::services::tui_o::shadow::capture::Fingerprint;
use crate::services::tui_prompt_dedupe::{binding_context, binding_events};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SETTLED: Duration = Duration::from_secs(2);
const KEPT_LIMIT: usize = 256;

#[derive(Clone, PartialEq)]
struct Inputs {
    revision: Option<u64>,
    channel: Option<u64>,
    owner: String,
    files: Vec<(PathBuf, Option<Fingerprint>)>,
}

struct Kept {
    inputs: Inputs,
    nonce: Option<String>,
    state: SourcePolicyState,
}

static KEPT: LazyLock<Mutex<HashMap<String, Kept>>> = LazyLock::new(Default::default);

#[cfg(test)]
thread_local! {
    /// Judgements this cache ran instead of reusing, on this thread.
    pub(super) static JUDGED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How far this thread's clock runs ahead, so a test file can count as settled.
    pub(super) static AHEAD: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
}

fn kept() -> MutexGuard<'static, HashMap<String, Kept>> {
    KEPT.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `judge`'s answer for the pane under `authority`; `judge` also names the execution nonce it
/// read from the spawn marker, whose canonical context is then one of the watched files.
pub(super) fn disposition(
    authority: &TmuxSourceAuthority<'_>,
    channel: Option<u64>,
    judge: impl FnOnce() -> (SourcePolicyState, Option<String>),
) -> SourcePolicyState {
    let tmux = authority.session();
    let previous = kept().get(tmux).map(|kept| kept.nonce.clone());
    if let Some(nonce) = previous
        && let Some((now, _)) = inputs(authority, channel, nonce.as_deref())
        && let Some(state) = kept()
            .get(tmux)
            .filter(|kept| kept.nonce == nonce && kept.inputs == now)
            .map(|kept| kept.state.clone())
    {
        return state;
    }
    #[cfg(test)]
    JUDGED.with(|count| count.set(count.get() + 1));
    let revision = authority.revision();
    let (state, nonce) = judge();
    let mut kept = kept();
    match inputs(authority, channel, nonce.as_deref()) {
        Some((inputs, true)) if inputs.revision == revision => {
            if kept.len() >= KEPT_LIMIT && !kept.contains_key(tmux) {
                kept.clear();
            }
            kept.insert(
                tmux.to_owned(),
                Kept {
                    inputs,
                    nonce,
                    state: state.clone(),
                },
            );
        }
        _ => {
            kept.remove(tmux);
        }
    }
    state
}

/// The pane's inputs now, and whether every present file is settled; `None` if one cannot be
/// stated.
fn inputs(
    authority: &TmuxSourceAuthority<'_>,
    channel: Option<u64>,
    nonce: Option<&str>,
) -> Option<(Inputs, bool)> {
    let tmux = authority.session();
    let mut paths = vec![
        PathBuf::from(tc::session_temp_path(tmux, "spawn_nonce")),
        PathBuf::from(tc::legacy_tmp_session_path(tmux, "spawn_nonce")),
        PathBuf::from(tc::session_temp_path(
            tmux,
            tc::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
        )),
    ];
    if let Some(channel) = channel
        && let Some(log) = binding_events::log_path(channel).ok()?
    {
        paths.push(log);
    }
    if let Some(nonce) = nonce {
        paths.push(binding_context::context_path("codex", nonce).ok()?);
    }
    let now = SystemTime::now();
    #[cfg(test)]
    let now = now + AHEAD.with(std::cell::Cell::get);
    let mut settled = true;
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let stamp = match std::fs::metadata(&path) {
            Ok(meta) => Some(Fingerprint::of(&meta).ok()?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return None,
        };
        settled &= stamp.as_ref().is_none_or(|stamp| at_rest(stamp, now));
        files.push((path, stamp));
    }
    let inputs = Inputs {
        revision: authority.revision(),
        channel,
        owner: tc::current_tmux_owner_marker(),
        files,
    };
    Some((inputs, cfg!(unix) && settled))
}

/// Both the content and the inode last changed at least [`SETTLED`] before `now`.
fn at_rest(stamp: &Fingerprint, now: SystemTime) -> bool {
    let Some(limit) = now.checked_sub(SETTLED) else {
        return false;
    };
    let changed = u64::try_from(stamp.change.0)
        .ok()
        .zip(u32::try_from(stamp.change.1).ok())
        .map(|(secs, nanos)| UNIX_EPOCH + Duration::new(secs, nanos));
    stamp.modified <= limit && changed.is_some_and(|changed| changed <= limit)
}

#[cfg(all(test, unix))]
#[path = "disposition_cache_tests.rs"]
mod tests;
