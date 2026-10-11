//! The intake start's replay check on the actual worker entry and exact-retry handoff.

use super::super::provider_dispatch::{
    INPUT_EFFECT_PROBE, INPUT_PROMPT_PROBE, InputEffectProbe, SubmittedPrompt,
};
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::replay_disposition::write::{self, BeginFrom, CanonicalInput, EffectProjection};
use crate::services::discord::admin_host_guard::tests::{Recorder, api_child};
use crate::services::discord::replay_policy::live::{RawObservation, Seen};
use crate::services::discord::replay_policy::permit::EffectTarget;
use crate::services::discord::replay_policy::{ExactHandoff, ExactRetry, RetryPermit};
use crate::services::discord::router::intake_dispatch::replay_admission::{
    input_hash, test_activation,
};
use std::time::Duration;

const STALE: &str = "stale-resume";

struct Host {
    api: Recorder,
    shared: Arc<SharedData>,
    pool: sqlx::PgPool,
    db: TestPostgresDb,
    _workspace: tempfile::TempDir,
    _root: crate::config::TestRuntimeRootGuard,
}

async fn host(channels: &[ChannelId]) -> Host {
    host_with(channels, Arc::new(|_: &axum::http::Method, _: &str| None)).await
}

/// How the runtime API stand-in answers one request ahead of its default body.
type Answer = Arc<dyn Fn(&axum::http::Method, &str) -> Option<serde_json::Value> + Send + Sync>;

async fn host_with(channels: &[ChannelId], answer: Answer) -> Host {
    let root = crate::config::TestRuntimeRootGuard::new();
    let boot = serde_json::from_value(serde_json::json!({"server": {}, "agents": []})).unwrap();
    crate::services::tui_o::channel_policy::install(&boot).unwrap();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let api = Recorder::start_with(answer).await;
    crate::services::discord::internal_api::init(api.port, None);
    let mut shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    Arc::get_mut(&mut shared).unwrap().api_port = api.port;
    let workspace = tempfile::tempdir().unwrap();
    for &channel in channels {
        crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, "").await;
        let mut core = shared.core.lock().await;
        let session = core.sessions.get_mut(&channel).unwrap();
        session.channel_name = Some(format!("replay-start-{channel}"));
        session.current_path = Some(workspace.path().display().to_string());
        session.session_id = Some(STALE.into());
    }
    Host {
        api,
        shared,
        pool,
        db,
        _workspace: workspace,
        _root: root,
    }
}

fn request(channel: ChannelId, message: u64, text: &str) -> IntakeRequest {
    IntakeRequest {
        intake_outbox_id: None,
        channel_id: channel,
        user_msg_id: MessageId::new(message),
        source_message_ids: vec![MessageId::new(message + 1)],
        busy_followup_retry_user_msg_id: MessageId::new(message),
        request_owner: UserId::new(7),
        request_owner_name: "owner".into(),
        user_text: text.into(),
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        merge_consecutive: false,
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: TurnKind::Foreground,
        preserve_on_cancel: false,
    }
}

/// Runs `entry`; returns what the provider received and the sender that releases it, or `None`
/// when nothing reached the provider.
async fn start<T>(
    channel: ChannelId,
    entry: impl std::future::Future<Output = T>,
) -> (T, Option<(SubmittedPrompt, std::sync::mpsc::Sender<()>)>) {
    let (capture, prompt) = tokio::sync::oneshot::channel();
    *INPUT_PROMPT_PROBE.lock().unwrap() = Some((channel.get(), capture));
    let (entered, _entered) = tokio::sync::oneshot::channel();
    let (release_provider, release) = std::sync::mpsc::channel();
    *INPUT_EFFECT_PROBE.lock().unwrap() = Some(InputEffectProbe {
        channel: channel.get(),
        entered,
        release,
        terminal_before_release: true,
    });
    let output = tokio::time::timeout(Duration::from_secs(60), entry)
        .await
        .unwrap();
    let submitted = tokio::time::timeout(Duration::from_secs(3), prompt).await;
    INPUT_PROMPT_PROBE.lock().unwrap().take();
    let started = submitted.ok().and_then(Result::ok);
    if started.is_none() {
        INPUT_EFFECT_PROBE.lock().unwrap().take();
    }
    (output, started.map(|prompt| (prompt, release_provider)))
}

