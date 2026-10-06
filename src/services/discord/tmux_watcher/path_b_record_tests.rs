use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

const TMUX: &str = "AgentDesk-claude-routine-path-b";
const CHANNEL: u64 = 5_707_401;
const LATER_DISPATCH: &str = "dispatch-B-created-after-A";

fn row(user_msg_id: u64, turn_start_offset: u64) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        CHANNEL,
        None,
        7,
        user_msg_id,
        user_msg_id + 1,
        "routine prompt".into(),
        None,
        Some(TMUX.into()),
        None,
        None,
        turn_start_offset,
    );
    state.turn_start_offset = Some(turn_start_offset);
    state
}

fn select<'a>(
    late: Option<&'a InflightTurnState>,
    pin: Option<&'a InflightTurnState>,
) -> Option<(u64, PathBSource)> {
    PathBRecord::select(late, pin, TMUX, 100)
        .map(|record| (record.state.user_msg_id, record.source))
}

#[test]
fn path_b_records_the_late_row_else_only_the_pin_of_this_committed_range() {
    let late = row(11, 10);
    let pin = row(22, 10);
    assert_eq!(
        select(Some(&late), Some(&pin)),
        Some((11, PathBSource::Late))
    );
    assert_eq!(select(None, Some(&pin)), Some((22, PathBSource::Pinned)));
    assert_eq!(select(None, None), None);

    let newer = row(33, 100);
    assert_eq!(
        select(None, Some(&newer)),
        None,
        "a turn starting at the range end is newer"
    );
    let mut other_tmux = row(44, 10);
    other_tmux.tmux_session_name = Some("AgentDesk-claude-other".into());
    assert_eq!(select(None, Some(&other_tmux)), None);
    let anchorless = row(0, 10);
    assert_eq!(select(None, Some(&anchorless)), None);
    let mut synthetic = row(55, 10);
    synthetic.rebind_origin = true;
    assert_eq!(select(None, Some(&synthetic)), None);
}

async fn create_pool() -> (
    crate::dispatch::test_support::DispatchPostgresTestDb,
    sqlx::PgPool,
) {
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "agentdesk_path_b_dispatch_5707",
        "watcher path b dispatch attribution",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    (db, pool)
}

async fn stored(pool: &sqlx::PgPool, user_msg_id: u64) -> (Option<String>, String) {
    sqlx::query_as::<_, (Option<String>, String)>(
        "SELECT dispatch_id, assistant_message FROM session_transcripts WHERE turn_id = $1",
    )
    .bind(format!("discord:{CHANNEL}:{user_msg_id}"))
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn persist(pool: &sqlx::PgPool, record: &PathBRecord<'_>, fallback_called: &AtomicBool) {
    record
        .persist_transcript(
            PathBTranscript {
                pool: Some(pool),
                channel_id: ChannelId::new(CHANNEL),
                provider: &ProviderKind::Claude,
                agent_id: None,
                assistant_message: "watcher body",
                events: &[],
            },
            || {
                fallback_called.store(true, Ordering::SeqCst);
                async { Some(LATER_DISPATCH.to_string()) }
            },
        )
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pinned_record_never_borrows_a_later_thread_dispatch_pg() {
    let (db, pool) = create_pool().await;
    let called = AtomicBool::new(false);

    // Turn A had no dispatch; B is the thread's newer pending dispatch.
    let a = row(1_420_000_000_000_000_001, 10);
    persist(
        &pool,
        &PathBRecord::select(None, Some(&a), TMUX, 100).unwrap(),
        &called,
    )
    .await;
    assert_eq!(stored(&pool, a.user_msg_id).await.0, None);
    assert!(
        !called.load(Ordering::SeqCst),
        "a pinned record must not consult the thread"
    );

    // A dispatch another writer already stored for the turn survives a pinned rewrite.
    let b = row(1_420_000_000_000_000_002, 10);
    crate::db::session_transcripts::persist_turn_db(
        Some(&pool),
        crate::db::session_transcripts::PersistSessionTranscript {
            turn_id: &format!("discord:{CHANNEL}:{}", b.user_msg_id),
            session_key: None,
            channel_id: Some(&CHANNEL.to_string()),
            agent_id: None,
            provider: Some("claude"),
            dispatch_id: Some("dispatch-A"),
            user_message: "routine prompt",
            assistant_message: "bridge body",
            events: &[],
            duration_ms: None,
            turn_started_at_millis: None,
        },
    )
    .await
    .unwrap();
    persist(
        &pool,
        &PathBRecord::select(None, Some(&b), TMUX, 100).unwrap(),
        &called,
    )
    .await;
    assert_eq!(
        stored(&pool, b.user_msg_id).await,
        (Some("dispatch-A".to_string()), "watcher body".to_string())
    );

    // The pin's own dispatch, or its prompt's DISPATCH: prefix, is the turn's.
    let mut c = row(1_420_000_000_000_000_003, 10);
    c.dispatch_id = Some("dispatch-A".into());
    persist(
        &pool,
        &PathBRecord::select(None, Some(&c), TMUX, 100).unwrap(),
        &called,
    )
    .await;
    assert_eq!(
        stored(&pool, c.user_msg_id).await.0.as_deref(),
        Some("dispatch-A")
    );
    let mut d = row(1_420_000_000_000_000_004, 10);
    d.user_text = "DISPATCH:dispatch-D - title".into();
    persist(
        &pool,
        &PathBRecord::select(None, Some(&d), TMUX, 100).unwrap(),
        &called,
    )
    .await;
    assert_eq!(
        stored(&pool, d.user_msg_id).await.0.as_deref(),
        Some("dispatch-D")
    );
    assert!(!called.load(Ordering::SeqCst));

    // A late row keeps the thread fallback it always had.
    let e = row(1_420_000_000_000_000_005, 10);
    persist(
        &pool,
        &PathBRecord::select(Some(&e), None, TMUX, 100).unwrap(),
        &called,
    )
    .await;
    assert_eq!(
        stored(&pool, e.user_msg_id).await.0.as_deref(),
        Some(LATER_DISPATCH)
    );
    assert!(called.load(Ordering::SeqCst));

    pool.close().await;
    db.drop().await;
}
