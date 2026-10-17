use super::*;

#[test]
fn canary_hold_diagnostic_preserves_expired_state_when_launch_context_is_unavailable() {
    let h = Fixture::canary();
    h.header(ID, false);
    h.send("session-start", ID, Value::Null, None);
    h.assert_proof();
    let before_fold = h.fold();
    let before_marker = fs::read(h.marker()).unwrap();
    let before_nonce = dedupe::binding_context::observe_spawn_nonce_marker(&h.context.tmux_session);
    let (before_channel, before_binding) = {
        let mut state = STATE.lock().unwrap_or_else(|p| p.into_inner());
        let _ = state.aged_stamp_for_tests(SESSION_MAPPING_TTL + std::time::Duration::from_secs(1));
        let expired = Instant::now() - SESSION_MAPPING_TTL - std::time::Duration::from_secs(1);
        let channel = state
            .channel_by_tmux
            .get_mut(&h.context.tmux_session)
            .unwrap();
        channel.recorded_at = expired;
        let before_channel = (channel.value, channel.recorded_at);
        let binding = state
            .runtime_by_tmux
            .get_mut(&h.context.tmux_session)
            .unwrap();
        binding.recorded_at = expired;
        (before_channel, (binding.value.clone(), binding.recorded_at))
    };
    fs::remove_file(&h.canonical).unwrap();
    for _ in 0..5 {
        assert_eq!(
            codex_verified_hold_reason(&h.context.tmux_session),
            "verified_unavailable"
        );
        assert_eq!(
            codex_verified_hold_detail(&h.context.tmux_session),
            "execution_context_unreadable"
        );
        let state = STATE.lock().unwrap_or_else(|p| p.into_inner());
        let channel = state
            .channel_by_tmux
            .get(&h.context.tmux_session)
            .expect("a diagnostic must not purge the expired channel mapping");
        let binding = state
            .runtime_by_tmux
            .get(&h.context.tmux_session)
            .expect("a diagnostic must not purge the expired runtime binding");
        assert_eq!((channel.value, channel.recorded_at), before_channel);
        assert_eq!(
            (&binding.value, binding.recorded_at),
            (&before_binding.0, before_binding.1)
        );
    }
    assert_eq!(h.raw(), Some(before_binding.0));
    assert_eq!(h.fold(), before_fold);
    assert_eq!(fs::read(h.marker()).unwrap(), before_marker);
    assert_eq!(
        dedupe::binding_context::observe_spawn_nonce_marker(&h.context.tmux_session),
        before_nonce
    );
}

#[test]
fn detailed_hold_reason_identifies_missing_nonce_without_changing_legacy_reason() {
    let h = Fixture::canary();
    h.header(ID, false);
    h.send("session-start", ID, Value::Null, None);
    fs::remove_file(crate::services::tmux_common::session_temp_path(
        &h.context.tmux_session,
        "spawn_nonce",
    ))
    .unwrap();
    assert!(codex_verified_input_blocked(&h.context.tmux_session));
    assert_eq!(
        codex_verified_hold_reason(&h.context.tmux_session),
        "verified_unavailable"
    );
    assert_eq!(
        codex_verified_hold_detail(&h.context.tmux_session),
        "nonce_marker_missing"
    );
}

fn assert_reason(h: &Fixture, detail: &str, legacy: &str) {
    assert_eq!(codex_verified_hold_reason(&h.context.tmux_session), legacy);
    assert_eq!(codex_verified_hold_detail(&h.context.tmux_session), detail);
}

