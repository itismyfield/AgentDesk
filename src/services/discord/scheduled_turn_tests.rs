use super::*;
use crate::services::provider::ProviderKind;
use crate::services::scheduled_messages::turn_completion::tests::{
    launch_agent_delivery, stored_outcome,
};
use std::sync::atomic::{AtomicUsize, Ordering};

fn row(channel_id: u64, mark: ScheduledTurnMark) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        channel_id,
        None,
        7,
        channel_id + 1,
        channel_id + 2,
        "scheduled prompt".to_string(),
        Some("snapshot-provider-session".to_string()),
        None,
        None,
        None,
        0,
    );
    state.scheduled = mark;
    state
}

fn scheduled(
    turn_id: &str,
    isolated: bool,
    outcome: Option<ScheduledTurnOutcome>,
) -> ScheduledTurnMark {
    ScheduledTurnMark::Scheduled {
        turn_id: turn_id.to_string(),
        isolated,
        outcome,
    }
}

/// A pool whose every query fails, like a database outage mid-completion.
fn unreachable_pool() -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy("postgres://agentdesk@127.0.0.1:1/unreachable")
        .expect("lazy pool")
}

#[test]
fn a_row_written_before_the_mark_reads_as_legacy_and_new_marks_round_trip() {
    let mut value = serde_json::to_value(row(1, ScheduledTurnMark::NotScheduled)).unwrap();
    value.as_object_mut().unwrap().remove("scheduled");
    let old: InflightTurnState = serde_json::from_value(value).unwrap();
    assert_eq!(old.scheduled, ScheduledTurnMark::Legacy);

    let marked = row(1, scheduled("t", true, Some(ScheduledTurnOutcome::NoReply)));
    let back: InflightTurnState =
        serde_json::from_str(&serde_json::to_string(&marked).unwrap()).unwrap();
    assert_eq!(back.scheduled, marked.scheduled);
}

#[test]
fn only_a_scheduled_or_snapshot_headless_start_is_marked_scheduled() {
    let metadata = serde_json::json!({ "scheduled_message_id": "sm-1" });
    assert_eq!(
        ScheduledTurnMark::at_headless_start(Some(&metadata), "t".into(), false),
        scheduled("t", false, None)
    );
    assert_eq!(
        ScheduledTurnMark::at_headless_start(None, "t".into(), true),
        scheduled("t", true, None)
    );
    let unrelated = serde_json::json!({ "agent_id": "a" });
    assert_eq!(
        ScheduledTurnMark::at_headless_start(Some(&unrelated), "t".into(), false),
        ScheduledTurnMark::NotScheduled
    );
}

#[tokio::test]
async fn channel_effects_follow_the_mark_and_fail_closed_on_a_legacy_lookup_error() {
    let pool = unreachable_pool();
    let not_scheduled = row(11, ScheduledTurnMark::NotScheduled);
    let legacy = row(12, ScheduledTurnMark::Legacy);
    let isolated = row(13, scheduled("t", true, None));
    let shared_context = row(14, scheduled("t", false, None));
    let effects = |rows: Vec<&InflightTurnState>| {
        let rows: Vec<Option<InflightTurnState>> = rows.into_iter().cloned().map(Some).collect();
        let pool = pool.clone();
        async move {
            let rows: Vec<_> = rows.iter().map(Option::as_ref).collect();
            channel_effects(Some(&pool), &rows).await
        }
    };

    // A human turn never asks the database, so an outage cannot withhold its writeback.
    assert_eq!(effects(vec![&not_scheduled]).await, ChannelEffects::Allowed);
    assert_eq!(
        effects(vec![&shared_context]).await,
        ChannelEffects::Allowed
    );
    assert_eq!(effects(vec![&isolated]).await, ChannelEffects::Suppressed);
    assert_eq!(
        effects(vec![&legacy]).await,
        ChannelEffects::Suppressed,
        "an unresolvable legacy row must not write channel context"
    );
    assert_eq!(
        effects(vec![&not_scheduled, &isolated]).await,
        ChannelEffects::Suppressed,
        "any isolated candidate row of the completion suppresses"
    );
    assert_eq!(
        channel_effects(None, &[Some(&legacy)]).await,
        ChannelEffects::Allowed,
        "without PostgreSQL no row is scheduled"
    );
    assert_eq!(channel_session_seed(Some(&pool), &isolated).await, None);
    assert_eq!(
        channel_session_seed(Some(&pool), &not_scheduled)
            .await
            .as_deref(),
        Some("snapshot-provider-session")
    );
}

