//! Scoped restore of a before-start policy receipt row: the raw row is classified before any actor,
//! replay, native or provider path, and only recovery admission with settlement on may settle it.

use super::b3a1_tests::{context, fixture, row};
use super::b3a2_tests::{BARRIER, barrier};
use super::*;
use crate::db::o_channel_homes::HomeState;
use crate::services::cluster::channel_home::{self, HomeGate};
use crate::services::discord::InflightRestartMode;
use crate::services::discord::recovery_engine::herdr_admitted_restart::ADMITTED_ABORT_NOTICE;
use crate::services::discord::recovery_engine::herdr_before_start_restart::test_hooks;
use crate::services::discord::recovery_engine::o_cut_recorder::{self, DiscordRecorder};
use crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE;
use std::sync::atomic::{AtomicU64, Ordering};

const NONCE: &str = "policy-receipt";
/// A change to a row's JSON before its raw write.
type Edit = fn(&mut serde_json::Value);
/// The restoring process's generation; the fixture receipt was admitted by generation 5.
const CURRENT: u64 = 9;

/// This thread's settlement switch for one test, restored on drop.
struct Settlement(bool);
impl Settlement {
    fn set(on: bool) -> Self {
        Self(HERDR_SETTLEMENT_OVERRIDE.replace(on))
    }
}
impl Drop for Settlement {
    fn drop(&mut self) {
        HERDR_SETTLEMENT_OVERRIDE.set(self.0);
    }
}

async fn runtime() -> (
    crate::db::auto_queue::test_support::TestPostgresDb,
    sqlx::PgPool,
    Arc<SharedData>,
) {
    let (db, pool, mut shared) = fixture().await;
    Arc::get_mut(&mut shared)
        .expect("a fresh runtime")
        .restart
        .current_generation = CURRENT;
    (db, pool, shared)
}

