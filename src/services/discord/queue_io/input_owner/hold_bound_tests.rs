use super::drive::{self, Drive, Front, Handoff};
use super::effect::{Completion, Leaf, PreparedPayload};
use super::{held_watch, pump};
use crate::config::TestRuntimeRootGuard;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::inflight::{self, InflightTurnState, RelayOwnerKind};
use crate::services::discord::router::message_handler::{OwnerTurn, owner_drive_for_test};
use crate::services::discord::turn_completion_events::{self, TurnCompletionPhase};
use crate::services::discord::turn_finalizer::{
    FinalizeContext, FinalizeOutcome, TerminalEvent, TurnKey,
};
use crate::services::discord::{SharedData, make_shared_data_for_tests_with_storage};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::{ChannelMailboxHandle, PendingQueueItem};
use serde_json::{Value, json};
use serenity::model::id::{ChannelId, MessageId, UserId};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

const BOUND: Duration = Duration::from_secs(180);
const LOOK: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_secs(30);
const OLD_MESSAGE: u64 = 41;
const FRONT_MESSAGE: u64 = 42;
const INPUT: &str = "queued original for this hold";
const CLAUDE_IDLE: &str = "Done\n\n────────────────────────────────────────────────────\n❯ \n────────────────────────────────────────────────────\n  🤖 Opus(H) │ 7% │ MCP: 2\n  ⏵⏵ bypass permissions on (shift+tab to cycle)";
const CLAUDE_DRAFT: &str = "Done\n\n────────────────────────────────────────────────────\n❯ unsent draft\n────────────────────────────────────────────────────\n  🤖 Opus(H) │ 7% │ MCP: 2\n  ⏵⏵ bypass permissions on (shift+tab to cycle)";
const CLAUDE_MODAL: &str = "Do you want to proceed?\n❯ 1. Yes\nEnter to confirm\n\n────────────────────────────────────────────────────\n❯ \n────────────────────────────────────────────────────\n  🤖 Opus(H) │ 7% │ MCP: 2\n  ⏵⏵ bypass permissions on (shift+tab to cycle)";
const CODEX_IDLE: &str = "\n› \n\n  90% context left\n";
const CODEX_DRAFT: &str = "\n› unsent draft\n\n  90% context left\n";
const CODEX_MODAL: &str = "\n› \n\n  90% context left\nq close\n";
const CODEX_IMAGE: &str = "Fixture output\n\n  [Image #1]\n\n› \n\n  90% context left\n";

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Transcript {
    Ended,
    Open,
    Unknown,
}

#[derive(Clone, Copy)]
pub(super) struct WitnessSample {
    pub(super) host: &'static str,
    pub(super) capture: Option<&'static str>,
    pub(super) transcript: Transcript,
}

impl WitnessSample {
    fn at(host: &'static str, capture: &'static str) -> Self {
        Self {
            host,
            capture: Some(capture),
            transcript: Transcript::Ended,
        }
    }
}

struct Samples {
    last: WitnessSample,
    pending: VecDeque<WitnessSample>,
    reads: usize,
}

static SAMPLES: LazyLock<Mutex<HashMap<u64, Samples>>> = LazyLock::new(Default::default);

pub(super) fn observation(channel: u64) -> Option<WitnessSample> {
    let mut all = SAMPLES.lock().unwrap();
    let samples = all.get_mut(&channel)?;
    if let Some(next) = samples.pending.pop_front() {
        samples.last = next;
    }
    samples.reads += 1;
    Some(samples.last)
}

struct Capture(u64);

impl Capture {
    fn new(channel: u64, first: WitnessSample) -> Self {
        assert!(
            SAMPLES
                .lock()
                .unwrap()
                .insert(
                    channel,
                    Samples {
                        last: first,
                        pending: VecDeque::new(),
                        reads: 0,
                    }
                )
                .is_none()
        );
        let observed = observation(channel).unwrap();
        assert_eq!(observed.host, first.host);
        assert_eq!(observed.capture, first.capture);
        assert_eq!(observed.transcript, first.transcript);
        SAMPLES.lock().unwrap().get_mut(&channel).unwrap().reads = 0;
        Self(channel)
    }

