//! The public watchdog pass over a turn-mode channel whose turn crossed a restart: real native
//! reader, O answering through the real demand/watch read.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use poise::serenity_prelude::{ChannelId, MessageId};

use super::super::HealthRegistry;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::InflightRestartMode;
use crate::services::discord::inflight::{self, InflightTurnState};
use crate::services::discord::turn_presence::activity::ClosedBoundary;
use crate::services::discord::{self, SharedData};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::tui_o::writer::actor::{Owing, Undelivered};
use crate::services::tui_o::writer::host::OwedView;

type Hooks = HashMap<(&'static str, u64), Box<dyn FnOnce()>>;

thread_local! {
    static NATIVE: RefCell<HashMap<u64, Option<ClosedBoundary>>> = RefCell::default();
    static HOOKS: RefCell<Hooks> = RefCell::default();
    static EVENTS: RefCell<Vec<(u64, &'static str)>> = const { RefCell::new(Vec::new()) };
}

/// A scripted native end, for a channel the activity reader cannot bind (no watcher).
pub(super) fn native_end(channel: u64) -> Option<Option<ClosedBoundary>> {
    NATIVE.with(|native| native.borrow().get(&channel).cloned())
}

pub(super) async fn pause(point: &'static str, channel: u64) {
    if let Some(hook) = HOOKS.with(|hooks| hooks.borrow_mut().remove(&(point, channel))) {
        hook();
    }
}

pub(super) fn note_event(channel: u64, event: &'static str) {
    EVENTS.with(|events| events.borrow_mut().push((channel, event)));
}

fn events(channel: u64) -> Vec<&'static str> {
    EVENTS.with(|events| {
        let events = events.borrow();
        events
            .iter()
            .filter(|(c, _)| *c == channel)
            .map(|(_, e)| *e)
            .collect()
    })
}

fn on(point: &'static str, channel: u64, hook: impl FnOnce() + 'static) {
    HOOKS.with(|hooks| hooks.borrow_mut().insert((point, channel), Box::new(hook)));
}

const PROVIDER: ProviderKind = ProviderKind::Claude;

fn user(id: &str) -> String {
    format!(r#"{{"type":"user","uuid":"{id}","message":{{"content":"carried input"}}}}"#)
}
const END: &str = r#"{"type":"system","subtype":"turn_duration"}"#;
const INTERRUPT: &str = r#"{"type":"user","message":{"content":"[Request interrupted by user]"}}"#;
const ENQUEUE: &str = r#"{"type":"queue-operation","operation":"enqueue","content":"next"}"#;
const QUEUED_COMMAND: &str =
    r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"next"}}"#;
const ASSISTANT: &str = r#"{"type":"assistant","uuid":"u-answer","apiBlockIndex":0,"message":{"id":"answer","content":[{"type":"text","text":"answer"}]}}"#;

fn append(path: &std::path::Path, rows: &[&str]) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for row in rows {
        writeln!(file, "{row}").unwrap();
    }
}

/// What the scripted O answers; empty `captured` reports the transcript's current length.
#[derive(Clone, Default)]
struct OAnswer {
    debt: Undelivered,
    behind: u64,
}

/// One channel's carried turn: an earlier generation's DrainRestart row restored as the mailbox
/// episode, a confirmed turn-mode transcript, and O's owed read.
struct Fixture {
    shared: Arc<SharedData>,
    registry: HealthRegistry,
    channel: ChannelId,
    path: std::path::PathBuf,
    user_msg_id: u64,
    o: Arc<Mutex<OAnswer>>,
    _keep: Vec<Box<dyn std::any::Any>>,
}

struct Options {
    born_generation: u64,
    watcher: bool,
    turn_mode: bool,
    episode: bool,
    transcript: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            born_generation: 5,
            watcher: true,
            turn_mode: true,
            episode: true,
            transcript: vec![user("carried"), END.to_string()],
        }
    }
}

