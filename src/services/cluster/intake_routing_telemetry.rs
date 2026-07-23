use std::sync::atomic::{AtomicU64, Ordering};

use super::intake_router_hook::{
    IntakeBlockedReason, IntakeRouterDecision, IntakeRoutingBasis, IntakeRoutingMode,
    ObservedIntakeOutcome, RanLocalReason, ResolvedSessionOwner,
};

static RECENT_DECISION_COUNT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntakeRoutingReasonCode {
    HookDisabled,
    DisabledButPreferenceSet,
    NoAgentForChannel,
    AgentHasNoPreference,
    NoEligibleWorker,
    LeaderIsOnlyEligible,
    DependencyFallback,
    NodeOverrideIsLeader,
    NodeOverrideRoutingDisabled,
    LiveOwnerLocal,
    LiveOwnerForeign,
    NoOwnerTargetSelected,
    DuplicateMessage,
    OpenRouteDeferred,
    OwnerLookupFailed,
    StaleSessionOwners,
    ConflictingLiveSessionOwners,
    OwnerProtocolIncompatible,
    OverrideUnavailable,
    NonPortableAttachmentForeignOwner,
    NonPortableAttachmentRoutedTarget,
    RoutingDependencyFailed,
}

impl IntakeRoutingReasonCode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::HookDisabled => "hook_disabled",
            Self::DisabledButPreferenceSet => "disabled_but_preference_set",
            Self::NoAgentForChannel => "no_agent_for_channel",
            Self::AgentHasNoPreference => "agent_has_no_preference",
            Self::NoEligibleWorker => "no_eligible_worker",
            Self::LeaderIsOnlyEligible => "leader_is_only_eligible",
            Self::DependencyFallback => "dependency_fallback",
            Self::NodeOverrideIsLeader => "node_override_is_leader",
            Self::NodeOverrideRoutingDisabled => "node_override_routing_disabled",
            Self::LiveOwnerLocal => "live_owner_local",
            Self::LiveOwnerForeign => "live_owner_foreign",
            Self::NoOwnerTargetSelected => "no_owner_target_selected",
            Self::DuplicateMessage => "duplicate_message",
            Self::OpenRouteDeferred => "open_route_deferred",
            Self::OwnerLookupFailed => "owner_lookup_failed",
            Self::StaleSessionOwners => "stale_session_owners",
            Self::ConflictingLiveSessionOwners => "conflicting_live_session_owners",
            Self::OwnerProtocolIncompatible => "owner_protocol_incompatible",
            Self::OverrideUnavailable => "override_unavailable",
            Self::NonPortableAttachmentForeignOwner => "nonportable_attachment_foreign_owner",
            Self::NonPortableAttachmentRoutedTarget => "nonportable_attachment_routed_target",
            Self::RoutingDependencyFailed => "routing_dependency_failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OwnerResolutionCode {
    NotEvaluated,
    NoOwner,
    LiveLocal,
    LiveForeign,
    Failed,
    Stale,
    Conflicting,
    Incompatible,
}

impl OwnerResolutionCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::NotEvaluated => "not_evaluated",
            Self::NoOwner => "no_owner",
            Self::LiveLocal => "live_local",
            Self::LiveForeign => "live_foreign",
            Self::Failed => "failed",
            Self::Stale => "stale",
            Self::Conflicting => "conflicting",
            Self::Incompatible => "incompatible",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PreferredLabelMatchCode {
    NotEvaluated,
    NoPreference,
    MatchedWorker,
    LeaderOnly,
    NoEligibleWorker,
    LookupFailed,
}

impl PreferredLabelMatchCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::NotEvaluated => "not_evaluated",
            Self::NoPreference => "no_preference",
            Self::MatchedWorker => "matched_worker",
            Self::LeaderOnly => "leader_only",
            Self::NoEligibleWorker => "no_eligible_worker",
            Self::LookupFailed => "lookup_failed",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct IntakeRoutingTelemetry<'a> {
    pub(crate) reason_code: IntakeRoutingReasonCode,
    pub(crate) would_assign_target: Option<&'a str>,
    pub(crate) owner_resolution: OwnerResolutionCode,
    pub(crate) preferred_label_match: PreferredLabelMatchCode,
}

