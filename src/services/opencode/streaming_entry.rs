use std::sync::Arc;
use std::sync::mpsc::Sender;

use crate::services::agent_protocol::StreamMessage;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::remote::RemoteProfile;

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
