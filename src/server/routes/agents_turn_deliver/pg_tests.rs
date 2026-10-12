use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::tests::{deliver, get, post, router};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::health::{
    HealthRegistry, register_bot_auth_for_tests, seed_external_turn_row_for_tests,
};
use crate::services::provider::ProviderKind;

#[tokio::test(flavor = "current_thread")]
async fn an_external_tui_turn_row_is_busy_for_start_and_deliver_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let cc = 6_245_111_u64;
    crate::db::agents::insert_agent_channels_for_tests(
        &pool,
        "busy-agent",
        Some(&cc.to_string()),
        None,
    )
    .await;
    let registry = Arc::new(HealthRegistry::new());
    register_bot_auth_for_tests(&registry, "claude", cc, Some(100), vec![200], false).await;
    // The TUI-direct turn holds only its durable row; the mailbox stays free.
    seed_external_turn_row_for_tests(&ProviderKind::Claude, cc);
    let app = router(Some(pool), Some(registry));

    let start = json!({"prompt": "status?"}).to_string();
    let input = json!({"text": "status?", "author_discord_user_id": "200"}).to_string();
    let (start_status, started) = post(&app, "/agents/busy-agent/turn/start", &start).await;
    let (deliver_status, delivered) = deliver(&app, "busy-agent", &input).await;
    let shape = |status: StatusCode, body: &Value, key: &str| {
        let field = |name: &str| body[name].as_str().unwrap_or("-").to_string();
        format!("{} {} {}", status.as_u16(), field(key), field("reason"))
    };
    let observed = [
        shape(start_status, &started, "status"),
        shape(deliver_status, &delivered, "delivery"),
    ];
    assert_eq!(
        observed,
        [
            "409 conflict external_turn_active",
            "200 queued external_turn_active"
        ],
        "{started} {delivered}"
    );
}

/// A reply queued behind a TUI-direct turn is, as the live enqueue normalized it, a singleton the
/// queued-head claim reserves durably.
#[tokio::test(flavor = "current_thread")]
async fn a_reply_queued_behind_a_busy_turn_is_a_claimable_singleton_pg() {
    use crate::services::turn_orchestrator::{
        ChannelMailboxRegistry, QueueIdentity, QueuePersistenceContext, QueuedHeadClaim,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let cc = 6_813_001_u64;
    let seed = crate::db::agents::insert_agent_channels_for_tests;
    seed(&pool, "head-agent", Some(&cc.to_string()), None).await;
    let registry = Arc::new(HealthRegistry::new());
    register_bot_auth_for_tests(&registry, "claude", cc, Some(100), vec![200], false).await;
    seed_external_turn_row_for_tests(&ProviderKind::Claude, cc);
    let app = router(Some(pool), Some(registry));

    let start = json!({"prompt": "status?"}).to_string();
    let input = json!({"text": "status?", "author_discord_user_id": "200"}).to_string();
    let (start_status, _) = post(&app, "/agents/head-agent/turn/start", &start).await;
    let (_, delivered) = deliver(&app, "head-agent", &input).await;
    assert_eq!(start_status, StatusCode::CONFLICT);
    assert_eq!(delivered["delivery"], "queued", "{delivered}");

    let channel = poise::serenity_prelude::ChannelId::new(cc);
    let handle = ChannelMailboxRegistry::global_handle(channel).expect("intake mailbox");
    let head = handle.snapshot().await.intervention_queue.remove(0);
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, "test-token-hash", None);
    let claim = handle
        .claim_queued_head(QueueIdentity::of(&head), persistence)
        .await;
    assert!(matches!(claim, QueuedHeadClaim::Reserved(_)));
}

/// The route reaches a scripted pane only when switched on; a pane veto hands the input back to
/// the queue front and names itself.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_route_injects_into_a_busy_tui_direct_turn_only_when_switched_on_pg() {
    use crate::services::discord::health::{InjectPane, queue_texts, register_inject_runtime};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let agents = [
        ("inject-on", 6_245_121_u64, "external"),
        ("inject-off", 6_245_122, "off"),
        ("inject-attached", 6_245_123, "external"),
        ("inject-unconfirmed", 6_245_124, "external"),
    ];
    let channels = agents.map(|(_, channel, _)| channel);
    for (agent, channel, _) in agents {
        let seed = crate::db::agents::insert_agent_channels_for_tests;
        seed(&pool, agent, Some(&channel.to_string()), None).await;
    }
    let registry = Arc::new(HealthRegistry::new());
    let shared = register_inject_runtime(&registry, &channels, Some(pool.clone())).await;
    let [on, off, attached, unconfirmed] = agents.map(|(_, ch, mode)| InjectPane::new(ch, mode));
    attached.set("attach", "1");
    unconfirmed.set("fail_paste", "");
    let app = router(Some(pool), Some(registry));

    let input = json!({"text": "status?", "author_discord_user_id": "200", "source": "imessage"});
    let mut observed = Vec::new();
    for ((agent, channel, _), pane) in agents.iter().zip([&on, &off, &attached, &unconfirmed]) {
        let (status, body) = deliver(&app, agent, &input.to_string()).await;
        let field = |name: &str| match body.get(name) {
            None => "-".to_string(),
            Some(value) => value.as_str().unwrap_or("null").to_string(),
        };
        let queued = queue_texts(&shared, *channel).await.len();
        // A queued reply names a fresh reservation id on this channel.
        let prefix = format!("discord:{channel}:");
        let turn = field("turn_id");
        let turn = if turn.starts_with(&prefix) {
            format!("{prefix}*")
        } else {
            turn
        };
        observed.push(format!(
            "{agent}: {} {} turn={turn} reason={} veto={} detail={} keys={:?} tmux={} queued={queued}",
            status.as_u16(),
            field("delivery"),
            field("reason"),
            field("inject_veto"),
            field("detail"),
            pane.keys(),
            pane.tmux_calls() > 0,
        ));
    }
    assert_eq!(
        observed,
        [
            "inject-on: 200 injected turn=null reason=null veto=- detail=- keys=[\"paste-buffer\", \"send-keys\"] tmux=true queued=0",
            "inject-off: 200 queued turn=discord:6245122:* reason=external_turn_active veto=- detail=- keys=[] tmux=false queued=1",
            "inject-attached: 200 queued turn=discord:6245123:* reason=handed_back veto=human_attached detail=- keys=[] tmux=true queued=1",
            "inject-unconfirmed: 200 unconfirmed turn=null reason=inject_unconfirmed veto=- detail=paste_failed keys=[] tmux=true queued=0",
        ]
    );
    assert!(on.transcript_recorded_the_paste());
    let alerts = crate::services::observability::events::recent(10_000);
    let alerted = |channel: u64| {
        alerts.iter().any(|event| {
            event.event_type == "busy_inject_unconfirmed" && event.channel_id == Some(channel)
        })
    };
    assert_eq!(channels.map(alerted), [false, false, false, true]);
}