fn blocked_reason_code(reason: &IntakeBlockedReason) -> IntakeRoutingReasonCode {
    match reason {
        IntakeBlockedReason::OwnerLookupFailed { .. } => IntakeRoutingReasonCode::OwnerLookupFailed,
        IntakeBlockedReason::StaleSessionOwners { .. } => {
            IntakeRoutingReasonCode::StaleSessionOwners
        }
        IntakeBlockedReason::ConflictingLiveSessionOwners { .. } => {
            IntakeRoutingReasonCode::ConflictingLiveSessionOwners
        }
        IntakeBlockedReason::OwnerProtocolIncompatible { .. } => {
            IntakeRoutingReasonCode::OwnerProtocolIncompatible
        }
        IntakeBlockedReason::OverrideUnavailable { .. } => {
            IntakeRoutingReasonCode::OverrideUnavailable
        }
        IntakeBlockedReason::NonPortableAttachmentForeignOwner { .. } => {
            IntakeRoutingReasonCode::NonPortableAttachmentForeignOwner
        }
        IntakeBlockedReason::NonPortableAttachmentRoutedTarget { .. } => {
            IntakeRoutingReasonCode::NonPortableAttachmentRoutedTarget
        }
        IntakeBlockedReason::RoutingDependencyFailed { .. } => {
            IntakeRoutingReasonCode::RoutingDependencyFailed
        }
    }
}

fn owner_resolution_code(owner: ResolvedSessionOwner) -> OwnerResolutionCode {
    match owner {
        ResolvedSessionOwner::NoOwner => OwnerResolutionCode::NoOwner,
        ResolvedSessionOwner::LiveLocal => OwnerResolutionCode::LiveLocal,
        ResolvedSessionOwner::LiveForeign => OwnerResolutionCode::LiveForeign,
    }
}

fn ran_local_telemetry(reason: &RanLocalReason) -> IntakeRoutingTelemetry<'static> {
    let (reason_code, owner_resolution, preferred_label_match) = match reason {
        RanLocalReason::HookDisabled => (
            IntakeRoutingReasonCode::HookDisabled,
            OwnerResolutionCode::NotEvaluated,
            PreferredLabelMatchCode::NoPreference,
        ),
        RanLocalReason::DisabledButPreferenceSet => (
            IntakeRoutingReasonCode::DisabledButPreferenceSet,
            OwnerResolutionCode::NotEvaluated,
            PreferredLabelMatchCode::NotEvaluated,
        ),
        RanLocalReason::NoAgentForChannel => (
            IntakeRoutingReasonCode::NoAgentForChannel,
            OwnerResolutionCode::NoOwner,
            PreferredLabelMatchCode::NoPreference,
        ),
        RanLocalReason::AgentHasNoPreference => (
            IntakeRoutingReasonCode::AgentHasNoPreference,
            OwnerResolutionCode::NoOwner,
            PreferredLabelMatchCode::NoPreference,
        ),
        RanLocalReason::NoEligibleWorker => (
            IntakeRoutingReasonCode::NoEligibleWorker,
            OwnerResolutionCode::NoOwner,
            PreferredLabelMatchCode::NoEligibleWorker,
        ),
        RanLocalReason::LeaderIsOnlyEligible => (
            IntakeRoutingReasonCode::LeaderIsOnlyEligible,
            OwnerResolutionCode::NoOwner,
            PreferredLabelMatchCode::LeaderOnly,
        ),
        RanLocalReason::DbErrorFellBackToLocal { .. } => (
            IntakeRoutingReasonCode::DependencyFallback,
            OwnerResolutionCode::NoOwner,
            PreferredLabelMatchCode::LookupFailed,
        ),
        RanLocalReason::NodeOverrideIsLeader => (
            IntakeRoutingReasonCode::NodeOverrideIsLeader,
            OwnerResolutionCode::NoOwner,
            PreferredLabelMatchCode::NotEvaluated,
        ),
        RanLocalReason::NodeOverrideRoutingDisabled => (
            IntakeRoutingReasonCode::NodeOverrideRoutingDisabled,
            OwnerResolutionCode::NotEvaluated,
            PreferredLabelMatchCode::NotEvaluated,
        ),
        RanLocalReason::LiveSessionOwnerIsLocal => (
            IntakeRoutingReasonCode::LiveOwnerLocal,
            OwnerResolutionCode::LiveLocal,
            PreferredLabelMatchCode::NotEvaluated,
        ),
    };
    IntakeRoutingTelemetry {
        reason_code,
        would_assign_target: None,
        owner_resolution,
        preferred_label_match,
    }
}

