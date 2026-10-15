//! Real intake arriving after a terminated execution's hold release waits through settlement,
//! then starts one Herdr launch. Routing stays `disabled`; the queued id and text witness intake.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use poise::serenity_prelude::MessageId;

use super::discord_mock::user_message;
use super::{CHANNEL_ID, RelayE2eHarness, serenity, wait_until};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::herdr_terminate::{
    OperatorTerminate, TerminationResult, probe_after_settlement, probe_settlement_window,
    terminate_explicit_herdr, test_turn,
};
use crate::services::discord::queue_io::cancel_backstop_test_support as observed;
use crate::services::discord::router;
use crate::services::provider::ProviderKind;
use crate::services::session_host::herdr_termination_rig as herdr;

const ORIGINAL: &str = "input arriving inside the settlement window [5340-m2a]";
/// The event backstop that takes over a listener's no-start waits 60s; the start follows it.
const START_WAIT: Duration = Duration::from_secs(120);

fn recent_message_id(low: u64) -> MessageId {
    const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
    let recent_ms = chrono::Utc::now().timestamp_millis() - 30_000 - DISCORD_EPOCH_MS;
    MessageId::new((u64::try_from(recent_ms).unwrap() << 22) | low)
}

/// Listener ready first, terminate, a real message inside the settlement window, settlement
/// against the held transition, then exactly one Herdr launch of that queued message.
#[test]
fn m2_1621_real_intake_through_termination_to_one_herdr_start_pg() {
    let path = concat!(
        module_path!(),
        "::m2_1621_real_intake_through_termination_to_one_herdr_start_pg"
    );
    if !herdr::in_child(path, CHANNEL_ID) {
        return;
    }
    let rig = herdr::P11TerminationRig::boot(CHANNEL_ID);
    let install = rig.thread_installer();
    install();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .on_thread_start(install)
        .build()
        .unwrap()
        .block_on(scenario(rig));
}

