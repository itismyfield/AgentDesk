//! A durable custody route keeps an AGY row out of every generic end.

use std::fs;
use std::num::NonZeroU64;

use uuid::Uuid;

use super::*;
use crate::services::discord::health::test_support::age_file;
use crate::services::discord::inflight::{
    AgyCustodyField, AgyCustodyRoute, AgyProducer, InflightTurnState, TerminalReceiptStamp,
    inflight_state_path, load_channel_inflight_for_probe,
};
use crate::services::provider::ProviderKind;
use crate::services::tui_o::store::agy_delivery::AgyOriginal;

const MESSAGE: u64 = 1_450_000_000_000_000_001;

fn route(channel: u64, nonce: Uuid) -> AgyCustodyRoute {
    AgyCustodyRoute {
        v: 1,
        producer: AgyProducer::Print,
        destination: channel,
        logical_channel: channel,
        message: MESSAGE,
        nonce,
        writer_generation: NonZeroU64::MIN,
        original: AgyOriginal::Discord { message: MESSAGE },
    }
}

/// An AGY foreground row; `routed` adds the custody field and nothing else.
fn row(channel: u64, routed: bool) -> InflightTurnState {
    let mut row = InflightTurnState::new(
        ProviderKind::Antigravity,
        channel,
        None,
        7,
        MESSAGE,
        channel + 1,
        "custody fixture".into(),
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
        row.agy_custody = Some(AgyCustodyField::Route(route(channel, nonce)));
    }
    row
}

/// Writes the row as the store would and returns its path.
fn write(root: &std::path::Path, row: &InflightTurnState) -> std::path::PathBuf {
    let path = inflight_state_path(root, &ProviderKind::Antigravity, row.channel_id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, serde_json::to_string_pretty(row).unwrap()).unwrap();
    path
}

fn inflight_root() -> std::path::PathBuf {
    crate::services::discord::runtime_store::discord_inflight_root().unwrap()
}

// A1, A2
#[test]
fn route_tells_agy_admission_from_its_legacy_twin() {
    let legacy = row(6_325_610_001, false);
    let mut routed = legacy.clone();
    let nonce = Uuid::parse_str(routed.turn_nonce.as_deref().unwrap()).unwrap();
    routed.agy_custody = Some(AgyCustodyField::Route(route(routed.channel_id, nonce)));
    assert_eq!(end_authority(&legacy), EndAuthority::Legacy);
    assert_eq!(end_authority(&routed), EndAuthority::Agy);
    assert!(generic_may_end(&legacy));
    assert!(!generic_may_end(&routed));
}

// A6
#[test]
fn route_that_differs_from_the_row_is_held() {
    let mut other_nonce = row(6_325_610_002, true);
    other_nonce.turn_nonce = Some(Uuid::new_v4().to_string());
    assert_eq!(end_authority(&other_nonce), EndAuthority::Held);

    let mut other_message = row(6_325_610_003, true);
    other_message.user_msg_id = MESSAGE + 1;
    assert_eq!(end_authority(&other_message), EndAuthority::Held);

    let mut other_parent = row(6_325_610_004, true);
    other_parent.logical_channel_id = Some(6_325_610_099);
    assert_eq!(end_authority(&other_parent), EndAuthority::Held);
}

// A8
#[test]
fn route_none_keeps_row_bytes_and_unknown_route_is_held() {
    let legacy = row(6_325_610_005, false);
    let bytes = serde_json::to_string(&legacy).unwrap();
    assert!(!bytes.contains("agy_custody"));

    let mut nulled: serde_json::Value = serde_json::from_str(&bytes).unwrap();
    nulled["agy_custody"] = serde_json::Value::Null;
    let parsed: InflightTurnState = serde_json::from_value(nulled).unwrap();
    assert!(parsed.agy_custody.is_none());
    assert_eq!(serde_json::to_string(&parsed).unwrap(), bytes);

    let routed = row(6_325_610_006, true);
    for unknown in [
        serde_json::json!({"v": 2, "producer": "print"}),
        serde_json::json!("not a route"),
    ] {
        let mut value = serde_json::to_value(&routed).unwrap();
        value["agy_custody"] = unknown.clone();
        let parsed: InflightTurnState = serde_json::from_value(value).unwrap();
        assert!(matches!(
            parsed.agy_custody,
            Some(AgyCustodyField::Unreadable(_))
        ));
        assert_eq!(end_authority(&parsed), EndAuthority::Held);
        let kept = serde_json::to_value(&parsed).unwrap();
        assert_eq!(kept["agy_custody"], unknown);
    }
}

