use super::*;

pub const DEFAULT_PHASE_GATE_KIND: &str = crate::phase_gate::DEFAULT_PHASE_GATE_KIND;

pub fn is_valid_phase_gate_kind(id: &str) -> bool {
    crate::phase_gate::is_valid_kind(id)
}

pub fn phase_gate_catalog_value() -> serde_json::Value {
    crate::phase_gate::catalog_value()
}

/// GET /api/queue/phase-gates/catalog
pub async fn catalog(State(_state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
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
    fn catalog_exposes_typed_declaration_and_unavailable_deploy_gate() {
        let value = phase_gate_catalog_value();
        assert_eq!(value["default_kind"], "pr-confirm");
        let kinds = value["kinds"].as_array().expect("kinds is array");
        let deploy = kinds
            .iter()
            .find(|kind| kind["id"] == "deploy-gate")
            .expect("deploy gate");
        assert_eq!(deploy["available"], false);
        assert_eq!(
            deploy["unavailable_reason"],
            crate::phase_gate::DEPLOY_GATE_UNAVAILABLE_REASON
        );
        assert!(deploy["declaration_version"].is_number());
        assert!(deploy["pass_verdict"].is_string());
        assert!(deploy["required_checks"].is_array());
        assert!(deploy["evidence_requirement"].is_string());
    }

    #[test]
    fn unknown_kind_rejected() {
        assert!(!is_valid_phase_gate_kind("ship-it"));
        assert!(!is_valid_phase_gate_kind(""));
    }
}
