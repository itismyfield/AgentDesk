//! A THREAD-GUARD verdict witnesses one episode only when the mailbox anchor and nonce
//! and the stale row filed under the thread all name it.

use poise::serenity_prelude::ChannelId;

use super::thread_guard_stale_pure_tests::{inflight_with_updated_at, watcher_snapshot};
use super::*;
use crate::services::discord::InflightRestartMode;

const THREAD: u64 = 900_000_000_006_199;
const NONCE: &str = "episode-a";

/// A labelled change to an otherwise witnessed proof.
type Change = (&'static str, fn(&mut StaleActiveTurnProof));

/// A no-owner stale proof of the thread whose mailbox and row both name episode A.
fn proof() -> StaleActiveTurnProof {
    let provider = ProviderKind::Codex;
    let now = chrono::Utc::now().timestamp();
    let threshold = crate::services::discord::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64;
    let stale = chrono::Local::now() - chrono::Duration::seconds(threshold + 5);
    let stale = stale.format("%Y-%m-%d %H:%M:%S").to_string();
    let mut row = inflight_with_updated_at(&provider, THREAD, &stale);
    row.turn_nonce = Some(NONCE.to_string());
    let mut snapshot = watcher_snapshot(
        &provider,
        THREAD,
        row.user_msg_id,
        false,
        Some(false),
        false,
    );
    snapshot.mailbox_active_turn_nonce = Some(NONCE.to_string());
    bind_stale_active_turn_proof(&row, snapshot, now, std::time::Instant::now())
}

fn bound(proof: &StaleActiveTurnProof) -> bool {
    let shared = crate::services::discord::make_shared_data_for_tests();
    ThreadGuardStaleWitness::bind(&shared, ChannelId::new(THREAD), proof).is_some()
}

#[test]
fn the_mailbox_and_the_row_naming_one_episode_bind_a_witness() {
    assert!(bound(&proof()));
}

#[test]
fn an_unanchored_or_other_episode_binds_no_witness() {
    let cases: [Change; 10] = [
        ("no anchor", |p| {
            p.snapshot.mailbox_active_user_msg_id = None
        }),
        ("anchor 0", |p| {
            p.snapshot.mailbox_active_user_msg_id = Some(0)
        }),
        ("row anchor 0", |p| {
            p.inflight.user_msg_id = 0;
            p.snapshot.mailbox_active_user_msg_id = Some(0);
        }),
        ("another anchor", |p| {
            p.snapshot.mailbox_active_user_msg_id = Some(1)
        }),
        ("no mailbox nonce", |p| {
            p.snapshot.mailbox_active_turn_nonce = None
        }),
        ("empty nonce", |p| {
            p.snapshot.mailbox_active_turn_nonce = Some(String::new());
            p.inflight.turn_nonce = Some(String::new());
        }),
        ("legacy row without a nonce", |p| {
            p.inflight.turn_nonce = None
        }),
        ("another nonce", |p| {
            p.inflight.turn_nonce = Some("episode-b".into())
        }),
        ("a row naming another channel", |p| {
            p.inflight.channel_id = THREAD + 1
        }),
        ("a planned restart or rebind row", |p| {
            p.inflight.restart_mode = Some(InflightRestartMode::DrainRestart)
        }),
    ];
    for (label, change) in cases {
        let mut proof = proof();
        change(&mut proof);
        assert!(!bound(&proof), "{label}");
    }
    let mut rebind = proof();
    rebind.inflight.rebind_origin = true;
    assert!(!bound(&rebind), "rebind origin");
}
