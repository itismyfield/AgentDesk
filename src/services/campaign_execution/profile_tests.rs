use super::*;
use crate::services::campaign_execution::definition::{
    DefinitionInput, NodeDefinition, VerificationKind, WORKFLOW_VERSION, validate_definition,
};
use crate::services::campaign_execution::model::{HostGate, SessionIdentity};

fn at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-28T04:00:00Z")
        .unwrap()
        .to_utc()
}

fn candidate(repository: Repository) -> Candidate {
    Candidate {
        repository,
        base_sha: "base".into(),
        head_sha: "candidate".into(),
    }
}

fn binding(candidate: &Candidate) -> AttemptBinding {
    AttemptBinding {
        campaign_id: "campaign".into(),
        run_id: "run".into(),
        node_id: "a".into(),
        series: 1,
        stage: Stage::Implementation,
        attempt_id: "attempt".into(),
        role: Role::Implementation,
        session: SessionIdentity {
            gate: HostGate {
                host: "fixture-host".into(),
                gate: "gate".into(),
                incarnation: "host-1".into(),
            },
            session: "implementation".into(),
            incarnation: "session-1".into(),
        },
        repository: candidate.repository,
        hashes: DefinitionHashes {
            requirements: "requirements".into(),
            acceptance: "acceptance".into(),
            profile: profile_hash(candidate.repository, PROFILE_VERSION).unwrap(),
        },
        workflow_version: WORKFLOW_VERSION.into(),
        base_sha: candidate.base_sha.clone(),
        candidate_sha: candidate.head_sha.clone(),
        control_version: 1,
    }
}

fn observed<T>(evidence: T) -> Observation<T> {
    Observation::Observed {
        evidence,
        observed_at: at(),
    }
}

struct FixtureAdapter {
    repository: Repository,
}

impl ProfileAdapter for FixtureAdapter {
    fn workspace(&self, candidate: &Candidate) -> Result<Workspace, ContractError> {
        if self.repository != candidate.repository {
            return Err(ContractError::CandidateMismatch);
        }
        let editing = Checkout {
            host: "fixture-host".into(),
            path: "/fixture/editing".into(),
            observed_sha: candidate.head_sha.clone(),
            observed_at: at(),
        };
        Ok(Workspace {
            candidate: candidate.clone(),
            verification: Checkout {
                path: "/fixture/verification".into(),
                ..editing.clone()
            },
            editing,
            verification_kind: match self.repository {
                Repository::Adk => VerificationCheckoutKind::Isolated,
                Repository::CookingHeart => VerificationCheckoutKind::Shared,
            },
        })
    }

    fn verification_plan(
        &self,
        stage: Stage,
        candidate: &Candidate,
    ) -> Result<VerificationPlan, ContractError> {
        self.workspace(candidate)?;
        Ok(VerificationPlan {
            candidate: candidate.clone(),
            stage,
            acceptance: vec![AcceptanceCondition {
                id: "exact-candidate".into(),
                description: "Verify exact candidate".into(),
                verification: VerificationKind::Test,
            }],
            checks: vec![
                VerificationCheck {
                    id: "targeted-tests".into(),
                    kind: CheckKind::Test,
                    required: true,
                    acceptance_ids: vec!["exact-candidate".into()],
                },
                VerificationCheck {
                    id: "required-ci".into(),
                    kind: CheckKind::RequiredCi,
                    required: true,
                    acceptance_ids: vec![],
                },
            ],
            environment_requirements: RepositoryProfile::for_repository(self.repository)
                .unwrap()
                .environment_requirements,
        })
    }

    fn resources(&self, _stage: Stage) -> Vec<ResourceRequirement> {
        let kinds = match self.repository {
            Repository::Adk => vec![ResourceKind::Host, ResourceKind::Build],
            Repository::CookingHeart => vec![
                ResourceKind::Host,
                ResourceKind::Build,
                ResourceKind::Editor,
                ResourceKind::SharedCheckout,
            ],
        };
        kinds
            .into_iter()
            .map(|kind| ResourceRequirement {
                resource: ResourceIdentity {
                    kind,
                    host: "fixture-host".into(),
                    identity: format!("fixture-{kind:?}"),
                },
                availability: Observation::NotYet {
                    reason: "Fixture has no resource allocator".into(),
                    observed_at: at(),
                },
            })
            .collect()
    }