#[tokio::test(start_paused = true)]
async fn record_retries_a_transient_error_and_accepts_loss_after_its_budget() {
    let attempts = AtomicUsize::new(0);
    let transient = record_with_retry(|| {
        let n = attempts.fetch_add(1, Ordering::SeqCst);
        async move {
            if n == 0 {
                Err(sqlx::Error::PoolTimedOut)
            } else {
                Ok(RecordOutcome::Recorded)
            }
        }
    })
    .await;
    assert_eq!(transient, Settlement::Evidence);
    assert_eq!(attempts.swap(0, Ordering::SeqCst), 2);

    let exhausted = record_with_retry(|| {
        attempts.fetch_add(1, Ordering::SeqCst);
        async { Err::<RecordOutcome, _>(sqlx::Error::PoolTimedOut) }
    })
    .await;
    assert_eq!(exhausted, Settlement::Failed);
    assert_eq!(
        attempts.swap(0, Ordering::SeqCst),
        RECORD_RETRY_DELAYS.len()
    );

    let not_owned = record_with_retry(|| {
        attempts.fetch_add(1, Ordering::SeqCst);
        async { Ok(RecordOutcome::NotOwned) }
    })
    .await;
    assert_eq!(not_owned, Settlement::NotOwned);
    assert_eq!(attempts.load(Ordering::SeqCst), 1, "NotOwned is final");
}

#[tokio::test]
async fn settle_records_nothing_for_a_dead_database_and_never_claims_evidence() {
    let pool = unreachable_pool();
    let state = row(
        21,
        scheduled("t", true, Some(ScheduledTurnOutcome::Delivered)),
    );
    let started = tokio::time::Instant::now();
    assert_eq!(
        settle(Some(&pool), &state, state.scheduled.outcome()).await,
        Some(Settlement::Failed)
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the retry budget is bounded"
    );
}

#[tokio::test]
async fn boot_settlement_records_each_committed_row_with_its_stored_outcome_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (agent, turn) in [
        ("boot-marked", "boot-marked-turn"),
        ("boot-unclassified", "boot-unclassified-turn"),
        ("boot-uncommitted", "boot-uncommitted-turn"),
    ] {
        launch_agent_delivery(&pool, agent, turn, true, "fail").await;
    }
    let legacy_turn = "discord:31:32";
    launch_agent_delivery(&pool, "boot-legacy", legacy_turn, false, "fail").await;

    let mut marked = row(
        41,
        scheduled(
            "boot-marked-turn",
            true,
            Some(ScheduledTurnOutcome::ProviderError),
        ),
    );
    marked.full_response = "an ordinary looking answer".to_string();
    marked.terminal_delivery_committed = true;
    let mut unclassified = row(42, scheduled("boot-unclassified-turn", true, None));
    unclassified.full_response = "answer".to_string();
    unclassified.terminal_delivery_committed = true;
    let mut uncommitted = row(
        43,
        scheduled(
            "boot-uncommitted-turn",
            true,
            Some(ScheduledTurnOutcome::Delivered),
        ),
    );
    uncommitted.full_response = "answer".to_string();
    let mut legacy = row(31, ScheduledTurnMark::Legacy);
    legacy.user_msg_id = 32;
    legacy.full_response = "NO_REPLY".to_string();
    legacy.terminal_delivery_committed = true;

    settle_loaded(Some(&pool), &[marked, unclassified, uncommitted, legacy]).await;

    assert_eq!(
        stored_outcome(&pool, "boot-marked-turn").await.as_deref(),
        Some("provider_error"),
        "the outcome stored with the commit wins over the body"
    );
    assert_eq!(stored_outcome(&pool, "boot-unclassified-turn").await, None);
    assert_eq!(stored_outcome(&pool, "boot-uncommitted-turn").await, None);
    assert_eq!(
        stored_outcome(&pool, legacy_turn).await.as_deref(),
        Some("no_reply")
    );

    pool.close().await;
    db.drop().await;
}

