use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::services::discord::turn_presence::activity::Observed;
use crate::services::tui_o::turn_mode::TestConfirmation;

const BUSY: Observed = Observed {
    activity: Activity::Busy,
    reason: "open",
};
const IDLE: Observed = Observed {
    activity: Activity::Idle,
    reason: "closed",
};
const UNKNOWN: Observed = Observed {
    activity: Activity::Unknown,
    reason: "facts_halted",
};

/// Scripted reads and recorded sends, timed from the test's start on tokio's paused clock.
struct Fake {
    start: Instant,
    channels: Vec<u64>,
    readings: Mutex<HashMap<u64, Observed>>,
    overtaken: AtomicBool,
    token: AtomicBool,
    head: Mutex<Option<u64>>,
    typing_delay: Mutex<HashMap<u64, Duration>>,
    typed: Mutex<Vec<(u64, Duration)>>,
    kicks: Mutex<Vec<(u64, Duration)>>,
    notices: Mutex<Vec<(u64, Duration, String)>>,
    notice_delay: Mutex<Option<Duration>>,
    gate_blocked: AtomicBool,
    /// When set, the mailbox answer comes from this runtime's real mailbox and disk markers.
    live_mailbox: Mutex<Option<Arc<SharedData>>>,
}

impl Fake {
    fn new(channels: &[u64], observed: Observed) -> Arc<Self> {
        Arc::new(Self {
            start: Instant::now(),
            channels: channels.to_vec(),
            readings: Mutex::new(channels.iter().map(|&c| (c, observed)).collect()),
            overtaken: AtomicBool::new(false),
            token: AtomicBool::new(false),
            head: Mutex::new(None),
            typing_delay: Mutex::new(HashMap::new()),
            typed: Mutex::new(Vec::new()),
            kicks: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
            notice_delay: Mutex::new(None),
            gate_blocked: AtomicBool::new(false),
            live_mailbox: Mutex::new(None),
        })
    }

    fn set(&self, channel: u64, observed: Observed) {
        lock(&self.readings).insert(channel, observed);
    }

    fn since(&self) -> Duration {
        Instant::now().duration_since(self.start)
    }

    fn notice_secs(&self) -> Vec<u64> {
        lock(&self.notices).iter().map(|n| n.1.as_secs()).collect()
    }

    fn typed_secs(&self, channel: u64) -> Vec<u64> {
        let typed = lock(&self.typed);
        typed
            .iter()
            .filter(|t| t.0 == channel)
            .map(|t| t.1.as_secs())
            .collect()
    }

    fn kick_secs(&self) -> Vec<u64> {
        lock(&self.kicks).iter().map(|k| k.1.as_secs()).collect()
    }

    fn notices(&self) -> Vec<(u64, String)> {
        let notices = lock(&self.notices);
        notices
            .iter()
            .map(|n| (n.1.as_secs(), n.2.clone()))
            .collect()
    }
}

#[async_trait]
impl Effects for Fake {
    fn channels(&self) -> Vec<u64> {
        self.channels.clone()
    }

    async fn read(&self, channel: u64) -> Reading {
        let observed = lock(&self.readings)[&channel];
        if self.overtaken.load(Ordering::SeqCst) {
            return Reading::overtaken_for_tests(observed);
        }
        Reading::unwatched_for_tests(observed, Some("pane"), Some("/t.jsonl"))
    }

    async fn mailbox(&self, channel: u64) -> (bool, Option<u64>) {
        let live = lock(&self.live_mailbox).clone();
        if let Some(shared) = live {
            return waiting_input(&shared, &ProviderKind::Claude, ChannelId::new(channel)).await;
        }
        (self.token.load(Ordering::SeqCst), *lock(&self.head))
    }

    async fn typing(&self, channel: u64) -> Result<(), String> {
        lock(&self.typed).push((channel, self.since()));
        let delay = lock(&self.typing_delay).get(&channel).copied();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        Ok(())
    }

    fn kickoff(&self, channel: u64) {
        lock(&self.kicks).push((channel, self.since()));
    }

    async fn legacy_gate_blocked(&self, _: u64) -> bool {
        self.gate_blocked.load(Ordering::SeqCst)
    }

