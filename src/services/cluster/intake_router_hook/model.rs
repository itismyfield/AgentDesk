//! Placement inputs and outcomes; transport and ownership remain in the routing hook.
use super::IntakeRoutingMode;
use crate::db::intake_outbox::InsertPendingPayload;
use crate::db::intake_outbox_status::IntakeOutboxStatus;

/// What the hook decided. The intake gate uses this to choose between
/// "skip local execution; the worker has the row" and "fall through
/// to `handle_text_message` as today".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntakeRoutingBasis {
    LiveForeignOwner,
    NodeOverride,
    AgentDefault,
    PreferredLabels,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResolvedSessionOwner {
    NoOwner,
    LiveLocal,
    LiveForeign,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IntakeRouterDecision {
    /// No forwarding happened (Disabled mode, a confirmed ownerless channel
    /// with no usable preference, or an availability fallback after that
    /// ownerless state was proven). The caller MUST run the turn locally.
    RanLocal { reason: RanLocalReason },
    /// Observe mode evaluated the same owner-aware placement path as Enforce,
    /// but did not mutate the outbox. The caller MUST still run locally.
    Observed { outcome: ObservedIntakeOutcome },
    /// The hook inserted a row for the worker. The caller MUST NOT
    /// run the turn locally — that would double-emit the Discord turn.
    /// `outbox_id` is the row's PK for log correlation.
    Forwarded {
        target_instance_id: String,
        outbox_id: i64,
        basis: IntakeRoutingBasis,
    },
    /// At-most-once skip: Discord redelivered the same
    /// `(channel_id, user_msg_id)` and the 3-tuple unique constraint
    /// rejected the attempt-1 INSERT. An earlier path already covers
    /// the message. Caller MUST NOT run the turn locally — running it
    /// would double-emit. Distinct from `RanLocal { DbErrorFellBack }`
    /// because the gate's response differs (skip vs run-local).
    SkippedDuplicate {
        resolved_owner: ResolvedSessionOwner,
    },
    /// A different message already owns the channel's single open outbox
    /// route. The producer must preserve/retry queued work and MUST NOT run it
    /// locally while the predecessor is open. `open_route_status` is carried to
    /// the admission boundary because only a still-pending local route can use
    /// the narrowly scoped stale-route recovery exception.
    DeferredOpenRoute {
        target_instance_id: String,
        open_route_status: Option<IntakeOutboxStatus>,
        open_route_id: Option<i64>,
        open_route_age_secs: Option<u64>,
        resolved_owner: ResolvedSessionOwner,
    },
    /// Ownership or placement could not be proven safe. Caller MUST NOT run
    /// the local execution body.
    Blocked { reason: IntakeBlockedReason },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ObservedIntakeOutcome {
    WouldKeepLocalExistingOwner,
    WouldForwardLiveForeignOwner {
        target_instance_id: String,
    },
    WouldAssignNoOwnerToTarget {
        target_instance_id: String,
    },
    WouldKeepNoOwnerLocal {
        reason: RanLocalReason,
    },
    WouldSkipDuplicate {
        resolved_owner: ResolvedSessionOwner,
    },
    WouldDeferOpenRoute {
        target_instance_id: String,
        resolved_owner: ResolvedSessionOwner,
    },
    WouldBlock {
        reason: IntakeBlockedReason,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IntakeBlockedReason {
    OwnerLookupFailed { detail: String },
    StaleSessionOwners { instance_ids: Vec<String> },
    ConflictingLiveSessionOwners { instance_ids: Vec<String> },
    OwnerProtocolIncompatible { instance_id: String },
    OverrideUnavailable { target_instance_id: String },
    NonPortableAttachmentForeignOwner { owner_instance_id: String },
    NonPortableAttachmentRoutedTarget { target_instance_id: String },
    AttachmentUnavailable { detail: String },
    RoutingDependencyFailed { detail: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RanLocalReason {
    /// Mode is `Disabled` (Phase 1-4 default).
    HookDisabled,
    /// Agent opted out (`preferred_intake_node_labels` empty).
    AgentHasNoPreference,
    /// Agent opted in but no worker matches (offline, missing labels).
    NoEligibleWorker,
    /// Agent opted in and a worker matched, but the only eligible
    /// candidate IS the leader.
    LeaderIsOnlyEligible,
    /// The ready leader is the agent's preferred execution device.
    AgentDefaultIsLeader,
    /// Some DB or schema error during the routing decision. Reported
    /// so operators see WHY a forward turned into a local fallback.
    DbErrorFellBackToLocal { detail: String },
    /// `Disabled` mode looked up the agent and found a non-empty
    /// preference; recorded for the eventual cutover (Phase 5 ops
    /// uses this to find agents whose preferences are set but
    /// not yet enforced).
    DisabledButPreferenceSet,
    /// Agent for this channel could not be looked up (channel not
    /// mapped to an agent). Treated as no-preference.
    NoAgentForChannel,
    /// Channel has an explicit `/node` override to this leader, so the
    /// leader should keep the turn local.
    NodeOverrideIsLeader,
    /// Channel has an explicit `/node` override, but the intake worker is only
    /// spawned in `Enforce` mode. Running locally avoids pending-row loss.
    NodeOverrideRoutingDisabled,
    /// This instance is the durable live owner for the session.
    LiveSessionOwnerIsLocal,
}

/// Inputs to the hook. Bundled into a struct so the intake gate can
/// thread per-channel context cleanly without a 6-argument fn call.
#[derive(Clone, Debug)]
pub(crate) struct IntakeRouterContext<'a> {
    pub mode: IntakeRoutingMode,
    pub leader_instance_id: &'a str,
    /// Provider of the bot handling this intake (#4349). Worker claim is
    /// scoped on this, so it must be the forwarding bot's provider — never
    /// `agents.provider`, which is a single column shared by an agent's
    /// cc and cdx channels.
    pub provider: &'a str,
    pub channel_id: &'a str,
    /// Direct agent binding, otherwise the verified Discord thread parent.
    /// Ownership and delivery always continue to use `channel_id`.
    pub policy_channel_id: &'a str,
    pub user_msg_id: &'a str,
    pub request_owner_id: &'a str,
    pub request_owner_name: Option<&'a str>,
    pub user_text: &'a str,
    pub reply_context: Option<&'a str>,
    pub has_reply_boundary: bool,
    pub dm_hint: Option<bool>,
    pub turn_kind: &'a str,
    pub merge_consecutive: bool,
    pub reply_to_user_message: bool,
    pub defer_watcher_resume: bool,
    pub wait_for_completion: bool,
    pub preserve_on_cancel: bool,
    pub node_override_instance_id: Option<&'a str>,
    pub has_nonportable_uploads: bool,
    pub attachment_refs: &'a [crate::services::cluster::attachment_transfer::uploads::BundleRef],
}

pub(super) fn build_payload_for_insert(
    ctx: &IntakeRouterContext<'_>,
    target: &str,
    preferred_labels: &[String],
    agent_id: &str,
) -> InsertPendingPayload {
    InsertPendingPayload {
        execution_requirements: serde_json::json!({}),
        attachment_refs: serde_json::json!(ctx.attachment_refs),
        target_instance_id: target.to_string(),
        forwarded_by_instance_id: ctx.leader_instance_id.to_string(),
        provider: ctx.provider.to_string(),
        required_labels: serde_json::Value::Array(
            preferred_labels
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        ),
        channel_id: ctx.channel_id.to_string(),
        user_msg_id: ctx.user_msg_id.to_string(),
        request_owner_id: ctx.request_owner_id.to_string(),
        request_owner_name: ctx.request_owner_name.map(str::to_string),
        user_text: ctx.user_text.to_string(),
        reply_context: ctx.reply_context.map(str::to_string),
        has_reply_boundary: ctx.has_reply_boundary,
        dm_hint: ctx.dm_hint,
        // Phase 4 codex follow-up: leader emits canonical "foreground"
        // for `TurnKind::Foreground`; the worker's `parse_turn_kind`
        // accepts both "foreground" and "standard" for backwards
        // compatibility with rows already in the queue.
        turn_kind: ctx.turn_kind.to_string(),
        merge_consecutive: ctx.merge_consecutive,
        reply_to_user_message: ctx.reply_to_user_message,
        defer_watcher_resume: ctx.defer_watcher_resume,
        wait_for_completion: ctx.wait_for_completion,
        preserve_on_cancel: ctx.preserve_on_cancel,
        agent_id: agent_id.to_string(),
    }
}