/// The HTTP entry carries the resolved channel into the same stash transaction as Discord.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_route_stashes_an_attached_persons_draft_only_on_the_allowlisted_channel_pg() {
    use crate::services::discord::health::{InjectPane, queue_texts, register_inject_runtime};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let agents = [
        ("stash-listed", 6_845_611_u64),
        ("stash-outside", 6_845_612),
    ];
    for (agent, channel) in agents {
        crate::db::agents::insert_agent_channels_for_tests(
            &pool,
            agent,
            Some(&channel.to_string()),
            None,
        )
        .await;
    }
    let registry = Arc::new(HealthRegistry::new());
    let shared = register_inject_runtime(
        &registry,
        &agents.map(|(_, channel)| channel),
        Some(pool.clone()),
    )
    .await;
    let draft = "  사람 초안 첫 줄\n\n  둘째 줄 — 그대로 보존";
    let mut listed = InjectPane::new(agents[0].1, "external");
    let mut outside = InjectPane::new(agents[1].1, "external");
    listed.stash(draft, true);
    outside.stash(draft, false);
    let app = router(Some(pool), Some(registry));
    let input = json!({
        "text": "status?", "author_discord_user_id": "200",
        "source": "imessage", "origin_id": "attached-stash-http",
    })
    .to_string();
    let (status, body) = deliver(&app, agents[0].0, &input).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["delivery"], "injected", "{body}");
    assert!(body["turn_id"].is_null(), "{body}");
    assert_eq!(listed.applied(), ["C-s", "paste", "Enter"]);
    assert_eq!(listed.read("composer").as_deref(), Some(draft));
    assert!(listed.read("stash").is_none());
    assert_eq!(listed.read("attached").as_deref(), Some("4"));
    let records = listed.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0], listed.read("buffer").unwrap());
    assert!(
        records[0].starts_with("[📱 imessage · 200 · "),
        "{records:?}"
    );
    assert!(records[0].ends_with("\nstatus?"), "{records:?}");
    assert!(!records[0].contains(draft));
    assert!(queue_texts(&shared, agents[0].1).await.is_empty());

    let (status, body) = deliver(&app, agents[1].0, &input).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["delivery"], "queued", "{body}");
    assert_eq!(body["reason"], "handed_back", "{body}");
    assert_eq!(body["inject_veto"], "human_attached", "{body}");
    assert_eq!(queue_texts(&shared, agents[1].1).await, ["status?"]);
    assert!(outside.applied().is_empty());
    assert!(outside.records().is_empty());
    assert_eq!(outside.read("composer").as_deref(), Some(draft));
    assert!(outside.read("stash").is_none());
}

/// Same-generation human typing cannot turn a contaminated submit into confirmed delivery.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_route_reports_contaminated_submit_and_protects_a_mixed_direct_draft_pg() {
    use crate::services::discord::health::{InjectPane, queue_texts, register_inject_runtime};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let agents = [
        ("stash-contaminated", 6_845_651_u64),
        ("direct-mixed", 6_845_652),
    ];
    for (agent, channel) in agents {
        crate::db::agents::insert_agent_channels_for_tests(
            &pool,
            agent,
            Some(&channel.to_string()),
            None,
        )
        .await;
    }
    let registry = Arc::new(HealthRegistry::new());
    let shared = register_inject_runtime(
        &registry,
        &agents.map(|(_, channel)| channel),
        Some(pool.clone()),
    )
    .await;
    let draft = "  첫째 줄 사람 초안\n\n  마지막 줄";
    let mut contaminated = InjectPane::new(agents[0].1, "external");
    contaminated.stash(draft, true);
    // Own capture returns before a person types X and puts the caret back at its old location.
    contaminated.set("human.3", "x=$(cat \"$d/cursor_x\"); y=$(cat \"$d/cursor_y\"); type_text X; echo \"$x\" > \"$d/cursor_x\"; echo \"$y\" > \"$d/cursor_y\"");
    let mut direct = InjectPane::new(agents[1].1, "external");
    direct.stash("", true);
    direct.set("human.1", "type_text 'person typed '");
    let app = router(Some(pool), Some(registry));
    let input = json!({
        "text": "status?", "author_discord_user_id": "200",
        "source": "imessage", "origin_id": "contaminated-submit-http",
    })
    .to_string();
    let (status, body) = deliver(&app, agents[0].0, &input).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["delivery"], "unconfirmed", "{body}");
    assert_eq!(body["detail"], "contaminated_submit", "{body}");
    assert_eq!(contaminated.applied(), ["C-s", "paste", "Enter"]);
    assert_eq!(
        contaminated.records(),
        [format!("{}X", contaminated.read("buffer").unwrap())]
    );
    assert_eq!(contaminated.read("composer").as_deref(), Some(draft));
    assert!(contaminated.read("stash").is_none());
    assert_eq!(contaminated.read("attached").as_deref(), Some("4"));
    assert_eq!(contaminated.read("last").as_deref(), Some("100"));
    assert_eq!(contaminated.alarm_count("busy_inject_unconfirmed"), 1);
    assert!(queue_texts(&shared, agents[0].1).await.is_empty());

    let (status, body) = deliver(&app, agents[1].0, &input).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["delivery"], "unconfirmed", "{body}");
    assert_eq!(direct.applied(), ["paste"]);
    assert!(direct.records().is_empty());
    assert!(
        direct
            .read("composer")
            .unwrap()
            .starts_with("person typed ")
    );
    assert_eq!(direct.alarm_count("busy_inject_draft_unrecovered"), 1);
    assert!(queue_texts(&shared, agents[1].1).await.is_empty());
    assert_eq!(
        crate::services::claude_tui::composer_lock::composer_admission(&direct.session(), || None),
        crate::services::claude_tui::composer_lock::ComposerAdmission::Held,
    );
    let applied = direct.applied();
    let (status, body) = deliver(&app, agents[1].0, &input).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["delivery"], "queued", "{body}");
    assert_eq!(body["inject_veto"], "draft", "{body}");
    assert_eq!(direct.applied(), applied);
    assert!(direct.records().is_empty());
    assert_eq!(queue_texts(&shared, agents[1].1).await, ["status?"]);
}