/// One channel's held home, its gate and a fake Discord.
struct Lane {
    context: RestoreContext,
    gate: Arc<HomeGate>,
    discord: DiscordRecorder,
}
impl Lane {
    async fn new(pool: &sqlx::PgPool, channel: u64) -> Self {
        sqlx::query("INSERT INTO o_channel_homes(channel_id,provider,state,holder,epoch,renewed_at) VALUES($1,'claude','worker','mini',1,NOW())")
            .bind(channel.to_string()).execute(pool).await.unwrap();
        Self {
            context: context(channel, 1, Arc::new(AtomicU64::new(1))),
            gate: channel_home::register_for_test(channel, Some(HomeState::Worker)),
            discord: o_cut_recorder::start(channel).await,
        }
    }
    async fn install(&self, shared: &Arc<SharedData>) -> InstalledInflight {
        install_inflight_scoped(
            &self.discord.http,
            shared,
            &ProviderKind::Claude,
            &self.context,
        )
        .await
        .unwrap()
    }
    /// The witness is test handoff evidence of an installation, not B3b-4's installed proof.
    async fn resume(
        &self,
        shared: &Arc<SharedData>,
        installed: &InstalledInflight,
    ) -> Result<InflightObservation, String> {
        let witness = RestoreWitness {
            scope: Some(self.context.scope.clone()),
            generation: self.context.generation,
            status: RestoreStatus::Restored,
            inflight: Some(installed.observation.clone()),
        };
        resume_inflight_scoped(
            &self.discord.http,
            shared,
            &ProviderKind::Claude,
            &self.context,
            &witness,
            installed,
        )
        .await
    }
    async fn actor(&self, shared: &Arc<SharedData>) -> bool {
        let channel = ChannelId::new(self.context.scope.channel);
        shared
            .mailbox(channel)
            .snapshot()
            .await
            .cancel_token
            .is_some()
    }
    fn written(&self) -> Vec<String> {
        self.discord.contents()
    }
}
impl Drop for Lane {
    fn drop(&mut self) {
        channel_home::unregister_if_same(&self.gate);
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

/// A prior process's row; `edit` may change it and its receipt before the raw write.
fn write_row(
    state: &inflight::InflightTurnState,
    notice: Option<&str>,
    edit: Edit,
) -> std::path::PathBuf {
    let mut value = serde_json::to_value(state).unwrap();
    if let Some(notice) = notice {
        value["herdr_before_start_stop"] = receipt(notice);
    }
    edit(&mut value);
    let path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        state.channel_id,
    );
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    path
}

fn prior_row(channel: u64) -> inflight::InflightTurnState {
    let mut state = row(channel, Some(NONCE));
    state.born_generation = 4;
    state
}

fn same(_: &mut serde_json::Value) {}

async fn withheld(pool: &sqlx::PgPool, state: &inflight::InflightTurnState) -> i64 {
    sqlx::query_scalar("INSERT INTO intake_outbox (target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,request_owner_id,user_text,turn_kind,agent_id,provider,status,replay_only,replay_disposition,replay_source_message_ids,replay_episode_nonce,replay_hold_reason,replay_preserved) VALUES ('worker','leader',$1,$2,'7',$3,'standard','agent','claude','unknown',TRUE,'withheld',$4,$5,'activity observed',$6) RETURNING id")
        .bind(state.channel_id.to_string()).bind(state.user_msg_id.to_string()).bind(&state.user_text)
        .bind(vec![state.user_msg_id.to_string()]).bind(&state.turn_nonce)
        .bind(serde_json::json!({"body": state.full_response})).fetch_one(pool).await.unwrap()
}

fn held(observation: &InflightObservation) -> Option<&RestoreFailure> {
    match observation {
        InflightObservation::PolicyReceiptHeld { reason } => Some(reason),
        _ => None,
    }
}

/// A prior pending receipt shows one stop, is acked, then its row cleared; a delivered one is only
/// cleared. One recovery permit with intake closed (no command) and no actor.
#[tokio::test]
async fn s3act_b3b3_t12_prior_receipt_settles_by_recovery_permit_only_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool, shared) = runtime().await;
    for (channel, notice) in [(5340601, "pending"), (5340602, "delivered")] {
        let lane = Lane::new(&pool, channel).await;
        let path = write_row(&prior_row(channel), Some(notice), same);
        let installed = lane.install(&shared).await;
        assert!(
            matches!(
                held(&installed.observation),
                Some(RestoreFailure::Retryable(_))
            ),
            "{notice}: installation only holds the receipt: {:?}",
            installed.observation
        );
        assert!(!lane.actor(&shared).await, "{notice}: no actor is claimed");
        lane.gate.close_intake();
        let seen: Arc<std::sync::Mutex<Option<(serde_json::Value, usize)>>> = Arc::default();
        let (gate, probe, report) = (lane.gate.clone(), path.clone(), seen.clone());
        test_hooks::set_before_cleanup(Box::new(move || {
            Box::pin(async move {
                let row: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(probe).unwrap()).unwrap();
                *report.lock().unwrap() = Some((
                    row["herdr_before_start_stop"]["notice"].clone(),
                    gate.commands_in_flight(),
                ));
            })
        }));
        let outcome = lane.resume(&shared, &installed).await;
        assert_eq!(
            outcome,
            Ok(InflightObservation::PolicyReceiptSettled),
            "{notice}"
        );
        assert_eq!(
            seen.lock().unwrap().take(),
            Some((serde_json::json!("delivered"), 1)),
            "{notice}: the durable ack precedes cleanup under exactly one permit"
        );
        assert!(
            !path.exists(),
            "{notice}: the guarded clear removed the row"
        );
        let calls = lane.discord.calls();
        if notice == "pending" {
            assert_eq!(lane.written(), [ADMITTED_ABORT_NOTICE], "{calls:?}");
            assert!(calls[0].route.starts_with("PATCH "), "{calls:?}");
            assert!(
                calls[0]
                    .route
                    .ends_with(&format!("/channels/{channel}/messages/72"))
            );
        } else {
            assert!(
                calls.is_empty(),
                "a delivered notice is never resent: {calls:?}"
            );
        }
        assert!(!lane.actor(&shared).await);
        assert_eq!(lane.gate.commands_in_flight(), 0);
    }
    assert!(shared.core.lock().await.sessions.is_empty());
    assert_eq!(shared.tmux_watchers.len(), 0);
    pool.close().await;
    db.drop().await;
}