async fn scenario(rig: herdr::P11TerminationRig) {
    let mut database = None;
    let harness = RelayE2eHarness::start_bound_on(async {
        let fixture = TestPostgresDb::create().await;
        let pool = fixture.connect_and_migrate().await;
        database = Some(fixture);
        pool
    })
    .await;
    let database = database.expect("an isolated PostgreSQL database");
    let pool = harness.shared.pg_pool.clone().expect("PG-backed runtime");
    let (shared, channel) = (harness.shared.clone(), harness.channel_id);
    harness.answer_placeholders_immediately();
    harness.cache_relay_transport();
    shared.restart.reconcile_done.store(true, Ordering::SeqCst);
    let previous = shared
        .core
        .lock()
        .await
        .sessions
        .get_mut(&channel)
        .expect("bound fixture channel")
        .channel_name
        .replace("p11-m2-herdr".into());
    assert!(previous.is_none());
    std::mem::forget(
        crate::services::claude_tui::hook_server::publish_hook_endpoint(
            "http://127.0.0.1:1".into(),
        ),
    );
    let (store, era) =
        crate::services::herdr_launch::o_store_for_test(harness.root.path(), &[CHANNEL_ID]);
    let mut seeded = store.open_channel(&era, CHANNEL_ID).unwrap().unwrap();
    seeded.set_binding_checkpoint(3).unwrap();

    let provider = ProviderKind::Claude;
    let session_key = crate::services::discord::adk_session::build_adk_session_key(
        &shared, channel, &provider, None,
    )
    .await
    .expect("a namespaced session key");
    let _switches = rig
        .bind(
            &pool,
            &shared.token_hash,
            &session_key,
            herdr::HostedState::Bound,
        )
        .await;
    let record = rig.record(&shared.token_hash, &session_key);
    let token_a = test_turn::start(&shared, channel, 51, "turn-a").await;

    // The listener runs its initial reconcile before anything else can wake it.
    crate::services::discord::queue_io::spawn_turn_completion_idle_queue_listener(
        shared.clone(),
        provider.clone(),
    );
    assert!(
        wait_until(Duration::from_secs(15), || {
            Box::pin(async move { observed::listener_completed_reconciles(channel) >= 1 })
        })
        .await,
        "the listener finished its initial reconcile"
    );
    assert!(!observed::backstop_waiting(channel));

    let queued = recent_message_id(701);
    let window = Arc::new(Mutex::new(None));
    let (ctx, data, probe_data_shared) =
        (harness.ctx.clone(), harness.clone_data(), shared.clone());
    let (probe_window, probe_record) = (window.clone(), record.clone());
    probe_settlement_window(Box::new(move || {
        Box::pin(async move {
            let held = herdr::held();
            let event = serenity::FullEvent::Message {
                new_message: user_message(queued.get(), ORIGINAL),
            };
            let intake = router::handle_event(&ctx, &event, &data).await;
            assert!(intake.is_ok(), "{intake:?}");
            let delivered = wait_until(Duration::from_secs(15), || {
                let shared = probe_data_shared.clone();
                Box::pin(async move {
                    crate::services::discord::mailbox_snapshot(&shared, channel)
                        .await
                        .intervention_queue
                        .iter()
                        .any(|item| item.message_id == queued)
                })
            })
            .await;
            let snapshot =
                crate::services::discord::mailbox_snapshot(&probe_data_shared, channel).await;
            let queue: Vec<_> = snapshot
                .intervention_queue
                .iter()
                .map(|item| {
                    (
                        item.message_id,
                        item.source_message_ids.clone(),
                        item.text.clone(),
                    )
                })
                .collect();
            *probe_window.lock().unwrap() =
                Some((held, delivered, queue, herdr::input_pin(&probe_record)));
        })
    }));
    let listener_first = Arc::new(AtomicBool::new(false));
    let probe_listener_first = listener_first.clone();
    probe_after_settlement(Box::new(move || {
        Box::pin(async move {
            // The listener takes QueueEligible while the transition is still held: its kickoff
            // must not start, and the queued message is handed to the event backstop.
            let waited = wait_until(Duration::from_secs(15), || {
                Box::pin(async move { observed::backstop_waiting(channel) })
            })
            .await;
            probe_listener_first.store(waited, Ordering::SeqCst);
        })
    }));

    let result = terminate_explicit_herdr(
        OperatorTerminate {
            session_key: session_key.clone(),
            execution_nonce: herdr::EXECUTION.into(),
        },
        shared.clone(),
        &pool,
    )
    .await;
    assert_eq!(result, TerminationResult::Retired);
    let (held_in_window, delivered, queue, pin) = window
        .lock()
        .unwrap()
        .take()
        .expect("the settlement window ran");
    assert!(!held_in_window, "the window opens after the hold release");
    assert!(delivered, "intake queued the message inside the window");
    assert_eq!(
        queue,
        vec![(queued, vec![queued], ORIGINAL.to_string())],
        "the original text and its source identity wait in the queue"
    );
    assert!(pin.is_err(), "the execution fence refuses input: {pin:?}");
    assert_eq!(
        rig.writes(),
        [("pane.close".to_string(), "w1-1".to_string())],
        "nothing but the close reached Herdr before settlement"
    );
    assert_eq!(harness.provider_starts(), 0);
    assert!(
        test_turn::active(&shared, channel).await.is_none(),
        "A was settled and nothing else holds the channel"
    );
    assert!(!token_a.cancelled.load(Ordering::Relaxed));

    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    // The queue consumer starts the queued message on Herdr: one launch, one typed command.
    let started = wait_until(START_WAIT, || {
        let writes = rig.writes();
        Box::pin(async move {
            writes
                .iter()
                .any(|(method, pane)| method == "pane.send_input" && pane == herdr::LAUNCHED_PANE)
        })
    })
    .await;
    assert!(
        started,
        "no Herdr start followed settlement: {:?}",
        rig.writes()
    );
    // A late event or backstop pass starts nothing more.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let writes = rig.writes();
    let count = |wanted: &str| writes.iter().filter(|(method, _)| method == wanted).count();
    assert_eq!(count("workspace.create"), 1, "{writes:?}");
    assert_eq!(count("pane.send_input"), 1, "{writes:?}");
    assert_eq!(
        writes.len(),
        3,
        "close, launch and its command only: {writes:?}"
    );
    // That launch is the queued message's turn: it answers that message and consumed it.
    assert!(
        harness
            .messages()
            .iter()
            .any(|(reply_to, _)| *reply_to == Some(queued.get())),
        "the started turn answers the queued message: {:?}",
        harness.messages()
    );
    let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel).await;
    assert!(
        snapshot.intervention_queue.is_empty(),
        "consumed from the queue"
    );
    assert!(
        harness.durable_queue().is_empty(),
        "consumed from the durable queue"
    );
    let row = herdr::row(&pool, &session_key).await;
    assert!(
        row.as_ref()
            .is_some_and(|(_, nonce)| nonce != herdr::EXECUTION),
        "the launch installed a new execution: {row:?}"
    );
    assert_launch_typed_its_script(&rig, &record.owner.logical_key);
    assert_eq!(
        harness.provider_starts(),
        0,
        "no other driver took the turn"
    );
    assert!(
        listener_first.load(Ordering::SeqCst),
        "the listener took QueueEligible and handed the no-start to the event backstop before \
         the transition was released"
    );
    drop(harness);
    pool.close().await;
    database.drop().await;
}