/// Restart right after a delivered snapshot answer's commit but before its
/// postlude record: the boot scan settles it before the committed row is cleared.
#[tokio::test]
async fn restart_after_the_delivery_commit_settles_the_turn_without_resending_pg() {
    use crate::services::discord::host_teardown_gate::test_support::{
        Stored, channel_key, seed, shared_on,
    };
    use crate::services::discord::recovery_engine::{o_cut_recorder, restore_inflight_turns};
    use crate::services::session_host::test_support::{
        InjectedLivenessGuard, InjectedPresenceGuard,
    };
    use crate::services::session_host::{HostLiveness, HostPresence, HostSessionRef};

    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let launched =
        launch_agent_delivery(&pool, "restart-c3", "restart-c3-turn", true, "fail").await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let channel = 1_479_742_000_000_000_001_u64;
    let name = provider.build_tmux_session_name("i7420-restart-c3");
    seed(
        &pool,
        &channel_key(&shared, &name),
        &name,
        channel,
        Stored::Legacy,
    )
    .await;
    let _pane = InjectedLivenessGuard::set(HostSessionRef::tmux(&name), HostLiveness::Live);
    let _presence = InjectedPresenceGuard::set(HostSessionRef::tmux(&name), HostPresence::Present);
    let mut state = row(
        channel,
        scheduled(
            "restart-c3-turn",
            true,
            Some(ScheduledTurnOutcome::Delivered),
        ),
    );
    state.tmux_session_name = Some(name.clone());
    state.born_generation = 0;
    state.full_response = "the scheduled snapshot answer".to_string();
    state.response_sent_offset = state.full_response.len();
    state.terminal_delivery_committed = true;
    crate::services::discord::inflight::save_inflight_state_create_new(&state).unwrap();
    let discord = o_cut_recorder::start(channel).await;

    restore_inflight_turns(&discord.http, &shared, &provider).await;

    assert_eq!(
        stored_outcome(&pool, "restart-c3-turn").await.as_deref(),
        Some("delivered")
    );
    assert!(crate::services::discord::inflight::load_inflight_state(&provider, channel).is_none());
    assert!(
        discord.contents().is_empty(),
        "the committed answer is not sent again: {:?}",
        discord.contents()
    );
    let snapshot = crate::services::discord::mailbox_snapshot(
        &shared,
        poise::serenity_prelude::ChannelId::new(channel),
    )
    .await;
    assert!(
        snapshot.cancel_token.is_none(),
        "recovery starts no provider turn"
    );
    let side_effects: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM message_outbox), (SELECT COUNT(*) FROM intake_outbox)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        side_effects,
        (0, 0),
        "no outbox body and no replayed intake"
    );
    let channel_session = shared
        .core
        .lock()
        .await
        .sessions
        .get(&poise::serenity_prelude::ChannelId::new(channel))
        .and_then(|session| session.session_id.clone());
    assert_eq!(
        channel_session, None,
        "the snapshot session never seeds the channel"
    );

    // The scheduler side relaunches nothing: the launch-committed delivery is
    // neither re-claimed nor lease-recovered.
    use crate::db::scheduled_messages as smdb;
    let reclaimed = smdb::claim_due_fires_pg(
        &pool,
        "next-boot",
        true,
        10,
        60,
        chrono::Utc::now() + chrono::Duration::hours(2),
    )
    .await
    .unwrap();
    assert!(reclaimed.is_empty());
    smdb::recover_expired_leases_pg(&pool).await.unwrap();
    let deliveries = smdb::list_deliveries_pg(&pool, &launched.message_id, 10, None)
        .await
        .unwrap();
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].status, "running");

    pool.close().await;
    db.drop().await;
}
