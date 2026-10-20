//! Normal verified recovery keeps its approved and uncertain delivery paths at host boot.

use super::*;
use crate::services::discord::commands::control::codex_clear_relaunch_tests::CanaryFixture;
use crate::services::tui_o::shadow::{UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::{LedgerEntry, LocalRetryMark, PieceOutcome};

const PAYLOAD: &str = "ordinary verified canary saved piece";

mod fresh_scope_restart_tests;

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
    restart_case(pending, false, None).await;
}

async fn restart_case(
    pending: Pending,
    fresh: bool,
    fault: Option<fresh_scope_restart_tests::Fault>,
) {
    let mut fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some("b1-absent-test-host"))
            .await;
    let pool: &sqlx::PgPool = fixture.shared.pg_pool.as_ref().unwrap();
    let session_key = crate::services::discord::adk_session::build_namespaced_session_key(
        &fixture.shared.token_hash,
        &ProviderKind::Codex,
        CANARY_TMUX,
    );
    let persisted = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        "SELECT provider, channel_id, raw_provider_session_id FROM sessions WHERE session_key=$1",
    )
    .bind(session_key)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(persisted.0, "codex");
    assert_eq!(persisted.1, Some(CANARY_CHANNEL.to_string()));
    assert_eq!(
        persisted.2.as_deref(),
        Some(fixture.old_source.session_id.as_str())
    );
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
    let mut channel = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
    let proof = dedupe::binding_events::codex::read_ownership(&fixture.context)
        .unwrap()
        .verified
        .unwrap();
    channel.set_binding_checkpoint(proof.seq).unwrap();
    drop(channel);
    let output = Discord::start().await;
    let preserved = if fresh {
        Some(fresh_scope_restart_tests::install(&mut fixture, &store, &era, &output).await)
    } else {
        None
    };
    let key = UnitKey {
        channel_id: CANARY_CHANNEL,
        provider: ShadowProvider::Codex,
        native_key: "operator-restored-unit".into(),
        kind: UnitKind::Body,
    };
    let mut channel = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
    let anchor_id = channel.ledger().anchor();
    let prepared = |serial, local_retry| LedgerEntry::Prepared {
        serial,
        unit_key: key.clone(),
        piece_index: 0,
        payload: PAYLOAD.into(),
        anchor_id,
        epoch: 1,
        local_retry,
    };
    let blocked_serial = channel.ledger().next_serial();
    let sent_serial = blocked_serial + 1;
    channel
        .append_ledger(prepared(
            blocked_serial,
            matches!(pending, Pending::LocalRetry).then_some(LocalRetryMark::Budget),
        ))
        .unwrap();
    channel
        .append_ledger(if matches!(pending, Pending::LocalRetry) {
            LedgerEntry::NotFound {
                serial: blocked_serial,
            }
        } else {
            LedgerEntry::Rejected {
                serial: blocked_serial,
                status: 403,
            }
        })
        .unwrap();
    drop(channel);
    if !matches!(pending, Pending::LocalRetry) {
        store
            .record_operator_resume(
                CANARY_CHANNEL,
                blocked_serial,
                "operator",
                "ordinary managed restart",
            )
            .unwrap();
    }
    let prior_posts = output.texts();
    if matches!(pending, Pending::Prepared) {
        let mut channel = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
        channel.append_ledger(prepared(sent_serial, None)).unwrap();
        assert_eq!(channel.ledger().unresolved().unwrap().0, sent_serial);
        assert_eq!(
            channel
                .ledger()
                .approval(blocked_serial)
                .unwrap()
                .consumed_serial,
            Some(sent_serial)
        );
        let existing_id = (900_001 + output.messages.lock().unwrap().len() as u64).to_string();
        output.messages.lock().unwrap().push(json!({
            "id": existing_id, "channel_id":CANARY_CHANNEL.to_string(), "content":PAYLOAD,
            "author":{"id":"1","username":"o-bot","discriminator":"0001","avatar":null,"bot":true},
            "timestamp":"2026-10-20T00:00:00+00:00", "edited_timestamp":null,
            "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
            "attachments":[],"embeds":[],"pinned":false,"type":0
        }));
    }
    if let Some(fault) = fault {
        fresh_scope_restart_tests::corrupt(&fixture, fault);
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
        if fault.is_some() {
            for _ in 0..100 {
                assert!(
                    readiness.submission(CANARY_CHANNEL).is_none(),
                    "invalid persisted scope must never publish a live actor"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(!readiness.accepts(CANARY_CHANNEL));
            assert!(
                matches!(
                    cutover::intake_route::for_input_placement("codex", CANARY_CHANNEL).await,
                    cutover::intake_route::IntakeRoute::Hold(_)
                ),
                "invalid persisted scope cannot admit fresh input"
            );
            assert_eq!(
                output.texts(),
                prior_posts,
                "invalid persisted scope cannot POST saved delivery work"
            );
            let channel = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
            assert!(channel.ledger().piece(sent_serial).is_none());
            return;
        }
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
        if fresh {
            fresh_scope_restart_tests::assert_selection(&fixture);
        }
        for _ in 0..2000 {
            if readiness.accepts(CANARY_CHANNEL) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let accepts = readiness.accepts(CANARY_CHANNEL);
        let route = cutover::intake_route::for_input_placement("codex", CANARY_CHANNEL).await;
        let channel = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
        let posts = output.texts();
        let saved_posts = posts.iter().filter(|text| text.as_str() == PAYLOAD).count();
        assert!(
            accepts,
            "P1-1R: installed suspension must restore ordinary delivery; \
             accepts={accepts} route={route:?} posts={} saved_posts={saved_posts} \
             next_serial={} unresolved={:?} approval_consumed={:?}",
            posts.len(),
            channel.ledger().next_serial(),
            channel.ledger().unresolved().map(|(serial, _)| serial),
            channel
                .ledger()
                .approval(blocked_serial)
                .map(|approval| approval.consumed_serial),
        );
        drop(channel);
        assert_eq!(
            route,
            cutover::intake_route::IntakeRoute::Gateway,
            "normal verified recovery must admit input through the existing writer gate"
        );
        if matches!(pending, Pending::LocalRetry) {
            let channel = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
            assert!(
                crate::services::tui_o::writer::local_retry::pending(channel.ledger()).is_some()
            );
            assert!(
                output.texts() == prior_posts,
                "pending budget waits for its original due time"
            );
            assert_eq!(channel.ledger().next_serial(), sent_serial);
        } else {
            eventually(
                || {
                    let channel = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
                    channel
                        .ledger()
                        .piece(sent_serial)
                        .is_some_and(|piece| matches!(piece.outcome, Some(PieceOutcome::Posted(_))))
                },
                "saved approved or uncertain piece settlement",
            )
            .await;
            let channel = store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap();
            assert_eq!(
                channel
                    .ledger()
                    .approval(blocked_serial)
                    .unwrap()
                    .consumed_serial,
                Some(sent_serial)
            );
            assert!(channel.ledger().unresolved().is_none());
            assert_eq!(channel.ledger().next_serial(), sent_serial + 1);
            let mut expected = prior_posts.clone();
            expected.push(PAYLOAD.to_owned());
            assert_eq!(
                output.texts(),
                expected,
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
    if let Some(preserved) = preserved {
        preserved.assert_preserved(&store.open_channel(&era, CANARY_CHANNEL).unwrap().unwrap());
    }
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
