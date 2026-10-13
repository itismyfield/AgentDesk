//! Episode keys name the one turn an iMessage reached; a channel or a busy reading never does.

use crate::services::provider::ProviderKind;

/// A turn the server started or dequeued, named by the message id it claimed the mailbox with.
pub(crate) fn turn(provider: &ProviderKind, channel_id: u64, user_msg_id: u64) -> String {
    format!("turn:{}:{channel_id}:{user_msg_id}", provider.as_str())
}

/// The durable row of the turn a busy pane is running, as the injection target verified it.
pub(crate) struct Holder<'a> {
    pub session: &'a str,
    pub user_msg_id: u64,
    pub started_at: &'a str,
    pub start_offset: Option<u64>,
    /// The model turn a Codex rollout read as open; None for Claude.
    pub native_turn: Option<&'a str>,
}

/// None when the holder names no single turn: a TUI-direct row (id 0) without its start offset.
pub(crate) fn busy(
    provider: &ProviderKind,
    channel_id: u64,
    holder: &Holder<'_>,
) -> Option<String> {
    let nameable = holder.user_msg_id != 0 || holder.start_offset.is_some();
    if !nameable || holder.session.is_empty() || holder.started_at.is_empty() {
        return None;
    }
    let optional = |value: Option<String>| value.unwrap_or_else(|| "-".to_string());
    Some(format!(
        "busy:{}:{channel_id}:{}:{}:{}:{}:{}",
        provider.as_str(),
        holder.session,
        holder.user_msg_id,
        holder.started_at,
        optional(holder.start_offset.map(|offset| offset.to_string())),
        optional(holder.native_turn.map(str::to_string)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(user_msg_id: u64, start_offset: Option<u64>) -> Holder<'static> {
        Holder {
            session: "AgentDesk-claude-a",
            user_msg_id,
            started_at: "2026-10-13 01:00:00",
            start_offset,
            native_turn: None,
        }
    }

    #[test]
    fn s1b_unidentified_busy_holder_has_no_episode() {
        let claude = ProviderKind::Claude;
        assert_eq!(busy(&claude, 7, &holder(0, None)), None);
        let empty = Holder {
            session: "",
            ..holder(0, Some(10))
        };
        assert_eq!(busy(&claude, 7, &empty), None);
        let direct = busy(&claude, 7, &holder(0, Some(10))).unwrap();
        assert_eq!(
            direct,
            "busy:claude:7:AgentDesk-claude-a:0:2026-10-13 01:00:00:10:-"
        );
        // Consecutive direct turns of one pane differ by their start offset alone.
        assert_ne!(busy(&claude, 7, &holder(0, Some(11))).unwrap(), direct);
        assert_ne!(busy(&claude, 8, &holder(0, Some(10))).unwrap(), direct);
        assert!(busy(&claude, 7, &holder(42, None)).is_some());
        assert_ne!(turn(&claude, 7, 42), turn(&claude, 7, 43));
    }
}
