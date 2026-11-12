//! The adoption wake: recovery gets only a row no live turn here holds, and an adoption commit
//! settles a row kept for it once, with no Legacy body sent.

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::discord::recovery_engine::o_cut_recorder;
use crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root;
use crate::services::tui_o::cutover::test_override;
use crate::test_support::wait::{WAIT_CAP, await_within};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};

const BODY: &str = "ADK-wake-kept-body";

/// A Claude TUI turn whose tmux is gone, as the boot recovery saw it: answered but not relayed.
fn kept_row(channel: u64) -> InflightTurnState {
    let tmux = format!("AgentDesk-claude-o-wake-{channel}");
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        7,
        channel + 1,
        0,
        "prompt".to_string(),
        None,
        Some(tmux),
        None,
        None,
        64,
    );
    row.full_response = BODY.to_string();
    row.runtime_kind = Some(ClaudeTui);
    inflight::save_inflight_state(&row).unwrap();
    row
}

fn watcher(tmux: &str, cancelled: bool) -> crate::services::discord::TmuxWatcherHandle {
    crate::services::discord::TmuxWatcherHandle {
        tmux_session_name: tmux.to_owned(),
        output_path: String::new(),
        paused: Arc::new(AtomicBool::new(false)),
        resume_offset: Arc::new(std::sync::Mutex::new(None)),
        cancel: Arc::new(AtomicBool::new(cancelled)),
        pause_epoch: Arc::new(AtomicU64::new(0)),
        turn_delivered: Arc::new(AtomicBool::new(false)),
        last_heartbeat_ts_ms: Arc::new(AtomicI64::new(chrono::Utc::now().timestamp_millis())),
    }
}

/// Only an idle mailbox and no live watcher on the row's tmux leave the row to the wake.
#[tokio::test(flavor = "current_thread")]
async fn only_a_row_no_live_turn_holds_is_left_to_the_adoption_wake() {
    with_isolated_runtime_root(|| async move {
        let channel = 9_765_101;
        let row = kept_row(channel);
        let tmux = row.tmux_session_name.clone().unwrap();
        let kept = |shared: Arc<SharedData>| async move {
            let kept = kept_rows(&shared, &ProviderKind::Claude, channel).await;
            kept.map_or(0, |rows| rows.len())
        };
        let cases = [
            ("no watcher", None, 1),
            (
                "another tmux",
                Some(watcher("AgentDesk-claude-other", false)),
                1,
            ),
            ("cancelled", Some(watcher(&tmux, true)), 1),
            ("live watcher", Some(watcher(&tmux, false)), 0),
        ];
        for (case, handle, expected) in cases {
            let shared = crate::services::discord::make_shared_data_for_tests();
            if let Some(handle) = handle {
                shared.tmux_watchers.insert(ChannelId::new(channel), handle);
            }
            assert_eq!(kept(shared).await, expected, "{case}");
        }
        let shared = crate::services::discord::make_shared_data_for_tests();
        let token = Arc::new(crate::services::provider::CancelToken::new());
        let (owner, message) = (serenity::UserId::new(1), serenity::MessageId::new(10));
        let claim = crate::services::discord::mailbox_try_start_turn;
        assert!(claim(&shared, ChannelId::new(channel), token, owner, message).await);
        assert_eq!(
            kept(shared).await,
            0,
            "an active mailbox turn holds the row"
        );
    })
    .await;
}

