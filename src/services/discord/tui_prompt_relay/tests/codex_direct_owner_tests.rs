//! TUI-direct claims while the live tmux watcher tails a file other than the row's output.
//! A Codex TUI watcher reads the relay jsonl, never the rollout the synthetic row names.
use super::super::codex_idle_rollout::PollNote;
use super::*;
use crate::services::cluster::stream_relay::{
    RelaySink, RelaySinkError, RelaySinkOutcome, StreamFrame,
};
use crate::services::discord::inflight;
use crate::services::discord::session_relay_sink::tests::idle_relay_harness::{
    IdleRelayHarness, PAYLOAD, live_watcher,
};
use std::sync::atomic::{AtomicU64, AtomicUsize};
use tokio::sync::Notify;

const PROMPT: &str = "codex direct prompt 5704";
const RESPONSE: &str = "DIRECT_5704_OK";
const OLD_RESPONSE: &str = "OLD_5704";

fn enable_session_bound_delivery() {
    let health = Arc::new(crate::services::discord::health::HealthRegistry::new());
    crate::services::discord::session_relay_sink::SessionBoundDiscordRelaySink::new(health)
        .enable_delivery_for_test();
}

/// The coverage fact behind `SessionBoundRelay`: the live watcher reads the row's output file.
#[test]
fn watcher_covers_only_the_output_file_it_reads() {
    use super::synthetic_start::claim::tui_direct_watcher_covers_output;
    let dir = tempfile::tempdir().expect("temp dir");
    let rollout = dir.path().join("rollout.jsonl");
    std::fs::write(&rollout, b"").expect("rollout");
    let alias = dir.path().join("alias.jsonl");
    std::os::unix::fs::symlink(&rollout, &alias).expect("alias");
    let relay = dir.path().join("relay.jsonl");
    let tmux = "AgentDesk-codex-5704-covers";
    let channel = ChannelId::new(5_704_000_001);
    let watchers = crate::services::discord::TmuxWatcherRegistry::new();
    let covers = |watchers: &crate::services::discord::TmuxWatcherRegistry| {
        tui_direct_watcher_covers_output(watchers, tmux, Some(&rollout))
    };

    assert!(!covers(&watchers), "no watcher");
    for (watched, expected) in [(&rollout, true), (&alias, true), (&relay, false)] {
        watchers.insert(channel, live_watcher(tmux, watched.to_str().unwrap()));
        assert_eq!(
            covers(&watchers),
            expected,
            "watcher on {}",
            watched.display()
        );
    }
    // Uncanonicalizable paths fall back to the raw path, and no output path is covered.
    let missing = dir.path().join("missing.jsonl");
    let dangling = dir.path().join("dangling.jsonl");
    std::os::unix::fs::symlink(&missing, &dangling).expect("dangling alias");
    let other_missing = dir.path().join("other-missing.jsonl");
    for (watched, output, expected) in [
        (&missing, Some(&missing), true),
        (&missing, Some(&other_missing), false),
        (&dangling, Some(&missing), false),
        (&relay, None, true),
    ] {
        watchers.insert(channel, live_watcher(tmux, watched.to_str().unwrap()));
        let output = output.map(PathBuf::as_path);
        assert_eq!(
            tui_direct_watcher_covers_output(&watchers, tmux, output),
            expected,
            "watcher on {} for {output:?}",
            watched.display()
        );
    }
}

