//! After a scoped terminate the owner is woken once, only when its exclusion is gone and input
//! still waits; a retired Claude execution loses its own admission, a newer listing keeps its own.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::test_support::{owner, record, wire};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, HostedState, load_hosted_execution_pg,
};
use crate::services::discord::adk_session;
use crate::services::discord::queue_io::cancel_backstop_test_support as wake_probe;
use crate::services::tui_prompt_dedupe as dedupe;
use crate::services::turn_orchestrator::{PendingQueueItem, v2_queue};

/// Terminates the rig's bound execution while one input waits for the V2 owner, its postlude
/// clearing that input when `clear`; returns the owner wakes seen under the exclusion and after.
async fn wakes(rig: &herdr::P11TerminationRig, clear: bool) -> (usize, usize) {
    let node = Node::bind(rig, herdr::HostedState::Bound, None).await;
    let handle = node.shared.mailbox(channel());
    let persistence = crate::services::discord::queue_persistence_context(
        &node.shared,
        &ProviderKind::Claude,
        channel(),
    );
    let (path, namespace) = v2_queue(&persistence, channel()).unwrap();
    handle.v2_adopt(path, namespace).await.unwrap();
    let queued: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
        "message_id":62,"channel_id":CHANNEL,"source_message_ids":[62],
        "queued_generation":1,"text":"queued behind the terminate"}))
    .unwrap();
    let enqueued = handle.v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        owner
            .enqueue_fixture(&queued)
            .map_err(|error| format!("{error:?}"))
    });
    assert_eq!(enqueued.await, Ok(true));
    let previous = wake_probe::set_owner_wake_for_tests(channel(), Some(Arc::new(|| {})));
    let before = wake_probe::owner_wakes(channel());
    let during = Arc::new(AtomicUsize::new(usize::MAX));
    let seen = during.clone();
    probe_before_postlude(Box::new(move || {
        Box::pin(async move { seen.store(wake_probe::owner_wakes(channel()), Ordering::SeqCst) })
    }));
    let request = OperatorTerminate {
        session_key: node.key.clone(),
        execution_nonce: herdr::EXECUTION.into(),
    };
    let expected = super::e08::identity::unbound(rig, &node);
    let mailbox = handle.clone();
    let bound = (request, expected);
    let registry = &node.registry;
    let (result, _) = terminate_for_http(registry, node.pool.clone(), bound, "test", move |_| {
        async move {
            if clear {
                let cleared = mailbox.v2_request(|slot| {
                    let cleared = slot.as_mut().map(|owner| owner.clear_waiting());
                    cleared.transpose().map(|_| ())
                });
                assert_eq!(cleared.await, Ok(()));
            }
        }
        .boxed()
    })
    .await;
    assert_eq!(result, TerminationResult::Retired);
    assert_eq!(
        node.effects(rig).await.closes(),
        1,
        "only the close reached Herdr"
    );
    let after = wake_probe::owner_wakes(channel());
    wake_probe::set_owner_wake_for_tests(channel(), previous);
    node.finish().await;
    (during.load(Ordering::SeqCst) - before, after - before)
}

#[test]
fn terminate_exclusion_blocks_write_then_releases_and_wakes_once_pg() {
    let name = "retired_wake::terminate_exclusion_blocks_write_then_releases_and_wakes_once_pg";
    let Some((rig, runtime)) = rig_child(name) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    assert_eq!(runtime.block_on(wakes(&rig, false)), (0, 1));
}

#[test]
fn clear_postlude_removed_front_is_not_revived_pg() {
    let name = "retired_wake::clear_postlude_removed_front_is_not_revived_pg";
    let Some((rig, runtime)) = rig_child(name) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    assert_eq!(runtime.block_on(wakes(&rig, true)), (0, 0));
}

#[tokio::test]
async fn claude_retire_withholds_same_nonce_admission_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut owner = owner(&CHANNEL.to_string());
    owner.logical_key = "AgentDesk-claude-retire-admission".into();
    let (logical, hash) = (owner.logical_key.clone(), owner.discord_token_hash.clone());
    let key = adk_session::build_namespaced_session_key(&hash, &ProviderKind::Claude, &logical);
    let bound = record(&owner, "n1", HostedState::Bound);
    for (listed, admitted) in [("n1", false), ("n2", true)] {
        let raw = Some(wire(&bound));
        inflight::seed_session_row_hashed(&pool, &key, CHANNEL, &hash, raw).await;
        let HostedLookup::Found(row) =
            load_hosted_execution_pg(&pool, HostedLookupKey::SessionKey(&key)).await
        else {
            panic!("the seeded row is readable");
        };
        dedupe::install_herdr_execution(&logical, listed);
        dedupe::admit_herdr_execution(&logical, listed);
        let retired = super::super::super::retire_confirmed(&pool, &row, &bound).await;
        assert_eq!(retired, Ok(()), "{listed}");
        let kept = dedupe::herdr_claim_admission(&logical, true).is_ok();
        assert_eq!(kept, admitted, "listed {listed}");
    }
    pool.close().await;
    db.drop().await;
}
