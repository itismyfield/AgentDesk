//! One live fake Codex execution: executor, native direct input, real idle relay, executor.
//! This protocol fixture does not drive Discord FullEvent ingress or a real provider client.
use super::*;
use crate::services::discord::tui_prompt_relay::{self, relay_e2e::discord_mock};
use crate::services::discord::{self, inflight};
use crate::services::tui_prompt_dedupe::{self as dedupe, hosted_source};
use poise::serenity_prelude::{ChannelId, HttpBuilder};

const DIRECT: &str = "ordinary direct prompt [codex-t14]";
const ANSWER: &str = "CODEX_T14_DIRECT_ANSWER";

// A new process owns the one global idle relay; this is test isolation, not a feature switch.
fn isolated() -> bool {
    const CHILD: &str = "ADK_CODEX_T14_COMPOSED_CHILD";
    let exact = format!(
        "{}::codex_direct_idle_relay_and_followup_keep_the_same_bound_execution_pg",
        module_path!().split_once("::").unwrap().1
    );
    if let Some(name) = std::env::var_os(CHILD) {
        assert_eq!(name, exact.as_str());
        return true;
    }
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", &exact, "--nocapture", "--test-threads=1"])
        .env(CHILD, &exact);
    let binary = command
        .get_program()
        .to_str()
        .expect("lib-test binary UTF-8")
        .to_owned();
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_str().expect("lib-test argument UTF-8").to_owned())
        .collect();
    let output = command.output().unwrap();
    // One physical JSON line retains every byte without emitting a second lib-test summary.
    eprintln!(
        "T14_CHILD_RAW {}",
        json!({"exact":exact,"binary":binary,"args":args,
        "status":output.status.to_string(),"success":output.status.success(),
        "stdout_bytes":output.stdout,"stderr_bytes":output.stderr})
    );
    let stdout = std::str::from_utf8(&output.stdout).expect("child stdout UTF-8");
    let stderr = std::str::from_utf8(&output.stderr).expect("child stderr UTF-8");
    let summaries: Vec<_> = stdout
        .lines()
        .filter(|line| line.starts_with("test result:"))
        .collect();
    // Keep the outer exact runner's soft-skip contract over the raw child lines, too.
    let skip = regex::Regex::new(r"(?i)(?:\bsoft[-_ ]?skip\b|\bPG_NOT_RUN\b|\bPG_THROWAWAY_UNAVAILABLE\b|\bNOT_RUN\b|\bskipping\b.*(?:\btest\b|postgres unavailable|database unavailable)|postgres.*not configured)").unwrap();
    let skipped = stdout.lines().chain(stderr.lines()).any(|line| {
        skip.is_match(line)
            && !line.contains("skipping postgres test database cleanup without an ownership token")
    });
    assert!(
        output.status.success()
            && summaries.len() == 1
            && summaries[0]
                .starts_with("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; ")
            && !skipped,
        "{exact}: {}; inspect T14_CHILD_RAW",
        output.status
    );
    eprintln!("isolated child verified: {exact}; selected=1");
    false
}

