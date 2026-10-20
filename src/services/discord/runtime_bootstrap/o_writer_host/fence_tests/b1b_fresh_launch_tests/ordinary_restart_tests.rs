//! Normal verified recovery keeps its approved and uncertain delivery paths at host boot.

use super::*;
use crate::services::tui_o::shadow::{UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::{LedgerEntry, LocalRetryMark, PieceOutcome};

const PAYLOAD: &str = "ordinary verified canary saved piece";

#[derive(Clone, Copy)]
enum Pending {
    Approval,
    Prepared,
    LocalRetry,
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_canary_restart_with_operator_approval_hosts_and_sends_once_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::ordinary_canary_restart_with_operator_approval_hosts_and_sends_once_pg"
    )) {
        ordinary_restart(Pending::Approval).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_canary_restart_settles_prepared_once_without_duplicate_post_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::ordinary_canary_restart_settles_prepared_once_without_duplicate_post_pg"
    )) {
        ordinary_restart(Pending::Prepared).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_canary_restart_with_not_found_local_retry_registers_live_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::ordinary_canary_restart_with_not_found_local_retry_registers_live_pg"
    )) {
        ordinary_restart(Pending::LocalRetry).await;
    }
}

async fn ordinary_restart(pending: Pending) {
    let fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some("b1-absent-test-host"))
            .await;
    let _routing = fixture.scoped_test_env("ADK_INTAKE_ROUTING_MODE", "disabled".as_ref());
    let _writer = fixture.scoped_test_env(test_override::CHILD_ENV, "1".as_ref());
    let channels = serde_json::to_string(&[(CANARY_CHANNEL, CodexTui)]).unwrap();
    let _channels = fixture.scoped_test_env(test_override::CHANNELS_ENV, channels.as_ref());
    let _selected = test_override::force_channels(&[(CANARY_CHANNEL, CodexTui)]);
    assert!(dedupe::codex_verified_channel_delivery_allowed(
        CANARY_CHANNEL
    ));
    assert!(dedupe::codex_verified_o_source_allowed(
        CANARY_CHANNEL,
        &fixture.old_source,
    ));
    assert!(dedupe::clear_relaunch::fresh_scope_eligible(CANARY_CHANNEL));
    let bytes = fs::read(&fixture.old_source.path).unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, fixture.root.path())
        .unwrap()
        .unwrap();
    let era = store
        .begin_era(&[CANARY_CHANNEL], chrono::Utc::now(), |channel| {
            Ok(Initialized {
                channel,
                sources: vec![InitSource {
                    source_id: fixture.old_source.clone(),
                    delivery_start: bytes.len() as u64,
                    prefix_hash: hex::encode(Sha256::digest(&bytes)),
                }],
                initial_anchor: 0,
                build_digest: "b1b-normal-canary-restart".into(),
                at: chrono::Utc::now(),
            })
        })
        .unwrap();
    let key = UnitKey {
        channel_id: CANARY_CHANNEL,
        provider: ShadowProvider::Codex,
        native_key: "operator-restored-unit".into(),
        kind: UnitKind::Body,
    };
    let prepared = |serial, local_retry| LedgerEntry::Prepared {
        serial,
        unit_key: key.clone(),
        piece_index: 0,
        payload: PAYLOAD.into(),
        anchor_id: 0,
        epoch: 1,
        local_retry,
    };
    let mut channel = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
    let proof = dedupe::binding_events::codex::read_ownership(&fixture.context)
        .unwrap()
        .verified
        .unwrap();
    channel.set_binding_checkpoint(proof.seq).unwrap();
    channel
        .append_ledger(prepared(
            0,
            matches!(pending, Pending::LocalRetry).then_some(LocalRetryMark::Budget),
        ))
        .unwrap();
    channel
        .append_ledger(if matches!(pending, Pending::LocalRetry) {
            LedgerEntry::NotFound { serial: 0 }
        } else {
            LedgerEntry::Rejected {
                serial: 0,
                status: 403,
            }
        })
        .unwrap();
    drop(channel);
    if !matches!(pending, Pending::LocalRetry) {
        store
            .record_operator_resume(CANARY_CHANNEL, 0, "operator", "ordinary managed restart")
            .unwrap();
    }
    let output = Discord::start().await;
    if matches!(pending, Pending::Prepared) {
        let mut channel = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
        channel.append_ledger(prepared(1, None)).unwrap();
        assert_eq!(channel.ledger().unresolved().unwrap().0, 1);
        assert_eq!(
            channel.ledger().approval(0).unwrap().consumed_serial,
            Some(1)
        );
        output.messages.lock().unwrap().push(json!({
            "id":"900001", "channel_id":CANARY_CHANNEL.to_string(), "content":PAYLOAD,
            "author":{"id":"1","username":"o-bot","discriminator":"0001","avatar":null,"bot":true},
            "timestamp":"2026-10-20T00:00:00+00:00", "edited_timestamp":null,
            "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
            "attachments":[],"embeds":[],"pinned":false,"type":0
        }));
    }
    let prepared = parts(
        &fixture.shared,
        &ProviderKind::Codex,
        None,
        Some(Arc::clone(&output.http)),
    );
    prepared.gate.acquired();
    let readiness = host::process_readiness();
    let mut hosts = host::start_managed(
        ShadowProvider::Codex,
        true,
        cutover::boot_ownership(),
        || prepared,
    );
    assert_eq!(hosts.len(), 1);
    let result = std::panic::AssertUnwindSafe(async {
        for _ in 0..1000 {
            if readiness.submission(CANARY_CHANNEL).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            readiness.submission(CANARY_CHANNEL).is_some(),
            "ordinary verified canary restart must install Live despite saved delivery work"
        );
        eventually(
            || readiness.accepts(CANARY_CHANNEL),
            "ordinary verified source restoration",
        )
        .await;
        assert_eq!(
            cutover::intake_route::for_input_placement("codex", CANARY_CHANNEL).await,
            cutover::intake_route::IntakeRoute::Gateway,
            "normal verified recovery must admit input through the existing writer gate"
        );
        if matches!(pending, Pending::LocalRetry) {
            let channel = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
            assert!(
                crate::services::tui_o::writer::local_retry::pending(channel.ledger()).is_some()
            );
            assert!(
                output.texts().is_empty(),
                "pending budget waits for its original due time"
            );
            assert_eq!(channel.ledger().next_serial(), 1);
        } else {
            eventually(
                || {
                    let channel = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
                    channel
                        .ledger()
                        .piece(1)
                        .is_some_and(|piece| matches!(piece.outcome, Some(PieceOutcome::Posted(_))))
                },
                "saved approved or uncertain piece settlement",
            )
            .await;
            let channel = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
            assert_eq!(
                channel.ledger().approval(0).unwrap().consumed_serial,
                Some(1)
            );
            assert!(channel.ledger().unresolved().is_none());
            assert_eq!(channel.ledger().next_serial(), 2);
            assert_eq!(
                output.texts(),
                [PAYLOAD],
                "saved piece appears once across the restart"
            );
            assert_eq!(
                output.history_reads.load(Ordering::SeqCst),
                usize::from(matches!(pending, Pending::Prepared)),
                "only the open Prepared performs one history read-back"
            );
        }
    })
    .catch_unwind()
    .await;
    hosts.pop().unwrap().stop_and_join().await.unwrap();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
