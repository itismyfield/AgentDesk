//! Receipts of the owners that end an execution outside the bridge, and the rows kept marked
//! when that commit fails: who writes which terminal, before the row goes, and who settles it.
use std::sync::Arc;
use std::time::Duration;

use poise::serenity_prelude::ChannelId;

use super::*;
use crate::services::agents::input_result::{InputResult, OriginChannel, lookup};
use crate::services::discord::turn_finalizer::{
    FinalizeContext, SyntheticClaimSnapshot, TerminalEvent, TurnKey,
};

const ORIGIN: &str = "G-settle-origin";

fn blocked_pool() -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(50))
        .connect_lazy("postgres://agentdesk@127.0.0.1:1/blocked")
        .unwrap()
}

fn row(provider: &ProviderKind, channel: u64, origin: Option<&str>) -> InflightTurnState {
    let mut row = InflightTurnState::new(
        provider.clone(),
        channel,
        None,
        7,
        channel + 100,
        8,
        "settle prompt".into(),
        None,
        None,
        None,
        None,
        0,
    );
    row.turn_nonce = Some(format!("nonce-{channel}"));
    row.input_origin_ids = origin.map(str::to_string).into_iter().collect();
    row
}

async fn receipt(pool: &PgPool, row: &InflightTurnState) -> Option<(Vec<String>, String)> {
    sqlx::query_as(
        "SELECT consumed_origin_ids, terminal_outcome FROM turns
         WHERE turn_id = $1 AND terminal_outcome IS NOT NULL",
    )
    .bind(format!("discord:{}:{}", row.channel_id, row.user_msg_id))
    .fetch_optional(pool)
    .await
    .unwrap()
}

fn on_disk(provider: &ProviderKind, row: &InflightTurnState) -> Option<InflightTurnState> {
    inflight::load_inflight_state_read_only(provider, row.channel_id)
}

#[test]
fn each_finalize_event_names_the_terminal_of_the_row_it_removes() {
    let fixed = |event: &TerminalEvent, ctx: FinalizeContext| {
        let clear = FinalizerClear::new(event, ctx);
        (
            clear.clear,
            clear.outcome.map(|outcome| match outcome {
                FinalizerOutcome::Body => None,
                FinalizerOutcome::Fixed(outcome) => Some(outcome),
            }),
        )
    };
    let stale = FinalizeContext::stale_busy_mailbox();
    let watcher = FinalizeContext::watcher();
    assert_eq!(fixed(&TerminalEvent::Complete, stale), (true, Some(None)));
    assert_eq!(
        fixed(&TerminalEvent::Cancel, stale),
        (true, Some(Some(TerminalOutcome::Failed))),
        "a backstop cancel settles a host proven gone"
    );
    assert_eq!(
        fixed(&TerminalEvent::Cancel, watcher),
        (false, Some(Some(TerminalOutcome::Cancelled)))
    );
    let gate = TerminalEvent::GateTimeout {
        pane_quiescent: None,
    };
    assert_eq!(fixed(&gate, stale).1, Some(Some(TerminalOutcome::Failed)));
    assert_eq!(
        fixed(&TerminalEvent::RelayMiss, stale).1,
        Some(Some(TerminalOutcome::Empty))
    );
    #[cfg(unix)]
    assert_eq!(
        fixed(
            &TerminalEvent::HostTerminated,
            FinalizeContext::host_terminated()
        ),
        (true, Some(Some(TerminalOutcome::Cancelled)))
    );
    let (clear, plain) = true.split();
    assert!(
        clear && plain.outcome.is_none(),
        "a plain claim records nothing"
    );
    assert_eq!(watcher_outcome(true, true), TerminalOutcome::Cancelled);
    assert_eq!(watcher_outcome(false, true), TerminalOutcome::Completed);
    assert_eq!(watcher_outcome(false, false), TerminalOutcome::Empty);
}

/// A blocked commit marks only a row that owes an origin its receipt; a row without one is left
/// to its removal as before, and an already marked row keeps its first mark.
#[tokio::test(flavor = "current_thread")]
async fn a_blocked_commit_marks_only_a_row_that_owes_an_origin() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Codex;
    let pool = blocked_pool();
    let owed = row(&provider, 7_340_901, Some(ORIGIN));
    let plain = row(&provider, 7_340_902, None);
    for state in [&owed, &plain] {
        inflight::save_inflight_state(state).unwrap();
    }

    let may_clear = record_or_stamp(Some(&pool), &owed, TerminalOutcome::Cancelled).await;
    assert!(!may_clear, "the owed row stays");
    let mark = on_disk(&provider, &owed).unwrap().terminal_receipt.unwrap();
    assert_eq!(mark.outcome, TerminalOutcome::Cancelled);

    assert!(record_or_stamp(Some(&pool), &plain, TerminalOutcome::Failed).await);
    assert!(
        on_disk(&provider, &plain)
            .unwrap()
            .terminal_receipt
            .is_none()
    );

    let marked = on_disk(&provider, &owed).unwrap();
    assert!(!record_or_stamp(Some(&pool), &marked, TerminalOutcome::Failed).await);
    assert_eq!(
        on_disk(&provider, &owed).unwrap().terminal_receipt,
        Some(mark)
    );

    // With no database there is nothing to keep for.
    assert!(record_or_stamp(None, &plain, TerminalOutcome::Failed).await);
}

