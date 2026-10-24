//! Read-only Codex evidence. Completeness says what was observed, never what may be retired.

use serde::Serialize;

use crate::services::tmux_common::TmuxSourceAuthority;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;
use crate::services::tui_prompt_dedupe::binding_events::BindingEvent;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum HostPath {
    Legacy,
    Herdr { identity: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Target {
    pub channel: u64,
    pub tmux: String,
    pub nonce: String,
    pub host: String,
    pub owner_runtime_root: String,
    pub host_path: HostPath,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum PaneState {
    Live,
    Dead,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Pane {
    pub name: String,
    pub state: PaneState,
}

pub(crate) type Coverage = Result<(), String>;

/// The observer verifies canonical context, native descriptor and runtime pin under authority.
/// The census covers log panes, current contexts and live host/owner discovery, without TTL filtering.
pub(crate) struct Inputs {
    pub target: Target,
    pub context: BindingContext,
    pub current: SourceId,
    pub events: Vec<BindingEvent>,
    pub tail: u64,
    pub log_digest: String,
    pub census_digest: String,
    pub panes: Vec<Pane>,
    pub coverage: Coverage,
}

pub(crate) trait Observer: Send + Sync {
    fn observe(&self, authority: &TmuxSourceAuthority<'_>) -> Result<Inputs, String>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Snapshot {
    pub target: Target,
    pub context: BindingContext,
    pub current: SourceId,
    pub held: Vec<SourceId>,
    pub tail: u64,
    pub log_digest: String,
    pub census_digest: String,
    pub panes: Vec<Pane>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum Observation {
    Complete(Box<Snapshot>),
    Unknown(String),
}
