use super::*;
use crate::domain::campaign::execution_pin::{
    ExecutionPin, IssueObservation, PinError, RequirementsSnapshot,
};
use crate::domain::campaign::model::{
    CampaignControl, Command, ExecutionReadiness, RejectionCode, Repository,
};
use crate::domain::campaign::verification_run::CommandPolicy;
use crate::services::campaign_execution::{
    adk_profile::{ObservedModel, ProviderCapability, ResolvedAdkProfile, RoleSettingObservation},
    control::{
        self,
        tests::{envelope, now, prepared, running},
    },
    view::{self, PublicCommand},
};

type Sources = Vec<(String, Vec<u8>)>;

#[test]
fn unconfirmed_effort_does_not_resolve_minimal_roles() {
    let inputs = sources();
    assert!(matches!(
        resolve(&settings(), scope(), &inputs),
        ProfileResolution::Resolved { .. }
    ));
    for index in 0..2 {
        for effort in ["unknown", "default", " unknown ", " default "] {
            let mut observations = settings();
            observations.roles[index].effort = Some(effort.into());
            assert!(
                matches!(
                    resolve(&observations, scope(), &inputs),
                    ProfileResolution::Unsupported { role: Some(role), .. }
                        if role == observations.roles[index].role
                ),
                "unconfirmed effort {effort:?} accepted"
            );
        }
    }
}

fn sources() -> Sources {
    REQUIRED_RULE_SOURCES
        .iter()
        .map(|id| ((*id).into(), format!("rule:{id}\n").into_bytes()))
        .collect()
}

fn scope() -> MinimalPipelineScope {
    MinimalPipelineScope::minimal(
        7438,
        "fixed-test-host".into(),
        [
            "i7438-r8-scope-1118".into(),
            "i7438-minimal-approve-1118".into(),
        ],
    )
}

fn settings() -> AdkSettingsObservation {
    AdkSettingsObservation {
        roles: [
            (Role::Implementation, "codex", "work", "xhigh"),
            (Role::Review, "webgpt", "review", "pro"),
        ]
        .into_iter()
        .map(|(role, provider, agent, effort)| RoleSettingObservation {
            role,
            agent_id: agent.into(),
            provider: provider.into(),
            model: ObservedModel::Concrete {
                model: format!("observed-{agent}"),
            },
            effort: Some(effort.into()),
            account_ref: None,
            source: "session-observation".into(),
        })
        .collect(),
        capabilities: ["codex", "webgpt", "claude"]
            .into_iter()
            .map(|provider| ProviderCapability {
                provider: provider.into(),
                independent_review_conversation: true,
                server_owned_external_effects: true,
            })
            .collect(),
        tool_policy_digest: "server-owned-tools".into(),
    }
}

fn spec() -> VerificationSpec {
    VerificationSpec {
        version: "adk-v1".into(),
        repository: Repository::Adk,
        checks: vec![],
        environment: vec![],
        command_policy: CommandPolicy {
            nextest_profile: "ci".into(),
            no_tests_fail: true,
            workers: 1,
        },
    }
}

fn resolve(
    settings: &AdkSettingsObservation,
    scope: MinimalPipelineScope,
    sources: &[(String, Vec<u8>)],
) -> ProfileResolution {
    let inputs: Vec<_> = sources
        .iter()
        .map(|(identity, bytes)| PipelineSource { identity, bytes })
        .collect();
    resolve_minimal_adk_profile(settings, &spec(), scope, &inputs)
}

fn resolved(result: ProfileResolution) -> ResolvedAdkProfile {
    match result {
        ProfileResolution::Resolved { profile } => profile,
        other => panic!("expected resolved: {other:?}"),
    }
}

fn requirements() -> RequirementsSnapshot {
    RequirementsSnapshot::observe(IssueObservation {
        repository: Repository::Adk,
        repository_identity: "itismyfield/AgentDesk".into(),
        issue_number: 7438,
        issue_url: "https://github.com/itismyfield/AgentDesk/issues/7438".into(),
        observed_at: now(),
        issue_updated_at: Some(now()),
        body: "requirements".into(),
        acceptance: vec![],
    })
    .unwrap()
}

fn pin(profile: &ResolvedAdkProfile) -> ExecutionPin {
    ExecutionPin::pin(
        prepared().definition.hashes()["a"].clone(),
        &requirements(),
        profile.hash().unwrap(),
        spec().hash().unwrap(),
    )
    .unwrap()
}

fn assert_drift(before: &ResolvedAdkProfile, after: &ResolvedAdkProfile) {
    let stored = pin(before);
    let bytes = serde_json::to_vec(&stored).unwrap();
    assert_ne!(before.hash().unwrap(), after.hash().unwrap());
    assert_eq!(
        stored.revalidate(
            &stored.hashes,
            &requirements(),
            &after.hash().unwrap(),
            &spec().hash().unwrap()
        ),
        Err(PinError::Drift("profile".into()))
    );
    assert_eq!(serde_json::to_vec(&stored).unwrap(), bytes);
}

