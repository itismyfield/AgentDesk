//! A Claude TUI fresh turn that fails to start, from the provider driver intake calls through
//! its own teardown and the bridge that settles the turn's mailbox episode.
use std::sync::Arc;
use std::sync::atomic::Ordering;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use crate::services::agent_protocol::{RuntimeHandoffKind, StreamMessage};
use crate::services::claude_tui::host_input::{SpyGuard, SpyState};
use crate::services::discord::inflight::{
    GuardedClearOutcome, InflightTurnIdentity, InflightTurnState,
};
use crate::services::discord::turn_bridge::{TurnBridgeContext, spawn_turn_bridge};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::provider_teardown::tests::test_support::{FakeTmux, cleared};
use crate::services::turn_orchestrator::{Intervention, InterventionMode};

const NAME: &str = "adk-t05-claude-fresh-fail";
const MESSAGE: u64 = 77_501;

/// A tmux before `FakeTmux` that launches a Claude TUI, answers the pre-paste read with `screen`,
/// records `sends`, and with `die_after_enter` kills the fake's pane after the submit Enter.
fn launchable_tmux() -> (tempfile::TempDir, Vec<crate::config::TestEnvVarGuard>) {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let fake = std::env::split_paths(&path).next().unwrap();
    let fake = fake.display();
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, body: String| {
        let file = dir.path().join(name);
        std::fs::write(&file, body).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        file
    };
    let own = dir.path().display().to_string();
    write(
        "tmux",
        format!(
            "#!/bin/sh\n[ \"$1\" = -u ] && shift\nd='{own}'\nif [ -f \"$d/screen\" ]; then \
             case \"$1\" in display-message) for a do last=\"$a\"; done; \
             [ \"$last\" = '#{{pane_id}}' ] && {{ echo %1; exit 0; }} ;; \
             capture-pane) cat \"$d/screen\"; exit 0 ;; load-buffer|delete-buffer) exit 0 ;; \
             paste-buffer|send-keys) echo \"$1\" >> \"$d/sends\"; \
             for a do last=\"$a\"; done; [ \"$1\" = send-keys ] && [ \"$last\" = Enter ] && \
             [ -f \"$d/die_after_enter\" ] && : > '{fake}/pane_dead'; \
             [ \"$1\" = send-keys ] || [ ! -f \"$d/paste_fails\" ]; exit $? ;; \
             esac; fi\nfor a do case \"$a\" in -V) echo 'tmux 3.5a'; exit 0 ;; \
             new-session) echo \"$*\" >> '{fake}/calls'; rm -f '{fake}/missing'; exit 0 ;; \
             esac; done\nexec '{fake}/tmux' \"$@\"\n"
        ),
    );
    let cli = write("claude", "#!/bin/sh\necho 2.1.289\n".to_string());
    let mut paths = vec![dir.path().to_path_buf()];
    paths.extend(std::env::split_paths(&path));
    let paths = std::env::join_paths(paths).unwrap();
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let env = vec![
        set("PATH", std::path::Path::new(&paths)),
        set("AGENTDESK_CLAUDE_PATH", &cli),
    ];
    (dir, env)
}

/// The ready, empty composer a fresh Claude TUI draws.
fn ready_screen() -> String {
    let rule = "─".repeat(80);
    format!(
        "Claude Code v2.1.289\n\n{rule}\n\u{276f} \n{rule}\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n"
    )
}

/// An uncertain first send, or a successful submit followed by a confirmed dead pane.
fn fail_fresh_turn(
    channel: u64,
    cancel: Arc<CancelToken>,
    sender: std::sync::mpsc::Sender<StreamMessage>,
    workspace: String,
    uncertain: bool,
) -> Result<(), String> {
    let ready = ready_screen();
    if !uncertain {
        // The pane dies only after the submit Enter, so the pre-paste read still sees it live.
        let launch = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .next()
            .unwrap();
        std::fs::write(launch.join("die_after_enter"), "").unwrap();
    }
    // The driver registers this session in the prompt-dedupe tables the dedupe tests own.
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let _spy = SpyGuard::install(SpyState {
        captures: std::iter::repeat_n(Some(ready), 64).collect(),
        ..SpyState::default()
    });
    let clearance = cleared(NAME);
    crate::services::claude::execute_command_streaming(
        "hello",
        None,
        &workspace,
        sender,
        Some(""),
        Some(&[]),
        Some(cancel),
        None,
        Some(NAME),
        Some(&clearance),
        Some(channel),
        Some(ProviderKind::Claude),
        None,
        None,
        None,
        0,
        None,
        None,
    )
}

