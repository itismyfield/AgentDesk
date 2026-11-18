use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalGate {
    Start,
    Implementation,
    Review,
    Landing,
    Merge,
    Deploy,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowOptions {
    pub start: bool,
    pub implementation: bool,
    pub review: bool,
    pub landing: bool,
    pub merge: bool,
    pub deploy: bool,
}

impl WorkflowOptions {
    pub fn required(&self, gate: ApprovalGate) -> bool {
        match gate {
            ApprovalGate::Start => self.start,
            ApprovalGate::Implementation => self.implementation,
            ApprovalGate::Review => self.review,
            ApprovalGate::Landing => self.landing,
            ApprovalGate::Merge => self.merge,
            ApprovalGate::Deploy => self.deploy,
        }
    }
}
