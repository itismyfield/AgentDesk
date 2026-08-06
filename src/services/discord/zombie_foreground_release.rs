//! #5176 — cancel succeeds only when mailbox foreground ownership is released.
//!
//! Before this module the three cancel surfaces (`/stop`, the cancel API, the
//! stale-turn reconciler) all defined success as "an interrupt was delivered or
//! deliberately skipped". That definition is exactly inverted for the failure it
//! was supposed to cover. When the mailbox anchors a foreground turn whose
//! runtime is already gone, there is nothing left to interrupt, so the stop
//! delivery layer reports `decision=skip_pre_generation` and every caller above
//! it reports success — while the mailbox keeps the foreground slot forever and
//! every queued user message stays locked behind it.
//!
//! `skip_pre_generation` is not the exception to the zombie case; it IS the
//! zombie case. So the skip must drive a release rather than suppress one.
//!
//! The dangerous direction is the opposite one: releasing the foreground slot of
//! a turn that is genuinely running would abandon live work and let a second turn
//! start on top of it. The release is therefore gated on a conjunction of three
//! INDEPENDENT pieces of terminal evidence, each sourced from a different
//! authority:
//!
//! 1. the mailbox itself says the anchored token was already `cancelled`
//!    (someone asked for this turn to stop; we never release an uncancelled turn),
//! 2. the persistent inflight-turn record for this provider/channel is absent
//!    (no turn-bridge loop owns this turn), and
//! 3. the provider tmux pane is structurally terminal — missing, or
//!    `ReadyForInput` per the shared `tmux_turn_liveness` authority that the
//!    stale-turn reconciler already trusts.
//!
//! A live turn fails (2) — it has an inflight record — and typically fails (3)
//! as well. Ambiguous or unprobeable panes resolve to `LiveOrAmbiguous`, which
//! fails (3). Every hold path is reported with the specific evidence that held
//! it, so an operator never has to guess which gate refused.

use std::path::Path;
use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::tmux_turn_liveness::IndependentTmuxReadiness;

use super::SharedData;

/// Terminal evidence gathered at one cancel boundary. Deliberately plain data:
/// the decision itself is a pure function so both directions (release the
/// zombie / keep the live turn) are unit-testable without a tmux pane, a
/// mailbox actor, or an inflight file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::services::discord) struct ZombieForegroundEvidence {
    /// The mailbox still anchors a foreground turn (`cancel_token.is_some()`).
    pub(in crate::services::discord) mailbox_holds_active_turn: bool,
    /// The anchored token has already been flipped to `cancelled`.
    pub(in crate::services::discord) active_turn_cancelled: bool,
    /// A persistent inflight-turn record exists for this provider/channel.
    pub(in crate::services::discord) inflight_state_present: bool,
    /// The provider tmux pane is structurally terminal (missing / prompt-ready).
    pub(in crate::services::discord) tui_structurally_idle: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ZombieForegroundVerdict {
    /// All three independent terminal evidences agree: release the anchor.
    Release,
    /// Nothing to release — the mailbox is already idle.
    HoldNoActiveTurn,
    /// The anchored turn was never cancelled; releasing it would abandon
    /// live work that nobody asked to stop.
    HoldTurnNotCancelled,
    /// A turn-bridge loop still owns this turn.
    HoldInflightPresent,
    /// The provider pane is streaming, or could not be probed.
    HoldTuiNotIdle,
}

impl ZombieForegroundVerdict {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::HoldNoActiveTurn => "hold_no_active_turn",
            Self::HoldTurnNotCancelled => "hold_turn_not_cancelled",
            Self::HoldInflightPresent => "hold_inflight_present",
            Self::HoldTuiNotIdle => "hold_tui_not_idle",
        }
    }

    pub(crate) const fn is_release(self) -> bool {
        matches!(self, Self::Release)
    }
}