    fn observe(&self, target: &ObservationTarget) -> Result<TargetObservation, ContractError> {
        Ok(TargetObservation {
            target: target.clone(),
            observation: Observation::Unknown {
                reason: "Fixture performs no external observation".into(),
                last_observed_at: Some(at()),
            },
        })
    }
}

fn execution(adapter: &FixtureAdapter) -> (VerificationPlan, AttemptBinding, TestEvidence) {
    let candidate = candidate(adapter.repository);
    let binding = binding(&candidate);
    let plan = adapter
        .verification_plan(binding.stage, &candidate)
        .unwrap();
    let workspace = adapter.workspace(&candidate).unwrap();
    let evidence = TestEvidence {
        execution_id: "fixture-execution".into(),
        binding: binding.clone(),
        environment: ExecutionEnvironment {
            os: "fixture-os".into(),
            checkout: workspace.verification,
            components: plan
                .environment_requirements
                .iter()
                .map(|component| ComponentObservation {
                    component: *component,
                    observation: Observation::Unknown {
                        reason: "No actual environment was observed".into(),
                        last_observed_at: None,
                    },
                })
                .collect(),
            allocations: vec![],
        },
        planned: vec![PlannedTest {
            id: "candidate-test".into(),
            check_id: "targeted-tests".into(),
            required: true,
        }],
        actual: vec![ExecutedTest {
            id: "candidate-test".into(),
            check_id: "targeted-tests".into(),
            outcome: TestOutcome::Passed,
            log_refs: vec!["fixture://test-log".into()],
        }],
        log_refs: vec!["fixture://execution-log".into()],
    };
    (plan, binding, evidence)
}

#[test]
fn both_profiles_share_candidate_plan_resource_and_unknown_contract() {
    for repository in [Repository::Adk, Repository::CookingHeart] {
        let adapter = FixtureAdapter { repository };
        let profile = RepositoryProfile::for_repository(repository).unwrap();
        let candidate = candidate(repository);
        let workspace = adapter.workspace(&candidate).unwrap();
        workspace.validate(&candidate).unwrap();
        candidate.validate_binding(&binding(&candidate)).unwrap();
        let plan = adapter
            .verification_plan(Stage::Implementation, &candidate)
            .unwrap();
        plan.validate().unwrap();
        assert!(
            plan.checks
                .iter()
                .any(|c| c.required && c.kind == CheckKind::RequiredCi)
        );
        assert_eq!(profile.readiness, ExecutionReadiness::Unavailable);
        assert!(profile.unavailable_reason.contains("not implemented"));
        assert_eq!(
            (
                profile.limits.concurrent_tasks,
                profile.limits.quality_repairs,
                profile.limits.design_review_rounds
            ),
            (2, 3, 2)
        );
        for role in [Role::Implementation, Role::Review] {
            assert!(profile.models.iter().any(|m| m.role == role && matches!(&m.selection, ModelDefault::Unspecified { reason } if !reason.is_empty())));
        }
        assert!(
            adapter
                .resources(Stage::Implementation)
                .iter()
                .all(|r| r.availability.observed_evidence().is_none())
        );
        if repository == Repository::CookingHeart {
            for component in [
                EnvironmentComponent::Engine,
                EnvironmentComponent::ResourcesLock,
                EnvironmentComponent::LoreAssets,
                EnvironmentComponent::MonolithSubmodule,
                EnvironmentComponent::Mcp,
                EnvironmentComponent::Editor,
                EnvironmentComponent::Pie,
            ] {
                assert!(plan.environment_requirements.contains(&component));
            }
        }
        let target = ObservationTarget::Ci {
            repository,
            run_id: "ci-run".into(),
            head_sha: candidate.head_sha,
        };
        let observation = adapter.observe(&target).unwrap();
        observation.validate_target(&target).unwrap();
        assert!(observation.observation.observed_evidence().is_none());
    }
}

