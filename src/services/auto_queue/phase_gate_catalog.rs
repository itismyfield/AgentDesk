use super::*;

/// Catalog of user-facing phase-gate kinds.
///
/// `batch_phase` is an integer key in auto_queue_entries; this catalog gives
/// callers (dashboard, agents) a shared vocabulary for *which kind of gate*
/// sits between phases — e.g. "PR 머지 확인" vs "스테이지 배포 검증". The
/// underlying `PhaseGateConfig.checks` list (merge_verified / issue_closed /
/// build_passed / ...) is internal verification logic; this catalog maps the
/// user-facing kind id to the set of checks it implies.
#[derive(Debug, Clone, Serialize)]
pub struct PhaseGateKind {
    pub id: &'static str,
    pub label: PhaseGateLabel,
    pub description: &'static str,
    pub checks: &'static [&'static str],
}

#[derive(Debug, Clone, Serialize)]
pub struct PhaseGateLabel {
    pub ko: &'static str,
    pub en: &'static str,
}

pub const DEFAULT_PHASE_GATE_KIND: &str = "pr-confirm";

const PHASE_GATE_KINDS: &[PhaseGateKind] = &[
    PhaseGateKind {
        id: "pr-confirm",
        label: PhaseGateLabel {
            ko: "PR 확인",
            en: "PR Verify",
        },
        description: "PR 머지 및 이슈 종료 확인 후 다음 페이즈 진행",
        checks: &["merge_verified", "issue_closed"],
    },
    PhaseGateKind {
        id: "deploy-gate",
        label: PhaseGateLabel {
            ko: "배포 게이트",
            en: "Deploy Gate",
        },
        description: "스테이지 빌드/배포 통과 후 다음 페이즈 진행",
        checks: &["build_passed", "deploy_verified"],
    },
];

pub fn list_phase_gate_kinds() -> &'static [PhaseGateKind] {
    PHASE_GATE_KINDS
}

pub fn is_valid_phase_gate_kind(id: &str) -> bool {
    PHASE_GATE_KINDS.iter().any(|kind| kind.id == id)
}

pub fn phase_gate_catalog_value() -> serde_json::Value {
    json!({
        "kinds": PHASE_GATE_KINDS.iter().map(|kind| json!({
            "id": kind.id,
            "label": { "ko": kind.label.ko, "en": kind.label.en },
            "description": kind.description,
            "checks": kind.checks,
        })).collect::<Vec<_>>(),
        "default_kind": DEFAULT_PHASE_GATE_KIND,
    })
}

/// GET /api/queue/phase-gates/catalog
pub async fn catalog(
    State(_state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::OK, Json(phase_gate_catalog_value()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_kind_present_in_catalog() {
        assert!(is_valid_phase_gate_kind(DEFAULT_PHASE_GATE_KIND));
    }

    #[test]
    fn catalog_contains_initial_two_kinds() {
        let ids: Vec<&str> = PHASE_GATE_KINDS.iter().map(|k| k.id).collect();
        assert!(ids.contains(&"pr-confirm"));
        assert!(ids.contains(&"deploy-gate"));
    }

    #[test]
    fn catalog_value_shape() {
        let value = phase_gate_catalog_value();
        assert_eq!(value["default_kind"], "pr-confirm");
        let kinds = value["kinds"].as_array().expect("kinds is array");
        assert_eq!(kinds.len(), PHASE_GATE_KINDS.len());
        let first = &kinds[0];
        assert!(first["id"].is_string());
        assert!(first["label"]["ko"].is_string());
        assert!(first["label"]["en"].is_string());
        assert!(first["description"].is_string());
        assert!(first["checks"].is_array());
    }

    #[test]
    fn unknown_kind_rejected() {
        assert!(!is_valid_phase_gate_kind("ship-it"));
        assert!(!is_valid_phase_gate_kind(""));
    }
}