impl Fixture {
    async fn new(channel: u64, user_msg_id: u64, options: Options) -> Self {
        let mut keep: Vec<Box<dyn std::any::Any>> = Vec::new();
        let runtime = crate::config::TestRuntimeRootGuard::new();
        let root = tempfile::tempdir().unwrap();
        let binding = discord::health::BindingRoot::enter(root.path());
        let shared = discord::make_shared_data_for_tests();
        let path = root.path().join(format!("{channel}.jsonl"));
        let rows: Vec<&str> = options.transcript.iter().map(String::as_str).collect();
        std::fs::write(&path, "").unwrap();
        append(&path, &rows);
        let session = format!("AgentDesk-claude-7533-{channel}");
        if options.watcher {
            keep.push(Box::new(discord::health::ReadyPane::mark(&session)));
            let confirmed = discord::health::bind_turn_mode_transcript(
                &shared, &PROVIDER, channel, &session, &path,
            );
            if options.turn_mode {
                keep.push(Box::new(confirmed));
            }
        } else if options.turn_mode {
            keep.push(Box::new(
                crate::services::tui_o::turn_mode::TestConfirmation::new(channel),
            ));
        }
        keep.push(Box::new(
            crate::services::tui_o::cutover::test_override::force_channels(&[(
                channel,
                RuntimeHandoffKind::ClaudeTui,
            )]),
        ));

        let mut row = InflightTurnState::new(
            PROVIDER,
            channel,
            None,
            7,
            user_msg_id,
            user_msg_id + 1,
            "carried".to_string(),
            Some("session-7533".to_string()),
            Some(session.clone()),
            Some(path.display().to_string()),
            None,
            0,
        );
        row.born_generation = options.born_generation;
        row.turn_nonce = Some(format!("nonce-{channel}"));
        inflight::save_inflight_state(&row).unwrap();
        let mut row = inflight::load_inflight_state(&PROVIDER, channel).unwrap();
        if options.episode {
            discord::recovery_engine::reregister_active_turn_from_inflight(&shared, &row).await;
            row = inflight::load_inflight_state(&PROVIDER, channel).unwrap();
        }
        // The managed restart's mark, as the shutdown left it on the row the next process reads.
        row.restart_mode = Some(InflightRestartMode::DrainRestart);
        // The replacement window the restart handoff keeps: the mark names the generation the
        // test process reads as current, so no sweep of this pass may remove it as foreign.
        row.restart_generation = Some(shared.restart.current_generation);
        let file = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &PROVIDER,
            channel,
        );
        std::fs::write(&file, serde_json::to_vec(&row).unwrap()).unwrap();

        if options.watcher {
            // The reader's first sight of a binding rebuilds off-thread; production's next tick
            // reads what this waits for.
            discord::health::settled_reason(&shared, &PROVIDER, channel).await;
        }
        let registry = HealthRegistry::new();
        registry
            .register(PROVIDER.as_str().to_string(), shared.clone())
            .await;
        let o = Arc::new(Mutex::new(OAnswer::default()));
        host_o(channel, &path, o.clone());
        keep.push(Box::new(binding));
        keep.push(Box::new(root));
        keep.push(Box::new(runtime));
        Self {
            shared,
            registry,
            channel: ChannelId::new(channel),
            path,
            user_msg_id,
            o,
            _keep: keep,
        }
    }

    async fn pass(&self) {
        super::super::run_stall_watchdog_pass(&self.registry, &PROVIDER).await;
    }

    fn row(&self) -> Option<InflightTurnState> {
        inflight::load_inflight_state(&PROVIDER, self.channel.get())
    }

    async fn episode_open(&self) -> bool {
        let snapshot = discord::mailbox_snapshot(&self.shared, self.channel).await;
        snapshot.cancel_token.is_some()
            && snapshot.active_user_message_id == Some(MessageId::new(self.user_msg_id))
    }

    fn archived(&self) -> bool {
        let dir = inflight::inflight_runtime_root().unwrap().join("archive");
        std::fs::read_dir(dir).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.starts_with(&format!(
                    "{}.json.turn-mode-carried-turn",
                    self.channel.get()
                ))
            })
        })
    }

    async fn assert_kept(&self, case: &str) {
        assert!(self.row().is_some(), "{case}: the carried row stays");
        assert!(self.episode_open().await, "{case}: the episode stays open");
        assert!(
            events(self.channel.get()).is_empty(),
            "{case}: nothing was submitted"
        );
        assert!(!self.archived(), "{case}: nothing was archived");
    }

    async fn assert_closed(&self, case: &str) {
        assert!(self.row().is_none(), "{case}: the carried row is gone");
        assert!(self.archived(), "{case}: the row was archived, not dropped");
        assert!(
            !self.episode_open().await,
            "{case}: the episode is released"
        );
    }
}

