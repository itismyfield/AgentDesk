//! Typed Herdr socket API subset (schema protocol 22 / schema_version 1) and
//! the observation model the adapter projects into host results.
#![cfg_attr(not(test), allow(dead_code))]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::services::session_host::model::{HostError, HostKind, HostLiveness, HostPresence};

pub(crate) const HERDR_PROTOCOL: u32 = 22;
pub(crate) const ENDPOINT_MISSING: &str = "endpoint_missing";
/// Server versions whose restore-on-start behavior was measured; protocol 22 alone cannot
/// tell 0.9.0 from 0.9.3, so E7 admits only these pong versions.
pub(crate) const VERIFIED_HERDR_VERSIONS: &[&str] = &["0.9.3"];

/// Where Herdr panes live. Every field is explicit; there is no default socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HerdrEndpoint {
    execution_node: String,
    config_key: String,
    socket_path: PathBuf,
    herdr_session: String,
    /// Directory of the dedicated server's own config; without it E7 never reads Off.
    herdr_home: Option<PathBuf>,
}

impl HerdrEndpoint {
    pub(crate) fn new(
        execution_node: &str,
        config_key: &str,
        socket_path: &Path,
        herdr_session: &str,
    ) -> Result<Self, HostError> {
        let blank = |value: &str| value.trim().is_empty();
        if blank(execution_node)
            || blank(config_key)
            || blank(herdr_session)
            || !socket_path.is_absolute()
        {
            return Err(HostError::Unsupported(HostKind::Herdr, ENDPOINT_MISSING));
        }
        Ok(Self {
            execution_node: execution_node.to_string(),
            config_key: config_key.to_string(),
            socket_path: socket_path.to_path_buf(),
            herdr_session: herdr_session.to_string(),
            herdr_home: None,
        })
    }

    /// The bootstrap home whose `config.toml` and `xdg/` the server must have been started with.
    pub(crate) fn with_herdr_home(self, herdr_home: &Path) -> Result<Self, HostError> {
        if !herdr_home.is_absolute() {
            return Err(HostError::Unsupported(HostKind::Herdr, ENDPOINT_MISSING));
        }
        Ok(Self {
            herdr_home: Some(herdr_home.to_path_buf()),
            ..self
        })
    }

    pub(crate) fn execution_node(&self) -> &str {
        &self.execution_node
    }

    pub(crate) fn config_key(&self) -> &str {
        &self.config_key
    }

    pub(crate) fn herdr_home(&self) -> Option<&Path> {
        self.herdr_home.as_deref()
    }

