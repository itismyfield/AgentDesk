use super::{
    model::Stage,
    pipeline::{
        ConditionOperator::Eq, ConditionValue, PipelineAll, PipelineComparison, PipelineCondition,
        PipelineDefinition, PipelineExecutor, PipelineKind, PipelineParameters, PipelineRetry,
        PipelineState, PipelineTransition,
    },
    pipeline_fields::PipelineField,
    workflow_options::ApprovalGate,
};

pub const TEMPLATE_ID: &str = "campaign-development-9";
pub const TEMPLATE_VERSION: u32 = 1;

impl Stage {
    pub fn pipeline_state_id(self) -> &'static str {
        match self {
            Self::Requirements => "requirements",
            Self::Design => "design",
            Self::DesignReview => "design_review",
            Self::Implementation => "implementation",
            Self::Review => "review",
            Self::Landing => "landing",
            Self::MainVerification => "main_verification",
            Self::FinalVerification => "final_verification",
            Self::Complete => "complete",
        }
    }
}

pub fn development_template() -> PipelineDefinition {
    use ApprovalGate as G;
    use ConditionValue::Boolean as flag;
    use PipelineField as F;
    use PipelineKind::*;
    use Stage::*;

    let states = [
        (
            Requirements,
            AutoCheck,
            "r8.requirements",
            None,
            &[G::Start][..],
        ),
        (Design, AgentTask, "r8.design", Some("implementation"), &[]),
        (
            DesignReview,
            AgentTask,
            "r8.design_review",
            Some("review"),
            &[],
        ),
        (
            Implementation,
            AgentTask,
            "r8.work",
            Some("implementation"),
            &[],
        ),
        (Review, AgentTask, "r8.review", Some("review"), &[]),
        (
            Landing,
            AutoAction,
            "r8.landing",
            None,
            &[G::Merge, G::Deploy],
        ),
        (MainVerification, AutoCheck, "r8.main_check", None, &[]),
        (
            FinalVerification,
            AutoAction,
            "r8.final_check",
            None,
            &[G::Deploy],
        ),
        (Complete, EndSuccess, "", None, &[]),
    ]
    .into_iter()
    .map(|(stage, kind, executor, owner, gates)| PipelineState {
        id: stage.pipeline_state_id().into(),
        kind,
        executor: (!executor.is_empty()).then(|| PipelineExecutor {
            id: executor.into(),
            version: 1,
            parameters: PipelineParameters {
                effect_gates: gates.iter().copied().collect(),
            },
        }),
        owner: owner.map(str::to_owned),
        instructions: None,
        timeout: None,
        title: None,
    })
    .chain(std::iter::once(PipelineState {
        id: "failed".into(),
        kind: EndFailure,
        executor: None,
        owner: None,
        instructions: None,
        timeout: None,
        title: None,
    }))
    .collect::<Vec<_>>();

    let mut transitions = Vec::new();
    for state in &states[..8] {
        transitions.push(edge(
            &state.id,
            "failed",
            5,
            cmp(F::StateOutcome, text("failed")),
        ));
    }
    for (from, to, priority, extra, gate) in [
        (
            Requirements,
            Design,
            20,
            vec![
                cmp(F::PolicyDesignEnabled, flag(true)),
                cmp(F::RequirementsReady, flag(true)),
            ],
            Some(G::Implementation),
        ),
        (
            Requirements,
            Implementation,
            30,
            vec![
                cmp(F::PolicyDesignEnabled, flag(false)),
                cmp(F::RequirementsReady, flag(true)),
            ],
            Some(G::Implementation),
        ),
        (Design, DesignReview, 20, vec![], Some(G::Review)),
        (
            DesignReview,
            Implementation,
            20,
            vec![cmp(F::ReviewVerdict, text("clean"))],
            Some(G::Implementation),
        ),
        (
            Implementation,
            Review,
            20,
            vec![
                cmp(F::WorkSealValid, flag(true)),
                cmp(F::VerificationReviewStartPassed, flag(true)),
            ],
            Some(G::Review),
        ),
        (
            Review,
            Landing,
            20,
            vec![
                cmp(F::ReviewVerdict, text("clean")),
                cmp(F::ReviewP0Count, ConditionValue::Integer(0)),
                cmp(F::ReviewP1Count, ConditionValue::Integer(0)),
                cmp(F::ReviewScopeComplete, flag(true)),
                cmp(F::VerificationVerdict, text("pass")),
            ],
            Some(G::Landing),
        ),
        (
            Landing,
            MainVerification,
            20,
            vec![
                cmp(F::PrMerged, flag(true)),
                cmp(F::MainContainsCandidate, flag(true)),
            ],
            None,
        ),
        (
            MainVerification,
            Complete,
            20,
            vec![
                cmp(F::AcceptanceComplete, flag(true)),
                cmp(F::EvidenceCurrent, flag(true)),
                cmp(F::ActionsSettled, flag(true)),
            ],
            None,
        ),
        (
            MainVerification,
            FinalVerification,
            30,
            vec![cmp(F::AcceptanceComplete, flag(false))],
            None,
        ),
        (
            FinalVerification,
            Complete,
            20,
            vec![
                cmp(F::AcceptanceComplete, flag(true)),
                cmp(F::EvidenceCurrent, flag(true)),
                cmp(F::ActionsSettled, flag(true)),
            ],
            None,
        ),
    ] {
        let mut all = vec![cmp(F::StateOutcome, text("succeeded"))];
        all.extend(extra);
        let mut transition = edge(
            from.pipeline_state_id(),
            to.pipeline_state_id(),
            priority,
            PipelineCondition::All(PipelineAll { all }),
        );
        transition.gate = gate;
        transitions.push(transition);
    }
    for (from, to, budget, max) in [
        (DesignReview, Design, "design_repair", 1),
        (Implementation, Implementation, "quality_repair", 3),
        (Review, Implementation, "quality_repair", 3),
    ] {
        let mut transition = edge(
            from.pipeline_state_id(),
            to.pipeline_state_id(),
            10,
            cmp(F::StateOutcome, text("quality_failed")),
        );
        transition.retry = Some(PipelineRetry {
            budget_id: budget.into(),
            max,
            then_to: "failed".into(),
        });
        transition.gate = Some(G::Implementation);
        transitions.push(transition);
    }

    PipelineDefinition {
        schema_version: 1,
        vocabulary_version: 1,
        evaluator_version: 1,
        registry_version: 1,
        start: Requirements.pipeline_state_id().into(),
        states,
        transitions,
        events: Vec::new(),
        policy: Default::default(),
        template_id: Some(TEMPLATE_ID.into()),
        template_version: Some(TEMPLATE_VERSION),
    }
}

fn edge(from: &str, to: &str, priority: u32, condition: PipelineCondition) -> PipelineTransition {
    PipelineTransition {
        id: format!("{from}-to-{to}"),
        from: from.into(),
        to: to.into(),
        priority,
        condition,
        retry: None,
        gate: None,
    }
}

fn text(s: &str) -> ConditionValue {
    ConditionValue::String(s.into())
}

fn cmp(field: PipelineField, value: ConditionValue) -> PipelineCondition {
    PipelineCondition::Compare(PipelineComparison {
        field,
        op: Eq,
        value,
    })
}
