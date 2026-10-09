use std::io::Write;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::super::activity::tests::{BindingRoot, bind_turn_mode_transcript};
use super::super::activity::{self, Activity, Observed};
use super::*;
use crate::db::o_channel_homes::{HeldHome, HomeState};
use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;
use crate::services::session_host::test_support::InjectedLivenessGuard;
use crate::services::session_host::{HostLiveness, HostSessionRef};
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::turn_mode::TestConfirmation;

static CHANNEL: AtomicU64 = AtomicU64::new(6_325_100);

struct Fixture {
    _runtime: crate::config::TestRuntimeRootGuard,
    root: tempfile::TempDir,
    _binding: BindingRoot,
    shared: Arc<SharedData>,
    confirmed: Option<TestConfirmation>,
    _host: InjectedLivenessGuard,
    identity: Identity,
    home: Arc<HomeGate>,
    incarnation: Arc<Incarnation>,
}

impl Fixture {
    fn new() -> Self {
        let runtime = crate::config::TestRuntimeRootGuard::new();
        let root = tempfile::tempdir().unwrap();
        let binding = BindingRoot::enter(root.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = CHANNEL.fetch_add(1, Ordering::SeqCst);
        let session = format!("b1-parent-{channel}");
        let path = root.path().join("parent.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"user\",\"uuid\":\"a\",\"message\":{\"content\":\"work\"}}\n",
        )
        .unwrap();
        let confirmed =
            bind_turn_mode_transcript(&shared, &ProviderKind::Claude, channel, &session, &path);
        let host = InjectedLivenessGuard::set(HostSessionRef::tmux(&session), HostLiveness::Live);
        let identity = Identity {
            provider: ShadowProvider::Claude,
            channel,
            session,
            source: source_id_for("parent", &path).unwrap(),
            binding_seq: 1,
            bot_id: 42,
        };
        let home = Arc::new(HomeGate::new(&channel.to_string(), "b1-worker"));
        home.confirm(
            &HeldHome::for_test(&channel.to_string(), "b1-worker", 7, HomeState::Worker),
            tokio::time::Instant::now(),
        )
        .unwrap();
        channel_home::register(home.clone());
        let incarnation = Arc::new(Incarnation::default());
        assert!(incarnation.replace(identity.clone()));
        Self {
            _runtime: runtime,
            root,
            _binding: binding,
            shared,
            confirmed: Some(confirmed),
            _host: host,
            identity,
            home,
            incarnation,
        }
    }

    async fn read(&self) -> Reading {
        for _ in 0..500 {
            let reading = activity::presence_reading_now(
                &self.shared,
                &ProviderKind::Claude,
                poise::serenity_prelude::ChannelId::new(self.identity.channel),
            )
            .await;
            if reading.observed.reason != "catching_up" {
                return reading;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("presence reader never settled");
    }

    async fn approval(&self) -> Approval {
        let reading = self.read().await;
        assert_eq!(reading.identity(42), Some(self.identity.clone()));
        self.incarnation.approve(reading).expect("fresh live Busy")
    }

    fn append(&self, row: &str) {
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&self.identity.source.path)
                .unwrap(),
            "{row}"
        )
        .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        channel_home::unregister_if_same(&self.home);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn fresh_busy_admits_one_first_poll_and_response_waits_outside_locks() {
    let fixture = Fixture::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = tokio::sync::oneshot::channel();
    let counted = polls.clone();
    let started = fixture
        .approval()
        .await
        .start(42, fixture.identity.channel, || {
            let mut rx = Box::pin(rx);
            poll_fn(move |cx| {
                counted.fetch_add(1, Ordering::SeqCst);
                rx.as_mut().poll(cx)
            })
        })
        .await
        .expect("admitted");
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    // These changes cannot wait for a pending response and revoke the next attempt.
    fixture.incarnation.invalidate();
    fixture.home.close();
    tx.send("accepted").unwrap();
    assert_eq!(started.finish().await.unwrap(), "accepted");
    assert!(fixture.incarnation.approve(fixture.read().await).is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn unstamped_or_legacy_busy_cannot_authorize_a_typing_effect() {
    let fixture = Fixture::new();
    assert_eq!(fixture.read().await.observed.activity, Activity::Busy);
    let busy = Observed {
        activity: Activity::Busy,
        reason: "open",
    };
    for session in [None, Some(fixture.identity.session.as_str())] {
        let reading = Reading::unwatched_for_tests(busy, session, Some("parent.jsonl"));
        assert!(fixture.incarnation.approve(reading).is_none());
    }
    let legacy = activity::reading_now(
        &fixture.shared,
        &ProviderKind::Claude,
        poise::serenity_prelude::ChannelId::new(fixture.identity.channel),
    )
    .await;
    assert_eq!(legacy.observed.activity, Activity::Busy);
    assert!(
        fixture.incarnation.approve(legacy).is_none(),
        "legacy observer is dormant"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn provider_channel_session_source_and_binding_seq_must_match_the_reading() {
    let fixture = Fixture::new();
    for which in 0..7 {
        let mut foreign = fixture.identity.clone();
        match which {
            0 => foreign.provider = ShadowProvider::Codex,
            1 => foreign.channel += 1,
            2 => foreign.session.push_str("-other"),
            3 => foreign.source.ino += 1,
            4 => foreign.source.session_id.push_str("-other"),
            5 => foreign.source.path = fixture.root.path().join("other.jsonl"),
            _ => foreign.binding_seq += 1,
        }
        // Use a held home for each identity so a wrong owner never masks the stamp assertion.
        let home = Arc::new(HomeGate::new(&foreign.channel.to_string(), "b1-worker"));
        home.confirm(
            &HeldHome::for_test(
                &foreign.channel.to_string(),
                "b1-worker",
                8,
                HomeState::Worker,
            ),
            tokio::time::Instant::now(),
        )
        .unwrap();
        channel_home::register(home.clone());
        assert!(fixture.incarnation.replace(foreign));
        assert!(
            fixture.incarnation.approve(fixture.read().await).is_none(),
            "field {which}"
        );
        channel_home::unregister_if_same(&home);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actual_bot_and_channel_are_checked_again_before_request_creation() {
    let fixture = Fixture::new();
    let _other = TestConfirmation::new(fixture.identity.channel + 1);
    for (bot, channel) in [
        (43, fixture.identity.channel),
        (42, fixture.identity.channel + 1),
        (0, fixture.identity.channel),
        (42, 0),
    ] {
        let approved = fixture.approval().await;
        let result = approved
            .start(bot, channel, || -> std::future::Ready<()> {
                panic!("foreign target created HTTP")
            })
            .await;
        assert!(result.is_none());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unsupported_host_and_unreadable_inflight_are_unknown_without_writes() {
    let fixture = Fixture::new();
    let marker =
        crate::services::tmux_common::session_temp_path(&fixture.identity.session, "host_kind");
    std::fs::write(&marker, "herdr").unwrap();
    let reading = fixture.read().await;
    assert_eq!(
        reading.observed,
        Observed {
            activity: Activity::Unknown,
            reason: "host_unobservable"
        }
    );
    assert!(fixture.incarnation.approve(reading).is_none());
    std::fs::write(&marker, "tmux").unwrap();
    use crate::services::discord::inflight;
    let path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        fixture.identity.channel,
    );
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let broken = b"{broken";
    std::fs::write(&path, broken).unwrap();
    let reading = fixture.read().await;
    assert_eq!(
        reading.observed,
        Observed {
            activity: Activity::Unknown,
            reason: "host_probe_failed"
        }
    );
    assert!(fixture.incarnation.approve(reading).is_none());
    assert_eq!(std::fs::read(path).unwrap(), broken);
}

#[tokio::test(flavor = "current_thread")]
async fn zero_bot_or_channel_cannot_seed_runtime_authority() {
    let fixture = Fixture::new();
    let gateway = ownership::gate("claude");
    gateway.acquired();
    for field in ["bot", "channel"] {
        let mut identity = fixture.identity.clone();
        if field == "bot" {
            identity.bot_id = 0;
        } else {
            identity.channel = 0;
        }
        assert!(!fixture.incarnation.replace(identity));
        assert!(fixture.incarnation.approve(fixture.read().await).is_none());
    }
    gateway.lost();
}

#[tokio::test(flavor = "current_thread")]
async fn an_unpolled_attempt_is_rechecked_after_confirmation_is_removed() {
    let mut fixture = Fixture::new();
    let attempt = fixture.approval().await.start(
        42,
        fixture.identity.channel,
        || -> std::future::Ready<()> { panic!("mode changed before the first poll") },
    );
    drop(fixture.confirmed.take());
    assert!(attempt.await.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn replaced_and_restarted_incarnations_reject_previous_busy_approvals() {
    let fixture = Fixture::new();
    let old = fixture.approval().await;
    assert!(fixture.incarnation.replace(fixture.identity.clone()));
    assert!(
        old.start(
            42,
            fixture.identity.channel,
            || -> std::future::Ready<()> { panic!("replaced token") }
        )
        .await
        .is_none()
    );
    let old = fixture.approval().await;
    fixture.incarnation.invalidate();
    let restarted = Arc::new(Incarnation::default());
    assert!(
        restarted.approve(fixture.read().await).is_none(),
        "restart starts without authority"
    );
    assert!(restarted.replace(fixture.identity.clone()));
    assert!(
        old.start(
            42,
            fixture.identity.channel,
            || -> std::future::Ready<()> { panic!("old process token") }
        )
        .await
        .is_none()
    );
    let fresh = restarted.approve(fixture.read().await).unwrap();
    assert_eq!(
        fresh
            .start(42, fixture.identity.channel, || std::future::ready(7))
            .await
            .unwrap()
            .finish()
            .await,
        7
    );
}

#[tokio::test(flavor = "current_thread")]
async fn later_idle_backlog_or_host_failure_revokes_an_earlier_busy_stamp() {
    for which in 0..3 {
        let fixture = Fixture::new();
        let old = fixture.approval().await;
        let _dead;
        match which {
            0 => fixture.append("{\"type\":\"system\",\"subtype\":\"turn_duration\"}"),
            1 => fixture.append(&serde_json::json!({"type":"summary", "summary":"x".repeat(4 * 1024 * 1024 + 1024)}).to_string()),
            _ => { _dead = InjectedLivenessGuard::set(HostSessionRef::tmux(&fixture.identity.session), HostLiveness::ProbeError); }
        }
        let later = activity::presence_reading_now(
            &fixture.shared,
            &ProviderKind::Claude,
            poise::serenity_prelude::ChannelId::new(fixture.identity.channel),
        )
        .await;
        assert_eq!(
            later.observed.activity,
            if which == 0 {
                Activity::Idle
            } else {
                Activity::Unknown
            }
        );
        assert!(fixture.incarnation.approve(later).is_none());
        assert!(
            old.start(
                42,
                fixture.identity.channel,
                || -> std::future::Ready<()> { panic!("superseded Busy stamp") }
            )
            .await
            .is_none()
        );
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn home_epoch_loss_expiry_and_replacement_never_fall_back_to_gateway() {
    let fixture = Fixture::new();
    let gateway = ownership::gate("claude");
    gateway.acquired();
    let old = fixture.approval().await;
    fixture
        .home
        .confirm(
            &HeldHome::for_test(
                &fixture.identity.channel.to_string(),
                "b1-worker",
                8,
                HomeState::Worker,
            ),
            tokio::time::Instant::now(),
        )
        .unwrap();
    assert!(
        old.start(
            42,
            fixture.identity.channel,
            || -> std::future::Ready<()> { panic!("old owner epoch") }
        )
        .await
        .is_none()
    );
    assert!(fixture.incarnation.replace(fixture.identity.clone()));
    let old = fixture.approval().await;
    tokio::time::advance(channel_home::HOLD_FOR).await;
    assert!(
        old.start(
            42,
            fixture.identity.channel,
            || -> std::future::Ready<()> { panic!("expired home fallback") }
        )
        .await
        .is_none()
    );
    assert!(
        !fixture.incarnation.replace(fixture.identity.clone()),
        "registered lost home owns the refusal"
    );
    let replacement = Arc::new(HomeGate::new(
        &fixture.identity.channel.to_string(),
        "b1-worker",
    ));
    replacement
        .confirm(
            &HeldHome::for_test(
                &fixture.identity.channel.to_string(),
                "b1-worker",
                9,
                HomeState::Worker,
            ),
            tokio::time::Instant::now(),
        )
        .unwrap();
    channel_home::register(replacement.clone());
    assert!(fixture.incarnation.replace(fixture.identity.clone()));
    let old = fixture.approval().await;
    channel_home::unregister_if_same(&replacement);
    assert!(
        old.start(
            42,
            fixture.identity.channel,
            || -> std::future::Ready<()> { panic!("old home became gateway") }
        )
        .await
        .is_none()
    );
    gateway.lost();
}

#[tokio::test(flavor = "current_thread")]
async fn gateway_epoch_unknown_and_loss_refuse_previously_approved_requests() {
    let fixture = Fixture::new();
    channel_home::unregister_if_same(&fixture.home);
    let gateway = ownership::gate("claude");
    for change in 0..3 {
        gateway.acquired();
        assert!(fixture.incarnation.replace(fixture.identity.clone()));
        let old = fixture.approval().await;
        match change {
            0 => {
                gateway.acquired();
            }
            1 => gateway.uncertain(),
            _ => gateway.lost(),
        }
        assert!(
            old.start(
                42,
                fixture.identity.channel,
                || -> std::future::Ready<()> { panic!("stale gateway authority") }
            )
            .await
            .is_none()
        );
    }
    gateway.lost();
}

#[tokio::test(flavor = "current_thread")]
async fn a_different_registered_home_cannot_use_an_old_gate_with_the_same_epoch() {
    let fixture = Fixture::new();
    // Model a replaced home before withdrawal has closed its old inner gate.
    let old_home = Arc::new(HomeGate::new(
        &fixture.identity.channel.to_string(),
        "b1-old",
    ));
    old_home
        .confirm(
            &HeldHome::for_test(
                &fixture.identity.channel.to_string(),
                "b1-old",
                7,
                HomeState::Worker,
            ),
            tokio::time::Instant::now(),
        )
        .unwrap();
    fixture
        .incarnation
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .unwrap()
        .owner = Owner::Home(old_home, 1);
    let old = fixture.approval().await;
    assert!(
        old.start(
            42,
            fixture.identity.channel,
            || -> std::future::Ready<()> { panic!("foreign registered home") }
        )
        .await
        .is_none()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn an_external_wake_target_requires_fresh_receiver_authority() {
    let fixture = Fixture::new();
    let (bot, actual_channel, _message_id) = (42, fixture.identity.channel, 123_u64);
    let fresh = fixture.approval().await;
    assert_eq!(
        fresh
            .start(bot, actual_channel, || std::future::ready("relight"))
            .await
            .unwrap()
            .finish()
            .await,
        "relight"
    );
    fixture.append("{\"type\":\"system\",\"subtype\":\"turn_duration\"}");
    assert!(
        fixture.incarnation.approve(fixture.read().await).is_none(),
        "a wake cannot restore Busy"
    );
}