fn queued(message_id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(message_id),
        queued_generation: crate::services::discord::runtime_store::load_generation(),
        source_message_ids: vec![MessageId::new(message_id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("queued behind {MESSAGE}"),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

fn bridge_context(row: InflightTurnState) -> TurnBridgeContext {
    TurnBridgeContext {
        provider: ProviderKind::Claude,
        gateway: Arc::new(crate::services::discord::gateway::HeadlessGateway),
        channel_id: ChannelId::new(row.channel_id),
        user_msg_id: Some(MessageId::new(row.user_msg_id)),
        user_text_owned: "hello".to_string(),
        request_owner_name: String::new(),
        role_binding: None,
        adk_session_key: None,
        adk_session_name: None,
        adk_session_info: None,
        adk_cwd: None,
        dispatch_id: None,
        dispatch_kind: None,
        memory_recall_usage: Default::default(),
        context_window_tokens: 0,
        context_compact_percent: 0,
        current_msg_id: Some(MessageId::new(18)),
        response_sent_offset: 0,
        full_response: String::new(),
        tmux_last_offset: None,
        new_session_id: None,
        defer_watcher_resume: false,
        reuse_status_panel_message: false,
        completion_tx: None,
        is_external_input_tui_direct: false,
        inflight_state: row,
    }
}

/// Drops the cached tmux availability on entry and exit, so this fixture's answer neither
/// comes from nor outlives another test's tmux.
struct FreshTmuxProbe;

impl FreshTmuxProbe {
    fn new() -> Self {
        crate::services::platform::tmux::invalidate_availability_cache();
        Self
    }
}

impl Drop for FreshTmuxProbe {
    fn drop(&mut self) {
        crate::services::platform::tmux::invalidate_availability_cache();
    }
}

/// The hook endpoint a Claude TUI launch needs, published under the lock the endpoint tests
/// share; the endpoint is withdrawn before the lock is released.
struct PublishedEndpoint {
    _endpoint: crate::services::claude_tui::hook_server::HookEndpointGuard,
    _lock: std::sync::MutexGuard<'static, ()>,
}

/// Needs the shared env lock already held, the order the other endpoint users take.
fn published_endpoint() -> PublishedEndpoint {
    let lock = crate::services::claude_tui::hook_server::tests::ENDPOINT_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let endpoint = crate::services::claude_tui::hook_server::publish_hook_endpoint(
        "http://127.0.0.1:9".to_string(),
    );
    PublishedEndpoint {
        _endpoint: endpoint,
        _lock: lock,
    }
}

struct Outcome {
    holder: Option<Arc<CancelToken>>,
    active_message: Option<MessageId>,
    queued: usize,
    global_active: usize,
    calls: Vec<String>,
    exit_reason: String,
    /// The durable row's bytes when a successor saved it and after the bridge exited.
    successor_row: Option<(Vec<u8>, Option<Vec<u8>>)>,
    held: bool,
    retained_input: Option<String>,
}

/// How far a successor got between A's teardown and A's bridge settlement.
#[derive(Clone, Copy, Debug)]
enum Successor {
    /// B holds the mailbox but has not saved its row yet, so A's row is still on disk.
    Admitted,
    /// B also saved its own row for the same message.
    RowSaved,
    /// B saved its row within A's start second: the rows share every identity field and only
    /// the turn nonce tells B's episode from A's.
    RowSavedSameSecond,
}

/// Runs episode A's fresh turn to its failure and its bridge to its exit; a `successor` takes
/// the channel after the provider returned and before the bridge settles.
async fn run_case(
    channel_raw: u64,
    successor: Option<(Arc<CancelToken>, Successor)>,
    uncertain: bool,
) -> (Arc<CancelToken>, Outcome) {
    let tmux = FakeTmux::install(NAME);
    tmux.mark_missing();
    let (launch, _launch_env) = launchable_tmux();
    // The A-plan reads the pane once before its paste, so this fixture answers that read.
    std::fs::write(launch.path().join("screen"), ready_screen()).unwrap();
    if uncertain {
        std::fs::write(launch.path().join("paste_fails"), "").unwrap();
    }
    let _probe = FreshTmuxProbe::new();
    let workspace = tempfile::tempdir().unwrap();

    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Claude;
    let channel = ChannelId::new(channel_raw);
    let a = Arc::new(CancelToken::new());
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        channel_raw,
        None,
        1,
        MESSAGE,
        18,
        "hello".to_string(),
        None,
        Some(NAME.to_string()),
        None,
        None,
        0,
    );
    row.turn_nonce = a.turn_nonce().map(str::to_owned);
    // Intake stamps the prelaunch kind of a Claude TUI turn before saving its row.
    row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    let capability = uncertain.then(|| {
        crate::services::discord::inflight::install_managed_submission_boundary(
            &mut row,
            &crate::services::turn_host::TurnHost::Tmux,
        )
        .unwrap()
    });
    let message = MessageId::new(MESSAGE);
    let started = crate::services::discord::mailbox_try_start_turn(
        &shared,
        channel,
        a.clone(),
        UserId::new(1),
        message,
    );
    assert!(started.await);
    // Another channel's turn keeps the counter off zero, so a double decrement shows.
    crate::services::discord::increment_global_active(&shared, "other_channel_turn");
    crate::services::discord::increment_global_active(&shared, "test_bridge_admission");
    let provider = ProviderKind::Claude;
    crate::services::discord::mailbox_enqueue_intervention(
        &shared,
        &provider,
        channel,
        queued(77_601),
    )
    .await;
    crate::services::discord::inflight::save_inflight_state(&row).unwrap();
    let saved = crate::services::discord::inflight::load_inflight_state(&provider, channel_raw);
    assert_eq!(
        saved.and_then(|saved| saved.runtime_kind),
        Some(RuntimeHandoffKind::ClaudeTui)
    );
    let row_path = crate::services::discord::inflight::inflight_state_path(
        &crate::services::discord::inflight::inflight_runtime_root().unwrap(),
        &provider,
        channel_raw,
    );
    let started_at = row.started_at.clone();
    let (a_identity, a_nonce) = (
        InflightTurnIdentity::from_state(&row),
        row.turn_nonce.clone(),
    );
    let mut bridge = bridge_context(row);
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    bridge.completion_tx = Some(completion_tx);
    let (tx, rx) = std::sync::mpsc::channel();
    spawn_turn_bridge(shared.clone(), a.clone(), rx, bridge);

    let (actor, sender, dir) = (
        a.clone(),
        tx.clone(),
        workspace.path().display().to_string(),
    );
    let observer = capability.clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::services::claude_tui::guarded_outcome::with_owner(
            observer.as_ref().map(|cap| cap.observer()),
            || {
                crate::services::claude_tui::submission_fence::with_scope(
                    observer.as_ref().map(|cap| cap.fence()),
                    || fail_fresh_turn(channel_raw, actor, sender, dir, uncertain),
                )
            },
        )
    })
    .await
    .unwrap();
    let error = result.expect_err("the fresh turn fails to start");
    // A write that failed after the fence stays a typed guarded hold, never a requeue.
    let failure = capability.as_ref().map(|cap| cap.classify_failure());
    let held = matches!(
        failure,
        Some(
            crate::services::discord::inflight::SubmissionFailure::GuardedStopped(
                crate::services::claude_tui::host_input::InputRun::Indeterminate { .. }
            )
        )
    );
    if uncertain {
        assert!(held, "send ACK remains a typed uncertain hold: {error}");
        let sends = std::fs::read_to_string(launch.path().join("sends")).unwrap_or_default();
        assert_eq!(
            sends, "paste-buffer\n",
            "the managed paste failed after the fence"
        );
        let retained = crate::services::discord::inflight::load_inflight_state_read_only(
            &provider,
            channel_raw,
        )
        .unwrap();
        assert_eq!(
            retained.user_text, "hello",
            "uncertain payload retains its original durable custody"
        );
        assert_eq!(retained.turn_nonce, a_nonce);
    } else {
        let sends = std::fs::read_to_string(launch.path().join("sends")).unwrap_or_default();
        assert_eq!(
            sends, "paste-buffer\nsend-keys\n",
            "one paste and one Enter reach the pane before it dies: {error}"
        );
    }
    let calls = tmux.take_calls();
    let reason_path = crate::services::tmux_common::session_temp_path(NAME, "exit_reason");
    let exit_reason = std::fs::read_to_string(reason_path).unwrap_or_default();

    let mut successor_saved = None;
    if let Some((next, stage)) = &successor {
        crate::services::discord::mailbox_finish_turn(&shared, &provider, channel).await;
        crate::services::discord::saturating_decrement_global_active(&shared);
        let started = crate::services::discord::mailbox_try_start_turn(
            &shared,
            channel,
            next.clone(),
            UserId::new(1),
            message,
        );
        assert!(started.await);
        crate::services::discord::increment_global_active(&shared, "successor_admission");
        if matches!(stage, Successor::RowSaved | Successor::RowSavedSameSecond) {
            let same_second = matches!(stage, Successor::RowSavedSameSecond);
            // A successor starts after A's launch failed, so its row's start second differs.
            let second = || chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !same_second && second() == started_at && std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            let mut successor_row = InflightTurnState::new(
                ProviderKind::Claude,
                channel_raw,
                None,
                1,
                MESSAGE,
                if same_second { 18 } else { 19 },
                "hello".to_string(),
                None,
                Some(NAME.to_string()),
                None,
                None,
                0,
            );
            successor_row.turn_nonce = next.turn_nonce().map(str::to_owned);
            successor_row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
            if same_second {
                successor_row.started_at.clone_from(&started_at);
                assert_eq!(InflightTurnIdentity::from_state(&successor_row), a_identity);
            } else {
                assert_ne!(successor_row.started_at, started_at);
            }
            // The claim that frees A's mailbox clears A's row; intake then creates B's row anew.
            let cleared =
                crate::services::discord::inflight::clear_inflight_state_for_captured_episode(
                    &provider,
                    channel_raw,
                    &a_identity,
                    a_nonce.as_deref(),
                );
            assert_eq!(cleared, GuardedClearOutcome::Cleared);
            crate::services::discord::inflight::save_inflight_state_create_new(&successor_row)
                .unwrap();
            successor_saved = Some(std::fs::read(&row_path).unwrap());
        }
    }
    let frame =
        crate::services::discord::router::message_handler::producer_error_frame(channel_raw, error);
    if !uncertain {
        tx.send(frame.expect("a confirmed hard failure reaches the bridge"))
            .unwrap();
    }
    drop(tx);
    tokio::time::timeout(std::time::Duration::from_secs(20), completion_rx)
        .await
        .expect("the bridge reports")
        .unwrap();
    // The bridge reports before its finalizer settles the mailbox.
    let settle = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut snapshot = crate::services::discord::mailbox_snapshot(&shared, channel).await;
    while snapshot
        .cancel_token
        .as_ref()
        .is_some_and(|token| Arc::ptr_eq(token, &a))
        && std::time::Instant::now() < settle
    {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        snapshot = crate::services::discord::mailbox_snapshot(&shared, channel).await;
    }
    // The bridge task holds A's token until its last cleanup ran; a mailbox still holding A
    // keeps one more reference, which the token assertions then report.
    let mailbox_holds_a = snapshot
        .cancel_token
        .take()
        .is_some_and(|token| Arc::ptr_eq(&token, &a));
    let exited = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while Arc::strong_count(&a) > 1 + usize::from(mailbox_holds_a)
        && std::time::Instant::now() < exited
    {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        Arc::strong_count(&a) <= 1 + usize::from(mailbox_holds_a),
        "the bridge task exits before the durable row is compared"
    );
    let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel).await;
    let outcome = Outcome {
        holder: snapshot.cancel_token.clone(),
        active_message: snapshot.active_user_message_id,
        queued: snapshot.intervention_queue.len(),
        global_active: shared.restart.global_active.load(Ordering::Relaxed),
        calls,
        exit_reason,
        successor_row: successor_saved.map(|saved| (saved, std::fs::read(&row_path).ok())),
        held,
        retained_input: crate::services::discord::inflight::load_inflight_state_read_only(
            &provider,
            channel_raw,
        )
        .map(|row| row.user_text),
    };
    crate::services::tmux_common::cleanup_session_temp_files(NAME);
    (a, outcome)
}

