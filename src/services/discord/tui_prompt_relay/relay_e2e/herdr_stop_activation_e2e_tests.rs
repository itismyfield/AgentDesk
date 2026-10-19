//! An activated `/stop` on a bound Herdr turn, end to end: intake starts the turn, the bridge holds
//! its row and token until the provider's own abort, then the finalizer frees the channel and the
//! queue starts the input that waited behind it, once.

use std::time::Duration;

use serde_json::{Value, json};

use super::registered_bootstrap_e2e::{FALLBACK, kick_off, recent_message_id, register_channel};
use super::{CHANNEL_ID, RelayE2eHarness, wait_until};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::inflight;
use crate::services::provider::ProviderKind;
use crate::services::provider::cancel_token_claude_interrupt::{
    HERDR_CANCEL_OVERRIDE, herdr_cancel_enabled,
};
use crate::services::session_host::herdr_termination_rig as herdr;

const FIRST: &str = "first input the stop ends [herdr-activation]";
const SENTINEL: &str = "sentinel queued behind the stopped turn [herdr-activation]";

#[test]
fn an_activated_stop_settles_through_the_bridge_and_starts_the_queued_input_once_pg() {
    let path = concat!(
        module_path!(),
        "::an_activated_stop_settles_through_the_bridge_and_starts_the_queued_input_once_pg"
    );
    if !herdr::in_child(path, CHANNEL_ID) {
        return;
    }
    let rig = herdr::P11TerminationRig::boot(CHANNEL_ID);
    // Test builds read and write the binding log only under a root set on the running thread.
    let events = tempfile::tempdir().unwrap();
    let (registry, root) = (rig.thread_installer(), events.path().to_path_buf());
    let install = move || {
        registry();
        crate::services::tui_prompt_dedupe::binding_events::set_test_root(Some(&root));
    };
    install();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .on_thread_start(install)
        .build()
        .unwrap()
        .block_on(scenario(rig, events.path()));
}

