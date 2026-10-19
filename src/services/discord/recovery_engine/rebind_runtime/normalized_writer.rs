use super::*;

pub(in crate::services::discord::recovery_engine) fn write_stream(
    receiver: std::sync::mpsc::Receiver<StreamMessage>,
    already_relayed_response: String,
    already_normalized_replay_events: Vec<serde_json::Value>,
    check: impl Fn() -> Result<(), String>,
    mut write: impl FnMut(
        StreamMessage,
        &mut std::collections::VecDeque<serde_json::Value>,
    ) -> Result<bool, String>,
) -> Result<(), String> {
    let mut already_relayed_response = already_relayed_response;
    let mut known_response_for_done = already_relayed_response.clone();
    let mut already_normalized_replay_events =
        std::collections::VecDeque::from(already_normalized_replay_events);
    for message in receiver {
        check()?;
        let Some(message) =
            codex_rebind_message_after_relayed_prefix(message, &mut already_relayed_response)
        else {
            continue;
        };
        if let crate::services::agent_protocol::StreamMessage::Done { result, .. }
        | crate::services::agent_protocol::StreamMessage::CodexTuiTerminalDone {
            result, ..
        } = &message
            && let Some(suffix) = codex_rebind_done_result_suffix(&known_response_for_done, result)
        {
            let suffix_message = crate::services::agent_protocol::StreamMessage::Text {
                content: suffix.clone(),
            };
            write(suffix_message, &mut already_normalized_replay_events)?;
            known_response_for_done.push_str(&suffix);
        }
        let forwarded_text = match &message {
            crate::services::agent_protocol::StreamMessage::Text { content } => {
                Some(content.clone())
            }
            _ => None,
        };
        write(message, &mut already_normalized_replay_events)?;
        if let Some(text) = forwarded_text {
            known_response_for_done.push_str(&text);
        }
    }
    Ok(())
}