/// With the switch off the route answers as it did before injection existed: a start claims
/// at once even past queued input, no success adds an injection field, and errors keep `detail`.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_switch_off_keeps_every_route_answer_field_for_field_pg() {
    use crate::services::discord::health::{
        end_turn, queue_texts, register_inject_runtime, start_without_gateway,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let agents = [
        ("off-idle", 6_845_291_u64),
        ("off-row", 6_845_292),
        ("off-nogw", 6_845_293),
    ];
    for (agent, channel) in agents {
        let seed = crate::db::agents::insert_agent_channels_for_tests;
        seed(&pool, agent, Some(&channel.to_string()), None).await;
    }
    let registry = Arc::new(HealthRegistry::new());
    let channels = agents.map(|(_, channel)| channel);
    let shared = register_inject_runtime(&registry, &channels, Some(pool.clone())).await;
    let _starts = start_without_gateway(channels[0]);
    seed_external_turn_row_for_tests(&ProviderKind::Claude, channels[1]);
    let app = router(Some(pool), Some(registry));
    let input = json!({"text": "status?", "author_discord_user_id": "200"}).to_string();
    let answer = |(status, mut body): (StatusCode, Value), channel: u64| {
        let prefix = format!("discord:{channel}:");
        if body["turn_id"]
            .as_str()
            .is_some_and(|id| id.starts_with(&prefix))
        {
            body["turn_id"] = json!(format!("{prefix}*"));
        }
        format!("{} {body}", status.as_u16())
    };
    let (idle, row, nogw) = (channels[0], channels[1], channels[2]);
    let mut observed = vec![answer(deliver(&app, "off-idle", &input).await, idle)];
    observed.push(answer(deliver(&app, "off-idle", &input).await, idle));
    end_turn(&shared, idle).await;
    observed.push(answer(deliver(&app, "off-idle", &input).await, idle));
    observed.push(format!("left={}", queue_texts(&shared, idle).await.len()));
    observed.push(answer(deliver(&app, "off-row", &input).await, row));
    observed.push(answer(deliver(&app, "off-nogw", &input).await, nogw));
    assert_eq!(
        observed,
        [
            r#"200 {"channel_id":"6845291","delivery":"started","ok":true,"reason":null,"turn_id":"discord:6845291:*"}"#,
            r#"200 {"channel_id":"6845291","delivery":"queued","ok":true,"reason":"turn_active","turn_id":"discord:6845291:*"}"#,
            r#"200 {"channel_id":"6845291","delivery":"started","ok":true,"reason":null,"turn_id":"discord:6845291:*"}"#,
            "left=1",
            r#"200 {"channel_id":"6845292","delivery":"queued","ok":true,"reason":"external_turn_active","turn_id":"discord:6845292:*"}"#,
            r#"503 {"detail":"provider runtime is not ready","error":"runtime_unavailable","ok":false}"#,
        ]
    );
}

/// A turn-mode direct turn holds no row: its transcript alone must queue a deliver and refuse a
/// start, naming whether it read busy or unknown, and the hold writes no row of its own.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_turn_mode_direct_turn_queues_deliver_and_refuses_start_pg() {
    use crate::services::discord::health::{
        BindingRoot, ReadyPane, bind_turn_mode_transcript, inflight_rows_for_tests, queue_texts,
        register_inject_runtime, seed_presence_for_tests, settled_reason, start_without_gateway,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let bindings = tempfile::tempdir().unwrap();
    let _binding = BindingRoot::enter(bindings.path());
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let user = r#"{"type":"user","uuid":"a","message":{"content":"direct"}}"#;
    // Every pane passes the final send check, so only the transcript holds the channel.
    let agents = [
        ("tm-busy", 6_845_301_u64, format!("{user}\n"), "open"),
        (
            "tm-unknown",
            6_845_302,
            format!("{user}\n{{\"type\":\"user\"\n"),
            "facts_halted",
        ),
        ("tm-missing", 6_845_303, String::new(), "binding_unreadable"),
    ];
    for (agent, channel, _, _) in &agents {
        let seed = crate::db::agents::insert_agent_channels_for_tests;
        seed(&pool, agent, Some(&channel.to_string()), None).await;
    }
    let registry = Arc::new(HealthRegistry::new());
    let channels = agents.each_ref().map(|(_, channel, _, _)| *channel);
    let shared = register_inject_runtime(&registry, &channels, Some(pool.clone())).await;
    let (mut starts, mut confirmed, mut ready) = (Vec::new(), Vec::new(), Vec::new());
    for (_, channel, transcript, reason) in &agents {
        starts.push(start_without_gateway(*channel));
        let path = bindings.path().join(format!("{channel}.jsonl"));
        std::fs::write(&path, transcript).unwrap();
        let session = format!("route-{channel}");
        ready.push(ReadyPane::mark(&session));
        let provider = ProviderKind::Claude;
        confirmed.push(bind_turn_mode_transcript(
            &shared, &provider, *channel, &session, &path,
        ));
        if transcript.is_empty() {
            std::fs::rename(&path, path.with_extension("moved")).unwrap();
        }
        assert_eq!(settled_reason(&shared, &provider, *channel).await, *reason);
    }
    let rows = || inflight_rows_for_tests(&ProviderKind::Claude, &channels);
    assert_eq!(rows(), 0);
    let app = router(Some(pool), Some(registry));
    let input = json!({"text": "status?", "author_discord_user_id": "200"}).to_string();
    let start = json!({"prompt": "status?"}).to_string();
    let mut observed = Vec::new();
    for (agent, channel, _, _) in &agents {
        let (status, body) = deliver(&app, agent, &input).await;
        let field = |body: &Value, name: &str| body[name].as_str().unwrap_or("-").to_string();
        let queued = queue_texts(&shared, *channel).await.len();
        let (start_status, started) =
            post(&app, &format!("/agents/{agent}/turn/start"), &start).await;
        observed.push(format!(
            "{agent}: deliver {} {} {} queued={queued} start {} {}",
            status.as_u16(),
            field(&body, "delivery"),
            field(&body, "reason"),
            start_status.as_u16(),
            field(&started, "reason"),
        ));
    }
    assert_eq!(
        observed,
        [
            "tm-busy: deliver 200 queued external_turn_active queued=1 start 409 external_turn_active",
            "tm-unknown: deliver 200 queued turn_activity_unknown queued=1 start 409 turn_activity_unknown",
            "tm-missing: deliver 200 queued turn_activity_unknown queued=1 start 409 turn_activity_unknown",
        ]
    );
    assert_eq!(rows(), 0, "a held deliver or start writes no row");
    // The turn endpoint shows a supervised presence only while the channel is in turn mode.
    for (index, (_, channel, _, _)) in agents.iter().enumerate() {
        seed_presence_for_tests(*channel, index == 0);
    }
    drop(confirmed.pop());
    let mut presence = Vec::new();
    for (agent, _, _, _) in &agents {
        let (status, body) = get(&app, &format!("/agents/{agent}/turn")).await;
        presence.push((status.as_u16(), body["turn_presence"]["activity"].clone()));
    }
    let expected = [
        (200, json!("busy")),
        (200, json!("unknown")),
        (200, Value::Null),
    ];
    assert_eq!(presence, expected);
}

