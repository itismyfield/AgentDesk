use super::*;
use crate::services::campaign_execution::profile::CheckKind;
use crate::services::campaign_execution::verification_run::{
    CommandPolicy, PhaseKind, PhaseTemplate, SelectionRule, SpecCheck,
};

fn spec(repository: Repository) -> VerificationSpec {
    VerificationSpec {
        version: "adk-v1".into(),
        repository,
        checks: vec![SpecCheck {
            id: "lib-tests".into(),
            kind: CheckKind::Test,
            required: true,
            acceptance_ids: vec![],
            selection: SelectionRule::NonPgClassifier,
            phases: vec![PhaseTemplate {
                id: "test".into(),
                kind: PhaseKind::Test,
                requires: vec![],
            }],
        }],
        environment: vec![],
        command_policy: CommandPolicy {
            nextest_profile: "ci".into(),
            no_tests_fail: true,
            workers: 1,
        },
    }
}

fn role(role: Role, agent: &str, model: ObservedModel) -> RoleSettingObservation {
    RoleSettingObservation {
        role,
        agent_id: agent.into(),
        provider: "claude".into(),
        model,
        effort: Some("high".into()),
        account_ref: Some("acct-1".into()),
        source: "agents.yaml".into(),
    }
}

fn concrete(model: &str) -> ObservedModel {
    ObservedModel::Concrete {
        model: model.into(),
    }
}

fn settings(review_model: ObservedModel) -> AdkSettingsObservation {
    AdkSettingsObservation {
        roles: vec![
            role(Role::Implementation, "dev", concrete("claude-opus-5-5")),
            role(Role::Review, "reviewer", review_model),
        ],
        capabilities: vec![ProviderCapability {
            provider: "claude".into(),
            independent_review_conversation: true,
            server_owned_external_effects: true,
        }],
        tool_policy_digest: "tools-1".into(),
    }
}

fn resolved(resolution: ProfileResolution) -> ResolvedAdkProfile {
    match resolution {
        ProfileResolution::Resolved { profile } => profile,
        ProfileResolution::Unsupported { role, reason } => {
            panic!("expected a resolved profile, got {role:?}: {reason}")
        }
    }
}

#[test]
fn concrete_settings_resolve_with_the_contract_limits_and_spec_hash() {
    let adk = spec(Repository::Adk);
    let profile = resolved(resolve_adk_profile(
        &settings(concrete("claude-sonnet-5-5")),
        &adk,
    ));
    assert_eq!(profile.profile_version, PROFILE_VERSION);
    assert_eq!(
        profile.profile_hash,
        RepositoryProfile::for_repository(Repository::Adk)
            .unwrap()
            .hash
    );
    assert_eq!(
        (
            profile.limits.concurrent_tasks,
            profile.limits.quality_repairs,
            profile.limits.design_review_rounds
        ),
        (2, 3, 2)
    );
    assert_eq!(profile.verification_spec_hash, adk.hash().unwrap());
    assert_eq!(
        profile.environment_requirements,
        [
            EnvironmentComponent::Toolchain,
            EnvironmentComponent::RustTarget
        ]
    );
    assert_eq!(profile.roles.len(), 2);
    assert_eq!(profile.roles[1].model, "claude-sonnet-5-5");
    let again = resolved(resolve_adk_profile(
        &settings(concrete("claude-sonnet-5-5")),
        &adk,
    ));
    assert_eq!(profile.hash().unwrap(), again.hash().unwrap());
    let other = resolved(resolve_adk_profile(
        &settings(concrete("claude-opus-5-5")),
        &adk,
    ));
    assert_ne!(profile.hash().unwrap(), other.hash().unwrap());
}

#[test]
fn provider_default_missing_model_shared_agent_or_missing_capability_is_unsupported() {
    let adk = spec(Repository::Adk);
    let unsupported = |resolution: ProfileResolution| match resolution {
        ProfileResolution::Unsupported { role, reason } => (role, reason),
        ProfileResolution::Resolved { .. } => panic!("must not resolve"),
    };
    let (role, reason) = unsupported(resolve_adk_profile(
        &settings(ObservedModel::ProviderDefault),
        &adk,
    ));
    assert_eq!(role, Some(Role::Review));
    assert!(reason.contains("provider default"));
    assert_eq!(
        unsupported(resolve_adk_profile(&settings(ObservedModel::Missing), &adk)).0,
        Some(Role::Review)
    );
    assert_eq!(
        unsupported(resolve_adk_profile(&settings(concrete("  ")), &adk)).0,
        Some(Role::Review)
    );
    let mut shared = settings(concrete("claude-sonnet-5-5"));
    shared.roles[1].agent_id = "dev".into();
    assert_eq!(
        unsupported(resolve_adk_profile(&shared, &adk)).0,
        Some(Role::Review)
    );
    let mut no_review_conversation = settings(concrete("claude-sonnet-5-5"));
    no_review_conversation.capabilities[0].independent_review_conversation = false;
    assert_eq!(
        unsupported(resolve_adk_profile(&no_review_conversation, &adk)).0,
        Some(Role::Review)
    );
    let mut unknown_provider = settings(concrete("claude-sonnet-5-5"));
    unknown_provider.capabilities.clear();
    assert_eq!(
        unsupported(resolve_adk_profile(&unknown_provider, &adk)).0,
        Some(Role::Implementation)
    );
    let mut no_effort = settings(concrete("claude-sonnet-5-5"));
    no_effort.roles[0].effort = None;
    assert_eq!(
        unsupported(resolve_adk_profile(&no_effort, &adk)).0,
        Some(Role::Implementation)
    );
    assert_eq!(
        unsupported(resolve_adk_profile(
            &settings(concrete("m")),
            &spec(Repository::CookingHeart)
        ))
        .0,
        None
    );
}