#[test]
fn source_scope_and_role_changes_drift_the_existing_pin() {
    let inputs = sources();
    let profile = resolved(resolve(&settings(), scope(), &inputs));
    let mut reordered = inputs.clone();
    reordered.reverse();
    let same = resolved(resolve(&settings(), scope(), &reordered));
    assert_eq!(profile.hash().unwrap(), same.hash().unwrap());
    let stored = pin(&profile);
    stored
        .revalidate(
            &stored.hashes,
            &requirements(),
            &same.hash().unwrap(),
            &spec().hash().unwrap(),
        )
        .unwrap();
    for i in 0..inputs.len() {
        let mut changed = inputs.clone();
        changed[i].1.extend_from_slice(b" changed");
        assert_drift(&profile, &resolved(resolve(&settings(), scope(), &changed)));
    }
    for i in 0..2 {
        let mut changed = scope();
        changed.decision_refs[i].push_str("-renewed");
        assert_drift(&profile, &resolved(resolve(&settings(), changed, &inputs)));
    }
    let mut other_host = scope();
    other_host.host = "next-fixed-host".into();
    assert_drift(
        &profile,
        &resolved(resolve(&settings(), other_host, &inputs)),
    );
    let mut other_issue = scope();
    other_issue.issue_number += 1;
    assert_drift(
        &profile,
        &resolved(resolve(&settings(), other_issue, &inputs)),
    );
    // Keep legacy roles fixed to exercise the snapshot's own role binding.
    let mut other_role = profile.clone();
    other_role.pipeline_rules.as_mut().unwrap().roles[0]
        .model
        .push_str("-changed");
    assert_drift(&profile, &other_role);
    let mut observations = settings();
    observations.roles[1].effort = Some("xhigh".into());
    assert_drift(
        &profile,
        &resolved(resolve(&observations, scope(), &inputs)),
    );
}

#[test]
fn snapshot_records_sources_scope_and_deferred_capabilities_without_raw_content() {
    let inputs = sources();
    let profile = resolved(resolve(&settings(), scope(), &inputs));
    let snapshot = profile.pipeline_rules.as_ref().unwrap();
    assert_eq!(
        snapshot.scope.decision_refs,
        ["i7438-r8-scope-1118", "i7438-minimal-approve-1118"]
    );
    assert!(
        snapshot.scope.single_issue && snapshot.scope.fixed_host && snapshot.scope.individual_pr
    );
    assert!(!snapshot.scope.automatic_approval && !snapshot.scope.approval_default_required);
    assert_eq!(
        snapshot.scope.approval_slots,
        [
            ApprovalSlot::Start,
            ApprovalSlot::Implementation,
            ApprovalSlot::Review,
            ApprovalSlot::Landing,
            ApprovalSlot::Merge,
            ApprovalSlot::Deploy
        ]
    );
    assert_eq!(snapshot.scope.deferred.len(), 8);
    assert!(
        snapshot
            .scope
            .deferred
            .contains(&DeferredCapability::ClaudeFallback)
    );
    assert!(
        snapshot
            .scope
            .deferred
            .contains(&DeferredCapability::Tracker)
    );
    assert!(
        snapshot
            .scope
            .deferred
            .contains(&DeferredCapability::GeneralOwnership)
    );
    assert_eq!(
        snapshot
            .roles
            .iter()
            .map(|r| r.provider.as_str())
            .collect::<Vec<_>>(),
        ["codex", "webgpt"]
    );
    assert_eq!(profile.limits.concurrent_tasks, 1);
    for (identity, bytes) in &inputs {
        assert!(snapshot.sources.contains(&RuleSourceDigest {
            identity: identity.clone(),
            digest: blake3::hash(bytes).to_hex().to_string()
        }));
    }
    let value = serde_json::to_value(&profile).unwrap();
    assert!(value["pipeline_rules"].get("bytes").is_none());
    let restored: ResolvedAdkProfile = serde_json::from_value(value).unwrap();
    assert_eq!(restored.hash().unwrap(), profile.hash().unwrap());
    println!("RULE_SNAPSHOT {}", serde_json::to_string(snapshot).unwrap());
}

#[test]
fn legacy_profile_keeps_its_hash_and_cannot_adopt_a_new_snapshot_quietly() {
    let profile = resolved(resolve(&settings(), scope(), &sources()));
    let mut old = serde_json::to_value(&profile).unwrap();
    old.as_object_mut().unwrap().remove("pipeline_rules");
    let restored: ResolvedAdkProfile = serde_json::from_value(old.clone()).unwrap();
    assert!(restored.pipeline_rules.is_none());
    let legacy_hash = crate::domain::campaign::verification_run::digest_of(&(
        &restored.profile_version,
        &restored.profile_hash,
        &restored.roles,
        &restored.limits,
        &restored.tool_policy_digest,
        &restored.verification_spec_hash,
    ))
    .unwrap();
    assert_eq!(restored.hash().unwrap(), legacy_hash);
    assert_drift(&restored, &profile);
    old["pipeline_rules"] = serde_json::json!({"sources": []});
    assert!(serde_json::from_value::<ResolvedAdkProfile>(old).is_err());
}