/// Answers O's owed read through the real demand/watch path for `channel`.
fn host_o(channel: u64, path: &std::path::Path, answer: Arc<Mutex<OAnswer>>) {
    let owing = Owing::default();
    let view = OwedView {
        published: owing.published.subscribe(),
        demand: owing.demand.clone(),
    };
    crate::services::tui_o::writer::host::process_readiness().track_owed_for_test(channel, view);
    let path = path.to_path_buf();
    tokio::spawn(async move {
        // Ends with the test's runtime.
        loop {
            if owing.demand.wanted() {
                let script = answer.lock().unwrap().clone();
                let len = std::fs::metadata(&path).map_or(0, |m| m.len());
                let mut owed = script.debt;
                owed.asked = owing.demand.asked();
                if owed.captured.is_empty() {
                    owed.captured = vec![(path.clone(), len.saturating_sub(script.behind))];
                }
                owing.published.send_replace(Some(owed));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
}

/// The fixture's restored row still carries the restart mark the archive must get past.
#[tokio::test(flavor = "current_thread")]
async fn carried_close_fixture_preserves_drain_restart_at_entry() {
    let fx = Fixture::new(7_533_301, 9_100_095_491_915_387_881, Options::default()).await;
    let row = fx.row().unwrap();
    assert_eq!(row.restart_mode, Some(InflightRestartMode::DrainRestart));
    assert!(fx.episode_open().await, "the restore re-minted the episode");
}

/// The managed-restart shape: a DrainRestart row of the earlier generation, its re-minted
/// episode, the native end read and O drained through it — archived, released, completed.
#[tokio::test(flavor = "current_thread")]
async fn carried_drain_restart_row_is_archived_and_episode_released() {
    // A headless (synthetic id) and a Discord (snowflake) turn close by the same rule.
    for (channel, user_msg_id) in [
        (7_533_311, 9_100_095_491_915_387_881),
        (7_533_312, 1_479_671_298_497_183_900),
    ] {
        let fx = Fixture::new(channel, user_msg_id, Options::default()).await;
        assert_eq!(
            fx.row().unwrap().restart_mode,
            Some(InflightRestartMode::DrainRestart)
        );
        fx.pass().await;
        fx.assert_closed("crossed turn").await;
        assert_eq!(events(channel), ["complete"]);
        fx.pass().await;
        assert_eq!(
            events(channel),
            ["complete"],
            "a closed turn is not closed again"
        );
    }
}

/// An input received for the holder after its last close, whose run has not
/// opened, keeps the carried episode — both receipt shapes Claude writes.
#[tokio::test(flavor = "current_thread")]
async fn observed_enqueue_before_native_open_keeps_carried_episode() {
    for (channel, receipt) in [(7_533_321, ENQUEUE), (7_533_322, QUEUED_COMMAND)] {
        let fx = Fixture::new(channel, 9_100_095_491_915_388_001, Options::default()).await;
        append(&fx.path, &[receipt]);
        fx.pass().await;
        fx.assert_kept(receipt).await;

        // The run opens: the episode stays the stall verdict's open turn, aged past the page
        // threshold, exactly as the verdict reads it (`TurnFacts::new`).
        append(&fx.path, &[&user("next"), ASSISTANT]);
        fx.pass().await;
        fx.assert_kept("its run is open").await;
        let mailbox = fx.shared.mailbox_peek(fx.channel).unwrap();
        mailbox
            .age_active_turn_for_test(Duration::from_secs(700))
            .await;
        fx.pass().await;
        fx.assert_kept("its run is still open").await;
        let snapshot = discord::mailbox_snapshot(&fx.shared, fx.channel).await;
        let turn_open =
            snapshot.cancel_token.is_some() || snapshot.active_user_message_id.is_some();
        let age = super::super::super::transcript_binding_stall::turn_age_secs(&snapshot, None);
        assert!(turn_open, "the verdict still sees the turn open");
        assert!(
            age.is_some_and(|age| age >= 600),
            "and past the 600 s page threshold: {age:?}"
        );

        // Its own end and O's delivery close it.
        append(&fx.path, &[END]);
        fx.pass().await;
        fx.assert_closed("its run ended and O delivered it").await;
    }
}

/// An injection settled after the transcript last grew may have reached the pane unrecorded.
#[tokio::test(flavor = "current_thread")]
async fn injection_delivered_after_last_growth_keeps_carried_episode() {
    let fx = Fixture::new(7_533_332, 9_100_095_491_915_388_012, Options::default()).await;
    // The reader has read the close; the injection settles after it.
    let _ = discord::turn_presence::activity::reading_now(&fx.shared, &PROVIDER, fx.channel).await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    discord::inject_disposition::note_channel_delivery(
        &PROVIDER,
        fx.channel,
        std::time::Instant::now(),
    );
    fx.pass().await;
    fx.assert_kept("injected after the last growth").await;
    append(&fx.path, &[&user("injected"), ASSISTANT, END]);
    fx.pass().await;
    fx.assert_closed("the injected run ended").await;
}

/// Injection holds the session transition lock through its paste and settlement.
#[tokio::test(flavor = "current_thread")]
async fn injection_holding_lock_blocks_close() {
    let fx = Fixture::new(7_533_341, 9_100_095_491_915_388_021, Options::default()).await;
    let held = fx
        .shared
        .session_transition_lock(fx.channel)
        .try_lock_owned()
        .unwrap();
    fx.pass().await;
    fx.assert_kept("lock held").await;
    drop(held);
    fx.pass().await;
    fx.assert_closed("lock free").await;
}

/// The transcript grows between O's read and the locked re-read: nothing closes this tick.
#[tokio::test(flavor = "current_thread")]
async fn native_growth_after_owed_read_blocks_close() {
    let fx = Fixture::new(7_533_342, 9_100_095_491_915_388_022, Options::default()).await;
    let path = fx.path.clone();
    on("locked", fx.channel.get(), move || {
        append(&path, &[&user("late")])
    });
    fx.pass().await;
    fx.assert_kept("grew under the lock").await;
}

/// A row rewritten just before the archive keeps both the row and the episode.
#[tokio::test(flavor = "current_thread")]
async fn archive_pin_drift_blocks_finalize() {
    let fx = Fixture::new(7_533_351, 9_100_095_491_915_388_031, Options::default()).await;
    let channel = fx.channel.get();
    on("archive", channel, move || {
        let file = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &PROVIDER,
            channel,
        );
        let mut row: InflightTurnState =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        row.save_generation += 1;
        std::fs::write(&file, serde_json::to_vec(&row).unwrap()).unwrap();
    });
    fx.pass().await;
    fx.assert_kept("pin drifted").await;
}

/// O still owing anything, or behind the native end, keeps the turn.
#[tokio::test(flavor = "current_thread")]
async fn o_debt_or_capture_behind_end_keeps_carried_turn() {
    let cases: [(u64, OAnswer); 4] = [
        (
            7_533_361,
            OAnswer {
                behind: 1,
                ..OAnswer::default()
            },
        ),
        (
            7_533_362,
            OAnswer {
                debt: Undelivered {
                    posting: 1,
                    ..Undelivered::default()
                },
                behind: 0,
            },
        ),
        (
            7_533_363,
            OAnswer {
                debt: Undelivered {
                    binding_pending: 1,
                    ..Undelivered::default()
                },
                behind: 0,
            },
        ),
        (
            7_533_364,
            OAnswer {
                debt: Undelivered {
                    verified_work: 1,
                    ..Undelivered::default()
                },
                behind: 0,
            },
        ),
    ];
    for (channel, answer) in cases {
        let fx = Fixture::new(channel, 9_100_095_491_915_388_041, Options::default()).await;
        *fx.o.lock().unwrap() = answer;
        fx.pass().await;
        fx.assert_kept("O not drained").await;
        *fx.o.lock().unwrap() = OAnswer::default();
        fx.pass().await;
        fx.assert_closed("O drained").await;
    }
}

/// An interrupted native end settles the episode as cancelled, never completed.
#[tokio::test(flavor = "current_thread")]
async fn interrupted_close_cancels() {
    let options = Options {
        transcript: vec![user("carried"), INTERRUPT.to_string()],
        ..Options::default()
    };
    let fx = Fixture::new(7_533_371, 9_100_095_491_915_388_051, options).await;
    fx.pass().await;
    fx.assert_closed("interrupted").await;
    assert_eq!(events(fx.channel.get()), ["cancel"]);
}

/// A pane with no turn evidence reads Idle, but not `closed`: nothing ended, nothing closes.
#[tokio::test(flavor = "current_thread")]
async fn no_turn_evidence_ready_is_not_closed() {
    let options = Options {
        transcript: Vec::new(),
        ..Options::default()
    };
    let fx = Fixture::new(7_533_372, 9_100_095_491_915_388_052, options).await;
    fx.pass().await;
    fx.assert_kept("no turn evidence").await;
}

/// This process's own open episode is the live bridge's to finish, and stays the verdict's.
#[tokio::test(flavor = "current_thread")]
async fn same_generation_open_episode_is_untouched() {
    let options = Options {
        born_generation: 0,
        ..Options::default()
    };
    let fx = Fixture::new(7_533_381, 9_100_095_491_915_388_061, options).await;
    fx.pass().await;
    fx.assert_kept("same generation").await;
}

/// A channel not confirmed for turn mode is left as main leaves it.
#[tokio::test(flavor = "current_thread")]
async fn not_turn_mode_is_unchanged() {
    let options = Options {
        turn_mode: false,
        ..Options::default()
    };
    let fx = Fixture::new(7_533_382, 9_100_095_491_915_388_062, options).await;
    fx.pass().await;
    fx.assert_kept("not turn mode").await;
}

/// A successor under the same message id but another nonce is not the carried episode.
#[tokio::test(flavor = "current_thread")]
async fn other_nonce_with_same_message_id_is_untouched() {
    let options = Options {
        episode: false,
        ..Options::default()
    };
    let fx = Fixture::new(7_533_383, 9_100_095_491_915_388_063, options).await;
    let token = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        "successor".into(),
    )));
    assert!(
        discord::mailbox_try_start_turn(
            &fx.shared,
            fx.channel,
            token,
            poise::serenity_prelude::UserId::new(7),
            MessageId::new(fx.user_msg_id),
        )
        .await
    );
    fx.pass().await;
    assert!(fx.row().is_some(), "the row stays");
    assert!(fx.episode_open().await, "the successor stays");
    assert!(events(fx.channel.get()).is_empty());
}

