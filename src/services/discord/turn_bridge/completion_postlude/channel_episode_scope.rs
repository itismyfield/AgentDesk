//! Completion-time ownership decision table for channel-scoped effects.
//!
//! The pure table is evaluated from a fresh mailbox read at each effect group:
//! 1. no mailbox handle => [`ChannelEpisodeScope::Unprovable`];
//! 2. mailbox and bridge hold the same token allocation => [`ChannelEpisodeScope::Mine`];
//! 3. mailbox has neither a token nor an active user message => [`ChannelEpisodeScope::Idle`];
//! 4. otherwise, equal non-empty durable turn nonces => [`ChannelEpisodeScope::Mine`];
//! 5. every other state => [`ChannelEpisodeScope::Foreign`].
//!
//! `Mine` and `Idle` permit effects; `Foreign` and `Unprovable` fail closed. An
//! `Idle` read still has a read-to-effect race and cannot distinguish “no successor”
//! from “a successor already finished.” A nonce-fallback `Mine` proves an episode,
//! not one rehydration attempt, so duplicate actors for the same nonce can both pass.
//! TUI-direct first creates a synthetic mailbox claim but its bridge mints a separate
//! token. After synthetic release it reads `Idle` when no successor exists and
//! `Foreign` when a successor is active, not normally `Unprovable`. A foreign read
//! can suppress the final status write and leave the namespaced TUI session stuck
//! `TURN_ACTIVE`; this round makes that residue unconditionally visible rather than
//! transporting the synthetic claim witness into the bridge.

use std::sync::Arc;

use super::super::super::relay_recovery::authority_observation;
use super::super::super::{ChannelMailboxSnapshot, SharedData};
use super::{ChannelId, InflightTurnState};
use crate::services::provider::CancelToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChannelEpisodeScope {
    Mine,
    Idle,
    Foreign,
    Unprovable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChannelEpisodeScopeReason {
    TokenAllocation,
    MailboxIdle,
    NonceFallback,
    ForeignEpisode,
    MailboxAbsent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ChannelEpisodeDecision {
    scope: ChannelEpisodeScope,
    reason: ChannelEpisodeScopeReason,
}

impl ChannelEpisodeDecision {
    pub(super) const fn permits_channel_effects(self) -> bool {
        matches!(
            self.scope,
            ChannelEpisodeScope::Mine | ChannelEpisodeScope::Idle
        )
    }

    const fn scope_label(self) -> &'static str {
        match self.scope {
            ChannelEpisodeScope::Mine => "mine",
            ChannelEpisodeScope::Idle => "idle",
            ChannelEpisodeScope::Foreign => "foreign",
            ChannelEpisodeScope::Unprovable => "unprovable",
        }
    }

    const fn reason_label(self) -> &'static str {
        match self.reason {
            ChannelEpisodeScopeReason::TokenAllocation => "token_allocation",
            ChannelEpisodeScopeReason::MailboxIdle => "mailbox_idle",
            ChannelEpisodeScopeReason::NonceFallback => "nonce_fallback",
            ChannelEpisodeScopeReason::ForeignEpisode => "foreign_episode",
            ChannelEpisodeScopeReason::MailboxAbsent => "mailbox_absent",
        }
    }
}

fn classify_channel_episode(
    snapshot: Option<&ChannelMailboxSnapshot>,
    mine: &Arc<CancelToken>,
    own_nonce: Option<&str>,
) -> ChannelEpisodeDecision {
    let Some(snapshot) = snapshot else {
        return ChannelEpisodeDecision {
            scope: ChannelEpisodeScope::Unprovable,
            reason: ChannelEpisodeScopeReason::MailboxAbsent,
        };
    };
    if snapshot
        .cancel_token
        .as_ref()
        .is_some_and(|active| Arc::ptr_eq(mine, active))
    {
        return ChannelEpisodeDecision {
            scope: ChannelEpisodeScope::Mine,
            reason: ChannelEpisodeScopeReason::TokenAllocation,
        };
    }
    if snapshot.cancel_token.is_none() && snapshot.active_user_message_id.is_none() {
        return ChannelEpisodeDecision {
            scope: ChannelEpisodeScope::Idle,
            reason: ChannelEpisodeScopeReason::MailboxIdle,
        };
    }
    if own_nonce
        .filter(|nonce| !nonce.is_empty())
        .is_some_and(|nonce| snapshot.active_turn_nonce.as_deref() == Some(nonce))
    {
        return ChannelEpisodeDecision {
            scope: ChannelEpisodeScope::Mine,
            reason: ChannelEpisodeScopeReason::NonceFallback,
        };
    }
    ChannelEpisodeDecision {
        scope: ChannelEpisodeScope::Foreign,
        reason: ChannelEpisodeScopeReason::ForeignEpisode,
    }
}