    pub(crate) fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub(crate) fn herdr_session(&self) -> &str {
        &self.herdr_session
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlPlane {
    Reachable,
    Unreachable,
    Incompatible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneState {
    Present,
    Missing,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecutionState {
    Live,
    Dead,
    Unknown,
}

/// Independent control-plane / pane / execution axes. Herdr has no server epoch:
/// `revision` and `shell_pid` are raw evidence for the identity owner, not a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HerdrObservation {
    pub control_plane: ControlPlane,
    pub pane: PaneState,
    pub execution: ExecutionState,
    pub revision: Option<u64>,
    pub shell_pid: Option<u32>,
}

impl HerdrObservation {
    pub(crate) fn failed(control_plane: ControlPlane) -> Self {
        Self {
            control_plane,
            pane: PaneState::Unknown,
            execution: ExecutionState::Unknown,
            revision: None,
            shell_pid: None,
        }
    }

    pub(crate) fn presence(&self) -> HostPresence {
        if self.control_plane != ControlPlane::Reachable {
            return HostPresence::ProbeFailed;
        }
        match self.pane {
            PaneState::Present => HostPresence::Present,
            PaneState::Missing => HostPresence::Missing,
            PaneState::Unknown => HostPresence::ProbeFailed,
        }
    }

    pub(crate) fn liveness(&self) -> HostLiveness {
        if self.control_plane != ControlPlane::Reachable {
            return HostLiveness::ProbeError;
        }
        match (self.pane, self.execution) {
            (PaneState::Missing, _) => HostLiveness::DeadOrAbsent,
            (PaneState::Present, ExecutionState::Live) => HostLiveness::Live,
            (PaneState::Present, ExecutionState::Dead) => HostLiveness::DeadOrAbsent,
            _ => HostLiveness::ProbeError,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HerdrReadSource {
    Visible,
    Recent,
    RecentUnwrapped,
    Detection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "method", content = "params")]
pub(crate) enum HerdrRequest {
    #[serde(rename = "ping")]
    Ping {},
    #[serde(rename = "session.snapshot")]
    SessionSnapshot {},
    #[serde(rename = "pane.get")]
    PaneGet { pane_id: String },
    #[serde(rename = "pane.process_info")]
    PaneProcessInfo { pane_id: String },
    #[serde(rename = "pane.read")]
    PaneRead {
        pane_id: String,
        source: HerdrReadSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        lines: Option<u32>,
        strip_ansi: bool,
    },
    #[serde(rename = "pane.send_text")]
    PaneSendText { pane_id: String, text: String },
    #[serde(rename = "pane.send_keys")]
    PaneSendKeys { pane_id: String, keys: Vec<String> },
    /// Text then keys as one input; an invalid key makes the server write neither.
    #[serde(rename = "pane.send_input")]
    PaneSendInput {
        pane_id: String,
        text: String,
        keys: Vec<String>,
    },
    /// No command field: the command is sent afterwards as input on the same connection.
    #[serde(rename = "workspace.create")]
    WorkspaceCreate {
        cwd: String,
        label: String,
        focus: bool,
    },
}

impl HerdrRequest {
    /// Only these may be resent: a retry cannot deliver input twice. Anything not
    /// listed, including a request added later, counts as a mutation.
    pub(crate) fn is_read_only(&self) -> bool {
        matches!(
            self,
            Self::Ping {}
                | Self::SessionSnapshot {}
                | Self::PaneGet { .. }
                | Self::PaneProcessInfo { .. }
                | Self::PaneRead { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HerdrCall {
    pub id: String,
    #[serde(flatten)]
    pub request: HerdrRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct HerdrPane {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub revision: u64,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct HerdrSnapshot {
    pub version: String,
    pub protocol: u32,
    pub panes: Vec<HerdrPane>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct HerdrProcessInfo {
    pub pane_id: String,
    pub shell_pid: Option<u32>,
    pub foreground_process_group_id: Option<u32>,
    /// `None` when the server sent no list, which is not the same as an empty one.
    #[serde(default)]
    pub foreground_processes: Option<Vec<HerdrForegroundProcess>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct HerdrForegroundProcess {
    pub pid: u32,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct HerdrRead {
    pub pane_id: String,
    pub source: HerdrReadSource,
    pub text: String,
    pub revision: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum HerdrResult {
    Pong {
        version: String,
        protocol: u32,
    },
    SessionSnapshot {
        snapshot: HerdrSnapshot,
    },
    PaneInfo {
        pane: HerdrPane,
    },
    PaneProcessInfo {
        process_info: HerdrProcessInfo,
    },
    PaneRead {
        read: HerdrRead,
    },
    WorkspaceCreated {
        root_pane: HerdrPane,
    },
    Ok,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct HerdrErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Deserialize)]
struct RawReply {
    id: String,
    result: Option<HerdrResult>,
    error: Option<HerdrErrorBody>,
}

/// One correlated reply: exactly one of `result` / `error`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawReply")]
pub(crate) struct HerdrReply {
    pub id: String,
    pub body: Result<HerdrResult, HerdrErrorBody>,
}

impl TryFrom<RawReply> for HerdrReply {
    type Error = String;

    fn try_from(raw: RawReply) -> Result<Self, Self::Error> {
        let body = match (raw.result, raw.error) {
            (Some(result), None) => Ok(result),
            (None, Some(error)) => Err(error),
            _ => {
                return Err(format!(
                    "reply {} needs exactly one of result/error",
                    raw.id
                ));
            }
        };
        Ok(Self { id: raw.id, body })
    }
}