/// The launch's one input typed, with Enter, the launch script prepared for session `tmux`.
fn assert_launch_typed_its_script(rig: &herdr::P11TerminationRig, tmux: &str) {
    let writes = rig.writes_with_params();
    let typed: Vec<_> = writes
        .iter()
        .filter(|(method, params)| {
            method == "pane.send_input" && params["pane_id"] == herdr::LAUNCHED_PANE
        })
        .map(|(_, params)| params)
        .collect();
    let [params] = typed.as_slice() else {
        panic!("one launch input: {writes:?}");
    };
    assert_eq!(params["keys"], serde_json::json!(["enter"]), "{params}");
    let ext = crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT;
    let script = crate::services::tmux_common::session_temp_path(tmux, ext);
    let typed = format!("bash {}", crate::services::process::shell_escape(&script));
    assert_eq!(params["text"], typed.as_str(), "{params}");
    let body = std::fs::read_to_string(&script).unwrap_or_default();
    assert!(
        body.contains("\nexec "),
        "the script launches the provider: {body}"
    );
}

/// A registered Herdr channel whose execution was retired, with no channel session or one
/// without a name: its first catch-up input starts once on Herdr through the real kickoff.
fn registered_retired_case(path: &str, nameless_session: bool) {
    if !herdr::in_child(path, CHANNEL_ID) {
        return;
    }
    let rig = herdr::P11TerminationRig::boot(CHANNEL_ID);
    let install = rig.thread_installer();
    install();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .on_thread_start(install)
        .build()
        .unwrap()
        .block_on(registered_retired(rig, nameless_session));
}

#[test]
fn m2q_registered_herdr_channel_without_session_starts_its_input_once_pg() {
    registered_retired_case(
        concat!(
            module_path!(),
            "::m2q_registered_herdr_channel_without_session_starts_its_input_once_pg"
        ),
        false,
    );
}

#[test]
fn m2q_registered_herdr_channel_without_channel_name_starts_its_input_once_pg() {
    registered_retired_case(
        concat!(
            module_path!(),
            "::m2q_registered_herdr_channel_without_channel_name_starts_its_input_once_pg"
        ),
        true,
    );
}

