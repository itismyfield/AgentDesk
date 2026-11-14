//! Turn-local context of a headless routine run. The run shares its agent's primary channel
//! mailbox but none of that channel's session: it cold-starts under its own per-run session key
//! and neither reads nor writes the channel's session, uploads, clear boundary or binding.
use super::*;

/// Launch metadata key carrying the run's pinned execution mode.
const RUN_MODE_KEY: &str = "routine_run_mode";
const HEADLESS: &str = "headless";

pub(super) const STRATEGY_REASON: &str = "isolated_routine_run";

pub(super) struct IsolatedRoutineProcess {
    current_path: String,
}

fn refused(reason: &str) -> HeadlessTurnStartError {
    HeadlessTurnStartError::InvalidTarget(format!("headless routine run refused: {reason}"))
}

/// Whether the launch metadata marks a headless run; an unreadable mark is refused.
fn marked_headless(metadata: Option<&serde_json::Value>) -> Result<bool, HeadlessTurnStartError> {
    match metadata.and_then(|metadata| metadata.get(RUN_MODE_KEY)) {
        None | Some(serde_json::Value::Null) => Ok(false),
        Some(serde_json::Value::String(mode)) => Ok(mode == HEADLESS),
        Some(_) => Err(refused("run mode is unreadable")),
    }
}

impl IsolatedRoutineProcess {
    /// `None` for every start that is not a headless routine run. A marked run that cannot be
    /// isolated is refused before anything is claimed: it never falls back to the shared session.
    pub(super) async fn resolve(
        (shared, channel_id, provider): (&Arc<SharedData>, ChannelId, &ProviderKind),
        prompt: &str,
        metadata: Option<&serde_json::Value>,
        tmux_session_label: Option<&str>,
        is_dm_hint: Option<bool>,
        workspace: impl FnOnce() -> Option<String>,
    ) -> Result<Option<Self>, HeadlessTurnStartError> {
        if !marked_headless(metadata)? {
            return Ok(None);
        }
        if !routine_metadata::routine_process_turn(metadata) {
            return Err(refused("it is not a fresh process attempt"));
        }
        if is_dm_hint == Some(true) || routine_metadata_agent_id(metadata).is_none() {
            return Err(refused("it needs the agent's own primary channel"));
        }
        // A goal command acts on the channel's own provider session.
        if classify_codex_goal_command_for_provider(provider, prompt, None)
            != GoalCommandKind::NotGoal
        {
            return Err(refused("a goal command targets the channel session"));
        }
        let label = tmux_session_label.map(str::trim).unwrap_or_default();
        let per_run = match label {
            "" => None,
            label => build_adk_session_key(shared, channel_id, provider, Some(label)).await,
        };
        let channel = build_adk_session_key(shared, channel_id, provider, None).await;
        if per_run.is_none() || per_run == channel {
            return Err(refused("it has no session key of its own"));
        }
        let workspace = workspace().ok_or_else(|| refused("no workspace resolved"))?;
        let path = std::path::Path::new(&workspace);
        if !path.is_dir() {
            return Err(refused("its workspace does not exist"));
        }
        let current_path = path
            .canonicalize()
            .map(|path| path.display().to_string())
            .unwrap_or(workspace);
        Ok(Some(Self { current_path }))
    }

    /// Provider session, memento flag and working directory of the cold start.
    pub(super) fn cold_start(&self) -> (Option<String>, bool, String) {
        (None, false, self.current_path.clone())
    }
}

#[cfg(all(test, unix))]
#[path = "isolated_context_tests.rs"]
mod tests;
