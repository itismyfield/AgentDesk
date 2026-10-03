//! A stop's finish acts only on the turn it judged: a successor admitted between the judgement
//! and the finish, or a finish the actor does not answer, leaves the channel's turn in place.

use std::cell::RefCell;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::FutureExt;
use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use crate::services::discord::health::HealthRegistry;
use crate::services::discord::{self as discord, SharedData, inflight};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_lifecycle::{TurnLifecycleStopResult, TurnLifecycleTarget};
use crate::services::turn_orchestrator::ChannelMailboxRegistry;

/// Where a stop runs a test's hook on its channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Seam {
    BeforeJudge,
    AfterJudge,
}

type Hook = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

thread_local! {
    static HOOKS: RefCell<Vec<(ChannelId, Seam, Hook)>> = const { RefCell::new(Vec::new()) };
}

/// Runs `hook` once, the next time a stop on `channel` reaches `seam`.
pub(crate) fn at<F>(channel: ChannelId, seam: Seam, hook: impl FnOnce() -> F + Send + 'static)
where
    F: Future<Output = ()> + Send + 'static,
{
    let hook: Hook = Box::new(move || Box::pin(hook()));
    HOOKS.with_borrow_mut(|hooks| hooks.push((channel, seam, hook)));
}

pub(super) async fn seam(channel: ChannelId, seam: Seam) {
    let hook = HOOKS.with_borrow_mut(|hooks| {
        let at = hooks
            .iter()
            .position(|(c, s, _)| (*c, *s) == (channel, seam))?;
        Some(hooks.remove(at).2)
    });
    if let Some(hook) = hook {
        hook().await;
    }
}

/// A Claude runtime whose channel runs the process turn `judged`, with the provider session,
/// start time, inflight row and soft queue item the stop must leave to a successor.
pub(crate) struct Channel {
    pub(crate) shared: Arc<SharedData>,
    pub(crate) registry: Arc<HealthRegistry>,
    pub(crate) channel: ChannelId,
    pub(crate) judged: Arc<CancelToken>,
}

const QUEUED: u64 = 7;

impl Channel {
    pub(crate) async fn new(channel: u64) -> Self {
        let shared = discord::make_shared_data_for_tests();
        let registry = Arc::new(HealthRegistry::new());
        registry
            .register("claude".to_string(), shared.clone())
            .await;
        let channel = ChannelId::new(channel);
        let judged = Arc::new(CancelToken::new());
        let user_msg = MessageId::new(channel.get() + 1);
        let start = discord::mailbox_try_start_turn;
        assert!(start(&shared, channel, judged.clone(), UserId::new(7), user_msg).await);
        super::super::stall_watchdog_auto_heal_tests::seed_runtime_session(&shared, channel).await;
        shared
            .turn_start_times
            .insert(channel, std::time::Instant::now());
        let queued = ChannelMailboxRegistry::queued_for_test(channel.get() + QUEUED);
        let persistence = discord::queue_persistence_context(&shared, &claude(), channel);
        shared
            .mailbox(channel)
            .replace_queue(vec![queued], persistence)
            .await;
        Self::row(channel, 1);
        Self {
            shared,
            registry,
            channel,
            judged,
        }
    }

    /// The channel's inflight row for the turn admitted `n`th, with no session name.
    pub(crate) fn row(channel: ChannelId, n: u64) -> PathBuf {
        let root = inflight::inflight_runtime_root().expect("inflight root");
        let path = inflight::inflight_state_path(&root, &claude(), channel.get());
        let _ = std::fs::remove_file(&path);
        let user_msg = channel.get() + n;
        let text = format!("judged finish turn {n}");
        let row = inflight::InflightTurnState::new(
            claude(),
            channel.get(),
            None,
            1,
            user_msg,
            user_msg + 100,
            text,
            None,
            None,
            None,
            Some("fifo".to_string()),
            0,
        );
        inflight::save_inflight_state_create_new(&row).expect("persist the inflight row");
        path
    }

    /// Waits until the stop raised the judged turn's cancel flag, then admits a successor.
    pub(crate) async fn admit_successor_after_cancel(&self) -> Successor {
        self.wait_judged_cancelled().await;
        admit_successor(&self.shared, self.channel).await
    }

    pub(crate) async fn wait_judged_cancelled(&self) {
        while !self.judged.cancelled.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

    /// The successor `next` holds the channel with its row, session and start time, untouched.
    pub(crate) async fn assert_successor_kept(&self, next: &Successor, case: &str) {
        let snapshot = discord::mailbox_snapshot(&self.shared, self.channel).await;
        let held = snapshot.cancel_token.as_ref();
        assert!(
            held.is_some_and(|held| Arc::ptr_eq(held, &next.token)),
            "{case}: Y holds it"
        );
        assert_eq!(
            snapshot.active_user_message_id,
            Some(next.user_msg),
            "{case}"
        );
        assert!(
            !next.token.cancelled.load(Ordering::SeqCst),
            "{case}: Y not cancelled"
        );
        let queued = snapshot
            .intervention_queue
            .iter()
            .map(|item| item.message_id);
        let queued: Vec<_> = queued.map(MessageId::get).collect();
        assert_eq!(queued, [self.channel.get() + QUEUED], "{case}: Y's queue");
        self.assert_runtime_kept(case).await;
        let path = Self::row_path(self.channel);
        assert_eq!(file_state(&path), next.row, "{case}: Y's row as it was");
    }

    /// The provider session and turn start time a cleanup would clear are still there.
    pub(crate) async fn assert_runtime_kept(&self, case: &str) {
        let session = self
            .shared
            .core
            .lock()
            .await
            .sessions
            .get(&self.channel)
            .cloned();
        let session = session.and_then(|session| session.session_id);
        assert_eq!(
            session.as_deref(),
            Some("runtime-provider-session"),
            "{case}"
        );
        let started = self.shared.turn_start_times.contains_key(&self.channel);
        assert!(started, "{case}: the start time stays");
    }

    pub(crate) fn row_path(channel: ChannelId) -> PathBuf {
        let root = inflight::inflight_runtime_root().expect("inflight root");
        inflight::inflight_state_path(&root, &claude(), channel.get())
    }

    fn target(&self) -> TurnLifecycleTarget {
        TurnLifecycleTarget {
            provider: Some(claude()),
            channel_id: Some(self.channel),
            tmux_name: String::new(),
        }
    }

    async fn force_kill(&self) -> TurnLifecycleStopResult {
        let kill = crate::services::turn_lifecycle::force_kill_turn;
        kill(Some(&self.registry), &self.target(), "rs13", "rs13_force").await
    }
}

/// Ends the channel's turn, if any, and admits `Y` in one step, as a turn ending while a queued
/// message starts; `Y` writes its own row.
pub(crate) async fn admit_successor(shared: &SharedData, channel: ChannelId) -> Successor {
    let handle = shared.mailbox(channel);
    let next = Arc::new(CancelToken::new());
    let user_msg = MessageId::new(channel.get() + 2);
    let persistence = discord::queue_persistence_context(shared, &claude(), channel);
    let finish = handle.finish_turn(persistence);
    let start = handle.try_start_turn(next.clone(), UserId::new(8), user_msg);
    let (_, started) = tokio::join!(finish, start);
    assert!(started, "Y holds the channel");
    let row = Channel::row(channel, 2);
    Successor {
        token: next,
        user_msg,
        row: file_state(&row),
    }
}

pub(crate) struct Successor {
    pub(crate) token: Arc<CancelToken>,
    user_msg: MessageId,
    row: Option<(Vec<u8>, std::time::SystemTime)>,
}

pub(crate) fn claude() -> ProviderKind {
    ProviderKind::Claude
}

/// The row's bytes and modification time, which any save changes.
pub(crate) fn file_state(path: &PathBuf) -> Option<(Vec<u8>, std::time::SystemTime)> {
    let bytes = std::fs::read(path).ok()?;
    Some((bytes, std::fs::metadata(path).ok()?.modified().ok()?))
}

/// A current-thread runtime on paused time, so a stop's three-second wait passes at once.
pub(crate) fn run<F: Future>(future: F) -> F::Output {
    let mut runtime = tokio::runtime::Builder::new_current_thread();
    let runtime = runtime.enable_all().start_paused(true).build().unwrap();
    runtime.block_on(future)
}

// A stop whose judged turn ends and whose channel admits a successor during its wait leaves the
// successor, its queue, row, session and start time, and sets no latch or completion for it.
#[test]
fn a_stop_leaves_a_turn_admitted_after_the_judged_one_ended() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    run(async {
        let fx = Channel::new(5_340_131_000).await;
        let target = fx.target();
        let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;
        let stop = stop(Some(&fx.registry), &target, "rs13");
        let race = async {
            let next = fx.admit_successor_after_cancel().await;
            let handle = fx.shared.mailbox(fx.channel);
            handle.recovery_done().reset();
            let events = discord::turn_completion_events::subscribe_turn_completion_events;
            (next, handle, events(&fx.shared))
        };
        let (result, (next, handle, mut events)) = tokio::join!(stop, race);

        assert!(!result.host_guard_kept(), "the judged turn was stopped");
        fx.assert_successor_kept(&next, "preserve").await;
        let recovery = handle.recovery_done().wait().now_or_never();
        assert!(recovery.is_none(), "no recovery done for Y");
        let finished = fx.shared.mailboxes.turn_finished(fx.channel);
        assert!(
            finished.wait().now_or_never().is_none(),
            "Y's turn is not finished"
        );
        while let Ok(event) = events.try_recv() {
            assert_ne!(
                event.channel_id, fx.channel,
                "no completion for Y: {event:?}"
            );
        }

        // A finish the channel's actor does not answer clears nothing and keeps the turn.
        let fx = Channel::new(5_340_131_100).await;
        let row = file_state(&Channel::row_path(fx.channel));
        let target = fx.target();
        let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;
        let stop = stop(Some(&fx.registry), &target, "rs13");
        let unanswered = async {
            fx.wait_judged_cancelled().await;
            fx.shared.mailboxes.insert_unreachable_for_test(fx.channel);
        };
        let (result, ()) = tokio::join!(stop, unanswered);
        assert!(result.host_guard_kept(), "unanswered: kept");
        fx.assert_runtime_kept("unanswered").await;
        assert_eq!(
            file_state(&Channel::row_path(fx.channel)),
            row,
            "unanswered: row"
        );
        fx.shared.mailboxes.remove_fixture_for_test(fx.channel);
    });
}

// A force-kill whose judged turn is superseded during its wait kills nothing and leaves the
// successor's row; one whose judgement of an approved host could not be read keeps the turn
// even when a fresh, empty actor answers its finish.
#[test]
fn a_force_kill_keeps_a_superseded_turn_and_one_it_could_not_read() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    run(async {
        let fx = Channel::new(5_340_132_000).await;
        let (result, next) = tokio::join!(fx.force_kill(), fx.admit_successor_after_cancel());
        assert!(result.host_guard_kept(), "superseded: kept, nothing killed");
        fx.assert_successor_kept(&next, "superseded").await;

        let fx = Channel::new(5_340_132_100).await;
        let row = file_state(&Channel::row_path(fx.channel));
        let (shared, channel) = (fx.shared.clone(), fx.channel);
        at(channel, Seam::BeforeJudge, move || async move {
            shared.mailboxes.insert_unreachable_for_test(channel);
        });
        let shared = fx.shared.clone();
        at(channel, Seam::AfterJudge, move || async move {
            shared.mailboxes.remove_fixture_for_test(channel);
        });
        let result = fx.force_kill().await;
        assert!(result.host_guard_kept(), "unread: kept");
        assert!(
            !fx.judged.cancelled.load(Ordering::SeqCst),
            "unread: not cancelled"
        );
        fx.assert_runtime_kept("unread").await;
        assert_eq!(
            file_state(&Channel::row_path(channel)),
            row,
            "unread: row as it was"
        );
        assert!(HOOKS.with_borrow(Vec::is_empty), "both seams were reached");
    });
}

