use crate::services::discord::turn_presence::activity::Reading;
use crate::services::discord::turn_presence::supervisor::tests::{
    BUSY, Fake, IDLE, UNKNOWN, supervise, tick_settled, typing_counts, wait,
};
use crate::services::discord::turn_presence::supervisor::{
    Action, NOTICE_EVERY, Presence, REGISTERED, Started, Supervisor, TICK, TYPING_EVERY, apply,
    lock, observes, register, registered, status, typed, unknown_snapshots,
};
#[cfg(unix)]
use crate::services::discord::turn_presence::supervisor::{Effects, LiveEffects};
use crate::services::provider::ProviderKind;
use crate::services::tui_o::turn_mode::TestConfirmation;
use poise::serenity_prelude::ChannelId;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::Instant;

/// Busy types at once, refreshes at each deadline, and stops when the channel reads idle.
#[tokio::test(start_paused = true)]
async fn busy_types_at_once_then_at_refresh_interval_and_idle_stops_it() {
    let channel = 6_200_001;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], BUSY);
    let run = supervise(&fake);
    wait(TYPING_EVERY.as_secs() * 4 + 1).await;
    let expected = (0..=4)
        .map(|n| TYPING_EVERY.as_secs() * n)
        .collect::<Vec<_>>();
    assert_eq!(fake.typed_secs(channel), expected);
    fake.set(channel, IDLE);
    wait(TICK.as_secs()).await;
    assert_eq!(status(channel).unwrap()["activity"], "idle");
    wait(40).await;
    run.abort();
    assert_eq!(fake.typed_secs(channel), expected);
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
        let actions = apply(
            &mut presence,
            channel,
            now,
            &read(observed),
            (false, None, false),
            None,
        );
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
    wait(TYPING_EVERY.as_secs() * 5 + 1).await;
    assert!(fake.typed_secs(channel).is_empty());
    fake.token.store(false, Ordering::SeqCst);
    wait(TYPING_EVERY.as_secs()).await;
    run.abort();
    assert_eq!(fake.typed_secs(channel), [TYPING_EVERY.as_secs() * 6]);
}

/// Waiting input sees one notice after a minute; recovery starts a fresh grace and a
/// continuous cause then remains suppressed.
#[tokio::test(start_paused = true)]
async fn unknown_for_a_minute_notices_once_per_cause_spell() {
    let channel = 6_200_004;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], UNKNOWN);
    *lock(&fake.head) = Some(7);
    let run = supervise(&fake);
    wait(59).await;
    assert_eq!(fake.notices(), []);
    wait(2).await;
    fake.set(channel, IDLE);
    wait(10).await;
    fake.set(channel, UNKNOWN);
    wait(20 * 60).await;
    assert_eq!(fake.notice_secs(), [60, 132]);
    wait(10 * 60).await;
    run.abort();
    assert_eq!(fake.notice_secs(), [60, 132]);
    assert!(fake.notices()[0].1.contains(r"(사유: facts\_halted)"));
    assert!(status(channel).unwrap()["unknown_notice"]["posted_at"].is_string());
}

/// An unaffected Unknown posts nothing yet still shows in health; waiting input gets the overdue
/// notice at once, and the same cause stays at one notice while the input leaves and returns.
#[tokio::test(start_paused = true)]
async fn unknown_without_input_notices_only_when_a_waiting_head_is_affected() {
    let channel = 6_200_031;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], UNKNOWN);
    let mut supervisor = Supervisor::new(fake.clone());
    let presence = supervisor.presence(channel);
    tick_settled(&supervisor, channel).await;
    tokio::time::advance(NOTICE_EVERY + Duration::from_secs(1)).await;
    tick_settled(&supervisor, channel).await;
    assert!(fake.notices().is_empty());
    assert!(lock(&presence).last_notice_at.is_none());
    assert!(status(channel).unwrap()["unknown_notice"]["posted_at"].is_null());
    assert!(
        unknown_snapshots()
            .iter()
            .any(|snapshot| snapshot.channel_id == channel),
        "health reports a held Unknown whether or not input waits"
    );

    *lock(&fake.head) = Some(7);
    tick_settled(&supervisor, channel).await;
    assert_eq!(fake.notice_secs(), [1801]);
    tokio::time::advance(NOTICE_EVERY).await;
    tick_settled(&supervisor, channel).await;
    assert_eq!(fake.notice_secs(), [1801]);

    let last_notice = lock(&presence).last_notice_at;
    let posted = status(channel).unwrap()["unknown_notice"]["posted_at"].clone();
    *lock(&fake.head) = None;
    tokio::time::advance(NOTICE_EVERY).await;
    tick_settled(&supervisor, channel).await;
    *lock(&fake.head) = Some(8);
    tick_settled(&supervisor, channel).await;
    assert_eq!(fake.notice_secs(), [1801], "the same cause is noticed once");
    assert_eq!(lock(&presence).last_notice_at, last_notice);
    assert_eq!(
        status(channel).unwrap()["unknown_notice"]["posted_at"],
        posted
    );
    assert!(fake.kick_secs().is_empty(), "Unknown never wakes the queue");
}

