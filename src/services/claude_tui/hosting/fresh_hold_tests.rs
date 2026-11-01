use super::*;
use crate::services::claude_tui::composer_lock::{DraftGuard, guard_draft};
use crate::services::claude_tui::host_input::{SpyGuard, SpyState};
use crate::services::claude_tui::input::submission::tests::{frame_renderer as renderer, screen};
use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;

#[test]
fn a2_fresh_readiness_hold_never_kills_the_session_or_owner_marker() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    let name = "a2-fresh-held";
    let transcript = f.root.path().join("a2-fresh.jsonl");
    let owner = f.root.path().join("a2-owner");
    std::fs::write(&transcript, "").unwrap();
    std::fs::write(&owner, "owner").unwrap();
    let previous = f.root.path().join("previous-native.jsonl");
    std::fs::write(&previous, "old transcript\n").unwrap();
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        name,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            output_path: previous.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let calls = f.root.path().join("a2-calls");
    let shim = f.root.path().join("tmux");
    let script = std::fs::read_to_string(&shim).unwrap();
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{}",
            calls.display(),
            script.strip_prefix("#!/bin/sh\n").unwrap()
        ),
    )
    .unwrap();
    for _ in 0..2 {
        if crate::services::platform::tmux::session_presence(name)
            != crate::services::platform::tmux::SessionPresence::Present
        {
            let prior = f.root.path().join(name);
            if prior.exists() {
                std::fs::rename(&prior, f.root.path().join(format!("{name}-retired"))).unwrap();
            }
            renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
        }
        guard_draft(name, DraftGuard::RecoveryRequired);
        let spy = SpyGuard::install(SpyState {
            captures: [
                Some(screen("")),
                Some(screen("")),
                Some(screen("")),
                Some(screen("")),
                Some("unknown overlay".into()),
            ]
            .into_iter()
            .collect(),
            draft_capture_at: Some((1, Some("unknown overlay".into()))),
            ..SpyState::default()
        });
        let (tx, _) = std::sync::mpsc::channel();
        let result = run_claude_tui_fresh_turn_and_finalize(
            &transcript,
            transcript.to_str().unwrap(),
            tx,
            Some(std::sync::Arc::new(CancelToken::new())),
            name,
            Some(&TeardownClearance::Unkeyed),
            "held-sid",
            None,
            "own",
            owner.to_str().unwrap(),
        );
        assert!(result.is_err());
        assert!(
            !spy.calls()
                .iter()
                .any(|c| c.starts_with("literal:") || c.starts_with("paste:"))
        );
    }
    let calls = std::fs::read_to_string(&calls).unwrap();
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.contains("kill-session"))
            .count(),
        0,
        "preparation holds must not kill: {calls}"
    );
    assert!(owner.exists(), "the held session retains its owner marker");
    assert_eq!(
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(name)
            .unwrap()
            .output_path,
        previous.display().to_string(),
        "preparation hold retains the old binding without killing or resetting it"
    );
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(name);
    assert_eq!(
        crate::services::platform::tmux::session_presence(name),
        crate::services::platform::tmux::SessionPresence::Present
    );
}

#[test]
fn p2_actual_warm_session_died_then_recreate_fresh_reuses_token() {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::claude_tui::input::submission::tests::{ScriptedTmux, view};
    use crate::services::tui_prompt_dedupe::{self, TuiRuntimeBinding};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("old.jsonl");
    let new = dir.path().join("recreated.jsonl");
    let (session, sid, frame) = ("p2-warm-recreate", "warm-sid", "own");
    std::fs::write(&old, "").unwrap();
    let binding = |path: &std::path::Path| TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(sid.into()),
        last_offset: 0,
        relay_last_offset: None,
    };
    tui_prompt_dedupe::register_tmux_runtime_binding(session, binding(&old));
    let tmux = ScriptedTmux::new(&view("", false));
    tmux.set_dead(false);
    tmux.die_on_enter();
    // No bytes may arrive: any offset advance makes main suppress a recreate.
    let _ready = crate::services::claude_tui::host_input::SpyGuard::install(
        crate::services::claude_tui::host_input::SpyState {
            captures: std::iter::repeat_n(
                Some(
                    include_str!("../../../../tests/fixtures/tui_input/claude-2.1.293-idle.txt")
                        .into(),
                ),
                64,
            )
            .collect(),
            ..Default::default()
        },
    );
    let token = std::sync::Arc::new(CancelToken::new());
    token.bind_claude_tmux_session(session);
    let (sender, _receiver) = std::sync::mpsc::channel();
    let outcome = crate::services::claude_tui::guarded_outcome::with_owner(None, || {
        crate::services::claude_tui::hosting::try_claude_tui_warm_followup(
            sid.into(),
            old.clone(),
            old.to_str().unwrap().into(),
            true,
            dir.path(),
            frame,
            sender.clone(),
            Some(token.clone()),
            &crate::services::claude_tui::hosting::FollowupHost::legacy_tmux(session),
            None,
        )
    });
    match outcome {
        crate::services::claude_tui::hosting::ClaudeTuiWarmFollowupOutcome::Recreate(_) => {}
        crate::services::claude_tui::hosting::ClaudeTuiWarmFollowupOutcome::Terminal(result) => {
            panic!(
                "must pass real warm SessionDied classification and retirement: {result:?}; sends={:?}",
                tmux.sends()
            )
        }
    }
    std::fs::write(&new, "").unwrap();
    std::fs::write(
        crate::services::tmux_common::session_temp_path(session, "spawn_nonce"),
        "recreated-pane",
    )
    .unwrap();
    tui_prompt_dedupe::register_tmux_runtime_binding(session, binding(&new));
    tmux.set_dead(false);
    tmux.on_enter(&new, &format!("{}\n{{\"type\":\"system\",\"subtype\":\"stop_hook_summary\"}}",
        serde_json::json!({"type":"user","uuid":"recreated-a","sessionId":sid,"message":{"role":"user","content":frame}})));
    let owner = dir.path().join("owner");
    let result = crate::services::claude_tui::guarded_outcome::with_owner(None, || {
        run_claude_tui_fresh_turn_and_finalize(
            &new,
            new.to_str().unwrap(),
            sender,
            Some(token.clone()),
            session,
            Some(&TeardownClearance::Unkeyed),
            sid,
            None,
            frame,
            owner.to_str().unwrap(),
        )
    });
    assert!(result.is_ok(), "fresh lifecycle succeeds: {result:?}");
    let pin = token
        .native_stop
        .receipt_sink()
        .observed()
        .expect("new reader observed the recreated input")
        .expect("new incarnation must not remain poisoned by the old submission");
    let pin = serde_json::to_value(pin).unwrap();
    assert_eq!(pin.get("uuid").unwrap_or(&pin[0]), "recreated-a");
    tui_prompt_dedupe::clear_tmux_runtime_binding(session);
}