#[test]
fn profile_hash_is_the_definition_hash_for_both_repositories() {
    for repository in [Repository::Adk, Repository::CookingHeart] {
        let definition = validate_definition(DefinitionInput {
            repository,
            title: "Contract".into(),
            goal: "Preserve exact evidence".into(),
            profile_version: PROFILE_VERSION.into(),
            nodes: vec![NodeDefinition {
                id: "a".into(),
                title: "Implement".into(),
                issue_url: "https://github.com/itismyfield/AgentDesk/issues/7438".into(),
                acceptance: vec![AcceptanceCondition {
                    id: "test".into(),
                    description: "Targeted tests".into(),
                    verification: VerificationKind::Test,
                }],
                dependencies: vec![],
                group: None,
            }],
        })
        .unwrap();
        let profile = RepositoryProfile::for_repository(repository).unwrap();
        assert_eq!(profile.hash, definition.hashes()["a"].profile);
        assert_ne!(
            profile.hash,
            profile_hash(repository, "next-version").unwrap()
        );
    }
    assert_ne!(
        RepositoryProfile::for_repository(Repository::Adk)
            .unwrap()
            .hash,
        RepositoryProfile::for_repository(Repository::CookingHeart)
            .unwrap()
            .hash
    );
}

#[test]
fn both_profiles_reject_candidate_or_binding_drift() {
    for repository in [Repository::Adk, Repository::CookingHeart] {
        let adapter = FixtureAdapter { repository };
        let candidate = candidate(repository);
        for field in ["repository", "base", "head"] {
            let mut wrong = binding(&candidate);
            match field {
                "repository" => {
                    wrong.repository = if repository == Repository::Adk {
                        Repository::CookingHeart
                    } else {
                        Repository::Adk
                    }
                }
                "base" => wrong.base_sha = "different-base".into(),
                _ => wrong.candidate_sha = "different-head".into(),
            }
            assert_eq!(
                candidate.validate_binding(&wrong),
                Err(ContractError::CandidateMismatch)
            );
        }
        for editing in [false, true] {
            let mut workspace = adapter.workspace(&candidate).unwrap();
            if editing {
                workspace.editing.observed_sha = "old".into();
            } else {
                workspace.verification.observed_sha = "old".into();
            }
            assert_eq!(
                workspace.validate(&candidate),
                Err(ContractError::CandidateMismatch)
            );
        }
        let (plan, binding, mut evidence) = execution(&adapter);
        evidence.binding.attempt_id = "old-attempt".into();
        assert_eq!(
            evidence.validate_execution(&plan, &binding),
            Err(ContractError::BindingMismatch)
        );
        evidence.binding = binding.clone();
        evidence.environment.checkout.observed_sha = "old".into();
        assert_eq!(
            evidence.validate_execution(&plan, &binding),
            Err(ContractError::CandidateMismatch)
        );
    }
}

#[test]
fn ch_rejects_editing_checkout_as_shared_verification_but_allows_another_host() {
    let adapter = FixtureAdapter {
        repository: Repository::CookingHeart,
    };
    let candidate = candidate(adapter.repository);
    let mut workspace = adapter.workspace(&candidate).unwrap();
    workspace.verification = workspace.editing.clone();
    assert_eq!(
        workspace.validate(&candidate),
        Err(ContractError::SharedCheckoutRequired)
    );
    workspace.verification.host = "another-host".into();
    workspace.validate(&candidate).unwrap();
    workspace.verification_kind = VerificationCheckoutKind::Isolated;
    assert_eq!(
        workspace.validate(&candidate),
        Err(ContractError::SharedCheckoutRequired)
    );
    let adapter = FixtureAdapter {
        repository: Repository::Adk,
    };
    let candidate = self::candidate(adapter.repository);
    let mut workspace = adapter.workspace(&candidate).unwrap();
    workspace.verification_kind = VerificationCheckoutKind::Shared;
    assert_eq!(
        workspace.validate(&candidate),
        Err(ContractError::IsolatedCheckoutRequired)
    );
}

