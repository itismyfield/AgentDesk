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
    let persistence =
        QueuePersistenceContext::new(&ProviderKind::Claude, "test-token-hash", None);
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
    let [on, off, attached, unconfirmed] =
        agents.map(|(_, ch, mode)| InjectPane::new(ch, mode));
    attached.set("attach", "1");
    unconfirmed.set("fail_paste", "");
    let app = router(Some(pool), Some(registry));

    let input =
        json!({"text": "status?", "author_discord_user_id": "200", "source": "imessage"});
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
        crate::services::claude_tui::composer_lock::composer_admission(
            &direct.session(),
            || None
        ),
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
        BindingRoot, ReadyPane, bind_turn_mode_transcript, inflight_rows_for_tests,
        queue_texts, register_inject_runtime, seed_presence_for_tests, settled_reason,
        start_without_gateway,
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
