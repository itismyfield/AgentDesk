//! Whether a Codex pane takes an input, judged from the bound rollout's latest turn since this
//! execution's floor, that turn's Stop and UserPromptSubmit hooks, and the provider process.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::busy_inject::{NativeEnd, read_latest_turn};
use crate::services::claude_tui::activity::processes::Descendants;
use crate::services::claude_tui::hook_server::HookEventKind;
use crate::services::platform::tmux::PaneLiveness;
use crate::services::tui_prompt_dedupe::binding_context::{
    self as context, BindingContext, SpawnNonceMarker,
};

/// UserPromptSubmit evidence with no readable turn start expires once the rollout has been this
/// quiet and the provider process has no child.
const QUIET: Duration =
    Duration::from_secs(crate::services::tui_turn_state::STALE_USER_SUBMITTED_RECLAIM_SECS as u64);
const EVIDENCE_TTL: chrono::Duration = chrono::Duration::hours(24);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The process is alive and the latest turn of this execution is proven ended.
    Idle(Proof),
    Busy(Busy),
    /// Nothing proves an end; never read as ready.
    Unknown(Why),
    /// The provider process is gone.
    Gone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Busy {
    /// The latest turn is open and no valid Stop hook names it.
    OpenTurn(Option<String>),
    /// A UserPromptSubmit hook names a turn whose start the rollout does not show yet.
    PromptSubmitted(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    ProbeError,
    /// The source is being replaced under a verified hold.
    SourceChanging,
    NoSource,
    NoRollout,
    /// The rollout was archived to `.zst`.
    SourceArchived,
    /// A torn line, an unnamed end or an unidentified start.
    Unreadable,
    /// This execution has started no turn; its first input belongs to the launch path.
    NoTurn,
}

impl Why {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ProbeError => "probe_error",
            Self::SourceChanging => "source_changing",
            Self::NoSource => "no_source",
            Self::NoRollout => "no_rollout",
            Self::SourceArchived => "source_archived",
            Self::Unreadable => "unreadable",
            Self::NoTurn => "no_turn",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tier {
    /// The turn's own end record.
    Record,
    /// The Stop hook of the same turn, with no activity recorded after it.
    StopHook,
}

/// What an Idle judgment stood on: the execution, the rollout file and the turn.
#[derive(Clone, Debug)]
pub(crate) struct Proof {
    key: Key,
    file: (u64, u64),
    turn: Option<String>,
    tier: Tier,
}

/// The same execution, file and turn; the end record replacing its Stop hook changes nothing.
impl PartialEq for Proof {
    fn eq(&self, other: &Self) -> bool {
        (&self.key, self.file, &self.turn) == (&other.key, other.file, &other.turn)
    }
}
impl Eq for Proof {}

impl Verdict {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Idle(proof) => match proof.tier {
                Tier::Record => "idle",
                Tier::StopHook => "idle_stop_hook",
            },
            Self::Busy(Busy::OpenTurn(_)) => "busy_open_turn",
            Self::Busy(Busy::PromptSubmitted(_)) => "busy_prompt_submitted",
            Self::Unknown(why) => why.as_str(),
            Self::Gone => "gone",
        }
    }

    /// The turn the judgment stands on, when it names one.
    pub(crate) fn turn(&self) -> Option<&str> {
        match self {
            Self::Idle(proof) => proof.turn.as_deref(),
            Self::Busy(Busy::OpenTurn(turn)) => turn.as_deref(),
            Self::Busy(Busy::PromptSubmitted(turn)) => Some(turn),
            Self::Unknown(_) | Self::Gone => None,
        }
    }
}

/// One execution's rollout: evidence of another execution or another file never applies.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    nonce: Option<String>,
    path: PathBuf,
}

#[derive(Clone, Debug, Default)]
struct Evidence {
    stop: Option<(String, DateTime<Utc>)>,
    prompt: Option<(String, DateTime<Utc>)>,
}

static EVIDENCE: LazyLock<Mutex<HashMap<Key, Evidence>>> = LazyLock::new(Default::default);

fn evidence() -> std::sync::MutexGuard<'static, HashMap<Key, Evidence>> {
    EVIDENCE.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Keeps a Codex Stop or UserPromptSubmit hook as evidence of the turn it names: only one with an