pub(crate) fn telemetry_for_decision(
    decision: &IntakeRouterDecision,
) -> IntakeRoutingTelemetry<'_> {
    match decision {
        IntakeRouterDecision::RanLocal { reason } => ran_local_telemetry(reason),
        IntakeRouterDecision::Observed { outcome } => match outcome {
            ObservedIntakeOutcome::WouldKeepLocalExistingOwner => IntakeRoutingTelemetry {
                reason_code: IntakeRoutingReasonCode::LiveOwnerLocal,
                would_assign_target: None,
                owner_resolution: OwnerResolutionCode::LiveLocal,
                preferred_label_match: PreferredLabelMatchCode::NotEvaluated,
            },
            ObservedIntakeOutcome::WouldForwardLiveForeignOwner { target_instance_id } => {
                IntakeRoutingTelemetry {
                    reason_code: IntakeRoutingReasonCode::LiveOwnerForeign,
                    would_assign_target: Some(target_instance_id),
                    owner_resolution: OwnerResolutionCode::LiveForeign,
                    preferred_label_match: PreferredLabelMatchCode::NotEvaluated,
                }
            }
            ObservedIntakeOutcome::WouldAssignNoOwnerToTarget { target_instance_id } => {
                IntakeRoutingTelemetry {
                    reason_code: IntakeRoutingReasonCode::NoOwnerTargetSelected,
                    would_assign_target: Some(target_instance_id),
                    owner_resolution: OwnerResolutionCode::NoOwner,
                    preferred_label_match: PreferredLabelMatchCode::MatchedWorker,
                }
            }
            ObservedIntakeOutcome::WouldKeepNoOwnerLocal { reason } => ran_local_telemetry(reason),
            ObservedIntakeOutcome::WouldSkipDuplicate { resolved_owner } => {
                IntakeRoutingTelemetry {
                    reason_code: IntakeRoutingReasonCode::DuplicateMessage,
                    would_assign_target: None,
                    owner_resolution: owner_resolution_code(*resolved_owner),
                    preferred_label_match: PreferredLabelMatchCode::NotEvaluated,
                }
            }
            ObservedIntakeOutcome::WouldDeferOpenRoute {
                target_instance_id,
                resolved_owner,
            } => IntakeRoutingTelemetry {
                reason_code: IntakeRoutingReasonCode::OpenRouteDeferred,
                would_assign_target: Some(target_instance_id),
                owner_resolution: owner_resolution_code(*resolved_owner),
                preferred_label_match: PreferredLabelMatchCode::NotEvaluated,
            },
            ObservedIntakeOutcome::WouldBlock { reason } => blocked_telemetry(reason),
        },
        IntakeRouterDecision::Forwarded {
            target_instance_id,
            basis,
            ..
        } => match basis {
            IntakeRoutingBasis::LiveForeignOwner => IntakeRoutingTelemetry {
                reason_code: IntakeRoutingReasonCode::LiveOwnerForeign,
                would_assign_target: Some(target_instance_id),
                owner_resolution: OwnerResolutionCode::LiveForeign,
                preferred_label_match: PreferredLabelMatchCode::NotEvaluated,
            },
            IntakeRoutingBasis::NodeOverride => IntakeRoutingTelemetry {
                reason_code: IntakeRoutingReasonCode::NoOwnerTargetSelected,
                would_assign_target: Some(target_instance_id),
                owner_resolution: OwnerResolutionCode::NoOwner,
                preferred_label_match: PreferredLabelMatchCode::NotEvaluated,
            },
            IntakeRoutingBasis::PreferredLabels => IntakeRoutingTelemetry {
                reason_code: IntakeRoutingReasonCode::NoOwnerTargetSelected,
                would_assign_target: Some(target_instance_id),
                owner_resolution: OwnerResolutionCode::NoOwner,
                preferred_label_match: PreferredLabelMatchCode::MatchedWorker,
            },
        },
        IntakeRouterDecision::SkippedDuplicate { resolved_owner } => IntakeRoutingTelemetry {
            reason_code: IntakeRoutingReasonCode::DuplicateMessage,
            would_assign_target: None,
            owner_resolution: owner_resolution_code(*resolved_owner),
            preferred_label_match: PreferredLabelMatchCode::NotEvaluated,
        },
        IntakeRouterDecision::DeferredOpenRoute {
            target_instance_id,
            resolved_owner,
        } => IntakeRoutingTelemetry {
            reason_code: IntakeRoutingReasonCode::OpenRouteDeferred,
            would_assign_target: Some(target_instance_id),
            owner_resolution: owner_resolution_code(*resolved_owner),
            preferred_label_match: PreferredLabelMatchCode::NotEvaluated,
        },
        IntakeRouterDecision::Blocked { reason } => blocked_telemetry(reason),
    }
}

