use super::*;
use crate::server::outbox_worker::drain_message_outbox_once;
use crate::services::agent_protocol::RuntimeHandoffKind::{
    self, ClaudeTui, CodexTui, ProcessBackend,
};
use crate::services::discord::bot_role::UtilityBotRole;
use crate::services::provider::ProviderKind::{Claude, Codex};
use crate::services::routines::{RoutineDiscordLogger, RoutineLifecycleEvent};
use crate::services::tui_o::{channel_policy::Adoption, cutover};
use poise::serenity_prelude::Channel;

const ANSWER: &str = "routine result delivered to its selected thread";
const PROCESS_SESSION: &str = "81170000-0000-4000-8000-000000000001";

#[derive(Clone, Copy)]
enum ThreadOrder {
    MessageBeforeRun,
    RunBeforeMessage,
    CreateAtRun,
}

fn test_child(name: &str) -> bool {
    api_child(&format!("{TEST}routine_result_process_tests::{name}"))
}

fn thread_lookup(fixture: &Fixture) -> cutover::test_override::LookupsGuard {
    let guard = cutover::test_override::force_channel_lookups();
    let http = fixture
        .shared
        .http
        .cached_serenity_ctx
        .get()
        .unwrap()
        .http
        .clone();
    cutover::install_channel_lookup(
        fixture.shared.provider.as_str(),
        Arc::new(move |channel| {
            let http = http.clone();
            Box::pin(async move {
                let channel = ChannelId::new(channel)
                    .to_channel(&http)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(match channel {
                    Channel::Guild(channel) => Some(channel),
                    _ => None,
                })
            })
        }),
    );
    guard
}

async fn thread_creation(fixture: &Fixture) -> (Recorder, Arc<HealthRegistry>) {
    let mock = Recorder::start_with_status(Arc::new(|method, uri| {
        let created = method == axum::http::Method::POST
            && uri.path() == format!("/api/v10/channels/{}/threads", PRIMARY.get());
        created.then(|| {
            let thread = json!({
                "id": THREAD.get().to_string(), "parent_id": PRIMARY.get().to_string(),
                "type": 11, "name": "routine", "guild_id": "940487400000009"
            });
            (axum::http::StatusCode::CREATED, thread)
        })
    }))
    .await;
    let registry = Arc::new(HealthRegistry::new());
    let provider = fixture.shared.provider.as_str();
    registry
        .register(provider.to_owned(), fixture.shared.clone())
        .await;
    registry
        .register_http(provider.to_owned(), mock.http.clone())
        .await;
    (mock, registry)
}

fn assert_deferred(kind: RuntimeHandoffKind) {
    let boot = cutover::boot_snapshot().expect("writer selection");
    let child = boot
        .child(THREAD.get())
        .expect("recorded child")
        .expect("verified child");
    assert_eq!(child.parent, PRIMARY.get());
    assert_eq!(boot.kind(THREAD.get()), Some(kind));
    assert!(child.candidate.late_adopt_origin());
    assert_eq!(child.candidate.peek(), Adoption::Deferred);
}