// The name-lookup stop finishes only the turn it judged: a successor admitted after a judged
// turn or after an empty judgement stays, a finish the actor drops keeps the turn, and with no
// actor registered the runtime's session is cleared as before.
#[test]
fn a_lookup_stop_finishes_only_the_turn_it_judged() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    run(async {
        for (n, case) in ["superseded", "empty", "unanswered", "no actor"]
            .into_iter()
            .enumerate()
        {
            let fx = Channel::new(5_340_133_000 + n as u64 * 100).await;
            let (shared, channel) = (fx.shared.clone(), fx.channel);
            if case == "empty" {
                let persistence = discord::queue_persistence_context(&shared, &claude(), channel);
                let finished = shared.mailbox(channel).finish_turn(persistence).await;
                assert!(finished.removed_token.is_some(), "{case}: judged empty");
            }
            if case == "no actor" {
                shared.mailboxes.remove_fixture_for_test(channel);
            }
            let next = Arc::new(std::sync::Mutex::new(None));
            let held = next.clone();
            at(channel, Seam::AfterJudge, move || async move {
                match case {
                    "superseded" | "empty" => {
                        let next = admit_successor(&shared, channel).await;
                        *held.lock().unwrap() = Some(next);
                    }
                    "unanswered" => shared.mailboxes.insert_reply_dropping_for_test(channel),
                    _ => {}
                }
            });
            let target = TurnLifecycleTarget {
                provider: None,
                channel_id: Some(channel),
                tmux_name: String::new(),
            };
            let stop = crate::services::turn_lifecycle::stop_turn_preserving_queue;
            let result = stop(Some(&fx.registry), &target, "rs13").await;

            let next = next.lock().unwrap().take();
            match next {
                Some(next) => fx.assert_successor_kept(&next, case).await,
                None if case == "unanswered" => {
                    assert!(result.host_guard_kept(), "{case}: kept");
                    fx.assert_runtime_kept(case).await;
                }
                None => {
                    let session = fx.shared.core.lock().await.sessions.get(&channel).cloned();
                    let session = session.and_then(|session| session.session_id);
                    assert_eq!(session, None, "{case}: the session is cleared");
                }
            }
            fx.shared.mailboxes.remove_fixture_for_test(channel);
        }
    });
}