    async fn notice(&self, channel: u64, text: String) -> Result<(), String> {
        lock(&self.notices).push((channel, self.since(), text));
        let delay = *lock(&self.notice_delay);
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        Ok(())
    }

    async fn inject_target(&self, _: u64) -> Option<(String, String)> {
        None
    }
}

/// Publishes a presence for `channel` as a running supervisor would.
pub(crate) fn seed_presence_for_tests(channel: u64, busy: bool) {
    let mut presence = Presence::new();
    presence.activity = if busy {
        Activity::Busy
    } else {
        Activity::Unknown
    };
    lock(&PRESENCE).insert(channel, Arc::new(Mutex::new(presence)));
}

/// Runs the supervisor over `fake` for the life of the returned handle.
fn supervise(fake: &Arc<Fake>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(Supervisor::new(fake.clone()).run())
}

async fn wait(secs: u64) {
    tokio::time::sleep(Duration::from_secs(secs)).await;
}

fn typing_counts(channel: u64) -> (u64, u64) {
    let json = status(channel).expect("turn-mode channel");
    let typing = &json["typing"];
    let count = |name: &str| typing[name].as_u64().unwrap();
    (count("sent_ok_total"), count("late_ok_total"))
}

/// Busy types at once, then every 8 seconds exactly, and stops when the channel reads idle.
#[tokio::test(start_paused = true)]
async fn busy_types_at_once_then_every_eight_seconds_and_idle_stops_it() {
    let channel = 6_200_001;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], BUSY);
    let run = supervise(&fake);
    wait(31).await;
    assert_eq!(fake.typed_secs(channel), [0, 8, 16, 24]);
    fake.set(channel, IDLE);
    wait(40).await;
    run.abort();
    assert_eq!(fake.typed_secs(channel), [0, 8, 16, 24, 32]);
    assert_eq!(typing_counts(channel), (5, 0));
}

/// A send still in flight when the channel turns idle is aborted: it counts nothing and leaves
/// no deadline; a late success observed for an older generation counts only as late.
#[tokio::test(start_paused = true)]
async fn a_send_overtaken_by_idle_counts_nothing_and_schedules_nothing() {
    let channel = 6_200_002;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], BUSY);
    lock(&fake.typing_delay).insert(channel, Duration::from_secs(5));
    let run = supervise(&fake);
    wait(1).await;
    fake.set(channel, IDLE);
    wait(60).await;
    run.abort();
    assert_eq!(fake.typed_secs(channel), [0]);
    assert_eq!(typing_counts(channel), (0, 0));

    // A send from an earlier busy spell that lands in a later one is late, not this spell's.
    let (mut presence, now) = (Presence::new(), Instant::now());
    let read = |observed| Reading::unwatched_for_tests(observed, Some("pane"), Some("/t.jsonl"));
    let mut first = None;
    for observed in [BUSY, IDLE, BUSY] {
        let actions = apply(&mut presence, channel, now, &read(observed), false, None);
        first = first.or(actions.into_iter().find_map(|action| match action {
            Action::Typing(generation) => Some(generation),
            _ => None,
        }));
    }
    typed(&mut presence, channel, first.unwrap(), now, Ok(()));
    assert_eq!(presence.next_typing_at, None);
    let typing = &presence.json()["typing"];
    assert_eq!(
        (&typing["sent_ok_total"], &typing["late_ok_total"]),
        (&0.into(), &1.into())
    );
}

/// A mailbox token's own turn types for itself, so a busy channel holding one gets no presence send.
#[tokio::test(start_paused = true)]
async fn a_mailbox_token_keeps_presence_from_typing() {
    let channel = 6_200_003;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], BUSY);
    fake.token.store(true, Ordering::SeqCst);
    let run = supervise(&fake);
    wait(41).await;
    fake.token.store(false, Ordering::SeqCst);
    wait(10).await;
    run.abort();
    assert_eq!(fake.typed_secs(channel), [48]);
}

