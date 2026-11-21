use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::{
    definition::valid_id,
    pipeline::{
        ConditionValue, PipelineCondition, PipelineDefinition, PipelineKind, PipelineState,
    },
    pipeline_fields::ConditionType,
};
use crate::utils::dag::find_cycle;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct PipelineDiagnostic {
    pub code: String,
    pub path: String,
    pub state_id: Option<String>,
    pub transition_id: Option<String>,
}

impl PipelineDiagnostic {
    pub(crate) fn new(
        code: &str,
        path: impl Into<String>,
        state_id: Option<&str>,
        transition_id: Option<&str>,
    ) -> Self {
        Self {
            code: code.into(),
            path: path.into(),
            state_id: state_id.map(str::to_owned),
            transition_id: transition_id.map(str::to_owned),
        }
    }
}

type Diagnostics = Vec<PipelineDiagnostic>;

impl PipelineDefinition {
    pub fn validate(&self) -> Diagnostics {
        let mut errors = Vec::new();
        let mut states = HashMap::new();
        let mut transitions = HashMap::new();
        let mut priorities = HashSet::new();
        let mut budgets = HashMap::new();
        let mut events = HashSet::new();
        for (name, version) in [
            ("schema_version", self.schema_version),
            ("vocabulary_version", self.vocabulary_version),
            ("evaluator_version", self.evaluator_version),
            ("registry_version", self.registry_version),
        ] {
            if version != 1 {
                errors.push(PipelineDiagnostic::new(
                    "UNSUPPORTED_VERSION",
                    format!("/{name}"),
                    None,
                    None,
                ));
            }
        }
        for (i, state) in self.states.iter().enumerate() {
            let id = state.id.as_str();
            if states.insert(id, i).is_some() || !valid_id(id) {
                errors.push(PipelineDiagnostic::new(
                    "INVALID_STATE_ID",
                    format!("/states/{i}/id"),
                    Some(id),
                    None,
                ));
            }
            if state.executor.as_ref().is_some_and(|executor| {
                terminal(state.kind) || !valid_id(&executor.id) || executor.version == 0
            }) {
                errors.push(PipelineDiagnostic::new(
                    "INVALID_EXECUTOR",
                    format!("/states/{i}/executor"),
                    Some(id),
                    None,
                ));
            }
        }
        let start = states.get(self.start.as_str()).copied();
        if start.is_none() {
            let code = if self.start.is_empty() {
                "START_MISSING"
            } else {
                "START_NOT_FOUND"
            };
            errors.push(PipelineDiagnostic::new(
                code,
                "/start",
                Some(&self.start),
                None,
            ));
        }

        let mut graph = Graph::new(self.states.len());
        for (i, transition) in self.transitions.iter().enumerate() {
            let id = Some(transition.id.as_str());
            let from = Some(transition.from.as_str());
            let diagnostic = |code, field| {
                PipelineDiagnostic::new(code, format!("/transitions/{i}/{field}"), from, id)
            };
            if transitions
                .insert(transition.id.as_str(), transition)
                .is_some()
                || !valid_id(&transition.id)
            {
                errors.push(diagnostic("INVALID_TRANSITION_ID", "id"));
            }
            if !priorities.insert((&transition.from, transition.priority)) {
                errors.push(diagnostic("DUPLICATE_PRIORITY", "priority"));
            }

            let mut endpoint = |name: &str, code, field| {
                let index = states.get(name).copied();
                if index.is_none() {
                    errors.push(PipelineDiagnostic::new(
                        code,
                        format!("/transitions/{i}/{field}"),
                        Some(name),
                        id,
                    ));
                }
                index
            };
            let source = endpoint(&transition.from, "TRANSITION_SOURCE_MISSING", "from");
            let target = endpoint(&transition.to, "TRANSITION_TARGET_MISSING", "to");
            let exhausted = transition.retry.as_ref().and_then(|retry| {
                endpoint(&retry.then_to, "RETRY_TARGET_MISSING", "retry/then_to")
            });

            if let Some(retry) = &transition.retry
                && (!valid_id(&retry.budget_id)
                    || budgets
                        .insert(&retry.budget_id, retry.max)
                        .is_some_and(|max| max != retry.max))
            {
                errors.push(diagnostic("INVALID_RETRY_BUDGET", "retry"));
            }
            if let Some(source) = source {
                let state = &self.states[source];
                graph.exits[source] += 1;
                if terminal(state.kind) {
                    errors.push(diagnostic("TERMINAL_HAS_EXIT", "from"));
                }
                validate_condition(
                    &transition.condition,
                    state,
                    &transition.id,
                    format!("/transitions/{i}/condition"),
                    &mut errors,
                );
                if let Some(target) = target {
                    graph.forward[source].push(target);
                    if transition.retry.is_none() {
                        graph.free[source].push((target, i, "to"));
                    }
                }
                if let Some(exhausted) = exhausted {
                    graph.forward[source].push(exhausted);
                    graph.free[source].push((exhausted, i, "retry/then_to"));
                }
            }
        }

        for (i, state) in self.states.iter().enumerate() {
            if let Some(timeout) = &state.timeout
                && (timeout.after_seconds == 0
                    || !transitions
                        .get(timeout.transition_id.as_str())
                        .is_some_and(|t| t.from == state.id))
            {
                errors.push(PipelineDiagnostic::new(
                    "INVALID_TIMEOUT_TRANSITION",
                    format!("/states/{i}/timeout"),
                    Some(&state.id),
                    Some(&timeout.transition_id),
                ));
            }
        }
        for (i, event) in self.events.iter().enumerate() {
            if !events.insert(&event.id) || !valid_id(&event.id) {
                errors.push(PipelineDiagnostic::new(
                    "INVALID_EVENT_ID",
                    format!("/events/{i}/id"),
                    Some(&event.state_id),
                    None,
                ));
            }
            if !states.contains_key(event.state_id.as_str()) {
                errors.push(PipelineDiagnostic::new(
                    "EVENT_STATE_MISSING",
                    format!("/events/{i}/state_id"),
                    Some(&event.state_id),
                    None,
                ));
            }
        }

        graph.validate(self, start, &mut errors);
        errors
    }
}