    fn next(&self, samples: impl IntoIterator<Item = WitnessSample>) {
        SAMPLES.lock().unwrap().get_mut(&self.0).unwrap().pending = samples.into_iter().collect();
    }

    fn reads(&self) -> usize {
        SAMPLES.lock().unwrap()[&self.0].reads
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        SAMPLES.lock().unwrap().remove(&self.0);
    }
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    fn read(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    fn dispatch(&self) -> tracing::Dispatch {
        let logs = self.clone();
        tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || logs.clone())
                .finish(),
        )
    }
}

impl Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Rig {
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel: ChannelId,
    mailbox: ChannelMailboxHandle,
    drive: Arc<Drive>,
    sent: Arc<AtomicUsize>,
}

impl Rig {
    async fn open(
        number: u64,
        provider: ProviderKind,
        pool: Option<sqlx::PgPool>,
        admit: bool,
    ) -> Self {
        let shared = make_shared_data_for_tests_with_storage(pool);
        let channel = ChannelId::new(number);
        let mailbox = shared.mailbox(channel);
        let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
        let file = root.join(format!("hold-bound/{number}.json"));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        mailbox
            .v2_adopt(file, "hold-bound-fixture".into())
            .await
            .unwrap();
        if admit {
            let original: PendingQueueItem = serde_json::from_value(json!({
                "author_id": 7, "message_id": FRONT_MESSAGE, "channel_id": number,
                "source_message_ids": [FRONT_MESSAGE], "queued_generation": 1, "text": INPUT,
            }))
            .unwrap();
            mailbox
                .v2_admit_ticket(original, "hold-bound-input".into())
                .await
                .unwrap();
        }
        let sent = Arc::new(AtomicUsize::new(0));
        let idle = idle_handoff(mailbox.clone(), sent.clone(), number, provider.clone());
        let drive = owner_drive_for_test(
            (mailbox.clone(), None),
            (provider.clone(), number),
            idle,
            Some(shared.clone()),
        );
        Self {
            shared,
            provider,
            channel,
            mailbox,
            drive,
            sent,
        }
    }

    async fn lease(&self, message: Option<u64>) -> TurnKey {
        let token = Arc::new(CancelToken::new());
        if let Some(message) = message {
            self.mailbox
                .restore_active_turn(token.clone(), UserId::new(7), MessageId::new(message))
                .await;
        } else {
            self.mailbox
                .recovery_kickoff(token.clone(), UserId::new(7), None)
                .await;
        }
        self.shared
            .restart
            .global_active
            .store(1, Ordering::Relaxed);
        let mut row = InflightTurnState::new(
            self.provider.clone(),
            self.channel.get(),
            None,
            7,
            message.unwrap_or(0),
            0,
            "previous turn".into(),
            Some("held-provider-session".into()),
            None,
            None,
            None,
            0,
        );
        row.turn_nonce = token.turn_nonce().map(str::to_owned);
        row.last_watcher_relayed_generation_mtime_ns = Some(123_456);
        inflight::save_inflight_state(&row).unwrap();
        TurnKey::new(
            self.channel,
            message.unwrap_or(0),
            self.shared.restart.current_generation,
        )
        .with_episode_nonce(token.turn_nonce())
    }

    fn row(&self) -> Option<InflightTurnState> {
        inflight::load_inflight_state_read_only(&self.provider, self.channel.get())
    }

    fn delivered(&self) -> usize {
        self.sent.load(Ordering::SeqCst)
    }