/// Unknown for a full minute posts one notice; it does not post again within 30 minutes, even
/// across a recovery and a new unknown, and may once that half hour has passed.
#[tokio::test(start_paused = true)]
async fn unknown_for_a_minute_notices_once_per_half_hour() {
    let channel = 6_200_004;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], UNKNOWN);
    let run = supervise(&fake);
    wait(59).await;
    assert_eq!(fake.notices(), []);
    wait(2).await;
    fake.set(channel, IDLE);
    wait(10).await;
    fake.set(channel, UNKNOWN);
    wait(20 * 60).await;
    assert_eq!(fake.notice_secs(), [60]);
    wait(10 * 60).await;
    run.abort();
    assert_eq!(fake.notice_secs(), [60, 1860]);
    assert!(fake.notices()[0].1.contains("(사유: facts_halted)"));
    assert!(status(channel).unwrap()["unknown_notice"]["posted_at"].is_string());
}

/// A new supervisor over an idle channel sends nothing, wakes nothing and posts nothing.
#[tokio::test(start_paused = true)]
async fn a_new_supervisor_over_an_idle_channel_stays_silent() {
    let channel = 6_200_005;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], IDLE);
    let run = supervise(&fake);
    wait(120).await;
    run.abort();
    assert_eq!(fake.typed_secs(channel), Vec::<u64>::new());
    assert_eq!((fake.kick_secs(), fake.notices()), (vec![], vec![]));
    assert_eq!(typing_counts(channel), (0, 0));
}

/// An idle channel with a queue and no token is woken at once on the transition and every 30
/// seconds while the queue stays; a token or an empty queue wakes nothing.
#[tokio::test(start_paused = true)]
async fn an_idle_queue_is_woken_on_the_transition_and_every_half_minute() {
    let channel = 6_200_006;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], BUSY);
    *lock(&fake.head) = Some(7);
    let run = supervise(&fake);
    wait(10).await;
    assert_eq!(fake.kick_secs(), Vec::<u64>::new(), "busy holds the queue");
    fake.set(channel, IDLE);
    wait(65).await;
    assert_eq!(fake.kick_secs(), [12, 42, 72]);
    fake.token.store(true, Ordering::SeqCst);
    wait(40).await;
    fake.token.store(false, Ordering::SeqCst);
    *lock(&fake.head) = None;
    wait(40).await;
    run.abort();
    assert_eq!(fake.kick_secs(), [12, 42, 72]);
    assert_eq!(status(channel).unwrap()["kickoffs_total"], 3);
}

/// A head two wakes did not move is looked into once, named after the Legacy gate only when
/// that gate blocks, and noticed after two more minutes.
#[tokio::test(start_paused = true)]
async fn a_queue_two_wakes_did_not_move_is_held_and_noticed_with_its_cause() {
    for (channel, blocked, words) in [
        (6_200_007, true, "Legacy TUI 판정에 막혀"),
        (6_200_008, false, "확인이 필요합니다"),
    ] {
        let _confirmed = TestConfirmation::new(channel);
        let fake = Fake::new(&[channel], IDLE);
        *lock(&fake.head) = Some(9);
        fake.gate_blocked.store(blocked, Ordering::SeqCst);
        let run = supervise(&fake);
        wait(31).await;
        assert!(status(channel).unwrap()["queue_held"]["reason"].is_null());
        wait(3).await;
        let reason = match blocked {
            true => "legacy_tui_gate",
            false => "unknown",
        };
        assert_eq!(status(channel).unwrap()["queue_held"]["reason"], reason);
        wait(125).await;
        run.abort();
        let notices = fake.notices();
        assert_eq!(notices.len(), 1, "{reason}");
        assert!(notices[0].1.contains(words), "{}", notices[0].1);
    }
}

/// A pending marker left on disk alone, with no queue or reservation in memory, is woken from
/// the real mailbox and store at the idle transition and every 30 seconds after.
#[tokio::test(start_paused = true)]
async fn a_marker_left_on_disk_alone_is_woken_at_once_and_every_half_minute() {
    let _root = crate::services::discord::relay_recovery::tests::isolated_agentdesk_root();
    let channel = 6_200_016;
    let _confirmed = TestConfirmation::new(channel);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let marker = crate::services::discord::relay_recovery::tests::orphan_token_finish::queued(31);
    let save = crate::services::turn_orchestrator::save_channel_pending_dispatch_marker;
    let (provider, id) = (ProviderKind::Claude, ChannelId::new(channel));
    save(&provider, &shared.token_hash, id, &marker, None).unwrap();
    let fake = Fake::new(&[channel], BUSY);
    *lock(&fake.live_mailbox) = Some(shared.clone());
    let run = supervise(&fake);
    wait(10).await;
    assert_eq!(fake.kick_secs(), Vec::<u64>::new(), "busy holds the marker");
    fake.set(channel, IDLE);
    wait(65).await;
    run.abort();
    assert_eq!(fake.kick_secs(), [12, 42, 72]);
}

