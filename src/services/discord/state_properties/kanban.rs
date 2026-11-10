//! Kanban pipeline reducer: random walks must follow the declared graph.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use proptest::prelude::*;

use crate::engine::transition::{
    CardState, ForceIntent, GateSnapshot, TransitionContext, TransitionIntent, TransitionOutcome,
    decide_status_transition, decide_transition,
};
use crate::engine::transition::TransitionEvent;
use crate::pipeline::{PipelineConfig, TransitionType};

fn pipeline() -> PipelineConfig {
    let text = include_str!("../../../../policies/default-pipeline.yaml");
    let config: PipelineConfig = serde_yaml::from_str(text).expect("default pipeline yaml");
    config.validate().expect("default pipeline");
    config
}

fn free_edges(pipeline: &PipelineConfig) -> BTreeMap<String, Vec<String>> {
    let mut edges: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for transition in &pipeline.transitions {
        if transition.transition_type == TransitionType::Free {
            edges
                .entry(transition.from.clone())
                .or_default()
                .push(transition.to.clone());
        }
    }
    edges
}

fn bfs(edges: &BTreeMap<String, Vec<String>>, from: &str, target: &str) -> Option<Vec<String>> {
    if from == target {
        return Some(Vec::new());
    }
    let mut parent = BTreeMap::new();
    let mut queue = VecDeque::from([from.to_string()]);
    let mut seen = BTreeSet::from([from.to_string()]);
    while let Some(current) = queue.pop_front() {
        for next in edges.get(&current).into_iter().flatten() {
            if !seen.insert(next.clone()) {
                continue;
            }
            parent.insert(next.clone(), current.clone());
            if next == target {
                let mut path = vec![next.clone()];
                let mut cursor = next.clone();
                while let Some(previous) = parent.get(&cursor) {
                    if previous == from {
                        break;
                    }
                    path.push(previous.clone());
                    cursor = previous.clone();
                }
                path.reverse();
                return Some(path);
            }
            queue.push_back(next.clone());
        }
    }
    None
}

fn card(status: &str) -> CardState {
    CardState {
        id: "card-1".into(),
        status: status.into(),
        review_status: None,
        latest_dispatch_id: None,
    }
}

fn context(pipeline: &PipelineConfig, status: &str, gates: GateSnapshot) -> TransitionContext {
    TransitionContext {
        card: card(status),
        pipeline: pipeline.clone(),
        gates,
    }
}

fn status_updates(intents: &[TransitionIntent]) -> Vec<(String, String)> {
    intents
        .iter()
        .filter_map(|intent| match intent {
            TransitionIntent::UpdateStatus { from, to, .. } => Some((from.clone(), to.clone())),
            _ => None,
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 40, .. ProptestConfig::default() })]
    fn free_paths_match_an_independent_search(from_ix in 0usize..6, to_ix in 0usize..6) {
        let pipeline = pipeline();
        let states: Vec<_> = pipeline.states.iter().map(|state| state.id.clone()).collect();
        let from = &states[from_ix % states.len()];
        let to = &states[to_ix % states.len()];
        let expected = bfs(&free_edges(&pipeline), from, to);
        let actual = pipeline.free_path_to_state(from, to);
        prop_assert_eq!(&actual, &expected);
        if let Some(path) = &actual {
            prop_assert!(path.len() <= states.len());
            let mut cursor = from.clone();
            for step in path {
                let decision = decide_status_transition(
                    &context(&pipeline, &cursor, GateSnapshot::default()),
                    step,
                    "property",
                    ForceIntent::None,
                );
                prop_assert_eq!(&decision.outcome, &TransitionOutcome::Allowed);
                let updates = status_updates(&decision.intents);
                prop_assert_eq!(updates, vec![(cursor.clone(), step.clone())]);
                cursor = step.to_string();
            }
            prop_assert_eq!(&cursor, to);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, .. ProptestConfig::default() })]
    fn reducer_walk_never_leaves_a_terminal_or_invents_a_status(
        steps in prop::collection::vec((0usize..8, any::<bool>(), any::<bool>(), any::<bool>()), 1..16)
    ) {
        let pipeline = pipeline();
        let states: Vec<_> = pipeline.states.iter().map(|state| state.id.clone()).collect();
        let mut status = pipeline.initial_state().to_string();
        for (index, force, active, review_pass) in steps {
            let target = &states[index % states.len()];
            let gates = GateSnapshot {
                has_active_dispatch: active,
                review_verdict_pass: review_pass,
                review_verdict_rework: !review_pass,
            };
            let forced = if force { ForceIntent::OperatorOverride } else { ForceIntent::None };
            let before = status.clone();
            let decision = decide_status_transition(
                &context(&pipeline, &status, gates),
                target,
                "property",
                forced,
            );
            let updates = status_updates(&decision.intents);
            match &decision.outcome {
                TransitionOutcome::NoOp => prop_assert!(updates.is_empty()),
                TransitionOutcome::Blocked(_) => {
                    prop_assert!(updates.is_empty());
                    if pipeline.is_terminal(&before) && !force {
                        prop_assert_ne!(target, &before);
                    }
                }
                TransitionOutcome::Allowed => {
                    prop_assert_eq!(updates.len(), 1);
                    prop_assert_eq!(&updates[0].0, &before);
                    prop_assert_eq!(&updates[0].1, target);
                    let syncs = decision.intents.iter().filter(|intent| {
                        matches!(intent, TransitionIntent::SyncAutoQueue { .. })
                    }).count();
                    prop_assert_eq!(syncs, usize::from(pipeline.is_terminal(target)));
                    status = target.clone();
                }
            }
            prop_assert!(pipeline.is_valid_state(&status));
        }
    }
}

pub(super) fn run() -> Vec<String> {
    [
        (
            "free_paths_match_an_independent_search",
            free_paths_match_an_independent_search as fn(),
        ),
        (
            "reducer_walk_never_leaves_a_terminal_or_invents_a_status",
            reducer_walk_never_leaves_a_terminal_or_invents_a_status,
        ),
        (
            "dispatch_events_do_not_move_a_card_without_a_declared_rule",
            dispatch_events_do_not_move_a_card_without_a_declared_rule,
        ),
    ]
    .into_iter()
    .filter_map(|(name, test)| super::exercise(name, test))
    .collect()
}

fn dispatch_events_do_not_move_a_card_without_a_declared_rule() {
    let pipeline = pipeline();
    let mut status = "backlog".to_string();
    let events = [
        TransitionEvent::DispatchAttached {
            dispatch_id: "d1".into(),
            dispatch_type: "work".into(),
            kickoff_state: None,
        },
        TransitionEvent::DispatchCompleted {
            dispatch_id: "d1".into(),
        },
        TransitionEvent::ReviewVerdict {
            verdict: "pass".into(),
        },
        TransitionEvent::RedispatchRequested,
    ];
    for event in events {
        let decision = decide_transition(
            &context(
                &pipeline,
                &status,
                GateSnapshot {
                    has_active_dispatch: true,
                    review_verdict_pass: true,
                    review_verdict_rework: false,
                },
            ),
            &event,
        );
        if let Some((from, to)) = status_updates(&decision.intents).into_iter().next() {
            assert_eq!(from, status);
            assert!(
                pipeline.find_transition(&from, &to).is_some() || decision.outcome == TransitionOutcome::Allowed,
                "{from} -> {to} via {event:?} is not a pipeline rule"
            );
            status = to;
        }
        assert!(pipeline.is_valid_state(&status), "{status}");
    }
}