#[test]
fn codex_direct_idle_relay_and_followup_keep_the_same_bound_execution_pg() {
    if !isolated() {
        return;
    }
    let fx = Fixture::admitted("t14-composed");
    let _database: &crate::db::auto_queue::test_support::TestPostgresDb = fx
        .db
        .as_ref()
        .expect("T14 fixture owns its live PostgreSQL database");
    let launcher = Arc::new(Launcher::default());
    let (nonce, path) = launch(&fx, &fx.ports(&launcher));
    let original = hosted_source::capture("codex", fx.logical(), CHANNEL, &path, None)
        .expect("the real SessionStart committed the source");
    assert_eq!(original.execution_nonce, nonce);
    assert!(
        original.hosted_admission_required,
        "the fixture exercises hosted adoption"
    );
    let fingerprint = std::fs::read(options_of(&fx)).unwrap();
    let initial = binding(&fx).expect("the original native binding");
    assert_eq!(Path::new(&initial.output_path), path.as_path());
    let _registry = fx.rig.registry_on_this_thread();
    let events = dedupe::binding_events::test_root().expect("fixture binding log");
    let install = fx.rig.thread_installer(NODE, KEY, fx.rig.socket(), SESSION);
    let mirror = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_start(move || {
            install();
            dedupe::binding_events::set_test_root(Some(&events));
        })
        .build()
        .unwrap();
    mirror.block_on(direct_phase(&fx, &path, &original));
    drop(mirror);
    assert_eq!(
        fx.row(),
        Some(HostedState::Bound),
        "normal terminal leaves the provider live"
    );
    assert_eq!(
        fx.rig.sends(),
        prompt_sends(),
        "direct input is never submitted twice"
    );
    assert_eq!(
        hosted_source::capture("codex", fx.logical(), CHANNEL, &path, None),
        Some(original.clone())
    );
    fx.rig.answer("pane.read", screen(READY));
    let (taken, messages) = fx.turn(&fx.record(), &fx.ports(&launcher), || {
        reply(&fx, &path, 4, "t14-followup", "CODEX_T14_FOLLOWUP_ANSWER")
    });
    assert_eq!(
        taken,
        Ok(()),
        "the actual Bound follow-up executor takes input"
    );
    assert_eq!(texts(&messages), ["CODEX_T14_FOLLOWUP_ANSWER"]);
    assert_eq!(fx.rig.sends(), [prompt_sends(), prompt_sends()].concat());
    assert_eq!(
        launcher.creates.load(Ordering::SeqCst),
        1,
        "no replacement execution"
    );
    assert_eq!(launcher.nonces.lock().unwrap().as_slice(), &[nonce]);
    assert_eq!(std::fs::read(options_of(&fx)).unwrap(), fingerprint);
    assert_eq!(
        hosted_source::capture("codex", fx.logical(), CHANNEL, &path, None),
        Some(original)
    );
    let after = binding(&fx).unwrap();
    assert_eq!(
        (&after.output_path, &after.session_id),
        (&initial.output_path, &initial.session_id)
    );
    assert_eq!(fx.row(), Some(HostedState::Bound));
}

async fn until(label: &str, mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|error| panic!("{label}: {error}"));
}

