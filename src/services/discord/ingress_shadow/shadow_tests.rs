use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use futures::FutureExt;

use super::telemetry::{Observed, Record};
use super::worker::{FixedRead, judge};
use super::*;
use crate::db::o_channel_homes::{ChannelHome, HeldHome, HomeState};
use crate::services::cluster::channel_home::{self, HomeGate};
use crate::services::cluster::channel_ingress::observed_calls;

const CH: u64 = 5_713_000_000_000_201;
const LOCAL: &str = "node-a";

fn renewed() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap()
}

fn worker_row(channel: u64, holder: &str, epoch: i64) -> ChannelHome {
    ChannelHome {
        channel_id: channel.to_string(),
        provider: "claude".into(),
        state: HomeState::Worker,
        holder: Some(holder.into()),
        target: None,
        epoch,
        renewed_at: Some(renewed()),
        updated_at: renewed(),
        detail: None,
    }
}

/// A lookup answering `row` read `age` after its renewal, after `delay`.
fn fixed(row: Option<ChannelHome>, age: Duration, delay: Duration) -> FixedRead {
    Arc::new(move |_| {
        let row = row.clone();
        async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            Ok(row.map(|row| (row, renewed() + chrono::Duration::from_std(age).unwrap())))
        }
        .boxed()
    })
}

/// How a test observer answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum Observer {
    /// Reads a foreign holder's fresh row at once.
    Answering,
    /// Its lookup outlives the timeout.
    TimingOut,
    /// Its queue is already full and never drained.
    Full,
    /// Its worker is gone.
    Closed,
}

/// What a test reads back from a started shadow.
pub(in crate::services::discord) struct Probe {
    records: Arc<Mutex<Vec<Record>>>,
    counts: Arc<Counts>,
    _held: Option<mpsc::Receiver<Descriptor>>,
}

impl Probe {
    pub(in crate::services::discord) fn submitted(&self) -> u64 {
        self.counts.submitted.load(Ordering::Acquire)
    }

    pub(in crate::services::discord) fn completed(&self) -> u64 {
        self.counts.completed.load(Ordering::Acquire)
    }

    pub(in crate::services::discord) fn dropped(&self) -> u64 {
        let read = |count: &std::sync::atomic::AtomicU64| count.load(Ordering::Acquire);
        read(&self.counts.dropped_full) + read(&self.counts.dropped_closed)
    }