fn terminal(kind: PipelineKind) -> bool {
    matches!(kind, PipelineKind::EndSuccess | PipelineKind::EndFailure)
}

fn validate_condition(
    condition: &PipelineCondition,
    state: &PipelineState,
    transition: &str,
    path: String,
    errors: &mut Diagnostics,
) {
    let mut push = |code, path| {
        errors.push(PipelineDiagnostic::new(
            code,
            path,
            Some(&state.id),
            Some(transition),
        ))
    };
    match condition {
        PipelineCondition::All(all) if all.all.is_empty() => push("EMPTY_CONDITION", path),
        PipelineCondition::All(all) => {
            for (i, condition) in all.all.iter().enumerate() {
                validate_condition(
                    condition,
                    state,
                    transition,
                    format!("{path}/all/{i}"),
                    errors,
                );
            }
        }
        PipelineCondition::Compare(comparison) => {
            let spec = comparison.field.spec(state.kind, state.executor.as_ref());
            if !spec.available {
                push("FIELD_NOT_ALLOWED", format!("{path}/field"));
            }
            if !spec.operators.contains(&comparison.op) {
                push("OP_NOT_ALLOWED", format!("{path}/op"));
            }
            let valid = match (spec.value_type, &comparison.value) {
                (ConditionType::Boolean, ConditionValue::Boolean(_))
                | (ConditionType::Integer, ConditionValue::Integer(_))
                | (ConditionType::String, ConditionValue::String(_)) => true,
                (ConditionType::Enum, ConditionValue::String(value)) => {
                    spec.enum_values.contains(&value.as_str())
                }
                _ => false,
            };
            if !valid {
                push("VALUE_TYPE_MISMATCH", format!("{path}/value"));
            }
        }
    }
}

struct Graph {
    forward: Vec<Vec<usize>>,
    free: Vec<Vec<(usize, usize, &'static str)>>,
    exits: Vec<usize>,
}

impl Graph {
    fn new(size: usize) -> Self {
        Self {
            forward: vec![Vec::new(); size],
            free: vec![Vec::new(); size],
            exits: vec![0; size],
        }
    }

    fn validate(
        &self,
        pipeline: &PipelineDefinition,
        start: Option<usize>,
        errors: &mut Diagnostics,
    ) {
        let mut reverse = vec![Vec::new(); self.forward.len()];
        for (from, targets) in self.forward.iter().enumerate() {
            for &to in targets {
                reverse[to].push(from);
            }
        }
        let from_start = reachable(&self.forward, start);
        let ends = reachable_from_ends(pipeline, &reverse);
        let mut has_success = false;
        for (i, state) in pipeline.states.iter().enumerate() {
            let mut push = |code| {
                errors.push(PipelineDiagnostic::new(
                    code,
                    format!("/states/{i}"),
                    Some(&state.id),
                    None,
                ))
            };
            has_success |= from_start[i] && state.kind == PipelineKind::EndSuccess;
            if start.is_some() && !from_start[i] {
                push("STATE_UNREACHABLE");
            }
            if !terminal(state.kind) {
                if self.exits[i] == 0 {
                    push("STATE_NO_EXIT");
                }
                if !ends[i] {
                    push("NO_TERMINAL_PATH");
                }
            }
        }
        if !has_success {
            errors.push(PipelineDiagnostic::new(
                "SUCCESS_UNREACHABLE",
                "/start",
                Some(&pipeline.start),
                None,
            ));
        }
        let free: Vec<Vec<_>> = self
            .free
            .iter()
            .map(|edges| edges.iter().map(|edge| edge.0).collect())
            .collect();
        if let Some(cycle) = find_cycle(&free) {
            for edge in cycle.windows(2) {
                for &(_, i, field) in self.free[edge[0]].iter().filter(|entry| entry.0 == edge[1]) {
                    let transition = &pipeline.transitions[i];
                    errors.push(PipelineDiagnostic::new(
                        "UNBOUNDED_CYCLE",
                        format!("/transitions/{i}/{field}"),
                        Some(&transition.from),
                        Some(&transition.id),
                    ));
                }
            }
        }
    }
}

fn reachable_from_ends(pipeline: &PipelineDefinition, reverse: &[Vec<usize>]) -> Vec<bool> {
    reachable(
        reverse,
        pipeline
            .states
            .iter()
            .enumerate()
            .filter_map(|(i, state)| terminal(state.kind).then_some(i)),
    )
}

fn reachable(graph: &[Vec<usize>], roots: impl IntoIterator<Item = usize>) -> Vec<bool> {
    let mut seen = vec![false; graph.len()];
    let mut stack: Vec<_> = roots.into_iter().collect();
    while let Some(state) = stack.pop() {
        if !std::mem::replace(&mut seen[state], true) {
            stack.extend(&graph[state]);
        }
    }
    seen
}