/// The canonical Source the execution's SessionStart logged when it was launched.
fn log_session_start(events: &std::path::Path, tmux: &str, session: &str, path: &std::path::Path) {
    use crate::services::tui_prompt_dedupe::binding_events as log;
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::metadata(path).unwrap();
    let event = log::BindingEvent {
        seq: 1,
        channel_id: CHANNEL_ID,
        provider: ProviderKind::Claude.as_str().into(),
        tmux_session: tmux.into(),
        execution_nonce: Some(herdr::EXECUTION.into()),
        old: None,
        new: log::BindingTarget::Source(log::SourceId {
            session_id: session.into(),
            path: path.to_path_buf(),
            dev: meta.dev(),
            ino: meta.ino(),
        }),
        cause: log::BindingCause::Startup,
        parent_hint: None,
        evidence: log::BindingEvidence {
            hook_event: Some("SessionStart".into()),
            received_at: chrono::Utc::now(),
        },
        committed_at: chrono::Utc::now(),
    };
    let dir = events.join(log::BINDING_EVENTS_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    let line = serde_json::to_string(&event).unwrap() + "\n";
    std::fs::write(dir.join(format!("{CHANNEL_ID}.log")), line).unwrap();
}

async fn scenario(rig: herdr::P11TerminationRig, events: &std::path::Path) {
    use crate::services::claude_tui::hook_server::observation_ingress::tests::{claude, uuid};
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
    crate::services::claude::herdr_turn::release_hold(herdr::EXECUTION);
    // Only the live config enables the Escape; no test override decides it.
    let mut config = (*crate::config_live_reload::current().unwrap()).clone();
    config.runtime.herdr_cancel_enabled = Some(true);
    crate::config_live_reload::install(config);
    assert_eq!(HERDR_CANCEL_OVERRIDE.with(std::cell::Cell::get), None);
    assert!(herdr_cancel_enabled(), "the configured switch alone");

    let session = uuid();
    let transcript = harness.root.path().join(format!("{session}.jsonl"));
    let append = |lines: &[Value]| {
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
    let stamped = |mut line: Value| {
        line["timestamp"] = json!(chrono::Utc::now().to_rfc3339());
        line["sessionId"] = json!(session);
        line
    };
    let done = json!({"type": "system", "subtype": "turn_duration"});
    append(&[
        stamped(json!({"type": "assistant", "message": {"role": "assistant",
            "stop_reason": "end_turn", "content": [{"type": "text", "text": "earlier"}]}})),
        stamped(done.clone()),
    ]);
    let register = crate::services::tui_prompt_dedupe::register_tmux_runtime_binding;
    register(&tmux, claude(&transcript, &session));
    log_session_start(events, &tmux, &session, &transcript);

    let first = recent_message_id();
    harness.seed_channel_history(&[(first, FIRST, false)]);
    harness.run_catch_up().await;
    let sends = || -> Vec<(String, Value)> {
        let all = rig.writes_with_params();
        all.into_iter()
            .filter(|(method, _)| method.starts_with("pane.send"))
            .collect()
    };
    let prompts_of = |text: &str| {
        let pasted = |(method, params): &(String, Value)| {
            method == "pane.send_text"
                && params["text"]
                    .as_str()
                    .is_some_and(|sent| sent.contains(text))
        };
        sends().iter().filter(|send| pasted(send)).count()
    };
    assert!(kick_off(&harness).await, "the first input starts");
    let prompted = wait_until(Duration::from_secs(60), || {
        let done = prompts_of(FIRST) == 1;
        Box::pin(async move { done })
    })
    .await;
    assert!(prompted, "no Herdr prompt: {:?}", rig.writes());

    // The sentinel arrives while the first turn runs and waits in the queue.
    harness
        .deliver_user_message(first + 1, SENTINEL)
        .await
        .unwrap();
    let queued: Vec<_> = harness
        .mailbox()
        .await
        .intervention_queue
        .iter()
        .map(|i| i.text.clone())
        .collect();
    assert!(
        queued.iter().any(|text| text.contains(SENTINEL)),
        "{queued:?}"
    );
    append(&[stamped(
        json!({"type": "user", "message": {"role": "user", "content": FIRST}}),
    )]);
    let live = harness.mailbox().await.cancel_token.expect("the live turn");
    let stop = crate::services::discord::turn_bridge::begin_user_stop;
    let stopped = match stop(&shared, &provider, channel, true, "/stop").await {
        crate::services::discord::turn_bridge::CommandStop::Herdr(stop) => format!("{stop:?}"),
        _ => "not a Herdr stop".into(),
    };
    assert!(stopped.starts_with("Requested("), "{stopped}");

    // Until its own terminal the turn keeps its row and token, and nothing else is prompted.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let row = inflight::load_inflight_state(&provider, channel.get()).expect("the turn's row");
    assert_eq!(row.turn_nonce.as_deref(), live.turn_nonce());
    let current = harness.mailbox().await.cancel_token;
    assert!(current.is_some_and(|current| std::sync::Arc::ptr_eq(&current, &live)));
    assert_eq!(prompts_of(SENTINEL), 0, "{:?}", sends());

    // The provider's own abort ends it; the bridge's finalizer frees the channel and the queue
    // prompts the sentinel once, well before the queue's 60s backstop would.
    let interrupt = json!({"type": "user", "message": {"role": "user",
        "content": [{"type": "text", "text": "[Request interrupted by user]"}]}});
    append(&[stamped(interrupt), stamped(done)]);
    let next = wait_until(Duration::from_secs(30), || {
        let done = prompts_of(SENTINEL) >= 1;
        Box::pin(async move { done })
    })
    .await;
    let after = inflight::load_inflight_state(&provider, channel.get());
    assert!(
        after
            .as_ref()
            .is_none_or(|row| row.turn_nonce.as_deref() != live.turn_nonce()),
        "the stopped turn's row is cleared: {after:?}"
    );
    assert!(next, "the sentinel was not prompted: {:?}", sends());
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(prompts_of(SENTINEL), 1, "{:?}", sends());
    assert_eq!(prompts_of(FIRST), 1, "{:?}", sends());
    assert!(
        harness.durable_queue().is_empty(),
        "consumed from the queue"
    );
    drop(harness);
    pool.close().await;
    database.drop().await;
}