/// A live actor, planned restart, unreadable receipt or changed home keeps the row unshown and
/// uncleared with no actor; a clear refused after the ack is never reported settled.
#[tokio::test]
async fn s3act_b3b3_t12_receipt_restart_cannot_settle_stays_held_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool, shared) = runtime().await;
    let cases: [(&str, Edit); 8] = [
        ("live actor", same),
        ("drain restart", |row| {
            row["restart_mode"] = serde_json::to_value(InflightRestartMode::DrainRestart).unwrap()
        }),
        ("hot-swap handoff", |row| {
            row["restart_mode"] = serde_json::to_value(InflightRestartMode::HotSwapHandoff).unwrap()
        }),
        ("unknown protocol", |row| {
            row["herdr_before_start_stop"]["protocol"] = 2.into()
        }),
        ("unknown field", |row| {
            row["herdr_before_start_stop"]["future"] = true.into()
        }),
        ("unknown type", |row| {
            row["herdr_before_start_stop"] = "pending".into()
        }),
        ("present null", |row| {
            row["herdr_before_start_stop"] = serde_json::Value::Null
        }),
        ("home changed", same),
    ];
    for (n, (case, edit)) in cases.into_iter().enumerate() {
        let channel = 5340611 + n as u64;
        let lane = Lane::new(&pool, channel).await;
        let path = write_row(&prior_row(channel), Some("pending"), edit);
        let before = std::fs::read(&path).unwrap();
        let installed = lane.install(&shared).await;
        assert!(held(&installed.observation).is_some(), "{case}");
        assert!(!lane.actor(&shared).await, "{case}: no actor is claimed");
        if case == "live actor" {
            shared
                .mailbox(ChannelId::new(channel))
                .restore_active_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(7),
                    MessageId::new(channel + 1),
                )
                .await;
        }
        let outcome = if case == "home changed" {
            let barrier = barrier("resume-receipt");
            let resume = BARRIER.scope(barrier.clone(), lane.resume(&shared, &installed));
            let change = async {
                barrier.entered.notified().await;
                sqlx::query("UPDATE o_channel_homes SET epoch=2 WHERE channel_id=$1")
                    .bind(channel.to_string())
                    .execute(&pool)
                    .await
                    .unwrap();
                barrier.release.notify_one();
            };
            let (outcome, ()) = tokio::join!(resume, change);
            assert!(outcome.is_err(), "{case}: {outcome:?}");
            outcome
        } else {
            let outcome = lane.resume(&shared, &installed).await;
            assert!(
                outcome.as_ref().is_ok_and(|o| held(o).is_some()),
                "{case}: {outcome:?}"
            );
            outcome
        };
        assert!(lane.discord.calls().is_empty(), "{case}: {outcome:?}");
        assert_eq!(std::fs::read(&path).ok(), Some(before), "{case}");
    }

    let channel = 5340621;
    let lane = Lane::new(&pool, channel).await;
    let path = write_row(&prior_row(channel), Some("delivered"), same);
    let installed = lane.install(&shared).await;
    let probe = path.clone();
    test_hooks::set_before_cleanup(Box::new(move || {
        Box::pin(async move {
            let mut row: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&probe).unwrap()).unwrap();
            row["save_generation"] = (row["save_generation"].as_u64().unwrap() + 1).into();
            std::fs::write(&probe, serde_json::to_vec_pretty(&row).unwrap()).unwrap();
        })
    }));
    let outcome = lane.resume(&shared, &installed).await;
    assert!(
        matches!(
            outcome.as_ref().map(held),
            Ok(Some(RestoreFailure::Retryable(_)))
        ),
        "a refused clear stays held: {outcome:?}"
    );
    assert!(path.exists(), "the changed row is kept");
    assert!(lane.discord.calls().is_empty());
    pool.close().await;
    db.drop().await;
}

/// With settlement off a receipt row is held byte for byte with no actor, effect admission or
/// Discord call, while the same row without its receipt still installs.
#[tokio::test]
async fn s3act_b3b3_t13_receipt_off_is_held_scoped_without_effects_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let _off = Settlement::set(false);
    let (db, pool, shared) = runtime().await;
    for (channel, notice) in [(5340631, Some("pending")), (5340632, Some("delivered"))] {
        let lane = Lane::new(&pool, channel).await;
        let path = write_row(&prior_row(channel), notice, same);
        let before = std::fs::read(&path).unwrap();
        let installed = lane.install(&shared).await;
        assert!(
            matches!(
                held(&installed.observation),
                Some(RestoreFailure::Rejected(_))
            ),
            "{notice:?}: {:?}",
            installed.observation
        );
        let outcome = lane.resume(&shared, &installed).await;
        assert!(
            matches!(
                outcome.as_ref().map(held),
                Ok(Some(RestoreFailure::Rejected(_)))
            ),
            "{notice:?}: {outcome:?}"
        );
        assert!(!installed.effects_started.load(Ordering::Acquire));
        assert!(
            !lane.actor(&shared).await,
            "{notice:?}: no actor is claimed"
        );
        assert!(lane.discord.calls().is_empty());
        assert_eq!(std::fs::read(&path).ok(), Some(before));
    }
    let lane = Lane::new(&pool, 5340633).await;
    write_row(&prior_row(5340633), None, same);
    let installed = lane.install(&shared).await;
    assert_eq!(installed.observation, InflightObservation::Active);
    assert!(
        lane.actor(&shared).await,
        "a receipt-free row still installs"
    );
    pool.close().await;
    db.drop().await;
}