    /// Waits until `count` descriptors are judged; false at the deadline.
    pub(in crate::services::discord) async fn judged(&self, count: u64, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while self.completed() < count {
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        true
    }

    fn observed(&self) -> Vec<Observed> {
        let records = self.records.lock().unwrap();
        let observed = records.iter().filter_map(|record| match record {
            Record::Observed(observed) => Some((**observed).clone()),
            _ => None,
        });
        observed.collect()
    }
}

fn scope(records: &Arc<Mutex<Vec<Record>>>, channels: &BTreeSet<u64>) -> Scope {
    Scope {
        provider: HomeProvider::Claude,
        bot: "bot-1".into(),
        generation: 7,
        instance_id: LOCAL.into(),
        allowlist: channels.iter().copied().collect(),
        build: Some("build-1".into()),
        sink: Sink::Capture(Arc::clone(records)),
    }
}

fn started(channels: &[u64], source: HomeSource) -> (IngressShadow, Probe) {
    let channels: BTreeSet<u64> = channels.iter().copied().collect();
    let records = Arc::new(Mutex::new(Vec::new()));
    let shadow = spawn(&channels, scope(&records, &channels), source);
    let counts = Arc::clone(&shadow.counts);
    let probe = Probe {
        records,
        counts,
        _held: None,
    };
    (shadow, probe)
}

/// A shadow on `channels` answering as `observer` does; must run inside a tokio runtime.
pub(in crate::services::discord) fn observer(
    observer: Observer,
    channels: &[u64],
) -> (IngressShadow, Probe) {
    match observer {
        Observer::Answering => {
            let answer: FixedRead = Arc::new(|channel| {
                let row = worker_row(channel, "node-b", 3);
                let read = fixed(Some(row), Duration::from_secs(1), Duration::ZERO);
                read(channel)
            });
            started(channels, HomeSource::Fixed(answer))
        }
        Observer::TimingOut => {
            let slow = fixed(None, Duration::ZERO, Duration::from_secs(5));
            started(channels, HomeSource::Fixed(slow))
        }
        Observer::Full | Observer::Closed => {
            let (tx, rx) = mpsc::channel(QUEUE);
            let shadow = IngressShadow {
                tx,
                channels: channels.iter().copied().collect(),
                counts: Arc::default(),
            };
            let filler = Descriptor {
                sequence: 0,
                kind: EventKind::Message,
                channel: NonZeroU64::MIN,
                source_id: 0,
                application_id: None,
                received: Instant::now(),
            };
            let held = if observer == Observer::Full {
                (0..QUEUE).for_each(|_| shadow.tx.try_send(filler).unwrap());
                Some(rx)
            } else {
                None
            };
            let probe = Probe {
                records: Arc::default(),
                counts: Arc::clone(&shadow.counts),
                _held: held,
            };
            (shadow, probe)
        }
    }
}

fn message(channel: u64, id: u64) -> serenity::FullEvent {
    let mut new_message = serenity::Message::default();
    new_message.channel_id = serenity::ChannelId::new(channel);
    new_message.id = serenity::MessageId::new(id);
    serenity::FullEvent::Message { new_message }
}

fn interaction(kind: u8, channel: u64, id: u64) -> serenity::FullEvent {
    let mut json = serde_json::json!({
        "id": id.to_string(), "application_id": "77", "type": kind, "token": "secret-token",
        "channel_id": channel.to_string(), "version": 1, "locale": "en-US",
        "user": {"id": "5", "username": "u", "discriminator": "0"}, "authorizing_integration_owners": {},
        "app_permissions": "0", "entitlements": [], "attachment_size_limit": 26214400
    });
    json["data"] = match kind {
        3 => serde_json::json!({"custom_id": "x", "component_type": 2}),
        5 => serde_json::json!({"custom_id": "x", "components": []}),
        _ => serde_json::json!({"id": "9", "name": "help", "type": 1}),
    };
    if kind == 3 {
        json["message"] = serde_json::to_value(serenity::Message::default()).unwrap();
        json["message"]["channel_id"] = serde_json::json!(channel.to_string());
    }
    let interaction = serde_json::from_value(json).expect("interaction fixture");
    serenity::FullEvent::InteractionCreate { interaction }
}

#[test]
fn only_the_switch_with_listed_channels_starts_an_observer() {
    let parse = |yaml: &str| {
        let runtime: crate::config::RuntimeSettingsConfig = serde_yaml::from_str(yaml).unwrap();
        let config = crate::config::Config {
            runtime,
            ..crate::config::Config::default()
        };
        ShadowConfig::of(&config)
    };
    for off in [
        "{}",
        "channel_ingress_shadow_enabled: false\nchannel_ingress_shadow_channels: [11]",
        "channel_ingress_shadow_enabled: true",
        "channel_ingress_shadow_enabled: true\nchannel_ingress_shadow_channels: []",
        "channel_ingress_shadow_enabled: true\nchannel_ingress_shadow_channels: [0]",
    ] {
        assert_eq!(parse(off), None, "{off}");
        let runtime: crate::config::RuntimeSettingsConfig = serde_yaml::from_str(off).unwrap();
        assert_eq!(
            runtime.is_empty(),
            off == "{}",
            "{off}: kept on a round-trip"
        );
    }
    let on =
        parse("channel_ingress_shadow_enabled: true\nchannel_ingress_shadow_channels: [11, 0, 12]");
    let on = on.expect("switch and channels");
    assert_eq!(on.channels, BTreeSet::from([11, 12]));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let begun = std::cell::Cell::new(0);
        let begin = || {
            begun.set(begun.get() + 1);
            Begin {
                bot: "bot-1".into(),
                generation: 1,
                pool: None,
            }
        };
        let (calls, samples) = (
            observed_calls(),
            channel_home::ingress_snapshot::sampled_calls(),
        );
        assert!(start_if_enabled(None, &ProviderKind::Claude, begin).is_none());
        assert!(start_if_enabled(Some(&on), &ProviderKind::Gemini, begin).is_none());
        observe(None, &message(11, 1));
        assert_eq!(begun.get(), 0, "off prepares nothing");
        let shadow = start_if_enabled(Some(&on), &ProviderKind::Claude, begin)
            .expect("the switch with a listed channel starts the observer");
        observe(Some(&shadow), &message(11, 1));
        observe(Some(&shadow), &message(13, 2));
        assert_eq!(begun.get(), 1);
        assert_eq!(
            shadow.counts.submitted.load(Ordering::Acquire),
            1,
            "listed channel only"
        );
        assert_eq!(
            (
                observed_calls(),
                channel_home::ingress_snapshot::sampled_calls()
            ),
            (calls, samples),
            "the hook never judges or samples on the event's thread"
        );
    });
    // The started worker digests the test binary; nothing here waits on it.
    runtime.shutdown_background();
}

