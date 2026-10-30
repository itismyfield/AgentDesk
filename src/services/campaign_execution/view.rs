use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{control, model::*};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayState {
    Stopping,
    Stopped,
    Prepared,
    Succeeded,
    Running,
    Waiting,
    UnableToProgress,
    NeedsInspection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicCommand {
    EditDefinition,
    Start,
    StopImmediate,
    StopFinishStage,
    Resume,
    PrepareRetry,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CommandAvailability {
    pub command: PublicCommand,
    pub rejection: Option<RejectionCode>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Reason {
    pub code: ReasonCode,
    pub target: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NodeView<'a> {
    pub id: &'a str,
    pub runtime: &'a NodeRuntime,
    pub direct_successors: Vec<&'a str>,
    pub ready_after_main: Vec<&'a str>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ServerCampaignView<'a> {
    pub execution_mode: &'static str,
    pub campaign_id: &'a str,
    pub as_of: DateTime<Utc>,
    pub definition_revision: u64,
    pub run_id: Option<&'a str>,
    pub control_version: Option<u64>,
    pub last_event_sequence: u64,
    pub last_event_at: Option<DateTime<Utc>>,
    pub control: CampaignControl,
    pub stop_mode: Option<StopMode>,
    pub stop_confirmed: bool,
    pub display: DisplayState,
    pub execution_readiness: ExecutionReadiness,
    pub unknown: bool,
    pub failed_count: usize,
    pub blocked_count: usize,
    pub reasons: Vec<Reason>,
    pub commands: Vec<CommandAvailability>,
    pub nodes: Vec<NodeView<'a>>,
    pub sessions: &'a [SessionRecord],
    pub unreconciled_actions: Vec<&'a Action>,
}

pub fn view(state: &CampaignState, as_of: DateTime<Utc>) -> ServerCampaignView<'_> {
    let sessions = state
        .run
        .as_ref()
        .map(|r| r.sessions.as_slice())
        .unwrap_or_default();
    let actions: Vec<_> = state
        .run
        .iter()
        .flat_map(|r| r.actions.values())
        .filter(|a| !matches!(a.state, ActionState::Cancelled | ActionState::Reconciled))
        .collect();
    let unknown = sessions
        .iter()
        .any(|s| matches!(s.observation, SessionObservation::Unknown { .. }))
        || actions
            .iter()
            .any(|a| matches!(a.state, ActionState::Unknown | ActionState::StartPending))
        || state.run.iter().flat_map(|r| r.actions.values()).any(|a| {
            matches!(a.kind, ActionKind::Launch | ActionKind::DeliverStage)
                && a.initiation_receipt.is_some()
                && !sessions.iter().any(|s| s.identity == a.binding.session)
        });
    let failed_count = state
        .nodes
        .values()
        .filter(|n| n.disposition == NodeDisposition::Failed)
        .count();
    let blocked_count = state
        .nodes
        .values()
        .filter(|n| n.disposition == NodeDisposition::Blocked)
        .count();
    let active = actions.iter().any(|a| {
        matches!(
            a.state,
            ActionState::StartPending | ActionState::Started | ActionState::Unknown
        )
    }) || state.nodes.values().any(|n| {
        n.wait_reason.is_none()
            && matches!(
                n.disposition,
                NodeDisposition::Running | NodeDisposition::Verifying
            )
    });
    let waiting = state.nodes.values().any(|n| {
        n.wait_reason.is_some_and(|r| {
            matches!(
                r,
                ReasonCode::ReviewResources
                    | ReasonCode::HostResources
                    | ReasonCode::OwnedElsewhere
            )
        }) || n.disposition == NodeDisposition::Pending
    });
    let display = match state.control {
        CampaignControl::Stopping => DisplayState::Stopping,
        CampaignControl::Stopped => DisplayState::Stopped,
        CampaignControl::Prepared => DisplayState::Prepared,
        CampaignControl::Succeeded
            if !unknown
                && state.nodes.values().all(|n| n.acceptance_complete)
                && !state.nodes.is_empty() =>
        {
            DisplayState::Succeeded
        }
        CampaignControl::Running if active => DisplayState::Running,
        CampaignControl::Running if waiting => DisplayState::Waiting,
        CampaignControl::Running if failed_count + blocked_count > 0 => {
            DisplayState::UnableToProgress
        }
        _ => DisplayState::NeedsInspection,
    };
    let mut reasons: Vec<_> = state
        .nodes
        .iter()
        .filter_map(|(id, n)| {
            n.wait_reason.map(|code| Reason {
                code,
                target: id.clone(),
            })
        })
        .collect();
    for definition in &state.definition.input().nodes {
        if state.nodes.get(&definition.id).is_some_and(|n| {
            matches!(
                n.disposition,
                NodeDisposition::Pending | NodeDisposition::Blocked
            )
        }) {
            for dependency in &definition.dependencies {
                if let Some(predecessor) = state.nodes.get(dependency)
                    && !predecessor.main_integrated
                {
                    reasons.push(Reason {
                        code: if predecessor.disposition == NodeDisposition::Failed {
                            ReasonCode::PredecessorFailed
                        } else {
                            ReasonCode::PredecessorMain
                        },
                        target: definition.id.clone(),
                    });
                }
            }
        }
    }
    reasons.push(Reason {
        code: ReasonCode::ExecutionUnavailable,
        target: state.id.clone(),
    });
    if unknown {
        reasons.push(Reason {
            code: ReasonCode::ObservationUnknown,
            target: state.id.clone(),
        });
    }
    let commands = [
        PublicCommand::EditDefinition,
        PublicCommand::Start,
        PublicCommand::StopImmediate,
        PublicCommand::StopFinishStage,
        PublicCommand::Resume,
        PublicCommand::PrepareRetry,
    ]
    .into_iter()
    .map(|command| {
        let result = match command {
            PublicCommand::EditDefinition => control::edit_guard(state),
            PublicCommand::StopImmediate | PublicCommand::StopFinishStage => {
                control::stop_guard(state)
            }
            _ => Err(Rejection::new(RejectionCode::ExecutionUnavailable)),
        };
        CommandAvailability {
            command,
            rejection: result.err().map(|e| e.code),
        }
    })
    .collect();
    let nodes = state
        .definition
        .input()
        .nodes
        .iter()
        .map(|definition| {
            let direct_successors: Vec<_> = state
                .definition
                .input()
                .nodes
                .iter()
                .filter(|n| n.dependencies.contains(&definition.id))
                .map(|n| n.id.as_str())
                .collect();
            let ready_after_main = state
                .definition
                .input()
                .nodes
                .iter()
                .filter(|n| {
                    direct_successors.contains(&n.id.as_str())
                        && state.nodes.get(&n.id).is_some_and(|r| {
                            r.disposition == NodeDisposition::Pending && r.wait_reason.is_none()
                        })
                        && !unknown
                        && n.dependencies.iter().all(|d| {
                            d == &definition.id
                                || state.nodes.get(d).is_some_and(|r| r.main_integrated)
                        })
                })
                .map(|n| n.id.as_str())
                .collect();
            (definition, direct_successors, ready_after_main)
        })
        .filter_map(|(d, direct_successors, ready_after_main)| {
            state.nodes.get(&d.id).map(|runtime| NodeView {
                id: &d.id,
                runtime,
                direct_successors,
                ready_after_main,
            })
        })
        .collect();
    let stop = state.run.as_ref().and_then(|r| r.stop.as_ref());
    ServerCampaignView {
        execution_mode: "server",
        campaign_id: &state.id,
        as_of,
        definition_revision: state.definition_revision,
        run_id: state.run.as_ref().map(|r| r.id.as_str()),
        control_version: state.run.as_ref().map(|r| r.control_version),
        last_event_sequence: state.event_sequence,
        last_event_at: state.last_event_at,
        control: state.control,
        stop_mode: stop.map(|s| s.mode),
        stop_confirmed: stop.is_some_and(|s| s.confirmed_event.is_some()),
        display,
        execution_readiness: control::execution_readiness(),
        unknown,
        failed_count,
        blocked_count,
        reasons,
        commands,
        nodes,
        sessions,
        unreconciled_actions: actions,
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
