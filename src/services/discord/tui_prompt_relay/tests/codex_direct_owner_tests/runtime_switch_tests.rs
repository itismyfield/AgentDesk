//! A Codex runtime replaced in one process: the one idle relay delivers through the new
//! runtime's routing to channels only it owns, and an answer already being relayed is sent once.
use super::*;

const A_PARKED: (u64, &str) = (7_632_520_301, "AgentDesk-codex-6325-a-parked");
const B_HOOK: (u64, &str) = (7_632_520_401, "AgentDesk-codex-6325-b-hook");
const B_ROLLOUT: (u64, &str) = (7_632_520_402, "AgentDesk-codex-6325-b-rollout");

/// What one B channel received: each accepted request as `(method, path, content)`, and where
/// its binding cursor ended.
type Output = (Vec<(String, String, String)>, u64);

fn content(body: &str) -> String {
    let value = serde_json::from_str::<serde_json::Value>(body).unwrap_or_default();
    value["content"].as_str().unwrap_or_default().to_string()
}

fn output(requests: &Requests, codex: &CodexChannel) -> Output {
    let sent = deliveries(requests, codex.channel)
        .into_iter()
        .map(|(method, path, body)| (method, path, content(&body)))
        .collect();
    let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&codex.tmux);
    (sent, binding.expect("binding").last_offset)
}

/// A channel with a live supervisor producer, as production leaves it after a Discord turn.
fn channel(
    shared: &Arc<SharedData>,
    root: &Path,
    (id, tmux): (u64, &str),
    frames: &Arc<AtomicUsize>,
    relays: &mut Vec<crate::services::cluster::stream_relay::StreamRelayHandle>,
) -> CodexChannel {
    // Input-fence gates outlive their tests and gate a channel for every provider.
    let gate = crate::services::discord::input_runtime::fence::channel_gate(id);
    assert!(
        gate.is_none(),
        "channel {id} is shared with an input-fence test"
    );
    let codex = CodexChannel::new(shared, root, id, tmux);
    let relay = crate::services::cluster::stream_relay::spawn_stream_relay(
        crate::services::cluster::session_matcher::MatchedChannel {
            channel_id: id.to_string(),
            agent_id: "agent-6325".to_string(),
            provider: ProviderKind::Codex,
            expected_session_name: tmux.to_string(),
            expected_rollout_path: codex.rollout.to_str().unwrap().to_string(),
        },
        Arc::new(CountingSink(frames.clone())),
    );
    crate::services::cluster::relay_producer_registry::global_relay_producer_registry()
        .register(tmux.to_string(), relay.producer());
    relays.push(relay);
    codex
}

/// The hook records the prompt, the loop scans past it, the observer claims, and the answer
/// lands: the claimed row's repair tail delivers it.
async fn hook_first(shared: &Arc<SharedData>, codex: &CodexChannel, anchor: u64) {
    crate::services::tui_prompt_dedupe::observe_hook_prompt_by_tmux_with_prompt_id_at(
        "codex",
        &codex.tmux,
        PROMPT,
        None,
        chrono::Utc::now(),
    );
    append(&codex.rollout, &user_line());
    let prompt_end = std::fs::metadata(&codex.rollout).unwrap().len();
    assert!(
        wait_for(Duration::from_secs(10), || {
            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&codex.tmux)
                .is_some_and(|binding| binding.last_offset == prompt_end)
        })
        .await,
        "the idle loop never scanned past {}'s prompt (cursor {:?}, prompt end {prompt_end})",
        codex.tmux,
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&codex.tmux)
            .map(|binding| binding.last_offset)
    );
    let lease = codex.lease(shared);
    assert!(codex.claim(shared, anchor, &lease).await.claimed);
    append(&codex.rollout, &answer_lines());
}

/// The loop publishes the rollout prompt, the observer claims it, and the loop's own tail
/// delivers the answer.
async fn rollout_first(shared: &Arc<SharedData>, codex: &CodexChannel, anchor: u64) {
    append(&codex.rollout, &user_line());
    let channel = codex.channel.get();
    assert!(
        wait_for(Duration::from_secs(10), || {
            crate::services::tui_prompt_dedupe::external_input_relay_lease_present(
                "codex",
                &codex.tmux,
                channel,
            )
        })
        .await,
        "the idle loop never published {}'s prompt",
        codex.tmux
    );
    let lease = crate::services::tui_prompt_dedupe::external_input_relay_lease(
        "codex",
        &codex.tmux,
        channel,
    )
    .expect("published lease");
    assert!(codex.claim(shared, anchor, &lease).await.claimed);
    append(&codex.rollout, &answer_lines());
}

async fn delivered(requests: &Requests, codex: &CodexChannel) {
    assert!(
        wait_for(Duration::from_secs(20), || !deliveries(
            requests,
            codex.channel
        )
        .is_empty())
        .await,
        "{} answer never reached Discord: {:?}",
        codex.tmux,
        requests.lock().unwrap()
    );
}

/// One final answer edit on the channel's claimed anchor, from one tail.
fn delivered_once(requests: &Requests, codex: &CodexChannel, anchor: u64) {
    let sent = deliveries(requests, codex.channel);
    let finals = sent.iter().filter(|(_, _, body)| content(body) == RESPONSE);
    assert_eq!(
        finals.count(),
        1,
        "one answer edit on {}: {sent:?}",
        codex.tmux
    );
    let anchor = format!("/channels/{}/messages/{anchor}", codex.channel.get());
    for (method, path, _) in &sent {
        assert!(method == "PATCH" && path.ends_with(&anchor), "{sent:?}");
    }
    assert_eq!(tail_starts(codex.channel), 1, "one tail for {}", codex.tmux);
}