#[test]
fn an_event_is_described_by_its_own_channel_and_ids() {
    let ids =
        |event| describe(&event).map(|(kind, channel, id, app)| (kind, channel.get(), id, app));
    assert_eq!(
        ids(message(CH, 31)),
        Some((EventKind::Message, CH, 31, None))
    );
    assert_eq!(
        ids(interaction(2, CH, 32)),
        Some((EventKind::Command, CH, 32, Some(77)))
    );
    assert_eq!(
        ids(interaction(3, CH, 33)),
        Some((EventKind::Component, CH, 33, Some(77)))
    );
    assert_eq!(
        ids(interaction(4, CH, 34)),
        Some((EventKind::Autocomplete, CH, 34, Some(77)))
    );
    assert_eq!(
        ids(interaction(5, CH, 35)),
        Some((EventKind::Modal, CH, 35, Some(77)))
    );
    let delete = serenity::FullEvent::MessageDelete {
        channel_id: serenity::ChannelId::new(CH),
        deleted_message_id: serenity::MessageId::new(36),
        guild_id: None,
    };
    assert_eq!(ids(delete), None);
}

#[test]
fn a_full_or_closed_queue_drops_and_counts_without_waiting() {
    for (kind, full, closed) in [(Observer::Full, 1, 0), (Observer::Closed, 0, 1)] {
        let (shadow, probe) = observer(kind, &[CH]);
        observe(Some(&shadow), &message(CH, 41));
        let read = |count: &std::sync::atomic::AtomicU64| count.load(Ordering::Acquire);
        assert_eq!(
            probe.submitted(),
            1,
            "{kind:?}: the dropped one has its sequence"
        );
        assert_eq!(
            (
                read(&shadow.counts.dropped_full),
                read(&shadow.counts.dropped_closed)
            ),
            (full, closed),
            "{kind:?}"
        );
    }
}