/// A row whose episode is already released is archived without any finalize.
#[tokio::test(flavor = "current_thread")]
async fn episode_released_row_left_is_archived() {
    let options = Options {
        episode: false,
        ..Options::default()
    };
    let fx = Fixture::new(7_533_391, 9_100_095_491_915_388_071, options).await;
    fx.pass().await;
    assert!(fx.row().is_none());
    assert!(fx.archived());
    assert!(
        events(fx.channel.get()).is_empty(),
        "no episode, no terminal"
    );
}

/// A confirmed channel with no watcher is still a candidate.
#[tokio::test(flavor = "current_thread")]
async fn watcherless_confirmed_channel_is_closed() {
    let options = Options {
        watcher: false,
        ..Options::default()
    };
    let fx = Fixture::new(7_533_392, 9_100_095_491_915_388_072, options).await;
    let len = std::fs::metadata(&fx.path).unwrap().len();
    NATIVE.with(|native| {
        native.borrow_mut().insert(
            fx.channel.get(),
            Some(ClosedBoundary {
                source: fx.path.clone(),
                through: len,
                grew_at: std::time::Instant::now() - Duration::from_secs(60),
                aborted: false,
            }),
        )
    });
    fx.pass().await;
    fx.assert_closed("watcherless").await;
}

/// An archived row whose episode the finalizer did not release is retried from memory, once.
#[tokio::test(flavor = "current_thread")]
async fn archived_target_retry_is_idempotent() {
    let fx = Fixture::new(7_533_393, 9_100_095_491_915_388_073, Options::default()).await;
    let channel = fx.channel.get();
    let snapshot = discord::turn_finalizer::SyntheticClaimSnapshot::from_row(&fx.row().unwrap());
    let row = fx.row().unwrap();
    let file = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &PROVIDER,
        channel,
    );
    let archive = inflight::inflight_runtime_root().unwrap().join("archive");
    std::fs::create_dir_all(&archive).unwrap();
    std::fs::rename(
        &file,
        archive.join(format!("{channel}.json.turn-mode-carried-turn-retry")),
    )
    .unwrap();
    super::archived().insert(
        channel,
        super::Target {
            finalizer_turn_id: row.effective_finalizer_turn_id(),
            nonce: row.turn_nonce.clone(),
            snapshot,
            born_generation: row.born_generation,
        },
    );
    fx.pass().await;
    fx.assert_closed("retried").await;
    assert_eq!(events(channel), ["complete"]);
    assert!(!super::archived().contains_key(&channel));
    fx.pass().await;
    assert_eq!(events(channel), ["complete"], "retry is not repeated");
}

/// Once closed, the restored episode no longer holds the channel against queued input.
#[tokio::test(flavor = "current_thread")]
async fn queue_is_unblocked_after_close() {
    let fx = Fixture::new(7_533_394, 9_100_095_491_915_388_074, Options::default()).await;
    assert!(discord::mailbox_has_blocking_active_turn(&fx.shared, fx.channel).await);
    fx.pass().await;
    fx.assert_closed("closed").await;
    assert!(
        !discord::mailbox_has_blocking_active_turn(&fx.shared, fx.channel).await,
        "a queued input's dequeue is no longer held by the carried episode"
    );
}