#[tokio::test(flavor = "current_thread")]
async fn target_and_author_gates_run_at_the_route_boundary_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let (cc, cdx) = (6_245_101_u64, 6_245_102_u64);
    let (cc_id, cdx_id) = (cc.to_string(), cdx.to_string());
    let seed = crate::db::agents::insert_agent_channels_for_tests;
    seed(&pool, "deliver-agent", Some(&cc_id), Some(&cdx_id)).await;
    seed(&pool, "unbound-agent", None, None).await;
    let registry = Arc::new(HealthRegistry::new());
    register_bot_auth_for_tests(&registry, "claude", cc, Some(100), vec![200], false).await;
    // A second bot that opens itself to everyone must not widen who may inject.
    register_bot_auth_for_tests(&registry, "codex", cdx, None, vec![200], true).await;
    let app = router(Some(pool), Some(registry));
    let with = |extra: Value| {
        let mut payload = json!({"text": "status?", "author_discord_user_id": "200"});
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        payload.to_string()
    };
    #[rustfmt::skip]
    let cases = [
        ("missing-agent", with(json!({})), StatusCode::NOT_FOUND, "agent not found"),
        ("unbound-agent", with(json!({})), StatusCode::CONFLICT, "agent primary provider is not configured"),
        ("deliver-agent", with(json!({"channel_id": "999"})), StatusCode::FORBIDDEN, "channel override 999 is not allowed for agent deliver-agent"),
        ("deliver-agent", with(json!({"provider": "nope"})), StatusCode::BAD_REQUEST, "unsupported provider override: nope"),
        ("deliver-agent", with(json!({"author_discord_user_id": "300"})), StatusCode::FORBIDDEN, "author_not_allowed"),
        ("deliver-agent", with(json!({"provider": "codex"})), StatusCode::FORBIDDEN, "author_not_allowed"),
        ("deliver-agent", with(json!({})), StatusCode::SERVICE_UNAVAILABLE, "runtime_unavailable"),
    ];
    for (agent, payload, status, error) in cases {
        let (actual, response) = deliver(&app, agent, &payload).await;
        assert_eq!(
            (actual, response["error"].as_str()),
            (status, Some(error)),
            "{agent} {payload}"
        );
    }
}

#[cfg(unix)]
mod s1a {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::Router;
    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tokio::sync::{Barrier, Notify, mpsc};
    use tokio::task::JoinHandle;
    use uuid::Uuid;

    use super::super::{ADMISSION_ENABLED_FOR_TEST, DELIVERY_PROBE_FOR_TEST, DeliveryProbe};
    use super::{Arc, TestPostgresDb, deliver, router};
    use crate::db::imsg_reply::{
        AckOutcome, ActionAck, ClaimAction, Obligation, Purpose, ack_action, claim_action,
        read_obligation,
    };
    use crate::services::discord::SharedData;
    use crate::services::discord::health::{
        HealthRegistry, InjectPane, inject_hook, queue_texts, register_inject_runtime,
        seed_external_turn_row_for_tests, set_human_input_auth_for_tests,
    };
    use crate::services::provider::ProviderKind;

    type Answer = (StatusCode, Value);

    struct Fixture {
        db: TestPostgresDb,
        pool: PgPool,
        peer: PgPool,
        app: Router,
        peer_app: Router,
        shared: Arc<SharedData>,
        channel: u64,
        calls: Arc<AtomicUsize>,
        _root: crate::config::TestRuntimeRootGuard,
    }

