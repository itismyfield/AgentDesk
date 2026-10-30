use std::collections::BTreeSet;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::adk_observe::{MainObservation, PrObservation, is_sha};
use super::model::Repository;
use super::profile::{
    IntegrationCheck, IntegrationMethod, MainIntegrationEvidence, NodeArtifact, NodeMainMapping,
    Observation, ReviewRange,
};
use crate::services::git::GitCommand;

// The landing candidate a PR carried: one member for a single-node PR, several for a bundle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationCandidate {
    pub repository: Repository,
    pub base_sha: String,
    pub head_sha: String,
    pub members: Vec<NodeArtifact>,
    // Needed whenever base..head is more than the members' own reviewed ranges.
    pub integration_review: Option<ReviewRange>,
    pub integration_verified_sha: Option<String>,
}

pub struct AdoptionRequest<'a> {
    // A clone holding the fetched main and the PR/member commits.
    pub repo: &'a Path,
    pub target_branch: &'a str,
    pub requested: &'a [NodeArtifact],
    pub integration: &'a IntegrationCandidate,
    pub pr: &'a PrObservation,
    pub main: &'a MainObservation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum AdoptionRejection {
    StackedBase { base_ref: String },
    MemberMismatch { detail: String },
    HeadMismatch { landed: String, sealed: String },
    UnrecognizedLanding { merge_sha: String },
    ContentMismatch { merge_sha: String },
    MemberNotIntegrated { node_id: String },
    IntegrationChangeUnreviewed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum AdoptionVerdict {
    Adopted,
    Rejected(AdoptionRejection),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MainAdoption {
    pub pr_number: u64,
    pub merge_sha: Option<String>,
    pub main_sha: String,
    pub main_fetched_at: DateTime<Utc>,
    pub evidence: Option<MainIntegrationEvidence>,
    pub verdict: AdoptionVerdict,
}

impl MainAdoption {
    pub fn is_adopted(&self) -> bool {
        self.verdict == AdoptionVerdict::Adopted
    }
}

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    GitCommand::new()
        .repo(repo)
        .args(args)
        .run_text()
        .map(|out| out.trim().to_owned())
        .map_err(|e| e.to_string())
}

fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Result<bool, String> {
    match GitCommand::new()
        .repo(repo)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .run_output()
    {
        Ok(_) => Ok(true),
        Err(error) if error.status_code() == Some(1) => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

// Tree of `onto` merged with `head`, or None when the merge conflicts.
fn merged_tree(repo: &Path, onto: &str, head: &str) -> Result<Option<String>, String> {
    match GitCommand::new()
        .repo(repo)
        .args(["merge-tree", "--write-tree", "--no-messages", onto, head])
        .run_text()
    {
        Ok(out) => Ok(out.lines().next().map(str::to_owned)),
        Err(error) if error.status_code() == Some(1) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn rejected(base: MainAdoption, reason: AdoptionRejection) -> MainAdoption {
    MainAdoption {
        verdict: AdoptionVerdict::Rejected(reason),
        ..base
    }
}

fn member_mismatch(base: MainAdoption, detail: impl Into<String>) -> MainAdoption {
    rejected(
        base,
        AdoptionRejection::MemberMismatch {
            detail: detail.into(),
        },
    )
}

// Every requested node is a member exactly once and nothing else rides along.
fn members_match(request: &AdoptionRequest<'_>) -> Result<(), String> {
    let members = &request.integration.members;
    if request.requested.is_empty() || members.len() != request.requested.len() {
        return Err(format!(
            "{} requested nodes, {} members",
            request.requested.len(),
            members.len()
        ));
    }
    let mut ids = BTreeSet::new();
    for member in members {
        if !ids.insert(&member.node_id) || !request.requested.contains(member) {
            return Err(format!("member {} is not a requested node", member.node_id));
        }
        if member.candidate.repository != request.integration.repository {
            return Err(format!(
                "member {} is in another repository",
                member.node_id
            ));
        }
        if member.review.head_sha != member.candidate.head_sha {
            return Err(format!(
                "review of {} does not cover its seal",
                member.node_id
            ));
        }
    }
    Ok(())
}

// The members' reviewed ranges chain base..head with no gap: integration added nothing of its own.
fn members_cover_integration(integration: &IntegrationCandidate) -> bool {
    let mut cursor = integration.base_sha.as_str();
    let mut left: Vec<&NodeArtifact> = integration.members.iter().collect();
    while let Some(pos) = left.iter().position(|m| m.candidate.base_sha == cursor) {
        cursor = left.swap_remove(pos).candidate.head_sha.as_str();
    }
    left.is_empty() && cursor == integration.head_sha
}

fn integration_change_covered(integration: &IntegrationCandidate) -> bool {
    integration
        .integration_review
        .as_ref()
        .is_some_and(|review| {
            review.base_sha == integration.base_sha && review.head_sha == integration.head_sha
        })
        && integration.integration_verified_sha.as_deref() == Some(integration.head_sha.as_str())
}

// Main adoption shared by eligibility and landing. No closed/CLEAN/CI/issue-text signal is an input: the
// merged PR, a fresh remote main fetch and the git graph decide.
pub fn judge_main_adoption(request: &AdoptionRequest<'_>) -> Observation<MainAdoption> {
    let (pr, main, integration) = (request.pr, request.main, request.integration);
    let unknown = |reason: String| Observation::Unknown {
        reason,
        last_observed_at: Some(main.fetched_at),
    };
    if pr.repository_identity != main.repository_identity
        || pr.repository != integration.repository
        || main.branch != request.target_branch
    {
        return unknown("PR, main observation and integration name different targets".into());
    }
    let (Some(merge_sha), Some(merged_at)) = (pr.merge_sha.clone(), pr.merged_at) else {
        return Observation::NotYet {
            reason: format!("PR #{} is not merged ({})", pr.number, pr.state),
            observed_at: main.fetched_at,
        };
    };
    if main.fetched_at < merged_at {
        return unknown("main was fetched before the PR merged".into());
    }
    let base = MainAdoption {
        pr_number: pr.number,
        merge_sha: Some(merge_sha.clone()),
        main_sha: main.main_sha.clone(),
        main_fetched_at: main.fetched_at,
        evidence: None,
        verdict: AdoptionVerdict::Adopted,
    };
    let observed = |adoption: MainAdoption| Observation::Observed {
        evidence: adoption,
        observed_at: main.fetched_at,
    };
    if pr.base_ref != request.target_branch {
        return observed(rejected(
            base,
            AdoptionRejection::StackedBase {
                base_ref: pr.base_ref.clone(),
            },
        ));
    }
    if let Err(detail) = members_match(request) {
        return observed(member_mismatch(base, detail));
    }
    if pr.head_sha != integration.head_sha {
        return observed(rejected(
            base,
            AdoptionRejection::HeadMismatch {
                landed: pr.head_sha.clone(),
                sealed: integration.head_sha.clone(),
            },
        ));
    }
    match is_ancestor(request.repo, &merge_sha, &main.main_sha) {
        Ok(true) => {}
        Ok(false) => {
            return Observation::NotYet {
                reason: format!("merge {merge_sha} is not in fetched main {}", main.main_sha),
                observed_at: main.fetched_at,
            };
        }
        Err(reason) => return unknown(reason),
    }
    let parents = match git(
        request.repo,
        &["rev-list", "--parents", "-n", "1", &merge_sha],
    ) {
        Ok(line) => line
            .split_whitespace()
            .skip(1)
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        Err(reason) => return unknown(reason),
    };
    let method = match parents.as_slice() {
        [_, second] if *second == pr.head_sha => IntegrationMethod::Merge,
        [_] => IntegrationMethod::Squash,
        _ => {
            return observed(rejected(
                base,
                AdoptionRejection::UnrecognizedLanding { merge_sha },
            ));
        }
    };
    // Merge or squash, the landed tree must equal the PR head merged onto the landing parent:
    // conflict resolution or edits made while landing are not the reviewed candidate.
    let landed_tree = git(
        request.repo,
        &["rev-parse", &format!("{merge_sha}^{{tree}}")],
    );
    let expected_tree = merged_tree(request.repo, &parents[0], &pr.head_sha);
    match (landed_tree, expected_tree) {
        (Ok(landed), Ok(Some(expected))) if landed == expected && is_sha(&landed) => {}
        (Ok(_), Ok(_)) => {
            return observed(rejected(
                base,
                AdoptionRejection::ContentMismatch { merge_sha },
            ));
        }
        (Err(reason), _) | (_, Err(reason)) => return unknown(reason),
    }
    for member in &integration.members {
        match is_ancestor(
            request.repo,
            &member.candidate.head_sha,
            &integration.head_sha,
        ) {
            Ok(true) => {}
            Ok(false) => {
                return observed(rejected(
                    base,
                    AdoptionRejection::MemberNotIntegrated {
                        node_id: member.node_id.clone(),
                    },
                ));
            }
            Err(reason) => return unknown(reason),
        }
    }
    if !members_cover_integration(integration) && !integration_change_covered(integration) {
        return observed(rejected(
            base,
            AdoptionRejection::IntegrationChangeUnreviewed,
        ));
    }
    let content_ref = format!(
        "tree of {merge_sha} equals {} merged onto {}",
        pr.head_sha, parents[0]
    );
    let evidence = MainIntegrationEvidence {
        repository: integration.repository,
        target_branch: request.target_branch.to_owned(),
        target_main_sha: main.main_sha.clone(),
        pr_number: pr.number,
        merge_sha: merge_sha.clone(),
        method,
        nodes: request
            .requested
            .iter()
            .map(|artifact| NodeMainMapping {
                artifact: artifact.clone(),
                main_commits: vec![merge_sha.clone()],
                ancestry: Observation::Observed {
                    evidence: IntegrationCheck {
                        matches: true,
                        reference: format!(
                            "{} in {}; {merge_sha} in {} fetched {}",
                            artifact.candidate.head_sha,
                            integration.head_sha,
                            main.main_sha,
                            main.fetched_at
                        ),
                    },
                    observed_at: main.fetched_at,
                },
                content: Observation::Observed {
                    evidence: IntegrationCheck {
                        matches: true,
                        reference: content_ref.clone(),
                    },
                    observed_at: main.fetched_at,
                },
            })
            .collect(),
    };
    if let Err(error) = evidence.validate_mappings(request.requested) {
        return observed(member_mismatch(base, error.to_string()));
    }
    observed(MainAdoption {
        evidence: Some(evidence),
        ..base
    })
}

#[cfg(test)]
#[path = "main_integration_tests.rs"]
mod tests;
