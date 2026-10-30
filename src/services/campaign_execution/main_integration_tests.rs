use std::path::PathBuf;

use super::*;
use crate::services::campaign_execution::adk_observe::observe_main;
use crate::services::campaign_execution::adk_observe::tests::{
    at, commit, remote_and_clone, sh_git,
};
use crate::services::campaign_execution::definition::DefinitionHashes;
use crate::services::campaign_execution::profile::{Candidate, ReviewExtent};

fn artifact(node: &str, base: &str, head: &str) -> NodeArtifact {
    NodeArtifact {
        node_id: node.into(),
        candidate: Candidate {
            repository: Repository::Adk,
            base_sha: base.into(),
            head_sha: head.into(),
        },
        hashes: DefinitionHashes {
            requirements: "r".into(),
            acceptance: "a".into(),
            profile: "p".into(),
        },
        review: ReviewRange {
            base_sha: base.into(),
            head_sha: head.into(),
            extent: ReviewExtent::Full,
        },
    }
}

struct Graph {
    _temp: tempfile::TempDir,
    work: PathBuf,
    identity: String,
    base: String,
}

impl Graph {
    fn new() -> Self {
        let (_temp, work, identity, base) = remote_and_clone();
        Self {
            _temp,
            work,
            identity,
            base,
        }
    }

    fn git(&self, args: &[&str]) -> String {
        sh_git(&self.work, args)
    }

    // A branch from `from` with one commit per file; returns each commit.
    fn branch(&self, name: &str, from: &str, files: &[&str]) -> Vec<String> {
        self.git(&["checkout", "-q", "-B", name, from]);
        let commits = files.iter().map(|f| commit(&self.work, f, f)).collect();
        self.git(&["checkout", "-q", "main"]);
        commits
    }

    fn land_merge(&self, head: &str) -> String {
        self.git(&["merge", "-q", "--no-ff", "-m", "merge", head]);
        self.push()
    }

    fn land_squash(&self, head: &str, extra: Option<&str>) -> String {
        self.git(&["merge", "-q", "--squash", head]);
        if let Some(file) = extra {
            std::fs::write(self.work.join(file), "landing edit").unwrap();
            self.git(&["add", "."]);
        }
        self.git(&["commit", "-q", "-m", "squash"]);
        self.push()
    }

