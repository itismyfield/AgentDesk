use super::*;

#[test]
fn diagnosis_reports_refusal_and_bounded_checker_output_without_reading_files() {
    let socket = Path::new("/no-server-diagnostics-test/herdr.sock");
    let cache = ProofCache::default();
    record(socket, &cache, &Err(Unverified::ResumeEnabled));
    checker(socket, &vec![b'x'; 8192], b"unknown key: theme.typo");
    let view = snapshot(socket);
    assert_eq!(
        view["next_restart_ready"], false,
        "health_does_not_promote_policy_refusal_contract"
    );
    assert_eq!(view["startup_proof"], "unknown");
    assert_eq!(view["reason"], "next_restart_resumes");
    assert!(
        view["checker"]
            .as_str()
            .is_some_and(|detail| detail.chars().count() <= 512 && detail.ends_with("theme.typo")),
        "health_checker_excerpt_is_bounded_and_keeps_the_last_error_contract"
    );
    assert!(
        !socket.exists(),
        "health_diagnostics_creates_no_server_or_config_contract"
    );
}

#[test]
fn health_checker_bounds_unicode_and_preserves_error_after_the_retained_prefix() {
    let socket = Path::new("/no-server-unicode-diagnostics-test/herdr.sock");
    let stderr = format!("{}\nlast config error: 🦀設定\n  \n", "🦀".repeat(9000));
    checker(socket, &"入力".repeat(5000).into_bytes(), stderr.as_bytes());
    let view = snapshot(socket);
    assert!(
        view["checker"]
            .as_str()
            .is_some_and(|detail| detail.chars().count() <= 512
                && detail.ends_with("last config error: 🦀設定")),
        "health_checker_excerpt_is_bounded_and_keeps_the_last_error_contract"
    );
    let mut config = crate::config::Config::default();
    config.session_hosts.herdr.endpoints.insert(
        "user".into(),
        crate::config::session_hosts::EndpointConfig {
            mode: crate::config::session_hosts::EndpointMode::UserDefault,
            execution_node: "fixture".into(),
            socket_path: socket.into(),
            herdr_home: Default::default(),
            herdr_session: "default".into(),
        },
    );
    let evidence = doctor_evidence(&config);
    assert_eq!(
        evidence["user"]["stderr"].as_str().unwrap().chars().count(),
        4096,
        "doctor_keeps_the_bounded_raw_checker_stream_contract"
    );
    assert_eq!(
        evidence["user"]["last_error_line"],
        "last config error: 🦀設定"
    );
    assert!(evidence["user"]["stdout"].as_str().unwrap().chars().count() <= 4096);
    config
        .session_hosts
        .herdr
        .endpoints
        .get_mut("user")
        .unwrap()
        .mode = crate::config::session_hosts::EndpointMode::LegacyDedicated;
    assert_eq!(
        doctor_evidence(&config),
        json!({}),
        "legacy_doctor_exposes_no_user_checker_evidence_contract"
    );
}
