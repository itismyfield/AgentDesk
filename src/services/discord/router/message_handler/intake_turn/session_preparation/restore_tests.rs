use super::*;
#[tokio::test]
async fn preparation_keeps_existing_and_explicit_restore_veto_states() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    for (existing, fresh, cleared, reset, expected) in [
        (Some("existing"), false, false, false, "before"),
        (None, true, false, false, "before"),
        (None, false, true, false, "session_cleared_by_user"),
        (None, false, false, true, "dispatch_provider_reset"),
        (None, false, false, false, "session_key_unavailable"),
    ] {
        let mut session = existing.map(str::to_owned);
        let mut memento = true;
        let mut reason = "before";
        restore(
            &shared,
            &ProviderKind::Claude,
            ChannelId::new(727374),
            Restore {
                session_id: &mut session,
                memento_context_loaded: &mut memento,
                reason: &mut reason,
                force_fresh: fresh,
                cleared,
                reset,
                recreate: false,
                session_key: None,
                tmux: None,
            },
        )
        .await;
        assert_eq!(session.as_deref(), existing);
        assert_eq!(reason, expected);
        assert!(memento);
    }
}

#[tokio::test]
async fn preparation_restores_provider_session_and_memento_from_memory() {
    const CHILD: &str = "ADK_SESSION_RESTORE_SUCCESS_CHILD";
    let name = concat!(
        module_path!(),
        "::preparation_restores_provider_session_and_memento_from_memory"
    )
    .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
    .unwrap();
    if std::env::var(CHILD).as_deref() != Ok(name) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, name)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && text.contains("1 passed; 0 failed; 0 ignored;"),
            "restore child must run one test: {text} {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut connection, _) = listener.accept().unwrap();
        let mut request = [0u8; 4096];
        let received = connection.read(&mut request).unwrap();
        assert!(received > 0, "restore must issue an HTTP request");
        let body = r#"{"session_id":"restored-sid"}"#;
        write!(connection,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
    });
    crate::services::discord::internal_api::init(port, None);
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    let channel = ChannelId::new(727394);
    shared.core.lock().await.sessions.insert(
        channel,
        DiscordSession {
            session_id: Some("restored-sid".into()),
            memento_context_loaded: true,
            memento_reflected: true,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            channel_id: Some(channel.get()),
            channel_name: None,
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: 0,
        },
    );
    let mut session = None;
    let mut memento = false;
    let mut reason = "before";
    restore(
        &shared,
        &ProviderKind::Claude,
        channel,
        Restore {
            session_id: &mut session,
            memento_context_loaded: &mut memento,
            reason: &mut reason,
            force_fresh: false,
            cleared: false,
            reset: false,
            recreate: false,
            session_key: Some("restore-success"),
            tmux: None,
        },
    )
    .await;
    server.join().unwrap();
    assert_eq!(session.as_deref(), Some("restored-sid"));
    assert_eq!(reason, "db_provider_session_restored");
    assert!(
        memento,
        "successful restore must copy the restored memento state"
    );
    assert!(!shared.core.lock().await.sessions[&channel].memento_reflected);
}

// A turn that starts before the settle pass retires an in-pane /clear rotation, for example when
// the frozen turn's finish released the queue first, resumes the session the pane runs now.
#[cfg(unix)]
#[test]
fn preparation_follows_a_pane_rotation_its_settle_pass_has_not_retired() {
    use crate::services::tui_prompt_dedupe as dedupe;
    let _serial = dedupe::lock_claude_session_rotations_for_tests();
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (old, new) = ("af3ece1a-pending", "fd530b40-pending");
    let (channel, tmux) = (ChannelId::new(727395), "claude-rotation-pending");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, tmux).await;
        shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&channel)
            .unwrap()
            .session_id = Some(old.into());
        dedupe::record_claude_session_rotation(dedupe::ClaudeSessionRotation {
            tmux_session_name: tmux.to_string(),
            old_session_id: Some(old.to_string()),
            old_output_path: "/nonexistent/old.jsonl".to_string(),
            old_last_offset: 0,
            new_session_id: new.to_string(),
            new_output_path: "/nonexistent/new.jsonl".to_string(),
            observed_drain_frontier: 0,
            polls_without_drain_progress: 0,
            selector_followed: false,
        });
        let (mut session, mut memento, mut reason) = (Some(old.to_string()), true, "before");
        let state = Restore {
            session_id: &mut session,
            memento_context_loaded: &mut memento,
            reason: &mut reason,
            force_fresh: false,
            cleared: false,
            reset: false,
            recreate: false,
            session_key: None,
            tmux: Some(tmux),
        };
        restore(&shared, &ProviderKind::Claude, channel, state).await;
        assert_eq!(
            (session.as_deref(), reason),
            (Some(new), "claude_session_rotation_followed")
        );
        let cached = shared.core.lock().await.sessions[&channel]
            .session_id
            .clone();
        assert_eq!(cached.as_deref(), Some(new));
        assert!(dedupe::claude_session_rotation_for_tmux(tmux).is_some());
    });
}
