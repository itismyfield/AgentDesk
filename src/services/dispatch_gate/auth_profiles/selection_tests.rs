use super::*;
use crate::services::provider::ProviderKind;
use crate::services::provider_auth_profile::fallback;

const AGENT: &str = "profile-pressure-agent";

fn pressure(a: u64, b: u64, c: u64, fetched_at: i64) {
    set_provider_pressure_snapshot(pressure_snapshot_from_payloads(
        &[('a', a), ('b', b), ('c', c)].map(|(id, used)| {
            serde_json::json!({
                "provider": "codex", "profile_id": id.to_string(),
                "fetched_at": fetched_at,
                "buckets": [{"limit": 100, "used": used, "reset": fetched_at + 10_000}]
            })
        }),
    ));
}

fn install() {
    super::super::tests::install_runtime(Some(true), Some(100), Some(600));
    clear_profiles();
    set_agent_provider_snapshot(HashMap::from([(AGENT.into(), "codex".into())]));
    AGENT_PROFILE
        .get_or_init(|| RwLock::new(HashMap::new()))
        .write()
        .unwrap()
        .insert(AGENT.into(), "codex:a".into());
    AGENT_FALLBACKS
        .get_or_init(|| RwLock::new(HashMap::new()))
        .write()
        .unwrap()
        .insert(AGENT.into(), vec!["codex:b".into(), "codex:c".into()]);
}

fn select(channel: u64) -> String {
    fallback::select(
        &ProviderKind::Codex,
        channel,
        &["a".into(), "b".into(), "c".into()],
        |id| !profile_deferred(&ProviderKind::Codex, id, Some(AGENT)),
    )
}

#[test]
fn persisted_admission_threshold_skips_an_earlier_pressured_backup_at_spawn() {
    let _guard = super::super::tests::global_gate_test_guard();
    install();
    let now = chrono::Utc::now().timestamp();
    pressure(100, 95, 10, now);
    let decision =
        evaluate_agent_provider_pressure_with_overrides(AGENT, now, None, Some(90), None);
    assert_eq!(decision.verdict, PressureVerdict::Allow);
    assert_eq!(decision.utilization_pct, Some(10));
    assert_eq!(select(9_203_101), "c");
}

#[test]
fn sticky_backup_above_admission_threshold_yields_to_a_healthy_primary() {
    let _guard = super::super::tests::global_gate_test_guard();
    install();
    let now = chrono::Utc::now().timestamp();
    pressure(100, 20, 10, now);
    evaluate_agent_provider_pressure_with_overrides(AGENT, now, None, Some(90), None);
    assert_eq!(select(9_203_102), "b");
    pressure(20, 95, 10, now);
    let decision =
        evaluate_agent_provider_pressure_with_overrides(AGENT, now, None, Some(90), None);
    assert_eq!(decision.utilization_pct, Some(20));
    assert_eq!(select(9_203_102), "a");
}

#[test]
fn primary_pressure_and_staleness_use_the_same_policy_as_fallback_selection() {
    let _guard = super::super::tests::global_gate_test_guard();
    install();
    let now = chrono::Utc::now().timestamp();
    pressure(95, 94, 10, now - 30);
    let decision =
        evaluate_agent_provider_pressure_with_overrides(AGENT, now, None, Some(90), Some(60));
    assert_eq!(decision.utilization_pct, Some(10));
    assert_eq!(select(9_203_103), "c");
    let decision =
        evaluate_agent_provider_pressure_with_overrides(AGENT, now, None, Some(90), Some(10));
    assert_eq!(decision.reason_code, PressureReasonCode::StaleTelemetry);
    assert_eq!(select(9_203_104), "a");
}

#[test]
fn disabling_gate_or_removing_override_releases_the_old_admission_threshold() {
    let _guard = super::super::tests::global_gate_test_guard();
    install();
    let now = chrono::Utc::now().timestamp();
    pressure(95, 94, 10, now);
    evaluate_agent_provider_pressure_with_overrides(AGENT, now, None, Some(90), None);
    assert_eq!(select(9_203_105), "c");
    let decision =
        evaluate_agent_provider_pressure_with_overrides(AGENT, now, Some(false), Some(90), None);
    assert_eq!(decision.reason_code, PressureReasonCode::GateDisabled);
    assert_eq!(select(9_203_106), "a");
    evaluate_agent_provider_pressure_with_overrides(AGENT, now, None, None, None);
    assert_eq!(select(9_203_107), "a");
}