async fn direct_phase(fx: &Fixture, path: &Path, original: &hosted_source::HostedSourcePin) {
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let health = Arc::new(discord::health::HealthRegistry::new());
    discord::session_relay_sink::SessionBoundDiscordRelaySink::new(health.clone())
        .enable_delivery_for_test();
    let mut shared = discord::make_shared_data_for_tests_with_storage(Some(fx.pool.clone()));
    let fresh = Arc::get_mut(&mut shared).unwrap();
    fresh.provider = ProviderKind::Codex;
    fresh.token_hash = TOKEN.to_string();
    fresh.health_registry = Arc::downgrade(&health);
    let channel = ChannelId::new(CHANNEL);
    assert!(
        shared
            .tmux_watchers
            .restore_owner_channel_for_tmux_session(fx.logical(), channel),
        "the restored Herdr owner supplies routing without a tmux watcher"
    );
    let mock = discord_mock::DiscordMockState::new();
    mock.allow_channel(CHANNEL);
    mock.park_first_placeholder.store(false, Ordering::SeqCst);
    let (proxy, _, server) = discord_mock::start(mock.clone()).await;
    let _server = AbortServer(server.abort_handle());
    let http = Arc::new(
        HttpBuilder::new("test-token")
            .proxy(proxy)
            .ratelimiter_disabled(true)
            .build(),
    );
    health
        .set_utility_bot_http_for_tests(discord::bot_role::UtilityBotRole::Notify, http.clone())
        .await;
    let _rest = discord::shared_state::test_rest::install(http);
    let mut observed = dedupe::subscribe_observed_prompts();
    let mut completed = shared.inflight_signals.subscribe();
    assert!(
        hosted_source::current_execution_for_adoption(&shared, "codex", channel, fx.logical())
            .await,
        "the same canonical Bound row and fake OS admit the direct prompt"
    );
    tui_prompt_relay::spawn_tui_prompt_relay(shared.clone(), ProviderKind::Codex);
    let turn = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    append(
        path,
        &[
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":turn}}),
            json!({"type":"turn_context","payload":{"turn_id":turn}}),
            json!({"timestamp":now.to_rfc3339(),"type":"response_item","payload":{
            "id":uuid::Uuid::new_v4().to_string(),"type":"message","role":"user",
            "content":[{"type":"input_text","text":DIRECT}],
            "internal_chat_message_metadata_passthrough":{"turn_id":turn,
                "create_time":now.timestamp_millis() as f64 / 1000.0,
                "content_item_kinds":["user.text"]}}}),
            json!({"type":"event_msg","payload":{"type":"item_completed","turn_id":turn,
            "item":{"type":"UserMessage"}}}),
        ],
    );
    let observation = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let prompt = observed.recv().await.expect("live idle observation stream");
            if prompt.tmux_session_name == fx.logical() && prompt.prompt == DIRECT {
                break prompt;
            }
        }
    })
    .await
    .expect("the actual native idle scanner publishes the direct prompt");
    assert_eq!(observation.native_turn_id.as_deref(), Some(turn.as_str()));
    assert_eq!(observation.hosted_source.as_ref(), Some(original));
    let claimed = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(row) =
                inflight::load_inflight_state_read_only(&ProviderKind::Codex, CHANNEL)
            {
                break row;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(claimed.is_ok(), "t14_codex_direct_live_relay_once_contract");
    let row = claimed.expect("the contract assertion confirmed the actual inflight row");
    let anchor = row.user_msg_id;
    assert_ne!(anchor, 0, "a real loopback Discord prompt anchor");
    assert_eq!(row.native_turn_id.as_deref(), Some(turn.as_str()));
    assert_eq!(
        row.hosted_execution_nonce.as_deref(),
        Some(original.execution_nonce.as_str())
    );
    let lease = dedupe::external_input_relay_lease("codex", fx.logical(), CHANNEL).unwrap();
    assert_eq!(lease.hosted_source.as_ref(), Some(original));
    assert_eq!(
        fx.rig.sends(),
        prompt_sends(),
        "observer never resubmits direct input"
    );
    append(
        path,
        &[
            json!({"timestamp":chrono::Utc::now().to_rfc3339(),"type":"response_item","payload":{
            "id":uuid::Uuid::new_v4().to_string(),"type":"message","role":"assistant",
            "content":[{"type":"output_text","text":ANSWER}]}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":turn,
            "last_agent_message":ANSWER}}),
        ],
    );
    until("one native answer and its terminal summary", || {
        let messages = mock.message_snapshot();
        messages
            .iter()
            .any(|(id, text)| *id == anchor && text == ANSWER)
            && messages.iter().any(|(_, text)| text.starts_with("-# ✅ "))
    })
    .await;
    let signal = tokio::time::timeout(Duration::from_secs(20), completed.recv())
        .await
        .expect("natural terminal bridge drop")
        .expect("completion stream");
    assert!(
        matches!(signal, inflight::InflightSignal::Completed { channel_id: CHANNEL, turn_id } if turn_id == anchor)
    );
    until("natural terminal clears its own inflight", || {
        inflight::load_inflight_state_read_only(&ProviderKind::Codex, CHANNEL).is_none()
    })
    .await;
    // The cursor is a state witness that the idle loop made further progress after completion.
    let eof = std::fs::metadata(path).unwrap().len();
    until("idle source cursor reaches the natural terminal", || {
        binding(fx).is_some_and(|binding| binding.last_offset >= eof)
    })
    .await;
    for pass in 0..2 {
        append(path, &[json!({"type":"t14_poll_checkpoint","pass":pass})]);
        let end = std::fs::metadata(path).unwrap().len();
        until("post-completion idle pass", || {
            binding(fx).is_some_and(|binding| binding.last_offset >= end)
        })
        .await;
    }
    let settled = mock.message_snapshot();
    assert_eq!(
        settled.iter().filter(|(_, text)| text == ANSWER).count(),
        1,
        "one logical answer"
    );
    assert_eq!(
        settled
            .iter()
            .filter(|(_, text)| text.starts_with("-# ✅ "))
            .count(),
        1,
        "one summary"
    );
    assert_eq!(
        mock.post_snapshot()
            .iter()
            .filter(|(_, text)| text.contains(DIRECT))
            .count(),
        1,
        "one actual observed-prompt announcement"
    );
    assert!(
        completed.try_recv().is_err(),
        "no duplicate terminal completion"
    );
    assert_eq!(
        discord::mailbox_snapshot(&shared, channel)
            .await
            .active_user_message_id,
        None
    );
    assert!(dedupe::prompt_anchor_for_response("codex", fx.logical(), CHANNEL).is_none());
    assert!(!dedupe::external_input_relay_lease_present(
        "codex",
        fx.logical(),
        CHANNEL
    ));
    assert!(
        mock.unhandled.lock().unwrap().is_empty(),
        "all HTTP stays on the loopback fixture"
    );
    assert!(
        hosted_source::current_execution_for_adoption(&shared, "codex", channel, fx.logical())
            .await,
        "task_complete leaves the same provider alive"
    );
}

struct AbortServer(tokio::task::AbortHandle);
impl Drop for AbortServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}
