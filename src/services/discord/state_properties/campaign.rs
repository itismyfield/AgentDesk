//! Campaign control reducer and quality-failure budget.

use chrono::{DateTime, Utc};
use proptest::prelude::*;

use crate::services::campaign_execution::control::{
    command_fingerprint, create_definition, decide, decide_with_receipt,
};
use crate::services::campaign_execution::definition::{
    AcceptanceCondition, DefinitionInput, NodeDefinition, VerificationKind,
};
use crate::services::campaign_execution::failure_budget::{FailureDisposition, record_quality_failure};
use crate::services::campaign_execution::model::{
    CampaignState, Command, CommandEnvelope, NodeDisposition, NodeRuntime, Receipt, RejectionCode,
    Repository,
};
use crate::services::campaign_execution::profile::ProfileLimits;

fn at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-28T06:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn definition(title: &str) -> DefinitionInput {
    DefinitionInput {
        repository: Repository::Adk,
        title: title.into(),
        goal: "Verified work".into(),
        profile_version: "v1".into(),
        nodes: vec![NodeDefinition {
            id: "a".into(),
            title: "A".into(),
            issue_url: "https://github.com/itismyfield/AgentDesk/issues/7438".into(),
            acceptance: vec![AcceptanceCondition {
                id: "ac-a".into(),
                description: "Prove A".into(),
                verification: VerificationKind::Test,
            }],
            dependencies: Vec::new(),
            group: None,
        }],
    }
}

fn envelope(key: &str, command: Command) -> CommandEnvelope {
    CommandEnvelope {
        idempotency_key: key.into(),
        observed_at: at(),
        command,
    }
}

fn prepared() -> CampaignState {
    create_definition("campaign".into(), definition("Campaign"), "create-key", at())
        .expect("definition")
        .state
}

#[derive(Clone, Debug)]
enum CampOp {
    Start,
    Edit { title: u8, stale: bool },
    Replay,
    Conflict,
    EmptyKey,
}

fn op_strategy() -> impl Strategy<Value = CampOp> {
    prop_oneof![
        Just(CampOp::Start),
        (0u8..20, any::<bool>()).prop_map(|(title, stale)| CampOp::Edit { title, stale }),
        Just(CampOp::Replay),
        Just(CampOp::Conflict),
        Just(CampOp::EmptyKey),
    ]
}

struct Model {
    state: CampaignState,
    edits: u64,
    last: Option<CommandEnvelope>,
}