pub(super) struct ChannelEpisodeProbe<'a> {
    shared: &'a SharedData,
    channel_id: ChannelId,
    provider: &'a super::ProviderKind,
    turn_id: u64,
    turn_source: &'static str,
    own_nonce: Option<String>,
    mine: Arc<CancelToken>,
}

impl<'a> ChannelEpisodeProbe<'a> {
    pub(super) fn new(
        shared: &'a SharedData,
        channel_id: ChannelId,
        provider: &'a super::ProviderKind,
        state: &InflightTurnState,
        mine: &Arc<CancelToken>,
    ) -> Self {
        Self {
            shared,
            channel_id,
            provider,
            turn_id: state.effective_finalizer_turn_id(),
            turn_source: state.turn_source.as_str(),
            own_nonce: state.turn_nonce.clone(),
            mine: mine.clone(),
        }
    }

    /// Each call is a fresh witness for one effect group. Callers must not reuse it
    /// across await-separated groups; R4 is the explicit accepted exception passed
    /// to the final epilogue and may be stale by the watcher-resume effect.
    pub(super) async fn read(&self, site: &'static str) -> ChannelEpisodeDecision {
        // Deliberately bypass `mailbox_snapshot`: that helper maps an absent handle
        // to `Default`, which is indistinguishable from idle and would fail open.
        let snapshot = match self.shared.mailbox_peek(self.channel_id) {
            Some(handle) => Some(handle.snapshot().await),
            None => None,
        };
        let decision =
            classify_channel_episode(snapshot.as_ref(), &self.mine, self.own_nonce.as_deref());
        authority_observation::record_completion_scope(
            authority_observation::CompletionScopeRecord {
                shared: self.shared,
                provider: self.provider,
                turn_id: self.turn_id,
                channel_id: self.channel_id.get(),
                site,
                turn_source: self.turn_source,
                scope: decision.scope_label(),
                scope_reason: decision.reason_label(),
            },
        );
        if !decision.permits_channel_effects() {
            tracing::warn!(
                target: "agentdesk::relay_authority_completion_suppressed",
                site,
                scope = decision.scope_label(),
                scope_reason = decision.reason_label(),
                turn_source = self.turn_source,
                channel_id = self.channel_id.get(),
                "completion channel effects suppressed"
            );
            authority_observation::record_completion_suppression();
        }
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serenity::all::MessageId;

    fn snapshot(
        cancel_token: Option<Arc<CancelToken>>,
        active_user_message_id: Option<MessageId>,
        active_turn_nonce: Option<&str>,
    ) -> ChannelMailboxSnapshot {
        ChannelMailboxSnapshot {
            cancel_token,
            active_user_message_id,
            active_turn_nonce: active_turn_nonce.map(str::to_owned),
            ..ChannelMailboxSnapshot::default()
        }
    }

    #[test]
    fn five_row_decision_table_is_fail_closed() {
        let mine = Arc::new(CancelToken::new());
        let foreign = Arc::new(CancelToken::new());
        let own_nonce = mine.turn_nonce();

        assert_eq!(
            classify_channel_episode(None, &mine, own_nonce),
            ChannelEpisodeDecision {
                scope: ChannelEpisodeScope::Unprovable,
                reason: ChannelEpisodeScopeReason::MailboxAbsent,
            }
        );
        assert_eq!(
            classify_channel_episode(
                Some(&snapshot(Some(mine.clone()), None, None)),
                &mine,
                own_nonce
            )
            .scope,
            ChannelEpisodeScope::Mine
        );
        assert_eq!(
            classify_channel_episode(Some(&snapshot(None, None, None)), &mine, own_nonce).scope,
            ChannelEpisodeScope::Idle
        );
        assert_eq!(
            classify_channel_episode(
                Some(&snapshot(
                    Some(foreign.clone()),
                    Some(MessageId::new(7)),
                    own_nonce
                )),
                &mine,
                own_nonce,
            )
            .reason,
            ChannelEpisodeScopeReason::NonceFallback
        );
        assert_eq!(
            classify_channel_episode(
                Some(&snapshot(
                    Some(foreign),
                    Some(MessageId::new(8)),
                    Some("other")
                )),
                &mine,
                own_nonce,
            )
            .scope,
            ChannelEpisodeScope::Foreign
        );
    }

    #[tokio::test]
    async fn tui_direct_synthetic_release_is_idle_but_an_active_successor_is_foreign() {
        use crate::services::turn_orchestrator::ActiveTurnKind;
        use serenity::all::{MessageId, UserId};

        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = crate::services::provider::ProviderKind::Claude;
        let channel_id = ChannelId::new(5_464_321);
        let synthetic = Arc::new(CancelToken::new());
        assert!(
            crate::services::discord::mailbox_try_start_turn_kinded(
                &shared,
                channel_id,
                synthetic,
                UserId::new(1),
                MessageId::new(10),
                ActiveTurnKind::Background,
            )
            .await
        );
        crate::services::discord::mailbox_finish_turn(&shared, &provider, channel_id).await;

        let bridge = Arc::new(CancelToken::new());
        let mut state = serde_json::from_value::<InflightTurnState>(serde_json::json!({
            "version": 9,
            "provider": "claude",
            "channel_id": channel_id.get(),
            "channel_name": "adk-claude-test",
            "request_owner_user_id": 1,
            "user_msg_id": 0,
            "current_msg_id": 10,
            "current_msg_len": 0,
            "user_text": "tui-direct",
            "source": "text",
            "session_id": null,
            "tmux_session_name": null,
            "output_path": null,
            "input_fifo_path": null,
            "last_offset": 0,
            "full_response": "",
            "response_sent_offset": 0,
            "started_at": "2026-08-20 00:00:00",
            "updated_at": "2026-08-20 00:00:00"
        }))
        .expect("TUI-direct inflight state");
        state.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
        let probe = ChannelEpisodeProbe::new(&shared, channel_id, &provider, &state, &bridge);
        let idle = probe.read("completion_r0").await;
        assert_eq!(idle.scope, ChannelEpisodeScope::Idle);
        assert!(idle.permits_channel_effects());

        let successor = Arc::new(CancelToken::new());
        assert!(
            crate::services::discord::mailbox_try_start_turn_kinded(
                &shared,
                channel_id,
                successor,
                UserId::new(2),
                MessageId::new(11),
                ActiveTurnKind::UserOrAgent,
            )
            .await
        );
        let before = authority_observation::observation_report();
        let foreign = probe.read("completion_r1").await;
        assert_eq!(foreign.scope, ChannelEpisodeScope::Foreign);
        assert!(!foreign.permits_channel_effects());
        let report = authority_observation::observation_report();
        assert_eq!(
            report.completion_suppressions,
            before.completion_suppressions + 1
        );
        assert_eq!(report.completion_scopes, before.completion_scopes);
    }

    #[test]
    fn empty_nonce_never_proves_ownership() {
        let mine = Arc::new(CancelToken::from_persisted_turn_nonce(None));
        let foreign = Arc::new(CancelToken::new());
        assert_eq!(
            classify_channel_episode(
                Some(&snapshot(Some(foreign), None, Some(""))),
                &mine,
                Some(""),
            )
            .scope,
            ChannelEpisodeScope::Foreign
        );
    }

    fn collect_rs_files(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read Rust source directory") {
            let path = entry.expect("Rust source entry").path();
            if path.is_dir() {
                collect_rs_files(&path, files);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }

    /// Source pin for design L-4. This fixes the complete production caller set and
    /// its token-registration contract. TUI-direct bridges intentionally mint a token
    /// distinct from their preceding synthetic claim, yielding Idle after release or
    /// Foreign when a successor has claimed the retained mailbox handle.
    #[test]
    fn bridge_entry_sites_pin_mailbox_token_registration_contract() {
        let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rs_files(&source_root, &mut files);
        let spawn = ["spawn_turn_", "bridge("].concat();
        let mut callers = Vec::new();
        for path in files {
            let source = std::fs::read_to_string(&path).expect("read Rust source");
            if path.ends_with("channel_episode_scope.rs") {
                continue;
            }
            if source.contains(&spawn) && !source.contains("fn spawn_turn_bridge(") {
                callers.push(
                    path.strip_prefix(&source_root)
                        .expect("source-relative path")
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
        callers.sort();
        assert_eq!(
            callers,
            [
                "services/discord/recovery_engine/restore_inflight.rs",
                "services/discord/router/message_handler/headless_turn.rs",
                "services/discord/router/message_handler/intake_turn.rs",
                "services/discord/tui_prompt_relay/claude_idle_bridge.rs",
            ],
            "every new production bridge caller must declare its mailbox token-registration contract"
        );

        let intake = include_str!("../../router/message_handler/intake_turn.rs");
        let headless = include_str!("../../router/message_handler/headless_turn.rs");
        let recovery = include_str!("../../recovery_engine/restore_inflight.rs");
        let tui_direct = include_str!("../../tui_prompt_relay/claude_idle_bridge.rs");
        assert_eq!(intake.matches(&spawn).count(), 2);
        assert_eq!(headless.matches(&spawn).count(), 1);
        assert_eq!(recovery.matches(&spawn).count(), 1);
        assert_eq!(tui_direct.matches(&spawn).count(), 2);
        assert!(intake.contains("cancel_token.clone(),\n            request_owner"));
        assert!(headless.contains("cancel_token.clone(),\n        request_owner"));
        assert!(recovery.contains("mailbox_recovery_kickoff(\n            shared,\n            channel_id,\n            cancel_token.clone(),"));
        assert_eq!(
            tui_direct
                .matches(
                    "spawn_turn_bridge(shared.clone(), Arc::new(CancelToken::new()), rx, bridge);"
                )
                .count(),
            2,
            "both TUI-direct entries intentionally omit mailbox registration"
        );
    }
}
