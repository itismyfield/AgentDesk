//! A restart-cut episode's notice posts at most once across repeat records, lost ACKs,
//! history misses and reboots; only a decisive answer closes it as delivered.

use super::*;
use crate::services::tui_o::store::restart_notice::{self, Intent};
use crate::services::tui_o::writer::restart_notice::{Notice, TEXT, unit_key};

fn intent() -> Intent {
    Intent::exact(CHANNEL, ShadowProvider::Claude, Some("cut-turn"), 4).unwrap()
}

fn fixture() -> Harness {
    let harness = Harness::new();
    harness.gate.acquired();
    harness
}

fn root(harness: &Harness) -> &std::path::Path {
    harness._runtime.path()
}

/// A second boot records the same episode again and delivers with a fresh writer.
async fn reboot(harness: &Harness) {
    restart_notice::record(root(harness), &intent()).unwrap();
    harness.writer().deliver_restart_notices().await;
}

#[tokio::test]
async fn a_lost_ack_is_confirmed_from_history_without_a_second_create() {
    let harness = fixture();
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::CreatedUnseen);
    let notice = harness.writer().post_restart_notice(&intent()).await;
    assert_eq!(notice, Notice::Delivered(1000));
    reboot(&harness).await;
    assert_eq!(harness.port.posts(), [TEXT]);
}

/// History cannot prove the created POST, so the notice stays unknown and never repeats.
#[tokio::test]
async fn an_unprovable_post_stays_unknown_across_a_reboot() {
    let harness = fixture();
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::CreatedUnseen);
    harness.port.unreadable.store(true, Ordering::SeqCst);
    let notice = harness.writer().post_restart_notice(&intent()).await;
    assert_eq!(notice, Notice::Unknown);
    harness.port.unreadable.store(false, Ordering::SeqCst);
    reboot(&harness).await;
    assert_eq!(
        harness.port.posts(),
        [TEXT],
        "one Discord message, never two"
    );
    assert!(restart_notice::pending(root(&harness), CHANNEL).is_empty());
}

/// A history miss is not a CREATE permit: the late success it missed would be doubled.
#[tokio::test]
async fn a_history_miss_never_permits_a_second_create_after_a_late_success() {
    let harness = fixture();
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::Unsent);
    let notice = harness.writer().post_restart_notice(&intent()).await;
    assert_eq!(notice, Notice::Unknown);
    harness.port.say(BOT, TEXT);
    reboot(&harness).await;
    assert_eq!(harness.port.posts(), [TEXT]);
}

/// Prepared was durable and the process ended before its POST result: the next writer only
/// settles it from history, whether or not the POST landed.
#[tokio::test]
async fn a_prepared_notice_is_settled_never_created_after_a_crash() {
    for landed in [true, false] {
        let harness = fixture();
        let (mut store, key) = (harness.channel(), unit_key(&intent()));
        let anchor_id = store.ledger().anchor();
        store
            .append_ledger(LedgerEntry::Prepared {
                serial: 0,
                unit_key: key,
                piece_index: 0,
                payload: TEXT.into(),
                anchor_id,
                epoch: 1,
                local_retry: None,
                notice_origin: None,
            })
            .unwrap();
        if landed {
            harness.port.say(BOT, TEXT);
        }
        reboot(&harness).await;
        assert!(harness.port.posts().is_empty(), "landed={landed}");
        let ledger = harness.channel();
        let (_, piece) = ledger
            .ledger()
            .latest_piece(&unit_key(&intent()), 0)
            .unwrap();
        assert_eq!(
            matches!(piece.outcome, Some(PieceOutcome::Posted(_))),
            landed
        );
        assert!(restart_notice::pending(root(&harness), CHANNEL).is_empty());
    }
}

/// Without the delivery lease nothing is prepared and the intent waits for a later poll.
#[tokio::test]
async fn a_busy_lease_keeps_the_intent_pending() {
    let harness = fixture();
    restart_notice::record(root(&harness), &intent()).unwrap();
    harness.lease.busy.store(true, Ordering::SeqCst);
    harness.writer().deliver_restart_notices().await;
    assert!(harness.port.posts().is_empty());
    assert_eq!(restart_notice::pending(root(&harness), CHANNEL), [intent()]);
    harness.lease.busy.store(false, Ordering::SeqCst);
    harness.writer().deliver_restart_notices().await;
    assert_eq!(harness.port.posts(), [TEXT]);
}

#[test]
fn an_episode_without_its_exact_identity_names_no_intent() {
    let claude = ShadowProvider::Claude;
    assert!(Intent::exact(CHANNEL, claude, None, 4).is_none());
    assert!(Intent::exact(CHANNEL, claude, Some(" "), 4).is_none());
    assert!(Intent::exact(CHANNEL, claude, Some("cut-turn"), 0).is_none());
    assert!(Intent::exact(0, claude, Some("cut-turn"), 4).is_none());
    let other = Intent::exact(CHANNEL, claude, Some("cut-turn"), 5).unwrap();
    assert_ne!(other.key(), intent().key());
}