// B1 keep_owed_row_marked: shared by generation invalidation and the reaper's stale verdict.
#[test]
fn stale_end_keeps_route_row_unmarked() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = inflight_root();
    let mut routed = row(6_325_610_007, true);
    routed.input_origin_ids = vec!["origin".into()];
    let path = write(&root, &routed);
    let kept = super::terminal_receipt_stamp::keep_owed_row_marked(&path, &routed).unwrap();
    assert!(kept.terminal_receipt.is_none());
    let on_disk: InflightTurnState =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert!(on_disk.terminal_receipt.is_none());

    let mut legacy = row(6_325_610_008, false);
    legacy.input_origin_ids = vec!["origin".into()];
    let legacy_path = write(&root, &legacy);
    let marked =
        super::terminal_receipt_stamp::keep_owed_row_marked(&legacy_path, &legacy).unwrap();
    assert!(marked.terminal_receipt.is_some());
}

// B1 generation invalidation (boot step 4)
#[test]
fn generation_invalidation_keeps_route_row() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = inflight_root();
    let mut routed = row(6_325_610_009, true);
    routed.restart_generation = Some(1);
    let routed_path = write(&root, &routed);
    let mut legacy = row(6_325_610_010, false);
    legacy.restart_generation = Some(1);
    let legacy_path = write(&root, &legacy);

    let removed =
        super::removal::invalidate_stale_generation_in_root(&root, &ProviderKind::Antigravity, 99);
    assert!(
        removed
            .iter()
            .all(|(channel, _)| *channel != routed.channel_id)
    );
    assert!(routed_path.exists());
    assert!(
        removed
            .iter()
            .any(|(channel, _)| *channel == legacy.channel_id)
    );
    assert!(!legacy_path.exists());
}

// B2 the boot reaper's stale verdict, read through the same classification
#[test]
fn stale_classification_keeps_route_row() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = inflight_root();
    let routed = row(6_325_610_011, true);
    age_file(&write(&root, &routed), 3_600);
    let legacy = row(6_325_610_012, false);
    age_file(&write(&root, &legacy), 3_600);

    let kept = load_channel_inflight_for_probe(&ProviderKind::Antigravity, routed.channel_id);
    assert!(matches!(kept, Ok(Some(_))));
    let hidden = load_channel_inflight_for_probe(&ProviderKind::Antigravity, legacy.channel_id);
    assert!(matches!(hidden, Ok(None)));
}

// B4, B4'
#[test]
fn malformed_row_is_held_only_when_it_names_custody() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = inflight_root();
    let named = 6_325_610_013;
    let named_path = inflight_state_path(&root, &ProviderKind::Antigravity, named);
    fs::create_dir_all(named_path.parent().unwrap()).unwrap();
    fs::write(
        &named_path,
        br#"{"provider": "antigravity", "agy_custody": {"v": 1"#,
    )
    .unwrap();
    let held = load_channel_inflight_for_probe(&ProviderKind::Antigravity, named).unwrap_err();
    assert!(held.contains("unreadable"), "{held}");

    // A damaged Legacy row beside an old sealed slot is still hidden as malformed.
    let plain = 6_325_610_014;
    let runtime = crate::config::runtime_root().unwrap();
    fs::create_dir_all(runtime.join(format!("o_store/{plain}/agy/v1/61"))).unwrap();
    let plain_path = inflight_state_path(&root, &ProviderKind::Antigravity, plain);
    fs::write(&plain_path, br#"{"provider": "antigravity", "user_msg"#).unwrap();
    let hidden = load_channel_inflight_for_probe(&ProviderKind::Antigravity, plain).unwrap_err();
    assert!(hidden.contains("unparseable"), "{hidden}");
}

// B3 settle_boot (boot step 5) hands no route row to settlement or the general restore
#[tokio::test]
async fn settle_boot_keeps_route_rows_out_of_restore() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let routed = row(6_325_610_015, true);
    let legacy = row(6_325_610_016, false);
    let (kept, _boot) = crate::services::discord::receipt_settle::settle_boot(
        &shared,
        vec![routed.clone(), legacy.clone()],
    )
    .await;
    assert!(
        kept.iter()
            .all(|state| state.channel_id != routed.channel_id)
    );
    assert!(
        kept.iter()
            .any(|state| state.channel_id == legacy.channel_id)
    );
}

// B3 settle_marked_row: the next admission never commits a route row's receipt
#[tokio::test]
async fn next_admission_never_settles_a_marked_route_row() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let mut routed = row(6_325_610_017, true);
    routed.terminal_receipt = Some(TerminalReceiptStamp {
        outcome: crate::db::turns::TerminalOutcome::Failed,
        confirmed_at: chrono::Utc::now(),
    });
    let path = write(&inflight_root(), &routed);
    let settled = crate::services::discord::receipt_settle::settle_marked_row(
        &shared,
        &ProviderKind::Antigravity,
        routed.channel_id,
    )
    .await;
    assert!(!settled);
    assert!(path.exists());
}