    async fn hold(&self) {
        let number = self.channel.get();
        let before = OwnerTurn::drive_results(number).len();
        drive::wake(self.drive.clone());
        until(
            || OwnerTurn::drive_results(number).len() > before,
            "the first handoff left its front",
        )
        .await;
        ready_tasks().await;
        assert_eq!(self.delivered(), 0);
        assert_eq!(self.mailbox.snapshot().await.queue_depth(), 1);
    }

    async fn kept(&self) {
        assert!(self.mailbox.has_active_turn().await.unwrap());
        assert!(self.row().is_some());
        assert_eq!(self.mailbox.snapshot().await.queue_depth(), 1);
        assert_eq!(self.delivered(), 0);
    }

    async fn accept(
        &self,
        events: &mut tokio::sync::broadcast::Receiver<turn_completion_events::TurnCompletionEvent>,
    ) {
        for phase in [
            TurnCompletionPhase::MailboxReleased,
            TurnCompletionPhase::QueueEligible,
        ] {
            let mut received = None;
            until(
                || {
                    received = events.try_recv().ok();
                    received.is_some()
                },
                "the hold bound did not publish a release",
            )
            .await;
            let event = received.unwrap();
            assert_eq!(event.channel_id, self.channel);
            assert_eq!(event.phase, phase);
            assert_eq!(event.turn_id, Some(OLD_MESSAGE));
        }
        assert!(!self.mailbox.has_active_turn().await.unwrap());
        assert!(self.row().is_none());
        drive::wake(self.drive.clone());
        until_async(
            || async { self.mailbox.snapshot().await.queue_depth() == 0 },
            "the eligible front did not drain",
        )
        .await;
        assert_eq!(self.delivered(), 1);
    }
}

fn attempt(number: u64, provider: &ProviderKind) -> Value {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    json!({"nonce": nonce, "provider": provider.as_str(),
        "target": {"host":"herdr","owner":format!("hold-bound-{number}"),"root":"/test",
            "execution_nonce":nonce},
        "source": {"kind":"pre_source","expected_session_id":null,
            "expected_transcript":null,"binding_watermark":null}})
}

fn idle_handoff(
    mailbox: ChannelMailboxHandle,
    sent: Arc<AtomicUsize>,
    number: u64,
    provider: ProviderKind,
) -> Handoff {
    Arc::new(move |front: Front| {
        let (mailbox, sent, provider) = (mailbox.clone(), sent.clone(), provider.clone());
        Box::pin(async move {
            let claim = drive::claim_turn(&mailbox, front.1.message_id, &front)
                .await?
                .ok_or("no granted front")?;
            let leaf: Leaf = Box::new(move |frame| {
                assert_eq!(frame, INPUT);
                sent.fetch_add(1, Ordering::SeqCst);
                Ok(Completion::Submitted)
            });
            let prepared = PreparedPayload {
                frame: front.1.text.clone(),
                sources: Vec::new(),
                attempt: attempt(number, &provider),
            };
            match pump::submit_front(&mailbox, Some(claim), prepared, Arc::new(|| 10_000), leaf)
                .await?
            {
                Completion::Submitted => Ok(()),
                other => Err(format!("{other:?}")),
            }
        })
    })
}

async fn ready_tasks() {
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
}

