use poise::serenity_prelude::{ChannelId, MessageId};
use std::time::{Duration, Instant};

use crate::services::provider::ProviderKind;

const PLACEHOLDER_CLEANUP_TTL: Duration = Duration::from_secs(60 * 60);
const PLACEHOLDER_CLEANUP_CAPACITY: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlaceholderCleanupOperation {
    DeleteTerminal,
    DeleteNonterminal,
    EditTerminal,
    EditPreserve,
    EditHandoff,
}

impl PlaceholderCleanupOperation {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::DeleteTerminal => "delete_terminal",
            Self::DeleteNonterminal => "delete_nonterminal",
            Self::EditTerminal => "edit_terminal",
            Self::EditPreserve => "edit_preserve",
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

#[derive(Debug, Clone)]
struct StoredPlaceholderCleanupRecord {
    record: PlaceholderCleanupRecord,
    recorded_at: Instant,
}

#[derive(Debug, Default)]
pub(super) struct PlaceholderCleanupRegistry {
    records: dashmap::DashMap<PlaceholderCleanupKey, StoredPlaceholderCleanupRecord>,
}

impl PlaceholderCleanupRegistry {
    pub(super) fn record(&self, record: PlaceholderCleanupRecord) {
        self.prune_expired();
        let key = PlaceholderCleanupKey {
            provider: record.provider.as_str().to_string(),
            channel_id: record.channel_id,
            message_id: record.message_id,
        };
        self.records.insert(
            key,
            StoredPlaceholderCleanupRecord {
                record,
                recorded_at: Instant::now(),
            },
        );
        self.prune_capacity();
    }

    pub(super) fn terminal_cleanup_committed(
        &self,
        provider: &ProviderKind,
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> bool {
        self.prune_expired();
        let key = PlaceholderCleanupKey {
            provider: provider.as_str().to_string(),
            channel_id,
            message_id,
        };
        self.records.get(&key).is_some_and(|stored| {
            matches!(
                stored.record.operation,
                PlaceholderCleanupOperation::DeleteTerminal
                    | PlaceholderCleanupOperation::EditTerminal
            ) && stored.record.outcome.is_committed()
        })
    }

    pub(super) fn terminal_cleanup_retry_pending(
        &self,
        provider: &ProviderKind,
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> bool {
        self.prune_expired();
        let key = PlaceholderCleanupKey {
            provider: provider.as_str().to_string(),
            channel_id,
            message_id,
        };
        self.records.get(&key).is_some_and(|stored| {
            matches!(
                stored.record.operation,
                PlaceholderCleanupOperation::DeleteTerminal
                    | PlaceholderCleanupOperation::EditTerminal
            ) && matches!(
                stored.record.outcome,
                PlaceholderCleanupOutcome::Failed { .. }
            )
        })
    }

    fn prune_expired(&self) {
        let now = Instant::now();
        self.records
            .retain(|_, stored| now.duration_since(stored.recorded_at) <= PLACEHOLDER_CLEANUP_TTL);
    }

    fn prune_capacity(&self) {
        let excess = self
            .records
            .len()
            .saturating_sub(PLACEHOLDER_CLEANUP_CAPACITY);
        if excess == 0 {
            return;
        }

        let mut oldest: Vec<_> = self
            .records
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().recorded_at))
            .collect();
        oldest.sort_by_key(|(_, recorded_at)| *recorded_at);
        for (key, _) in oldest.into_iter().take(excess) {
            self.records.remove(&key);
        }
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
        self.records.get(&key).map(|stored| stored.record.clone())
    }

    #[cfg(test)]
    fn force_age_for_test(
        &self,
        provider: &ProviderKind,
        channel_id: ChannelId,
        message_id: MessageId,
        age: Duration,
    ) {
        let key = PlaceholderCleanupKey {
            provider: provider.as_str().to_string(),
            channel_id,
            message_id,
        };
        if let Some(mut stored) = self.records.get_mut(&key) {
            stored.recorded_at = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
        }
    }

    #[cfg(test)]
    fn len_for_test(&self) -> usize {
        self.records.len()
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
            operation: PlaceholderCleanupOperation::DeleteTerminal,
            outcome: PlaceholderCleanupOutcome::Succeeded,
            source: "test",
        });

