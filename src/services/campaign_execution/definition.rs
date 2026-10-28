use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::model::Repository;
use crate::utils::dag::{self, DagError};

pub const WORKFLOW_VERSION: &str = "campaign-v1";
pub const PROFILE_VERSION: &str = "v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefinitionInput {
    pub repository: Repository,
    pub title: String,
    pub goal: String,
    pub profile_version: String,
    pub nodes: Vec<NodeDefinition>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeDefinition {
    pub id: String,
    pub title: String,
    pub issue_url: String,
    pub acceptance: Vec<AcceptanceCondition>,
    pub dependencies: Vec<String>,
    pub group: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceCondition {
    pub id: String,
    pub description: String,
    pub verification: VerificationKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationKind {
    Test,
    Main,
    Deployment,
    Operational,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefinitionHashes {
    pub requirements: String,
    pub acceptance: String,
    pub profile: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatedDefinition {
    input: DefinitionInput,
    hashes: BTreeMap<String, DefinitionHashes>,
}

impl ValidatedDefinition {
    pub fn input(&self) -> &DefinitionInput {
        &self.input
    }
    pub fn hashes(&self) -> &BTreeMap<String, DefinitionHashes> {
        &self.hashes
    }
    pub fn superseded_nodes(&self, next: &Self) -> Vec<String> {
        self.hashes
            .iter()
            .filter(|(id, hashes)| next.hashes.get(*id) != Some(*hashes))
            .map(|(id, _)| id.clone())
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code:?}: {target:?} {path:?}")]
pub struct DefinitionError {
    pub code: DefinitionErrorCode,
    pub target: Option<String>,
    pub path: Vec<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefinitionErrorCode {
    InvalidField,
    UnsupportedProfile,
    EmptyAcceptance,
    DuplicateAcceptance,
    DuplicateNode,
    MissingDependency,
    RepeatedDependency,
    Cycle,
}

fn error(code: DefinitionErrorCode, target: impl Into<String>) -> DefinitionError {
    DefinitionError {
        code,
        target: Some(target.into()),
        path: Vec::new(),
    }
}
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
fn text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max
}
fn hash(value: &impl Serialize) -> Result<String, DefinitionError> {
    serde_json::to_vec(value)
        .map(|bytes| blake3::hash(&bytes).to_hex().to_string())
        .map_err(|_| error(DefinitionErrorCode::InvalidField, "encoding"))
}
fn graph_error(value: DagError) -> DefinitionError {
    use DefinitionErrorCode as C;
    match value {
        DagError::DuplicateNode(id) => error(C::DuplicateNode, id),
        DagError::MissingDependency { node, dependency } => DefinitionError {
            code: C::MissingDependency,
            target: Some(node),
            path: vec![dependency],
        },
        DagError::RepeatedDependency { node, dependency } => DefinitionError {
            code: C::RepeatedDependency,
            target: Some(node),
            path: vec![dependency],
        },
        DagError::Cycle(path) => DefinitionError {
            code: C::Cycle,
            target: None,
            path,
        },
    }
}

pub fn validate_definition(input: DefinitionInput) -> Result<ValidatedDefinition, DefinitionError> {
    use DefinitionErrorCode as C;
    if !text(&input.title, 512)
        || !text(&input.goal, 64_000)
        || input.nodes.is_empty()
        || input.nodes.len() > 1000
    {
        return Err(error(C::InvalidField, "definition"));
    }
    if input.profile_version != PROFILE_VERSION {
        return Err(error(C::UnsupportedProfile, "profile_version"));
    }
    let mut ids = std::collections::HashMap::new();
    let mut hashes = BTreeMap::new();
    let profile = hash(&(input.repository, &input.profile_version, WORKFLOW_VERSION))?;
    for node in &input.nodes {
        dag::insert_node(&mut ids, &node.id).map_err(graph_error)?;
        let issue = url::Url::parse(&node.issue_url).ok();
        let valid_issue = issue.as_ref().is_some_and(|url| {
            let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
            url.scheme() == "https"
                && url.host_str() == Some("github.com")
                && parts.len() == 4
                && parts[2] == "issues"
                && parts[3].parse::<u64>().is_ok_and(|n| n > 0)
        });
        if !valid_id(&node.id)
            || !text(&node.title, 512)
            || !valid_issue
            || node.group.as_ref().is_some_and(|g| !text(g, 512))
        {
            return Err(error(C::InvalidField, &node.id));
        }
        if node.acceptance.is_empty() {
            return Err(error(C::EmptyAcceptance, &node.id));
        }
        let mut acs = BTreeMap::new();
        for ac in &node.acceptance {
            if !valid_id(&ac.id) || !text(&ac.description, 16_000) {
                return Err(error(C::InvalidField, &ac.id));
            }
            if acs.insert(&ac.id, ac).is_some() {
                return Err(error(C::DuplicateAcceptance, &ac.id));
            }
        }
        hashes.insert(
            node.id.clone(),
            DefinitionHashes {
                requirements: hash(&(&input.goal, &node.issue_url))?,
                acceptance: hash(&acs)?,
                profile: profile.clone(),
            },
        );
    }
    dag::validate_dependencies(
        &ids,
        input
            .nodes
            .iter()
            .map(|n| (n.id.as_str(), n.dependencies.as_slice())),
    )
    .map_err(graph_error)?;
    Ok(ValidatedDefinition { input, hashes })
}

pub fn graph_changed(old: &ValidatedDefinition, next: &ValidatedDefinition) -> bool {
    let edges = |definition: &ValidatedDefinition| -> BTreeSet<(String, String)> {
        definition
            .input
            .nodes
            .iter()
            .flat_map(|node| {
                node.dependencies
                    .iter()
                    .map(|dep| (dep.clone(), node.id.clone()))
            })
            .collect()
    };
    edges(old) != edges(next)
}

#[cfg(test)]
#[path = "definition_tests.rs"]
mod tests;
