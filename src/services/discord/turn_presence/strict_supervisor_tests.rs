//! The strict path on the real supervisor, live effects and presence; only Discord HTTP is scripted.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use poise::serenity_prelude::{MessageId, UserId};

use super::*;
use crate::services::cluster::channel_home;
use crate::services::discord::turn_presence::activity::tests::bind_turn_mode_transcript;
use crate::services::discord::turn_presence::admission::tests::{Fixture, held_home};
use crate::services::discord::turn_presence::entrypoints;
use crate::services::discord::turn_presence::strict_runtime::{self, Client, Owner, Transport};
use crate::services::session_host::test_support::InjectedLivenessGuard;
use crate::services::session_host::{HostLiveness, HostSessionRef};
use crate::services::tui_o::turn_mode::TestConfirmation;

/// Supervisors here walk the same registered channels and write the same display map.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static NEXT: AtomicU64 = AtomicU64::new(6_325_300_000);

/// The strict client's Discord HTTP: counts first polls, can hold a send and sees it dropped.
struct Wire {
    bot: u64,
    polls: AtomicUsize,
    returned: AtomicUsize,
    cancelled: AtomicUsize,
    hold: AtomicBool,
    release: tokio::sync::Notify,
    /// Runs as a send returns, before its result reaches the supervisor.
    then: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Wire {
    fn new(bot: u64, hold: bool) -> Arc<Self> {
        Arc::new(Self {
            bot,
            polls: AtomicUsize::new(0),
            returned: AtomicUsize::new(0),
            cancelled: AtomicUsize::new(0),
            hold: AtomicBool::new(hold),
            release: tokio::sync::Notify::new(),
            then: Mutex::new(None),
        })
    }

    fn polls(&self) -> usize {
        self.polls.load(Ordering::SeqCst)
    }

    fn cancelled(&self) -> usize {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// Counts a send dropped before it returned.
struct Unfinished<'a>(Option<&'a AtomicUsize>);

impl Drop for Unfinished<'_> {
    fn drop(&mut self) {
        if let Some(cancelled) = self.0 {
            cancelled.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[async_trait]
impl Transport for Wire {
    async fn bot_id(&self) -> Result<u64, String> {
        Ok(self.bot)
    }

    async fn typing(&self, _: u64) -> Result<(), String> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        let mut unfinished = Unfinished(Some(&self.cancelled));
        if self.hold.load(Ordering::SeqCst) {
            self.release.notified().await;
        }
        unfinished.0 = None;
        let then = lock(&self.then).take();
        if let Some(then) = then {
            then();
        }
        self.returned.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A strict supervisor over the live effects of `shared`, open while the owner lives.
async fn strict(shared: &Arc<SharedData>, wire: &Arc<Wire>) -> (Supervisor<LiveEffects>, Owner) {
    let client = Client::verify(wire.clone()).await.unwrap();
    let (strict, owner) = Strict::open(shared.clone(), ProviderKind::Claude, client);
    let effects = Arc::new(LiveEffects {
        shared: shared.clone(),
        provider: ProviderKind::Claude,
    });
    (Supervisor::strict(effects, strict), owner)
}

/// Whether `done` holds within `ms`, while spawned work runs.
async fn within(ms: u64, done: impl Fn() -> bool) -> bool {
    for _ in 0..ms / 5 {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    done()
}

/// Runs the loop's tick and due sends until `done`, within about ten seconds.
async fn tick_until(supervisor: &mut Supervisor<LiveEffects>, done: impl Fn() -> bool) -> bool {
    for _ in 0..200 {
        supervisor.tick(Instant::now()).await;
        supervisor.send_due(Instant::now()).await;
        if within(50, &done).await {
            return true;
        }
    }
    false
}

fn displayed(channel: u64) -> Option<Arc<Mutex<Presence>>> {
    lock(&PRESENCE).get(&channel).cloned()
}

/// A fixture whose channel the supervisors here walk, its watch already settled on Busy.
async fn busy_fixture() -> Fixture {
    let fixture = Fixture::new();
    assert_eq!(fixture.read().await.observed.activity, Activity::Busy);
    register(&ProviderKind::Claude, &[fixture.identity.channel]);
    fixture
}

// A Home mounting over the one an approval rests on clears the real presence's send, schedule and
// child at once; only a fresh reading on the new mount types again.
#[tokio::test(flavor = "current_thread")]
async fn b2b3_a_home_change_clears_the_real_presence_and_only_a_fresh_reading_types_again() {
    let _serial = SERIAL.lock().await;
    let fixture = busy_fixture().await;
    let channel = fixture.identity.channel;
    let wire = Wire::new(42, true);
    let (mut supervisor, _owner) = strict(&fixture.shared, &wire).await;
    assert!(
        tick_until(&mut supervisor, || wire.polls() == 1).await,
        "a fresh Busy types"
    );
    let presence = supervisor.channels[&channel].clone();
    assert!(lock(&presence).sending.is_some(), "the send is in flight");
    let old = supervisor.strict.as_ref().unwrap().ticket(channel).unwrap();
    let replacement = held_home(channel, 8);
    channel_home::register(replacement.clone());
    {
        let presence = lock(&presence);
        assert!(
            presence.sending.is_none(),
            "the send left with its approval"
        );
        assert_eq!(
            presence.next_typing_at, None,
            "no schedule outlived its approval"
        );
    }
    assert!(old.incarnation().is_none(), "the old registration is gone");
    assert!(
        within(2000, || wire.cancelled() == 1).await,
        "the send's task was cancelled"
    );
    assert!(
        tick_until(&mut supervisor, || wire.polls() == 2).await,
        "a fresh reading on the new mount types again"
    );
    channel_home::unregister_if_same(&replacement);
}

// A send that returns after its registration was withdrawn counts only as late and schedules
// nothing, even though no transition moved the presence's generation.
#[tokio::test(flavor = "current_thread")]
async fn b2b3_a_send_returning_after_its_withdrawal_restores_no_schedule() {
    let _serial = SERIAL.lock().await;
    let fixture = busy_fixture().await;
    let channel = fixture.identity.channel;
    let wire = Wire::new(42, true);
    let (mut supervisor, _owner) = strict(&fixture.shared, &wire).await;
    assert!(tick_until(&mut supervisor, || wire.polls() == 1).await);
    let presence = supervisor.channels[&channel].clone();
    let shared = fixture.shared.clone();
    *lock(&wire.then) = Some(Box::new(move || {
        entrypoints::withdraw(&shared, channel, "turn_stopped");
    }));
    wire.release.notify_one();
    assert!(within(2000, || wire.returned.load(Ordering::SeqCst) == 1).await);
    let presence = lock(&presence);
    assert_eq!(
        presence.next_typing_at, None,
        "a late completion restored a schedule"
    );
    assert_eq!((presence.sent_ok_total, presence.late_ok_total), (0, 1));
}

// Session cleanup's retire takes down this runtime's display and the next tick starts the channel
// fresh; a runtime that ends takes down only its own display, never a successor's.
#[tokio::test(flavor = "current_thread")]
async fn b2b3_retire_and_runtime_end_take_down_only_their_own_display() {
    let _serial = SERIAL.lock().await;
    let fixture = busy_fixture().await;
    let channel = fixture.identity.channel;
    let wire = Wire::new(42, false);
    let (mut first, first_owner) = strict(&fixture.shared, &wire).await;
    assert!(tick_until(&mut first, || wire.polls() == 1).await);
    let retired = first.channels[&channel].clone();
    entrypoints::retire(&fixture.shared, channel);
    assert!(displayed(channel).is_none(), "the retired display is down");
    assert_eq!(lock(&retired).next_typing_at, None);
    assert!(
        tick_until(&mut first, || wire.polls() == 2).await,
        "a fresh registration types again"
    );
    let fresh = first.channels[&channel].clone();
    assert!(
        !Arc::ptr_eq(&retired, &fresh),
        "the retired presence is not reused"
    );
    assert!(displayed(channel).is_some_and(|shown| Arc::ptr_eq(&shown, &fresh)));
    let (mut second, second_owner) = strict(&fixture.shared, &Wire::new(42, false)).await;
    second.tick(Instant::now()).await;
    let successor = second.channels[&channel].clone();
    drop(first_owner);
    assert!(
        displayed(channel).is_some_and(|shown| Arc::ptr_eq(&shown, &successor)),
        "an ended runtime took down its successor's display"
    );
    drop(second_owner);
    assert!(
        displayed(channel).is_none(),
        "an ended runtime takes down its own display"
    );
}

// With no owner to rest on, an approval is refused before any request is polled; that is neither a
// send error nor a reason to fall back to the direct send.
#[tokio::test(flavor = "current_thread")]
async fn b2b3_a_refused_approval_polls_no_request_and_is_no_send_error() {
    let _serial = SERIAL.lock().await;
    let fixture = busy_fixture().await;
    let channel = fixture.identity.channel;
    fixture.home.close();
    let wire = Wire::new(42, false);
    let (mut supervisor, _owner) = strict(&fixture.shared, &wire).await;
    supervisor.tick(Instant::now()).await;
    let presence = supervisor.channels[&channel].clone();
    assert_eq!(lock(&presence).activity, Activity::Busy);
    assert!(lock(&presence).sending.is_some(), "a send started");
    assert!(within(2000, || lock(&presence).sending.is_none()).await);
    assert_eq!(wire.polls(), 0, "a refused approval polled a request");
    let presence = lock(&presence);
    assert_eq!(presence.last_error, None, "a refusal is not a send error");
    let next = presence.next_typing_at.expect("the send cadence goes on");
    assert!(
        next <= Instant::now() + TYPING_EVERY,
        "a refusal took the error retry"
    );
}

// A boot dropped before its task's first poll leaves no runtime installed, an unverified client
// starts nothing, and a second start for the same runtime closes the first before it installs.
#[tokio::test(flavor = "current_thread")]
async fn b2b3_a_boot_owns_its_runtime_before_the_first_poll_and_a_restart_closes_the_first() {
    let _serial = SERIAL.lock().await;
    let fixture = busy_fixture().await;
    let shared = &fixture.shared;
    let start = |wire: &Arc<Wire>| {
        let transport = wire.clone() as Arc<dyn Transport>;
        strict_runtime::spawn(shared.clone(), ProviderKind::Claude, transport)
    };
    assert!(start(&Wire::new(0, false)).await.is_err());
    assert_eq!(
        entrypoints::installed_for(shared),
        0,
        "an unverified client"
    );
    let unpolled = Wire::new(42, false);
    let boot = start(&unpolled).await.unwrap();
    assert_eq!(
        entrypoints::installed_for(shared),
        1,
        "open before the first poll"
    );
    drop(boot);
    assert!(
        within(2000, || entrypoints::installed_for(shared) == 0).await,
        "the dropped boot closed its runtime"
    );
    assert_eq!(unpolled.polls(), 0);
    let first = Wire::new(42, true);
    start(&first).await.unwrap().hand_off();
    assert!(within(5000, || first.polls() == 1).await, "the first types");
    let second = Wire::new(42, true);
    let _boot = start(&second).await.unwrap();
    assert!(
        within(5000, || first.cancelled() == 1).await,
        "the restart closed the first runtime's send"
    );
    assert!(
        within(5000, || second.polls() == 1).await,
        "the second types"
    );
    assert!(within(2000, || entrypoints::installed_for(shared) == 1).await);
}

/// Binds `channel` as one more busy turn-mode channel of the fixture's runtime and binding root.
fn busy_channel(fixture: &Fixture, channel: u64) -> (TestConfirmation, InjectedLivenessGuard) {
    let session = format!("b2b3-held-{channel}");
    let path = fixture.root.path().join(format!("{channel}.jsonl"));
    let row = "{\"type\":\"user\",\"uuid\":\"a\",\"message\":{\"content\":\"work\"}}\n";
    std::fs::write(&path, row).unwrap();
    let provider = ProviderKind::Claude;
    let confirmed = bind_turn_mode_transcript(&fixture.shared, &provider, channel, &session, &path);
    let host = InjectedLivenessGuard::set(HostSessionRef::tmux(&session), HostLiveness::Live);
    (confirmed, host)
}

fn queued(message: u64) -> crate::services::turn_orchestrator::Intervention {
    crate::services::turn_orchestrator::Intervention {
        author_id: UserId::new(4_162_001),
        author_is_bot: false,
        message_id: MessageId::new(message),
        queued_generation: 0,
        source_message_ids: vec![MessageId::new(message)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "queued".to_string(),
        mode: crate::services::turn_orchestrator::InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

// The production spawn installs no runtime: its direct send still runs on its own client, a channel
// whose mailbox holds a token gets no typing from it, and an idle queue is still woken.
#[tokio::test(flavor = "current_thread")]
async fn b2b3_production_spawn_installs_nothing_and_keeps_direct_typing_the_hold_and_the_wake() {
    let _serial = SERIAL.lock().await;
    let fixture = busy_fixture().await;
    let channel = fixture.identity.channel;
    let held = NEXT.fetch_add(1, Ordering::SeqCst);
    let _held = busy_channel(&fixture, held);
    let id = ChannelId::new(held);
    for _ in 0..500 {
        let reading = reading_now(&fixture.shared, &ProviderKind::Claude, id).await;
        if reading.observed.activity == Activity::Busy {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let token = Arc::new(crate::services::provider::CancelToken::new());
    let start = crate::services::discord::mailbox_try_start_turn;
    assert!(
        start(
            &fixture.shared,
            id,
            token,
            UserId::new(1),
            MessageId::new(1)
        )
        .await
    );
    let idle = NEXT.fetch_add(1, Ordering::SeqCst);
    let _idle = TestConfirmation::new(idle);
    let enqueue = crate::services::discord::mailbox_enqueue_intervention;
    let provider = ProviderKind::Claude;
    enqueue(
        &fixture.shared,
        &provider,
        ChannelId::new(idle),
        queued(idle),
    )
    .await;
    register(&provider, &[held, idle]);
    spawn(fixture.shared.clone(), provider);
    let direct = || status(channel).is_some_and(|s| s["typing"]["last_error"] == "no http");
    assert!(
        within(5000, direct).await,
        "the direct send ran on its own client"
    );
    assert_eq!(
        entrypoints::installed_for(&fixture.shared),
        0,
        "production installs nothing"
    );
    let woken = || status(idle).is_some_and(|s| s["kickoffs_total"] == 1);
    assert!(within(5000, woken).await, "the idle queue was woken");
    let held = status(held).unwrap();
    assert_eq!(held["activity"], "busy");
    assert!(
        held["typing"]["last_error"].is_null(),
        "a held mailbox got typing"
    );
}