        assert!(registry.terminal_cleanup_committed(&provider, channel_id, message_id));
        assert_eq!(
            registry
                .latest(&provider, channel_id, message_id)
                .expect("recorded")
                .operation,
            PlaceholderCleanupOperation::DeleteTerminal
        );
    }

    #[test]
    fn failed_terminal_cleanup_marks_retry_pending_until_committed_or_expired() {
        let registry = PlaceholderCleanupRegistry::default();
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(10);
        let message_id = MessageId::new(20);

        registry.record(PlaceholderCleanupRecord {
            provider: provider.clone(),
            channel_id,
            message_id,
            tmux_session_name: Some("AgentDesk-codex-test".to_string()),
            operation: PlaceholderCleanupOperation::EditTerminal,
            outcome: PlaceholderCleanupOutcome::failed("HTTP 500 edit failed"),
            source: "test",
        });
        assert!(registry.terminal_cleanup_retry_pending(&provider, channel_id, message_id));

        registry.record(PlaceholderCleanupRecord {
            provider: provider.clone(),
            channel_id,
            message_id,
            tmux_session_name: Some("AgentDesk-codex-test".to_string()),
            operation: PlaceholderCleanupOperation::EditTerminal,
            outcome: PlaceholderCleanupOutcome::Succeeded,
            source: "test",
        });
        assert!(!registry.terminal_cleanup_retry_pending(&provider, channel_id, message_id));

        let expired_message_id = MessageId::new(21);
        registry.record(PlaceholderCleanupRecord {
            provider: provider.clone(),
            channel_id,
            message_id: expired_message_id,
            tmux_session_name: Some("AgentDesk-codex-test".to_string()),
            operation: PlaceholderCleanupOperation::DeleteTerminal,
            outcome: PlaceholderCleanupOutcome::failed("HTTP 500 delete failed"),
            source: "test",
        });
        registry.force_age_for_test(
            &provider,
            channel_id,
            expired_message_id,
            PLACEHOLDER_CLEANUP_TTL + Duration::from_secs(1),
        );
        assert!(!registry.terminal_cleanup_retry_pending(
            &provider,
            channel_id,
            expired_message_id
        ));
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

    #[test]
    fn preserve_edit_and_nonterminal_delete_do_not_count_as_terminal_cleanup() {
        let registry = PlaceholderCleanupRegistry::default();
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(10);

        for (message_id, operation) in [
            (
                MessageId::new(21),
                PlaceholderCleanupOperation::EditPreserve,
            ),
            (
                MessageId::new(22),
                PlaceholderCleanupOperation::DeleteNonterminal,
            ),
        ] {
            registry.record(PlaceholderCleanupRecord {
                provider: provider.clone(),
                channel_id,
                message_id,
                tmux_session_name: Some("AgentDesk-codex-test".to_string()),
                operation,
                outcome: PlaceholderCleanupOutcome::Succeeded,
                source: "test",
            });

            assert!(!registry.terminal_cleanup_committed(&provider, channel_id, message_id));
        }
    }

    #[test]
    fn registry_prunes_expired_and_capacity_records() {
        let registry = PlaceholderCleanupRegistry::default();
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(10);
        let expired_message_id = MessageId::new(20);
        registry.record(PlaceholderCleanupRecord {
            provider: provider.clone(),
            channel_id,
            message_id: expired_message_id,
            tmux_session_name: Some("AgentDesk-codex-test".to_string()),
            operation: PlaceholderCleanupOperation::DeleteTerminal,
            outcome: PlaceholderCleanupOutcome::Succeeded,
            source: "test",
        });
        registry.force_age_for_test(
            &provider,
            channel_id,
            expired_message_id,
            PLACEHOLDER_CLEANUP_TTL + Duration::from_secs(1),
        );
        registry.record(PlaceholderCleanupRecord {
            provider: provider.clone(),
            channel_id,
            message_id: MessageId::new(21),
            tmux_session_name: Some("AgentDesk-codex-test".to_string()),
            operation: PlaceholderCleanupOperation::DeleteTerminal,
            outcome: PlaceholderCleanupOutcome::Succeeded,
            source: "test",
        });
        assert!(
            registry
                .latest(&provider, channel_id, expired_message_id)
                .is_none()
        );

        for offset in 0..(PLACEHOLDER_CLEANUP_CAPACITY + 10) {
            registry.record(PlaceholderCleanupRecord {
                provider: provider.clone(),
                channel_id,
                message_id: MessageId::new(1_000 + offset as u64),
                tmux_session_name: Some("AgentDesk-codex-test".to_string()),
                operation: PlaceholderCleanupOperation::DeleteTerminal,
                outcome: PlaceholderCleanupOutcome::Succeeded,
                source: "test",
            });
        }
        assert!(registry.len_for_test() <= PLACEHOLDER_CLEANUP_CAPACITY);
    }
}