fn assert_torn_down(outcome: &Outcome) {
    let called = |command: &str| outcome.calls.iter().any(|call| call.starts_with(command));
    assert!(called("new-session"), "{:?}", outcome.calls);
    assert!(
        called("list-panes"),
        "confirmed dead pane: {:?}",
        outcome.calls
    );
    assert!(called("kill-session"), "kill: {:?}", outcome.calls);
    assert!(
        outcome
            .exit_reason
            .contains("claude tui session died before turn completion")
            && !outcome.held,
        "{}",
        outcome.exit_reason
    );
}

/// A fresh turn that fails to start is torn down and its bridge returns the channel to idle:
/// exactly its own token and one active slot go, and the queued input stays.
#[tokio::test(flavor = "current_thread")]
async fn a_failed_fresh_start_releases_its_mailbox_episode_and_keeps_the_queue() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let _endpoint = published_endpoint();
    let (a, outcome) = run_case(6_333_511, None, false).await;
    assert_torn_down(&outcome);
    assert!(outcome.holder.is_none(), "A's token is released");
    assert_eq!(outcome.active_message, None);
    assert!(a.cancelled.load(Ordering::Relaxed));
    assert_eq!(
        outcome.global_active, 1,
        "exactly A's active slot is released"
    );
    assert_eq!(outcome.queued, 1, "the queued input stays");
}

