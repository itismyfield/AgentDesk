//! Human-authored Antigravity turns through a resident `agy acp` client (ACP v1),
//! behind `agy_acp.enabled`, read once at boot. Off by default: turns keep `agy --print`.

mod actor;
mod child;
mod rpc;
mod turn;
mod wire;

#[cfg(all(any(test, feature = "test-support"), unix))]
pub mod lifecycle_tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{LazyLock, Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::services::agent_protocol::StreamMessage;
use crate::services::platform::BinaryResolution;
use crate::services::stream_json_cli::ProviderTurnRequest;
use crate::services::stream_json_cli::policy::ToolPolicy;
use crate::services::tui_o::store::agy_spool::{AgySpool, Failure as SpoolFailure};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgyAcpConfig {
    /// Routes human-authored Antigravity turns through `agy acp`; false keeps `agy --print`.
    #[serde(default)]
    pub enabled: bool,
}

impl AgyAcpConfig {
    pub fn is_default(&self) -> bool {
        !self.enabled
    }
}

/// Set once per boot: a config reload never moves running sessions between paths.
pub static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn install(config: &crate::config::Config) {
    ENABLED.store(config.agy_acp.enabled, Ordering::Relaxed);
}

/// How a destination's child is started.
#[derive(Clone, Debug)]
pub struct Launch {
    pub program: PathBuf,
    pub resolution: Option<BinaryResolution>,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub unset_env: Vec<String>,
}

pub struct AcpTurn<'a> {
    pub destination: u64,
    pub request: ProviderTurnRequest,
    /// The turn's provider-session value, returned to turn_bridge byte for byte.
    pub shared_session: Option<&'a str>,
    /// `None` starts the installed `agy acp`.
    pub launch: Option<Launch>,
    pub spool: Option<&'a mut AgySpool>,
}

pub fn run_turn(turn: AcpTurn<'_>, sender: Sender<StreamMessage>) -> Result<(), String> {
    let AcpTurn {
        destination,
        request,
        shared_session,
        launch,
        mut spool,
    } = turn;
    let launch = match launch.map_or_else(|| installed(&request, destination), Ok) {
        Ok(launch) => launch,
        Err(error) => {
            if let Some(spool) = spool.as_deref_mut()
                && let Err(storage) = spool.fail(SpoolFailure::Preparation)
            {
                tracing::warn!(%storage, "agy acp preparation failure could not be recorded");
            }
            return Err(error);
        }
    };
    static NEXT_TURN: AtomicU64 = AtomicU64::new(1);
    let (events, _) = std::sync::mpsc::channel();
    let actor_request = actor::TurnRequest {
        turn: NEXT_TURN.fetch_add(1, Ordering::Relaxed),
        launch,
        text: request.prompt.clone(),
        system: request
            .system_prompt
            .clone()
            .filter(|system| !system.is_empty()),
        uuid_session: spool.is_some(),
        events,
    };
    let outputs = turn::Outputs {
        sender,
        spool,
        shared_session: shared_session.map(str::to_owned),
        cancel: request.cancel.as_deref(),
        conversation: None,
    };
    turn::drive(&actor_for(destination), actor_request, outputs)
}

fn actor_for(destination: u64) -> actor::Handle {
    static ACTORS: LazyLock<Mutex<HashMap<u64, actor::Handle>>> = LazyLock::new(Default::default);
    ACTORS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(destination)
        .or_insert_with(|| actor::spawn(state_dir(destination), actor::DEADLINES))
        .clone()
}

fn state_dir(destination: u64) -> Option<PathBuf> {
    crate::config::runtime_root().map(|root| root.join("agy_acp").join(destination.to_string()))
}

fn installed(request: &ProviderTurnRequest, destination: u64) -> Result<Launch, String> {
    if !matches!(
        request.tool_policy.effective_for_stream_json(),
        ToolPolicy::ProviderDefault
    ) {
        return Err(
            "UnsupportedToolPolicy: Antigravity restricted tool policy is not proven".into(),
        );
    }
    let resolution = crate::services::stream_json_cli::dialects::agy::resolve_agy_binary();
    let program = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Antigravity CLI (agy) not found".to_string())?;
    let mut env = crate::services::provider_auth_profile::overlay_env_pairs(&request.auth_overlay);
    env.push(("AGY_CLI_DISABLE_AUTO_UPDATE".into(), "1".into()));
    if let Some(tmp) = state_dir(destination).map(|dir| dir.join("tmp")) {
        std::fs::create_dir_all(&tmp).map_err(|error| format!("agy acp TMPDIR: {error}"))?;
        env.push(("TMPDIR".into(), tmp.to_string_lossy().into_owned()));
    }
    let args = [
        "acp",
        "--dangerously-skip-permissions",
        "--app-data-dir",
        "antigravity-acp",
    ];
    Ok(Launch {
        program: program.into(),
        resolution: Some(resolution),
        args: args.map(str::to_owned).to_vec(),
        cwd: request.working_directory.clone(),
        env,
        unset_env: crate::services::provider_auth_profile::overlay_unset_keys(
            &request.auth_overlay,
        ),
    })
}