impl Model {
    fn apply(&mut self, op: CampOp) {
        let before = self.state.clone();
        let sequence = self.state.event_sequence;
        match op {
            CampOp::Start => {
                let err = decide(
                    &self.state,
                    &envelope(
                        "start",
                        Command::Start {
                            expected_revision: self.state.definition_revision,
                        },
                    ),
                )
                .expect_err("start is gated");
                assert_eq!(err.code, RejectionCode::ExecutionUnavailable);
                assert_eq!(self.state, before);
            }
            CampOp::Edit { title, stale } => {
                let expected = if stale && self.state.definition_revision > 0 {
                    self.state.definition_revision.wrapping_add(9)
                } else {
                    self.state.definition_revision
                };
                let command = envelope(
                    &format!("edit-{sequence}-{title}"),
                    Command::EditDefinition {
                        expected_revision: expected,
                        definition: definition(&format!("Campaign {title}")),
                        change: None,
                    },
                );
                match decide(&self.state, &command) {
                    Ok(decision) => {
                        assert!(!stale, "stale revision was applied");
                        assert_eq!(decision.state.event_sequence, sequence + 1);
                        assert_eq!(
                            decision.state.definition_revision,
                            before.definition_revision + 1
                        );
                        assert_eq!(
                            decision.state.nodes.keys().collect::<Vec<_>>(),
                            decision.state.definition.input().nodes.iter().map(|node| &node.id).collect::<Vec<_>>()
                        );
                        self.state = decision.state;
                        self.edits += 1;
                        self.last = Some(command);
                    }
                    Err(error) => {
                        assert!(
                            stale || error.code == RejectionCode::MissingChangeReason,
                            "fresh edit rejected: {error:?}"
                        );
                        assert_eq!(self.state, before);
                    }
                }
            }
            CampOp::Replay => {
                let Some(command) = self.last.clone() else {
                    return;
                };
                let fingerprint = command_fingerprint(&command.command).unwrap();
                let receipt = Receipt {
                    fingerprint,
                    result: Ok(crate::services::campaign_execution::model::CommandResult {
                        definition_revision: self.state.definition_revision,
                        run_id: self.state.run.as_ref().map(|run| run.id.clone()),
                        control_version: self.state.run.as_ref().map(|run| run.control_version),
                        event_sequence: self.state.event_sequence,
                        control: self.state.control,
                    }),
                };
                let replay = decide_with_receipt(&self.state, &command, Some(&receipt)).unwrap();
                assert!(replay.events.is_empty(), "replay appended events");
                assert_eq!(replay.state.event_sequence, sequence);
                assert_eq!(replay.state, before);
            }
            CampOp::Conflict => {
                let Some(command) = self.last.clone() else {
                    return;
                };
                let mut other = command.clone();
                other.command = Command::Start {
                    expected_revision: self.state.definition_revision,
                };
                let receipt = Receipt {
                    fingerprint: command_fingerprint(&command.command).unwrap(),
                    result: Ok(crate::services::campaign_execution::model::CommandResult {
                        definition_revision: self.state.definition_revision,
                        run_id: None,
                        control_version: None,
                        event_sequence: self.state.event_sequence,
                        control: self.state.control,
                    }),
                };
                let err = decide_with_receipt(&self.state, &other, Some(&receipt)).unwrap_err();
                assert_eq!(err.code, RejectionCode::IdempotencyConflict);
                assert_eq!(self.state, before);
            }
            CampOp::EmptyKey => {
                let err = decide(
                    &self.state,
                    &envelope(
                        "   ",
                        Command::Start {
                            expected_revision: self.state.definition_revision,
                        },
                    ),
                )
                .unwrap_err();
                assert_eq!(err.code, RejectionCode::InvalidDefinition);
                assert_eq!(self.state, before);
            }
        }
        assert_eq!(self.state.event_sequence, 1 + self.edits);
        assert!(self.state.run.is_none(), "start became a run without a host");
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, .. ProptestConfig::default() })]
    fn campaign_commands_are_idempotent_and_do_not_invent_runs(
        ops in prop::collection::vec(op_strategy(), 1..18)
    ) {
        let mut model = Model { state: prepared(), edits: 0, last: None };
        for op in ops {
            model.apply(op);
        }
    }
}

pub(super) fn run() -> Vec<String> {
    [
        (
            "campaign_commands_are_idempotent_and_do_not_invent_runs",
            campaign_commands_are_idempotent_and_do_not_invent_runs as fn(),
        ),
        (
            "exhausted_quality_budget_does_not_keep_counting",
            exhausted_quality_budget_does_not_keep_counting,
        ),
    ]
    .into_iter()
    .filter_map(|(name, test)| super::exercise(name, test))
    .collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 40, .. ProptestConfig::default() })]
    fn exhausted_quality_budget_does_not_keep_counting(
        repairs in 0u32..4,
        extra in 1u32..6,
    ) {
        let limits = ProfileLimits {
            concurrent_tasks: 1,
            quality_repairs: repairs,
            design_review_rounds: 1,
        };
        let mut node = NodeRuntime::default();
        let mut counted = None;
        for cycle in 0..=repairs.saturating_add(extra) {
            let already_failed = node.disposition == NodeDisposition::Failed;
            let before = node.quality_failures;
            record_quality_failure(&mut node, &mut counted, cycle, &limits);
            if already_failed {
                prop_assert_eq!(
                    node.quality_failures, before,
                    "cycle {} counted another failure after the node was exhausted",
                    cycle
                );
                prop_assert_eq!(node.disposition, NodeDisposition::Failed);
            }
            let frozen = node.quality_failures;
            let again = record_quality_failure(&mut node, &mut counted, cycle, &limits);
            prop_assert_eq!(again, FailureDisposition::AlreadyCounted);
            prop_assert_eq!(node.quality_failures, frozen);
        }
    }
}