/// Claude transcript rotation, watcher still on the previous transcript: the claim picks
/// `BridgeAdapter`, the bridge tail is due to spawn, and the session-bound sink sends nothing.
#[tokio::test(flavor = "current_thread")]
async fn claude_claim_beside_a_watcher_on_the_previous_transcript_picks_the_bridge_owner() {
    let root = tempfile::tempdir().expect("isolated inflight root");
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    enable_session_bound_delivery();
    let channel = ChannelId::new(5_704_000_002);
    let tmux = "AgentDesk-claude-5704-rotation";
    let mut harness = IdleRelayHarness::start(root.path(), channel.get(), tmux).await;
    let shared = harness.shared.clone();
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        tmux,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: harness.transcript.to_str().expect("utf8 path").to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let previous = root.path().join("previous-transcript.jsonl");
    std::fs::write(&previous, b"").expect("previous transcript");
    shared
        .tmux_watchers
        .insert(channel, live_watcher(tmux, previous.to_str().unwrap()));

    let claim = synthetic_start::claim_tui_direct_synthetic_turn(
        &shared,
        &ProviderKind::Claude,
        channel,
        tmux,
        "prompt",
        MessageId::new(5_704_000_102),
        &s1_lease_5833(Some("turn-5704-rotation")),
    )
    .await;
    assert!(claim.claimed);
    assert_eq!(claim.relay_owner, ExternalInputRelayOwner::BridgeAdapter);
    assert!(observer_should_spawn_bridge_tail(false, claim.relay_owner));
    let row = inflight::load_inflight_state(&ProviderKind::Claude, channel.get()).expect("row");
    assert_eq!(row.effective_relay_owner_kind(), RelayOwnerKind::None);
    assert!(
        !crate::services::discord::session_relay_sink::session_bound_discord_relay_can_own_terminal_delivery(
            Some(&row),
            tmux
        )
    );

    std::fs::write(&harness.transcript, PAYLOAD).expect("turn output");
    tokio::time::sleep(std::time::Duration::from_millis(11_000)).await;
    assert!(
        harness.sent().is_empty(),
        "the session-bound sink stays silent"
    );
    harness.stop().await;
}

/// An idle observer stands down for a session-bound row only while the live watcher reads
/// that row's output; a watcher on the relay jsonl feeds the sink nothing, so it must not.
#[tokio::test(flavor = "current_thread")]
async fn idle_observer_waits_out_a_session_bound_row_only_when_the_watcher_reads_its_output() {
    let root = tempfile::tempdir().expect("isolated inflight root");
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let channel = ChannelId::new(5_704_000_003);
    let tmux = "AgentDesk-codex-5704-wait";
    let rollout = root.path().join("rollout.jsonl");
    std::fs::write(&rollout, b"").expect("rollout");
    let alias = root.path().join("rollout-alias.jsonl");
    std::os::unix::fs::symlink(&rollout, &alias).expect("alias");
    let relay = crate::services::tmux_common::session_temp_path(tmux, "jsonl");
    let shared = crate::services::discord::make_shared_data_for_tests();
    let row = build_tui_direct_synthetic_inflight_state(
        ProviderKind::Codex,
        channel,
        MessageId::new(5_704_000_103),
        None,
        PROMPT,
        tmux,
        Some(&rollout),
        0,
        &ExternalInputRelayLease::unassigned(Some(channel.get())),
        RelayOwnerKind::SessionBoundRelay,
    );
    inflight::save_inflight_state(&row).expect("session-bound row");
    let relay_handle = crate::services::cluster::stream_relay::spawn_stream_relay(
        crate::services::cluster::session_matcher::MatchedChannel {
            channel_id: channel.get().to_string(),
            agent_id: "agent-5704".to_string(),
            provider: ProviderKind::Codex,
            expected_session_name: tmux.to_string(),
            expected_rollout_path: rollout.to_str().unwrap().to_string(),
        },
        Arc::new(CountingSink(Arc::default())),
    );
    let producers =
        crate::services::cluster::relay_producer_registry::global_relay_producer_registry();
    producers.register(tmux.to_string(), relay_handle.producer());
    let waits = |watched: &str| {
        shared
            .tmux_watchers
            .insert(channel, live_watcher(tmux, watched));
        synthetic_start::wait_for_tui_direct_synthetic_non_bridge_claim(
            &shared.tmux_watchers,
            &ProviderKind::Codex,
            channel,
            tmux,
        )
    };

    assert!(
        !waits(&relay).await,
        "a relay jsonl watcher never feeds the sink"
    );
    assert!(
        waits(alias.to_str().unwrap()).await,
        "the watcher reads the row's rollout"
    );
    producers.deregister(tmux);
}

