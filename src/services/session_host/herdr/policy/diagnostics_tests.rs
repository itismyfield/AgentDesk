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
    assert_eq!(view["checker"]["stdout"].as_str().unwrap().len(), 4096);
    assert!(
        view["checker"]["stderr"]
            .as_str()
            .unwrap()
            .contains("theme.typo")
    );
    assert!(
        !socket.exists(),
        "health_diagnostics_creates_no_server_or_config_contract"
    );
}
