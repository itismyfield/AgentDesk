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
fn unsupported_refusal_names_the_server_version_the_socket_reported() {
    let socket = Path::new("/no-server-version-diagnostics-test/herdr.sock");
    server(socket, "0.9.2", 22);
    let view = record(
        socket,
        &ProofCache::default(),
        &Err(Unverified::Unsupported),
    );
    assert_eq!(view["reason"], "next_restart_policy_unverified:Unsupported");
    assert_eq!(view["server"], "herdr 0.9.2 / protocol 22");
    assert_eq!(snapshot(socket)["server"], view["server"]);
    server(socket, &"9".repeat(500), 21);
    let view = record(
        socket,
        &ProofCache::default(),
        &Err(Unverified::Unsupported),
    );
    assert_eq!(
        view["server"],
        format!("herdr {} / protocol 21", "9".repeat(64))
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

#[test]
fn doctor_uses_the_selected_boot_socket_when_raw_config_omits_the_default() {
    use crate::config::session_hosts::{self, EndpointMode};
    let _boot = session_hosts::force_for_test(
        Some("doctor-default-node"),
        &[(7309, "doctor-default-node")],
    )
    .with_mode(EndpointMode::UserDefault);
    let mut config = crate::config::Config::default();
    config.session_hosts = session_hosts::with_boot(|boot| boot.unwrap().config().clone());
    let endpoint = config
        .session_hosts
        .herdr
        .endpoints
        .get_mut("doctor-default-node-endpoint")
        .unwrap();
    checker(
        &endpoint.socket_path,
        b"resolved stdout",
        b"resolved stderr",
    );
    endpoint.socket_path = PathBuf::new();
    assert_eq!(
        doctor_evidence(&config)["doctor-default-node-endpoint"]["stderr"],
        "resolved stderr",
        "doctor_omitted_socket_uses_selected_boot_observation_contract"
    );
    config
        .session_hosts
        .herdr
        .endpoints
        .get_mut("doctor-default-node-endpoint")
        .unwrap()
        .mode = EndpointMode::LegacyDedicated;
    assert_eq!(
        doctor_evidence(&config),
        json!({}),
        "legacy_doctor_still_returns_before_selected_boot_observation_contract"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn diagnosis_names_each_loaded_image_refusal_and_never_calls_ambiguity_a_change() {
    let socket = Path::new("/no-server-image-diagnostics-test/herdr.sock");
    let cache = ProofCache::default();
    let mut actions = Vec::new();
    for (refusal, reason) in [
        (
            Unverified::ImageChangedSinceStart,
            "next_restart_image_changed_since_start",
        ),
        (
            Unverified::ImageTimeAmbiguous,
            "next_restart_image_time_ambiguous",
        ),
        (
            Unverified::ImageFilesystemUnsupported,
            "next_restart_image_filesystem_unsupported",
        ),
        (
            Unverified::ImageLayoutUnsupported,
            "next_restart_image_layout_unsupported",
        ),
    ] {
        record(socket, &cache, &Err(refusal));
        let view = snapshot(socket);
        assert_eq!(view["reason"], reason, "{refusal:?}");
        actions.push(view["action"].as_str().unwrap_or_default().to_owned());
    }
    assert!(
        actions[0].contains("changed after this server process started")
            && !actions[1].contains("changed after"),
        "an order-unknown change time is not reported as a later change"
    );
    let distinct: std::collections::BTreeSet<_> = actions.iter().collect();
    assert_eq!(distinct.len(), 4, "each refusal has its own action");
}

#[test]
fn an_unverified_peer_is_not_called_an_absent_server() {
    let socket = Path::new("/no-server-peer-diagnostics-test/herdr.sock");
    for reason in [Unverified::Peer, Unverified::Changed] {
        let view = record(socket, &ProofCache::default(), &Err(reason));
        let action = view["action"].as_str().unwrap_or_default();
        assert!(
            action.contains("could not be verified") && action.contains("autostart"),
            "{action}"
        );
        for claim in ["listening", "absent", "not running", "start bare herdr"] {
            assert!(
                !action.contains(claim),
                "{reason:?} asserts absence: {action}"
            );
        }
    }
}