async fn registered_retired(rig: herdr::P11TerminationRig, nameless_session: bool) {
    use super::registered_bootstrap_e2e::{
        FALLBACK, kick_off, recent_message_id, register_channel,
    };
    let mut database = None;
    let harness = RelayE2eHarness::start_unbound_on(async {
        let fixture = TestPostgresDb::create().await;
        let pool = fixture.connect_and_migrate().await;
        database = Some(fixture);
        pool
    })
    .await;
    let database = database.expect("an isolated PostgreSQL database");
    let pool = harness.shared.pg_pool.clone().expect("PG-backed runtime");
    let (shared, channel) = (harness.shared.clone(), harness.channel_id);
    register_channel(&harness);
    harness.answer_placeholders_immediately();
    std::mem::forget(
        crate::services::claude_tui::hook_server::publish_hook_endpoint(
            "http://127.0.0.1:1".into(),
        ),
    );
    let o_store = crate::services::herdr_launch::o_store_for_test;
    let (store, era) = o_store(harness.root.path(), &[CHANNEL_ID]);
    let mut seeded = store.open_channel(&era, CHANNEL_ID).unwrap().unwrap();
    seeded.set_binding_checkpoint(3).unwrap();
    let provider = ProviderKind::Claude;
    let tmux = provider.build_tmux_session_name(FALLBACK);
    let build = crate::services::discord::adk_session::build_namespaced_session_key;
    let session_key = build(&shared.token_hash, &provider, &tmux);
    let retired = herdr::HostedState::Retired;
    let _switches = rig
        .bind(&pool, &shared.token_hash, &session_key, retired)
        .await;

    let id = recent_message_id();
    harness.seed_channel_history(&[(id, ORIGINAL, false)]);
    harness.run_catch_up().await;
    assert!(
        shared.core.lock().await.sessions.is_empty(),
        "no channel session"
    );
    if nameless_session {
        let map = crate::services::discord::host_defer_gate::tests::map_channel;
        map(&shared, channel, FALLBACK).await;
        let mut core = shared.core.lock().await;
        core.sessions.get_mut(&channel).unwrap().channel_name = None;
    }
    let queued: Vec<_> = harness
        .mailbox()
        .await
        .intervention_queue
        .iter()
        .map(|i| i.message_id.get())
        .collect();
    assert_eq!(queued, vec![id], "catch-up queued the input");
    assert!(
        kick_off(&harness).await,
        "the retired row lets the kickoff start the input"
    );

    let started = wait_until(Duration::from_secs(60), || {
        let writes = rig.writes();
        Box::pin(async move {
            writes
                .iter()
                .any(|(method, pane)| method == "pane.send_input" && pane == herdr::LAUNCHED_PANE)
        })
    })
    .await;
    assert!(started, "no Herdr start: {:?}", rig.writes());
    tokio::time::sleep(Duration::from_secs(3)).await;
    let writes = rig.writes();
    let methods: Vec<&str> = writes.iter().map(|(method, _)| method.as_str()).collect();
    assert_eq!(
        methods,
        ["workspace.create", "pane.send_input"],
        "{writes:?}"
    );
    assert_launch_typed_its_script(&rig, &tmux);
    assert!(
        harness
            .messages()
            .iter()
            .any(|(reply_to, _)| *reply_to == Some(id)),
        "the started turn answers the input: {:?}",
        harness.messages()
    );
    assert!(
        harness.mailbox().await.intervention_queue.is_empty(),
        "consumed from the queue"
    );
    assert!(
        harness.durable_queue().is_empty(),
        "consumed from the durable queue"
    );
    let row = herdr::row(&pool, &session_key).await;
    assert!(
        row.as_ref()
            .is_some_and(|(_, nonce)| nonce != herdr::EXECUTION),
        "the launch installed a new execution: {row:?}"
    );
    assert_eq!(
        harness.provider_starts(),
        0,
        "no other driver took the turn"
    );
    drop(harness);
    pool.close().await;
    database.drop().await;
}

/// After a restart, a registered Herdr channel with no channel session whose execution is Bound
/// and idle: the real kickoff takes its queued input to that execution once, with no local tmux.
#[test]
fn a_restarted_bound_herdr_channel_prompts_its_queued_input_once_pg() {
    let path = concat!(
        module_path!(),
        "::a_restarted_bound_herdr_channel_prompts_its_queued_input_once_pg"
    );
    run_bound(path, false);
}