/// A row the boot kept for a pending adoption is settled by the adopted writer's ready wake alone.
#[tokio::test(flavor = "current_thread")]
async fn an_adoption_commit_settles_the_kept_row_once_without_a_legacy_body_pg() {
    with_isolated_runtime_root(|| async move {
        let provider = ProviderKind::Claude;
        let channel = 9_765_111;
        // The keyed teardown gate confirms the dead tmux against PG's sessions table.
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "adoption_wake",
            "kept row wake",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let _pending = test_override::force_candidates(&[(channel, ClaudeTui)]);
        let row = kept_row(channel);
        // The turn's tmux is gone, so recovery takes the missing-session branch.
        use crate::services::session_host::test_support::{
            InjectedLivenessGuard, InjectedPresenceGuard,
        };
        use crate::services::session_host::{HostLiveness, HostPresence, HostSessionRef};
        let session = HostSessionRef::tmux(row.tmux_session_name.as_deref().unwrap());
        let _dead = InjectedLivenessGuard::set(session.clone(), HostLiveness::DeadOrAbsent);
        let _gone = InjectedPresenceGuard::set(session, HostPresence::Missing);
        let recorder = o_cut_recorder::start(channel).await;
        let restore = crate::services::discord::recovery_engine::restore_serially;
        restore(&recorder.http, &shared, &provider, vec![row.clone()]).await;
        let still = inflight::load_inflight_state(&provider, channel);
        assert_eq!(still.map(|row| row.recovery_relay_attempts), Some(0));
        assert!(recorder.contents().is_empty(), "{:?}", recorder.contents());

        let root = crate::config::runtime_root().unwrap();
        crate::test_support::o_store::commit_o_store(&root, &[channel]);
        let snapshot = crate::services::tui_o::cutover::boot_snapshot().unwrap();
        assert!(snapshot.candidate(channel).unwrap().confirm_store());
        let host = GatewayHost {
            shared: Arc::clone(&shared),
            execution: BTreeMap::new(),
            alarms: Arc::new(AlarmRouter::for_process(None, None)),
            self_id_wait: Duration::ZERO,
            configured_id: None,
            turn: TurnConfig::off(),
            rest: Some(Arc::clone(&recorder.http)),
            boot: None,
        };
        let began = std::time::Instant::now();
        host.input_ready(channel, ShadowProvider::Claude);
        let settled = async {
            while inflight::load_inflight_state(&provider, channel).is_some() {
                tokio::task::yield_now().await;
            }
        };
        await_within(settled, WAIT_CAP).await;
        eprintln!("WAKE_SETTLED_AFTER {:?}", began.elapsed());
        // Legacy shows only the body-free recovery marker; the body is O's to post.
        let contents = recorder.contents();
        assert_eq!(contents.len(), 1, "{contents:?}");
        assert!(!contents[0].contains(BODY), "{contents:?}");
        pool.close().await;
        db.drop().await;
    })
    .await;
}

thread_local! {
    static KICKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static JUDGED: std::cell::RefCell<Option<tokio::sync::mpsc::UnboundedSender<bool>>> =
        const { std::cell::RefCell::new(None) };
}

/// Reports each wake judgement to a test waiting on it: whether a claim held the row.
pub(super) fn judged(judged: &Result<Vec<InflightTurnState>, Held>) {
    let claimed = matches!(judged, Err(Held::Claimed));
    JUDGED.with(|slot| slot.borrow().as_ref().map(|tx| tx.send(claimed)));
}

/// Counts the wake's queue kicks for the input a claim gave back.
pub(super) fn kicked() {
    KICKS.with(|kicks| kicks.set(kicks.get() + 1));
}

/// An O-owned channel's kept row whose tmux is gone, recovered through `recorder`.
struct Rig {
    shared: Arc<SharedData>,
    row: InflightTurnState,
    recorder: o_cut_recorder::DiscordRecorder,
    db: crate::dispatch::test_support::DispatchPostgresTestDb,
    pool: sqlx::PgPool,
    _owned: test_override::ChannelsGuard,
    _dead: crate::services::session_host::test_support::InjectedLivenessGuard,
    _gone: crate::services::session_host::test_support::InjectedPresenceGuard,
}

impl Rig {
    async fn new(channel: u64, first: Arc<o_cut_recorder::FirstPost>) -> Self {
        use crate::services::session_host::test_support::{
            InjectedLivenessGuard, InjectedPresenceGuard,
        };
        use crate::services::session_host::{HostLiveness, HostPresence, HostSessionRef};
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "adoption_wake_retry",
            "kept row wake retry",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let _owned = test_override::force_channels(&[(channel, ClaudeTui)]);
        let row = kept_row(channel);
        let session = HostSessionRef::tmux(row.tmux_session_name.as_deref().unwrap());
        let _dead = InjectedLivenessGuard::set(session.clone(), HostLiveness::DeadOrAbsent);
        let _gone = InjectedPresenceGuard::set(session, HostPresence::Missing);
        let recorder = o_cut_recorder::start_first_post(channel, first).await;
        Self {
            shared,
            row,
            recorder,
            db,
            pool,
            _owned,
            _dead,
            _gone,
        }
    }

    async fn settled(&self) {
        let provider = ProviderKind::Claude;
        let channel = self.row.channel_id;
        let settled = async {
            while inflight::load_inflight_state(&provider, channel).is_some() {
                tokio::task::yield_now().await;
            }
        };
        await_within(settled, WAIT_CAP).await;
    }