#[test]
fn both_profiles_reject_empty_required_plans_and_broken_ac_links() {
    for repository in [Repository::Adk, Repository::CookingHeart] {
        let adapter = FixtureAdapter { repository };
        let (mut plan, binding, evidence) = execution(&adapter);
        plan.checks.clear();
        assert_eq!(plan.validate(), Err(ContractError::NoRequiredChecks));
        assert_eq!(
            evidence.validate_execution(&plan, &binding),
            Err(ContractError::NoRequiredChecks)
        );
        let (mut plan, _, _) = execution(&adapter);
        for check in &mut plan.checks {
            check.required = false;
        }
        assert_eq!(plan.validate(), Err(ContractError::NoRequiredChecks));
        plan.checks[0].required = true;
        plan.checks[0].acceptance_ids = vec!["missing-ac".into()];
        assert_eq!(
            plan.validate(),
            Err(ContractError::UnknownAcceptance("missing-ac".into()))
        );
        plan.checks[0].acceptance_ids.clear();
        plan.checks[1].id = plan.checks[0].id.clone();
        assert_eq!(
            plan.validate(),
            Err(ContractError::DuplicateId("targeted-tests".into()))
        );
    }
}

#[test]
fn both_profiles_reject_zero_missing_skipped_unknown_and_duplicate_required_tests() {
    for repository in [Repository::Adk, Repository::CookingHeart] {
        let adapter = FixtureAdapter { repository };
        let (plan, binding, evidence) = execution(&adapter);
        evidence.validate_execution(&plan, &binding).unwrap();
        let mut changed = evidence.clone();
        changed.actual.clear();
        assert_eq!(
            changed.validate_execution(&plan, &binding),
            Err(ContractError::ZeroTests)
        );
        changed.actual = evidence.actual.clone();
        changed.actual[0].id = "other-test".into();
        assert_eq!(
            changed.validate_execution(&plan, &binding),
            Err(ContractError::RequiredTestMissing("candidate-test".into()))
        );
        for outcome in [
            TestOutcome::Skipped {
                reason: "environment missing".into(),
            },
            TestOutcome::Unknown {
                reason: "log lost".into(),
            },
        ] {
            changed = evidence.clone();
            changed.actual[0].outcome = outcome;
            assert_eq!(
                changed.validate_execution(&plan, &binding),
                Err(ContractError::RequiredTestUnexecuted(
                    "candidate-test".into()
                ))
            );
        }
        changed = evidence.clone();
        changed.planned.clear();
        assert_eq!(
            changed.validate_execution(&plan, &binding),
            Err(ContractError::RequiredTestMissing("targeted-tests".into()))
        );
        changed = evidence.clone();
        changed.planned[0].required = false;
        assert_eq!(
            changed.validate_execution(&plan, &binding),
            Err(ContractError::RequiredTestMissing("targeted-tests".into()))
        );
        changed = evidence.clone();
        changed.actual.push(evidence.actual[0].clone());
        assert_eq!(
            changed.validate_execution(&plan, &binding),
            Err(ContractError::DuplicateId("candidate-test".into()))
        );
        changed = evidence.clone();
        changed.planned.push(evidence.planned[0].clone());
        assert_eq!(
            changed.validate_execution(&plan, &binding),
            Err(ContractError::DuplicateId("candidate-test".into()))
        );
        changed = evidence.clone();
        changed.planned[0].check_id = "required-ci".into();
        assert_eq!(
            changed.validate_execution(&plan, &binding),
            Err(ContractError::UnknownCheck("required-ci".into()))
        );
        changed = evidence.clone();
        changed.execution_id.clear();
        assert_eq!(
            changed.validate_execution(&plan, &binding),
            Err(ContractError::MissingExecutionIdentity)
        );
        changed = evidence;
        changed.log_refs.clear();
        assert_eq!(
            changed.validate_execution(&plan, &binding),
            Err(ContractError::MissingExecutionIdentity)
        );
    }
}