    impl Fixture {
        async fn create(channel: u64) -> Self {
            let root = crate::config::TestRuntimeRootGuard::new();
            let db = TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            let peer = db.connect_and_migrate().await;
            let tables: (bool, bool) = sqlx::query_as(
                "SELECT to_regclass('imsg_reply_obligations') IS NOT NULL,
                        to_regclass('imsg_reply_actions') IS NOT NULL",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(tables, (true, true), "the real S0 migration must execute");
            crate::db::agents::insert_agent_channels_for_tests(
                &pool,
                "s1a-agent",
                Some(&channel.to_string()),
                None,
            )
            .await;
            let registry = Arc::new(HealthRegistry::new());
            let shared =
                register_inject_runtime(&registry, &[channel, channel + 100], Some(pool.clone()))
                    .await;
            let app = router(Some(pool.clone()), Some(registry.clone()));
            let peer_app = router(Some(peer.clone()), Some(registry));
            Self {
                db,
                pool,
                peer,
                app,
                peer_app,
                shared,
                channel,
                calls: Arc::new(AtomicUsize::new(0)),
                _root: root,
            }
        }

        fn probe(&self) -> DeliveryProbe {
            DeliveryProbe {
                calls: self.calls.clone(),
                ..Default::default()
            }
        }

        fn busy(&self) {
            seed_external_turn_row_for_tests(&ProviderKind::Claude, self.channel);
        }

        async fn call(&self, enabled: bool, body: Value) -> Answer {
            call(&self.app, "s1a-agent", enabled, self.probe(), body).await
        }

        async fn row(&self, origin: &str) -> Obligation {
            read_obligation(&self.peer, origin).await.unwrap().unwrap()
        }

        async fn counts(&self) -> (i64, i64) {
            sqlx::query_as(
                "SELECT (SELECT COUNT(*) FROM imsg_reply_obligations),
                        (SELECT COUNT(*) FROM imsg_reply_actions)",
            )
            .fetch_one(&self.peer)
            .await
            .unwrap()
        }

        fn assert_calls(&self, count: usize) {
            assert_eq!(
                self.calls.load(Ordering::SeqCst),
                count,
                "actual delivery polls"
            );
        }

        async fn add_agent(&self, agent: &str, channel: u64) {
            crate::db::agents::insert_agent_channels_for_tests(
                &self.pool,
                agent,
                Some(&channel.to_string()),
                None,
            )
            .await;
        }

        async fn close(self) {
            drop(self.app);
            drop(self.peer_app);
            self.pool.close().await;
            self.peer.close().await;
            self.db.drop().await;
        }
    }

    fn input(origin: Option<&str>, text: &str, author: u64) -> Value {
        json!({
            "text": text, "author_discord_user_id": author.to_string(),
            "source": "imessage", "origin_id": origin,
        })
    }

    async fn call(
        app: &Router,
        agent: &str,
        enabled: bool,
        probe: DeliveryProbe,
        body: Value,
    ) -> Answer {
        let body = body.to_string();
        ADMISSION_ENABLED_FOR_TEST
            .scope(
                enabled,
                DELIVERY_PROBE_FOR_TEST.scope(probe, deliver(app, agent, &body)),
            )
            .await
    }

    async fn parked(f: &Fixture, origin: &str) -> (JoinHandle<Answer>, Arc<Notify>) {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let probe = DeliveryProbe {
            calls: f.calls.clone(),
            entered: Some(entered.clone()),
            release: Some(release.clone()),
        };
        let (app, body) = (f.app.clone(), input(Some(origin), "status?", 200));
        let task = tokio::spawn(async move { call(&app, "s1a-agent", true, probe, body).await });
        tokio::time::timeout(Duration::from_secs(10), entered.notified())
            .await
            .expect("authorized admission must reach the parked delivery");
        assert_eq!(f.row(origin).await.admission, "admitting");
        (task, release)
    }

    fn claim<'a>(origin: &'a str, purpose: Purpose, key: &'a str) -> ClaimAction<'a> {
        ClaimAction {
            id: Uuid::new_v4(),
            origin_id: origin,
            purpose,
            action_key: key,
            reply_episode_key: Some("early-episode"),
        }
    }

    async fn final_ack(f: &Fixture, origin: &str) -> Uuid {
        let id = claim_action(&f.peer, &claim(origin, Purpose::Final, "early-final"))
            .await
            .unwrap()
            .expect("Final must claim an admitting obligation");
        assert!(
            ack_action(
                &f.peer,
                &ActionAck {
                    origin_id: origin,
                    id,
                    outcome: AckOutcome::Sent,
                    receipt: Some(json!({"message_id": "early-final-receipt"})),
                },
            )
            .await
            .unwrap(),
            "Final ACK must settle before delivery finishes"
        );
        id
    }