/// A successor that took the channel between the failed start's teardown and its bridge's
/// settlement keeps its token, message and active slot, and, once saved, its durable row.
#[tokio::test(flavor = "current_thread")]
async fn a_failed_fresh_start_leaves_a_successor_episode_untouched() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let _endpoint = published_endpoint();
    for (channel, stage) in [
        (6_333_512, Successor::Admitted),
        (6_333_513, Successor::RowSaved),
    ] {
        let b = Arc::new(CancelToken::new());
        let (_, outcome) = run_case(channel, Some((b.clone(), stage)), false).await;
        assert_torn_down(&outcome);
        assert_successor_untouched(stage, &b, outcome);
    }
}

/// B's row saved in A's start second carries A's exact identity; A's failed bridge must still
/// leave B's token, slot, queue and row bytes alone, told apart by the nonce alone.
#[tokio::test(flavor = "current_thread")]
async fn same_second_fresh_failure_keeps_b_episode_7219() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let _endpoint = published_endpoint();
    let b = Arc::new(CancelToken::new());
    let stage = Successor::RowSavedSameSecond;
    let (_, outcome) = run_case(6_333_514, Some((b.clone(), stage)), false).await;
    assert_torn_down(&outcome);
    assert_successor_untouched(stage, &b, outcome);
}

