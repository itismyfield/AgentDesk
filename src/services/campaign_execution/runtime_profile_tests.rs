use super::*;
use crate::services::campaign_execution::verification_spec::adk_verification_spec;

pub(super) fn host(boot: &str) -> HostGate {
    HostGate {
        host: "local".into(),
        gate: "campaign".into(),
        incarnation: boot.into(),
    }
}

pub(in crate::services::campaign_execution) fn witness(role: Role, host: &HostGate) -> RoleWitness {
    let (marker, provider, model, effort) = match role {
        Role::Implementation => (RuntimeMarker::Codex, "codex", "gpt-99.4-sol", "xhigh"),
        Role::Review => (
            RuntimeMarker::WebGpt,
            "webgpt",
            "observed-review-model",
            "pro",
        ),
    };
    RoleWitness {
        marker,
        setting: RoleSettingObservation {
            role,
            agent_id: format!("{provider}-observer"),
            provider: provider.into(),
            model: ObservedModel::Concrete {
                model: model.into(),
            },
            effort: Some(effort.into()),
            account_ref: None,
            source: "direct observer".into(),
        },
        capability: ProviderCapability {
            provider: provider.into(),
            independent_review_conversation: true,
            server_owned_external_effects: true,
        },
        session: SessionIdentity {
            gate: host.clone(),
            session: format!("{provider}-session"),
            incarnation: "live-process".into(),
        },
        authenticated: true,
        fallback_enabled: false,
        tool_policy_digest: "tool-policy".into(),
    }
}

impl<F: Fn(Role) -> Observation<RoleWitness> + Send + Sync> RoleObserver for F {
    fn observe(&self, role: Role) -> Observation<RoleWitness> {
        self(role)
    }
}

pub(in crate::services::campaign_execution) fn rule_files(root: &Path) {
    for source in REQUIRED_RULE_SOURCES {
        let path = root.join(source);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
}

#[test]
fn m4_runtime_rejects_astra_fallback_and_account_observations() {
    let tmp = tempfile::tempdir().unwrap();
    rule_files(tmp.path());
    let host = host("boot-a");
    let scope =
        MinimalPipelineScope::minimal(7438, host.host.clone(), ["scope".into(), "approved".into()]);
    let spec = adk_verification_spec();
    spec.validate().unwrap();
    for mode in [
        "ready",
        "astra",
        "fallback",
        "account",
        "marker",
        "review effort",
    ] {
        let observer = |role| {
            let mut evidence = witness(role, &host);
            match (mode, role) {
                ("astra", Role::Implementation) => {
                    evidence.setting.model = ObservedModel::Concrete {
                        model: "gpt-99.4-astra".into(),
                    }
                }
                ("fallback", Role::Review) => evidence.fallback_enabled = true,
                ("account", Role::Implementation) => {
                    evidence.setting.account_ref = Some("1".into())
                }
                ("marker", Role::Implementation) => evidence.marker = RuntimeMarker::WebGpt,
                ("review effort", Role::Review) => evidence.setting.effort = Some("xhigh".into()),
                _ => {}
            }
            Observation::Observed {
                evidence,
                observed_at: Utc::now(),
            }
        };
        let request = RuntimeProfile {
            rule_root: tmp.path(),
            scope: &scope,
            tool_policy_digest: "tool-policy",
            observer: &observer,
        };
        let result = request.observe(&host, &spec);
        if mode == "ready" {
            let resolved = result.unwrap();
            assert!(resolved.profile().pipeline_rules.is_some());
            assert!(
                resolved
                    .require_session(
                        Role::Implementation,
                        &witness(Role::Implementation, &host).session
                    )
                    .is_ok()
            );
            assert!(
                resolved
                    .require_session(Role::Implementation, &witness(Role::Review, &host).session)
                    .is_err()
            );
        } else {
            assert!(
                matches!(result, Err(RuntimeError::Provider(_))),
                "accepted {mode}"
            );
        }
    }
}

#[test]
fn m4_runtime_requires_fresh_observations_from_the_current_boot() {
    let tmp = tempfile::tempdir().unwrap();
    rule_files(tmp.path());
    let old = host("boot-a");
    let new = host("boot-b");
    let scope =
        MinimalPipelineScope::minimal(7438, old.host.clone(), ["scope".into(), "approved".into()]);
    let spec = adk_verification_spec();
    let sampler = |role| Observation::Observed {
        evidence: witness(role, &old),
        observed_at: Utc::now(),
    };
    let request = RuntimeProfile {
        rule_root: tmp.path(),
        scope: &scope,
        tool_policy_digest: "tool-policy",
        observer: &sampler,
    };
    assert!(request.observe(&old, &spec).is_ok());
    assert!(matches!(
        request.observe(&new, &spec),
        Err(RuntimeError::Identity)
    ));
    let missing = |_| Observation::Unknown {
        reason: "offline".into(),
        last_observed_at: None,
    };
    let request = RuntimeProfile {
        observer: &missing,
        ..request
    };
    assert!(matches!(
        request.observe(&new, &spec),
        Err(RuntimeError::Unobserved(Role::Implementation))
    ));
    let cached = |role| Observation::Observed {
        evidence: witness(role, &new),
        observed_at: Utc::now() - chrono::Duration::seconds(10),
    };
    let request = RuntimeProfile {
        observer: &cached,
        ..request
    };
    assert!(matches!(
        request.observe(&new, &spec),
        Err(RuntimeError::Unobserved(Role::Implementation))
    ));
}
