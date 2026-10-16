//! Restart and the before-start policy receipt: which rows it takes, that it shows one stop and
//! clears the row, and that every row it cannot settle stays untouched and silent.
use super::*;
use crate::services::agent_protocol::{NativeTerminalKind, RuntimeHandoffKind};
use crate::services::discord::InflightRestartMode;
use crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE;

const CHANNEL: u64 = 1_479_671_301_387_200_000;
const NONCE: &str = "nonce-before-start-restart";
const CURRENT: u64 = 9;

/// A change applied to a row's JSON.
type Edit = fn(&mut serde_json::Value);

/// A prior process's policy close on a saved row, under a fresh test root.
struct Fixture {
    _root: tempfile::TempDir,
    _env: crate::config::test_env::TestEnvVarGuard,
    _legacy: crate::services::tui_o::cutover::test_override::ChannelsGuard,
    provider: ProviderKind,
    row: inflight::InflightTurnState,
    shared: Arc<SharedData>,
}

impl Fixture {
    fn new(tag: &str, marked: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let env = crate::config::set_agentdesk_root_for_test(root.path());
        let legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let provider = ProviderKind::Claude;
        let name = provider.build_tmux_session_name(&format!("before-start-{tag}"));
        if marked {
            let marker = crate::services::tmux_common::session_temp_path(&name, "host_kind");
            std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
            std::fs::write(marker, "herdr").unwrap();
        }
        let mut row = inflight::InflightTurnState::new(
            provider.clone(),
            CHANNEL,
            None,
            1,
            2,
            3,
            "cold input".to_string(),
            None,
            Some(name),
            None,
            None,
            0,
        );
        row.turn_nonce = Some(NONCE.to_string());
        row.born_generation = 4;
        let row = edited(&row, |row| {
            row["herdr_before_start_stop"] = receipt("pending")
        });
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared)
            .unwrap()
            .restart
            .current_generation = CURRENT;
        Self {
            _root: root,
            _env: env,
            _legacy: legacy,
            provider,
            row,
            shared,
        }
    }

    /// Saves `row` and returns it as the loader reads it back.
    fn persist(&self, row: &inflight::InflightTurnState) -> inflight::InflightTurnState {
        std::fs::create_dir_all(self.path().parent().unwrap()).unwrap();
        std::fs::write(self.path(), serde_json::to_vec_pretty(row).unwrap()).unwrap();
        self.durable().unwrap()
    }

    fn path(&self) -> std::path::PathBuf {
        inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &self.provider,
            CHANNEL,
        )
    }

    fn bytes(&self) -> Option<Vec<u8>> {
        std::fs::read(self.path()).ok()
    }

    fn durable(&self) -> Option<inflight::InflightTurnState> {
        inflight::load_inflight_state(&self.provider, CHANNEL)
    }

    async fn settle(
        &self,
        http: &Arc<serenity::Http>,
        row: &inflight::InflightTurnState,
    ) -> BeforeStartRestartReport {
        let decoded = decode(&self.shared, &self.provider, row).expect("a receipt row");
        decoded.settle(http, &self.shared, row).await
    }

    /// Runs the whole restart scan against a fresh fake Discord.
    async fn restore(&self) -> super::o_cut_recorder::DiscordRecorder {
        let discord = super::o_cut_recorder::start(CHANNEL).await;
        super::restore_inflight_turns(&discord.http, &self.shared, &self.provider).await;
        discord
    }
}

fn receipt(notice: &str) -> serde_json::Value {
    serde_json::json!({
        "protocol": 1,
        "kind": "unsubmitted_before_start_stop",
        "turn_nonce": NONCE,
        "admitted_generation": 5,
        "basis": "no_attempt",
        "notice": notice,
    })
}

fn edited(row: &inflight::InflightTurnState, edit: Edit) -> inflight::InflightTurnState {
    let mut value = serde_json::to_value(row).unwrap();
    edit(&mut value);
    serde_json::from_value(value).unwrap()
}

/// What the restart scan wrote to this channel.
fn written(discord: &super::o_cut_recorder::DiscordRecorder) -> Vec<String> {
    let path = format!("/channels/{CHANNEL}/messages");
    discord
        .calls()
        .into_iter()
        .filter(|call| {
            (call.route.starts_with("POST ") || call.route.starts_with("PATCH "))
                && call.route.contains(&path)
        })
        .map(|call| call.content.unwrap_or_default())
        .collect()
}

fn class(decoded: Option<BeforeStartRestart>) -> &'static str {
    match decoded {
        None => "existing path",
        Some(BeforeStartRestart::Held(_)) => "held",
        Some(BeforeStartRestart::Settle(..)) => "settle",
    }
}

