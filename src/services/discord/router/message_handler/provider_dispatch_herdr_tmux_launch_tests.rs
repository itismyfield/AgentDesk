//! Real tmux Claude launches whose sessions move to Herdr or come back from it.

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

/// Runs the channel's tmux turn on a fake tmux until its launch recorded its session's account and
/// spawn nonce, then cancels it; the recorded `session\naccount` and the nonce.
fn launch_tmux(fx: &Fixture, session: Option<&str>) -> (String, String) {
    let logical = fx.logical().to_owned();
    let cwd = fx.cwd.path().to_str().unwrap().to_owned();
    let tmux = FakeTmux::install(&logical);
    tmux.mark_missing();
    tmux.enable_fixture_launch();
    let _launch_bin = fx.own_claude();
    let record = crate::services::tmux_common::session_temp_path(&logical, "auth_session");
    let spawn = crate::services::tmux_common::session_temp_path(&logical, "spawn_nonce");
    let cancel = Arc::new(CancelToken::new());
    std::thread::scope(|scope| {
        let clearance = cleared(&logical);
        let token = cancel.clone();
        let (cwd, logical) = (&cwd, &logical);
        let turn = scope.spawn(move || {
            crate::services::claude::tmux_tui_turn_for_tests(
                cwd, session, token, logical, &clearance, CHANNEL,
            )
        });
        eventually("the tmux launch", || {
            Path::new(&record).exists() && Path::new(&spawn).exists()
        });
        cancel.cancelled.store(true, Ordering::SeqCst);
        let _ = turn.join().unwrap();
    });
    let launched = std::fs::read_to_string(&record).unwrap();
    let nonce = std::fs::read_to_string(&spawn).unwrap();
    (launched, nonce.trim().to_owned())
}

/// The launched execution's pane bound to its launch session `s0`, and its hook envelope.
fn launched_pane(fx: &Fixture, s0: &str, nonce: &str) -> HookBindingEnvelope {
    let logical = fx.logical();
    let path = claude_path(fx, s0);
    append(&path, &[json!({"type": "mode", "sessionId": s0})]);
    crate::services::tui_prompt_dedupe::register_provider_session("claude", s0, logical);
    let binding = claude(&path, s0);
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(logical, binding);
    HookBindingEnvelope {
        context: CapturedContext::Captured(execution_context("claude", nonce).unwrap()),
        observed: Default::default(),
    }
}

fn claude_path(fx: &Fixture, session: &str) -> PathBuf {
    let path = crate::services::claude_tui::transcript_tail::claude_transcript_path;
    path(fx.cwd.path(), session, None).unwrap()
}

/// The same execution's `/clear` to a new session and that session's first prompt: their hooks,
/// under the hook command the running Claude launched with, name the new session and its
/// transcript, which the pane adopts.
fn clear_to_new_session(
    fx: &Fixture,
    envelope: &HookBindingEnvelope,
    s0: &str,
) -> (String, PathBuf) {
    let session = uuid();
    let path = claude_path(fx, &session);
    append(&path, &[json!({"type": "mode", "sessionId": session})]);
    let started = json!({"session_id": session, "source": "clear", "transcript_path": path});
    let prompted = json!({"session_id": session, "prompt": "hi", "transcript_path": path});
    for (event, payload) in [("SessionStart", started), ("UserPromptSubmit", prompted)] {
        let (status, _) = fx.ingress.send_envelope(
            &format!("/hooks/claude/{event}?session_id={s0}"),
            &payload,
            Some(&uuid()),
            Some(&envelope.encode().unwrap()),
        );
        assert_eq!(status, 202, "{event}");
        crate::services::claude_tui::hook_server::retry_deferred_claude_adoptions();
    }
    let bound = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(fx.logical());
    assert_eq!(bound.unwrap().session_id.as_deref(), Some(session.as_str()));
    (session, path)
}