async fn run(host: &Host, request: IntakeRequest) -> Option<SubmittedPrompt> {
    let channel = request.channel_id;
    let entry = execute_intake_turn_core(&host.api.http, &host.shared, "", request, Vec::new());
    let (result, started) = start(channel, entry).await;
    result.unwrap();
    started.map(|(prompt, release)| {
        let _ = release.send(());
        prompt
    })
}

/// The receipt stored under the Discord request key, and its current attempt and projection.
async fn receipt(host: &Host, channel: ChannelId, message: u64) -> Option<(i64, String, String)> {
    let key = format!("discord:claude:{channel}:{message}");
    sqlx::query_as(
        "SELECT id, replay_disposition, replay_episode_nonce FROM intake_outbox
          WHERE replay_request_key = $1",
    )
    .bind(key)
    .fetch_optional(&host.pool)
    .await
    .unwrap()
}

async fn session_key(host: &Host, channel: ChannelId) -> String {
    crate::services::discord::adk_session::build_adk_session_key(
        &host.shared,
        channel,
        &ProviderKind::Claude,
        None,
    )
    .await
    .expect("session key")
}

async fn seed_session(host: &Host, key: &str, channel: &str, bound: Option<(i64, &str)>) {
    sqlx::query(
        "INSERT INTO sessions (session_key, channel_id, provider, status, claude_session_id,
                               current_replay_receipt_id, replay_episode_nonce)
         VALUES ($1, $2, 'claude', 'turn_active', $3, $4, $5)",
    )
    .bind(key)
    .bind(channel)
    .bind(STALE)
    .bind(bound.map(|(receipt, _)| receipt))
    .bind(bound.map(|(_, nonce)| nonce))
    .execute(&host.pool)
    .await
    .unwrap();
}

/// A started receipt of `key` over `sources`, its attempt and nonce, as an earlier start left it.
async fn started(
    host: &Host,
    key: String,
    channel: ChannelId,
    sources: Vec<String>,
    text: &str,
) -> (write::ExactAttempt, String) {
    let input = CanonicalInput {
        request_key: key,
        provider: "claude".into(),
        channel: channel.to_string(),
        sources,
        original_text: text.into(),
        owner_id: "7".into(),
        agent_id: String::new(),
        attachments: serde_json::json!([]),
        reply_context: None,
        provenance: "new_input",
    };
    let receipt = write::register_or_reuse(&host.pool, &input, "node-a")
        .await
        .unwrap();
    let nonce = receipt.episode_nonce.unwrap();
    let binding = serde_json::json!({});
    let effect = EffectProjection {
        effect_target: "earlier-start",
        input_hash: "earlier",
        binding: &binding,
    };
    let from = BeginFrom::Registered { nonce: &nonce };
    let ack = write::begin(&host.pool, receipt.id, from, &effect, "incarnation-a").await;
    (ack.unwrap().into_parts().0, nonce)
}

async fn queued(host: &Host, channel: ChannelId) -> Vec<(u64, Vec<u64>)> {
    let snapshot = host.shared.mailbox(channel).snapshot().await;
    let ids = |item: &crate::services::discord::Intervention| {
        let sources = item.source_message_ids.iter().map(|id| id.get()).collect();
        (item.message_id.get(), sources)
    };
    snapshot.intervention_queue.iter().map(ids).collect()
}