/// A receipt row with replay debt or a native terminal is held: nothing delivered, cleared or
/// installed, while the same debt without the receipt reaches Discord.
#[tokio::test]
async fn s3act_b3b3_t34_receipt_conflicts_are_held_scoped_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool, shared) = runtime().await;
    for (channel, notice, case) in [
        (5340641, Some("pending"), "replay"),
        (5340642, Some("pending"), "native"),
        (5340643, None, "replay"),
    ] {
        let lane = Lane::new(&pool, channel).await;
        let mut state = prior_row(channel);
        state.full_response = "saved receipt debt".into();
        if case == "native" {
            let name = ProviderKind::Claude.build_tmux_session_name(&format!("b3b3-{channel}"));
            let marker = crate::services::tmux_common::session_temp_path(&name, "host_kind");
            std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
            std::fs::write(marker, "herdr").unwrap();
            state.tmux_session_name = Some(name);
            state.tui_terminal_kind = Some(NativeTerminalKind::Aborted);
        } else {
            withheld(&pool, &state).await;
        }
        let path = write_row(&state, notice, same);
        let before = std::fs::read(&path).unwrap();
        let installed = lane.install(&shared).await;
        let outcome = lane.resume(&shared, &installed).await;
        if notice.is_none() {
            assert!(
                matches!(
                    outcome,
                    Ok(InflightObservation::ReplayHeld {
                        delivery: Some(Ok(())),
                        ..
                    })
                ),
                "{outcome:?}"
            );
            assert!(
                lane.written()
                    .iter()
                    .any(|s| s.contains("saved receipt debt"))
            );
            continue;
        }
        assert!(
            matches!(
                held(&installed.observation),
                Some(RestoreFailure::Rejected(_))
            ),
            "{case}: {:?}",
            installed.observation
        );
        assert!(
            matches!(
                outcome.as_ref().map(held),
                Ok(Some(RestoreFailure::Rejected(_)))
            ),
            "{case}: {outcome:?}"
        );
        assert!(lane.discord.calls().is_empty(), "{case}");
        assert_eq!(std::fs::read(&path).ok(), Some(before), "{case}");
        assert!(!lane.actor(&shared).await, "{case}");
    }
    let withheld: Vec<String> = sqlx::query_scalar(
        "SELECT replay_disposition FROM intake_outbox WHERE channel_id='5340641'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        withheld,
        ["withheld"],
        "the conflicting debt stays canonical"
    );
    pool.close().await;
    db.drop().await;
}

/// A receipt this process admitted, or on a row it started, is held even with no actor: no effect
/// is admitted, no stop shown and nothing acked or cleared.
#[tokio::test]
async fn s3act_b3b3_g1_current_generation_receipt_is_held_without_actor_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool, shared) = runtime().await;
    let admitted_now: Edit =
        |row| row["herdr_before_start_stop"]["admitted_generation"] = CURRENT.into();
    let born_now: Edit = |row| row["born_generation"] = CURRENT.into();
    for (n, (notice, edit)) in [
        ("pending", admitted_now),
        ("delivered", admitted_now),
        ("pending", born_now),
        ("delivered", born_now),
    ]
    .into_iter()
    .enumerate()
    {
        let channel = 5340651 + n as u64;
        let lane = Lane::new(&pool, channel).await;
        let path = write_row(&prior_row(channel), Some(notice), edit);
        let before = std::fs::read(&path).unwrap();
        let installed = lane.install(&shared).await;
        let outcome = lane.resume(&shared, &installed).await;
        assert!(
            matches!(
                outcome.as_ref().map(held),
                Ok(Some(RestoreFailure::Rejected(_)))
            ),
            "{n} {notice}: {outcome:?}"
        );
        assert!(
            !installed.effects_started.load(Ordering::Acquire),
            "{n} {notice}: no effect is admitted"
        );
        assert!(lane.discord.calls().is_empty(), "{n} {notice}");
        assert_eq!(std::fs::read(&path).ok(), Some(before), "{n} {notice}");
        assert!(!lane.actor(&shared).await);
    }
    pool.close().await;
    db.drop().await;
}