#[test]
fn execution_completeness_preserves_failures_optional_skips_and_unknown_environment() {
    for repository in [Repository::Adk, Repository::CookingHeart] {
        let (plan, binding, mut evidence) = execution(&FixtureAdapter { repository });
        evidence.actual[0].outcome = TestOutcome::Failed;
        evidence.planned.push(PlannedTest {
            id: "optional".into(),
            check_id: "targeted-tests".into(),
            required: false,
        });
        evidence.actual.push(ExecutedTest {
            id: "optional".into(),
            check_id: "targeted-tests".into(),
            outcome: TestOutcome::Skipped {
                reason: "fixture".into(),
            },
            log_refs: vec![],
        });
        evidence.environment.allocations.push(ResourceAllocation {
            resource: ResourceIdentity {
                kind: ResourceKind::Build,
                host: "fixture-host".into(),
                identity: "fixture-target".into(),
            },
            allocation_id: "fixture-allocation".into(),
        });
        evidence.validate_execution(&plan, &binding).unwrap();
        let encoded = serde_json::to_string(&evidence).unwrap();
        let restored: TestEvidence = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(restored.actual[0].outcome, TestOutcome::Failed));
        assert!(
            matches!(&restored.actual[1].outcome, TestOutcome::Skipped { reason } if reason == "fixture")
        );
        assert!(
            restored
                .environment
                .components
                .iter()
                .all(|c| c.observation.observed_evidence().is_none())
        );
        assert_eq!(restored.actual.len(), 2);
        assert_eq!(restored.planned.len(), 2);
    }
}

fn artifacts(repository: Repository) -> Vec<NodeArtifact> {
    ["a", "b"]
        .into_iter()
        .map(|id| NodeArtifact {
            node_id: id.into(),
            candidate: Candidate {
                head_sha: format!("candidate-{id}"),
                ..candidate(repository)
            },
            hashes: binding(&candidate(repository)).hashes,
            review: ReviewRange {
                base_sha: format!("review-base-{id}"),
                head_sha: format!("candidate-{id}"),
                extent: if id == "a" {
                    ReviewExtent::Full
                } else {
                    ReviewExtent::Delta
                },
            },
        })
        .collect()
}

fn main_evidence(repository: Repository, method: IntegrationMethod) -> MainIntegrationEvidence {
    MainIntegrationEvidence {
        repository,
        target_branch: "main".into(),
        target_main_sha: "main-sha".into(),
        pr_number: 7438,
        merge_sha: "merge-sha".into(),
        method,
        nodes: artifacts(repository)
            .into_iter()
            .map(|artifact| NodeMainMapping {
                artifact,
                main_commits: vec!["merge-sha".into()],
                ancestry: Observation::NotYet {
                    reason: "Ancestry not inspected".into(),
                    observed_at: at(),
                },
                content: Observation::Unknown {
                    reason: "Content not inspected".into(),
                    last_observed_at: None,
                },
            })
            .collect(),
    }
}

#[test]
fn both_profiles_preserve_grouped_main_mappings_for_merge_and_squash() {
    for repository in [Repository::Adk, Repository::CookingHeart] {
        for method in [IntegrationMethod::Merge, IntegrationMethod::Squash] {
            let requested = artifacts(repository);
            let evidence = main_evidence(repository, method);
            evidence.validate_mappings(&requested).unwrap();
            let restored: MainIntegrationEvidence =
                serde_json::from_str(&serde_json::to_string(&evidence).unwrap()).unwrap();
            restored.validate_mappings(&requested).unwrap();
            assert_eq!(restored.nodes.len(), 2);
            assert!(
                restored
                    .nodes
                    .iter()
                    .all(|n| n.ancestry.observed_evidence().is_none()
                        && n.content.observed_evidence().is_none())
            );
            for mutation in [
                "drop",
                "duplicate",
                "candidate",
                "review",
                "hash",
                "commits",
            ] {
                let mut changed = restored.clone();
                match mutation {
                    "drop" => {
                        changed.nodes.pop();
                    }
                    "duplicate" => changed.nodes[1] = changed.nodes[0].clone(),
                    "candidate" => changed.nodes[0].artifact.candidate.head_sha = "wrong".into(),
                    "review" => changed.nodes[0].artifact.review.base_sha = "wrong".into(),
                    "hash" => changed.nodes[0].artifact.hashes.acceptance = "wrong".into(),
                    _ => changed.nodes[0].main_commits.clear(),
                }
                assert_eq!(
                    changed.validate_mappings(&requested),
                    Err(ContractError::NodeMappingMismatch),
                    "{mutation}"
                );
            }
        }
    }
}