// A dormant build starts as before: no receipt, the stored resume passed on. Active, the first
// input is made only after its receipt, projection and session binding committed.
#[test]
fn intake_start_begins_its_receipt_before_the_first_input_only_when_active_pg() {
    if !api_child(concat!(
        "services::discord::router::message_handler::intake_turn::replay_start_tests::",
        "intake_start_begins_its_receipt_before_the_first_input_only_when_active_pg"
    )) {
        return;
    }
    crate::services::discord::commands::control::native::tests::runtime().block_on(async {
        let (dormant, active) = (ChannelId::new(6_008_701), ChannelId::new(6_008_702));
        let host = host(&[dormant, active]).await;
        let submitted = run(&host, request(dormant, 6_008_711, "dormant start")).await;
        assert_eq!(submitted.unwrap().session_id.as_deref(), Some(STALE));
        assert_eq!(receipt(&host, dormant, 6_008_711).await, None);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox")
            .fetch_one(&host.pool)
            .await
            .unwrap();
        assert_eq!(rows, 0, "a dormant start writes no replay row");

        let key = session_key(&host, active).await;
        seed_session(&host, &key, &active.to_string(), None).await;
        let _active = test_activation::activate(active.get());
        let entry = execute_intake_turn_core(
            &host.api.http,
            &host.shared,
            "",
            request(active, 6_008_721, "active start"),
            Vec::new(),
        );
        let (result, started) = start(active, entry).await;
        result.unwrap();
        let (submitted, release) = started.expect("the active start reaches the provider");
        let (id, disposition, nonce) = receipt(&host, active, 6_008_721).await.unwrap();
        assert_eq!(disposition, "started_unclassified");
        let projection: (String, String) = sqlx::query_as(
            "SELECT replay_preserved->'projection'->>'effect_target',
                    replay_preserved->'projection'->>'input_hash'
               FROM intake_outbox WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&host.pool)
        .await
        .unwrap();
        let target = EffectTarget::ProviderStart {
            provider: "claude".into(),
            channel: active.to_string(),
        };
        assert_eq!(projection, (target.key(), input_hash(&submitted.prompt)));
        let bound: (Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT current_replay_receipt_id, replay_episode_nonce FROM sessions
              WHERE session_key = $1",
        )
        .bind(&key)
        .fetch_one(&host.pool)
        .await
        .unwrap();
        assert_eq!(bound, (Some(id), Some(nonce)));
        let row = crate::services::discord::inflight::load_inflight_state(
            &ProviderKind::Claude,
            active.get(),
        )
        .unwrap();
        assert_eq!(row.replay_receipt_id, Some(id));
        let token = host.shared.mailbox(active).snapshot().await.cancel_token;
        let observed = token.is_some_and(|token| token.replay_attempt().is_some());
        assert!(observed, "the started provider attempt has its live reader");
        let _ = release.send(());
        host.db.drop().await;
    });
}

// A held source is neither started nor requeued; an unacknowledged begin, a refused binding or
// another turn's row keep it queued without input, and a requeued maybe-begun request is consumed.
#[test]
fn undecided_or_held_intake_start_keeps_custody_without_input_pg() {
    if !api_child(concat!(
        "services::discord::router::message_handler::intake_turn::replay_start_tests::",
        "undecided_or_held_intake_start_keeps_custody_without_input_pg"
    )) {
        return;
    }
    crate::services::discord::commands::control::native::tests::runtime().block_on(async {
        let channels = [6_008_731, 6_008_732, 6_008_733, 6_008_734].map(ChannelId::new);
        let [held, lost, fenced, owned] = channels;
        let host = host(&channels).await;
        let _activated = channels.map(|channel| test_activation::activate(channel.get()));

        let sources = vec!["6008741".to_string(), "6008742".to_string()];
        started(&host, "earlier-held".into(), held, sources, "held").await;
        assert!(run(&host, request(held, 6_008_741, "held")).await.is_none());
        assert!(queued(&host, held).await.is_empty());
        let mailbox = host.shared.mailbox(held).snapshot().await;
        assert!(
            mailbox.cancel_token.is_none(),
            "the consumed start released its claim"
        );

        test_activation::lose_next_ack(lost.get());
        assert!(
            run(&host, request(lost, 6_008_751, "lost ack"))
                .await
                .is_none()
        );
        let (_, disposition, _) = receipt(&host, lost, 6_008_751).await.unwrap();
        assert_eq!(disposition, "started_unclassified");
        assert_eq!(
            queued(&host, lost).await,
            vec![(6_008_751, vec![6_008_752, 6_008_751])]
        );
        let persistence = crate::services::discord::queue_persistence_context(
            &host.shared,
            &ProviderKind::Claude,
            lost,
        );
        host.shared
            .mailbox(lost)
            .replace_queue(Vec::new(), persistence)
            .await;
        assert!(
            run(&host, request(lost, 6_008_751, "lost ack"))
                .await
                .is_none()
        );
        assert!(
            queued(&host, lost).await.is_empty(),
            "no permit is issued again"
        );

        let other = started(&host, "other".into(), fenced, vec!["1".into()], "other").await;
        let key = session_key(&host, fenced).await;
        seed_session(
            &host,
            &key,
            "elsewhere",
            Some((other.0.receipt_id(), &other.1)),
        )
        .await;
        assert!(
            run(&host, request(fenced, 6_008_761, "fenced"))
                .await
                .is_none()
        );
        let (_, disposition, _) = receipt(&host, fenced, 6_008_761).await.unwrap();
        assert_eq!(disposition, "registered_not_started");
        assert_eq!(
            queued(&host, fenced).await,
            vec![(6_008_761, vec![6_008_762, 6_008_761])]
        );

        // Another turn's inflight row still owns the channel: nothing is begun or replaced.
        let provider = ProviderKind::Claude;
        let row = InflightTurnState::new(
            provider.clone(),
            owned.get(),
            None,
            7,
            6_008_700,
            6_008_700,
            "other turn".into(),
            None,
            None,
            None,
            None,
            0,
        );
        crate::services::discord::inflight::save_inflight_state(&row).unwrap();
        assert!(
            run(&host, request(owned, 6_008_765, "owned"))
                .await
                .is_none()
        );
        assert_eq!(receipt(&host, owned, 6_008_765).await, None);
        assert_eq!(
            queued(&host, owned).await,
            vec![(6_008_765, vec![6_008_766, 6_008_765])]
        );
        let kept = crate::services::discord::inflight::load_inflight_state(&provider, owned.get());
        assert_eq!(kept.map(|row| row.user_msg_id), Some(6_008_700));
        host.db.drop().await;
    });
}