/// The worker on this thread's registry: the row read with PG's clock, the gate sampled as the
/// row's provider, and the contract's verdict recorded with the event's ids.
#[tokio::test(start_paused = true)]
async fn the_worker_judges_the_real_gate_and_records_every_id() {
    let gate = Arc::new(HomeGate::new(&CH.to_string(), LOCAL));
    let written = HeldHome::for_test(&CH.to_string(), LOCAL, 4, HomeState::Worker);
    gate.confirm(&written, Instant::now()).unwrap();
    channel_home::register(Arc::clone(&gate));
    let rows = [
        Some(worker_row(CH, LOCAL, 4)),
        Some(worker_row(CH, "node-b", 4)),
        Some(worker_row(CH, LOCAL, 5)),
        None,
    ];
    let mut seen = Vec::new();
    for (index, row) in rows.into_iter().enumerate() {
        let source = HomeSource::Fixed(fixed(row, Duration::from_secs(1), Duration::ZERO));
        let (shadow, probe) = started(&[CH], source);
        let id = 51 + index as u64;
        observe(Some(&shadow), &message(CH, id));
        assert!(
            probe.judged(1, Duration::from_secs(1)).await,
            "worker completed"
        );
        let observed = probe.observed();
        let [observed] = observed.as_slice() else {
            panic!("one record: {observed:?}")
        };
        assert_eq!(
            (
                observed.kind,
                observed.channel,
                observed.source_id,
                observed.sequence
            ),
            (EventKind::Message, CH, id, 1)
        );
        assert_eq!(
            (
                observed.instance_id.as_str(),
                observed.bot.as_str(),
                observed.provider.as_str()
            ),
            (LOCAL, "bot-1", "claude")
        );
        seen.push((
            observed.verdict.clone(),
            observed.reason.clone(),
            observed.row_epoch,
            observed.local_epoch,
            observed.sampler.clone(),
        ));
    }
    let row = |verdict: &str, reason: Option<&str>, row, local, sampler: &str| {
        (
            verdict.to_owned(),
            reason.map(str::to_owned),
            row,
            local,
            sampler.to_owned(),
        )
    };
    assert_eq!(
        seen,
        vec![
            row("local_candidate", None, Some(4), Some(4), "sampled"),
            row("ignore_foreign", None, Some(4), Some(4), "sampled"),
            row(
                "hold",
                Some("LocalEpochMismatch"),
                Some(5),
                Some(4),
                "sampled"
            ),
            row("hold", Some("NoHomeAuthority"), None, None, "not_sampled"),
        ]
    );
    assert!(gate.expiry().is_some(), "judging left the gate held");
    channel_home::unregister(&CH.to_string());
}

/// Freshness runs on PG's clock from before the pool acquire to the verdict; an unknown clock or a
/// lookup past its timeout holds and counts as unknown.
#[tokio::test(start_paused = true)]
async fn freshness_counts_the_whole_lookup_on_the_database_clock() {
    let foreign = Some(worker_row(CH, "node-b", 4));
    let ms = Duration::from_millis;
    let cases = [
        (ms(19_700), ms(200), "ignore_foreign", None, false),
        (ms(19_900), ms(200), "hold", Some("RowExpired"), false),
        (ms(19_900), ms(0), "ignore_foreign", None, false),
        (ms(20_000), ms(0), "hold", Some("RowExpired"), false),
        (ms(300), ms(300), "hold", Some("AuthorityUnreadable"), true),
    ];
    for (age, delay, verdict, reason, unknown) in cases {
        let source = HomeSource::Fixed(fixed(foreign.clone(), age, delay));
        let (shadow, probe) = started(&[CH], source);
        observe(Some(&shadow), &message(CH, 61));
        assert!(probe.judged(1, Duration::from_secs(1)).await);
        let observed = probe.observed().remove(0);
        let expected = (verdict, reason, u64::from(unknown));
        let unknowns = probe.counts.unknown.load(Ordering::Acquire);
        assert_eq!(
            (
                observed.verdict.as_str(),
                observed.reason.as_deref(),
                unknowns
            ),
            expected,
            "age {age:?} delay {delay:?}"
        );
    }
    let mut ahead = worker_row(CH, "node-b", 4);
    ahead.renewed_at = Some(renewed() + chrono::Duration::seconds(2));
    let source = HomeSource::Fixed(fixed(Some(ahead), Duration::from_secs(1), Duration::ZERO));
    let (shadow, probe) = started(&[CH], source);
    observe(Some(&shadow), &message(CH, 62));
    assert!(probe.judged(1, Duration::from_secs(1)).await);
    let observed = probe.observed().remove(0);
    assert_eq!(observed.reason.as_deref(), Some("TimeUnknown"));
    assert_eq!(probe.counts.unknown.load(Ordering::Acquire), 1);
}