async fn publish_process(fixture: &Fixture, kind: RuntimeHandoffKind, order: ThreadOrder) {
    let api = real_api(fixture).await;
    let http = fixture
        .shared
        .http
        .cached_serenity_ctx
        .get()
        .unwrap()
        .http
        .clone();
    fixture
        .registry
        .set_utility_bot_http_for_tests(UtilityBotRole::Notify, http)
        .await;
    fixture.discord.unname_channel(THREAD.get());
    fixture.discord.add_thread(THREAD.get(), PRIMARY.get());
    let _lookups = thread_lookup(fixture);
    let (creator, creation_registry) = thread_creation(fixture).await;
    let pool = Arc::new(fixture.pool.clone());
    let (store, routine) = attach_routine(fixture, false, None, (0, None)).await;

    if !matches!(order, ThreadOrder::CreateAtRun) {
        let logger = RoutineDiscordLogger::new_with_health_registry(
            pool.clone(),
            Some(creation_registry.clone()),
        );
        let attached = logger
            .log_routine_event_by_id(&store, &routine.id, RoutineLifecycleEvent::Attached)
            .await;
        assert_eq!(attached.status, "ok", "{attached:?}");
    }
    if matches!(order, ThreadOrder::MessageBeforeRun) {
        // The message event uses this checked child recorder before filtering bot messages.
        assert!(
            cutover::record_thread_child_by_id(THREAD.get(), PRIMARY.get())
                .await
                .unwrap()
        );
        assert_deferred(kind);
    }
    let claimed = store
        .claim_run_now(&routine.id)
        .await
        .unwrap()
        .expect("run-now claim");
    let run = claimed.run_id.clone();
    let registry = if matches!(order, ThreadOrder::CreateAtRun) {
        creation_registry
    } else {
        fixture.registry.clone()
    };
    let executor = RoutineAgentExecutor::new(pool.clone(), Some(registry), 60)
        .with_headless_fresh_agents(true);
    let launched = executor
        .start_agent_run(
            &store,
            claimed,
            "nightly report".to_string(),
            None,
            None,
            None,
            None,
            false,
        )
        .await
        .unwrap();
    assert_eq!(launched.status, "running", "{launched:?}");
    let result = launched.result_json.unwrap();
    let provider = &fixture.shared.provider;
    assert_eq!(result["provider"], provider.as_str(), "{result}");
    assert_eq!(result["execution_mode"], "process", "{result}");
    assert_eq!(
        result["discord_thread_id"],
        THREAD.get().to_string(),
        "{result}"
    );
    let creates = creator.take();
    assert_eq!(
        creates
            .iter()
            .filter(|call| call.starts_with("POST ") && call.contains("/threads "))
            .count(),
        1
    );
    assert_deferred(kind);
    let before =
        load_inflight_state_read_only(provider, THREAD.get()).expect("routine producer held open");
    assert_eq!(
        before.born_generation,
        fixture.shared.restart.current_generation
    );
    assert!(before.tmux_session_name.is_none() && before.input_fifo_path.is_none());
    assert!(
        fixture
            .discord
            .delivered_texts()
            .iter()
            .all(|text| !text.contains(ANSWER))
    );

    std::fs::write(fixture.stubs.join("answer"), ANSWER).unwrap();
    std::fs::write(fixture.stubs.join("release"), "").unwrap();
    let outbox = async {
        loop {
            let row: Option<(i64, String)> = sqlx::query_as(
                "SELECT id, target FROM message_outbox WHERE source='headless_turn' AND content LIKE $1",
            )
            .bind(format!("%{ANSWER}%"))
            .fetch_optional(pool.as_ref()).await.unwrap();
            if row.is_some() {
                break row;
            }
            if !turn_active(fixture, THREAD).await
                && fixture
                    .shared
                    .restart
                    .finalizing_turns
                    .load(std::sync::atomic::Ordering::Relaxed)
                    == 0
            {
                break None;
            }
            tokio::task::yield_now().await;
        }
    };
    let (id, target) = await_within(outbox, WAIT_CAP)
        .await
        .expect("terminal process answer reached the delivery outbox");
    assert_eq!(target, format!("channel:{}", THREAD.get()));
    assert!(drain_message_outbox_once(&pool, &fixture.registry, "routine-result-worker").await > 0);
    turn_ended(fixture, THREAD).await;
    let sent: bool = sqlx::query_scalar(
        "SELECT status='sent' AND sent_at IS NOT NULL FROM message_outbox WHERE id=$1",
    )
    .bind(id)
    .fetch_one(pool.as_ref())
    .await
    .unwrap();
    assert!(sent, "wire delivery must acknowledge the outbox");
    let texts = fixture.discord.delivered_texts();
    assert_eq!(
        texts.iter().filter(|text| text.contains(ANSWER)).count(),
        1,
        "{texts:?}"
    );
    assert_eq!(before.runtime_kind, Some(ProcessBackend));
    assert!(load_inflight_state_read_only(provider, THREAD.get()).is_none());
    assert_eq!(
        std::fs::read_to_string(fixture.stubs.join("spawned"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let ended = executor.poll_agent_runs(&store, 10, false).await.unwrap();
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(
        (ended[0].run_id.as_str(), ended[0].status.as_str()),
        (run.as_str(), "succeeded")
    );
    api.abort();
}

#[tokio::test]
async fn routine_process_claude_publishes_selected_thread_pg() {
    if !test_child("routine_process_claude_publishes_selected_thread_pg") {
        return;
    }
    let _boot = cutover::test_override::force_candidates(&[(PRIMARY.get(), ClaudeTui)]);
    let fixture = fixture_for(Claude, true).await;
    publish_process(&fixture, ClaudeTui, ThreadOrder::MessageBeforeRun).await;
    fixture.db.drop().await;
}

#[tokio::test]
async fn routine_process_codex_publishes_selected_thread_pg() {
    if !test_child("routine_process_codex_publishes_selected_thread_pg") {
        return;
    }
    let _boot = cutover::test_override::force_candidates(&[(PRIMARY.get(), CodexTui)]);
    let fixture = fixture_for(Codex, true).await;
    publish_process(&fixture, CodexTui, ThreadOrder::MessageBeforeRun).await;
    fixture.db.drop().await;
}

#[tokio::test]
async fn routine_process_records_thread_before_message_event_pg() {
    if !test_child("routine_process_records_thread_before_message_event_pg") {
        return;
    }
    for order in [ThreadOrder::RunBeforeMessage, ThreadOrder::CreateAtRun] {
        let _boot = cutover::test_override::force_candidates(&[(PRIMARY.get(), ClaudeTui)]);
        let fixture = fixture_for(Claude, true).await;
        publish_process(&fixture, ClaudeTui, order).await;
        assert!(
            !cutover::record_thread_child_by_id(THREAD.get(), PRIMARY.get())
                .await
                .unwrap()
        );
        assert_deferred(ClaudeTui);
        fixture.db.drop().await;
    }
}

async fn reply_selection(
    fixture: &Fixture,
    native: &str,
    tmux: &str,
) -> (Option<String>, &'static str) {
    use crate::services::discord::router::message_handler::intake_turn::session_preparation::{
        Restore, restore,
    };
    use crate::services::platform::tmux::PaneLiveness;
    let _gone = crate::services::tmux_diagnostics::PaneLivenessOverrideGuard::set(
        tmux,
        PaneLiveness::DeadOrAbsent,
    );
    let (mut session_id, mut memory, mut reason) = (Some(native.to_owned()), true, "cached");
    restore(
        &fixture.shared,
        &Claude,
        THREAD,
        Restore {
            session_id: &mut session_id,
            memento_context_loaded: &mut memory,
            reason: &mut reason,
            force_fresh: false,
            cleared: false,
            reset: false,
            recreate: false,
            session_key: None,
            tmux: Some(tmux),
        },
    )
    .await;
    (session_id, reason)
}

#[tokio::test]
async fn routine_process_reply_cold_fresh_then_verified_resume_pg() {
    if !test_child("routine_process_reply_cold_fresh_then_verified_resume_pg") {
        return;
    }
    use crate::services::tui_o::shadow::binding_reader::source_id_for;
    use crate::services::tui_prompt_dedupe::binding_events as bindings;
    const VERIFIED: &str = "81170000-0000-4000-8000-000000000002";
    let _boot = cutover::test_override::force_candidates(&[(PRIMARY.get(), ClaudeTui)]);
    let fixture = fixture_for(Claude, true).await;
    publish_process(&fixture, ClaudeTui, ThreadOrder::MessageBeforeRun).await;
    bindings::set_test_root(Some(fixture._root.path()));
    let tmux = Claude.build_tmux_session_name("thread-agent");
    let outcome = reply_selection(&fixture, PROCESS_SESSION, &tmux).await;
    assert_eq!(outcome, (None, "thread_unverified_provider_session"));

    // The next human reply sees the session verified by the TUI hook.
    let transcript = fixture._root.path().join("verified-thread.jsonl");
    std::fs::write(&transcript, "{}\n").unwrap();
    let source = source_id_for(VERIFIED, &transcript).unwrap();
    let event = json!({
        "seq": 1, "channel_id": THREAD.get(), "provider": "claude", "tmux_session": tmux,
        "execution_nonce": "fixture", "old": null, "new": {"source": source},
        "cause": "same_session_verified", "parent_hint": null,
        "evidence": {"hook_event": "UserPromptSubmit", "received_at": chrono::Utc::now()},
        "committed_at": chrono::Utc::now(), "verified": true
    });
    let log = fixture._root.path().join(bindings::BINDING_EVENTS_DIR);
    std::fs::create_dir_all(&log).unwrap();
    std::fs::write(
        log.join(format!("{}.log", THREAD.get())),
        format!("{event}\n"),
    )
    .unwrap();
    let outcome = reply_selection(&fixture, VERIFIED, &tmux).await;
    assert_eq!(outcome, (Some(VERIFIED.to_owned()), "cached"));
    let notices: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM message_outbox WHERE target=$1 AND session_key LIKE 'claude_resume_unverified:%'",
    )
    .bind(format!("channel:{}", THREAD.get()))
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(notices, 1);
    fixture.db.drop().await;
}