/// accepted binding, the execution's own stored context and a publish time inside that execution.
pub(crate) fn note_hook(
    accepted: bool,
    provider: &str,
    kind: &HookEventKind,
    payload: &Value,
    context: Option<&BindingContext>,
    published: Option<DateTime<Utc>>,
) {
    if !accepted
        || provider != "codex"
        || !matches!(kind, HookEventKind::Stop | HookEventKind::UserPromptSubmit)
    {
        return;
    }
    let (Some(ctx), Some(at)) = (context, published) else {
        return;
    };
    if ctx.provider != "codex"
        || at > Utc::now()
        || at < ctx.created_at
        || !context::execution_context("codex", &ctx.execution_nonce)
            .is_ok_and(|canonical| canonical == *ctx)
    {
        return;
    }
    let turn = crate::services::agent_protocol::codex_payload_turn_id(payload);
    let path = payload["transcript_path"].as_str().map(PathBuf::from);
    let (Some(turn), Some(path)) = (turn, path) else {
        return;
    };
    record(
        Key {
            nonce: Some(ctx.execution_nonce.clone()),
            path,
        },
        kind,
        turn,
        at,
    );
}

fn record(key: Key, kind: &HookEventKind, turn: &str, at: DateTime<Utc>) {
    let mut state = evidence();
    state.retain(|_, entry| {
        [&entry.stop, &entry.prompt]
            .into_iter()
            .flatten()
            .any(|(_, seen)| at.signed_duration_since(*seen) < EVIDENCE_TTL)
    });
    let entry = state.entry(key).or_default();
    let slot = match kind {
        HookEventKind::Stop => &mut entry.stop,
        _ => &mut entry.prompt,
    };
    // A late hook of an older turn never replaces a newer one.
    if slot.as_ref().is_none_or(|(_, seen)| *seen <= at) {
        *slot = Some((turn.to_string(), at));
    }
}

/// What one judgment reads of a pane besides its rollout.
pub(crate) struct Pane<'a> {
    pub liveness: PaneLiveness,
    /// The pane's execution nonce and the time its launch context was created.
    pub nonce: Option<String>,
    pub floor: Option<DateTime<Utc>>,
    /// The bound rollout.
    pub source: PathBuf,
    pub children: &'a dyn Fn() -> Descendants,
}

/// Judges the legacy tmux Codex pane `tmux`. A pane with no bound rollout proves nothing, not even
/// through its process: its caller's own pane read stays the only answer.
pub(crate) fn judge(tmux: &str) -> Verdict {
    judge_in(tmux, None)
}

/// [`judge`] with the cwd the pane's rollout must belong to; `None` reads the pane's own. A bound
/// rollout that another cwd owns is no source.
pub(crate) fn judge_in(tmux: &str, cwd: Option<&str>) -> Verdict {
    if crate::services::tui_prompt_dedupe::codex_verified_input_blocked(tmux) {
        return Verdict::Unknown(Why::SourceChanging);
    }
    let bound = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux)
        .filter(|binding| {
            binding.runtime_kind == crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
        })
        .map(|binding| PathBuf::from(binding.output_path));
    let Some(path) = bound else {
        return Verdict::Unknown(Why::NoSource);
    };
    if path.is_file() {
        let pane = match cwd {
            Some(_) => None,
            None => crate::services::tmux_diagnostics::tmux_session_pane_cwd(tmux),
        };
        let owned = cwd
            .or(pane.as_deref())
            .is_none_or(|cwd| super::rollout_tail::rollout_file_matches_cwd(&path, Path::new(cwd)));
        if !owned {
            return Verdict::Unknown(Why::NoSource);
        }
    }
    // The pane read the input path itself uses; only a clean negative from tmux is Gone.
    let liveness = if super::host_input::legacy_pane_alive(tmux) {
        PaneLiveness::Live
    } else {
        match crate::services::platform::tmux::pane_liveness(tmux) {
            PaneLiveness::DeadOrAbsent => PaneLiveness::DeadOrAbsent,
            _ => PaneLiveness::ProbeError,
        }
    };
    // A pane that names an execution is read from that execution's floor or not at all.
    let (nonce, floor) = match context::observe_spawn_nonce_marker(tmux) {
        SpawnNonceMarker::Known(nonce) => match context::execution_context("codex", &nonce) {
            Ok(ctx) => (Some(nonce), Some(ctx.created_at)),
            Err(_) => return Verdict::Unknown(Why::ProbeError),
        },
        SpawnNonceMarker::Absent => (None, None),
        SpawnNonceMarker::Unreadable => return Verdict::Unknown(Why::ProbeError),
    };
    decide(
        Pane {
            liveness,
            nonce,
            floor,
            source: path,
            children: &|| crate::services::claude_tui::activity::processes::observe(tmux),
        },
        Utc::now(),
    )
}