/// Rows restart must keep, each refused by exactly one decoder condition.
const HELD: [(&str, Edit); 10] = [
    ("unknown protocol", |row| {
        row["herdr_before_start_stop"]["protocol"] = 2.into()
    }),
    ("unknown kind", |row| {
        row["herdr_before_start_stop"]["kind"] = "future_kind".into()
    }),
    ("unknown basis", |row| {
        row["herdr_before_start_stop"]["basis"] = "future_basis".into()
    }),
    ("unknown notice", |row| {
        row["herdr_before_start_stop"]["notice"] = "future_notice".into()
    }),
    ("native terminal", |row| {
        row["tui_terminal_kind"] = serde_json::to_value(NativeTerminalKind::Aborted).unwrap()
    }),
    ("foreign nonce", |row| {
        row["herdr_before_start_stop"]["turn_nonce"] = "other-turn".into()
    }),
    ("zero generation", |row| {
        row["herdr_before_start_stop"]["admitted_generation"] = 0.into()
    }),
    ("this process's close", |row| {
        row["herdr_before_start_stop"]["admitted_generation"] = CURRENT.into()
    }),
    ("born in this process", |row| {
        row["born_generation"] = CURRENT.into()
    }),
    ("a later process's close", |row| {
        row["herdr_before_start_stop"]["admitted_generation"] = (CURRENT + 1).into()
    }),
];

/// Only a readable prior-close receipt naming its row settles, marker or not; other receipts are
/// held, and a row without one, or with settlement off, keeps the existing path.
#[test]
fn decode_takes_only_a_prior_readable_receipt_that_names_its_row() {
    let fixture = Fixture::new("decode", false);
    let (shared, provider) = (&fixture.shared, &fixture.provider);
    assert_eq!(class(decode(shared, provider, &fixture.row)), "settle");
    let delivered = edited(&fixture.row, |row| {
        row["herdr_before_start_stop"]["notice"] = "delivered".into()
    });
    assert_eq!(class(decode(shared, provider, &delivered)), "settle");
    for (case, edit) in HELD {
        let row = edited(&fixture.row, edit);
        assert_eq!(class(decode(shared, provider, &row)), "held", "{case}");
    }
    let absent = edited(&fixture.row, |row| {
        row.as_object_mut()
            .unwrap()
            .remove("herdr_before_start_stop");
    });
    assert_eq!(class(decode(shared, provider, &absent)), "existing path");
    HERDR_SETTLEMENT_OVERRIDE.set(false);
    let off = class(decode(shared, provider, &fixture.row));
    HERDR_SETTLEMENT_OVERRIDE.set(true);
    assert_eq!(off, "held", "production's hard-false settlement");
}

/// A pending receipt shows one stop on the turn's own message, acks it and clears the row; an
/// already delivered one only clears.
#[tokio::test(flavor = "current_thread")]
async fn a_prior_receipt_shows_one_stop_then_clears_its_row() {
    let fixture = Fixture::new("settle", false);
    let row = fixture.persist(&fixture.row);
    let discord = super::o_cut_recorder::start(CHANNEL).await;
    let outcome = fixture.settle(&discord.http, &row).await;
    assert_eq!(outcome, BeforeStartRestartReport::Settled);
    assert!(fixture.durable().is_none());
    let calls = discord.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].route.starts_with("PATCH "), "{calls:?}");
    assert!(
        calls[0]
            .route
            .ends_with(&format!("/channels/{CHANNEL}/messages/3"))
    );
    assert_eq!(calls[0].content.as_deref(), Some(ADMITTED_ABORT_NOTICE));

    let delivered = edited(&fixture.row, |row| {
        row["herdr_before_start_stop"]["notice"] = "delivered".into()
    });
    let row = fixture.persist(&delivered);
    let discord = super::o_cut_recorder::start(CHANNEL).await;
    let outcome = fixture.settle(&discord.http, &row).await;
    assert_eq!(outcome, BeforeStartRestartReport::Settled);
    assert!(fixture.durable().is_none());
    assert!(discord.calls().is_empty(), "{:?}", discord.calls());
}

/// Restart keeps each held receipt row byte-for-byte with no Discord call, marker or not: neither
/// the existing path (it looks the channel up) nor, by a native kind, the admitted path runs.
#[tokio::test(flavor = "current_thread")]
async fn the_restart_scan_keeps_every_held_receipt_silent_and_untouched() {
    for marked in [false, true] {
        for (case, edit) in HELD {
            let fixture = Fixture::new("held", marked);
            fixture.persist(&edited(&fixture.row, edit));
            let before = fixture.bytes();
            let discord = fixture.restore().await;
            assert!(
                discord.calls().is_empty(),
                "{case} marked={marked}: {:?}",
                discord.calls()
            );
            assert_eq!(fixture.bytes(), before, "{case} marked={marked}");
        }
    }
}