fn assert_successor_untouched(stage: Successor, b: &Arc<CancelToken>, outcome: Outcome) {
    assert!(
        outcome
            .holder
            .as_ref()
            .is_some_and(|token| Arc::ptr_eq(token, b)),
        "{stage:?}: B keeps the channel: holder {:?}, message {:?}",
        outcome.holder.as_ref().map(|token| token.turn_nonce()),
        outcome.active_message
    );
    assert_eq!(
        outcome.active_message,
        Some(MessageId::new(MESSAGE)),
        "{stage:?}"
    );
    assert!(!b.cancelled.load(Ordering::Relaxed), "{stage:?}");
    assert_eq!(
        outcome.global_active, 2,
        "{stage:?}: B's and the other channel's slots stay"
    );
    assert_eq!(outcome.queued, 1, "{stage:?}");
    match (stage, outcome.successor_row) {
        (Successor::RowSaved | Successor::RowSavedSameSecond, Some((saved, after))) => {
            assert_eq!(after.as_ref(), Some(&saved), "B's durable row is untouched");
        }
        (Successor::Admitted, None) => {}
        (stage, row) => panic!("{stage:?}: successor row saved = {}", row.is_some()),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn bundle_uncertain_fresh_send_preserves_payload_pane_and_successor() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let _endpoint = published_endpoint();
    for (channel, stage) in [
        (6_333_515, None),
        (6_333_516, Some(Successor::RowSavedSameSecond)),
    ] {
        let b = Arc::new(CancelToken::new());
        let (_, outcome) = run_case(channel, stage.map(|s| (b.clone(), s)), true).await;
        assert!(
            outcome.held,
            "an uncertain send never becomes a legacy hard failure"
        );
        assert_eq!(
            outcome
                .calls
                .iter()
                .filter(|call| call.starts_with("kill-session"))
                .count(),
            0,
            "uncertain payload custody never authorizes kill: {:?}",
            outcome.calls
        );
        assert_eq!(outcome.queued, 1, "the original queued input stays");
        if let Some(stage) = stage {
            assert_successor_untouched(stage, &b, outcome);
        } else {
            assert_eq!(
                outcome.retained_input.as_deref(),
                Some("hello"),
                "uncertain payload custody survives bridge cleanup"
            );
        }
    }
}