/// The whole guard, as a pure function.
///
/// Evaluation order is chosen so the reported hold reason is the STRONGEST
/// evidence against releasing: an inflight record outranks a busy pane, because
/// a busy pane can be a sibling process while an inflight record is this turn's
/// own liveness claim.
pub(in crate::services::discord) fn classify_zombie_foreground(
    evidence: ZombieForegroundEvidence,
) -> ZombieForegroundVerdict {
    if terminal_evidence_allows_mailbox_release(
        evidence.mailbox_holds_active_turn,
        evidence.active_turn_cancelled,
        evidence.inflight_state_present,
        evidence.tui_structurally_idle,
    ) {
        return ZombieForegroundVerdict::Release;
    }
    if !evidence.mailbox_holds_active_turn {
        return ZombieForegroundVerdict::HoldNoActiveTurn;
    }
    if !evidence.active_turn_cancelled {
        return ZombieForegroundVerdict::HoldTurnNotCancelled;
    }
    if evidence.inflight_state_present {
        return ZombieForegroundVerdict::HoldInflightPresent;
    }
    if !evidence.tui_structurally_idle {
        return ZombieForegroundVerdict::HoldTuiNotIdle;
    }
    // Dead by construction. Staying total keeps the P0 relay cancel path free of
    // a panic point, and the fall-through HOLDS rather than releasing: reaching
    // it can only mean a conjunct was added above without its negation arm, i.e.
    // evidence this function cannot account for — and unaccounted evidence is
    // fail-closed everywhere else in #5068. `debug_assert!` makes that loud in
    // tests; release builds hold and retry on the next tick.
    debug_assert!(
        false,
        "classify_zombie_foreground negation chain is not exhaustive"
    );
    ZombieForegroundVerdict::HoldTuiNotIdle
}

/// Shared #5176 terminal-evidence conjunction.
///
/// Cancel surfaces pass `release_authorized = token.cancelled`.  The #5068
/// finalizer reconciler passes an exact episode-nonce match between the
/// terminal submission and the residual mailbox token.  Both callers must
/// independently prove that no inflight bridge owns the turn and that the TUI
/// is structurally terminal. Keeping the conjunction here prevents the two
/// recovery surfaces from drifting toward different definitions of a zombie.
pub(in crate::services::discord) const fn terminal_evidence_allows_mailbox_release(
    mailbox_holds_active_turn: bool,
    release_authorized: bool,
    inflight_state_present: bool,
    tui_structurally_idle: bool,
) -> bool {
    mailbox_holds_active_turn
        && release_authorized
        && !inflight_state_present
        && tui_structurally_idle
}

/// What a cancel surface actually accomplished at the mailbox.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ZombieForegroundReleaseOutcome {
    pub(crate) verdict: Option<ZombieForegroundVerdict>,
    /// `true` only when this call took the foreground anchor away from the
    /// mailbox. This — not "an interrupt was sent" — is cancel success.
    pub(crate) released: bool,
    /// Queued interventions still parked behind the released anchor.
    pub(crate) queue_depth_after: usize,
    /// A deferred kickoff was armed so the survived queue is promoted rather
    /// than left parked behind a now-idle mailbox.
    pub(crate) queue_kickoff_scheduled: bool,
}

impl ZombieForegroundReleaseOutcome {
    pub(crate) fn verdict_str(&self) -> &'static str {
        self.verdict
            .map_or("not_probed", ZombieForegroundVerdict::as_str)
    }
}

/// `Missing` is terminal evidence, not an error: a foreground turn whose tmux
/// session no longer exists cannot be running.
fn readiness_is_structurally_idle(readiness: IndependentTmuxReadiness) -> bool {
    matches!(
        readiness,
        IndependentTmuxReadiness::Missing | IndependentTmuxReadiness::ReadyForInput
    )
}