/// A notice that takes 30 seconds to post holds neither another channel's typing nor its stop.
#[tokio::test(start_paused = true)]
async fn a_slow_notice_holds_only_its_own_channel() {
    let (unknown, busy) = (6_200_014, 6_200_015);
    let _confirmed = (TestConfirmation::new(unknown), TestConfirmation::new(busy));
    let fake = Fake::new(&[unknown, busy], BUSY);
    fake.set(unknown, UNKNOWN);
    *lock(&fake.notice_delay) = Some(Duration::from_secs(30));
    let run = supervise(&fake);
    wait(70).await;
    fake.set(busy, IDLE);
    wait(30).await;
    run.abort();
    assert_eq!(fake.notice_secs(), [60]);
    assert_eq!(fake.typed_secs(busy), [0, 8, 16, 24, 32, 40, 48, 56, 64]);
}

/// One channel's slow send never delays another channel's typing.
#[tokio::test(start_paused = true)]
async fn a_slow_send_holds_only_its_own_channel() {
    let (slow, fast) = (6_200_009, 6_200_010);
    let _confirmed = (TestConfirmation::new(slow), TestConfirmation::new(fast));
    let fake = Fake::new(&[slow, fast], BUSY);
    lock(&fake.typing_delay).insert(slow, Duration::from_secs(100));
    let run = supervise(&fake);
    wait(20).await;
    run.abort();
    assert_eq!(fake.typed_secs(slow), [0]);
    assert_eq!(fake.typed_secs(fast), [0, 8, 16]);
}

/// A reading a later poll moved past publishes nothing: no transition, no typing, no wake.
#[tokio::test(start_paused = true)]
async fn an_overtaken_reading_is_never_published() {
    let channel = 6_200_011;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], BUSY);
    *lock(&fake.head) = Some(3);
    fake.overtaken.store(true, Ordering::SeqCst);
    let run = supervise(&fake);
    wait(20).await;
    run.abort();
    assert_eq!(fake.typed_secs(channel), Vec::<u64>::new());
    let json = status(channel).unwrap();
    assert_eq!(
        (json["activity"].as_str(), json["generation"].as_u64()),
        (Some("unknown"), Some(0))
    );
}

/// Only confirmed turn-mode channels are supervised, and only they get a `turn_presence` block,
/// which carries every field the turn endpoint documents.
#[tokio::test(start_paused = true)]
async fn only_confirmed_channels_are_supervised_and_reported() {
    let (confirmed, unconfirmed) = (6_200_012, 6_200_013);
    let provider = ProviderKind::Codex;
    register(&provider, &[confirmed, unconfirmed]);
    let _confirmed = TestConfirmation::new(confirmed);
    let listed = registered(&provider);
    assert!(listed.contains(&confirmed) && !listed.contains(&unconfirmed));
    let fake = Fake::new(&[confirmed], IDLE);
    let run = supervise(&fake);
    wait(1).await;
    run.abort();
    assert_eq!(status(unconfirmed), None);
    let json = status(confirmed).unwrap();
    let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    let expected = [
        "activity",
        "activity_since",
        "generation",
        "kickoffs_total",
        "n1_observation",
        "observed_at",
        "process_started_at",
        "queue_held",
        "reason",
        "session",
        "source_id",
        "typing",
        "unknown_notice",
    ];
    assert_eq!(keys, expected);
    assert_eq!(json["n1_observation"]["status"], "unavailable");
    assert_eq!(
        (json["session"].as_str(), json["source_id"].as_str()),
        (Some("pane"), Some("/t.jsonl"))
    );
    let mut typing: Vec<_> = json["typing"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    typing.sort();
    assert_eq!(
        typing,
        ["last_error", "last_ok_at", "late_ok_total", "sent_ok_total"]
    );
}