/// A token alone is affected input: one notice after the grace, none again for the same cause,
/// and none once the token is released.
#[tokio::test(start_paused = true)]
async fn unknown_with_only_a_token_notices_until_the_token_is_released() {
    let channel = 6_200_032;
    let _confirmed = TestConfirmation::new(channel);
    let fake = Fake::new(&[channel], UNKNOWN);
    fake.token.store(true, Ordering::SeqCst);
    let mut supervisor = Supervisor::new(fake.clone());
    supervisor.presence(channel);
    tick_settled(&supervisor, channel).await;
    tokio::time::advance(Duration::from_secs(59)).await;
    tick_settled(&supervisor, channel).await;
    assert!(fake.notices().is_empty());
    tokio::time::advance(Duration::from_secs(1)).await;
    tick_settled(&supervisor, channel).await;
    assert_eq!(fake.notice_secs(), [60]);
    tokio::time::advance(NOTICE_EVERY).await;
    tick_settled(&supervisor, channel).await;
    assert_eq!(fake.notice_secs(), [60]);

    fake.token.store(false, Ordering::SeqCst);
    tokio::time::advance(NOTICE_EVERY).await;
    tick_settled(&supervisor, channel).await;
    assert_eq!(fake.notice_secs(), [60]);
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
        assert!(
            notices[0].1.starts_with("-# ⚠️ ")
                && crate::services::discord::session_notice::is_inline(&notices[0].1),
            "the held notice must reach the provider effect as one subtext line: {}",
            notices[0].1
        );
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

/// A front only an input owner holds is read from that owner: the idle transition wakes the owner
/// once, and an unchanged Idle at 2, 5, 30 and 60 seconds wakes neither it nor a kickoff.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn an_owned_front_wakes_its_owner_once_at_the_idle_transition_and_never_by_time() {
    use crate::services::discord::queue_io::input_owner::admission::{
        Admission, AdmissionTicket, admit,
    };
    let _root = crate::services::discord::relay_recovery::tests::isolated_agentdesk_root();
    let channel = 6_200_017;
    let _confirmed = TestConfirmation::new(channel);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let mailbox = shared.mailbox(ChannelId::new(channel));
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let file = dir.join(format!("claude/presence-owner/{channel}.json"));
    mailbox
        .v2_adopt(file, "presence-owner".into())
        .await
        .unwrap();
    let original = serde_json::from_value(serde_json::json!({"author_id":7,"message_id":62_000_171,
        "channel_id":channel,"source_message_ids":[62_000_171],"queued_generation":1,"text":"owned"}))
    .unwrap();
    let admitted = admit(&mailbox, &original, AdmissionTicket::new()).await;
    assert!(matches!(admitted, Admission::Accepted(_)));
    let fake = Fake::new(&[channel], BUSY);
    *lock(&fake.live_mailbox) = Some(shared.clone());
    let run = supervise(&fake);
    wait(10).await;
    fake.set(channel, IDLE);
    let owner_wakes = || {
        lock(&fake.owner_wakes)
            .iter()
            .map(|w| w.1.as_secs())
            .collect::<Vec<_>>()
    };
    for at in [5, 2, 30, 60] {
        wait(at).await;
        assert_eq!(owner_wakes(), [12], "{at}s more");
    }
    run.abort();
    assert_eq!(fake.kick_secs(), Vec::<u64>::new());
}

