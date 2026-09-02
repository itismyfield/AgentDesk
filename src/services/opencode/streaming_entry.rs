use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::services::agent_protocol::StreamMessage;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::remote::RemoteProfile;
use crate::services::stream_json_cli::{ConfiguredToolPolicy, ProviderTurnRequest};

pub fn execute_command_streaming(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
    cancel_token: Option<Arc<CancelToken>>,
    remote_profile: Option<&RemoteProfile>,
    tmux_session_name: Option<&str>,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
    model: Option<&str>,
    compact_percent: Option<u64>,
) -> Result<(), String> {
    if matches!(report_provider.as_ref(), Some(ProviderKind::Grok)) {
        return crate::services::stream_json_cli::execute_streaming(
            crate::services::provider::StreamJsonDialectId::Grok,
            ProviderTurnRequest::for_discord_turn(
                ProviderKind::Grok,
                prompt.to_string(),
                system_prompt.map(str::to_string),
                ConfiguredToolPolicy::from_legacy_allowed_tools(allowed_tools.unwrap_or(&[])),
                model.map(str::to_string),
                None,
                PathBuf::from(working_dir),
                session_id,
                false,
                remote_profile.cloned(),
                Duration::from_secs(300),
                cancel_token,
            ),
            sender,
        );
    }

    super::execute_command_streaming_inner(
        prompt,
        session_id,
        working_dir,
        sender,
        system_prompt,
        allowed_tools,
        cancel_token.as_deref(),
        remote_profile,
        tmux_session_name,
        report_channel_id,
        report_provider,
        model,
        compact_percent,
    )
}