// Only the exact continuation restarts a no-effect request: its next start drops the stale
// resume and moves receipt and session to the next nonce. Unacknowledged, nothing resets or runs.
#[test]
fn exact_retry_starts_only_through_its_continuation_pg() {
    if !api_child(concat!(
        "services::discord::router::message_handler::intake_turn::replay_start_tests::",
        "exact_retry_starts_only_through_its_continuation_pg"
    )) {
        return;
    }
    crate::services::discord::commands::control::native::tests::runtime().block_on(async {
        let channels = [6_008_771, 6_008_772].map(ChannelId::new);
        let host = host(&channels).await;
        let mut outcomes = Vec::new();
        for (channel, lose_ack) in channels.into_iter().zip([false, true]) {
            let _active = test_activation::activate(channel.get());
            let message = channel.get() + 100;
            let key = format!("discord:claude:{channel}:{message}");
            let sources = vec![message.to_string(), (message + 1).to_string()];
            let (attempt, nonce) = started(&host, key, channel, sources, "exact").await;
            let session = session_key(&host, channel).await;
            let receipt_id = attempt.receipt_id();
            seed_session(&host, &session, &channel.to_string(), Some((receipt_id, &nonce))).await;
            let mut observation = RawObservation::for_attempt(&attempt);
            for seen in [Seen::StdoutEof, Seen::Exited { waited: true }, Seen::StderrDrained] {
                observation.record(seen);
            }
            let evidence = observation.no_effect_evidence().unwrap();
            let ack = write::classify_no_effect(&host.pool, attempt, &evidence).await;
            let permit = RetryPermit::from_no_effect(ack.unwrap());
            let next = permit.next_nonce().to_string();
            if lose_ack {
                test_activation::lose_next_ack(channel.get());
            }
            let retry = ExactRetry {
                user_message_id: MessageId::new(message),
                sources: vec![MessageId::new(message + 1)],
                request_owner: UserId::new(7),
                text: "exact".into(),
                reply_context: None,
                permit,
            };
            let gateway = crate::services::discord::gateway::DiscordGateway::new(
                host.api.http.clone(),
                host.shared.clone(),
                ProviderKind::Claude,
                None,
            );
            let handoff = crate::services::discord::gateway::TurnGateway::schedule_exact_retry_with_history(
                &gateway, channel, retry,
            );
            let (handoff, started) = start(channel, handoff).await;
            let resume = host.shared.core.lock().await.sessions[&channel].session_id.clone();
            let stored: (String, String, Option<String>) = sqlx::query_as(
                "SELECT replay_disposition, replay_episode_nonce, replay_retry_of_nonce
                   FROM intake_outbox WHERE id = $1",
            )
            .bind(receipt_id)
            .fetch_one(&host.pool)
            .await
            .unwrap();
            let moved: (Option<String>, Option<String>) = sqlx::query_as(
                "SELECT claude_session_id, replay_episode_nonce FROM sessions WHERE session_key = $1",
            )
            .bind(&session)
            .fetch_one(&host.pool)
            .await
            .unwrap();
            let submitted = started.map(|(prompt, release)| {
                let _ = release.send(());
                prompt.session_id
            });
            assert_eq!(stored, ("started_unclassified".into(), next.clone(), Some(nonce)));
            assert_eq!(moved, (None, Some(next)));
            outcomes.push((handoff, submitted, resume));
        }
        assert_eq!(
            outcomes,
            vec![
                (ExactHandoff::Started, Some(None), None),
                (ExactHandoff::Refused, None, Some(STALE.into())),
            ]
        );
        host.db.drop().await;
    });
}