    async fn make_automatic_eligible(pool: &PgPool, origin: &str) {
        sqlx::query(
            "UPDATE imsg_reply_obligations SET admission_kind='started',
                 delivery_confirmed=TRUE, episode_key='early-episode', terminal_seen_at=NOW(),
                 opened_at=NOW()-INTERVAL '24 hours', expires_at=NOW()
             WHERE origin_id=$1",
        )
        .bind(origin)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn s1a_off_delivery_byte_equivalence() {
        let f = Fixture::create(8_624_501).await;
        f.busy();
        assert!(!crate::services::imsg_reply::parse_enabled(None));
        assert!(!crate::services::imsg_reply::parse_enabled(Some("off")));
        let (status, mut body) = f.call(false, input(None, "  legacy\ninput  ", 200)).await;
        assert_eq!(status, StatusCode::OK, "OFF legacy route response: {body}");
        let turn = body["turn_id"].as_str().unwrap();
        assert!(turn.starts_with(&format!("discord:{}:", f.channel)));
        body["turn_id"] = json!("legacy-turn");
        assert_eq!(
            (status, body),
            (
                StatusCode::OK,
                json!({
                    "ok": true, "delivery": "queued", "turn_id": "legacy-turn",
                    "channel_id": f.channel.to_string(), "reason": "external_turn_active",
                })
            )
        );
        assert_eq!(queue_texts(&f.shared, f.channel).await, ["legacy\ninput"]);
        let queued = f
            .shared
            .queue_fixture_parts()
            .0
            .handle(poise::serenity_prelude::ChannelId::new(f.channel))
            .snapshot()
            .await;
        assert!(queued.intervention_queue[0].reply_context.is_none());
        assert_eq!(f.counts().await, (0, 0));
        let default_body = input(None, "default legacy input", 200).to_string();
        let default_route = deliver(&f.app, "s1a-agent", &default_body).await;
        assert_eq!(
            default_route.0,
            StatusCode::OK,
            "unscoped default route: {}",
            default_route.1
        );
        assert_eq!(f.counts().await, (0, 0));

        let pane_channel = f.channel + 100;
        f.add_agent("s1a-off-pane", pane_channel).await;
        let pane = InjectPane::new(pane_channel, "external");
        pane.fold_paste(2);
        let (status, body) = call(
            &f.app,
            "s1a-off-pane",
            false,
            f.probe(),
            input(None, "  pane\nbytes  ", 200),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["delivery"], "injected", "{body}");
        let buffer = pane.read("buffer").expect("legacy pane bytes");
        let (header, text) = buffer.split_once('\n').unwrap();
        assert_eq!(text, "pane\nbytes");
        assert!(header.starts_with("[📱 imessage · 200 · ") && header.ends_with(']'));
        assert!(pane.transcript_recorded_the_paste());
        assert_eq!(f.counts().await, (0, 0));

        let mut external = input(None, "external is unchanged", 200);
        external["source"] = json!("external");
        assert_eq!(f.call(true, external).await.0, StatusCode::OK);
        assert_eq!(f.counts().await, (0, 0));
        let positive = f
            .call(
                true,
                input(Some("off-positive"), "authorized positive", 200),
            )
            .await;
        assert_eq!(positive.0, StatusCode::OK, "{}", positive.1);
        assert_eq!(
            f.counts().await,
            (1, 0),
            "ON control reaches real admission SQL"
        );
        drop(pane);
        f.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn s1a_author_before_origin_insert() {
        let f = Fixture::create(8_624_502).await;
        f.busy();
        let denied = f
            .call(true, input(Some("denied-author"), "status?", 300))
            .await;
        assert_eq!(
            denied,
            (
                StatusCode::FORBIDDEN,
                json!({"ok": false, "error": "author_not_allowed"})
            )
        );
        assert_eq!(
            f.counts().await,
            (0, 0),
            "authorization must precede INSERT"
        );
        f.assert_calls(0);
        set_human_input_auth_for_tests(&f.shared, None, vec![200], true).await;
        assert_eq!(
            f.call(true, input(Some("ownerless"), "status?", 200))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(f.counts().await, (0, 0));
        set_human_input_auth_for_tests(&f.shared, Some(100), vec![200], true).await;
        let allowed = f
            .call(true, input(Some("allowed-author"), "status?", 200))
            .await;
        assert_eq!(allowed.0, StatusCode::OK, "{}", allowed.1);
        assert_eq!(
            f.counts().await,
            (1, 0),
            "same ON route must insert for an allowed author"
        );
        assert_eq!(queue_texts(&f.shared, f.channel).await, ["status?"]);
        f.assert_calls(1);
        f.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn s1a_origin_required_and_collision() {
        let f = Fixture::create(8_624_503).await;
        f.busy();
        for origin in [None, Some(""), Some("   ")] {
            let response = f.call(true, input(origin, "status?", 200)).await;
            assert_eq!(
                response.0,
                StatusCode::BAD_REQUEST,
                "missing GUID: {}",
                response.1
            );
        }
        let long = "g".repeat(257);
        assert_eq!(
            f.call(true, input(Some(&long), "status?", 200)).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(f.counts().await, (0, 0));
        f.assert_calls(0);
        let original = f
            .call(true, input(Some("MiXeD-GUID"), "  status?\n", 200))
            .await;
        assert_eq!(original.0, StatusCode::OK, "{}", original.1);
        let mut normalized = input(Some("MiXeD-GUID"), "status?", 200);
        normalized["source"] = json!(" imessage ");
        assert_eq!(
            f.call(true, normalized).await,
            original,
            "actual trimmed delivery bytes determine the hash"
        );
        let conflict = f
            .call(true, input(Some("MiXeD-GUID"), "different text", 200))
            .await;
        assert_eq!(conflict.0, StatusCode::CONFLICT, "{}", conflict.1);
        assert_eq!(conflict.1["error"], "origin_conflict");
        f.assert_calls(1);
        let different_case = f
            .call(true, input(Some("mixed-guid"), "case distinct", 200))
            .await;
        assert_eq!(different_case.0, StatusCode::OK, "{}", different_case.1);
        assert_eq!(f.row("MiXeD-GUID").await.origin_id, "MiXeD-GUID");
        assert_eq!(f.counts().await, (2, 0));
        f.assert_calls(2);
        f.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn s1a_same_guid_concurrent_single_effect() {
        let f = Fixture::create(8_624_504).await;
        f.busy();
        let barrier = Arc::new(Barrier::new(3));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (tx, mut rx) = mpsc::channel(2);
        let mut tasks = Vec::new();
        for app in [f.app.clone(), f.peer_app.clone()] {
            let (barrier, tx) = (barrier.clone(), tx.clone());
            let probe = DeliveryProbe {
                calls: f.calls.clone(),
                entered: Some(entered.clone()),
                release: Some(release.clone()),
            };
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                let answer = call(
                    &app,
                    "s1a-agent",
                    true,
                    probe,
                    input(Some("concurrent"), "status?", 200),
                )
                .await;
                tx.send(answer).await.unwrap();
            }));
        }
        drop(tx);
        barrier.wait().await;
        tokio::time::timeout(Duration::from_secs(10), entered.notified())
            .await
            .expect("one INSERT owner enters delivery");
        let pending = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await;
        let calls = f.calls.load(Ordering::SeqCst);
        release.notify_waiters();
        assert_eq!(calls, 1, "ON CONFLICT loser must not poll delivery");
        let pending = pending
            .expect("duplicate returns while delivery is parked")
            .unwrap();
        assert_eq!(pending.0, StatusCode::ACCEPTED, "{}", pending.1);
        assert_eq!(pending.1["error"], "admission_pending");
        let accepted = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(accepted.0, StatusCode::OK, "{}", accepted.1);
        assert_eq!(
            f.call(true, input(Some("concurrent"), "status?", 200))
                .await,
            accepted
        );
        assert_eq!(f.counts().await, (1, 0));
        assert_eq!(queue_texts(&f.shared, f.channel).await, ["status?"]);
        f.assert_calls(1);
        f.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn s1a_admitting_retry_never_redelivers() {
        let f = Fixture::create(8_624_505).await;
        f.busy();
        let (task, release) = parked(&f, "retry-pending").await;
        let retry = call(
            &f.peer_app,
            "s1a-agent",
            true,
            f.probe(),
            input(Some("retry-pending"), "status?", 200),
        )
        .await;
        release.notify_one();
        let accepted = task.await.unwrap();
        assert_eq!(retry.0, StatusCode::ACCEPTED, "{}", retry.1);
        assert_eq!(retry.1["error"], "admission_pending");
        assert_eq!(accepted.0, StatusCode::OK, "{}", accepted.1);
        assert_eq!(queue_texts(&f.shared, f.channel).await, ["status?"]);
        f.assert_calls(1);
        f.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn s1a_unconfirmed_is_admitted_once() {
        let f = Fixture::create(8_624_506).await;
        let pane = InjectPane::new(f.channel, "external");
        pane.set("fail_paste", "");
        let body = input(Some("unconfirmed"), "status?", 200);
        let original = f.call(true, body.clone()).await;
        assert_eq!(original.0, StatusCode::OK, "{}", original.1);
        assert_eq!(original.1["delivery"], "unconfirmed");
        let row = f.row("unconfirmed").await;
        assert_eq!(row.admission, "admitted");
        assert_eq!(row.admission_kind.as_deref(), Some("unconfirmed"));
        assert_eq!(row.delivery_confirmed, Some(false));
        let tmux_calls = pane.tmux_calls();
        assert!(tmux_calls > 0, "the actual pane path must execute");
        assert_eq!(f.call(true, body).await, original);
        assert_eq!(pane.tmux_calls(), tmux_calls);
        f.assert_calls(1);
        assert_eq!(f.counts().await, (1, 0));
        drop(pane);
        f.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn s1a_refusal_requires_no_effect_proof() {
        let f = Fixture::create(8_624_507).await;
        let known = input(Some("known-refusal"), "status?", 200);
        let (task, release) = parked(&f, "known-refusal").await;
        set_human_input_auth_for_tests(&f.shared, Some(100), Vec::new(), false).await;
        release.notify_one();
        let refused = task.await.unwrap();
        assert_eq!(refused.0, StatusCode::FORBIDDEN, "{}", refused.1);
        let row = f.row("known-refusal").await;
        assert_eq!(
            (row.admission.as_str(), row.resolution.as_str()),
            ("refused", "closed")
        );
        assert!(row.closed_at.is_some());
        assert!(queue_texts(&f.shared, f.channel).await.is_empty());
        set_human_input_auth_for_tests(&f.shared, Some(100), vec![200], false).await;
        assert_eq!(f.call(true, known).await, refused);
        f.assert_calls(1);

        let pane = InjectPane::new(f.channel, "external");
        pane.set("attach", "1");
        inject_hook::fault_handback(f.channel, inject_hook::HandbackFault::AnswerLost);
        let uncertain = input(
            Some("uncertain-refusal"),
            "effect persisted but answer lost",
            200,
        );
        let response = f.call(true, uncertain.clone()).await;
        assert_eq!(
            response.0,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            response.1
        );
        assert_eq!(response.1["error"], "queue_refused");
        assert_eq!(
            queue_texts(&f.shared, f.channel).await,
            ["effect persisted but answer lost"]
        );
        let row = f.row("uncertain-refusal").await;
        assert!(matches!(row.admission.as_str(), "admitting" | "unknown"));
        assert_eq!(
            row.resolution, "open",
            "effect uncertainty is not proved refusal"
        );
        assert!(row.closed_at.is_none());
        let retry = f.call(true, uncertain).await;
        assert_eq!(retry.0, StatusCode::ACCEPTED, "{}", retry.1);
        assert_eq!(
            retry.1["error"],
            if row.admission == "unknown" {
                "admission_unknown"
            } else {
                "admission_pending"
            }
        );
        f.assert_calls(2);
        assert_eq!(f.counts().await, (2, 0));
        drop(pane);
        f.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn s1a_pg_failure_no_effect() {
        let f = Fixture::create(8_624_508).await;
        f.busy();
        assert_eq!(
            f.call(true, input(Some("insert-positive"), "positive", 200))
                .await
                .0,
            StatusCode::OK
        );
        // Reject only INSERT so the live authorized route proves storage failure precedes delivery.
        sqlx::raw_sql("CREATE FUNCTION s1a_fail_insert() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 's1a INSERT fault'; END $$; CREATE TRIGGER s1a_insert_fault BEFORE INSERT ON imsg_reply_obligations FOR EACH ROW EXECUTE FUNCTION s1a_fail_insert()")
            .execute(&f.pool).await.unwrap();
        let failed = f
            .call(true, input(Some("insert-failure"), "must not deliver", 200))
            .await;
        assert_eq!(failed.0, StatusCode::SERVICE_UNAVAILABLE, "{}", failed.1);
        assert!(
            read_obligation(&f.peer, "insert-failure")
                .await
                .unwrap()
                .is_none()
        );
        f.assert_calls(1);
        assert_eq!(queue_texts(&f.shared, f.channel).await, ["positive"]);
        assert_eq!(f.counts().await, (1, 0));
        f.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn s1a_origin_identity_isolation() {
        let f = Fixture::create(8_624_509).await;
        f.busy();
        for (origin, column, value) in [
            ("identity-agent", "agent_id", "other-agent"),
            ("identity-channel", "channel_id", "999"),
            ("identity-person", "person_id", "300"),
        ] {
            let body = input(Some(origin), origin, 200);
            assert_eq!(f.call(true, body.clone()).await.0, StatusCode::OK);
            let original_hash = f.row(origin).await.request_hash;
            let update = match column {
                "agent_id" => "UPDATE imsg_reply_obligations SET agent_id=$2 WHERE origin_id=$1",
                "channel_id" => {
                    "UPDATE imsg_reply_obligations SET channel_id=$2 WHERE origin_id=$1"
                }
                _ => {
                    "UPDATE imsg_reply_obligations SET person_id=$2::text::bigint WHERE origin_id=$1"
                }
            };
            sqlx::query(update)
                .bind(origin)
                .bind(value)
                .execute(&f.pool)
                .await
                .unwrap();
            assert_eq!(
                f.row(origin).await.request_hash,
                original_hash,
                "identity guard is tested independently of hash"
            );
            let rejected = f.call(true, body).await;
            assert_eq!(
                rejected.0,
                StatusCode::CONFLICT,
                "same hash, different {column}: {}",
                rejected.1
            );
            assert_eq!(rejected.1["error"], "origin_conflict");
        }
        f.assert_calls(3);
        assert_eq!(f.counts().await, (3, 0));
        f.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn p1_3_a_admitting_early_final_ack() {
        let f = Fixture::create(8_624_510).await;
        f.busy();
        let (task, release) = parked(&f, "early-final").await;
        final_ack(&f, "early-final").await;
        let row = f.row("early-final").await;
        release.notify_one();
        assert_eq!(task.await.unwrap().0, StatusCode::OK);
        assert_eq!(
            (row.admission.as_str(), row.resolution.as_str()),
            ("admitting", "replied")
        );
        assert!(row.active_send_action.is_none());
        f.assert_calls(1);
        f.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn p1_3_b_late_settlement_keeps_replied() {
        let f = Fixture::create(8_624_511).await;
        f.busy();
        let (task, release) = parked(&f, "late-settlement").await;
        let id = final_ack(&f, "late-settlement").await;
        let closed_at = f.row("late-settlement").await.closed_at;
        release.notify_one();
        let delivered = task.await.unwrap();
        assert_eq!(delivered.0, StatusCode::OK, "{}", delivered.1);
        let row = f.row("late-settlement").await;
        assert_eq!(
            (row.admission.as_str(), row.resolution.as_str()),
            ("admitted", "replied")
        );
        assert_eq!(row.closed_at, closed_at);
        assert!(row.active_send_action.is_none());
        let action = crate::db::imsg_reply::read_action(&f.peer, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(action.status, "sent");
        assert_eq!(
            action.receipt,
            Some(json!({"message_id": "early-final-receipt"}))
        );
        f.assert_calls(1);
        f.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn p1_3_c_replied_restart_not_fallback_candidate() {
        let f = Fixture::create(8_624_512).await;
        f.busy();
        let (task, release) = parked(&f, "restart-replied").await;
        final_ack(&f, "restart-replied").await;
        release.notify_one();
        assert_eq!(task.await.unwrap().0, StatusCode::OK);
        make_automatic_eligible(&f.peer, "restart-replied").await;
        f.pool.close().await;
        f.peer.close().await;
        let restarted = f.db.connect_and_migrate().await;
        assert_eq!(
            read_obligation(&restarted, "restart-replied")
                .await
                .unwrap()
                .unwrap()
                .resolution,
            "replied"
        );
        let candidates: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM imsg_reply_obligations WHERE origin_id='restart-replied' AND resolution='open' AND expires_at<=NOW()")
            .fetch_one(&restarted).await.unwrap();
        assert_eq!(
            candidates, 0,
            "restart reads must not revive an already replied origin"
        );
        for purpose in [Purpose::Fallback, Purpose::Expiry] {
            assert!(
                claim_action(&restarted, &claim("restart-replied", purpose, "restart"))
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        let counts: (i64, i64) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM imsg_reply_obligations), (SELECT COUNT(*) FROM imsg_reply_actions)")
            .fetch_one(&restarted).await.unwrap();
        assert_eq!(counts, (1, 1));
        f.assert_calls(1);
        restarted.close().await;
        f.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn p1_3_d_admitting_blocks_automatic_claims() {
        let f = Fixture::create(8_624_513).await;
        f.busy();
        let (task, release) = parked(&f, "automatic-pending").await;
        make_automatic_eligible(&f.peer, "automatic-pending").await;
        let mut claimed = Vec::new();
        for purpose in [Purpose::Fallback, Purpose::Expiry] {
            claimed.push(
                claim_action(&f.peer, &claim("automatic-pending", purpose, "pending"))
                    .await
                    .unwrap(),
            );
        }
        let before = f.row("automatic-pending").await;
        release.notify_one();
        assert_eq!(task.await.unwrap().0, StatusCode::OK);
        assert_eq!(
            claimed,
            [None, None],
            "admitting gate must independently exclude automatic sends"
        );
        assert!(before.active_send_action.is_none());
        assert_eq!(f.counts().await, (1, 0));
        f.assert_calls(1);
        f.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn s1a_settlement_failure_keeps_admitting_without_success_or_redelivery() {
        let f = Fixture::create(8_624_514).await;
        f.busy();
        // Reject only UPDATE: delivery must occur once, while failed settlement cannot invent success.
        sqlx::raw_sql("CREATE FUNCTION s1a_fail_settlement() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 's1a settlement fault'; END $$; CREATE TRIGGER s1a_settlement_fault BEFORE UPDATE ON imsg_reply_obligations FOR EACH ROW EXECUTE FUNCTION s1a_fail_settlement()")
            .execute(&f.pool).await.unwrap();
        let body = input(Some("settlement-failure"), "delivered once", 200);
        let response = f.call(true, body.clone()).await;
        assert_eq!(
            response.0,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            response.1
        );
        assert_eq!(f.row("settlement-failure").await.admission, "admitting");
        assert_eq!(queue_texts(&f.shared, f.channel).await, ["delivered once"]);
        let retry = f.call(true, body).await;
        assert_eq!(retry.0, StatusCode::ACCEPTED, "{}", retry.1);
        assert_eq!(retry.1["error"], "admission_pending");
        f.assert_calls(1);
        assert_eq!(f.counts().await, (1, 0));
        f.close().await;
    }
}