/// Runs B's two direct turns, after a replaced runtime A when `replaced`, else with B alone.
async fn run(root: &Path, replaced: bool) -> (Output, Output) {
    enable_session_bound_delivery();
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let frames = Arc::new(AtomicUsize::new(0));
    let mut relays = Vec::new();
    let (shared_a, shared_b) = (
        crate::services::discord::make_shared_data_for_tests(),
        crate::services::discord::make_shared_data_for_tests(),
    );
    // A's channel is routed only by A, B's only by B.
    let parked = replaced.then(|| channel(&shared_a, root, A_PARKED, &frames, &mut relays));
    let (requests, http, _server) = recording_discord(B_HOOK.0).await;
    let _rest = crate::services::discord::shared_state::test_rest::install(http);
    super::super::super::CODEX_IDLE_ROLLOUT_RELAY_STARTED.store(false, Ordering::Release);
    super::super::super::codex_idle_rollout::TAIL_STARTS
        .lock()
        .unwrap()
        .clear();

    let resume = Arc::new(Notify::new());
    if let Some(parked) = &parked {
        super::super::super::spawn_codex_idle_rollout_relay(shared_a.clone());
        // A's answer is mid-relay, parked before bridge capture, when B takes over.
        let entered = Arc::new(Notify::new());
        *super::super::super::codex_idle_rollout::CAPTURE_PAUSE
            .lock()
            .unwrap() = Some((parked.channel.get(), entered.clone(), resume.clone()));
        hook_first(&shared_a, parked, 7_632_520_301_001).await;
        tokio::time::timeout(Duration::from_secs(15), entered.notified())
            .await
            .expect("A's tail reaches bridge capture");
    }
    super::super::super::spawn_codex_idle_rollout_relay(shared_b.clone());
    // B's sessions exist once B runs, routed only by B.
    let b_hook = channel(&shared_b, root, B_HOOK, &frames, &mut relays);
    let b_rollout = channel(&shared_b, root, B_ROLLOUT, &frames, &mut relays);
    if let Some(parked) = &parked {
        resume.notify_one();
        delivered(&requests, parked).await;
    }
    hook_first(&shared_b, &b_hook, 7_632_520_401_001).await;
    delivered(&requests, &b_hook).await;
    rollout_first(&shared_b, &b_rollout, 7_632_520_402_001).await;
    delivered(&requests, &b_rollout).await;

    // Two more polls of each session past every delivery: nothing is sent twice.
    for codex in [&b_hook, &b_rollout] {
        let visits = poll_notes(&codex.tmux, PollNote::Visit);
        let polled = || poll_notes(&codex.tmux, PollNote::Visit) >= visits + 2;
        assert!(
            wait_for(Duration::from_secs(10), polled).await,
            "polls stopped on {}",
            codex.tmux
        );
    }
    if let Some(parked) = &parked {
        delivered_once(&requests, parked, 7_632_520_301_001);
    }
    delivered_once(&requests, &b_hook, 7_632_520_401_001);
    delivered_once(&requests, &b_rollout, 7_632_520_402_001);
    assert_eq!(
        frames.load(Ordering::SeqCst),
        0,
        "no watcher frame reached a sink"
    );
    let outputs = (output(&requests, &b_hook), output(&requests, &b_rollout));
    let all: Vec<_> = [parked.as_ref(), Some(&b_hook), Some(&b_rollout)]
        .into_iter()
        .flatten()
        .collect();
    for codex in all {
        crate::services::cluster::relay_producer_registry::global_relay_producer_registry()
            .deregister(&codex.tmux);
        crate::services::tmux_diagnostics::set_pane_liveness_override_for_tests(&codex.tmux, None);
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&codex.tmux);
    }
    drop(relays);
    outputs
}

fn scenario(replaced: bool) -> (Output, Output) {
    let root = tempfile::tempdir().expect("isolated root");
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(run(root.path(), replaced))
}

// After B replaces A, both B tails deliver to channels only B routes, A's in-flight answer goes
// once to A, and B's output and cursors equal those of B alone.
#[test]
fn a_replaced_runtime_relays_through_its_own_routing_and_output() {
    let _env_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let alone = scenario(false);
    crate::services::tui_prompt_dedupe::reset_state_for_tests();
    let replaced = scenario(true);
    assert_eq!(
        replaced, alone,
        "B's output and cursors are those of B alone"
    );
}

// Recovery cannot post for a Codex TUI row yet (no pane-ready fallback), so a stale runtime there
// changes no output; once this opens, recovery needs its own B-only case.
#[test]
fn codex_tui_recovery_has_no_pane_ready_fallback_yet() {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    assert!(
        !crate::services::tui_turn_state::pane_ready_fallback_allowed(
            &ProviderKind::Codex,
            Some(RuntimeHandoffKind::CodexTui),
        )
    );
    assert!(
        crate::services::provider::fallback_capture_ready_for_input(
            "› ready",
            &ProviderKind::Codex,
            Some(RuntimeHandoffKind::CodexTui),
        )
        .is_none()
    );
}
