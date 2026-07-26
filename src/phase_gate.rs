//! Canonical typed phase-gate declarations shared by HTTP, policy dispatch, and reducers.
//!
//! Declarations are immutable snapshots. A dispatch can advance only when its snapshot is an exact
//! match for the current declaration and every required check uses an authority class that the
//! current reducer can verify. Deployment evidence intentionally remains unavailable here; this
//! containment prevents agent-reported values from standing in for a future trusted capability.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const DEFAULT_PHASE_GATE_KIND: &str = "pr-confirm";
pub const DEPLOY_GATE_UNAVAILABLE_REASON: &str =
    "deploy-gate unavailable: trusted deployment evidence capability is not configured";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PhaseGateKind {
    PrConfirm,
    DeployGate,
}

impl PhaseGateKind {
    pub const fn id(self) -> &'static str {
        match self {
            Self::PrConfirm => "pr-confirm",
            Self::DeployGate => "deploy-gate",
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        match id {
            "pr-confirm" => Some(Self::PrConfirm),
            "deploy-gate" => Some(Self::DeployGate),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseGateCheck {
    MergeVerified,
    IssueClosed,
    BuildPassed,
    DeployVerified,
}

impl PhaseGateCheck {
    pub const fn id(self) -> &'static str {
        match self {
            Self::MergeVerified => "merge_verified",
            Self::IssueClosed => "issue_closed",
            Self::BuildPassed => "build_passed",
            Self::DeployVerified => "deploy_verified",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseGateCheckAuthority {
    DispatchResult,
    TrustedDeploymentEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseGateEvidenceRequirement {
    DispatchResultChecks,
    TrustedDeploymentEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhaseGateCheckDeclaration {
    pub check: PhaseGateCheck,
    pub authority: PhaseGateCheckAuthority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhaseGateDeclaration {
    pub kind: PhaseGateKind,
    pub version: u32,
    pub label_ko: &'static str,
    pub label_en: &'static str,
    pub description: &'static str,
    pub pass_verdict: &'static str,
    pub evidence_requirement: PhaseGateEvidenceRequirement,
    pub required_checks: &'static [PhaseGateCheckDeclaration],
    pub unavailable_reason: Option<&'static str>,
}

const PR_CONFIRM_CHECKS: &[PhaseGateCheckDeclaration] = &[
    PhaseGateCheckDeclaration {
        check: PhaseGateCheck::MergeVerified,
        authority: PhaseGateCheckAuthority::DispatchResult,
    },
    PhaseGateCheckDeclaration {
        check: PhaseGateCheck::IssueClosed,
        authority: PhaseGateCheckAuthority::DispatchResult,
    },
    PhaseGateCheckDeclaration {
        check: PhaseGateCheck::BuildPassed,
        authority: PhaseGateCheckAuthority::DispatchResult,
    },
];

const DEPLOY_GATE_CHECKS: &[PhaseGateCheckDeclaration] = &[
    PhaseGateCheckDeclaration {
        check: PhaseGateCheck::BuildPassed,
        authority: PhaseGateCheckAuthority::DispatchResult,
    },
    PhaseGateCheckDeclaration {
        check: PhaseGateCheck::DeployVerified,
        authority: PhaseGateCheckAuthority::TrustedDeploymentEvidence,
    },
];

pub const PHASE_GATE_DECLARATIONS: &[PhaseGateDeclaration] = &[
    PhaseGateDeclaration {
        kind: PhaseGateKind::PrConfirm,
        version: 1,
        label_ko: "PR 확인",
        label_en: "PR Verify",
        description: "PR 머지, 이슈 종료, 빌드 통과 확인 후 다음 페이즈 진행",
        pass_verdict: "phase_gate_passed",
        evidence_requirement: PhaseGateEvidenceRequirement::DispatchResultChecks,
        required_checks: PR_CONFIRM_CHECKS,
        unavailable_reason: None,
    },
    PhaseGateDeclaration {
        kind: PhaseGateKind::DeployGate,
        version: 1,
        label_ko: "배포 게이트",
        label_en: "Deploy Gate",
        description: "신뢰 가능한 배포 증거가 확인된 후 다음 페이즈 진행",
        pass_verdict: "phase_gate_passed",
        evidence_requirement: PhaseGateEvidenceRequirement::TrustedDeploymentEvidence,
        required_checks: DEPLOY_GATE_CHECKS,
        unavailable_reason: Some(DEPLOY_GATE_UNAVAILABLE_REASON),
    },
];

pub fn declaration_for_id(id: &str) -> Option<&'static PhaseGateDeclaration> {
    let kind = PhaseGateKind::parse(id)?;
    PHASE_GATE_DECLARATIONS
        .iter()
        .find(|declaration| declaration.kind == kind)
}

pub fn is_valid_kind(id: &str) -> bool {
    declaration_for_id(id).is_some()
}

pub fn kind_unavailable_reason(id: &str) -> Option<&'static str> {
    declaration_for_id(id).and_then(|declaration| declaration.unavailable_reason)
}

fn declaration_value(declaration: &PhaseGateDeclaration) -> Value {
    json!({
        "kind": declaration.kind.id(),
        "declaration_version": declaration.version,
        "pass_verdict": declaration.pass_verdict,
        "evidence_requirement": declaration.evidence_requirement,
        "required_checks": declaration.required_checks.iter().map(|required| json!({
            "check": required.check,
            "authority": required.authority,
        })).collect::<Vec<_>>(),
        "available": declaration.unavailable_reason.is_none(),
        "unavailable_reason": declaration.unavailable_reason,
    })
}

pub fn resolve_declaration_value(id: &str) -> Option<Value> {
    declaration_for_id(id).map(declaration_value)
}

pub fn catalog_value() -> Value {
    json!({
        "kinds": PHASE_GATE_DECLARATIONS.iter().map(|declaration| {
            let mut value = declaration_value(declaration);
            if let Some(object) = value.as_object_mut() {
                object.insert("id".to_string(), json!(declaration.kind.id()));
                object.insert("label".to_string(), json!({
                    "ko": declaration.label_ko,
                    "en": declaration.label_en,
                }));
                object.insert("description".to_string(), json!(declaration.description));
                object.insert("checks".to_string(), json!(declaration.required_checks.iter()
                    .map(|required| required.check.id())
                    .collect::<Vec<_>>()));
            }
            value
        }).collect::<Vec<_>>(),
        "default_kind": DEFAULT_PHASE_GATE_KIND,
    })
}

pub fn snapshot_matches_current(phase_gate: &Value) -> bool {
    let Some(object) = phase_gate.as_object() else {
        return false;
    };
    let Some(kind) = object.get("kind").and_then(Value::as_str) else {
        return false;
    };
    let Some(expected) = resolve_declaration_value(kind) else {
        return false;
    };
    if expected.get("available").and_then(Value::as_bool) != Some(true) {
        return false;
    }

    for field in [
        "kind",
        "declaration_version",
        "pass_verdict",
        "evidence_requirement",
        "required_checks",
    ] {
        if object.get(field) != expected.get(field) {
            return false;
        }
    }
    true
}

pub fn dispatch_result_checks(phase_gate: &Value) -> Option<Vec<&str>> {
    if !snapshot_matches_current(phase_gate) {
        return None;
    }
    phase_gate
        .get("required_checks")?
        .as_array()?
        .iter()
        .map(|required| {
            let authority = required.get("authority")?.as_str()?;
            if authority != "dispatch_result" {
                return None;
            }
            required.get("check")?.as_str()
        })
        .collect()
}

/// Return an evaluation context whose declaration fields come only from this registry.
///
/// Current dispatches must carry an exact declaration snapshot. The sole compatibility case is a
/// pre-snapshot context whose persisted run/phase entries were independently proven to all have a
/// NULL/blank kind. Only that closed provenance may reconstruct the current `pr-confirm`
/// declaration; partial/stale snapshots never fall back through this path.
pub fn authoritative_evaluation_context(
    context: &Value,
    persisted_legacy_default: bool,
) -> Option<Value> {
    let phase_gate = context.get("phase_gate")?;
    if snapshot_matches_current(phase_gate) {
        return Some(context.clone());
    }
    if !persisted_legacy_default || !legacy_context_lacks_declaration_snapshot(phase_gate) {
        return None;
    }

    let declaration = resolve_declaration_value(DEFAULT_PHASE_GATE_KIND)?;
    let mut normalized = context.clone();
    let normalized_gate = normalized.get_mut("phase_gate")?.as_object_mut()?;
    for field in [
        "kind",
        "declaration_version",
        "pass_verdict",
        "evidence_requirement",
        "required_checks",
    ] {
        normalized_gate.insert(field.to_string(), declaration.get(field)?.clone());
    }
    Some(normalized)
}

fn legacy_context_lacks_declaration_snapshot(phase_gate: &Value) -> bool {
    let Some(object) = phase_gate.as_object() else {
        return false;
    };
    [
        "kind",
        "declaration_version",
        "required_checks",
        "evidence_requirement",
    ]
    .iter()
    .all(|field| !object.contains_key(*field))
}

#[cfg(test)]
mod auto_queue_phase_gate_tests {
    use super::*;

    #[test]
    fn phase_gate_registry_catalog_and_dispatch_mapping_share_declarations() {
        let catalog = catalog_value();
        for declaration in PHASE_GATE_DECLARATIONS {
            let resolved = resolve_declaration_value(declaration.kind.id()).expect("declaration"); // agentdesk-audit: allow-unwrap — immutable built-in declaration fixture
            let catalog_entry = catalog["kinds"]
                .as_array()
                .and_then(|kinds| {
                    kinds
                        .iter()
                        .find(|kind| kind["id"] == declaration.kind.id())
                })
                .expect("catalog entry"); // agentdesk-audit: allow-unwrap — catalog is generated from the same immutable registry
            assert_eq!(
                catalog_entry["required_checks"],
                resolved["required_checks"]
            );
            assert_eq!(
                catalog_entry["declaration_version"],
                resolved["declaration_version"]
            );
            assert_eq!(catalog_entry["pass_verdict"], resolved["pass_verdict"]);
        }
    }

    #[test]
    fn phase_gate_registry_deploy_check_requires_authoritative_evidence() {
        let declaration = declaration_for_id("deploy-gate").expect("deploy declaration"); // agentdesk-audit: allow-unwrap — immutable built-in declaration fixture
        assert_eq!(
            declaration.unavailable_reason,
            Some(DEPLOY_GATE_UNAVAILABLE_REASON)
        );
        assert!(declaration.required_checks.iter().any(|required| {
            required.check == PhaseGateCheck::DeployVerified
                && required.authority == PhaseGateCheckAuthority::TrustedDeploymentEvidence
        }));
    }

    #[test]
    fn authoritative_context_reconstructs_only_proven_pre_snapshot_legacy_default() {
        let legacy = json!({
            "phase_gate": {
                "run_id": "run-legacy",
                "batch_phase": 0,
                "checks": ["attacker_override"],
                "pass_verdict": "attacker_pass",
            }
        });
        assert!(authoritative_evaluation_context(&legacy, false).is_none());
        let normalized =
            authoritative_evaluation_context(&legacy, true).expect("proven legacy default"); // agentdesk-audit: allow-unwrap — test-only canonical compatibility fixture
        assert_eq!(normalized["phase_gate"]["kind"], "pr-confirm");
        assert_eq!(
            normalized["phase_gate"]["required_checks"],
            resolve_declaration_value("pr-confirm").expect("pr-confirm declaration")["required_checks"] // agentdesk-audit: allow-unwrap — immutable built-in declaration fixture
        );
        assert_eq!(
            normalized["phase_gate"]["pass_verdict"],
            "phase_gate_passed"
        );

        for incompatible in [
            json!({"phase_gate": {"kind": "ship-it"}}),
            json!({"phase_gate": {"kind": "deploy-gate"}}),
            json!({"phase_gate": {"declaration_version": 1}}),
        ] {
            assert!(authoritative_evaluation_context(&incompatible, true).is_none());
        }
    }

    #[test]
    fn phase_gate_registry_snapshot_validation_fails_closed() {
        let valid =
            resolve_declaration_value(DEFAULT_PHASE_GATE_KIND).expect("default declaration"); // agentdesk-audit: allow-unwrap — immutable built-in declaration fixture
        assert!(snapshot_matches_current(&valid));

        for field in [
            "kind",
            "declaration_version",
            "required_checks",
            "pass_verdict",
        ] {
            let mut malformed = valid.clone();
            malformed.as_object_mut().expect("object").remove(field); // agentdesk-audit: allow-unwrap — registry declarations serialize as JSON objects
            assert!(!snapshot_matches_current(&malformed), "missing {field}");
        }

        let deploy = resolve_declaration_value("deploy-gate").expect("deploy declaration"); // agentdesk-audit: allow-unwrap — immutable built-in declaration fixture
        assert!(!snapshot_matches_current(&deploy));
        assert!(!snapshot_matches_current(&json!({"kind": "ship-it"})));
    }
}