/// While the replay source lookup fails a settleable receipt is held with no effect; once it
/// answers, the same installation settles it.
#[tokio::test]
async fn s3act_b3b3_g2_unverified_replay_authority_holds_the_receipt_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool, shared) = runtime().await;
    let channel = 5340661;
    let lane = Lane::new(&pool, channel).await;
    let path = write_row(&prior_row(channel), Some("pending"), same);
    let before = std::fs::read(&path).unwrap();
    sqlx::query("ALTER TABLE intake_outbox RENAME TO intake_outbox_unreadable")
        .execute(&pool)
        .await
        .unwrap();
    let installed = lane.install(&shared).await;
    let outcome = lane.resume(&shared, &installed).await;
    assert!(
        matches!(
            outcome.as_ref().map(held),
            Ok(Some(RestoreFailure::Retryable(_)))
        ),
        "{outcome:?}"
    );
    assert!(!installed.effects_started.load(Ordering::Acquire));
    assert!(lane.discord.calls().is_empty());
    assert_eq!(std::fs::read(&path).ok(), Some(before));
    sqlx::query("ALTER TABLE intake_outbox_unreadable RENAME TO intake_outbox")
        .execute(&pool)
        .await
        .unwrap();
    let outcome = lane.resume(&shared, &installed).await;
    assert_eq!(outcome, Ok(InflightObservation::PolicyReceiptSettled));
    assert_eq!(lane.written(), [ADMITTED_ABORT_NOTICE]);
    assert!(!path.exists());
    pool.close().await;
    db.drop().await;
}

/// Settlement on or off, a receipt-free row keeps its class: work needs a command, debt is
/// delivered, a rebind keeps its owner; a held Herdr turn admits no effect.
#[tokio::test]
async fn s3act_b3b3_g3_receipt_free_rows_keep_their_class_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool, shared) = runtime().await;
    let mut classes = Vec::new();
    for (base, on) in [(5340671, true), (5340681, false)] {
        let _p10 = Settlement::set(on);
        let mut seen = Vec::new();
        for (n, case) in ["ordinary", "replay", "rebind", "held"]
            .into_iter()
            .enumerate()
        {
            if case == "held" && !on {
                continue;
            }
            let channel = base + n as u64;
            let lane = Lane::new(&pool, channel).await;
            let mut state = row(channel, Some(NONCE));
            match case {
                "replay" => {
                    state.full_response = "saved debt".into();
                    withheld(&pool, &state).await;
                }
                "rebind" => state.rebind_origin = true,
                "held" => {
                    let name =
                        ProviderKind::Claude.build_tmux_session_name(&format!("b3b3-{channel}"));
                    let marker =
                        crate::services::tmux_common::session_temp_path(&name, "host_kind");
                    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap())
                        .unwrap();
                    std::fs::write(marker, "herdr").unwrap();
                    state.tmux_session_name = Some(name);
                }
                _ => {}
            }
            let path = write_row(&state, None, same);
            let before = std::fs::read(&path).unwrap();
            let installed = lane.install(&shared).await;
            lane.gate.close_intake();
            let outcome = match lane.resume(&shared, &installed).await {
                Ok(InflightObservation::ReplayHeld { delivery, .. }) => {
                    format!("replay {delivery:?}")
                }
                outcome => format!("{outcome:?}"),
            };
            let delivered = !lane.discord.calls().is_empty();
            assert_eq!(delivered, case == "replay", "{case} on={on}: {outcome}");
            if case != "replay" {
                assert_eq!(std::fs::read(&path).ok(), Some(before), "{case} on={on}");
            }
            seen.push((case, outcome));
        }
        classes.push(seen);
    }
    let held = classes[0].pop().unwrap();
    assert_eq!(
        held,
        (
            "held",
            format!("{:?}", Ok::<_, String>(InflightObservation::HerdrHeld))
        )
    );
    assert_eq!(
        classes[0], classes[1],
        "settlement does not reclassify a receipt-free row"
    );
    assert_eq!(
        classes[0],
        [
            (
                "ordinary",
                format!("{:?}", Err::<(), _>("home execution admission refused"))
            ),
            ("replay", "replay Some(Ok(()))".to_string()),
            (
                "rebind",
                format!(
                    "{:?}",
                    Ok::<_, String>(InflightObservation::Deferred(
                        "existing recovery owner retained the row".into()
                    ))
                )
            ),
        ]
    );
    pool.close().await;
    db.drop().await;
}