/// While the database is unreachable the origin of a marked row still reads terminal from its
/// row, and a running row is still an error rather than a guess.
#[tokio::test(flavor = "current_thread")]
async fn a_marked_row_reads_terminal_without_the_database() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Codex;
    let pool = blocked_pool();
    let mut marked = row(&provider, 7_340_911, Some(ORIGIN));
    let confirmed_at = chrono::Utc::now();
    marked.terminal_receipt = Some(TerminalReceiptStamp {
        outcome: TerminalOutcome::Empty,
        confirmed_at,
    });
    inflight::save_inflight_state(&marked).unwrap();
    let channels = [OriginChannel {
        provider: "codex".into(),
        channel_id: marked.channel_id,
    }];
    let Ok(Some(InputResult::Terminal(found))) = lookup(&pool, "agent", ORIGIN, &channels).await
    else {
        panic!("the mark answers");
    };
    assert_eq!(found.terminal_outcome, "empty");
    assert_eq!(found.terminal_confirmed_at, confirmed_at);

    marked.terminal_receipt = None;
    inflight::save_inflight_state(&marked).unwrap();
    assert!(lookup(&pool, "agent", ORIGIN, &channels).await.is_err());
}

async fn finalize(
    shared: &Arc<SharedData>,
    state: &InflightTurnState,
    event: TerminalEvent,
    ctx: FinalizeContext,
) {
    let key = TurnKey::new(
        ChannelId::new(state.channel_id),
        state.user_msg_id,
        shared.restart.current_generation,
    )
    .with_episode_nonce(state.turn_nonce.as_deref());
    shared
        .turn_finalizer
        .submit_terminal_with_claim_snapshot(
            key,
            state.provider_kind().unwrap(),
            event,
            ctx,
            Some(SyntheticClaimSnapshot::from_row(state)),
            shared.clone(),
        )
        .await;
}

/// A finalize that removes the row commits the terminal its own event names before the row
/// goes; one that leaves the row to its reader records nothing; a blocked commit keeps it marked.
#[tokio::test(flavor = "current_thread")]
async fn a_finalizer_clear_commits_its_event_terminal_before_the_row_goes_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let provider = shared.provider.clone();

    let backstop = row(&provider, 7_340_921, Some(ORIGIN));
    inflight::save_inflight_state(&backstop).unwrap();
    finalize(
        &shared,
        &backstop,
        TerminalEvent::Cancel,
        FinalizeContext::stale_busy_mailbox(),
    )
    .await;
    assert_eq!(
        receipt(&pool, &backstop).await,
        Some((vec![ORIGIN.to_string()], "failed".to_string()))
    );
    assert!(on_disk(&provider, &backstop).is_none());

    #[cfg(unix)]
    {
        let terminated = row(&provider, 7_340_922, Some(ORIGIN));
        inflight::save_inflight_state(&terminated).unwrap();
        let ctx = FinalizeContext::host_terminated();
        finalize(&shared, &terminated, TerminalEvent::HostTerminated, ctx).await;
        assert_eq!(
            receipt(&pool, &terminated).await.map(|found| found.1),
            Some("cancelled".to_string())
        );
        assert!(on_disk(&provider, &terminated).is_none());
    }

    let kept = row(&provider, 7_340_923, Some(ORIGIN));
    inflight::save_inflight_state(&kept).unwrap();
    finalize(
        &shared,
        &kept,
        TerminalEvent::Complete,
        FinalizeContext::watcher(),
    )
    .await;
    assert_eq!(
        receipt(&pool, &kept).await,
        None,
        "the reader owns this ending"
    );
    assert!(on_disk(&provider, &kept).is_some());

    let blocked =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(blocked_pool()));
    let owed = row(&provider, 7_340_924, Some(ORIGIN));
    inflight::save_inflight_state(&owed).unwrap();
    finalize(
        &blocked,
        &owed,
        TerminalEvent::Cancel,
        FinalizeContext::stale_busy_mailbox(),
    )
    .await;
    let mark = on_disk(&provider, &owed).and_then(|row| row.terminal_receipt);
    assert_eq!(mark.map(|mark| mark.outcome), Some(TerminalOutcome::Failed));
    pool.close().await;
    db.drop().await;
}

