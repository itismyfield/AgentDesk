use std::time::Instant;

use async_trait::async_trait;
use poise::serenity_prelude as serenity;

use super::super::{Intervention, InterventionMode, MailboxEnqueueOutcome};
use crate::services::turn_orchestrator::EnqueueRefusalReason;
use crate::services::turn_orchestrator::SourceMessageQueuedGeneration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IntakeQueueCommitSource {
    BusyActiveTurn,
    ThreadGuard,
    DispatchGuard,
    ReconcileGate,
    DrainMode,
    IdleBacklog,
}

impl IntakeQueueCommitSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::BusyActiveTurn => "busy_active_turn",
            Self::ThreadGuard => "thread_guard",
            Self::DispatchGuard => "dispatch_guard",
            Self::ReconcileGate => "reconcile_gate",
            Self::DrainMode => "drain_mode",
            Self::IdleBacklog => "idle_backlog",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IntakeQueueAuthorClass {
    Human,
    AllowedBot,
    OtherBot,
}

impl IntakeQueueAuthorClass {
    pub(super) fn from_flags(author_is_bot: bool, is_allowed_bot: bool) -> Self {
        if is_allowed_bot {
            Self::AllowedBot
        } else if author_is_bot {
            Self::OtherBot
        } else {
            Self::Human
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::AllowedBot => "allowed_bot",
            Self::OtherBot => "other_bot",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IntakeQueueIdleKickoffPolicy {
    Never,
    AlwaysAfterAttempt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IntakeQueuePendingReactionPolicy {
    QueueState,
    Static(char),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PendingReactionRepair {
    pub(super) emoji: char,
    pub(super) delivered: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct IntakeQueueCommitOptions {
    pub(super) pending_reaction: IntakeQueuePendingReactionPolicy,
    pub(super) advance_checkpoint: bool,
    pub(super) idle_kickoff: IntakeQueueIdleKickoffPolicy,
}

impl IntakeQueueCommitOptions {
    pub(super) fn idle_backlog() -> Self {
        Self {
            idle_kickoff: IntakeQueueIdleKickoffPolicy::AlwaysAfterAttempt,
            ..Self::default()
        }
    }
}

impl Default for IntakeQueueCommitOptions {
    fn default() -> Self {
        Self {
            pending_reaction: IntakeQueuePendingReactionPolicy::QueueState,
            advance_checkpoint: true,
            idle_kickoff: IntakeQueueIdleKickoffPolicy::Never,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct SoftInterventionSpec {
    pub(super) channel_id: serenity::ChannelId,
    pub(super) author_id: serenity::UserId,
    pub(super) author_is_bot: bool,
    pub(super) author_is_allowed_automation: bool,
    pub(super) message_id: serenity::MessageId,
    pub(super) text: String,
    pub(super) reply_context: Option<String>,
    pub(super) has_reply_boundary: bool,
    pub(super) merge_consecutive: bool,
    pub(super) pending_uploads: Vec<String>,
    pub(super) voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
}

impl SoftInterventionSpec {
    pub(super) fn into_intervention(self) -> Intervention {
        let queued_generation = crate::services::discord::runtime_store::load_generation();
        let source_generation = if self.author_is_bot || self.author_is_allowed_automation {
            SourceMessageQueuedGeneration::new(self.message_id, queued_generation)
        } else {
            SourceMessageQueuedGeneration::user_instruction(self.message_id, queued_generation)
        };
        Intervention {
            author_id: self.author_id,
            author_is_bot: self.author_is_bot,
            message_id: self.message_id,
            queued_generation,
            source_message_ids: vec![self.message_id],
            source_message_queued_generations: vec![source_generation],
            source_text_segments: Vec::new(),
            text: self.text,
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: self.reply_context,
            has_reply_boundary: self.has_reply_boundary,
            merge_consecutive: self.merge_consecutive,
            pending_uploads: self.pending_uploads,
            voice_announcement: self.voice_announcement,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct SoftInterventionCommitRequest {
    pub(super) source: IntakeQueueCommitSource,
    pub(super) author_class: IntakeQueueAuthorClass,
    pub(super) intervention: SoftInterventionSpec,
    pub(super) options: IntakeQueueCommitOptions,
}

impl SoftInterventionCommitRequest {
    fn channel_id(&self) -> serenity::ChannelId {
        self.intervention.channel_id
    }

    fn message_id(&self) -> serenity::MessageId {
        self.intervention.message_id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IntakeQueueCommittedStep {
    MailboxEnqueued,
    PendingReactionApplied,
    CheckpointAdvanced,
    IdleKickoffScheduled,
}

impl IntakeQueueCommittedStep {
    fn as_str(self) -> &'static str {
        match self {
            Self::MailboxEnqueued => "mailbox_enqueued",
            Self::PendingReactionApplied => "pending_reaction_applied",
            Self::CheckpointAdvanced => "checkpoint_advanced",
            Self::IdleKickoffScheduled => "idle_kickoff_scheduled",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PendingReactionSkipReason {
    Disabled,
    Refused,
    Failed,
}

impl PendingReactionSkipReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Refused => "refused",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PendingReactionDecision {
    Apply(char),
    Skip(PendingReactionSkipReason),
}

impl PendingReactionDecision {
    fn as_log_value(self) -> String {
        match self {
            Self::Apply(emoji) => format!("apply:{emoji}"),
            Self::Skip(reason) => format!("skip:{}", reason.as_str()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum IntakeQueueCommitStatus {
    Enqueued {
        merged: bool,
    },
    Refused {
        reason: Option<EnqueueRefusalReason>,
    },
    Failed {
        error: String,
    },
}

impl IntakeQueueCommitStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Enqueued { .. } => "enqueued",
            Self::Refused { .. } => "refused",
            Self::Failed { .. } => "failed",
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct IntakeQueueCommitOutcome {
    pub(super) channel_id: serenity::ChannelId,
    pub(super) message_id: serenity::MessageId,
    pub(super) source: IntakeQueueCommitSource,
    pub(super) author_class: IntakeQueueAuthorClass,
    pub(super) status: IntakeQueueCommitStatus,
    pub(super) mailbox_outcome: MailboxEnqueueOutcome,
    pub(super) pending_reaction: PendingReactionDecision,
    pub(super) committed_steps: Vec<IntakeQueueCommittedStep>,
    pub(super) checkpoint_advanced_to: Option<u64>,
    pub(super) idle_kickoff_started_count: Option<usize>,
}

impl IntakeQueueCommitOutcome {
    pub(super) fn accepted(&self) -> bool {
        matches!(self.status, IntakeQueueCommitStatus::Enqueued { .. })
    }

    pub(super) fn failed(&self) -> bool {
        matches!(self.status, IntakeQueueCommitStatus::Failed { .. })
    }

    pub(super) fn merged(&self) -> bool {
        matches!(
            self.status,
            IntakeQueueCommitStatus::Enqueued { merged: true }
        )
    }

    pub(super) fn refusal_reason(&self) -> Option<EnqueueRefusalReason> {
        match self.status {
            IntakeQueueCommitStatus::Refused { reason } => reason,
            _ => None,
        }
    }

    pub(super) fn checkpoint_advanced(&self) -> bool {
        self.checkpoint_advanced_to.is_some()
    }

    fn committed_steps_log_value(&self) -> String {
        if self.committed_steps.is_empty() {
            return "none".to_string();
        }
        self.committed_steps
            .iter()
            .map(|step| step.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[async_trait]
pub(super) trait IntakeQueueCommitEffects {
    async fn enqueue_soft_intervention(
        &mut self,
        intervention: SoftInterventionSpec,
    ) -> MailboxEnqueueOutcome;

    async fn apply_pending_reaction(
        &mut self,
        channel_id: serenity::ChannelId,
        message_id: serenity::MessageId,
        emoji: char,
    ) -> bool;

    async fn repair_queued_source_pending_reaction(
        &mut self,
        channel_id: serenity::ChannelId,
        message_id: serenity::MessageId,
        policy: IntakeQueuePendingReactionPolicy,
    ) -> Option<PendingReactionRepair>;

    async fn notify_pending_reaction_failure(
        &mut self,
        channel_id: serenity::ChannelId,
        message_id: serenity::MessageId,
    );

    fn advance_checkpoint(
        &mut self,
        channel_id: serenity::ChannelId,
        message_id: serenity::MessageId,
    ) -> u64;

    async fn schedule_idle_kickoff(&mut self) -> usize;
}

pub(super) fn queue_pending_reaction_for(outcome: &MailboxEnqueueOutcome) -> char {
    if outcome.merged {
        super::super::queue_reactions::QUEUE_MERGED_PENDING_REACTION
    } else {
        super::super::queue_reactions::QUEUE_STANDALONE_PENDING_REACTION
    }
}

pub(super) async fn commit_soft_intervention_transaction<E>(
    effects: &mut E,
    request: SoftInterventionCommitRequest,
) -> IntakeQueueCommitOutcome
where
    E: IntakeQueueCommitEffects,
{
    let channel_id = request.channel_id();
    let message_id = request.message_id();
    let source = request.source;
    let author_class = request.author_class;
    let options = request.options;
    let has_voice_announcement = request.intervention.voice_announcement.is_some();
    let mut intervention = request.intervention;
    if has_voice_announcement {
        // Do not expose a queued item that can be replayed as already accepted
        // before the durable one-shot claim runs. The queued dispatch path
        // re-enters handle_text_message with the original readable announce
        // text, which resolves and consumes the durable PG row at processing
        // time. Keeping voice items standalone preserves message-id ownership
        // and avoids mixing two voice prompts into one queue head.
        intervention.voice_announcement = None;
        intervention.merge_consecutive = false;
    }
    let mailbox_outcome = effects.enqueue_soft_intervention(intervention).await;

    let status = classify_mailbox_outcome(&mailbox_outcome);
    let mut outcome = IntakeQueueCommitOutcome {
        channel_id,
        message_id,
        source,
        author_class,
        status,
        mailbox_outcome,
        pending_reaction: PendingReactionDecision::Skip(PendingReactionSkipReason::Disabled),
        committed_steps: Vec::new(),
        checkpoint_advanced_to: None,
        idle_kickoff_started_count: None,
    };

    if outcome.mailbox_outcome.enqueued {
        outcome
            .committed_steps
            .push(IntakeQueueCommittedStep::MailboxEnqueued);
    }

    match outcome.status {
        IntakeQueueCommitStatus::Enqueued { .. } => {
            match options.pending_reaction {
                IntakeQueuePendingReactionPolicy::QueueState => {
                    let emoji = queue_pending_reaction_for(&outcome.mailbox_outcome);
                    if effects
                        .apply_pending_reaction(channel_id, message_id, emoji)
                        .await
                    {
                        outcome.pending_reaction = PendingReactionDecision::Apply(emoji);
                        outcome
                            .committed_steps
                            .push(IntakeQueueCommittedStep::PendingReactionApplied);
                    } else {
                        outcome.pending_reaction =
                            PendingReactionDecision::Skip(PendingReactionSkipReason::Failed);
                        effects
                            .notify_pending_reaction_failure(channel_id, message_id)
                            .await;
                    }
                }
                IntakeQueuePendingReactionPolicy::Static(emoji) => {
                    if effects
                        .apply_pending_reaction(channel_id, message_id, emoji)
                        .await
                    {
                        outcome.pending_reaction = PendingReactionDecision::Apply(emoji);
                        outcome
                            .committed_steps
                            .push(IntakeQueueCommittedStep::PendingReactionApplied);
                    } else {
                        outcome.pending_reaction =
                            PendingReactionDecision::Skip(PendingReactionSkipReason::Failed);
                        effects
                            .notify_pending_reaction_failure(channel_id, message_id)
                            .await;
                    }
                }
            }

            if options.advance_checkpoint {
                let checkpoint = effects.advance_checkpoint(channel_id, message_id);
                outcome.checkpoint_advanced_to = Some(checkpoint);
                outcome
                    .committed_steps
                    .push(IntakeQueueCommittedStep::CheckpointAdvanced);
            }
        }
        IntakeQueueCommitStatus::Refused { reason } => {
            let repair = if reason == Some(EnqueueRefusalReason::SourceIdAlreadyQueued) {
                effects
                    .repair_queued_source_pending_reaction(
                        channel_id,
                        message_id,
                        options.pending_reaction,
                    )
                    .await
            } else {
                None
            };
            match repair {
                Some(PendingReactionRepair {
                    emoji,
                    delivered: true,
                }) => {
                    outcome.pending_reaction = PendingReactionDecision::Apply(emoji);
                    outcome
                        .committed_steps
                        .push(IntakeQueueCommittedStep::PendingReactionApplied);
                }
                Some(PendingReactionRepair {
                    delivered: false, ..
                }) => {
                    outcome.pending_reaction =
                        PendingReactionDecision::Skip(PendingReactionSkipReason::Failed);
                    effects
                        .notify_pending_reaction_failure(channel_id, message_id)
                        .await;
                }
                None => {
                    outcome.pending_reaction =
                        PendingReactionDecision::Skip(PendingReactionSkipReason::Refused);
                }
            }
        }
        IntakeQueueCommitStatus::Failed { .. } => {
            outcome.pending_reaction =
                PendingReactionDecision::Skip(PendingReactionSkipReason::Failed);
        }
    }

    let should_schedule_idle_kickoff = match options.idle_kickoff {
        IntakeQueueIdleKickoffPolicy::Never => false,
        IntakeQueueIdleKickoffPolicy::AlwaysAfterAttempt => true,
    };
    if should_schedule_idle_kickoff {
        let started = effects.schedule_idle_kickoff().await;
        outcome.idle_kickoff_started_count = Some(started);
        outcome
            .committed_steps
            .push(IntakeQueueCommittedStep::IdleKickoffScheduled);
    }

    log_commit_outcome(&outcome);
    outcome
}

fn classify_mailbox_outcome(outcome: &MailboxEnqueueOutcome) -> IntakeQueueCommitStatus {
    if let Some(error) = outcome.persistence_error.as_ref() {
        return IntakeQueueCommitStatus::Failed {
            error: error.clone(),
        };
    }
    if outcome.enqueued {
        IntakeQueueCommitStatus::Enqueued {
            merged: outcome.merged,
        }
    } else {
        IntakeQueueCommitStatus::Refused {
            reason: outcome.refusal_reason,
        }
    }
}

fn log_commit_outcome(outcome: &IntakeQueueCommitOutcome) {
    let committed_steps = outcome.committed_steps_log_value();
    let pending_reaction = outcome.pending_reaction.as_log_value();
    let refusal_reason = outcome
        .refusal_reason()
        .map(EnqueueRefusalReason::as_str)
        .unwrap_or("none");
    let persistence_error = outcome
        .mailbox_outcome
        .persistence_error
        .as_deref()
        .unwrap_or("none");

    match outcome.status {
        IntakeQueueCommitStatus::Failed { .. } => {
            tracing::warn!(
                channel_id = outcome.channel_id.get(),
                message_id = outcome.message_id.get(),
                source = outcome.source.as_str(),
                author_class = outcome.author_class.as_str(),
                outcome = outcome.status.as_str(),
                committed_steps = committed_steps.as_str(),
                pending_reaction = pending_reaction.as_str(),
                refusal_reason,
                persistence_error,
                "discord intake queue transaction finished with failed commit"
            );
        }
        _ => {
            tracing::info!(
                channel_id = outcome.channel_id.get(),
                message_id = outcome.message_id.get(),
                source = outcome.source.as_str(),
                author_class = outcome.author_class.as_str(),
                outcome = outcome.status.as_str(),
                committed_steps = committed_steps.as_str(),
                pending_reaction = pending_reaction.as_str(),
                refusal_reason,
                persistence_error,
                "discord intake queue transaction committed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvRootGuard {
        previous: Option<std::ffi::OsString>,
        _lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
    }

    impl EnvRootGuard {
        fn set(path: &std::path::Path) -> Self {
            let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
            let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
            // SAFETY: the crate-wide env lock serializes test environment
            // mutations, and Drop restores the previous value while locked.
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
            Self {
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvRootGuard {
        fn drop(&mut self) {
            // SAFETY: this guard still owns the crate-wide env lock.
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    struct FakeEffects {
        enqueue_outcome: MailboxEnqueueOutcome,
        enqueued_specs: Vec<SoftInterventionSpec>,
        reactions: Vec<(serenity::ChannelId, serenity::MessageId, char)>,
        reaction_delivery: bool,
        repair_reaction: Option<char>,
        repair_delivery: bool,
        fallback_notices: Vec<(serenity::ChannelId, serenity::MessageId)>,
        checkpoints: Vec<(serenity::ChannelId, serenity::MessageId)>,
        idle_kickoffs: usize,
    }

    impl Default for FakeEffects {
        fn default() -> Self {
            Self {
                enqueue_outcome: MailboxEnqueueOutcome::default(),
                enqueued_specs: Vec::new(),
                reactions: Vec::new(),
                reaction_delivery: true,
                repair_reaction: None,
                repair_delivery: true,
                fallback_notices: Vec::new(),
                checkpoints: Vec::new(),
                idle_kickoffs: 0,
            }
        }
    }

    #[async_trait]
    impl IntakeQueueCommitEffects for FakeEffects {
        async fn enqueue_soft_intervention(
            &mut self,
            intervention: SoftInterventionSpec,
        ) -> MailboxEnqueueOutcome {
            self.enqueued_specs.push(intervention);
            self.enqueue_outcome.clone()
        }

        async fn apply_pending_reaction(
            &mut self,
            channel_id: serenity::ChannelId,
            message_id: serenity::MessageId,
            emoji: char,
        ) -> bool {
            self.reactions.push((channel_id, message_id, emoji));
            self.reaction_delivery
        }

        async fn repair_queued_source_pending_reaction(
            &mut self,
            channel_id: serenity::ChannelId,
            message_id: serenity::MessageId,
            policy: IntakeQueuePendingReactionPolicy,
        ) -> Option<PendingReactionRepair> {
            let emoji = match policy {
                IntakeQueuePendingReactionPolicy::QueueState => self.repair_reaction?,
                IntakeQueuePendingReactionPolicy::Static(emoji) => emoji,
            };
            self.reactions.push((channel_id, message_id, emoji));
            Some(PendingReactionRepair {
                emoji,
                delivered: self.repair_delivery,
            })
        }

        async fn notify_pending_reaction_failure(
            &mut self,
            channel_id: serenity::ChannelId,
            message_id: serenity::MessageId,
        ) {
            self.fallback_notices.push((channel_id, message_id));
        }

        fn advance_checkpoint(
            &mut self,
            channel_id: serenity::ChannelId,
            message_id: serenity::MessageId,
        ) -> u64 {
            self.checkpoints.push((channel_id, message_id));
            message_id.get()
        }

        async fn schedule_idle_kickoff(&mut self) -> usize {
            self.idle_kickoffs += 1;
            self.idle_kickoffs
        }
    }

    fn request(options: IntakeQueueCommitOptions) -> SoftInterventionCommitRequest {
        SoftInterventionCommitRequest {
            source: IntakeQueueCommitSource::BusyActiveTurn,
            author_class: IntakeQueueAuthorClass::Human,
            intervention: SoftInterventionSpec {
                channel_id: serenity::ChannelId::new(42),
                author_id: serenity::UserId::new(7),
                author_is_bot: false,
                author_is_allowed_automation: false,
                message_id: serenity::MessageId::new(100),
                text: "hello".to_string(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: true,
                pending_uploads: Vec::new(),
                voice_announcement: None,
            },
            options,
        }
    }

    fn voice_announcement() -> crate::voice::prompt::VoiceTranscriptAnnouncement {
        crate::voice::prompt::VoiceTranscriptAnnouncement {
            transcript: "상태 알려줘".to_string(),
            user_id: "42".to_string(),
            utterance_id: "utt-3724".to_string(),
            language: "ko-KR".to_string(),
            verbose_progress: true,
            started_at: Some("2026-06-28T22:00:00+09:00".to_string()),
            completed_at: Some("2026-06-28T22:00:01+09:00".to_string()),
            samples_written: Some(48_000),
            control_channel_id: Some(300),
            stt_mode: Some("file".to_string()),
            stt_latency_ms: Some(120),
        }
    }

    #[test]
    fn only_human_intake_sets_cancel_preservation_marker() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());

        let human = request(Default::default()).intervention.into_intervention();
        assert!(human.preserve_on_cancel());

        let mut bot = request(Default::default()).intervention;
        bot.author_is_bot = true;
        assert!(!bot.into_intervention().preserve_on_cancel());
    }

    #[test]
    fn false_flag_allowed_automation_dispatch_is_not_cancel_preserved() {
        let temp = tempfile::tempdir().expect("create temp runtime root");
        let _guard = EnvRootGuard::set(temp.path());

        let mut automation = request(Default::default()).intervention;
        automation.author_is_allowed_automation = true;
        automation.text = "DISPATCH:1f3c2b1a-0000-4000-8000-000000000000".to_string();

        assert!(!automation.into_intervention().preserve_on_cancel());
    }

    #[tokio::test]
    async fn accepted_commit_applies_reaction_and_advances_checkpoint() {
        let mut effects = FakeEffects {
            enqueue_outcome: MailboxEnqueueOutcome {
                enqueued: true,
                merged: false,
                refusal_reason: None,
                persistence_error: None,
            },
            ..FakeEffects::default()
        };

        let outcome =
            commit_soft_intervention_transaction(&mut effects, request(Default::default())).await;

        assert!(outcome.accepted());
        assert_eq!(
            outcome.pending_reaction,
            PendingReactionDecision::Apply('📬')
        );
        assert_eq!(
            effects.reactions,
            vec![(
                serenity::ChannelId::new(42),
                serenity::MessageId::new(100),
                '📬'
            )]
        );
        assert_eq!(
            effects.checkpoints,
            vec![(serenity::ChannelId::new(42), serenity::MessageId::new(100))]
        );
        assert_eq!(
            outcome.committed_steps,
            vec![
                IntakeQueueCommittedStep::MailboxEnqueued,
                IntakeQueueCommittedStep::PendingReactionApplied,
                IntakeQueueCommittedStep::CheckpointAdvanced,
            ]
        );
        assert!(
            effects.fallback_notices.is_empty(),
            "a delivered accepted reaction must not emit fallback UI"
        );
    }

    #[tokio::test]
    async fn accepted_commit_records_reaction_failure_without_claiming_application() {
        let mut effects = FakeEffects {
            enqueue_outcome: MailboxEnqueueOutcome {
                enqueued: true,
                merged: false,
                refusal_reason: None,
                persistence_error: None,
            },
            reaction_delivery: false,
            ..FakeEffects::default()
        };

        let outcome =
            commit_soft_intervention_transaction(&mut effects, request(Default::default())).await;

        assert!(outcome.accepted());
        assert_eq!(
            outcome.pending_reaction,
            PendingReactionDecision::Skip(PendingReactionSkipReason::Failed)
        );
        assert_eq!(
            outcome.committed_steps,
            vec![
                IntakeQueueCommittedStep::MailboxEnqueued,
                IntakeQueueCommittedStep::CheckpointAdvanced,
            ],
            "a failed reaction must never be reported as PendingReactionApplied"
        );
        assert_eq!(
            effects.fallback_notices,
            vec![(serenity::ChannelId::new(42), serenity::MessageId::new(100))],
            "a failed accepted reaction must emit exactly one referenced fallback notice"
        );
    }

    #[tokio::test]
    async fn accepted_commit_can_apply_static_reaction() {
        let mut effects = FakeEffects {
            enqueue_outcome: MailboxEnqueueOutcome {
                enqueued: true,
                merged: false,
                refusal_reason: None,
                persistence_error: None,
            },
            ..FakeEffects::default()
        };
        let mut options = IntakeQueueCommitOptions::default();
        options.pending_reaction = IntakeQueuePendingReactionPolicy::Static(
            super::super::super::queue_reactions::QUEUE_RECONCILE_PENDING_REACTION,
        );

        let outcome = commit_soft_intervention_transaction(&mut effects, request(options)).await;

        assert!(outcome.accepted());
        assert_eq!(
            outcome.pending_reaction,
            PendingReactionDecision::Apply('🔄')
        );
        assert_eq!(
            effects.reactions,
            vec![(
                serenity::ChannelId::new(42),
                serenity::MessageId::new(100),
                '🔄'
            )]
        );
        assert_eq!(
            effects.checkpoints,
            vec![(serenity::ChannelId::new(42), serenity::MessageId::new(100))]
        );
        assert!(
            effects.fallback_notices.is_empty(),
            "a delivered static reaction must not emit fallback UI"
        );
    }

    #[tokio::test]
    async fn accepted_static_reaction_failure_emits_exactly_one_fallback() {
        let mut effects = FakeEffects {
            enqueue_outcome: MailboxEnqueueOutcome {
                enqueued: true,
                merged: false,
                refusal_reason: None,
                persistence_error: None,
            },
            reaction_delivery: false,
            ..FakeEffects::default()
        };
        let mut options = IntakeQueueCommitOptions::default();
        options.pending_reaction = IntakeQueuePendingReactionPolicy::Static(
            super::super::super::queue_reactions::QUEUE_RECONCILE_PENDING_REACTION,
        );

        let outcome = commit_soft_intervention_transaction(&mut effects, request(options)).await;

        assert!(outcome.accepted());
        assert_eq!(
            outcome.pending_reaction,
            PendingReactionDecision::Skip(PendingReactionSkipReason::Failed)
        );
        assert_eq!(
            effects.reactions,
            vec![(
                serenity::ChannelId::new(42),
                serenity::MessageId::new(100),
                '🔄'
            )]
        );
        assert_eq!(
            effects.fallback_notices,
            vec![(serenity::ChannelId::new(42), serenity::MessageId::new(100))],
            "a failed static accepted reaction must emit exactly one fallback"
        );
    }

    #[tokio::test]
    async fn source_id_duplicate_refusal_repairs_existing_queue_reaction_without_checkpoint() {
        let mut effects = FakeEffects {
            enqueue_outcome: MailboxEnqueueOutcome {
                enqueued: false,
                merged: false,
                refusal_reason: Some(EnqueueRefusalReason::SourceIdAlreadyQueued),
                persistence_error: None,
            },
            repair_reaction: Some('📬'),
            ..FakeEffects::default()
        };

        let outcome =
            commit_soft_intervention_transaction(&mut effects, request(Default::default())).await;

        assert!(!outcome.accepted());
        assert_eq!(
            outcome.refusal_reason(),
            Some(EnqueueRefusalReason::SourceIdAlreadyQueued)
        );
        assert_eq!(
            outcome.pending_reaction,
            PendingReactionDecision::Apply('📬')
        );
        assert_eq!(
            effects.reactions,
            vec![(
                serenity::ChannelId::new(42),
                serenity::MessageId::new(100),
                '📬'
            )]
        );
        assert!(effects.checkpoints.is_empty());
        assert_eq!(
            outcome.committed_steps,
            vec![IntakeQueueCommittedStep::PendingReactionApplied]
        );
        assert!(
            effects.fallback_notices.is_empty(),
            "a delivered duplicate repair must not emit fallback UI"
        );
    }

    #[tokio::test]
    async fn source_id_duplicate_repair_failure_is_not_committed_as_applied() {
        let mut effects = FakeEffects {
            enqueue_outcome: MailboxEnqueueOutcome {
                enqueued: false,
                merged: false,
                refusal_reason: Some(EnqueueRefusalReason::SourceIdAlreadyQueued),
                persistence_error: None,
            },
            repair_reaction: Some('📬'),
            repair_delivery: false,
            ..FakeEffects::default()
        };

        let outcome =
            commit_soft_intervention_transaction(&mut effects, request(Default::default())).await;

        assert_eq!(
            outcome.pending_reaction,
            PendingReactionDecision::Skip(PendingReactionSkipReason::Failed),
            "failed duplicate repair must stay observable as a reaction delivery failure"
        );
        assert!(
            !outcome
                .committed_steps
                .contains(&IntakeQueueCommittedStep::PendingReactionApplied),
            "failed duplicate repair must never be recorded as PendingReactionApplied"
        );
        assert_eq!(
            effects.fallback_notices,
            vec![(serenity::ChannelId::new(42), serenity::MessageId::new(100))],
            "failed duplicate repair must emit exactly one referenced fallback notice"
        );
    }

    #[tokio::test]
    async fn source_id_duplicate_refusal_preserves_static_reconcile_reaction_policy() {
        let mut effects = FakeEffects {
            enqueue_outcome: MailboxEnqueueOutcome {
                enqueued: false,
                merged: false,
                refusal_reason: Some(EnqueueRefusalReason::SourceIdAlreadyQueued),
                persistence_error: None,
            },
            repair_reaction: Some('📬'),
            ..FakeEffects::default()
        };
        let mut options = IntakeQueueCommitOptions::default();
        options.pending_reaction = IntakeQueuePendingReactionPolicy::Static(
            super::super::super::queue_reactions::QUEUE_RECONCILE_PENDING_REACTION,
        );

        let outcome = commit_soft_intervention_transaction(&mut effects, request(options)).await;

        assert_eq!(
            outcome.pending_reaction,
            PendingReactionDecision::Apply('🔄'),
            "a duplicate reconcile-gate delivery must retain its static pending-reaction policy"
        );
        assert_eq!(
            effects.reactions,
            vec![(
                serenity::ChannelId::new(42),
                serenity::MessageId::new(100),
                '🔄'
            )],
            "duplicate repair must not replace 🔄 with a queue-position-derived marker"
        );
        assert!(effects.checkpoints.is_empty());
    }

    #[tokio::test]
    async fn persistence_failure_does_not_emit_misleading_reaction_or_checkpoint() {
        let mut effects = FakeEffects {
            enqueue_outcome: MailboxEnqueueOutcome {
                enqueued: true,
                merged: false,
                refusal_reason: None,
                persistence_error: Some("disk unavailable".to_string()),
            },
            ..FakeEffects::default()
        };

        let outcome =
            commit_soft_intervention_transaction(&mut effects, request(Default::default())).await;

        assert!(outcome.failed());
        assert!(effects.reactions.is_empty());
        assert!(
            effects.fallback_notices.is_empty(),
            "an intervention that was not durably queued must not claim queue admission"
        );
        assert!(effects.checkpoints.is_empty());
        assert_eq!(
            outcome.committed_steps,
            vec![IntakeQueueCommittedStep::MailboxEnqueued]
        );
    }

    #[tokio::test]
    async fn voice_commit_defers_durable_claim_until_dispatch() {
        let mut effects = FakeEffects {
            enqueue_outcome: MailboxEnqueueOutcome {
                enqueued: true,
                merged: false,
                refusal_reason: None,
                persistence_error: None,
            },
            ..FakeEffects::default()
        };
        let mut req = request(Default::default());
        req.intervention.voice_announcement = Some(voice_announcement());

        let outcome = commit_soft_intervention_transaction(&mut effects, req).await;

        assert!(outcome.accepted());
        assert_eq!(effects.enqueued_specs.len(), 1);
        assert!(effects.enqueued_specs[0].voice_announcement.is_none());
        assert!(
            !effects.enqueued_specs[0].merge_consecutive,
            "queued voice announcements stay standalone so dispatch can resolve the durable row by the head message id"
        );
        assert_eq!(
            outcome.pending_reaction,
            PendingReactionDecision::Apply('📬')
        );
        assert_eq!(
            effects.reactions,
            vec![(
                serenity::ChannelId::new(42),
                serenity::MessageId::new(100),
                '📬'
            )]
        );
        assert_eq!(
            effects.checkpoints,
            vec![(serenity::ChannelId::new(42), serenity::MessageId::new(100))]
        );
        assert_eq!(
            outcome.committed_steps,
            vec![
                IntakeQueueCommittedStep::MailboxEnqueued,
                IntakeQueueCommittedStep::PendingReactionApplied,
                IntakeQueueCommittedStep::CheckpointAdvanced,
            ]
        );
    }

    #[tokio::test]
    async fn voice_persistence_failure_never_exposes_accepted_replay_payload() {
        let mut effects = FakeEffects {
            enqueue_outcome: MailboxEnqueueOutcome {
                enqueued: true,
                merged: false,
                refusal_reason: None,
                persistence_error: Some("disk unavailable".to_string()),
            },
            ..FakeEffects::default()
        };
        let mut req = request(Default::default());
        req.intervention.voice_announcement = Some(voice_announcement());

        let outcome = commit_soft_intervention_transaction(&mut effects, req).await;

        assert!(outcome.failed());
        assert_eq!(effects.enqueued_specs.len(), 1);
        assert!(effects.enqueued_specs[0].voice_announcement.is_none());
        assert!(effects.reactions.is_empty());
        assert!(effects.checkpoints.is_empty());
        assert_eq!(
            outcome.committed_steps,
            vec![IntakeQueueCommittedStep::MailboxEnqueued]
        );
    }

    #[tokio::test]
    async fn idle_backlog_commit_schedules_kickoff_after_accept() {
        let mut effects = FakeEffects {
            enqueue_outcome: MailboxEnqueueOutcome {
                enqueued: true,
                merged: true,
                refusal_reason: None,
                persistence_error: None,
            },
            ..FakeEffects::default()
        };
        let mut req = request(IntakeQueueCommitOptions::idle_backlog());
        req.source = IntakeQueueCommitSource::IdleBacklog;

        let outcome = commit_soft_intervention_transaction(&mut effects, req).await;

        assert!(outcome.accepted());
        assert_eq!(
            outcome.pending_reaction,
            PendingReactionDecision::Apply('➕')
        );
        assert_eq!(effects.idle_kickoffs, 1);
        assert_eq!(outcome.idle_kickoff_started_count, Some(1));
        assert!(
            outcome
                .committed_steps
                .contains(&IntakeQueueCommittedStep::IdleKickoffScheduled)
        );
    }

    #[tokio::test]
    async fn idle_backlog_refusal_still_kicks_existing_backlog_without_reaction_or_checkpoint() {
        let mut effects = FakeEffects {
            enqueue_outcome: MailboxEnqueueOutcome {
                enqueued: false,
                merged: false,
                refusal_reason: Some(EnqueueRefusalReason::LastItemDedup),
                persistence_error: None,
            },
            ..FakeEffects::default()
        };
        let mut req = request(IntakeQueueCommitOptions::idle_backlog());
        req.source = IntakeQueueCommitSource::IdleBacklog;

        let outcome = commit_soft_intervention_transaction(&mut effects, req).await;

        assert!(!outcome.accepted());
        assert!(effects.reactions.is_empty());
        assert!(effects.checkpoints.is_empty());
        assert_eq!(effects.idle_kickoffs, 1);
        assert_eq!(
            outcome.committed_steps,
            vec![IntakeQueueCommittedStep::IdleKickoffScheduled]
        );
    }
}