fn parse(at: Option<&str>) -> Option<DateTime<Utc>> {
    at?.parse::<DateTime<Utc>>().ok()
}

#[cfg(unix)]
fn identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn identity(path: &Path) -> Option<(u64, u64)> {
    std::fs::metadata(path).ok().map(|meta| (0, meta.len()))
}

fn archived(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".zst");
    PathBuf::from(name)
}

/// The rollout, or its archive, has not changed for the quiet window. With neither file the
/// evidence's own age stands in, so evidence of a vanished rollout cannot hold for good.
fn quiet(path: &Path, seen: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    let stamp = [path.to_path_buf(), archived(path)]
        .iter()
        .find_map(|file| std::fs::metadata(file).ok()?.modified().ok());
    match stamp {
        Some(modified) => SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age >= QUIET),
        None => now
            .signed_duration_since(seen)
            .to_std()
            .is_ok_and(|age| age >= QUIET),
    }
}

/// The judgment itself; every read of the world arrives through `pane`.
pub(crate) fn decide(pane: Pane<'_>, now: DateTime<Utc>) -> Verdict {
    match pane.liveness {
        PaneLiveness::Live => {}
        PaneLiveness::DeadOrAbsent => return Verdict::Gone,
        PaneLiveness::ProbeError => return Verdict::Unknown(Why::ProbeError),
    }
    let key = Key {
        nonce: pane.nonce,
        path: pane.source,
    };
    let file = identity(&key.path);
    let read = file.and_then(|_| read_latest_turn(&key.path));
    // A turn that started before this execution's launch context belongs to an earlier one.
    let earlier = read.as_ref().is_some_and(|read| {
        matches!((pane.floor, parse(read.started_at.as_deref())),
            (Some(floor), Some(started)) if started < floor)
    });
    let (end, turn, started, activity) = match &read {
        Some(read) if !earlier => (
            read.end,
            read.turn.clone(),
            parse(read.started_at.as_deref()),
            parse(read.activity_at.as_deref()),
        ),
        _ => (NativeEnd::NoStart, None, None, None),
    };
    let known = evidence().get(&key).cloned().unwrap_or_default();
    if let Some((submitted, seen)) = &known.prompt {
        let shown = turn.as_deref() == Some(submitted.as_str())
            || started.is_some_and(|started| started > *seen)
            || known
                .stop
                .as_ref()
                .is_some_and(|(stopped, _)| stopped == submitted);
        if shown || (quiet(&key.path, *seen, now) && (pane.children)() == Descendants::NoneObserved)
        {
            if let Some(entry) = evidence().get_mut(&key)
                && entry.prompt.as_ref() == Some(&(submitted.clone(), *seen))
            {
                entry.prompt = None;
            }
        } else {
            return Verdict::Busy(Busy::PromptSubmitted(submitted.clone()));
        }
    }
    let Some(file) = file else {
        return Verdict::Unknown(if archived(&key.path).is_file() {
            Why::SourceArchived
        } else {
            Why::NoRollout
        });
    };
    if read.is_none() {
        return Verdict::Unknown(Why::Unreadable);
    }
    // A Stop hook ends the turn it names unless the turn recorded activity after the hook.
    let stopped = turn.as_ref().is_some_and(|turn| {
        known.stop.as_ref().is_some_and(|(stopped, seen)| {
            stopped == turn && activity.is_none_or(|activity| activity <= *seen)
        })
    });
    let idle = |tier| {
        Verdict::Idle(Proof {
            key: key.clone(),
            file,
            turn: turn.clone(),
            tier,
        })
    };
    match end {
        NativeEnd::Ended => idle(Tier::Record),
        NativeEnd::Open | NativeEnd::Unknown if stopped => idle(Tier::StopHook),
        NativeEnd::Open => Verdict::Busy(Busy::OpenTurn(turn)),
        NativeEnd::NoStart => Verdict::Unknown(Why::NoTurn),
        NativeEnd::Unknown => Verdict::Unknown(Why::Unreadable),
    }
}

#[cfg(test)]
#[path = "readiness_tests.rs"]
mod tests;