/// A forced stop that removes the row records the cancel first; a stop that keeps the row for
/// a hand-over records nothing.
#[tokio::test(flavor = "current_thread")]
async fn a_forced_stop_that_removes_the_row_records_the_cancel_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let provider = shared.provider.clone();
    let cleared = row(&provider, 7_340_931, Some(ORIGIN));
    let handed = row(&provider, 7_340_932, Some(ORIGIN));
    for state in [&cleared, &handed] {
        inflight::save_inflight_state(state).unwrap();
    }
    let clear = crate::services::discord::TmuxCleanupPolicy::CleanupSession {
        termination_reason_code: Some("test"),
    };
    stopped(
        &shared,
        &provider,
        ChannelId::new(cleared.channel_id),
        clear,
        None,
    )
    .await;
    let preserve = crate::services::discord::TmuxCleanupPolicy::PreserveSessionAndInflight {
        restart_mode: crate::services::discord::InflightRestartMode::HotSwapHandoff,
    };
    stopped(
        &shared,
        &provider,
        ChannelId::new(handed.channel_id),
        preserve,
        None,
    )
    .await;
    assert_eq!(
        receipt(&pool, &cleared).await.map(|found| found.1),
        Some("cancelled".into())
    );
    assert_eq!(receipt(&pool, &handed).await, None);
    pool.close().await;
    db.drop().await;
}

/// Boot records a marked row with its own mark, an evicted origin row as failed, and a row restore
/// removed without an ending as failed after the restore, unless an earlier receipt exists.
#[tokio::test(flavor = "current_thread")]
async fn a_boot_records_marked_and_silently_removed_rows_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared = crate::services::discord::host_teardown_gate::test_support::shared_on(&pool).await;
    let provider = ProviderKind::Claude;

    let mut marked = row(&provider, 7_340_941, Some(ORIGIN));
    let marked_at = chrono::Utc::now() - chrono::Duration::minutes(5);
    marked.terminal_receipt = Some(TerminalReceiptStamp {
        outcome: TerminalOutcome::Cancelled,
        confirmed_at: marked_at,
    });
    let mut stale = row(&provider, 7_340_942, Some(ORIGIN));
    stale.restart_generation = Some(shared.restart.current_generation.saturating_sub(1).max(1));
    let mut unknown = row(&provider, 7_340_943, Some(ORIGIN));
    unknown.version = inflight::inflight_state_version() + 1;
    unknown.runtime_kind = None;
    let mut answered = row(&provider, 7_340_944, Some(ORIGIN));
    answered.version = unknown.version;
    answered.runtime_kind = None;
    for state in [&marked, &stale, &unknown, &answered] {
        inflight::save_inflight_state(state).unwrap();
    }
    record(
        Some(&pool),
        &answered,
        TerminalOutcome::Completed,
        chrono::Utc::now(),
    )
    .await;
    let current = shared
        .restart
        .current_generation
        .max(stale.restart_generation.unwrap() + 1);
    inflight::invalidate_stale_generation(&provider, current);
    assert!(
        on_disk(&provider, &stale)
            .unwrap()
            .terminal_receipt
            .is_some()
    );

    let discord =
        crate::services::discord::recovery_engine::o_cut_recorder::start(marked.channel_id).await;
    crate::services::discord::recovery_engine::restore_inflight_turns(
        &discord.http,
        &shared,
        &provider,
    )
    .await;

    let at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT terminal_confirmed_at FROM turns WHERE turn_id = $1")
            .bind(format!(
                "discord:{}:{}",
                marked.channel_id, marked.user_msg_id
            ))
            .fetch_optional(&pool)
            .await
            .unwrap()
            .flatten();
    assert_eq!(
        at.map(|at| at.timestamp_micros()),
        Some(marked_at.timestamp_micros())
    );
    for (state, outcome) in [
        (&marked, "cancelled"),
        (&stale, "failed"),
        (&unknown, "failed"),
        (&answered, "completed"),
    ] {
        assert_eq!(
            receipt(&pool, state).await,
            Some((vec![ORIGIN.to_string()], outcome.to_string())),
            "channel {}",
            state.channel_id
        );
        assert!(
            on_disk(&provider, state).is_none(),
            "channel {}",
            state.channel_id
        );
    }
    assert!(
        discord.contents().is_empty(),
        "no settled row was revived or relayed"
    );
    pool.close().await;
    db.drop().await;
}