/// Session-bound sink that only counts the frames a watcher forwarded.
struct CountingSink(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl RelaySink for CountingSink {
    async fn deliver(&self, _frame: &StreamFrame) -> Result<RelaySinkOutcome, RelaySinkError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(RelaySinkOutcome::FrameAccepted)
    }
}

/// `(method, path, body)` of every Discord REST request.
type Requests = Arc<std::sync::Mutex<Vec<(String, String, String)>>>;

async fn recording_discord(channel: u64) -> (Requests, Arc<serenity::Http>, AbortOnDrop) {
    use axum::body::Bytes;
    use axum::http::{Method, StatusCode, Uri};
    use axum::response::IntoResponse;
    let requests = Requests::default();
    let next = Arc::new(AtomicU64::new(5_704_900_000));
    let recorded = requests.clone();
    let app = axum::Router::new().fallback(axum::routing::any(
        move |method: Method, uri: Uri, body: Bytes| {
            let (recorded, next) = (recorded.clone(), next.clone());
            async move {
                let path = uri.path().to_string();
                let body = String::from_utf8_lossy(&body).into_owned();
                recorded
                    .lock()
                    .unwrap()
                    .push((method.to_string(), path.clone(), body));
                if method == Method::DELETE || method == Method::PUT || path.ends_with("/typing") {
                    return StatusCode::NO_CONTENT.into_response();
                }
                let id = path
                    .rsplit('/')
                    .next()
                    .and_then(|tail| tail.parse::<u64>().ok())
                    .filter(|_| method != Method::POST)
                    .unwrap_or_else(|| next.fetch_add(1, Ordering::SeqCst));
                axum::Json(serde_json::json!({
                    "id": id.to_string(), "channel_id": channel.to_string(), "content": "",
                    "author": {"id": "1", "username": "bot", "discriminator": "0001", "avatar": null},
                    "timestamp": "2026-10-07T00:00:00+00:00", "edited_timestamp": null,
                    "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
                    "attachments": [], "embeds": [], "pinned": false, "type": 0
                }))
                .into_response()
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = serenity::HttpBuilder::new("test-token")
        .proxy(format!("http://{}", listener.local_addr().unwrap()))
        .ratelimiter_disabled(true)
        .build();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (requests, Arc::new(http), AbortOnDrop(server.abort_handle()))
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn rollout_line(value: serde_json::Value) -> String {
    format!("{value}\n")
}

fn user_line() -> String {
    rollout_line(serde_json::json!({"type": "response_item", "payload": {
        "type": "message", "role": "user", "content": [{"type": "input_text", "text": PROMPT}]}}))
}

fn answer_lines() -> String {
    answer_lines_with(RESPONSE)
}

fn answer_lines_with(text: &str) -> String {
    rollout_line(serde_json::json!({"type": "response_item", "payload": {
        "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}}))
        + &rollout_line(serde_json::json!({"type": "event_msg", "payload": {
            "type": "task_complete", "last_agent_message": text}}))
}

fn append(path: &Path, text: &str) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open rollout");
    file.write_all(text.as_bytes()).expect("append rollout");
}

/// One Codex TUI channel as production leaves it after a Discord turn: the binding names
/// the rollout, the live watcher tails the relay jsonl, and a supervisor producer is live.
struct CodexChannel {
    channel: ChannelId,
    tmux: String,
    rollout: PathBuf,
}

impl CodexChannel {
    fn new(shared: &Arc<SharedData>, root: &Path, channel: u64, tmux: &str) -> Self {
        Self::with_history(shared, root, channel, tmux, "")
    }

    /// `history` is rollout content the binding cursor has already passed.
    fn with_history(
        shared: &Arc<SharedData>,
        root: &Path,
        channel: u64,
        tmux: &str,
        history: &str,
    ) -> Self {
        let rollout = root.join(format!("{tmux}-rollout.jsonl"));
        let header =
            rollout_line(serde_json::json!({"type": "session_meta", "payload": {"id": "s-5704"}}))
                + history;
        std::fs::write(&rollout, &header).expect("rollout");
        let relay = crate::services::tmux_common::session_temp_path(tmux, "jsonl");
        let generation = crate::services::tmux_common::session_temp_path(tmux, "generation");
        std::fs::write(generation, b"1").expect("tmux generation");
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            tmux,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: rollout.to_str().unwrap().to_string(),
                relay_output_path: Some(relay.clone()),
                input_fifo_path: None,
                session_id: None,
                last_offset: header.len() as u64,
                relay_last_offset: Some(0),
            },
        );
        let channel = ChannelId::new(channel);
        shared
            .tmux_watchers
            .insert(channel, live_watcher(tmux, &relay));
        crate::services::tmux_diagnostics::set_pane_liveness_override_for_tests(
            tmux,
            Some(crate::services::platform::tmux::PaneLiveness::Live),
        );
        Self {
            channel,
            tmux: tmux.to_string(),
            rollout,
        }
    }

    /// The lease the observer records for this prompt (owner resolved from the watcher).
    fn lease(&self, shared: &Arc<SharedData>) -> ExternalInputRelayLease {
        super::super::relay_ownership::record_external_turn_lease_for_output(
            shared,
            &ProviderKind::Codex,
            self.channel,
            &self.tmux,
            RuntimeHandoffKind::CodexTui,
            &self.rollout,
            chrono::Utc::now(),
        )
    }

    async fn claim(
        &self,
        shared: &Arc<SharedData>,
        anchor: u64,
        lease: &ExternalInputRelayLease,
    ) -> synthetic_start::TuiDirectSyntheticTurnClaim {
        synthetic_start::claim_tui_direct_synthetic_turn(
            shared,
            &ProviderKind::Codex,
            self.channel,
            &self.tmux,
            PROMPT,
            MessageId::new(anchor),
            lease,
        )
        .await
    }

    fn row(&self) -> Option<inflight::InflightTurnState> {
        inflight::load_inflight_state(&ProviderKind::Codex, self.channel.get())
    }
}

/// `(method, path, body)` of the Discord requests on `channel` whose body carries the answer.
fn deliveries(requests: &Requests, channel: ChannelId) -> Vec<(String, String, String)> {
    let needle = format!("/channels/{}/", channel.get());
    requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, path, body)| path.contains(&needle) && body.contains(RESPONSE))
        .cloned()
        .collect()
}

fn tail_starts(channel: ChannelId) -> usize {
    let starts = super::super::codex_idle_rollout::TAIL_STARTS
        .lock()
        .unwrap();
    starts.iter().filter(|id| **id == channel.get()).count()
}

fn poll_notes(tmux: &str, note: PollNote) -> usize {
    let notes = super::super::codex_idle_rollout::POLL_NOTES.lock().unwrap();
    notes.get(&(tmux.to_string(), note)).copied().unwrap_or(0)
}

async fn wait_for(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    done()
}

/// Real idle rollout loop, watcher on the relay jsonl: every claim/scan order delivers the
/// answer once, and a rebinding channel gets no rollout-side delivery.
#[test]
fn codex_direct_answer_reaches_discord_once_beside_a_relay_jsonl_watcher() {
    let _env_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("isolated root");
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(codex_direct_answer_scenario(root.path()));
}

async fn codex_direct_answer_scenario(root: &Path) {
    enable_session_bound_delivery();
    // An empty writer list keeps every channel on the Legacy relay these paths serve.
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let hook_first = CodexChannel::new(&shared, root, 5_704_100, "AgentDesk-codex-5704-hook");
    let rollout_first = CodexChannel::new(&shared, root, 5_704_200, "AgentDesk-codex-5704-rollout");
    let rebinding = CodexChannel::new(&shared, root, 5_704_300, "AgentDesk-codex-5704-rebind");
    let claim_first = CodexChannel::new(&shared, root, 5_704_400, "AgentDesk-codex-5704-claim");
    let parked = CodexChannel::new(&shared, root, 5_704_500, "AgentDesk-codex-5704-parked");
    let earlier_turn = user_line() + &answer_lines_with(OLD_RESPONSE);
    let repeat = CodexChannel::with_history(
        &shared,
        root,
        5_704_600,
        "AgentDesk-codex-5704-repeat",
        &earlier_turn,
    );
    let channels = [
        &hook_first,
        &rollout_first,
        &rebinding,
        &claim_first,
        &parked,
        &repeat,
    ];
    let frames = Arc::new(AtomicUsize::new(0));
    let mut relays = Vec::new();
    for codex in channels {
        let relay = crate::services::cluster::stream_relay::spawn_stream_relay(
            crate::services::cluster::session_matcher::MatchedChannel {
                channel_id: codex.channel.get().to_string(),
                agent_id: "agent-5704".to_string(),
                provider: ProviderKind::Codex,
                expected_session_name: codex.tmux.clone(),
                expected_rollout_path: codex.rollout.to_str().unwrap().to_string(),
            },
            Arc::new(CountingSink(frames.clone())),
        );
        crate::services::cluster::relay_producer_registry::global_relay_producer_registry()
            .register(codex.tmux.clone(), relay.producer());
        relays.push(relay);
    }
    let mut finalized = shared.inflight_signals.subscribe();
    let (requests, http, _server) = recording_discord(hook_first.channel.get()).await;
    let _rest = crate::services::discord::shared_state::test_rest::install(http);
    super::super::CODEX_IDLE_ROLLOUT_RELAY_STARTED.store(false, Ordering::Release);
    super::super::spawn_codex_idle_rollout_relay(shared.clone());

    // Hook first: the UserPromptSubmit hook records the prompt, the rollout scan sees a
    // recent duplicate and only advances its cursor, then the observer claims.
    crate::services::tui_prompt_dedupe::observe_hook_prompt_by_tmux_with_prompt_id_at(
        "codex",
        &hook_first.tmux,
        PROMPT,
        None,
        chrono::Utc::now(),
    );
    append(&hook_first.rollout, &user_line());
    let prompt_end = std::fs::metadata(&hook_first.rollout).unwrap().len();
    assert!(
        wait_for(Duration::from_secs(5), || {
            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&hook_first.tmux)
                .is_some_and(|binding| binding.last_offset == prompt_end)
        })
        .await,
        "the idle loop never scanned past the hook-observed prompt"
    );
    let lease = hook_first.lease(&shared);
    assert!(hook_first.claim(&shared, 5_704_101, &lease).await.claimed);
    append(&hook_first.rollout, &answer_lines());
    assert!(
        wait_for(Duration::from_secs(15), || !deliveries(
            &requests,
            hook_first.channel
        )
        .is_empty())
        .await,
        "hook-first answer never reached Discord: {:?}",
        requests.lock().unwrap()
    );

    // Claim before the scan: the claim's start sits before the prompt, and the repair tail
    // must deliver under that boundary rather than restamp it.
    crate::services::tui_prompt_dedupe::observe_hook_prompt_by_tmux_with_prompt_id_at(
        "codex",
        &claim_first.tmux,
        PROMPT,
        None,
        chrono::Utc::now(),
    );
    let lease = claim_first.lease(&shared);
    let claim = claim_first.claim(&shared, 5_704_401, &lease).await;
    assert!(claim.claimed);
    append(&claim_first.rollout, &user_line());
    let prompt_end = std::fs::metadata(&claim_first.rollout).unwrap().len();
    assert!(claim.turn_start_offset < prompt_end);
    append(&claim_first.rollout, &answer_lines());
    assert!(
        wait_for(Duration::from_secs(15), || !deliveries(
            &requests,
            claim_first.channel
        )
        .is_empty())
        .await,
        "claim-first answer never reached Discord: {:?}",
        requests.lock().unwrap()
    );

    // A repair tail parked just before bridge capture: the polls meanwhile start no second tail.
    let (entered, resume) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    *super::super::codex_idle_rollout::CAPTURE_PAUSE
        .lock()
        .unwrap() = Some((parked.channel.get(), entered.clone(), resume.clone()));
    crate::services::tui_prompt_dedupe::observe_hook_prompt_by_tmux_with_prompt_id_at(
        "codex",
        &parked.tmux,
        PROMPT,
        None,
        chrono::Utc::now(),
    );
    let lease = parked.lease(&shared);
    assert!(parked.claim(&shared, 5_704_501, &lease).await.claimed);
    append(&parked.rollout, &(user_line() + &answer_lines()));
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .expect("the repair tail reaches bridge capture");
    assert_eq!(parked.row().expect("parked row").current_msg_id, 0);
    let visits = poll_notes(&parked.tmux, PollNote::Visit);
    assert!(
        wait_for(Duration::from_secs(10), || poll_notes(
            &parked.tmux,
            PollNote::Visit
        ) >= visits + 2)
        .await,
        "polls stopped visiting the parked session"
    );
    assert_eq!(
        tail_starts(parked.channel),
        1,
        "polls during the parked tail start no other"
    );
    resume.notify_one();
    assert!(
        wait_for(Duration::from_secs(15), || !deliveries(
            &requests,
            parked.channel
        )
        .is_empty())
        .await,
        "parked answer never reached Discord"
    );

    // Same prompt after a completed turn, claimed before it lands: the repair poll meets only
    // the earlier prompt and must leave the claim, its lease and the cursor as they stand.
    crate::services::tui_prompt_dedupe::observe_hook_prompt_by_tmux_with_prompt_id_at(
        "codex",
        &repeat.tmux,
        PROMPT,
        None,
        chrono::Utc::now(),
    );
    let lease = repeat.lease(&shared);
    let claim = repeat.claim(&shared, 5_704_601, &lease).await;
    assert!(claim.claimed);
    let cursor = std::fs::metadata(&repeat.rollout).unwrap().len();
    assert_eq!(claim.turn_start_offset, cursor);
    let row_before = serde_json::to_value(repeat.row().expect("repeat row")).unwrap();
    let live_lease = || {
        crate::services::tui_prompt_dedupe::external_input_relay_lease(
            "codex",
            &repeat.tmux,
            repeat.channel.get(),
        )
    };
    let lease_before = live_lease();
    assert!(
        wait_for(Duration::from_secs(10), || poll_notes(
            &repeat.tmux,
            PollNote::EarlierPrompt
        ) > 0)
        .await,
        "the repair poll never met the earlier prompt"
    );
    let visits = poll_notes(&repeat.tmux, PollNote::Visit);
    assert!(
        wait_for(Duration::from_secs(10), || poll_notes(
            &repeat.tmux,
            PollNote::Visit
        ) > visits)
        .await,
        "polls stopped visiting the repeat session"
    );
    assert_eq!(
        serde_json::to_value(repeat.row().expect("repeat row")).unwrap(),
        row_before,
        "the claim stands as it was"
    );
    assert_eq!(live_lease(), lease_before);
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&repeat.tmux);
    assert_eq!(binding.expect("repeat binding").last_offset, cursor);
    assert_eq!(tail_starts(repeat.channel), 0);
    append(&repeat.rollout, &(user_line() + &answer_lines()));
    assert!(
        wait_for(Duration::from_secs(15), || !deliveries(
            &requests,
            repeat.channel
        )
        .is_empty())
        .await,
        "repeated prompt's answer never reached Discord: {:?}",
        requests.lock().unwrap()
    );

    // Rollout first: the loop publishes the prompt and waits for the observer's claim.
    append(&rollout_first.rollout, &user_line());
    let channel = rollout_first.channel.get();
    assert!(
        wait_for(Duration::from_secs(5), || {
            crate::services::tui_prompt_dedupe::external_input_relay_lease_present(
                "codex",
                &rollout_first.tmux,
                channel,
            )
        })
        .await,
        "the idle loop never published the rollout prompt"
    );
    let lease = crate::services::tui_prompt_dedupe::external_input_relay_lease(
        "codex",
        &rollout_first.tmux,
        channel,
    )
    .expect("published lease");
    assert!(
        rollout_first
            .claim(&shared, 5_704_201, &lease)
            .await
            .claimed
    );
    // Before the loop's claim wait ends: no live pane here keeps a tail waiting for output.
    append(&rollout_first.rollout, &answer_lines());
    assert!(
        wait_for(Duration::from_secs(15), || !deliveries(
            &requests,
            rollout_first.channel
        )
        .is_empty())
        .await,
        "rollout-first answer never reached Discord: {:?}",
        requests.lock().unwrap()
    );

    // Rebind: a watcher-owned rebind row holds the channel while its writer feeds the relay
    // jsonl, so the direct input gets no synthetic row and the loop adds no delivery.
    let mut rebind_row = build_tui_direct_synthetic_inflight_state(
        ProviderKind::Codex,
        rebinding.channel,
        MessageId::new(5_704_301),
        Some(MessageId::new(5_704_302)),
        "rebound turn",
        &rebinding.tmux,
        Some(Path::new(&crate::services::tmux_common::session_temp_path(
            &rebinding.tmux,
            "jsonl",
        ))),
        0,
        &ExternalInputRelayLease::unassigned(Some(rebinding.channel.get())),
        RelayOwnerKind::Watcher,
    );
    rebind_row.rebind_origin = true;
    rebind_row.turn_source = inflight::TurnSource::ExternalAdopted;
    rebind_row.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
    inflight::save_inflight_state(&rebind_row).expect("rebind row");
    crate::services::tui_prompt_dedupe::observe_hook_prompt_by_tmux_with_prompt_id_at(
        "codex",
        &rebinding.tmux,
        PROMPT,
        None,
        chrono::Utc::now(),
    );
    let lease = rebinding.lease(&shared);
    assert!(!rebinding.claim(&shared, 5_704_303, &lease).await.claimed);
    append(&rebinding.rollout, &(user_line() + &answer_lines()));

    // Several polls past every first delivery: no second tail, sink frame or rebind send.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let mut finalized_turns = Vec::new();
    while let Ok(inflight::InflightSignal::Completed { channel_id, .. }) = finalized.try_recv() {
        finalized_turns.push(channel_id);
    }
    for (codex, anchor) in [
        (&hook_first, 5_704_101),
        (&rollout_first, 5_704_201),
        (&claim_first, 5_704_401),
        (&parked, 5_704_501),
        (&repeat, 5_704_601),
    ] {
        let sent = deliveries(&requests, codex.channel);
        let anchor = format!("/channels/{}/messages/{anchor}", codex.channel.get());
        assert_eq!(sent.len(), 1, "one answer edit: {sent:?}");
        let (method, path, body) = &sent[0];
        assert!(method == "PATCH" && path.ends_with(&anchor), "{sent:?}");
        let content = serde_json::from_str::<serde_json::Value>(body).unwrap()["content"].clone();
        assert_eq!(content, RESPONSE, "the answer edits the claimed anchor");
        let turns = finalized_turns
            .iter()
            .filter(|id| **id == codex.channel.get());
        assert_eq!(turns.count(), 1, "one bridge turn delivers {}", codex.tmux);
        assert!(codex.row().is_none(), "{} row cleared", codex.tmux);
        let mailbox = crate::services::discord::mailbox_snapshot(&shared, codex.channel).await;
        assert_eq!(
            mailbox.active_user_message_id, None,
            "{} released",
            codex.tmux
        );
        assert_eq!(tail_starts(codex.channel), 1, "one tail for {}", codex.tmux);
    }
    assert!(deliveries(&requests, rebinding.channel).is_empty());
    assert!(
        !requests
            .lock()
            .unwrap()
            .iter()
            .any(|(_, _, body)| body.contains(OLD_RESPONSE)),
        "an earlier turn's answer is never sent"
    );
    assert_eq!(
        frames.load(Ordering::SeqCst),
        0,
        "no watcher frame reached a sink"
    );
    let rebind_after = rebinding.row().expect("rebind row stays");
    assert!(rebind_after.rebind_origin);
    assert_eq!(rebind_after.user_msg_id, 5_704_301);
    for codex in channels {
        crate::services::cluster::relay_producer_registry::global_relay_producer_registry()
            .deregister(&codex.tmux);
        crate::services::tmux_diagnostics::set_pane_liveness_override_for_tests(&codex.tmux, None);
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&codex.tmux);
    }
    drop(relays);
}