/// Probe the provider pane through the same authority the stale-turn
/// reconciler uses, so the two surfaces can never disagree about what "idle"
/// means. A token with no bound tmux session has no pane to contradict the
/// other two evidences, so it is treated as structurally idle — the inflight
/// and cancelled gates still have to pass.
pub(in crate::services::discord) fn tui_structurally_idle(
    provider: &ProviderKind,
    token: &Arc<CancelToken>,
) -> bool {
    let Some(tmux_session) = token
        .tmux_session_name()
        .filter(|session| !session.trim().is_empty())
    else {
        return true;
    };
    let runtime_kind =
        crate::services::tmux_common::resolve_tmux_runtime_kind_marker(&tmux_session);
    let output_path =
        crate::services::tmux_common::resolve_session_temp_path(&tmux_session, "jsonl");
    readiness_is_structurally_idle(
        crate::services::tmux_turn_liveness::independent_tmux_readiness(
            &tmux_session,
            provider,
            runtime_kind,
            output_path.as_deref().map(Path::new),
            None,
        ),
    )
}

/// #5176 — is a persistent inflight-turn record still claiming this tmux
/// session? The stale-turn reconciler needs the same "no turn-bridge loop owns
/// this" evidence the release guard uses, but a session key carries a tmux name
/// rather than a Discord channel id, so presence is resolved by tmux name there.
/// `rebind_origin` rows are synthetic reattach markers, not a running turn, and
/// must not read as liveness.
pub(crate) fn inflight_state_present_for_tmux_name(
    provider: &ProviderKind,
    tmux_name: &str,
) -> bool {
    super::inflight::load_inflight_states(provider)
        .into_iter()
        .any(|state| !state.rebind_origin && state.tmux_session_name.as_deref() == Some(tmux_name))
}

pub(in crate::services::discord) fn collect_zombie_foreground_evidence(
    provider: &ProviderKind,
    channel_id: ChannelId,
    active_token: Option<&Arc<CancelToken>>,
) -> ZombieForegroundEvidence {
    let Some(token) = active_token else {
        return ZombieForegroundEvidence {
            mailbox_holds_active_turn: false,
            active_turn_cancelled: false,
            inflight_state_present: false,
            tui_structurally_idle: false,
        };
    };
    ZombieForegroundEvidence {
        mailbox_holds_active_turn: true,
        active_turn_cancelled: token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
        inflight_state_present: super::inflight::inflight_state_file_exists(
            provider,
            channel_id.get(),
        ),
        tui_structurally_idle: tui_structurally_idle(provider, token),
    }
}