#[test]
fn observations_keep_exact_targets_and_unknown_never_becomes_observed() {
    for repository in [Repository::Adk, Repository::CookingHeart] {
        let main = main_evidence(repository, IntegrationMethod::Squash);
        let pairs = vec![
            (
                ObservationTarget::Pr {
                    repository,
                    number: 7438,
                },
                ExternalEvidence::Pr {
                    head_sha: "candidate".into(),
                    merge_sha: Some("merge-sha".into()),
                    reference: "fixture://pr".into(),
                },
            ),
            (
                ObservationTarget::Ci {
                    repository,
                    run_id: "run-1".into(),
                    head_sha: "candidate".into(),
                },
                ExternalEvidence::Ci {
                    conclusion: "success".into(),
                    reference: "fixture://ci".into(),
                },
            ),
            (
                ObservationTarget::Main {
                    repository,
                    branch: "main".into(),
                    target_sha: "main-sha".into(),
                },
                ExternalEvidence::Main(main),
            ),
            (
                ObservationTarget::Deploy {
                    repository,
                    host: "fixture-host".into(),
                    deployment_id: "deployment-1".into(),
                    expected_sha: "main-sha".into(),
                },
                ExternalEvidence::Deploy {
                    actual_sha: "old-deployment".into(),
                    reference: "fixture://deploy".into(),
                },
            ),
        ];
        for (target, evidence) in pairs {
            let mut observation = TargetObservation {
                target: target.clone(),
                observation: observed(evidence),
            };
            observation.validate_target(&target).unwrap();
            let restored: TargetObservation =
                serde_json::from_str(&serde_json::to_string(&observation).unwrap()).unwrap();
            restored.validate_target(&target).unwrap();
            assert!(restored.observation.observed_evidence().is_some());
            let other = ObservationTarget::Pr {
                repository,
                number: 9999,
            };
            assert_eq!(
                restored.validate_target(&other),
                Err(ContractError::ObservationTargetMismatch)
            );
            observation.observation = Observation::Unknown {
                reason: "host unreachable".into(),
                last_observed_at: Some(at()),
            };
            let restored: TargetObservation =
                serde_json::from_str(&serde_json::to_string(&observation).unwrap()).unwrap();
            assert!(restored.observation.observed_evidence().is_none());
            observation.observation = Observation::NotYet {
                reason: "still pending".into(),
                observed_at: at(),
            };
            assert!(observation.observation.observed_evidence().is_none());
        }
        let target = ObservationTarget::Main {
            repository,
            branch: "main".into(),
            target_sha: "other-main".into(),
        };
        let observation = TargetObservation {
            target: target.clone(),
            observation: observed(ExternalEvidence::Main(main_evidence(
                repository,
                IntegrationMethod::Merge,
            ))),
        };
        assert_eq!(
            observation.validate_target(&target),
            Err(ContractError::ObservationTargetMismatch)
        );
        let observation = TargetObservation {
            target: target.clone(),
            observation: observed(ExternalEvidence::Ci {
                conclusion: "success".into(),
                reference: "fixture://ci".into(),
            }),
        };
        assert_eq!(
            observation.validate_target(&target),
            Err(ContractError::ObservationTargetMismatch)
        );
    }
}