/// The receipt is decoded before the marker hold: a marked receipt row settles in the scan.
#[tokio::test(flavor = "current_thread")]
async fn the_restart_scan_settles_a_marked_receipt_before_the_marker_hold() {
    let fixture = Fixture::new("marked", true);
    fixture.persist(&fixture.row);
    let discord = fixture.restore().await;
    assert_eq!(written(&discord), [ADMITTED_ABORT_NOTICE]);
    assert!(fixture.durable().is_none());
}

/// O's body, a planned restart, a live actor or a missing message keep the receipt: nothing is
/// sent, acked or cleared.
#[tokio::test(flavor = "current_thread")]
async fn a_destination_restart_does_not_own_keeps_the_receipt() {
    let cases: [(&str, Edit); 4] = [
        ("O owns the body", |row| {
            row["runtime_kind"] = serde_json::to_value(RuntimeHandoffKind::ClaudeTui).unwrap()
        }),
        ("planned restart", |row| {
            row["restart_mode"] = serde_json::to_value(InflightRestartMode::DrainRestart).unwrap()
        }),
        ("no message", |row| row["current_msg_id"] = 0.into()),
        ("delivered without its message", |row| {
            row["current_msg_id"] = 0.into();
            row["herdr_before_start_stop"] = receipt("delivered");
        }),
    ];
    for (case, edit) in cases {
        let fixture = Fixture::new("kept", false);
        let _o = (case == "O owns the body").then(|| {
            crate::services::tui_o::cutover::test_override::force_channels(&[(
                CHANNEL,
                RuntimeHandoffKind::ClaudeTui,
            )])
        });
        let row = fixture.persist(&edited(&fixture.row, edit));
        let before = fixture.bytes();
        let discord = super::o_cut_recorder::start(CHANNEL).await;
        let outcome = fixture.settle(&discord.http, &row).await;
        assert!(
            matches!(outcome, BeforeStartRestartReport::Retained(_)),
            "{case}"
        );
        if case.contains("message") {
            let held = BeforeStartRestartReport::Retained("no message to show the stop on");
            assert_eq!(outcome, held, "{case}: held before any later step");
        }
        assert_eq!(fixture.bytes(), before, "{case}");
        assert!(discord.calls().is_empty(), "{case}: {:?}", discord.calls());
        if case == "delivered without its message" {
            let cleared = row.clear_before_start_receipt(CURRENT);
            assert_ne!(cleared, inflight::GuardedClearOutcome::Cleared, "{case}");
            assert_eq!(fixture.bytes(), before, "{case}: the clear refused it");
        }
    }
    let fixture = Fixture::new("busy", false);
    let row = fixture.persist(&fixture.row);
    fixture
        .shared
        .mailbox(ChannelId::new(CHANNEL))
        .restore_active_turn(
            Arc::new(CancelToken::new()),
            serenity::UserId::new(7),
            serenity::MessageId::new(CHANNEL + 1),
        )
        .await;
    let before = fixture.bytes();
    let discord = super::o_cut_recorder::start(CHANNEL).await;
    let outcome = fixture.settle(&discord.http, &row).await;
    assert!(matches!(outcome, BeforeStartRestartReport::Retained(_)));
    assert_eq!(fixture.bytes(), before);
    assert!(discord.calls().is_empty(), "{:?}", discord.calls());
}

fn dead_http() -> Arc<serenity::Http> {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy(format!("http://{address}"))
            .ratelimiter_disabled(true)
            .build(),
    )
}