    fn push(&self) -> String {
        self.git(&["push", "-q", "origin", "main"]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn pr(&self, head: &str, merge: Option<&str>) -> PrObservation {
        PrObservation {
            repository: Repository::Adk,
            repository_identity: self.identity.clone(),
            number: 77,
            state: if merge.is_some() { "closed" } else { "open" }.into(),
            head_sha: head.into(),
            head_ref: "feat/x".into(),
            base_ref: "main".into(),
            base_sha: self.base.clone(),
            merged: merge.is_some(),
            merge_sha: merge.map(str::to_owned),
            merged_at: merge.map(|_| at(5)),
            updated_at: at(5),
        }
    }

    fn main(&self) -> MainObservation {
        observe_main(&self.work, &self.identity, "main", at(10))
            .observed_evidence()
            .cloned()
            .expect("main observed")
    }

    fn judge(
        &self,
        requested: &[NodeArtifact],
        integration: &IntegrationCandidate,
        pr: &PrObservation,
    ) -> Observation<MainAdoption> {
        let main = self.main();
        judge_main_adoption(&AdoptionRequest {
            repo: &self.work,
            target_branch: "main",
            requested,
            integration,
            pr,
            main: &main,
        })
    }
}

fn integration(base: &str, head: &str, members: &[NodeArtifact]) -> IntegrationCandidate {
    IntegrationCandidate {
        repository: Repository::Adk,
        base_sha: base.into(),
        head_sha: head.into(),
        members: members.to_vec(),
        integration_review: None,
        integration_verified_sha: None,
    }
}

fn verdict(observation: &Observation<MainAdoption>) -> AdoptionVerdict {
    observation
        .observed_evidence()
        .unwrap_or_else(|| panic!("expected Observed, got {observation:?}"))
        .verdict
        .clone()
}

#[test]
fn merge_squash_and_bundle_landings_map_every_node() {
    // Merge commit.
    let g = Graph::new();
    let h1 = g.branch("n1", &g.base, &["a.txt"]).pop().unwrap();
    let merge = g.land_merge(&h1);
    let n1 = artifact("n1", &g.base, &h1);
    let observed = g.judge(
        std::slice::from_ref(&n1),
        &integration(&g.base, &h1, std::slice::from_ref(&n1)),
        &g.pr(&h1, Some(&merge)),
    );
    let adoption = observed.observed_evidence().expect("observed");
    assert!(adoption.is_adopted(), "{adoption:?}");
    let evidence = adoption.evidence.as_ref().unwrap();
    assert_eq!(evidence.method, IntegrationMethod::Merge);
    assert_eq!(evidence.nodes[0].main_commits, vec![merge]);

    // Squash: the node head is not a main ancestor, the content link still holds.
    let g = Graph::new();
    let h1 = g.branch("n1", &g.base, &["a.txt"]).pop().unwrap();
    let squash = g.land_squash(&h1, None);
    let n1 = artifact("n1", &g.base, &h1);
    assert!(!is_ancestor(&g.work, &h1, "main").unwrap());
    let observed = g.judge(
        std::slice::from_ref(&n1),
        &integration(&g.base, &h1, std::slice::from_ref(&n1)),
        &g.pr(&h1, Some(&squash)),
    );
    let adoption = observed.observed_evidence().expect("observed");
    assert!(adoption.is_adopted(), "{adoption:?}");
    assert_eq!(
        adoption.evidence.as_ref().unwrap().method,
        IntegrationMethod::Squash
    );

    // Bundle: two stacked nodes squashed in one PR.
    let g = Graph::new();
    let mut stack = g.branch("bundle", &g.base, &["a.txt", "b.txt"]);
    let h2 = stack.pop().unwrap();
    let h1 = stack.pop().unwrap();
    let squash = g.land_squash(&h2, None);
    let members = [artifact("n1", &g.base, &h1), artifact("n2", &h1, &h2)];
    let observed = g.judge(
        &members,
        &integration(&g.base, &h2, &members),
        &g.pr(&h2, Some(&squash)),
    );
    let adoption = observed.observed_evidence().expect("observed");
    assert!(adoption.is_adopted(), "{adoption:?}");
    assert_eq!(adoption.evidence.as_ref().unwrap().nodes.len(), 2);
}

#[test]
fn merged_pr_alone_is_not_main_adoption() {
    let g = Graph::new();
    let h1 = g.branch("n1", &g.base, &["a.txt"]).pop().unwrap();
    let n1 = artifact("n1", &g.base, &h1);
    let candidate = integration(&g.base, &h1, std::slice::from_ref(&n1));
    // Closed without merge.
    assert!(matches!(
        g.judge(std::slice::from_ref(&n1), &candidate, &g.pr(&h1, None)),
        Observation::NotYet { .. }
    ));
    // GitHub says merged, but the merge commit never reached the fetched remote main.
    g.git(&["checkout", "-q", "-b", "elsewhere"]);
    g.git(&["merge", "-q", "--no-ff", "-m", "merge", &h1]);
    let unpublished = g.git(&["rev-parse", "HEAD"]);
    g.git(&["checkout", "-q", "main"]);
    assert!(matches!(
        g.judge(
            std::slice::from_ref(&n1),
            &candidate,
            &g.pr(&h1, Some(&unpublished))
        ),
        Observation::NotYet { .. }
    ));
}

#[test]
fn landing_edits_and_unreviewed_integration_changes_are_rejected() {
    let g = Graph::new();
    let h1 = g.branch("n1", &g.base, &["a.txt"]).pop().unwrap();
    let squash = g.land_squash(&h1, Some("extra.txt"));
    let n1 = artifact("n1", &g.base, &h1);
    let observed = g.judge(
        std::slice::from_ref(&n1),
        &integration(&g.base, &h1, std::slice::from_ref(&n1)),
        &g.pr(&h1, Some(&squash)),
    );
    assert!(matches!(
        verdict(&observed),
        AdoptionVerdict::Rejected(AdoptionRejection::ContentMismatch { .. })
    ));

    // Two parallel nodes joined by an integration commit carrying its own edit.
    let g = Graph::new();
    let h1 = g.branch("n1", &g.base, &["a.txt"]).pop().unwrap();
    let h2 = g.branch("n2", &g.base, &["b.txt"]).pop().unwrap();
    g.git(&["checkout", "-q", "-b", "bundle", &h1]);
    g.git(&["merge", "-q", "--no-ff", "-m", "join", &h2]);
    let joined = commit(&g.work, "glue.txt", "glue");
    g.git(&["checkout", "-q", "main"]);
    let squash = g.land_squash(&joined, None);
    let members = [artifact("n1", &g.base, &h1), artifact("n2", &g.base, &h2)];
    let mut candidate = integration(&g.base, &joined, &members);
    let pr = g.pr(&joined, Some(&squash));
    assert_eq!(
        verdict(&g.judge(&members, &candidate, &pr)),
        AdoptionVerdict::Rejected(AdoptionRejection::IntegrationChangeUnreviewed)
    );
    candidate.integration_review = Some(ReviewRange {
        base_sha: g.base.clone(),
        head_sha: joined.clone(),
        extent: ReviewExtent::Full,
    });
    candidate.integration_verified_sha = Some(joined.clone());
    assert_eq!(
        verdict(&g.judge(&members, &candidate, &pr)),
        AdoptionVerdict::Adopted
    );
}

#[test]
fn bundle_member_missing_or_foreign_is_rejected() {
    let g = Graph::new();
    let mut stack = g.branch("bundle", &g.base, &["a.txt", "b.txt"]);
    let h2 = stack.pop().unwrap();
    let h1 = stack.pop().unwrap();
    let squash = g.land_squash(&h2, None);
    let n1 = artifact("n1", &g.base, &h1);
    let n2 = artifact("n2", &h1, &h2);
    let pr = g.pr(&h2, Some(&squash));
    let mut covered = integration(&g.base, &h2, std::slice::from_ref(&n2));
    covered.integration_review = Some(ReviewRange {
        base_sha: g.base.clone(),
        head_sha: h2.clone(),
        extent: ReviewExtent::Full,
    });
    covered.integration_verified_sha = Some(h2.clone());
    for (requested, members) in [
        (vec![n1.clone(), n2.clone()], vec![n2.clone()]),
        (vec![n2.clone()], vec![n1.clone(), n2.clone()]),
        (vec![n1.clone(), n2.clone()], vec![n2.clone(), n2.clone()]),
    ] {
        let candidate = IntegrationCandidate {
            members,
            ..covered.clone()
        };
        assert!(
            matches!(
                verdict(&g.judge(&requested, &candidate, &pr)),
                AdoptionVerdict::Rejected(AdoptionRejection::MemberMismatch { .. })
            ),
            "{requested:?}"
        );
    }

    // A member whose seal is not inside the landed head.
    let stray = g.branch("stray", &g.base, &["z.txt"]).pop().unwrap();
    let n3 = artifact("n3", &g.base, &stray);
    let members = [n1, n2, n3];
    let candidate = IntegrationCandidate {
        members: members.to_vec(),
        ..covered
    };
    assert_eq!(
        verdict(&g.judge(&members, &candidate, &pr)),
        AdoptionVerdict::Rejected(AdoptionRejection::MemberNotIntegrated {
            node_id: "n3".into()
        })
    );
}

#[test]
fn stacked_base_and_stale_main_do_not_adopt() {
    let g = Graph::new();
    let h1 = g.branch("n1", &g.base, &["a.txt"]).pop().unwrap();
    let merge = g.land_merge(&h1);
    let n1 = artifact("n1", &g.base, &h1);
    let candidate = integration(&g.base, &h1, std::slice::from_ref(&n1));
    let mut stacked = g.pr(&h1, Some(&merge));
    stacked.base_ref = "feat/parent".into();
    assert!(matches!(
        verdict(&g.judge(std::slice::from_ref(&n1), &candidate, &stacked)),
        AdoptionVerdict::Rejected(AdoptionRejection::StackedBase { .. })
    ));

    let mut late = g.pr(&h1, Some(&merge));
    late.merged_at = Some(at(30));
    assert!(matches!(
        g.judge(std::slice::from_ref(&n1), &candidate, &late),
        Observation::Unknown { .. }
    ));

    let other = g.branch("other", &g.base, &["o.txt"]).pop().unwrap();
    assert!(matches!(
        verdict(&g.judge(
            std::slice::from_ref(&n1),
            &candidate,
            &g.pr(&other, Some(&merge))
        )),
        AdoptionVerdict::Rejected(AdoptionRejection::HeadMismatch { .. })
    ));
}
