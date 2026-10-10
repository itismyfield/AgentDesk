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
    runtime: Arc<crate::services::discord::PresenceRuntime>,
    _lifetime: super::super::lifecycle::Lifetime,
    incarnation: Arc<Incarnation>,
}

impl Fixture {
    fn new() -> Self {
        let runtime_root = crate::config::TestRuntimeRootGuard::new();
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
        // The real commit path loads a healthy writer; a seq read back without one admits nothing.
        crate::services::tui_prompt_dedupe::binding_events::pinned_source(channel, &session)
            .unwrap();
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
        let runtime = Arc::new(crate::services::discord::PresenceRuntime::default());
        let lifetime = runtime.own();
        let ticket = runtime.register(channel).unwrap();
        let incarnation = ticket.incarnation().unwrap();
        assert!(incarnation.replace(&ticket, identity.clone()));
        Self {
            _runtime: runtime_root,
            root,
            _binding: binding,
            shared,
            confirmed: Some(confirmed),
            _host: host,
            identity,
            home,
            runtime,
            _lifetime: lifetime,
            incarnation,
        }
    }

    fn ticket(&self) -> Ticket {
        self.runtime.register(self.identity.channel).unwrap()
    }

    async fn read(&self) -> Reading {
        let ticket = self.ticket();
        for _ in 0..500 {
            let reading = activity::presence_reading_now(
                &self.shared,
                &ProviderKind::Claude,
                poise::serenity_prelude::ChannelId::new(self.identity.channel),
                &ticket,
            )
            .await
            .unwrap();
            if reading.observed.reason != "catching_up" {
                return reading;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("presence reader never settled");
    }

    async fn approval(&self) -> Approval {
        let reading = self.read().await;
        assert!(reading.identity(0).is_none());
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
        let runtime = Arc::new(crate::services::discord::PresenceRuntime::default());
        let _lifetime = runtime.own();
        let ticket = runtime.register(foreign.channel).unwrap();
        let incarnation = ticket.incarnation().unwrap();
        let channel = foreign.channel;
        assert!(incarnation.replace(&ticket, foreign));
        // Observed under the foreign ticket, so only the stamp mismatch can refuse it.
        let mut reading = fixture.read().await;
        reading.rebind(runtime.register(channel).unwrap());
        assert!(incarnation.approve(reading).is_none(), "field {which}");
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
        let runtime = Arc::new(crate::services::discord::PresenceRuntime::default());
        let _lifetime = runtime.own();
        if identity.channel == 0 {
            assert!(runtime.register(0).is_none());
        } else {
            let ticket = runtime.register(identity.channel).unwrap();
            let incarnation = ticket.incarnation().unwrap();
            assert!(!incarnation.replace(&ticket, identity));
            let mut reading = fixture.read().await;
            reading.rebind(runtime.register(fixture.identity.channel).unwrap());
            assert!(incarnation.approve(reading).is_none());
        }
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
    let mut changed = fixture.identity.clone();
    changed.source.ino += 1;
    assert!(fixture.incarnation.replace(&fixture.ticket(), changed));
    assert!(
        old.start(
            42,
            fixture.identity.channel,
            || -> std::future::Ready<()> { panic!("replaced token") }
        )
        .await
        .is_none()
    );
    assert!(
        fixture
            .incarnation
            .replace(&fixture.ticket(), fixture.identity.clone())
    );
    let old = fixture.approval().await;
    fixture.incarnation.invalidate();
    let runtime = Arc::new(crate::services::discord::PresenceRuntime::default());
    let _lifetime = runtime.own();
    let ticket = runtime.register(fixture.identity.channel).unwrap();
    let restarted = ticket.incarnation().unwrap();
    let mut reading = fixture.read().await;
    reading.rebind(runtime.register(fixture.identity.channel).unwrap());
    assert!(
        restarted.approve(reading).is_none(),
        "restart starts without authority"
    );
    assert!(restarted.replace(&ticket, fixture.identity.clone()));
    assert!(
        old.start(
            42,
            fixture.identity.channel,
            || -> std::future::Ready<()> { panic!("old process token") }
        )
        .await
        .is_none()
    );
    let mut reading = fixture.read().await;
    reading.rebind(runtime.register(fixture.identity.channel).unwrap());
    let fresh = restarted.approve(reading).unwrap();
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
    for which in 0..5 {
        let fixture = Fixture::new();
        let old = fixture.approval().await;
        let _dead;
        match which {
            0 => fixture.append("{\"type\":\"system\",\"subtype\":\"turn_duration\"}"),
            1 => fixture.append(&serde_json::json!({"type":"summary", "summary":"x".repeat(4 * 1024 * 1024 + 1024)}).to_string()),
            2 => { _dead = InjectedLivenessGuard::set(HostSessionRef::tmux(&fixture.identity.session), HostLiveness::ProbeError); }
            3 => { fixture.shared.tmux_watchers.remove(&poise::serenity_prelude::ChannelId::new(fixture.identity.channel)).unwrap(); }
            _ => {}
        }
        let later = activity::presence_reading_now(
            &fixture.shared,
            if which == 4 {
                &ProviderKind::Gemini
            } else {
                &ProviderKind::Claude
            },
            poise::serenity_prelude::ChannelId::new(fixture.identity.channel),
            &fixture.ticket(),
        )
        .await
        .unwrap();
        assert_eq!(
            later.observed.activity,
            if which == 0 || which == 3 {
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
                || -> std::future::Ready<()> { panic!("superseded Busy stamp ({which})") }
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
    assert!(
        fixture
            .incarnation
            .replace(&fixture.ticket(), fixture.identity.clone())
    );
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
        !fixture
            .incarnation
            .replace(&fixture.ticket(), fixture.identity.clone()),
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
    assert!(
        fixture
            .incarnation
            .replace(&fixture.ticket(), fixture.identity.clone())
    );
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
        assert!(
            fixture
                .incarnation
                .replace(&fixture.ticket(), fixture.identity.clone())
        );
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

#[tokio::test(flavor = "current_thread")]
async fn b2_unchanged_identity_and_owner_keep_the_ticket_and_approval() {
    let fixture = Fixture::new();
    let approved = fixture.approval().await;
    let ticket = fixture.ticket();
    assert!(
        fixture
            .incarnation
            .replace(&ticket, fixture.identity.clone())
    );
    assert!(ticket.incarnation().is_some());
    assert_eq!(
        approved
            .start(42, fixture.identity.channel, || std::future::ready(1))
            .await
            .unwrap()
            .finish()
            .await,
        1
    );
}

#[tokio::test(flavor = "current_thread")]
async fn b2_reading_started_before_invalidation_cannot_reseed_after_await() {
    let fixture = Fixture::new();
    let ticket = fixture.ticket();
    let reading = fixture.read().await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let delayed = async {
        rx.await.unwrap();
        let identity = reading.identity(42).unwrap();
        fixture.incarnation.replace(&ticket, identity)
    };
    assert!(
        fixture
            .runtime
            .invalidate_if_current(&ticket, "await boundary")
    );
    tx.send(()).unwrap();
    assert!(!delayed.await);
    let current = fixture.ticket();
    let successor = current.incarnation().unwrap();
    assert!(successor.approve(fixture.read().await).is_none());
    let old_observer = activity::presence_reading_now(
        &fixture.shared,
        &ProviderKind::Claude,
        poise::serenity_prelude::ChannelId::new(fixture.identity.channel),
        &ticket,
    )
    .await;
    assert!(old_observer.is_none());
    assert!(successor.replace(&current, fixture.read().await.identity(42).unwrap()));
    assert!(successor.approve(fixture.read().await).is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn b2_strict_observer_rechecks_ticket_after_a_blocked_read() {
    let fixture = Fixture::new();
    let ticket = fixture.ticket();
    let channel = poise::serenity_prelude::ChannelId::new(fixture.identity.channel);
    fixture.shared.tmux_watchers.remove(&channel).unwrap();
    let core = fixture.shared.core.lock().await;
    let mut reading = Box::pin(activity::presence_reading_now(
        &fixture.shared,
        &ProviderKind::Claude,
        channel,
        &ticket,
    ));
    assert!(poll_fn(|cx| Poll::Ready(reading.as_mut().poll(cx).is_pending())).await);
    assert!(
        fixture
            .runtime
            .invalidate_if_current(&ticket, "blocked read")
    );
    drop(core);
    assert!(reading.await.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn b2_identity_switch_makes_late_invalidate_harmless() {
    let fixture = Fixture::new();
    let old = fixture.ticket();
    let mut successor = fixture.identity.clone();
    successor.source.ino += 1;
    assert!(fixture.incarnation.replace(&old, successor.clone()));
    assert!(!fixture.runtime.invalidate_if_current(&old, "late identity"));
    let current = fixture.ticket();
    assert!(current.incarnation().is_some());
    assert_eq!(
        fixture
            .incarnation
            .0
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .identity,
        successor
    );
}

#[tokio::test(flavor = "current_thread")]
async fn b2_suspend_blocks_first_poll_and_resume_requires_new_reading_authority() {
    let fixture = Fixture::new();
    let old = fixture.approval().await;
    let ticket = fixture.ticket();
    fixture.runtime.suspend_runtime();
    let polls = AtomicUsize::new(0);
    assert!(
        old.start(42, fixture.identity.channel, || {
            polls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(())
        })
        .await
        .is_none()
    );
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert!(
        !fixture
            .incarnation
            .replace(&ticket, fixture.identity.clone())
    );
    fixture.runtime.resume_fresh();
    let fresh = fixture.ticket();
    let incarnation = fresh.incarnation().unwrap();
    assert!(incarnation.approve(fixture.read().await).is_none());
    assert!(incarnation.replace(&fresh, fixture.read().await.identity(42).unwrap()));
    assert!(incarnation.approve(fixture.read().await).is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn b2_committed_seq_revokes_old_approval_without_a_watch_poll_or_wake() {
    use crate::services::tui_prompt_dedupe::binding_events as p5;
    let fixture = Fixture::new();
    let old = fixture.approval().await;
    let before = std::fs::read(
        fixture
            .root
            .path()
            .join(p5::BINDING_EVENTS_DIR)
            .join(format!("{}.log", fixture.identity.channel)),
    )
    .unwrap();
    let candidate = fixture.root.path().join("next.jsonl");
    let path = candidate.to_str().unwrap();
    let proposal = p5::Proposal {
        channel_id: fixture.identity.channel,
        provider: "claude",
        tmux_session: &fixture.identity.session,
        session_id: Some("next"),
        path,
        replaced: None,
        cause: p5::CauseSource::Hook(p5::BindingCause::Startup),
        hook: None,
    };
    assert_eq!(
        p5::record_pending(&proposal).unwrap(),
        p5::PendingRecord::Recorded
    );
    let after = std::fs::read(
        fixture
            .root
            .path()
            .join(p5::BINDING_EVENTS_DIR)
            .join(format!("{}.log", fixture.identity.channel)),
    )
    .unwrap();
    assert!(after.starts_with(&before));
    let polls = AtomicUsize::new(0);
    let result = old
        .start(42, fixture.identity.channel, || {
            polls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(())
        })
        .await;
    assert!(result.is_none());
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::read(
            fixture
                .root
                .path()
                .join(p5::BINDING_EVENTS_DIR)
                .join(format!("{}.log", fixture.identity.channel))
        )
        .unwrap(),
        after
    );
}

#[tokio::test(flavor = "current_thread")]
async fn b2b1_reading_kept_across_resume_cannot_seed_or_approve_the_new_ticket() {
    let fixture = Fixture::new();
    let channel = fixture.identity.channel;
    let mut kept = fixture.read().await;
    assert_eq!(kept.observed.activity, Activity::Busy);
    fixture.runtime.suspend_runtime();
    fixture.runtime.resume_fresh();
    let ticket = fixture.ticket();
    let incarnation = ticket.incarnation().unwrap();
    assert!(
        !incarnation.adopt_reading(&mut kept, 42),
        "a reading from before the resume"
    );
    assert!(
        incarnation.0.lock().unwrap().is_none(),
        "nothing was seeded"
    );
    let mut fresh = fixture.read().await;
    assert!(incarnation.adopt_reading(&mut fresh, 42));
    let polls = AtomicUsize::new(0);
    let started = incarnation
        .approve(fresh)
        .expect("a fresh reading carries the adopted ticket")
        .start(42, channel, || {
            polls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(())
        })
        .await;
    assert!(started.is_some());
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    // Seated with the same identity, the kept reading still names the ticket it was read under.
    assert!(incarnation.replace(&fixture.ticket(), kept.identity(42).unwrap()));
    assert!(incarnation.approve(kept).is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn b2b1_watcher_unpair_waits_for_a_first_poll_and_then_withdraws_its_approvals() {
    let fixture = Fixture::new();
    let fence =
        Arc::downgrade(&fixture.runtime) as std::sync::Weak<dyn super::super::entrypoints::Fence>;
    let _installed = super::super::entrypoints::tests::install(&fixture.shared, fence);
    let channel = poise::serenity_prelude::ChannelId::new(fixture.identity.channel);
    let first = fixture.approval().await;
    let next = fixture.approval().await;
    let (done, removed) = std::sync::mpsc::channel();
    let shared = fixture.shared.clone();
    let mut remover = None;
    let started = first
        .start(42, channel.get(), || {
            remover = Some(std::thread::spawn(move || {
                done.send(shared.tmux_watchers.remove(&channel).is_some())
                    .unwrap();
            }));
            let waited = removed.recv_timeout(std::time::Duration::from_millis(300));
            assert!(waited.is_err(), "the unpair ran inside a first poll");
            let paired = fixture.shared.tmux_watchers.channel_binding(&channel);
            assert!(
                paired.is_some(),
                "the watcher changed between the poll's checks"
            );
            std::future::ready(())
        })
        .await;
    assert!(started.is_some());
    remover.unwrap().join().unwrap();
    assert!(removed.recv().unwrap(), "the watcher was unpaired");
    let polls = AtomicUsize::new(0);
    let attempt = next.start(42, channel.get(), || {
        polls.fetch_add(1, Ordering::SeqCst);
        std::future::ready(())
    });
    assert!(attempt.await.is_none());
    assert_eq!(polls.load(Ordering::SeqCst), 0);
}