/// The live presence read of a channel without a mailbox mints none: it reports no input, or the
/// on-disk marker's input; a channel whose owner holds a front still reports that front.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_live_presence_read_never_mints_a_mailbox_for_an_absent_channel() {
    use crate::services::discord::queue_io::input_owner::admission::{AdmissionTicket, admit};
    let _root = crate::services::discord::relay_recovery::tests::isolated_agentdesk_root();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let (bare, marked, owned) = (6_200_031_u64, 6_200_032_u64, 6_200_033_u64);
    let marker = crate::services::discord::relay_recovery::tests::orphan_token_finish::queued(41);
    let save = crate::services::turn_orchestrator::save_channel_pending_dispatch_marker;
    let provider = ProviderKind::Claude;
    save(
        &provider,
        &shared.token_hash,
        ChannelId::new(marked),
        &marker,
        None,
    )
    .unwrap();
    let mailbox = shared.mailbox(ChannelId::new(owned));
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let file = dir.join(format!("claude/presence-read/{owned}.json"));
    mailbox
        .v2_adopt(file, "presence-read".into())
        .await
        .unwrap();
    let original =
        serde_json::from_value(serde_json::json!({"author_id":7,"message_id":62_000_331,
        "channel_id":owned,"source_message_ids":[62_000_331],"queued_generation":1,"text":"owned"}))
        .unwrap();
    let _ = admit(&mailbox, &original, AdmissionTicket::new()).await;
    let effects = LiveEffects {
        shared: shared.clone(),
        provider,
    };
    let mut observed = Vec::new();
    for channel in [bare, marked, owned] {
        let read = effects.mailbox(channel).await;
        let present = shared.mailbox_peek(ChannelId::new(channel)).is_some();
        observed.push((channel, read, present));
    }
    let expected = [
        (bare, (false, None, false), false),
        (marked, (false, Some(41), false), false),
        (owned, (false, Some(62_000_331), true), true),
    ];
    assert_eq!(observed, expected, "(channel, read, registry entry)");
}

/// The live effects' owner wake reaches the channel's owner drive, which grants the queued front
/// and hands it to the idle start.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_live_owner_wake_hands_the_queued_front_to_the_owner_drive() {
    use crate::services::discord::queue_io::input_owner::admission::{AdmissionTicket, admit};
    let _root = crate::services::discord::relay_recovery::tests::isolated_agentdesk_root();
    let channel = ChannelId::new(6_200_018);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let mailbox = shared.mailbox(channel);
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let file = dir.join("claude/presence-live/6200018.json");
    mailbox
        .v2_adopt(file, "presence-live".into())
        .await
        .unwrap();
    let original =
        serde_json::from_value(serde_json::json!({"author_id":7,"message_id":62_000_181,
        "channel_id":channel.get(),"source_message_ids":[62_000_181],"queued_generation":1,
        "text":"owned"}))
        .unwrap();
    let _ = admit(&mailbox, &original, AdmissionTicket::new()).await;
    let granted = || mailbox.v2_request(|slot| Ok(slot.as_ref().unwrap().front_outstanding()));
    assert!(!granted().await.unwrap(), "no grant before the wake");
    let effects = LiveEffects {
        shared: shared.clone(),
        provider: ProviderKind::Claude,
    };
    effects.wake_owner(channel.get());
    let mut handed = false;
    for _ in 0..100 {
        handed = granted().await.unwrap();
        if handed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(handed, "the owner drive granted its front");
}

/// A waiting input's notice that takes 30 seconds holds neither another channel's typing nor stop.
#[tokio::test(start_paused = true)]
async fn a_slow_notice_holds_only_its_own_channel() {
    let (unknown, busy) = (6_200_014, 6_200_015);
    let _confirmed = (TestConfirmation::new(unknown), TestConfirmation::new(busy));
    let fake = Fake::new(&[unknown, busy], BUSY);
    fake.set(unknown, UNKNOWN);
    *lock(&fake.head) = Some(7);
    *lock(&fake.notice_delay) = Some(Duration::from_secs(30));
    let run = supervise(&fake);
    wait(70).await;
    fake.set(busy, IDLE);
    wait(30).await;
    run.abort();
    assert_eq!(fake.notice_secs(), [60]);
    assert_eq!(
        fake.typed_secs(busy),
        (0..70)
            .step_by(TYPING_EVERY.as_secs() as usize)
            .collect::<Vec<_>>()
    );
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
    assert_eq!(
        fake.typed_secs(fast),
        (0..20)
            .step_by(TYPING_EVERY.as_secs() as usize)
            .collect::<Vec<_>>()
    );
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

#[test]
fn injection_hourglass_requires_registration_confirmation_and_a_running_supervisor() {
    let provider = ProviderKind::Unsupported("injection-observation-contract".into());
    let channel = 7_269_005;
    let confirmed = TestConfirmation::new(channel);
    let started = Started::new(&provider);
    assert!(
        !observes(&provider, channel),
        "confirmation alone must not register a channel"
    );
    register(&provider, &[channel]);
    assert!(observes(&provider, channel));
    drop(confirmed);
    assert!(
        !observes(&provider, channel),
        "registration alone must not confirm turn mode"
    );
    let _confirmed = TestConfirmation::new(channel);
    drop(started);
    assert!(
        !observes(&provider, channel),
        "a stopped supervisor cannot finish hourglasses"
    );
    lock(&REGISTERED).remove(provider.as_str());
}