async fn until(mut ready: impl FnMut() -> bool, message: &str) {
    let end = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < end {
        if ready() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("{message}");
}

async fn until_async<F: Future<Output = bool>>(mut ready: impl FnMut() -> F, message: &str) {
    let end = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < end {
        if ready().await {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("{message}");
}

async fn step(duration: Duration) {
    tokio::time::advance(duration + Duration::from_millis(1)).await;
    ready_tasks().await;
}

async fn three_looks() {
    step(LOOK).await;
    step(LOOK).await;
}

async fn notices(pool: &sqlx::PgPool, channel: ChannelId) -> Vec<String> {
    sqlx::query_scalar("SELECT content FROM message_outbox WHERE target = $1 AND content LIKE '%3분 넘게 보류%' ORDER BY id")
        .bind(format!("channel:{channel}"))
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn wait_notice(pool: &sqlx::PgPool, channel: ChannelId) -> Vec<String> {
    let mut result = Vec::new();
    for _ in 0..32 {
        result = notices(pool, channel).await;
        if !result.is_empty() {
            break;
        }
    }
    assert_eq!(result.len(), 1, "{result:?}");
    assert!(result[0].contains(INPUT), "{result:?}");
    result
}

#[tokio::test]
async fn hold_bound_target_incident_herdr_idle_releases_once_and_notices_pg() {
    let _root = TestRuntimeRootGuard::with_isolated_config();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let rig = Rig::open(84_301, ProviderKind::Claude, Some(pool.clone()), true).await;
    let capture = Capture::new(rig.channel.get(), WitnessSample::at("herdr", CLAUDE_IDLE));
    let logs = Logs::default();
    let _dispatch = tracing::dispatcher::set_default(&logs.dispatch());
    rig.lease(Some(OLD_MESSAGE)).await;
    let mut events = turn_completion_events::subscribe_turn_completion_events(&rig.shared);

    tokio::time::pause();
    rig.hold().await;
    step(BOUND - Duration::from_secs(1)).await;
    rig.kept().await;
    assert_eq!(capture.reads(), 0);
    step(Duration::from_secs(1)).await;
    assert_eq!(capture.reads(), 1);
    step(LOOK).await;
    rig.kept().await;
    assert_eq!(capture.reads(), 2);
    step(LOOK).await;
    rig.accept(&mut events).await;
    assert_eq!(capture.reads(), 3);
    tokio::time::resume();

    let posts = wait_notice(&pool, rig.channel).await;
    assert!(posts[0].contains("남은 턴 기록을 정리한 뒤"), "{posts:?}");
    let trace = logs.read();
    assert_eq!(
        trace.matches("input_hold_bound_released").count(),
        1,
        "{trace}"
    );
    assert!(trace.contains("operator_released"), "{trace}");
    assert!(events.try_recv().is_err());
    drop(rig);
    drop(pool);
    db.drop().await;
}

async fn claude_waiting(channel: u64, sample: WitnessSample, reason: &str) {
    let _root = TestRuntimeRootGuard::with_isolated_config();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let rig = Rig::open(channel, ProviderKind::Claude, Some(pool.clone()), true).await;
    let capture = Capture::new(channel, sample);
    let logs = Logs::default();
    let _dispatch = tracing::dispatcher::set_default(&logs.dispatch());
    rig.lease(Some(OLD_MESSAGE)).await;

    tokio::time::pause();
    rig.hold().await;
    step(BOUND).await;
    until(|| capture.reads() == 1, "initial negative witness read").await;
    tokio::time::resume();
    let first = wait_notice(&pool, rig.channel).await;
    assert!(first[0].contains("3분 넘게 보류 중입니다"), "{first:?}");
    assert!(first[0].contains(reason), "{first:?}");

    tokio::time::pause();
    step(RETRY).await;
    until(|| capture.reads() >= 2, "negative witness was retried").await;
    step(RETRY).await;
    until(|| capture.reads() >= 3, "second negative retry").await;
    rig.kept().await;
    tokio::time::resume();
    assert_eq!(notices(&pool, rig.channel).await, first);
    let trace = logs.read();
    assert_eq!(
        trace.matches("input_hold_bound_waiting").count(),
        1,
        "{trace}"
    );
    assert_eq!(
        trace.matches("input_hold_bound_declined").count(),
        1,
        "{trace}"
    );
    assert!(!trace.contains("input_hold_bound_released"), "{trace}");
    drop(rig);
    drop(pool);
    db.drop().await;
}

#[tokio::test]
async fn hold_bound_target_draft_does_not_release_and_notices_once_pg() {
    claude_waiting(
        84_302,
        WitnessSample::at("herdr", CLAUDE_DRAFT),
        "화면 초안",
    )
    .await;
}

#[tokio::test]
async fn hold_bound_target_modal_does_not_release_and_notices_once_pg() {
    claude_waiting(84_303, WitnessSample::at("herdr", CLAUDE_MODAL), "모달").await;
}

#[tokio::test]
async fn hold_bound_target_running_idle_screen_open_transcript_notices_once_pg() {
    claude_waiting(
        84_304,
        WitnessSample {
            transcript: Transcript::Open,
            ..WitnessSample::at("herdr", CLAUDE_IDLE)
        },
        "작업 중",
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn hold_bound_target_image_draft_codex_hosts_preserve_each_screen_blocker() {
    use crate::services::codex_tui::busy_inject::screen::{self, Composer};

    let _root = TestRuntimeRootGuard::with_isolated_config();
    let image = screen::read(CODEX_IMAGE);
    assert_eq!(image.composer, Composer::Empty);
    assert!(image.attachments, "image must be visible apart from text");
    assert!(screen::delivery_veto(CODEX_IMAGE).is_none());
    let mut number = 84_310;
    for host in ["herdr", "tmux"] {
        for (kind, text) in [
            ("draft", CODEX_DRAFT),
            ("modal", CODEX_MODAL),
            ("image", CODEX_IMAGE),
        ] {
            number += 1;
            let rig = Rig::open(number, ProviderKind::Codex, None, true).await;
            let capture = Capture::new(number, WitnessSample::at(host, text));
            let original = rig.lease(Some(OLD_MESSAGE)).await;
            let mut events = turn_completion_events::subscribe_turn_completion_events(&rig.shared);
            rig.hold().await;
            step(BOUND).await;
            step(RETRY).await;
            step(RETRY).await;
            assert_eq!(capture.reads(), 3, "{host}/{kind}");
            rig.kept().await;
            assert!(
                original.matches_episode_nonce(rig.row().unwrap().turn_nonce.as_deref()),
                "{host}/{kind}"
            );
            assert!(events.try_recv().is_err(), "{host}/{kind}");

            capture.next([WitnessSample::at(host, CODEX_IDLE)]);
            step(RETRY).await;
            three_looks().await;
            rig.accept(&mut events).await;
            assert_eq!(capture.reads(), 6, "{host}/{kind}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn hold_bound_mut_no_dedupe_queue_eligible_and_held_watch_deliver_once() {
    use crate::services::claude_tui::busy_inject::Screen;

    let _root = TestRuntimeRootGuard::with_isolated_config();
    let rig = Rig::open(84_320, ProviderKind::Claude, None, true).await;
    let _capture = Capture::new(rig.channel.get(), WitnessSample::at("herdr", CLAUDE_IDLE));
    rig.lease(Some(OLD_MESSAGE)).await;
    let mut events = turn_completion_events::subscribe_turn_completion_events(&rig.shared);
    rig.hold().await;
    step(BOUND).await;
    step(LOOK).await;

    // Start both wake sources at the last read. The held probe's readiness is released
    // only after the actor actually publishes QueueEligible.
    let (release, wait) = std::sync::mpsc::channel();
    let (entered, entering) = std::sync::mpsc::channel();
    held_watch::arm(
        rig.channel.get(),
        Box::new(move || {
            entered.send(()).unwrap();
            crate::test_support::wait::recv_within(&wait, Duration::from_secs(5));
            Screen::Ready
        }),
    );
    let (_, observed) = held_watch::watch_observed(rig.drive.clone(), FRONT_MESSAGE);
    step(LOOK).await;
    until(
        || entering.try_recv().is_ok(),
        "held watch did not read at the deadline",
    )
    .await;
    let mut completed = Vec::new();
    until(
        || {
            while let Ok(event) = events.try_recv() {
                completed.push(event);
            }
            completed.len() == 2
        },
        "CAS did not publish QueueEligible",
    )
    .await;
    assert_eq!(completed[1].phase, TurnCompletionPhase::QueueEligible);
    release.send(()).unwrap();
    let observed = observed.await.unwrap();
    assert_eq!(observed.verdict, held_watch::Verdict::Wake);
    drive::wake(rig.drive.clone());
    until_async(
        || async { rig.mailbox.snapshot().await.queue_depth() == 0 },
        "the front did not drain",
    )
    .await;
    ready_tasks().await;
    assert_eq!(rig.delivered(), 1);
    assert!(events.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn hold_bound_mut_no_dedupe_attempted_head_never_releases_the_lease() {
    let _root = TestRuntimeRootGuard::with_isolated_config();
    let rig = Rig::open(84_321, ProviderKind::Claude, None, true).await;
    let capture = Capture::new(rig.channel.get(), WitnessSample::at("herdr", CLAUDE_IDLE));
    rig.lease(Some(OLD_MESSAGE)).await;
    rig.hold().await;

    let prepared = attempt(rig.channel.get(), &rig.provider);
    rig.mailbox
        .v2_request(move |slot| {
            let owner = slot.as_mut().unwrap();
            let attempt = serde_json::from_value(prepared).map_err(|e| e.to_string())?;
            owner.reserve_unit(attempt, |item| Some((String::new(), item.text.clone())))?;
            Ok(())
        })
        .await
        .unwrap();

    step(BOUND).await;
    three_looks().await;
    rig.kept().await;
    assert_eq!(capture.reads(), 0);
    let queue = rig
        .mailbox
        .v2_request(|slot| slot.as_ref().unwrap().json())
        .await
        .unwrap();
    assert!(!queue["head_injection"].is_null(), "{queue}");
}

#[test]
fn hold_bound_target_restart_reopens_the_clock_from_zero() {
    let _root = TestRuntimeRootGuard::with_isolated_config();
    let channel = 84_322;
    let capture = Capture::new(channel, WitnessSample::at("herdr", CLAUDE_IDLE));
    let runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
    };

    let first = runtime();
    first.block_on(async {
        let rig = Rig::open(channel, ProviderKind::Claude, None, true).await;
        rig.lease(Some(OLD_MESSAGE)).await;
        rig.hold().await;
        step(BOUND - Duration::from_secs(1)).await;
        rig.kept().await;
        assert_eq!(capture.reads(), 0);
    });
    drop(first);

    runtime().block_on(async {
        let rig = Rig::open(channel, ProviderKind::Claude, None, false).await;
        rig.lease(Some(OLD_MESSAGE)).await;
        let mut events = turn_completion_events::subscribe_turn_completion_events(&rig.shared);
        rig.hold().await;
        step(BOUND - Duration::from_secs(1)).await;
        rig.kept().await;
        assert_eq!(capture.reads(), 0);
        step(Duration::from_secs(1)).await;
        three_looks().await;
        rig.accept(&mut events).await;
        assert_eq!(capture.reads(), 3);
    });
}

#[tokio::test(start_paused = true)]
async fn hold_bound_target_flicker_each_unknown_requires_three_new_idle_reads() {
    let _root = TestRuntimeRootGuard::with_isolated_config();
    let idle = WitnessSample::at("tmux", CODEX_IDLE);
    for (number, unknown) in [
        (
            84_330,
            WitnessSample {
                capture: None,
                ..idle
            },
        ),
        (
            84_331,
            WitnessSample {
                transcript: Transcript::Unknown,
                ..idle
            },
        ),
        (84_332, WitnessSample::at("tmux", "composer missing")),
    ] {
        let rig = Rig::open(number, ProviderKind::Codex, None, true).await;
        let capture = Capture::new(number, idle);
        capture.next([idle, unknown, idle, idle, idle]);
        rig.lease(Some(OLD_MESSAGE)).await;
        let mut events = turn_completion_events::subscribe_turn_completion_events(&rig.shared);
        rig.hold().await;

        step(BOUND).await;
        step(LOOK).await;
        rig.kept().await;
        assert_eq!(capture.reads(), 2);
        step(RETRY).await;
        step(LOOK).await;
        rig.kept().await;
        assert_eq!(capture.reads(), 4);
        assert!(events.try_recv().is_err());
        step(LOOK).await;
        rig.accept(&mut events).await;
        assert_eq!(capture.reads(), 5);
    }
}

#[tokio::test(start_paused = true)]
async fn hold_bound_target_late_terminal_cannot_relay_or_clear_the_successor() {
    let _root = TestRuntimeRootGuard::with_isolated_config();
    let rig = Rig::open(84_340, ProviderKind::Claude, None, true).await;
    let _capture = Capture::new(rig.channel.get(), WitnessSample::at("herdr", CLAUDE_IDLE));
    let old = rig.lease(Some(OLD_MESSAGE)).await;
    rig.shared.turn_finalizer.register_start(
        old,
        rig.provider.clone(),
        RelayOwnerKind::Watcher,
        &rig.shared,
    );
    let mut events = turn_completion_events::subscribe_turn_completion_events(&rig.shared);
    rig.hold().await;
    step(BOUND).await;
    three_looks().await;
    rig.accept(&mut events).await;

    // A fresh execution can retain the message id while changing its nonce.
    let successor = rig.lease(Some(OLD_MESSAGE)).await;
    rig.shared.turn_finalizer.register_start(
        successor,
        rig.provider.clone(),
        RelayOwnerKind::Watcher,
        &rig.shared,
    );
    let new = rig.mailbox.snapshot().await.cancel_token.unwrap();
    let old_other_key = TurnKey {
        generation: old.generation + 1,
        ..old
    };
    for key in [old, old_other_key] {
        let outcome = rig
            .shared
            .turn_finalizer
            .submit_terminal(
                key,
                rig.provider.clone(),
                TerminalEvent::Complete,
                FinalizeContext::bridge(),
                rig.shared.clone(),
            )
            .await;
        assert!(matches!(outcome, FinalizeOutcome::AlreadyFinalized));
        let live = rig.mailbox.snapshot().await.cancel_token.unwrap();
        assert!(Arc::ptr_eq(&live, &new));
        let row = rig.row().unwrap();
        assert!(successor.matches_episode_nonce(row.turn_nonce.as_deref()));
        assert_eq!(row.last_watcher_relayed_generation_mtime_ns, Some(123_456));
        assert!(!row.terminal_delivery_committed);
        assert!(events.try_recv().is_err());
        assert_eq!(rig.delivered(), 1);
    }
}

#[tokio::test]
async fn hold_bound_target_incident_unbound_lease_declines_with_one_notice_pg() {
    let _root = TestRuntimeRootGuard::with_isolated_config();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let rig = Rig::open(84_341, ProviderKind::Claude, Some(pool.clone()), true).await;
    let capture = Capture::new(rig.channel.get(), WitnessSample::at("herdr", CLAUDE_IDLE));
    let logs = Logs::default();
    let _dispatch = tracing::dispatcher::set_default(&logs.dispatch());
    rig.lease(None).await;
    let mut events = turn_completion_events::subscribe_turn_completion_events(&rig.shared);

    tokio::time::pause();
    rig.hold().await;
    step(BOUND).await;
    three_looks().await;
    rig.kept().await;
    assert_eq!(capture.reads(), 0);
    assert!(events.try_recv().is_err());
    tokio::time::resume();

    let posts = wait_notice(&pool, rig.channel).await;
    assert!(posts[0].contains("3분 넘게 보류 중입니다"), "{posts:?}");
    let trace = logs.read();
    assert_eq!(
        trace.matches("input_hold_bound_declined").count(),
        1,
        "{trace}"
    );
    assert!(trace.contains("unbound_message_identity"), "{trace}");
    assert_eq!(
        trace.matches("input_hold_bound_waiting").count(),
        1,
        "{trace}"
    );
    drop(rig);
    drop(pool);
    db.drop().await;
}