#[test]
fn detailed_hold_reason_separates_launch_context_failures() {
    for reason in [
        "nonce_marker_unreadable",
        "execution_context_unreadable",
        "tmux_session_mismatch",
        "owner_root_mismatch",
        "source_policy_changed",
    ] {
        let h = Fixture::new("fresh", None);
        h.header(ID, false);
        h.send("session-start", ID, Value::Null, None);
        let mut context = h.context.clone();
        match reason {
            "nonce_marker_unreadable" => fs::write(
                crate::services::tmux_common::session_temp_path(
                    &context.tmux_session,
                    "spawn_nonce",
                ),
                "",
            )
            .unwrap(),
            "execution_context_unreadable" => fs::remove_file(&h.canonical).unwrap(),
            "tmux_session_mismatch" => context.tmux_session.push_str("-other"),
            "owner_root_mismatch" => context.owner_runtime_root.push_str("-other"),
            "source_policy_changed" => context.source_policy = Some("legacy".into()),
            _ => unreachable!(),
        }
        if context != h.context {
            fs::write(&h.canonical, serde_json::to_vec(&context).unwrap()).unwrap();
        }
        assert_reason(
            &h,
            reason,
            if reason == "source_policy_changed" {
                "binding_missing"
            } else {
                "verified_unavailable"
            },
        );
        assert_eq!(
            codex_verified_input_blocked(&h.context.tmux_session),
            reason != "source_policy_changed"
        );
    }
}

#[test]
fn detailed_hold_reason_separates_ownership_and_binding_failures() {
    for reason in [
        "ownership_missing",
        "ownership_pending",
        "ownership_unreadable",
        "binding_missing",
        "source_identity_mismatch",
    ] {
        let h = Fixture::new("fresh", None);
        match reason {
            "ownership_missing" => {}
            "ownership_pending" => {
                h.send("user-prompt-submit", ID, json!(PROMPT), None);
            }
            _ => {
                h.header(ID, false);
                h.send("session-start", ID, Value::Null, None);
                h.assert_proof();
                match reason {
                    "ownership_unreadable" => fs::write(
                        h.root
                            .path()
                            .join(binding_events::BINDING_EVENTS_DIR)
                            .join(format!("{}.log", h.context.channel_id.unwrap())),
                        "corrupt\n",
                    )
                    .unwrap(),
                    "binding_missing" => {
                        STATE
                            .lock()
                            .unwrap()
                            .runtime_by_tmux
                            .remove(&h.context.tmux_session);
                    }
                    "source_identity_mismatch" => {
                        STATE
                            .lock()
                            .unwrap()
                            .runtime_by_tmux
                            .get_mut(&h.context.tmux_session)
                            .unwrap()
                            .value
                            .session_id = Some(CHILD.into());
                    }
                    _ => unreachable!(),
                }
            }
        }
        assert!(codex_verified_input_blocked(&h.context.tmux_session));
        assert_reason(
            &h,
            reason,
            match reason {
                "binding_missing" => "binding_missing",
                "source_identity_mismatch" => "source_identity_mismatch",
                _ => "verified_unavailable",
            },
        );
    }
}

#[test]
fn detailed_hold_reason_separates_history_and_canary_fallbacks() {
    use crate::services::codex::{CodexSourceMode, SOURCE_MODE_TEST};
    struct ModeReset;
    impl Drop for ModeReset {
        fn drop(&mut self) {
            SOURCE_MODE_TEST.with(|mode| mode.set(None));
        }
    }
    for reason in [
        "execution_history_unreadable",
        "proof_context_missing",
        "canary_pin_unavailable",
        "canary_history_unreadable",
        "canary_unavailable",
    ] {
        let h = Fixture::canary();
        let _mode_reset = ModeReset;
        h.header(ID, false);
        h.send("session-start", ID, Value::Null, None);
        fs::remove_file(&h.canonical).unwrap();
        if reason != "execution_history_unreadable" {
            STATE
                .lock()
                .unwrap()
                .channel_by_tmux
                .remove(&h.context.tmux_session);
        }
        if reason != "proof_context_missing" {
            fs::remove_file(h.marker()).unwrap();
        }
        let log = h
            .root
            .path()
            .join(binding_events::BINDING_EVENTS_DIR)
            .join(format!("{CANARY_CHANNEL}.log"));
        match reason {
            "execution_history_unreadable" | "canary_history_unreadable" => {
                fs::write(log, "corrupt\n").unwrap()
            }
            "canary_unavailable" => {
                fs::remove_file(log).unwrap();
                SOURCE_MODE_TEST.with(|mode| mode.set(Some(CodexSourceMode::Verified)));
            }
            _ => {}
        }
        assert!(codex_verified_input_blocked(&h.context.tmux_session));
        assert_reason(&h, reason, "verified_unavailable");
    }
}