#[test]
fn unsupported_scope_sources_and_role_observations_are_rejected() {
    let inputs = sources();
    let reject = |result| assert!(matches!(result, ProfileResolution::Unsupported { .. }));
    let mut scopes = Vec::new();
    let mut changed = scope();
    changed.repository = Repository::CookingHeart;
    scopes.push(changed);
    let mut changed = scope();
    changed.single_issue = false;
    scopes.push(changed);
    let mut changed = scope();
    changed.fixed_host = false;
    scopes.push(changed);
    let mut changed = scope();
    changed.individual_pr = false;
    scopes.push(changed);
    let mut changed = scope();
    changed.automatic_approval = true;
    scopes.push(changed);
    let mut changed = scope();
    changed.approval_slots[5] = ApprovalSlot::Merge;
    scopes.push(changed);
    let mut changed = scope();
    changed.decision_refs[1].clear();
    scopes.push(changed);
    let mut changed = scope();
    changed.deferred.clear();
    scopes.push(changed);
    for changed in scopes {
        reject(resolve(&settings(), changed, &inputs));
    }
    reject(resolve(&settings(), scope(), &inputs[1..]));
    let mut duplicate = inputs.clone();
    duplicate.push(inputs[0].clone());
    reject(resolve(&settings(), scope(), &duplicate));
    for role in 0..2 {
        for model in [
            ObservedModel::Missing,
            ObservedModel::ProviderDefault,
            ObservedModel::Concrete {
                model: "unknown".into(),
            },
        ] {
            let mut changed = settings();
            changed.roles[role].model = model;
            reject(resolve(&changed, scope(), &inputs));
        }
        for provider in ["claude", "unobserved"] {
            let mut changed = settings();
            changed.roles[role].provider = provider.into();
            reject(resolve(&changed, scope(), &inputs));
        }
    }
    let mut duplicate = settings();
    duplicate.roles.push(duplicate.roles[0].clone());
    reject(resolve(&duplicate, scope(), &inputs));
}

#[test]
fn a_snapshot_and_pin_do_not_enable_public_execution() {
    let profile = resolved(resolve(&settings(), scope(), &sources()));
    assert_eq!(
        control::execution_readiness(),
        ExecutionReadiness::Unavailable
    );
    let state = prepared();
    assert!(
        matches!(control::decide(&state, &envelope(Command::Start { expected_revision: 1 })), Err(error) if error.code == RejectionCode::ExecutionUnavailable)
    );
    let mut stopped = running();
    stopped.control = CampaignControl::Stopped;
    let pinned = pin(&profile);
    pinned
        .revalidate(
            &pinned.hashes,
            &requirements(),
            &profile.hash().unwrap(),
            &spec().hash().unwrap(),
        )
        .unwrap();
    for command in [
        Command::Resume {
            expected_revision: 1,
            expected_run: "run".into(),
        },
        Command::PrepareRetry {
            expected_revision: 1,
            expected_run: "run".into(),
            node_id: "a".into(),
        },
    ] {
        assert!(
            matches!(control::decide(&stopped, &envelope(command)), Err(error) if error.code == RejectionCode::ExecutionUnavailable)
        );
    }
    for command in view::view(&stopped, now()).commands {
        if matches!(
            command.command,
            PublicCommand::Start | PublicCommand::Resume | PublicCommand::PrepareRetry
        ) {
            assert_eq!(command.rejection, Some(RejectionCode::ExecutionUnavailable));
        }
    }
}

#[test]
fn stored_pipeline_snapshot_is_not_silently_discarded() {
    let stored = serde_json::json!({
        "profile_version": "adk-v1", "profile_hash": "base", "roles": [],
        "limits": {"concurrent_tasks": 1, "quality_repairs": 3, "design_review_rounds": 2},
        "tool_policy_digest": "tools", "verification_spec_hash": "spec",
        "environment_requirements": [],
        "pipeline_rules": {
            "sources": [{"identity": "SKILL.md", "digest": "latest-source"}],
            "scope": {
                "repository": "adk", "issue_number": 7438, "host": "fixed-test-host",
                "decision_refs": ["i7438-r8-scope-1118", "i7438-minimal-approve-1118"],
                "single_issue": true, "fixed_host": true, "individual_pr": true,
                "automatic_approval": false, "approval_default_required": false,
                "approval_slots": ["start", "implementation", "review", "landing", "merge", "deploy"],
                "deferred": ["dag", "multi_host", "automatic_approval", "claude_fallback", "tracker", "ui", "general_ownership", "design_stages"]
            },
            "roles": [
                {"role": "implementation", "provider": "codex", "model": "observed-work", "effort": "xhigh"},
                {"role": "review", "provider": "webgpt", "model": "observed-review", "effort": "pro"}
            ]
        }
    });
    let profile: ResolvedAdkProfile = serde_json::from_value(stored.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(profile).unwrap()["pipeline_rules"],
        stored["pipeline_rules"]
    );
}