// A dispatch whose request is held stays with that hold: an active start carries it neither from
// its text nor from the thread's card link, while a dormant build reads nothing and keeps it.
#[test]
fn held_dispatch_is_not_inherited_by_an_active_start_pg() {
    if !api_child(concat!(
        "services::discord::router::message_handler::intake_turn::replay_start_tests::",
        "held_dispatch_is_not_inherited_by_an_active_start_pg"
    )) {
        return;
    }
    crate::services::discord::commands::control::native::tests::runtime().block_on(async {
        let (dormant, active) = (ChannelId::new(6_008_771), ChannelId::new(6_008_772));
        // Once set, the thread's card link answers with this dispatch, as for a card thread.
        let linked = Arc::new(std::sync::Mutex::new(None::<String>));
        let thread = linked.clone();
        let answer = Arc::new(move |_: &axum::http::Method, path: &str| {
            let lookup = path == "/api/internal/pending-dispatch-for-thread";
            let linked = thread.lock().unwrap().clone().filter(|_| lookup)?;
            Some(serde_json::json!({ "dispatch_id": linked }))
        });
        let host = host_with(&[dormant, active], answer).await;
        for channel in [dormant, active] {
            let key = format!("held-dispatch-{channel}");
            let sources = vec![channel.to_string()];
            let (attempt, _) = started(&host, key, channel, sources, "held work").await;
            let preserved = serde_json::json!({ "hold": "test" });
            write::withhold(&host.pool, &attempt, "held", &preserved)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO task_dispatches (id, status, replay_receipt_id)
                 VALUES ($1, 'dispatched', $2)",
            )
            .bind(format!("held-{channel}"))
            .bind(attempt.receipt_id())
            .execute(&host.pool)
            .await
            .unwrap();
            let key = session_key(&host, channel).await;
            seed_session(&host, &key, &channel.to_string(), None).await;
        }
        let blocked: bool = sqlx::query_scalar("SELECT replay_dispatch_blocked($1)")
            .bind(format!("held-{active}"))
            .fetch_one(&host.pool)
            .await
            .unwrap();
        assert!(blocked, "the fixture's dispatch is held");

        let text = format!("DISPATCH: held-{dormant}\nwork");
        let started = run(&host, request(dormant, 6_008_781, &text)).await;
        assert!(started.is_some(), "the dormant start runs");
        let _active = test_activation::activate(active.get());
        *linked.lock().unwrap() = Some(format!("held-{active}"));
        let text = format!("DISPATCH: held-{active}\nwork");
        let started = run(&host, request(active, 6_008_791, &text)).await;
        assert!(
            started.is_some(),
            "the active start runs without the dispatch"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        let calls = host.api.take();
        let carried = |channel: ChannelId| {
            let lookup = format!("GET /api/dispatches/held-{channel} ");
            let field = format!("\"dispatch_id\":\"held-{channel}\"");
            let carried = |call: &String| call.starts_with(&lookup) || call.contains(&field);
            calls.iter().any(carried)
        };
        assert!(carried(dormant), "dormant keeps the dispatch: {calls:?}");
        assert!(!carried(active), "{calls:?}");
        // The prompt's turn records name the dispatch it carries.
        let recorded = |channel: ChannelId| {
            let query = "SELECT COUNT(*) FROM turn_lifecycle_events WHERE dispatch_id = $1";
            sqlx::query_scalar::<_, i64>(query)
                .bind(format!("held-{channel}"))
                .fetch_one(&host.pool)
        };
        assert!(
            recorded(dormant).await.unwrap() > 0,
            "dormant prompt carries it"
        );
        assert_eq!(recorded(active).await.unwrap(), 0);
        host.pool.close().await;
        host.db.drop().await;
    });
}
