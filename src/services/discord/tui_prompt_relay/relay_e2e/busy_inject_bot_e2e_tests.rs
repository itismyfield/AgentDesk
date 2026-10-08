//! Text the announce bot posts reaches a busy pane as a person's does, and a vetoed paste keeps the
//! author marks intake's own queue entry would carry.

use super::*;
use crate::services::discord::bot_role::UtilityBotRole;
use crate::services::discord::tui_prompt_relay::relay_e2e::ProviderStub;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock::user_message;

const ANNOUNCE_ID: u64 = 940_487_400_000_011;

/// A gated busy channel whose runtime resolves the announce bot, as agent-to-agent sends post.
async fn busy_with_announce() -> Busy {
    let mut db = None;
    let storage = async {
        let fixture = TestPostgresDb::create().await;
        let pool = fixture.connect_and_migrate().await;
        db = Some(fixture);
        Some(pool)
    };
    let h = RelayE2eHarness::start_inner(ProviderStub::Success, true, storage, true).await;
    let registry = h.health_registry.clone().expect("a health registry");
    registry
        .set_utility_bot_user_id_for_tests(UtilityBotRole::Announce, ANNOUNCE_ID)
        .await;
    let restart = &h.shared.restart;
    restart.reconcile_done.store(true, Ordering::SeqCst);
    h.answer_placeholders_immediately();
    h.cache_relay_transport();
    let rt = Runtime {
        h,
        _db: db.expect("a database under the harness lock"),
    };
    Busy {
        pane: InjectPane::new(CHANNEL_ID, "off"),
        _gate: hook::open_gate(CHANNEL_ID),
        rt,
    }
}

/// Production intake of `text` posted by the announce bot.
async fn deliver_from_announce(rt: &Runtime, message: u64, text: &str) {
    let mut arrival = user_message(message, text);
    arrival.author.id = serenity::UserId::new(ANNOUNCE_ID);
    arrival.author.name = "announce-bot".to_string();
    arrival.author.bot = true;
    rt.h.spawn_message(arrival).await.unwrap().unwrap();
}

/// The announce bot's text on a busy pane is pasted once and marked 📥, nothing queued or started.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_pane_takes_the_announce_bot_s_text_pg() {
    let busy = busy_with_announce().await;
    let rt = &busy.rt;
    let message = fresh_id();
    deliver_from_announce(rt, message, "status?").await;
    let observed = (
        busy.pane.keys(),
        rt.marks(message),
        rt.queue().await,
        rt.starts(),
        on_disk(message),
    );
    let keys = vec!["paste-buffer".to_string(), "send-keys".to_string()];
    let disk = Some("\"observed\"".to_string());
    rt.finish().await;
    assert_eq!(observed, (keys, vec!["📥".to_string()], vec![], 0, disk));
}

/// A vetoed paste queues as intake would have: the bot's entry is a bot's and drops on cancel,
/// a person's is a person's and survives it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vetoed_paste_queues_with_the_sender_s_own_marks_pg() {
    let mut observed = Vec::new();
    for bot in [true, false] {
        let busy = busy_with_announce().await;
        let rt = &busy.rt;
        rt.hold_mailbox().await;
        busy.pane.draft();
        let message = fresh_id();
        if bot {
            deliver_from_announce(rt, message, "status?").await;
        } else {
            rt.h.deliver_user_message(message, "status?").await.unwrap();
        }
        let outcomes = hook::seen(message).outcomes.into_iter();
        let handed: Vec<bool> = outcomes.map(|o| o.starts_with("HandedBack")).collect();
        let channel = ChannelId::new(CHANNEL_ID);
        let snapshot = crate::services::discord::mailbox_snapshot(&rt.h.shared, channel).await;
        let entries = snapshot.intervention_queue.iter().map(|entry| {
            let sources = entry.source_message_queued_generations.iter();
            let preserve: Vec<bool> = sources.map(|source| source.preserve_on_cancel).collect();
            (
                entry.message_id.get() == message,
                entry.author_is_bot,
                preserve,
            )
        });
        observed.push((handed, entries.collect::<Vec<_>>()));
        rt.finish().await;
    }
    let bot = (vec![true], vec![(true, true, vec![false])]);
    let person = (vec![true], vec![(true, false, vec![true])]);
    assert_eq!(observed, [bot, person]);
}
