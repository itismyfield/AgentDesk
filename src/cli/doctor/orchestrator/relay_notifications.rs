use super::{Check, CheckGroup, CheckStatus, FixSafety, SecurityExposure, Severity, config, json};

pub(super) fn check(cfg: &config::Config) -> Check {
    let (state, status, severity, detail) = match cfg.kanban.human_alert_channel_id.as_deref() {
        None => (
            "local_target_unset",
            CheckStatus::Warn,
            Severity::Warning,
            "로컬 구성에 운영 알림 대상이 선언되지 않았습니다. 실행 서버의 실효 KV 설정과 알림 배달 상태는 확인하지 않았습니다.",
        ),
        Some(value) if value.trim().is_empty() => (
            "local_target_blank",
            CheckStatus::Warn,
            Severity::Warning,
            "로컬 구성의 운영 알림 대상이 비어 있습니다. 실행 서버의 실효 KV 설정과 알림 배달 상태는 확인하지 않았습니다.",
        ),
        Some(_) => (
            "local_target_declared_unverified",
            CheckStatus::Pass,
            Severity::Info,
            "로컬 구성에 운영 알림 대상 문자열이 선언되어 있습니다. 대상 유효성·실효 KV 반영·권한·배달은 검증하지 않았습니다.",
        ),
    };
    let mut check = Check::ok(
        "relay_operator_alert_target_config",
        CheckGroup::Core,
        "Relay Operator Alert Target (local config)",
        detail,
    )
    .with_subsystem("relay_notifications")
    .with_severity(severity)
    .with_fix_safety(FixSafety::ReadOnly)
    .with_security_exposure(SecurityExposure::OperationalMetadata)
    .with_evidence(json!({
        "scope": "local_config_snapshot",
        "target_state": state,
        "runtime_effective_state": "unknown",
        "delivery_state": "not_checked",
        "circuit_state": "not_checked"
    }));
    check.status = status;
    check.guidance = Some("알림 설정 검토 시 실행 서버의 kanban_human_alert_channel_id 실효 설정을 별도 읽기 전용 절차로 확인하십시오. 이 진단은 알림 활성화나 복구를 수행하지 않습니다. CIRCUIT_STAMP=1 설정을 해결책으로 제시하지 않습니다.".into());
    check
}

#[cfg(test)]
mod tests {
    use super::super::{DoctorOptions, RunContext, build_json_report, print_group};
    use super::*;
    fn assert_target(input: Option<&str>, state: &str, status: &str, severity: &str) {
        let mut cfg = config::Config::default();
        cfg.kanban.human_alert_channel_id = input.map(str::to_owned);
        let check = check(&cfg);
        assert_eq!(check.status.as_str(), status);
        assert_eq!(check.severity.as_str(), severity);
        assert_eq!(check.id, "relay_operator_alert_target_config");
        assert_eq!(check.group.as_str(), "core");
        assert_eq!(check.subsystem, "relay_notifications");
        assert_eq!(check.fix_safety, FixSafety::ReadOnly);
        assert_eq!(
            check.security_exposure,
            SecurityExposure::OperationalMetadata
        );
        assert_eq!(
            check.evidence,
            Some(json!({
                "scope": "local_config_snapshot", "target_state": state,
                "runtime_effective_state": "unknown", "delivery_state": "not_checked",
                "circuit_state": "not_checked"
            }))
        );
        assert!(check.guidance.as_ref().unwrap().contains("별도 읽기 전용"));
        assert_eq!(
            check.label(),
            if status == "warn" { "WARN" } else { "PASS" }
        );
        for fix in [false, true] {
            let options = DoctorOptions {
                fix,
                json: true,
                allow_restart: fix,
                repair_sqlite_cache: false,
                allow_remote: fix,
                profile: None,
                run_context: RunContext::ManualCli,
                artifact_path: None,
            };
            let report = build_json_report(&options, std::slice::from_ref(&check), &[]);
            let value = serde_json::to_value(report).unwrap();
            let output = &value["checks"][0];
            assert_eq!(output["status"], status);
            assert_eq!(output["severity"], severity);
            assert_eq!(output["fix_safety"], "read_only");
            assert_eq!(output["evidence"], check.evidence.clone().unwrap());
            if let Some(secret) = input.filter(|s| !s.trim().is_empty()) {
                assert!(!value.to_string().contains(secret));
                assert!(!check.detail.contains(secret));
                assert!(!check.guidance.as_ref().unwrap().contains(secret));
            }
        }
        print_group("core", &[check]); // Exercise the existing text renderer; no runtime probes.
    }
    #[test]
    fn unset_is_warning() {
        assert_target(None, "local_target_unset", "warn", "warning");
    }
    #[test]
    fn rust_whitespace_is_blank() {
        for value in ["", " ", "\t", "\n", "\u{2003}\u{00a0}"] {
            assert_target(Some(value), "local_target_blank", "warn", "warning");
        }
    }
    #[test]
    fn declarations_are_unverified_even_when_invalid() {
        for value in [
            "123",
            "channel:123",
            " 123 ",
            "channel:",
            "private-target-token",
        ] {
            assert_target(
                Some(value),
                "local_target_declared_unverified",
                "pass",
                "info",
            );
        }
    }
    #[test]
    fn wiring_and_read_only_boundary_lexical_tripwire() {
        let source = include_str!("../orchestrator.rs");
        let core = source
            .split("fn build_core_checks(")
            .nth(1)
            .unwrap()
            .split("fn build_provider_checks(")
            .next()
            .unwrap();
        assert_eq!(core.matches("relay_notifications::check(cfg)").count(), 1);
        assert!(core.contains("check_config_audit(snapshot)"));
        assert!(core.contains("check_degraded_reasons(snapshot)"));
        let fixes = source
            .split("fn apply_safe_fixes(")
            .nth(1)
            .unwrap()
            .split("fn print_fix_actions(")
            .next()
            .unwrap();
        assert!(!fixes.contains("relay_notifications"));
        assert!(!fixes.contains("relay_operator_alert_target_config"));
        let helper = include_str!("relay_notifications.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            "std::env",
            "env!",
            "HealthSnapshot",
            "load_graceful",
            "save(",
            "enqueue",
            "sqlx",
            "reqwest",
        ] {
            assert!(
                !helper.contains(forbidden),
                "unexpected input/action: {forbidden}"
            );
        }
    }
}