fn blocked_telemetry(reason: &IntakeBlockedReason) -> IntakeRoutingTelemetry<'_> {
    let owner_resolution = match reason {
        IntakeBlockedReason::OwnerLookupFailed { .. } => OwnerResolutionCode::Failed,
        IntakeBlockedReason::StaleSessionOwners { .. } => OwnerResolutionCode::Stale,
        IntakeBlockedReason::ConflictingLiveSessionOwners { .. } => {
            OwnerResolutionCode::Conflicting
        }
        IntakeBlockedReason::OwnerProtocolIncompatible { .. } => OwnerResolutionCode::Incompatible,
        IntakeBlockedReason::NonPortableAttachmentForeignOwner { .. } => {
            OwnerResolutionCode::LiveForeign
        }
        IntakeBlockedReason::OverrideUnavailable { .. }
        | IntakeBlockedReason::NonPortableAttachmentRoutedTarget { .. }
        | IntakeBlockedReason::RoutingDependencyFailed { .. } => OwnerResolutionCode::NotEvaluated,
    };
    IntakeRoutingTelemetry {
        reason_code: blocked_reason_code(reason),
        would_assign_target: match reason {
            IntakeBlockedReason::OverrideUnavailable { target_instance_id }
            | IntakeBlockedReason::NonPortableAttachmentRoutedTarget { target_instance_id } => {
                Some(target_instance_id)
            }
            IntakeBlockedReason::NonPortableAttachmentForeignOwner { owner_instance_id } => {
                Some(owner_instance_id)
            }
            _ => None,
        },
        owner_resolution,
        preferred_label_match: PreferredLabelMatchCode::NotEvaluated,
    }
}

pub(crate) fn record_decision(
    mode: IntakeRoutingMode,
    channel_id: &str,
    user_msg_id: &str,
    authority_channel_opted_in: bool,
    decision: &IntakeRouterDecision,
) {
    let telemetry = telemetry_for_decision(decision);
    RECENT_DECISION_COUNT.fetch_add(1, Ordering::Relaxed);
    tracing::info!(
        event = "intake_routing_decision",
        mode = mode.as_str(),
        channel_id,
        user_msg_id,
        authority_channel_opted_in,
        authority_scope = "telemetry_only",
        would_assign_target = telemetry.would_assign_target,
        owner_resolution = telemetry.owner_resolution.as_str(),
        preferred_label_match = telemetry.preferred_label_match.as_str(),
        reason_code = telemetry.reason_code.as_str(),
        "[intake_router] routing decision"
    );
}

pub(crate) fn recent_decision_count() -> u64 {
    RECENT_DECISION_COUNT.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::fmt::writer::MakeWriter;

    use super::*;

    #[derive(Clone)]
    struct CapturingWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn observe_planner_telemetry_is_stable_and_complete() {
        let decision = IntakeRouterDecision::Observed {
            outcome: ObservedIntakeOutcome::WouldAssignNoOwnerToTarget {
                target_instance_id: "worker-mac".to_string(),
            },
        };
        let telemetry = telemetry_for_decision(&decision);
        assert_eq!(
            telemetry,
            IntakeRoutingTelemetry {
                reason_code: IntakeRoutingReasonCode::NoOwnerTargetSelected,
                would_assign_target: Some("worker-mac"),
                owner_resolution: OwnerResolutionCode::NoOwner,
                preferred_label_match: PreferredLabelMatchCode::MatchedWorker,
            }
        );
        assert_eq!(telemetry.reason_code.as_str(), "no_owner_target_selected");
    }

    #[test]
    fn observe_decision_emits_info_level_structured_fields() {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::INFO)
            .with_writer(CapturingWriter(buffer.clone()))
            .finish();
        let decision = IntakeRouterDecision::Observed {
            outcome: ObservedIntakeOutcome::WouldAssignNoOwnerToTarget {
                target_instance_id: "worker-mac".to_string(),
            },
        };

        tracing::subscriber::with_default(subscriber, || {
            record_decision(IntakeRoutingMode::Observe, "123", "456", true, &decision);
        });

        let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        for field in [
            "event=\"intake_routing_decision\"",
            "mode=\"observe\"",
            "authority_channel_opted_in=true",
            "would_assign_target=\"worker-mac\"",
            "owner_resolution=\"no_owner\"",
            "preferred_label_match=\"matched_worker\"",
            "reason_code=\"no_owner_target_selected\"",
        ] {
            assert!(logs.contains(field), "missing {field} in logs={logs}");
        }
    }

    #[test]
    fn owner_failure_uses_stable_reason_and_resolution_codes() {
        let decision = IntakeRouterDecision::Observed {
            outcome: ObservedIntakeOutcome::WouldBlock {
                reason: IntakeBlockedReason::ConflictingLiveSessionOwners {
                    instance_ids: vec!["worker-a".into(), "worker-b".into()],
                },
            },
        };
        let telemetry = telemetry_for_decision(&decision);
        assert_eq!(
            telemetry.reason_code.as_str(),
            "conflicting_live_session_owners"
        );
        assert_eq!(telemetry.owner_resolution, OwnerResolutionCode::Conflicting);
    }
}