    async fn drop_db(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

/// A queued input's claim that lets go after the wake saw it leaves the row to a later
/// judgement of the same wake: no new ready, input or restart is needed to settle it.
#[tokio::test(flavor = "current_thread")]
async fn a_claim_that_lets_go_after_the_wake_saw_it_still_settles_the_kept_row_pg() {
    with_isolated_runtime_root(|| async move {
        let channel = 9_765_121;
        let rig = Rig::new(channel, Arc::default()).await;
        let (tx, mut judged) = tokio::sync::mpsc::unbounded_channel();
        JUDGED.with(|slot| *slot.borrow_mut() = Some(tx));
        let id = ChannelId::new(channel);
        let token = Arc::new(crate::services::provider::CancelToken::new());
        let (owner, message) = (serenity::UserId::new(1), serenity::MessageId::new(10));
        let claim = crate::services::discord::mailbox_try_start_turn;
        assert!(claim(&rig.shared, id, token, owner, message).await);
        let http = Some(Arc::clone(&rig.recorder.http));
        spawn(&rig.shared, http, ProviderKind::Claude, channel);
        let first = await_within(judged.recv(), WAIT_CAP).await;
        assert_eq!(first, Some(true), "the claim holds the first judgement");
        let persistence = crate::services::discord::queue_persistence_context(
            &rig.shared,
            &ProviderKind::Claude,
            id,
        );
        rig.shared.mailbox(id).finish_turn(persistence).await;
        rig.settled().await;
        // The input that gave its claim back is kicked again once the row is gone.
        let kicked = async {
            while KICKS.with(std::cell::Cell::get) == 0 {
                tokio::task::yield_now().await;
            }
        };
        await_within(kicked, WAIT_CAP).await;
        assert_eq!(KICKS.with(std::cell::Cell::get), 1);
        let contents = rig.recorder.contents();
        assert_eq!(contents.len(), 1, "{contents:?}");
        assert!(!contents[0].contains(BODY), "{contents:?}");
        JUDGED.with(|slot| *slot.borrow_mut() = None);
        rig.drop_db().await;
    })
    .await;
}

/// A recovery send that fails transiently is judged again by the same wake and then settles.
#[tokio::test(flavor = "current_thread")]
async fn a_transient_recovery_failure_is_judged_again_and_settles_the_kept_row_pg() {
    with_isolated_runtime_root(|| async move {
        let channel = 9_765_131;
        let first = o_cut_recorder::FirstPost::failing();
        let rig = Rig::new(channel, Arc::clone(&first)).await;
        let http = Some(Arc::clone(&rig.recorder.http));
        spawn(&rig.shared, http, ProviderKind::Claude, channel);
        rig.settled().await;
        assert_eq!(KICKS.with(std::cell::Cell::get), 0, "no claim, no kick");
        assert_eq!(first.posts(), 2, "{:?}", rig.recorder.contents());
        rig.drop_db().await;
    })
    .await;
}

/// Recovery sends that keep failing are judged once and again after 1, 2 and 3 s; then the wake
/// ends with the row and its restart budget as they were, and kicks nothing.
#[tokio::test(flavor = "current_thread")]
async fn a_wake_whose_sends_keep_failing_ends_after_three_retries_and_keeps_the_row_pg() {
    with_isolated_runtime_root(|| async move {
        let channel = 9_765_141;
        let first = o_cut_recorder::FirstPost::failing_always();
        let rig = Rig::new(channel, Arc::clone(&first)).await;
        let (tx, mut judged) = tokio::sync::mpsc::unbounded_channel();
        JUDGED.with(|slot| *slot.borrow_mut() = Some(tx));
        let http = Arc::clone(&rig.recorder.http);
        let times = async {
            let mut at = Vec::new();
            while at.len() < 4 && judged.recv().await.is_some() {
                at.push(std::time::Instant::now());
            }
            at
        };
        // Ending the wake drops the sender, so fewer judgements show as a short list, not a wait.
        let woke = async {
            wake(&http, &rig.shared, &ProviderKind::Claude, channel).await;
            JUDGED.with(|slot| *slot.borrow_mut() = None);
        };
        let ((), at) = tokio::join!(await_within(woke, WAIT_CAP), await_within(times, WAIT_CAP));
        assert_eq!(at.len(), 4, "the first judgement and three retries");
        for (gap, pair) in at.windows(2).enumerate() {
            let backoff = BACKOFF * (gap as u32 + 1);
            assert!(
                pair[1] - pair[0] >= backoff,
                "retry {gap} waited {backoff:?}"
            );
        }
        assert_eq!(first.posts(), 4, "{:?}", rig.recorder.contents());
        let row = inflight::load_inflight_state(&ProviderKind::Claude, channel);
        let kept = row.expect("the exhausted wake keeps the row");
        assert_eq!(
            kept.recovery_relay_attempts, 0,
            "and spends no restart budget"
        );
        let contents = rig.recorder.contents();
        assert!(
            !contents.iter().any(|text| text.contains(BODY)),
            "{contents:?}"
        );
        assert_eq!(KICKS.with(std::cell::Cell::get), 0, "no claim, no kick");
        JUDGED.with(|slot| *slot.borrow_mut() = None);
        rig.drop_db().await;
    })
    .await;
}
