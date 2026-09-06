//! Shared provider invocation for user intake and explicit headless turns.
//! Keep their provider selection, cancellation and session options identical.

use std::sync::{Arc, mpsc::Sender};
use std::time::Duration;

use crate::services::agent_protocol::StreamMessage;
use crate::services::provider::{CancelToken, LegacyDispatchKind, ProviderKind};
use crate::services::remote::RemoteProfile;
use crate::services::stream_json_cli::{
    ConfiguredToolPolicy, ProviderTurnRequest, execute_streaming,
};
use crate::services::{claude, codex, gemini, opencode, qwen};

pub(super) struct StreamingTurn<'a> {
    pub provider: &'a ProviderKind,
    pub prompt: &'a str,
    pub session_id: Option<&'a str>,
    pub working_dir: &'a str,
    pub system_prompt: Option<&'a str>,
    pub allowed_tools: &'a [String],
    pub cancel: Arc<CancelToken>,
    pub remote_profile: Option<&'a RemoteProfile>,
    pub tmux_session_name: Option<&'a str>,
    pub channel_id: u64,
    pub model: Option<&'a str>,
    pub native_fast_mode: Option<bool>,
    pub codex_goals: Option<bool>,
    pub compact_percent: Option<u64>,
    pub compact_lower_bound_tokens: u64,
    pub compact_token_limit: Option<u64>,
    pub cache_ttl_minutes: Option<u32>,
    pub dispatch_type: Option<&'a str>,
    pub force_fresh: bool,
}

pub(super) fn execute(
    turn: StreamingTurn<'_>,
    sender: Sender<StreamMessage>,
) -> Result<(), String> {
    match turn.provider.legacy_streaming_dispatch_kind() {
        LegacyDispatchKind::Claude => claude::execute_command_streaming(
            turn.prompt,
            turn.session_id,
            turn.working_dir,
            sender,
            turn.system_prompt,
            Some(turn.allowed_tools),
            Some(turn.cancel),
            turn.remote_profile,
            turn.tmux_session_name,
            Some(turn.channel_id),
            Some(turn.provider.clone()),
            turn.model,
            turn.native_fast_mode,
            turn.compact_percent,
            turn.compact_lower_bound_tokens,
            turn.cache_ttl_minutes,
            turn.dispatch_type,
        ),
        LegacyDispatchKind::Codex => codex::execute_command_streaming(
            turn.prompt,
            turn.session_id,
            turn.working_dir,
            sender,
            turn.system_prompt,
            Some(turn.allowed_tools),
            Some(turn.cancel),
            turn.remote_profile,
            turn.tmux_session_name,
            Some(turn.channel_id),
            Some(turn.provider.clone()),
            turn.model,
            turn.native_fast_mode,
            turn.codex_goals,
            turn.compact_token_limit,
            turn.force_fresh,
        ),
        LegacyDispatchKind::Gemini => gemini::execute_command_streaming(
            turn.prompt,
            turn.session_id,
            turn.working_dir,
            sender,
            turn.system_prompt,
            Some(turn.allowed_tools),
            Some(turn.cancel),
            turn.remote_profile,
            turn.tmux_session_name,
            Some(turn.channel_id),
            Some(turn.provider.clone()),
            turn.model,
            None,
        ),
        LegacyDispatchKind::Qwen => qwen::execute_command_streaming(
            turn.prompt,
            turn.session_id,
            turn.working_dir,
            sender,
            turn.system_prompt,
            Some(turn.allowed_tools),
            Some(turn.cancel),
            turn.remote_profile,
            turn.tmux_session_name,
            Some(turn.channel_id),
            Some(turn.provider.clone()),
            turn.model,
            None,
            turn.force_fresh,
        ),
        LegacyDispatchKind::OpenCode => opencode::execute_command_streaming(
            turn.prompt,
            turn.session_id,
            turn.working_dir,
            sender,
            turn.system_prompt,
            Some(turn.allowed_tools),
            Some(turn.cancel),
            turn.remote_profile,
            turn.tmux_session_name,
            Some(turn.channel_id),
            Some(turn.provider.clone()),
            turn.model,
            None,
        ),
        LegacyDispatchKind::StreamJsonCli(dialect) => {
            execute_streaming(dialect, stream_json_request(&turn), sender)
        }
        LegacyDispatchKind::Unsupported(name) => {
            let _ = sender.send(StreamMessage::Error {
                message: format!("Provider '{}' is not installed", name),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
            });
            Ok(())
        }
    }
}

fn stream_json_request(turn: &StreamingTurn<'_>) -> ProviderTurnRequest {
    ProviderTurnRequest::for_discord_turn(
        turn.provider.clone(),
        turn.prompt.to_string(),
        turn.system_prompt.map(str::to_string),
        ConfiguredToolPolicy::from_legacy_allowed_tools(turn.allowed_tools),
        turn.model.map(str::to_string),
        None,
        turn.working_dir.into(),
        turn.session_id,
        turn.force_fresh,
        turn.remote_profile.cloned(),
        Duration::from_secs(300),
        Some(Arc::clone(&turn.cancel)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agy_discord_dispatch_preserves_turn_identity_policy_and_cancellation() {
        let cancel = Arc::new(CancelToken::new());
        let provider = ProviderKind::from_str("agy").unwrap();
        let tools = vec!["Read".to_string()];
        let mut turn = StreamingTurn {
            provider: &provider,
            prompt: "question",
            session_id: Some("conversation"),
            working_dir: "workspace",
            system_prompt: Some("role"),
            allowed_tools: &tools,
            cancel: Arc::clone(&cancel),
            remote_profile: None,
            tmux_session_name: None,
            channel_id: 42,
            model: Some("configured-model"),
            native_fast_mode: None,
            codex_goals: None,
            compact_percent: None,
            compact_lower_bound_tokens: 0,
            compact_token_limit: None,
            cache_ttl_minutes: None,
            dispatch_type: None,
            force_fresh: false,
        };
        assert!(matches!(
            provider.legacy_streaming_dispatch_kind(),
            LegacyDispatchKind::StreamJsonCli(crate::services::provider::StreamJsonDialectId::Agy)
        ));
        let request = stream_json_request(&turn);
        assert_eq!(request.provider, ProviderKind::Antigravity);
        assert_eq!(request.prompt, "question");
        assert_eq!(request.system_prompt.as_deref(), Some("role"));
        assert_eq!(request.model.as_deref(), Some("configured-model"));
        assert_eq!(
            request.working_directory,
            std::path::PathBuf::from("workspace")
        );
        assert_eq!(
            request.session.as_ref().map(|session| session.as_str()),
            Some("conversation")
        );
        assert_eq!(
            request.tool_policy,
            ConfiguredToolPolicy::from_legacy_allowed_tools(&tools)
        );
        assert!(Arc::ptr_eq(request.cancel.as_ref().unwrap(), &cancel));
        turn.force_fresh = true;
        assert!(stream_json_request(&turn).session.is_none());
    }
}
