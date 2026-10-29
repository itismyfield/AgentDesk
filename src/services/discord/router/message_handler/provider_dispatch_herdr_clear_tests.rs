//! A tmux Claude that moved on to a new session by `/clear` moves to Herdr under that session.

use super::*;
use crate::services::provider_teardown::tests::test_support::{FakeTmux, cleared};
use crate::services::tui_prompt_dedupe::binding_context::{
    CapturedContext, HookBindingEnvelope, execution_context,
};

/// Polls `ready` for up to a minute.
fn eventually(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// O's cursor over all of `path`, with the prefix hash its writer keeps.
fn read_through(path: &Path, session: &str) {
    use crate::services::tui_o::store::spool::{Cursor, source_key};
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::MetadataExt;
    let body = std::fs::read(path).unwrap();
    let meta = std::fs::metadata(path).unwrap();
    let source = crate::services::tui_o::shadow::SourceId {
        session_id: session.to_owned(),
        path: path.to_path_buf(),
        dev: meta.dev(),
        ino: meta.ino(),
    };
    let root = crate::config::runtime_root().unwrap();
    let dir = root
        .join("o_store")
        .join(CHANNEL.to_string())
        .join("cursor");
    std::fs::create_dir_all(&dir).unwrap();
    let cursor = Cursor {
        source: source.clone(),
        captured_through: body.len() as u64,
        prefix_hash: hex::encode(Sha256::digest(&body)),
        retired: false,
        transition_cleanup: None,
    };
    let file = dir.join(format!("{}.json", source_key(&source)));
    std::fs::write(file, serde_json::to_vec(&cursor).unwrap()).unwrap();
}

// The tmux launch records its session's account, a `/clear` of that execution moves the record on
// to the new session, and the move to Herdr resumes the new session after one `/exit`.
#[test]
fn f9_a_session_a_clear_moved_on_to_resumes_on_herdr_under_its_launch_account_pg() {
    let fx = Fixture::new("resume-f9", None);
    let logical = fx.logical().to_owned();
    let cwd = fx.cwd.path().to_str().unwrap().to_owned();
    let tmux = FakeTmux::install(&logical);
    tmux.mark_missing();
    tmux.enable_fixture_launch();
    let launch_bin = fx.own_claude();
    let record = crate::services::tmux_common::session_temp_path(&logical, "auth_session");
    let spawn = crate::services::tmux_common::session_temp_path(&logical, "spawn_nonce");
    let cancel = Arc::new(CancelToken::new());
    std::thread::scope(|scope| {
        let clearance = cleared(&logical);
        let token = cancel.clone();
        let (cwd, logical) = (&cwd, &logical);
        let turn = scope.spawn(move || {
            crate::services::claude::tmux_tui_turn_for_tests(
                cwd, token, logical, &clearance, CHANNEL,
            )
        });
        eventually("the tmux launch", || {
            Path::new(&record).exists() && Path::new(&spawn).exists()
        });
        cancel.cancelled.store(true, Ordering::SeqCst);
        let _ = turn.join().unwrap();
    });
    let launched = std::fs::read_to_string(&record).unwrap();
    let (s0, account) = launched.split_once('\n').unwrap();
    let s0 = s0.to_owned();
    assert_eq!(
        account,
        crate::services::provider_auth_profile::DEFAULT_PROFILE_ID
    );

    // The same execution's `/clear`: its SessionStart names a new session with its transcript.
    let s1 = uuid();
    let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        fx.cwd.path(),
        &s1,
        None,
    )
    .unwrap();
    append(&path, &[json!({"type": "mode", "sessionId": s1})]);
    let nonce = std::fs::read_to_string(&spawn).unwrap();
    let s0_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        fx.cwd.path(),
        &s0,
        None,
    )
    .unwrap();
    append(&s0_path, &[json!({"type": "mode", "sessionId": s0})]);
    crate::services::tui_prompt_dedupe::register_provider_session("claude", &s0, &logical);
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        &logical,
        claude(&s0_path, &s0),
    );
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(execution_context("claude", nonce.trim()).unwrap()),
        observed: Default::default(),
    };
    let payload = json!({"session_id": s1, "source": "clear", "transcript_path": path});
    let (status, _) = fx.ingress.send_envelope(
        &format!("/hooks/claude/SessionStart?session_id={s0}"),
        &payload,
        Some(&uuid()),
        Some(&envelope.encode().unwrap()),
    );
    assert_eq!(status, 202);
    crate::services::claude_tui::hook_server::retry_deferred_claude_adoptions();
    let bound = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&logical);
    assert_eq!(bound.unwrap().session_id.as_deref(), Some(s1.as_str()));

    // A completed turn on the new session, read through by O, then the move to Herdr.
    let said = json!({"type": "user", "sessionId": s1, "timestamp": "2026-10-01T00:00:00Z",
        "message": {"role": "user", "content": "기억해: beta"}});
    let answered = json!({"type": "assistant", "sessionId": s1,
        "timestamp": "2026-10-01T00:00:01Z", "message": {"role": "assistant",
        "stop_reason": "end_turn", "content": [{"type": "text", "text": "beta"}]}});
    append(&path, &[said, answered]);
    read_through(&path, &s1);
    *fx.session.lock().unwrap() = Some(s1.clone());
    drop((launch_bin, tmux));
    let launcher = Arc::new(Launcher::default());
    let ports = fx.ports(&launcher);
    ports.old_tmux.alive.store(true, Ordering::SeqCst);
    let (result, _) = fx.turn(&HostedRecord::Legacy, &ports, || {
        if let Some(started) = fx.start_provider_as(&launcher, true, "resume") {
            assert_eq!((&started.session, &started.path), (&s1, &path));
            fx.answer(&started);
        }
    });
    assert_eq!(result, Ok(()));
    assert_eq!(ports.old_tmux.exits.load(Ordering::SeqCst), 1);
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    assert_eq!(fx.rig.sends(), prompt_sends());
    assert_eq!(fx.row(), Some(HostedState::Bound));
    let script = fx.launch_script();
    assert!(script.contains(&format!("'--resume' '{s1}'")), "{script}");
    assert!(!script.contains("'--session-id'"), "{script}");
    crate::services::tmux_common::cleanup_session_temp_files(&logical);
}
