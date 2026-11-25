//! The sweeper never edits or ends a row the AGY owner holds (#8274 4a-1).

use std::num::NonZeroU64;

use uuid::Uuid;

use super::*;
use crate::services::discord::health::test_support::{MockDiscord, age_file, fingerprint};
use crate::services::discord::inflight::{
    AgyCustodyField, AgyCustodyRoute, AgyProducer, inflight_state_path, save_inflight_state,
};
use crate::services::tui_o::store::agy_delivery::AgyOriginal;

const MESSAGE: u64 = 1_450_000_000_000_000_101;

fn row(channel: u64, routed: bool) -> InflightTurnState {
    let mut row = InflightTurnState::new(
        ProviderKind::Antigravity,
        channel,
        None,
        1,
        MESSAGE,
        channel + 1,
        "sweeper custody fixture".into(),
        None,
        None,
        None,
        None,
        0,
    );
    let nonce = Uuid::new_v4();
    row.turn_nonce = Some(nonce.to_string());
    row.logical_channel_id = Some(channel);
    if routed {
        row.agy_custody = Some(AgyCustodyField::Route(AgyCustodyRoute {
            v: 1,
            producer: AgyProducer::Print,
            destination: channel,
            logical_channel: channel,
            message: MESSAGE,
            nonce,
            writer_generation: NonZeroU64::MIN,
            original: AgyOriginal::Discord { message: MESSAGE },
        }));
    }
    row
}

fn seed(row: &InflightTurnState, age: i64) -> std::path::PathBuf {
    save_inflight_state(row).unwrap();
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let path = inflight_state_path(&root, &ProviderKind::Antigravity, row.channel_id);
    age_file(&path, age);
    path
}

// B5
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sweeper_skips_route_row_past_thirty_minutes() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let discord = MockDiscord::start().await;
    let routed = 6_325_620_001;
    let path = seed(&row(routed, true), 31 * 60);
    let before = fingerprint(&path);
    let legacy = 6_325_620_002;
    let legacy_path = seed(&row(legacy, false), 31 * 60);
    let legacy_before = fingerprint(&legacy_path);

    let mut tracker = StalledEditTracker::default();
    run_placeholder_sweep_pass(
        &discord.http,
        &shared,
        &ProviderKind::Antigravity,
        &mut tracker,
    )
    .await;

    assert!(
        discord.calls_for(routed).is_empty(),
        "no edit of a route row"
    );
    assert!(path.exists(), "the AGY owner ends a route row");
    assert_eq!(fingerprint(&path), before);
    // The Legacy twin of the same age is still handled by the sweeper.
    assert!(
        !discord.calls_for(legacy).is_empty()
            || !legacy_path.exists()
            || fingerprint(&legacy_path) != legacy_before
    );
}
