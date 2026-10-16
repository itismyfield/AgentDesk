use super::*;
use crate::services::tui_o::cutover::test_override;
use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};

fn config(enabled: bool, foreign: bool) -> Config {
    let agents: Vec<_> = [41_u64, 42, 43, 44]
        .into_iter()
        .map(|id| {
            let provider = if id == 42 { "codex" } else { "claude" };
            serde_json::json!({"id": format!("a{id}"), "name": "Fixture",
            "channels": {provider: {"id": id.to_string(), "runtime": "tui"}}})
        })
        .collect();
    serde_json::from_value(serde_json::json!({
        "server": {}, "agents": agents,
        "cluster": {"enabled": foreign, "instance_id": "worker", "gateway_preferred_instance_id": "home"},
        "tui_o": {"writer": {"channels": [41,42,43,44], "late_adopt": enabled}}
    })).unwrap()
}

#[test]
fn production_seed_reserves_before_any_host_placement_or_body() {
    for foreign in [false, true] {
        let config = config(true, foreign);
        let boot = BootChannels::validate(&config)
            .unwrap()
            .seeded(true, &config, |_, _| {
                Ok(BTreeMap::from([
                    (41, Adoption::Pending),
                    (42, Adoption::Pending),
                    (43, Adoption::Held),
                    (44, Adoption::Committed),
                ]))
            })
            .unwrap();
        let candidate = boot.candidate(41).or(boot.standby(41)).unwrap().clone();
        assert!(boot.late_adopt_capable());
        assert!(
            candidate.late_adopt_origin(),
            "actual seeded origin, foreign={foreign}"
        );
        assert_eq!(candidate.peek(), Adoption::Deferred);
        let codex = boot.candidate(42).or(boot.standby(42)).unwrap();
        assert!(codex.late_adopt_origin(), "Codex reserves as Claude does");
        assert_eq!(codex.peek(), Adoption::Deferred);
        assert_eq!(
            boot.candidate(43).or(boot.standby(43)).unwrap().peek(),
            Adoption::Held
        );
        assert_eq!(
            boot.candidate(44).or(boot.standby(44)).unwrap().peek(),
            Adoption::Committed
        );
        let _installed = test_override::force_boot(boot);
        assert!(!candidate.claim(41));
        let body = candidate.claim_body(41);
        assert!(!body.owned);
        assert_eq!(
            candidate.peek(),
            Adoption::Deferred,
            "pre-host placement must preserve reservation"
        );
        assert_eq!(candidate.sends(), (1, 0));
        drop(body);
        assert_eq!(candidate.sends(), (1, 1));
    }
}

#[test]
fn boot_off_and_writer_off_keep_the_original_pending_policy() {
    for (late, writer) in [(false, true), (true, false)] {
        let config = config(late, false);
        let boot = BootChannels::validate(&config)
            .unwrap()
            .seeded(writer, &config, |_, _| {
                Ok(BTreeMap::from([(41, Adoption::Pending)]))
            })
            .unwrap();
        if writer {
            let candidate = boot.candidate(41).unwrap();
            assert!(!candidate.late_adopt_origin());
            assert!(!candidate.claim(41));
            assert_eq!(candidate.peek(), Adoption::Released);
        } else {
            assert!(boot.candidate(41).is_none());
        }
    }
}

#[test]
fn actual_install_child_publishes_reserved_candidate() {
    const CHILD: &str = "ADK_TEST_LATE_RESERVATION_INSTALL";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "services::tui_o::channel_policy::late_reservation_tests::actual_install_child_publishes_reserved_candidate", "--nocapture"])
            .env(CHILD, "1").env(test_override::CHILD_ENV, "1")
            .env("AGENTDESK_ROOT_DIR", root.path()).output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
        return;
    }
    install(&config(true, false)).unwrap();
    let candidate = boot().unwrap().candidate(41).unwrap();
    assert!(candidate.late_adopt_origin());
    assert!(!candidate.claim(41));
    assert_eq!(candidate.peek(), Adoption::Deferred);
}

#[test]
fn cleanup_before_first_poll_and_panic_releases_absent_store() {
    for panic in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let candidate = Candidate::reserved_late();
        let guard = candidate
            .late_cleanup_guard(41, Some(root.path().to_owned()))
            .unwrap();
        if panic {
            let result = std::panic::catch_unwind(move || {
                let _guard = guard;
                panic!("pre-init host");
            });
            assert!(result.is_err());
        } else {
            drop(guard);
        }
        assert_eq!(candidate.peek(), Adoption::Released);
    }
}

fn init() -> Initialized {
    Initialized {
        channel: 41,
        sources: Vec::new(),
        initial_anchor: 0,
        build_digest: "late-fixture".into(),
        at: chrono::Utc::now(),
    }
}

#[test]
fn cleanup_settles_published_init_and_orphan_before_releasing() {
    for sealed in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let candidate = Candidate::reserved_late();
        let guard = candidate
            .late_cleanup_guard(41, Some(root.path().to_owned()))
            .unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root.path())
            .unwrap()
            .unwrap();
        if sealed {
            store.begin_era(&[41], init().at, |_| Ok(init())).unwrap();
        } else {
            store.init_channel(&init()).unwrap();
        }
        drop(guard);
        assert_eq!(
            candidate.peek(),
            if sealed {
                Adoption::Committed
            } else {
                Adoption::Held
            }
        );
    }
}

#[test]
fn cleanup_preserves_decided_states_and_does_not_claim_ordinary_deferred() {
    for state in [Adoption::Committed, Adoption::Held, Adoption::Released] {
        let candidate = Candidate::reserved_late();
        let guard = candidate.late_cleanup_guard(41, None).unwrap();
        *candidate.lock() = state;
        drop(guard);
        assert_eq!(candidate.peek(), state);
    }
    assert!(
        Candidate::new(Adoption::Deferred)
            .late_cleanup_guard(41, None)
            .is_none()
    );
}