/// A completed turn on `session`, read through by O, then the move to Herdr: one `/exit`, one
/// launch resuming `session` and one prompt.
fn moves_to_herdr_resuming(fx: &Fixture, session: &str, path: &Path) {
    let said = json!({"type": "user", "sessionId": session, "timestamp": "2026-10-01T00:00:00Z",
        "message": {"role": "user", "content": "기억해: beta"}});
    let answered = json!({"type": "assistant", "sessionId": session,
        "timestamp": "2026-10-01T00:00:01Z", "message": {"role": "assistant",
        "stop_reason": "end_turn", "content": [{"type": "text", "text": "beta"}]}});
    append(path, &[said, answered]);
    read_through(path, session);
    *fx.session.lock().unwrap() = Some(session.to_owned());
    let launcher = Arc::new(Launcher::default());
    let ports = fx.ports(&launcher);
    ports.old_tmux.alive.store(true, Ordering::SeqCst);
    let (result, _) = fx.turn(&HostedRecord::Legacy, &ports, || {
        if let Some(started) = fx.start_provider_as(&launcher, true, "resume") {
            let expected = (session.to_owned(), path.to_path_buf());
            assert_eq!((started.session.clone(), started.path.clone()), expected);
            fx.answer(&started);
        }
    });
    assert_eq!(result, Ok(()));
    assert_eq!(ports.old_tmux.exits.load(Ordering::SeqCst), 1);
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    assert_eq!(fx.rig.sends(), prompt_sends());
    assert_eq!(fx.row(), Some(HostedState::Bound));
    let script = fx.launch_script();
    assert!(
        script.contains(&format!("'--resume' '{session}'")),
        "{script}"
    );
    assert!(!script.contains("'--session-id'"), "{script}");
}

// The tmux launch records its session's account, each `/clear` of that execution moves the record
// on to its new session, and the move to Herdr resumes the last one after one `/exit`.
fn clears_then_moves_to_herdr(tag: &str, clears: usize) {
    let fx = Fixture::new(tag, None);
    let (launched, nonce) = launch_tmux(&fx, None);
    let (s0, account) = launched.split_once('\n').unwrap();
    assert_eq!(
        account,
        crate::services::provider_auth_profile::DEFAULT_PROFILE_ID
    );
    let envelope = launched_pane(&fx, s0, &nonce);
    let mut last = (s0.to_owned(), claude_path(&fx, s0));
    for _ in 0..clears {
        last = clear_to_new_session(&fx, &envelope, s0);
    }
    let (session, path) = last;
    // Only the adoptions moved the record; the test never wrote it after the launch.
    assert_eq!(fx.session_auth(), Some(format!("{session}\n{account}")));
    moves_to_herdr_resuming(&fx, &session, &path);
    crate::services::tmux_common::cleanup_session_temp_files(fx.logical());
}

#[test]
fn f9_a_session_a_clear_moved_on_to_resumes_on_herdr_under_its_launch_account_pg() {
    clears_then_moves_to_herdr("resume-f9", 1);
}

// The running Claude keeps its launch hook command, so the second `/clear`'s hooks name the launch
// session while the pane moves on from the first cleared one, on that session's first prompt.
#[test]
fn f9b_a_session_a_second_clear_moved_on_to_resumes_on_herdr_under_its_launch_account_pg() {
    clears_then_moves_to_herdr("resume-f9b", 2);
}

// A session handed back to tmux under a named account is found in that account's own home and
// resumed there, its launch context naming that home; in the daemon's home only it starts anew.
#[test]
fn a_named_account_tmux_turn_resumes_its_session_from_its_own_home_pg() {
    use crate::services::claude::herdr_return::tests::claude_account;
    for own in [true, false] {
        let fx = Fixture::new("resume-home", None);
        let (_account, _user, home) = claude_account("work");
        let session = uuid();
        let under = own.then_some(home.as_path());
        let path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
            fx.cwd.path(),
            &session,
            under,
        )
        .unwrap();
        append(&path, &[json!({"type": "mode", "sessionId": session})]);
        // As the return to tmux leaves it: the account the session last ran under.
        crate::services::tmux_common::write_tmux_session_auth_profile(fx.logical(), "work")
            .unwrap();
        let (_, nonce) = launch_tmux(&fx, Some(&session));
        let script = fx.launch_script();
        let resumed = script.contains(&format!("'--resume' '{session}'"));
        assert_eq!(resumed, own, "own home: {own}\n{script}");
        assert_eq!(script.contains("'--session-id'"), !own, "{script}");
        assert!(script.contains(home.to_str().unwrap()), "{script}");
        let context = execution_context("claude", &nonce).unwrap();
        assert_eq!(context.provider_root, Some(home.join("projects")));
        crate::services::tmux_common::cleanup_session_temp_files(fx.logical());
    }
}