/// Release a zombie foreground turn, or explain which evidence held it.
///
/// Every cancel surface calls this AFTER its own stop attempt, so a cancel that
/// had nothing to interrupt still ends with the mailbox actually free. The
/// underlying `finish_cancelled_turn` mailbox message carries its own
/// last-line-of-defence guard (it refuses to finalize an uncancelled token), so
/// even a mis-evaluated evidence set cannot take a fresh turn's anchor.
pub(crate) async fn release_zombie_foreground_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    surface: &str,
) -> ZombieForegroundReleaseOutcome {
    let Some(handle) = shared.mailbox_peek(channel_id) else {
        return ZombieForegroundReleaseOutcome::default();
    };
    let snapshot = handle.snapshot().await;
    let evidence =
        collect_zombie_foreground_evidence(provider, channel_id, snapshot.cancel_token.as_ref());
    let verdict = classify_zombie_foreground(evidence);

    if !verdict.is_release() {
        tracing::debug!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            surface,
            verdict = verdict.as_str(),
            mailbox_holds_active_turn = evidence.mailbox_holds_active_turn,
            active_turn_cancelled = evidence.active_turn_cancelled,
            inflight_state_present = evidence.inflight_state_present,
            tui_structurally_idle = evidence.tui_structurally_idle,
            "[zombie-foreground] mailbox foreground ownership held"
        );
        return ZombieForegroundReleaseOutcome {
            verdict: Some(verdict),
            released: false,
            queue_depth_after: snapshot.intervention_queue.len(),
            queue_kickoff_scheduled: false,
        };
    }

    let finish = super::mailbox_finish_cancelled_turn(shared, channel_id).await;
    let released = finish.removed_token.is_some();
    if released {
        super::saturating_decrement_global_active(shared);
    }
    let queue_depth_after = handle.snapshot().await.intervention_queue.len();

    // The queued user messages must not merely survive the release — the
    // channel is idle now, so nothing else will come along to promote them.
    let queue_kickoff_scheduled = released && finish.mailbox_online && finish.has_pending;
    if queue_kickoff_scheduled {
        super::schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            provider.clone(),
            channel_id,
            "zombie_foreground_release",
        );
    }

    tracing::warn!(
        provider = provider.as_str(),
        channel_id = channel_id.get(),
        surface,
        verdict = verdict.as_str(),
        released,
        queue_depth_after,
        queue_kickoff_scheduled,
        inflight_state_present = evidence.inflight_state_present,
        tui_structurally_idle = evidence.tui_structurally_idle,
        "[zombie-foreground] released mailbox foreground ownership after a cancel with nothing to interrupt"
    );

    ZombieForegroundReleaseOutcome {
        verdict: Some(verdict),
        released,
        queue_depth_after,
        queue_kickoff_scheduled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zombie() -> ZombieForegroundEvidence {
        ZombieForegroundEvidence {
            mailbox_holds_active_turn: true,
            active_turn_cancelled: true,
            inflight_state_present: false,
            tui_structurally_idle: true,
        }
    }

    /// The exact production shape from #5176: `active_turn=foreground`,
    /// `mailbox_has_cancel_token=true`, `inflight_state_present=false`, TUI
    /// parked at the `❯` prompt. This is the configuration in which `/stop`
    /// reported `decision=skip_pre_generation` + `status="sent"` and changed
    /// nothing.
    #[test]
    fn issue_5176_zombie_shape_releases() {
        assert_eq!(
            classify_zombie_foreground(zombie()),
            ZombieForegroundVerdict::Release
        );
    }

    /// The counter-direction, which is the real risk of this fix. Each of the
    /// three gates must independently be able to save a live turn.
    #[test]
    fn live_turn_is_never_released_by_any_single_missing_evidence() {
        let inflight_running = ZombieForegroundEvidence {
            inflight_state_present: true,
            ..zombie()
        };
        assert_eq!(
            classify_zombie_foreground(inflight_running),
            ZombieForegroundVerdict::HoldInflightPresent,
            "a turn-bridge loop still owns this turn"
        );

        let streaming_pane = ZombieForegroundEvidence {
            tui_structurally_idle: false,
            ..zombie()
        };
        assert_eq!(
            classify_zombie_foreground(streaming_pane),
            ZombieForegroundVerdict::HoldTuiNotIdle,
            "a streaming or unprobeable pane is not terminal evidence"
        );

        let not_cancelled = ZombieForegroundEvidence {
            active_turn_cancelled: false,
            ..zombie()
        };
        assert_eq!(
            classify_zombie_foreground(not_cancelled),
            ZombieForegroundVerdict::HoldTurnNotCancelled,
            "nobody asked this turn to stop"
        );

        // A turn that is running hard: every gate says live.
        let fully_live = ZombieForegroundEvidence {
            mailbox_holds_active_turn: true,
            active_turn_cancelled: false,
            inflight_state_present: true,
            tui_structurally_idle: false,
        };
        assert!(!classify_zombie_foreground(fully_live).is_release());
    }

    #[test]
    fn idle_mailbox_is_a_hold_not_a_release() {
        let idle = ZombieForegroundEvidence {
            mailbox_holds_active_turn: false,
            active_turn_cancelled: false,
            inflight_state_present: false,
            tui_structurally_idle: true,
        };
        assert_eq!(
            classify_zombie_foreground(idle),
            ZombieForegroundVerdict::HoldNoActiveTurn
        );
        assert_eq!(
            collect_zombie_foreground_evidence(&ProviderKind::Claude, ChannelId::new(1), None),
            ZombieForegroundEvidence {
                mailbox_holds_active_turn: false,
                active_turn_cancelled: false,
                inflight_state_present: false,
                tui_structurally_idle: false,
            }
        );
    }

    #[test]
    fn only_terminal_tmux_readiness_counts_as_structurally_idle() {
        assert!(readiness_is_structurally_idle(
            IndependentTmuxReadiness::Missing
        ));
        assert!(readiness_is_structurally_idle(
            IndependentTmuxReadiness::ReadyForInput
        ));
        assert!(
            !readiness_is_structurally_idle(IndependentTmuxReadiness::LiveOrAmbiguous),
            "probe failure and streaming panes must fail closed"
        );
    }

    mod fixtures {
        use std::sync::Arc;
        use std::sync::atomic::Ordering;
        use std::time::Instant;

        use poise::serenity_prelude::{ChannelId, MessageId, UserId};

        use crate::services::discord::inflight::InflightTurnState;
        use crate::services::provider::{CancelToken, ProviderKind};
        use crate::services::turn_orchestrator::{Intervention, InterventionMode};

        use super::super::{ZombieForegroundVerdict, release_zombie_foreground_turn};

        const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

        /// Isolate the runtime root (inflight records + pending-queue files) for
        /// one test. `TestEnvVarGuard` is the canonical serialization path here:
        /// it owns the shared test-env mutex for its whole lifetime, so the root
        /// stays set across the awaits without a bare lock guard on the stack.
        fn isolated_runtime_root(tmp: &tempfile::TempDir) -> crate::config::TestEnvVarGuard {
            crate::config::TestEnvVarGuard::set_path(AGENTDESK_ROOT_DIR_ENV, tmp.path())
        }

        fn queued_user_message(message_id: u64) -> Intervention {
            Intervention {
                author_id: UserId::new(1),
                author_is_bot: false,
                message_id: MessageId::new(message_id),
                queued_generation: crate::services::discord::runtime_store::process_generation(),
                source_message_ids: vec![MessageId::new(message_id)],
                source_message_queued_generations: Vec::new(),
                source_text_segments: Vec::new(),
                text: "the instruction that #5176 locked behind a zombie".to_string(),
                mode: InterventionMode::Soft,
                created_at: Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
                pending_uploads: Vec::new(),
                voice_announcement: None,
            }
        }

        fn inflight_row(channel_id: ChannelId) -> InflightTurnState {
            InflightTurnState::new(
                ProviderKind::Claude,
                channel_id.get(),
                Some("adk-cc".to_string()),
                7,
                700,
                701,
                "a genuinely running turn".to_string(),
                Some("session".to_string()),
                Some("AgentDesk-claude-5176-live".to_string()),
                Some("/tmp/claude-transcript.jsonl".to_string()),
                None,
                0,
            )
        }

        /// #5176 reproduction fixture, shared by the release direction and the
        /// hold direction so the only difference between them is the evidence
        /// under test.
        ///
        /// Shape: `mailbox_has_cancel_token=true` (foreground anchored),
        /// `inflight_state_present=false` (isolated runtime root, so no
        /// inflight row exists), TUI structurally idle (no tmux session bound
        /// to the token), plus one queued user message — the message the real
        /// incident kept locked for a week.
        async fn seed_zombie_channel(
            shared: &Arc<crate::services::discord::SharedData>,
            provider: &ProviderKind,
            channel_id: ChannelId,
            cancelled: bool,
        ) -> Arc<CancelToken> {
            let token = Arc::new(CancelToken::new());
            token.cancelled.store(cancelled, Ordering::Relaxed);
            assert!(
                shared
                    .mailbox(channel_id)
                    .try_start_turn(token.clone(), UserId::new(7), MessageId::new(70))
                    .await,
                "fixture must own the mailbox foreground slot"
            );
            shared
                .mailbox(channel_id)
                .replace_queue(
                    vec![queued_user_message(9_001)],
                    crate::services::discord::queue_persistence_context(
                        shared, provider, channel_id,
                    ),
                )
                .await;
            token
        }

        /// The zombie is actually released, AND the queued user message
        /// survives the release and gets a promotion scheduled. `/stop` and the
        /// cancel API both reach this code through
        /// `release_zombie_foreground_turn`, so pinning it here pins both.
        #[tokio::test]
        async fn zombie_foreground_turn_is_released_and_the_queue_survives() {
            let tmp = tempfile::tempdir().unwrap();
            let _root = isolated_runtime_root(&tmp);

            let provider = ProviderKind::Claude;
            let channel_id = ChannelId::new(5_176_001);
            let shared = crate::services::discord::make_shared_data_for_tests();
            let token = seed_zombie_channel(&shared, &provider, channel_id, true).await;

            let outcome =
                release_zombie_foreground_turn(&shared, &provider, channel_id, "test/stop").await;

            assert_eq!(outcome.verdict, Some(ZombieForegroundVerdict::Release));
            assert!(
                outcome.released,
                "cancel success means the mailbox let go of the foreground slot"
            );
            let snapshot = shared.mailbox(channel_id).snapshot().await;
            assert!(
                snapshot.cancel_token.is_none(),
                "mailbox must no longer anchor the zombie turn"
            );
            assert!(snapshot.active_user_message_id.is_none());
            assert_eq!(
                snapshot.intervention_queue.len(),
                1,
                "the queued user message must NOT be discarded by the release"
            );
            assert_eq!(outcome.queue_depth_after, 1);
            assert!(
                outcome.queue_kickoff_scheduled,
                "an idle mailbox with a backlog must arm a promotion, or the message stays parked"
            );
            drop(token);
        }

        /// The dangerous direction. A turn that nobody cancelled keeps its
        /// foreground slot even though the other two evidences look terminal.
        #[tokio::test]
        async fn uncancelled_live_turn_keeps_its_foreground_slot() {
            let tmp = tempfile::tempdir().unwrap();
            let _root = isolated_runtime_root(&tmp);

            let provider = ProviderKind::Claude;
            let channel_id = ChannelId::new(5_176_002);
            let shared = crate::services::discord::make_shared_data_for_tests();
            let token = seed_zombie_channel(&shared, &provider, channel_id, false).await;

            let outcome =
                release_zombie_foreground_turn(&shared, &provider, channel_id, "test/stop").await;

            assert_eq!(
                outcome.verdict,
                Some(ZombieForegroundVerdict::HoldTurnNotCancelled)
            );
            assert!(!outcome.released);
            let snapshot = shared.mailbox(channel_id).snapshot().await;
            assert!(
                snapshot
                    .cancel_token
                    .as_ref()
                    .is_some_and(|active| Arc::ptr_eq(active, &token)),
                "a turn nobody asked to stop must survive the release probe"
            );
        }

        /// The other dangerous direction, and the one that matters most: a
        /// CANCELLED turn whose turn-bridge loop is still alive (inflight row
        /// present) must keep its foreground slot, because that loop will run
        /// the canonical exit itself. Releasing here would let a second turn
        /// start on top of a running one.
        #[tokio::test]
        async fn cancelled_turn_with_live_inflight_keeps_its_foreground_slot() {
            let tmp = tempfile::tempdir().unwrap();
            let _root = isolated_runtime_root(&tmp);

            let provider = ProviderKind::Claude;
            let channel_id = ChannelId::new(5_176_003);
            let shared = crate::services::discord::make_shared_data_for_tests();
            let token = seed_zombie_channel(&shared, &provider, channel_id, true).await;
            crate::services::discord::inflight::save_inflight_state(&inflight_row(channel_id))
                .expect("seed a live inflight turn row");

            let outcome =
                release_zombie_foreground_turn(&shared, &provider, channel_id, "test/stop").await;

            assert_eq!(
                outcome.verdict,
                Some(ZombieForegroundVerdict::HoldInflightPresent)
            );
            assert!(!outcome.released);
            assert!(
                shared
                    .mailbox(channel_id)
                    .snapshot()
                    .await
                    .cancel_token
                    .as_ref()
                    .is_some_and(|active| Arc::ptr_eq(active, &token)),
                "a live turn-bridge loop owns the canonical exit; the release must not steal it"
            );
        }
    }

    #[test]
    fn verdict_labels_are_stable_for_operator_logs() {
        assert_eq!(ZombieForegroundVerdict::Release.as_str(), "release");
        assert_eq!(
            ZombieForegroundVerdict::HoldInflightPresent.as_str(),
            "hold_inflight_present"
        );
        assert_eq!(
            ZombieForegroundReleaseOutcome::default().verdict_str(),
            "not_probed"
        );
    }
}