/// An unshown notice is never acked, so a later boot shows it; a durable ack is never shown again,
/// even when cleanup did not run after it.
#[tokio::test(flavor = "current_thread")]
async fn an_unshown_notice_waits_and_a_durable_ack_is_not_shown_again() {
    let fixture = Fixture::new("retry", false);
    let row = fixture.persist(&fixture.row);
    let before = fixture.bytes();
    let outcome = fixture.settle(&dead_http(), &row).await;
    assert!(matches!(outcome, BeforeStartRestartReport::Retained(_)));
    assert_eq!(fixture.bytes(), before, "no ack without a shown notice");

    let (shared, token) = (fixture.shared.clone(), Arc::new(CancelToken::new()));
    test_hooks::set_before_cleanup(Box::new(move || {
        Box::pin(async move {
            shared
                .mailbox(ChannelId::new(CHANNEL))
                .restore_active_turn(
                    token,
                    serenity::UserId::new(7),
                    serenity::MessageId::new(CHANNEL + 1),
                )
                .await;
        })
    }));
    let discord = super::o_cut_recorder::start(CHANNEL).await;
    let outcome = fixture.settle(&discord.http, &row).await;
    assert!(matches!(outcome, BeforeStartRestartReport::Retained(_)));
    assert_eq!(written(&discord), [ADMITTED_ABORT_NOTICE]);
    let acked = fixture.durable().expect("an actor arrived before cleanup");
    assert!(
        !acked
            .herdr_before_start_stop
            .as_ref()
            .unwrap()
            .notice_pending()
    );

    let mut next_boot = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut next_boot)
        .unwrap()
        .restart
        .current_generation = CURRENT + 1;
    let decoded = decode(&next_boot, &fixture.provider, &acked).unwrap();
    let discord = super::o_cut_recorder::start(CHANNEL).await;
    let outcome = decoded.settle(&discord.http, &next_boot, &acked).await;
    assert_eq!(outcome, BeforeStartRestartReport::Settled);
    assert!(discord.calls().is_empty(), "{:?}", discord.calls());
    assert!(fixture.durable().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b_receipt_off_stays_held_no_effect_pg() {
    let fixture = Fixture::new("b3b2-off", false);
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    assert_eq!(
        sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    std::fs::create_dir_all(fixture.path().parent().unwrap()).unwrap();
    std::fs::write(
        fixture.path(),
        serde_json::to_vec_pretty(&fixture.row).unwrap(),
    )
    .unwrap();
    let before = fixture.bytes();
    HERDR_SETTLEMENT_OVERRIDE.set(false);
    assert_eq!(
        class(decode(&fixture.shared, &fixture.provider, &fixture.row)),
        "held"
    );
    let discord = fixture.restore().await;
    assert!(written(&discord).is_empty());
    assert_eq!(fixture.bytes(), before);
    assert!(
        super::mailbox_snapshot(&fixture.shared, ChannelId::new(CHANNEL))
            .await
            .cancel_token
            .is_none()
    );
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b_all_raw_receipt_precedes_rebind_pg() {
    let mut fixture = Fixture::new("b3b2-rebind", false);
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    Arc::get_mut(&mut fixture.shared).unwrap().pg_pool = Some(pool.clone());
    fixture.row.rebind_origin = true;
    std::fs::create_dir_all(fixture.path().parent().unwrap()).unwrap();
    std::fs::write(
        fixture.path(),
        serde_json::to_vec_pretty(&fixture.row).unwrap(),
    )
    .unwrap();
    let before = fixture.bytes();
    HERDR_SETTLEMENT_OVERRIDE.set(false);
    let discord = fixture.restore().await;
    assert_eq!(
        fixture.bytes(),
        before,
        "All raw classification precedes rebind clear"
    );
    assert!(written(&discord).is_empty());
    pool.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b_all_receipt_precedes_replay_debt_pg() {
    let mut fixture = Fixture::new("b3b2-replay", false);
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    Arc::get_mut(&mut fixture.shared).unwrap().pg_pool = Some(pool.clone());
    fixture.row.full_response = "saved receipt debt".into();
    let id:i64=sqlx::query_scalar("INSERT INTO intake_outbox (target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,request_owner_id,user_text,turn_kind,agent_id,provider,status,replay_only,replay_disposition,replay_source_message_ids,replay_episode_nonce,replay_hold_reason,replay_preserved) VALUES ('worker','leader',$1,$2,'7',$3,'standard','agent','claude','unknown',TRUE,'withheld',$4,$5,'activity observed',$6) RETURNING id")
        .bind(CHANNEL.to_string()).bind(fixture.row.user_msg_id.to_string()).bind(&fixture.row.user_text)
        .bind(vec![fixture.row.user_msg_id.to_string()]).bind(&fixture.row.turn_nonce)
        .bind(serde_json::json!({"body":fixture.row.full_response})).fetch_one(&pool).await.unwrap();
    fixture.row.replay_receipt_id = Some(id);
    std::fs::create_dir_all(fixture.path().parent().unwrap()).unwrap();
    std::fs::write(
        fixture.path(),
        serde_json::to_vec_pretty(&fixture.row).unwrap(),
    )
    .unwrap();
    let before = fixture.bytes();
    HERDR_SETTLEMENT_OVERRIDE.set(false);
    let discord = fixture.restore().await;
    assert_eq!(
        fixture.bytes(),
        before,
        "classification before replay hydration/debt mutation"
    );
    assert!(
        discord.calls().is_empty(),
        "receipt debt must not reach Discord"
    );
    let canonical: (String, serde_json::Value) =
        sqlx::query_as("SELECT replay_disposition,replay_preserved FROM intake_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        canonical,
        (
            "withheld".into(),
            serde_json::json!({"body":"saved receipt debt"})
        )
    );
    assert!(
        super::mailbox_snapshot(&fixture.shared, ChannelId::new(CHANNEL))
            .await
            .cancel_token
            .is_none()
    );
    let mut ordinary = serde_json::to_value(&fixture.row).unwrap();
    ordinary
        .as_object_mut()
        .unwrap()
        .remove("herdr_before_start_stop");
    std::fs::write(
        fixture.path(),
        serde_json::to_vec_pretty(&ordinary).unwrap(),
    )
    .unwrap();
    let discord = fixture.restore().await;
    assert!(
        !discord.calls().is_empty(),
        "receipt-free replay debt really reaches Discord"
    );
    assert!(fixture.durable().unwrap().terminal_delivery_completed());
    pool.close().await;
}

/// The scan's first receipt check holds a row that replay, a native terminal or a rebind also
/// claims, unchanged and silent; settlement on covers only conflicts the row itself records.
#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b_receipt_replay_conflict_is_held_pg() {
    for (on, case) in [
        (false, "replay"),
        (false, "native"),
        (false, "rebind"),
        (true, "native"),
        (true, "rebind"),
    ] {
        let mut fixture = Fixture::new("b3b3-conflict", case == "native");
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        Arc::get_mut(&mut fixture.shared).unwrap().pg_pool = Some(pool.clone());
        let mut debt = None;
        match case {
            "replay" => {
                fixture.row.full_response = "saved receipt debt".into();
                debt = Some(withheld(&pool, &fixture.row).await);
                fixture.row.replay_receipt_id = debt;
            }
            "native" => fixture.row.tui_terminal_kind = Some(NativeTerminalKind::Aborted),
            _ => fixture.row.rebind_origin = true,
        }
        let mut row = serde_json::to_value(&fixture.row).unwrap();
        std::fs::create_dir_all(fixture.path().parent().unwrap()).unwrap();
        std::fs::write(fixture.path(), serde_json::to_vec_pretty(&row).unwrap()).unwrap();
        let before = fixture.bytes();
        HERDR_SETTLEMENT_OVERRIDE.set(on);
        test_hooks::early_holds();
        let discord = fixture.restore().await;
        let holds = test_hooks::early_holds();
        HERDR_SETTLEMENT_OVERRIDE.set(true);
        assert!(
            holds >= 1,
            "{case} on={on}: the first receipt check held the row"
        );
        assert_eq!(fixture.bytes(), before, "{case} on={on}");
        assert!(discord.calls().is_empty(), "{case} on={on}");
        assert!(
            super::mailbox_snapshot(&fixture.shared, ChannelId::new(CHANNEL))
                .await
                .cancel_token
                .is_none()
        );
        if let Some(id) = debt {
            let canonical: (String, serde_json::Value) = sqlx::query_as(
                "SELECT replay_disposition,replay_preserved FROM intake_outbox WHERE id=$1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
            let body = serde_json::json!({"body":"saved receipt debt"});
            assert_eq!(canonical, ("withheld".into(), body));
            row.as_object_mut()
                .unwrap()
                .remove("herdr_before_start_stop");
            std::fs::write(fixture.path(), serde_json::to_vec_pretty(&row).unwrap()).unwrap();
            let discord = fixture.restore().await;
            assert!(
                !discord.calls().is_empty(),
                "receipt-free replay debt really reaches Discord"
            );
            assert!(fixture.durable().unwrap().terminal_delivery_completed());
        }
        pool.close().await;
    }
}

async fn withheld(pool: &sqlx::PgPool, row: &inflight::InflightTurnState) -> i64 {
    sqlx::query_scalar("INSERT INTO intake_outbox (target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,request_owner_id,user_text,turn_kind,agent_id,provider,status,replay_only,replay_disposition,replay_source_message_ids,replay_episode_nonce,replay_hold_reason,replay_preserved) VALUES ('worker','leader',$1,$2,'7',$3,'standard','agent','claude','unknown',TRUE,'withheld',$4,$5,'activity observed',$6) RETURNING id")
        .bind(CHANNEL.to_string()).bind(row.user_msg_id.to_string()).bind(&row.user_text)
        .bind(vec![row.user_msg_id.to_string()]).bind(&row.turn_nonce)
        .bind(serde_json::json!({"body":row.full_response})).fetch_one(pool).await.unwrap()
}
