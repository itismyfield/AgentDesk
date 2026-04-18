pub(crate) const AGENTDESK_REPO_ID: &str = "itismyfield/AgentDesk";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BuiltinPipelineStage {
    pub stage_name: &'static str,
    pub stage_order: i64,
    pub trigger_after: Option<&'static str>,
    pub provider: Option<&'static str>,
    pub skip_condition: Option<&'static str>,
}

pub(crate) const AGENTDESK_PIPELINE_STAGES: &[BuiltinPipelineStage] = &[
    BuiltinPipelineStage {
        stage_name: "dev-deploy",
        stage_order: 100,
        trigger_after: Some("review_pass"),
        provider: Some("self"),
        skip_condition: Some("no_rs_changes"),
    },
    BuiltinPipelineStage {
        stage_name: "e2e-test",
        stage_order: 200,
        trigger_after: None,
        provider: Some("counter"),
        skip_condition: Some("no_rs_changes"),
    },
];