#[tokio::test(start_paused = true)]
async fn an_unreadable_gate_holds_and_counts_as_unknown() {
    let gate = Arc::new(HomeGate::new(&CH.to_string(), LOCAL));
    let written = HeldHome::for_test(&CH.to_string(), LOCAL, 4, HomeState::Worker);
    gate.confirm(&written, Instant::now()).unwrap();
    channel_home::register(Arc::clone(&gate));
    let records = Arc::new(Mutex::new(Vec::new()));
    let channels = BTreeSet::from([CH]);
    let scope = scope(&records, &channels);
    let source = HomeSource::Fixed(fixed(
        Some(worker_row(CH, LOCAL, 4)),
        Duration::ZERO,
        Duration::ZERO,
    ));
    let descriptor = Descriptor {
        sequence: 1,
        kind: EventKind::Message,
        channel: NonZeroU64::new(CH).unwrap(),
        source_id: 71,
        application_id: None,
        received: Instant::now(),
    };
    // A writer's admission holds the gate's inner lock while this judges.
    let judging = || {
        judge(&scope, "inc", &source, descriptor)
            .now_or_never()
            .unwrap()
    };
    let busy = gate.gate().admit(|_| judging()).expect("held gate admits");
    let free = judge(&scope, "inc", &source, descriptor)
        .now_or_never()
        .unwrap();
    channel_home::unregister(&CH.to_string());
    assert_eq!(
        (busy.0.reason.as_deref(), busy.0.sampler.as_str(), busy.1),
        (Some("NoLocalLease"), "inner_busy", true)
    );
    assert_eq!(
        (free.0.verdict.as_str(), free.1),
        ("local_candidate", false)
    );
}

/// The observer's own one-connection pool reads the delegated row with PG's clock, and closes
/// once the shadow is dropped.
#[tokio::test]
async fn the_observer_pool_reads_rows_on_one_connection_and_closes_pg() {
    use crate::db::o_channel_homes::{adopt, delegate, finish_release};
    let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let channel = CH.to_string();
    delegate(&pool, &channel, "claude", "gw", "node-b")
        .await
        .unwrap();
    finish_release(&pool, &channel, "gw", 1).await.unwrap();
    adopt(&pool, &channel, "node-b", 2).await.unwrap();

    let source = HomeSource::of(Some(&pool));
    let HomeSource::Pg(observer_pool) = &source else {
        panic!("a runtime pool gives a database source");
    };
    let observer_pool = observer_pool.clone();
    assert_eq!(observer_pool.options().get_max_connections(), 1);
    assert_eq!(
        observer_pool.size(),
        0,
        "nothing connects before the first lookup"
    );
    let (shadow, probe) = started(&[CH, CH + 1], source);
    observe(Some(&shadow), &message(CH, 111));
    observe(Some(&shadow), &message(CH + 1, 112));
    assert!(
        probe.judged(2, Duration::from_secs(10)).await,
        "both judged"
    );
    drop(shadow);
    let deadline = Instant::now() + Duration::from_secs(10);
    let terminal = |records: &Vec<Record>| {
        records
            .iter()
            .any(|record| matches!(record, Record::Close { terminal: true, .. }))
    };
    while !terminal(&probe.records.lock().unwrap()) {
        assert!(Instant::now() < deadline, "worker ended");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        observer_pool.is_closed(),
        "the observer pool closes with its worker"
    );
    let verdicts: Vec<_> = probe
        .observed()
        .into_iter()
        .map(|o| {
            (
                o.channel,
                o.lookup,
                o.verdict,
                o.reason,
                o.row_epoch,
                o.holder,
            )
        })
        .collect();
    assert_eq!(
        verdicts,
        vec![
            (
                CH,
                "row".into(),
                "ignore_foreign".into(),
                None,
                Some(2),
                Some("node-b".into())
            ),
            (
                CH + 1,
                "missing".into(),
                "hold".into(),
                Some("NoHomeAuthority".into()),
                None,
                None
            ),
        ]
    );
    pool.close().await;
    pg_db.drop().await;
}
