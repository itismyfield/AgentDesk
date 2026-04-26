use poise::serenity_prelude::{ChannelId, MessageId};

use crate::services::provider::ProviderKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlaceholderCleanupOperation {
    Delete,
    EditTerminal,
    EditHandoff,
}

impl PlaceholderCleanupOperation {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Delete => "delete",
            Self::EditTerminal => "edit_terminal",
            Self::EditHandoff => "edit_handoff",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlaceholderCleanupFailureClass {
    PermissionOrRoutingDiagnostic,
    LifecycleFailure,
}

impl PlaceholderCleanupFailureClass {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::PermissionOrRoutingDiagnostic => "permission_or_routing_diagnostic",
            Self::LifecycleFailure => "lifecycle_failure",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PlaceholderCleanupOutcome {
    Succeeded,
    AlreadyGone,
    Failed {
        class: PlaceholderCleanupFailureClass,
        detail: String,
    },
}

impl PlaceholderCleanupOutcome {
    pub(super) fn is_committed(&self) -> bool {
        matches!(self, Self::Succeeded | Self::AlreadyGone)
    }

    pub(super) fn failed(detail: impl Into<String>) -> Self {
        let detail = detail.into();
        Self::Failed {
            class: classify_cleanup_failure(&detail),
            detail,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PlaceholderCleanupRecord {
    pub(super) provider: ProviderKind,
    pub(super) channel_id: ChannelId,
    pub(super) message_id: MessageId,
    pub(super) tmux_session_name: Option<String>,
    pub(super) operation: PlaceholderCleanupOperation,
    pub(super) outcome: PlaceholderCleanupOutcome,
    pub(super) source: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PlaceholderCleanupKey {
    provider: String,
    channel_id: ChannelId,
    message_id: MessageId,
}

#[derive(Debug, Default)]
pub(super) struct PlaceholderCleanupRegistry {
    records: dashmap::DashMap<PlaceholderCleanupKey, PlaceholderCleanupRecord>,
}

impl PlaceholderCleanupRegistry {
    pub(super) fn record(&self, record: PlaceholderCleanupRecord) {
        let key = PlaceholderCleanupKey {
            provider: record.provider.as_str().to_string(),
            channel_id: record.channel_id,
            message_id: record.message_id,
        };
        self.records.insert(key, record);
    }

    pub(super) fn terminal_cleanup_committed(
        &self,
        provider: &ProviderKind,
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> bool {
        let key = PlaceholderCleanupKey {
            provider: provider.as_str().to_string(),
            channel_id,
            message_id,
        };
        self.records.get(&key).is_some_and(|record| {
            matches!(
                record.operation,
                PlaceholderCleanupOperation::Delete | PlaceholderCleanupOperation::EditTerminal
            ) && record.outcome.is_committed()
        })
    }

    #[cfg(test)]
    pub(super) fn latest(
        &self,
        provider: &ProviderKind,
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> Option<PlaceholderCleanupRecord> {
        let key = PlaceholderCleanupKey {
            provider: provider.as_str().to_string(),
            channel_id,
            message_id,
        };
        self.records.get(&key).map(|record| record.clone())
    }
}

pub(super) fn classify_cleanup_failure(detail: &str) -> PlaceholderCleanupFailureClass {
    let lower = detail.to_ascii_lowercase();
    if lower.contains("403")
        || lower.contains("forbidden")
        || lower.contains("missing permissions")
        || lower.contains("missing access")
        || lower.contains("not allowed for bot settings")
        || lower.contains("channelnotallowed")
        || lower.contains("agentnotallowed")
        || lower.contains("routing")
        || lower.contains("wrong bot")
    {
        PlaceholderCleanupFailureClass::PermissionOrRoutingDiagnostic
    } else {
        PlaceholderCleanupFailureClass::LifecycleFailure
    }
}

pub(super) fn classify_delete_error(detail: &str) -> PlaceholderCleanupOutcome {
    let lower = detail.to_ascii_lowercase();
    if lower.contains("404") || lower.contains("unknown message") || lower.contains("not found") {
        PlaceholderCleanupOutcome::AlreadyGone
    } else {
        PlaceholderCleanupOutcome::failed(detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_and_routing_errors_are_diagnostics_not_lifecycle_failures() {
        for detail in [
            "HTTP 403 Forbidden: Missing Permissions",
            "not allowed for bot settings",
            "wrong bot routing for provider channel",
        ] {
            assert_eq!(
                classify_cleanup_failure(detail),
                PlaceholderCleanupFailureClass::PermissionOrRoutingDiagnostic,
                "{detail}"
            );
        }
    }

    #[test]
    fn unknown_message_delete_is_already_gone() {
        assert_eq!(
            classify_delete_error("HTTP 404 Unknown Message"),
            PlaceholderCleanupOutcome::AlreadyGone
        );
    }

    #[test]
    fn registry_records_committed_terminal_cleanup() {
        let registry = PlaceholderCleanupRegistry::default();
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(10);
        let message_id = MessageId::new(20);
        registry.record(PlaceholderCleanupRecord {
            provider: provider.clone(),
            channel_id,
            message_id,
            tmux_session_name: Some("AgentDesk-codex-test".to_string()),
            operation: PlaceholderCleanupOperation::Delete,
            outcome: PlaceholderCleanupOutcome::Succeeded,
            source: "test",
        });

        assert!(registry.terminal_cleanup_committed(&provider, channel_id, message_id));
        assert_eq!(
            registry
                .latest(&provider, channel_id, message_id)
                .expect("recorded")
                .operation,
            PlaceholderCleanupOperation::Delete
        );
    }

    #[test]
    fn handoff_edit_does_not_count_as_terminal_cleanup() {
        let registry = PlaceholderCleanupRegistry::default();
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(10);
        let message_id = MessageId::new(20);
        registry.record(PlaceholderCleanupRecord {
            provider: provider.clone(),
            channel_id,
            message_id,
            tmux_session_name: Some("AgentDesk-codex-test".to_string()),
            operation: PlaceholderCleanupOperation::EditHandoff,
            outcome: PlaceholderCleanupOutcome::Succeeded,
            source: "test",
        });

        assert!(!registry.terminal_cleanup_committed(&provider, channel_id, message_id));
    }
}
