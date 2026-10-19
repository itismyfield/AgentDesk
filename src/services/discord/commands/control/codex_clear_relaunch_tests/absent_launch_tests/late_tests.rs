//! Late provider checks reject changed ownership before cleanup or session creation.

use super::super::CanaryFixture;
use super::auxiliary_tests::{
    instruction, kickoff, recorder, restore_instructions, target_missing, wait_idle,
};
use super::*;

#[derive(Clone, Copy, Debug)]
enum LateChange {
    TurnNonce,
    Cancellation,
    SpawnNonce,
    Pane,
}

#[derive(Clone, Copy, Debug)]
enum Boundary {
    BeforeCleanup,
    BeforeCreate,
}

#[tokio::test(flavor = "current_thread")]
async fn late_row_nonce_change_holds_before_cleanup_pg() {
    if isolated(concat!(
        module_path!(),
        "::late_row_nonce_change_holds_before_cleanup_pg"
    )) {
        late_change_holds(LateChange::TurnNonce, Boundary::BeforeCleanup).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn late_cancellation_holds_before_cleanup_pg() {
    if isolated(concat!(
        module_path!(),
        "::late_cancellation_holds_before_cleanup_pg"
    )) {
        late_change_holds(LateChange::Cancellation, Boundary::BeforeCleanup).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn late_foreign_spawn_nonce_holds_before_cleanup_pg() {
    if isolated(concat!(
        module_path!(),
        "::late_foreign_spawn_nonce_holds_before_cleanup_pg"
    )) {
        late_change_holds(LateChange::SpawnNonce, Boundary::BeforeCleanup).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn late_pane_appearance_holds_before_cleanup_pg() {
    if isolated(concat!(
        module_path!(),
        "::late_pane_appearance_holds_before_cleanup_pg"
    )) {
        late_change_holds(LateChange::Pane, Boundary::BeforeCleanup).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn late_row_nonce_change_holds_before_create_pg() {
    if isolated(concat!(
        module_path!(),
        "::late_row_nonce_change_holds_before_create_pg"
    )) {
        late_change_holds(LateChange::TurnNonce, Boundary::BeforeCreate).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn late_foreign_spawn_nonce_holds_before_create_pg() {
    if isolated(concat!(
        module_path!(),
        "::late_foreign_spawn_nonce_holds_before_create_pg"
    )) {
        late_change_holds(LateChange::SpawnNonce, Boundary::BeforeCreate).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn initial_probe_failure_cannot_become_absent_launch_pg() {
    if !isolated(concat!(
        module_path!(),
        "::initial_probe_failure_cannot_become_absent_launch_pg"
    )) {
        return;
    }
    let mut fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some("b1-absent-test-host"))
            .await;
    let api = recorder(&mut fixture).await;
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "http://127.0.0.1:{}",
        api.port
    ));
    let _cli = fixture.cold_cli();
    let _warm = fixture.warm_followup(false);
    target_missing(&fixture, false);
    crate::services::platform::tmux::invalidate_availability_cache();
    restore_instructions(
        &mut fixture,
        vec![instruction(
            CANARY_CHANNEL + 251,
            "fix the initial launch reason",
        )],
    )
    .await;
    let mut evidence = old_evidence(&fixture);
    for extension in [
        "spawn_nonce",
        crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
    ] {
        let path = PathBuf::from(crate::services::tmux_common::session_temp_path(
            CANARY_TMUX,
            extension,
        ));
        evidence.push((path.clone(), fs::read(path).unwrap()));
    }
    let (setup_tx, setup_rx) = mpsc::channel();
    let (start_tx, start_rx) = mpsc::channel();
    let mut start = ReleaseEffect(Some(start_tx));
    let (captured_tx, captured_rx) = mpsc::channel();
    let (capture_tx, capture_rx) = mpsc::channel();
    let mut capture = ReleaseEffect(Some(capture_tx));
    let (escaped_tx, escaped_rx) = mpsc::channel();
    let (effect_tx, effect_rx) = mpsc::channel();
    let mut effect = ReleaseEffect(Some(effect_tx));
    let _setup = crate::services::discord::codex_absence::test_setup::install(
        CANARY_CHANNEL,
        Box::new(move |_| {
            crate::services::codex::actual_submit_tests::after_initial_absence_capture(Box::new(
                move |captured| {
                    captured_tx.send(captured).unwrap();
                    capture_rx
                        .recv_timeout(Duration::from_secs(30))
                        .expect("release the initial capture boundary");
                },
            ));
            crate::services::codex::actual_submit_tests::before_create_effect(Box::new(
                move || {
                    escaped_tx.send(()).unwrap();
                    effect_rx
                        .recv_timeout(Duration::from_secs(30))
                        .expect("release an incorrectly admitted launch");
                },
            ));
            setup_tx.send(()).unwrap();
            start_rx
                .recv_timeout(Duration::from_secs(30))
                .expect("release the actual provider entry");
            Box::new(())
        }),
    );
    let (hold_tx, hold_rx) = mpsc::channel();
    *crate::services::codex_tui::verified_hold::HOLD_ENTERED_FOR_TESTS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(hold_tx);
    let _hold_reset = HoldHookReset;
    fixture.fake.take_calls();
    let _ = crate::services::codex::actual_submit_tests::take_dead_temp_cleanup_trace(CANARY_TMUX);
    assert!(kickoff(&fixture, &api).await);
    let guard = fixture.turn_failure_guard().await;
    receive(&setup_rx, "actual provider setup").await;
    fixture.fake.fail_probes();
    assert_eq!(
        crate::services::platform::tmux::session_presence(CANARY_TMUX),
        crate::services::platform::tmux::SessionPresence::ProbeFailed
    );
    start.release();
    assert!(
        !receive(&captured_rx, "initial failed absence capture").await,
        "a failed presence probe must not capture an absence permission"
    );
    fixture.fake.restore_probes();
    assert_eq!(
        crate::services::platform::tmux::session_presence(CANARY_TMUX),
        crate::services::platform::tmux::SessionPresence::Missing
    );
    capture.release();
    let preserved = tokio::select! {
        tmux = receive(&hold_rx, "the preserving provider hold") => {
            assert_eq!(tmux, CANARY_TMUX);
            true
        }
        () = receive(&escaped_rx, "an incorrectly admitted create boundary") => false,
    };
    guard.token.cancelled.store(true, Ordering::SeqCst);
    effect.release();
    wait_idle(&fixture).await;
    assert!(
        preserved,
        "a later Missing probe replaced the initial failed launch reason"
    );
    let cleanup =
        crate::services::codex::actual_submit_tests::take_dead_temp_cleanup_trace(CANARY_TMUX);
    assert!(
        cleanup.is_empty(),
        "initial rejection performed cleanup: {cleanup:?}"
    );
    let calls = fixture.fake.take_calls();
    assert!(
        calls.iter().all(|call| {
            !["new-session", "kill-session", "send-keys", "paste-buffer"]
                .iter()
                .any(|effect| call.starts_with(effect))
        }),
        "initial rejection performed pane effects: {calls:?}"
    );
    assert!(!fixture.root.path().join("fresh-context").exists());
    assert!(!fixture.root.path().join("fresh-argv.json").exists());
    for (path, bytes) in evidence {
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "changed {}",
            path.display()
        );
    }
    drop(guard);
}

async fn receive<T>(receiver: &mpsc::Receiver<T>, what: &str) -> T {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match receiver.try_recv() {
                Ok(value) => return value,
                Err(mpsc::TryRecvError::Empty) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(mpsc::TryRecvError::Disconnected) => panic!("{what} disconnected"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

struct ReleaseEffect(Option<mpsc::Sender<()>>);

impl ReleaseEffect {
    fn release(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

impl Drop for ReleaseEffect {
    fn drop(&mut self) {
        self.release();
    }
}

fn old_evidence(fixture: &CanaryFixture) -> Vec<(PathBuf, Vec<u8>)> {
    let runtime = PathBuf::from(&fixture.context.owner_runtime_root).join("runtime");
    [
        fixture.history(),
        fixture
            .context
            .provider_root
            .as_ref()
            .unwrap()
            .join(format!("rollout-{NATIVE_ID}.jsonl")),
        runtime
            .join("binding_contexts/codex")
            .join(format!("{}.json", fixture.context.execution_nonce)),
        runtime
            .join("binding_evidence/codex")
            .join(format!("{}.json", fixture.context.execution_nonce)),
    ]
    .into_iter()
    .map(|path| {
        let bytes = fs::read(&path).unwrap();
        (path, bytes)
    })
    .collect()
}

async fn late_change_holds(change: LateChange, boundary: Boundary) {
    let mut fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some("b1-absent-test-host"))
            .await;
    let api = recorder(&mut fixture).await;
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "http://127.0.0.1:{}",
        api.port
    ));
    let _cli = fixture.cold_cli();
    target_missing(&fixture, false);
    restore_instructions(
        &mut fixture,
        vec![instruction(
            CANARY_CHANNEL + 241,
            "B1 actual provider late absence recheck",
        )],
    )
    .await;
    let evidence = old_evidence(&fixture);
    let spawn_path = PathBuf::from(crate::services::tmux_common::session_temp_path(
        CANARY_TMUX,
        "spawn_nonce",
    ));
    let ownership_path = PathBuf::from(crate::services::tmux_common::session_temp_path(
        CANARY_TMUX,
        crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
    ));
    let old_spawn = fs::read(&spawn_path).unwrap();
    let old_ownership = fs::read(&ownership_path).unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let mut release = ReleaseEffect(Some(release_tx));
    let _setup = crate::services::discord::codex_absence::test_setup::install(
        CANARY_CHANNEL,
        Box::new(move |_| {
            let effect = Box::new(move || {
                entered_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(Duration::from_secs(30))
                    .expect("the async test must release the exact late boundary");
            });
            match boundary {
                Boundary::BeforeCleanup => {
                    crate::services::codex::actual_submit_tests::before_pane_effect(effect);
                }
                Boundary::BeforeCreate => {
                    crate::services::codex::actual_submit_tests::before_create_effect(effect);
                }
            }
            Box::new(())
        }),
    );
    let (hold_tx, hold_rx) = mpsc::channel();
    *crate::services::codex_tui::verified_hold::HOLD_ENTERED_FOR_TESTS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(hold_tx);
    let _hold_reset = HoldHookReset;
    fixture.fake.take_calls();
    let _ = crate::services::codex::actual_submit_tests::take_dead_temp_cleanup_trace(CANARY_TMUX);
    assert!(kickoff(&fixture, &api).await);
    let guard = fixture.turn_failure_guard().await;
    tokio::select! {
        () = receive(&entered_rx, "the selected provider boundary") => {}
        held = receive(&hold_rx, "early provider hold") => {
            panic!("provider held at {held} before exercising {boundary:?}");
        }
    }
    let foreign_nonce = uuid::Uuid::new_v4().simple().to_string();
    let spawn_before_change = fs::read(&spawn_path).ok();
    match change {
        LateChange::TurnNonce => {
            let root = crate::services::discord::inflight::inflight_runtime_root().unwrap();
            let path = crate::services::discord::inflight::inflight_state_path(
                &root,
                &ProviderKind::Codex,
                CANARY_CHANNEL,
            );
            let mut row: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(row["turn_nonce"], json!(guard.token.turn_nonce()));
            row["turn_nonce"] = json!(foreign_nonce);
            fs::write(path, serde_json::to_vec(&row).unwrap()).unwrap();
        }
        LateChange::Cancellation => guard.token.cancelled.store(true, Ordering::SeqCst),
        LateChange::SpawnNonce => fs::write(&spawn_path, &foreign_nonce).unwrap(),
        LateChange::Pane => fixture.fake.mark_present(),
    }
    release.release();
    let mut calls = Vec::new();
    let held = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            calls.extend(fixture.fake.take_calls());
            assert!(
                calls.iter().all(|call| {
                    !["new-session", "kill-session", "send-keys", "paste-buffer"]
                        .iter()
                        .any(|effect| call.starts_with(effect))
                }),
                "{change:?}/{boundary:?}: effect before preserving hold: {calls:?}"
            );
            if let Ok(held) = hold_rx.try_recv() {
                break held;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("late provider must acknowledge its preserving hold");
    assert_eq!(held, CANARY_TMUX);
    guard.token.cancelled.store(true, Ordering::SeqCst);
    wait_idle(&fixture).await;
    calls.extend(fixture.fake.take_calls());
    assert!(
        calls.iter().all(|call| {
            !["new-session", "kill-session", "send-keys", "paste-buffer"]
                .iter()
                .any(|effect| call.starts_with(effect))
        }),
        "{change:?}/{boundary:?}: late refusal produced a pane effect: {calls:?}"
    );
    let cleanup =
        crate::services::codex::actual_submit_tests::take_dead_temp_cleanup_trace(CANARY_TMUX);
    match boundary {
        Boundary::BeforeCleanup => {
            assert!(cleanup.is_empty(), "{cleanup:?}");
            assert_eq!(fs::read(&ownership_path).unwrap(), old_ownership);
            if !matches!(change, LateChange::SpawnNonce) {
                assert_eq!(fs::read(&spawn_path).unwrap(), old_spawn);
            }
        }
        Boundary::BeforeCreate => {
            assert_eq!(cleanup, ["session_temp", "codex_session_files"]);
            assert!(
                spawn_before_change.is_none(),
                "cold cleanup removes the old current marker"
            );
            if !matches!(change, LateChange::SpawnNonce) {
                assert_eq!(fs::read(&spawn_path).ok(), spawn_before_change);
            }
        }
    }
    if matches!(change, LateChange::SpawnNonce) {
        assert_eq!(fs::read_to_string(&spawn_path).unwrap(), foreign_nonce);
    }
    for (path, bytes) in evidence {
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "changed {}",
            path.display()
        );
    }
    assert!(!fixture.root.path().join("fresh-context").exists());
    // Intentional cancellation leaves completion assertions armed on this failure guard.
    drop(guard);
}
