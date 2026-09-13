//! Completion-time ownership decision table for channel-scoped effects.
//!
//! The pure table is evaluated from a fresh mailbox read at each effect group:
//! 1. no mailbox handle => [`ChannelEpisodeScope::Unprovable`];
//! 2. mailbox and bridge hold the same token allocation => [`ChannelEpisodeScope::Mine`];
//! 3. mailbox has neither a token nor an active user message => [`ChannelEpisodeScope::Idle`];
//! 4. legacy callers may use equal non-empty nonces => [`ChannelEpisodeScope::Mine`];
//! 5. every other state => [`ChannelEpisodeScope::Foreign`].
//!
//! `Mine` and `Idle` permit effects; `Foreign` and `Unprovable` fail closed. An
//! `Idle` read still has a read-to-effect race and cannot distinguish “no successor”
//! from “a successor already finished.” A nonce-fallback `Mine` proves an episode,
//! not one rehydration attempt, so duplicate actors for the same nonce can both pass.
//! TUI-direct carries its synthetic claim's token allocation into the bridge,
//! preserving the same-actor witness while the mailbox remains owned. After
//! release it reads `Idle` without a successor and `Foreign` with any different
//! allocation, even when a recovery actor reused the nonce. These later reads
//! still guard each channel-scoped effect group.

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

#[cfg(test)]
fn classify_channel_episode(
    snapshot: Option<&ChannelMailboxSnapshot>,
    mine: &Arc<CancelToken>,
    own_nonce: Option<&str>,
) -> ChannelEpisodeDecision {
    classify_channel_episode_with_actor_policy(snapshot, mine, own_nonce, false)
}

fn classify_channel_episode_with_actor_policy(
    snapshot: Option<&ChannelMailboxSnapshot>,
    mine: &Arc<CancelToken>,
    own_nonce: Option<&str>,
    require_captured_actor: bool,
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
    if !require_captured_actor
        && own_nonce
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
    require_captured_actor: bool,
}

impl<'a> ChannelEpisodeProbe<'a> {
    // The actual synthetic actor remains active through terminal transport and
    // projection. Submit its original allocation only after those boundaries
    // settle; an actor-only same-nonce replacement must survive both the first
    // and AlreadyFinalized paths.
    pub(super) async fn finalize_synthetic_actor(
        &self,
        shared_owned: &Arc<SharedData>,
        inflight_state: &InflightTurnState,
        cancelled: bool,
        terminal_projection_committed: bool,
        has_queued_turns: bool,
    ) -> bool {
        if !self.require_captured_actor
            || !terminal_projection_committed
            || !self
                .read("completion_synthetic_finalize")
                .await
                .permits_channel_effects()
        {
            return has_queued_turns;
        }
        let channel_id = self.channel_id;
        let provider = self.provider;
        let cancel_token = &self.mine;
        let outcome = shared_owned
            .turn_finalizer
            .submit_terminal_with_claim_snapshot(
                crate::services::discord::turn_finalizer::TurnKey::new(
                    channel_id,
                    inflight_state.effective_finalizer_turn_id(),
                    shared_owned.restart.current_generation,
                )
                .with_episode_nonce(inflight_state.turn_nonce.as_deref()),
                provider.clone(),
                if cancelled {
                    crate::services::discord::turn_finalizer::TerminalEvent::Cancel
                } else {
                    crate::services::discord::turn_finalizer::TerminalEvent::Complete
                },
                crate::services::discord::turn_finalizer::FinalizeContext::bridge(),
                Some(super::post_loop_finalize::bridge_terminal_claim_snapshot(
                    inflight_state,
                    Some(cancel_token),
                )),
                shared_owned.clone(),
            )
            .await;
        if let crate::services::discord::turn_finalizer::FinalizeOutcome::Finalized {
            has_pending,
            ..
        } = outcome
        {
            has_pending
        } else {
            has_queued_turns
        }
    }

    pub(super) async fn owns_synthetic_cleanup(&self, terminal_delivery_committed: bool) -> bool {
        !self.require_captured_actor
            || (terminal_delivery_committed
                && self
                    .read("completion_synthetic_cleanup")
                    .await
                    .permits_channel_effects())
    }

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
            require_captured_actor: false,
        }
    }

    pub(super) fn requiring_captured_actor(mut self, required: bool) -> Self {
        self.require_captured_actor = required;
        self
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
        let decision = classify_channel_episode_with_actor_policy(
            snapshot.as_ref(),
            &self.mine,
            self.own_nonce.as_deref(),
            self.require_captured_actor,
        );
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
    /// its token-registration contract. TUI-direct bridges retain their synthetic
    /// claim's token, yielding Idle after release or Foreign when a successor has
    /// claimed the retained mailbox handle.
    #[test]
    fn bridge_entry_sites_pin_mailbox_token_registration_contract() {
        let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rs_files(&source_root, &mut files);
        let spawn = ["spawn_turn_", "bridge("].concat();
        let pinned = ["spawn_turn_", "bridge_with_pin("].concat();
        let mut callers = Vec::new();
        for path in files {
            let source = std::fs::read_to_string(&path).expect("read Rust source");
            if path.ends_with("channel_episode_scope.rs") {
                continue;
            }
            if (source.contains(&spawn) || source.contains(&pinned))
                && !source.contains("fn spawn_turn_bridge(")
            {
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
        assert_eq!(tui_direct.matches(&spawn).count(), 0);
        assert_eq!(tui_direct.matches(&pinned).count(), 2);
        assert!(intake.contains("cancel_token.clone(),\n            request_owner"));
        assert!(headless.contains("cancel_token.clone(),\n        request_owner"));
        assert!(recovery.contains("mailbox_recovery_kickoff(\n            shared,\n            channel_id,\n            cancel_token.clone(),"));
        assert_eq!(
            tui_direct
                .matches(concat!(
                    "spawn_turn_bridge_with_pin(\n",
                    "        shared.clone(),\n",
                    "        claim.actor.clone(),\n",
                    "        rx,\n",
                    "        bridge,\n",
                    "        pin,\n",
                    "    );"
                ))
                .count(),
            2,
            "both TUI-direct entries preserve the captured synthetic mailbox actor"
        );
    }
}