/// A mapped Bound channel whose Herdr turn switch is off: the refused kickoff leaves the queued
/// input in place, and once the switch is on the same execution gets it once.
#[test]
fn a_refused_herdr_kickoff_keeps_its_queued_input_for_the_switch_pg() {
    let path = concat!(
        module_path!(),
        "::a_refused_herdr_kickoff_keeps_its_queued_input_for_the_switch_pg"
    );
    run_bound(path, true);
}

fn run_bound(path: &str, refused_first: bool) {
    if !herdr::in_child(path, CHANNEL_ID) {
        return;
    }
    let rig = herdr::P11TerminationRig::boot(CHANNEL_ID);
    let install = rig.thread_installer();
    install();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .on_thread_start(install)
        .build()
        .unwrap()
        .block_on(registered_bound(rig, refused_first));
}

async fn registered_bound(rig: herdr::P11TerminationRig, refused_first: bool) {
    use super::registered_bootstrap_e2e::{
        FALLBACK, kick_off, recent_message_id, register_channel,
    };
    use crate::services::claude_tui::hook_server::observation_ingress::tests::{claude, uuid};
    use serde_json::json;
    let mut database = None;
    let harness = RelayE2eHarness::start_unbound_on(async {
        let fixture = TestPostgresDb::create().await;
        let pool = fixture.connect_and_migrate().await;
        database = Some(fixture);
        pool
    })
    .await;
    let database = database.expect("an isolated PostgreSQL database");
    let pool = harness.shared.pg_pool.clone().expect("PG-backed runtime");
    let (shared, channel) = (harness.shared.clone(), harness.channel_id);
    register_channel(&harness);
    harness.answer_placeholders_immediately();
    let fake_tmux = crate::services::tui_prompt_dedupe::binding_context::tests::fake_tmux;
    let _tmux = fake_tmux(harness.root.path());
    let o_store = crate::services::herdr_launch::o_store_for_test;
    let (store, era) = o_store(harness.root.path(), &[CHANNEL_ID]);
    let mut seeded = store.open_channel(&era, CHANNEL_ID).unwrap().unwrap();
    seeded.set_binding_checkpoint(3).unwrap();
    let provider = ProviderKind::Claude;
    let tmux = provider.build_tmux_session_name(FALLBACK);
    let build = crate::services::discord::adk_session::build_namespaced_session_key;
    let session_key = build(&shared.token_hash, &provider, &tmux);
    let bound = herdr::HostedState::Bound;
    let _switches = rig
        .bind(&pool, &shared.token_hash, &session_key, bound)
        .await;
    // The execution's last prompt settled before the restart, so no input hold remains.
    crate::services::claude::herdr_turn::release_hold(herdr::EXECUTION);
    let pane = rig
        .record(&shared.token_hash, &session_key)
        .location
        .unwrap()
        .pane_id;
    // The restored source: an idle transcript whose last turn ended.
    let session = uuid();
    let transcript = harness.root.path().join(format!("{session}.jsonl"));
    let append = |lines: &[serde_json::Value]| {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&transcript)
            .unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    };
    let answered = |text: &str| {
        let answer = json!({"type": "assistant", "sessionId": session, "message": {
            "role": "assistant", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": text}]}});
        let done = json!({"type": "system", "subtype": "turn_duration", "sessionId": session});
        [answer, done]
    };
    append(&answered("earlier"));
    let register = crate::services::tui_prompt_dedupe::register_tmux_runtime_binding;
    register(&tmux, claude(&transcript, &session));

    let id = recent_message_id();
    harness.seed_channel_history(&[(id, ORIGINAL, false)]);
    harness.run_catch_up().await;
    assert!(
        shared.core.lock().await.sessions.is_empty(),
        "no channel session"
    );
    assert!(shared.tmux_watchers.get(&channel).is_none(), "no watcher");
    let queued: Vec<_> = harness
        .mailbox()
        .await
        .intervention_queue
        .iter()
        .map(|i| i.message_id.get())
        .collect();
    assert_eq!(queued, vec![id], "catch-up queued the input");
    let sends = || -> Vec<(String, serde_json::Value)> {
        let all = rig.writes_with_params();
        all.into_iter()
            .filter(|(method, _)| method.starts_with("pane.send"))
            .collect()
    };
    // The tmux availability probe (`-V`) reads no session; nothing else may reach local tmux.
    let tmux_calls = || -> Vec<String> {
        let log = harness.root.path().join("tmux.calls");
        let calls = std::fs::read_to_string(log).unwrap_or_default();
        let acted = calls.lines().filter(|call| !call.ends_with("-V"));
        acted.map(str::to_owned).collect()
    };
    if refused_first {
        let map = crate::services::discord::host_defer_gate::tests::map_channel;
        map(&shared, channel, FALLBACK).await;
        let turn_switch = |on| {
            let mut config = (*crate::config_live_reload::current().unwrap()).clone();
            config.runtime.herdr_turn_enabled = Some(on);
            crate::config_live_reload::install(config);
        };
        turn_switch(false);
        let input = |queue: &[crate::services::discord::Intervention]| -> Vec<_> {
            let fields = |i: &crate::services::discord::Intervention| {
                let sources = i.source_message_ids.clone();
                (
                    i.message_id,
                    i.text.clone(),
                    sources,
                    i.pending_uploads.clone(),
                )
            };
            queue.iter().map(fields).collect()
        };
        let before = input(&harness.durable_queue());
        assert!(!kick_off(&harness).await, "the refused kickoff started");
        let held = harness.mailbox().await.intervention_queue;
        assert_eq!(input(&held), before, "refused input left the queue");
        assert_eq!(
            input(&harness.durable_queue()),
            before,
            "left the durable queue"
        );
        assert_eq!(sends(), Vec::new(), "the refused kickoff prompted");
        assert_eq!(tmux_calls(), Vec::<String>::new(), "no local tmux call");
        assert_eq!(
            harness.provider_starts(),
            0,
            "no driver took the refused turn"
        );
        turn_switch(true);
    }
    let started = kick_off(&harness).await;
    let depth = harness.mailbox().await.intervention_queue.len();
    assert!(started, "the kickoff held the queue at depth {depth}");

    let prompted = wait_until(Duration::from_secs(60), || {
        let done = sends().len() >= 2 || !tmux_calls().is_empty();
        Box::pin(async move { done })
    })
    .await;
    assert_eq!(tmux_calls(), Vec::<String>::new(), "no local tmux call");
    assert!(prompted, "no Herdr prompt: {:?}", rig.writes());
    let user = json!({"type": "user", "sessionId": session,
        "message": {"role": "user", "content": ORIGINAL}});
    append(&[user]);
    append(&answered("answer"));
    tokio::time::sleep(Duration::from_secs(3)).await;
    let sends = sends();
    let methods: Vec<&str> = sends.iter().map(|(method, _)| method.as_str()).collect();
    assert_eq!(methods, ["pane.send_text", "pane.send_keys"], "{sends:?}");
    assert!(
        sends
            .iter()
            .all(|(_, params)| params["pane_id"] == pane.as_str()),
        "both sends reach the bound execution's pane: {sends:?}"
    );
    let text = sends[0].1["text"].as_str().unwrap_or_default();
    assert!(
        text.contains(ORIGINAL),
        "the paste carries the input: {text}"
    );
    assert_eq!(sends[1].1["keys"], json!(["enter"]), "{sends:?}");
    assert!(
        rig.writes()
            .iter()
            .all(|(method, _)| method != "workspace.create"),
        "nothing launched: {:?}",
        rig.writes()
    );
    assert!(
        harness.mailbox().await.intervention_queue.is_empty(),
        "consumed from the queue"
    );
    assert!(
        harness.durable_queue().is_empty(),
        "consumed from the durable queue"
    );
    let row = herdr::row(&pool, &session_key).await;
    assert_eq!(
        row,
        Some((bound, herdr::EXECUTION.to_string())),
        "the same execution stays bound"
    );
    assert_eq!(
        harness.provider_starts(),
        0,
        "no other driver took the turn"
    );
    assert_eq!(tmux_calls(), Vec::<String>::new(), "no local tmux call");
    drop(harness);
    pool.close().await;
    database.drop().await;
}
