use super::*;
use crate::dispatch::test_support::DispatchPostgresTestDb;
use crate::services::agent_recovery::tests::{CHANNEL, enabled_runtime};

fn source_payload() -> CheckpointPayload {
    CheckpointPayload::compact(
        "monitoring",
        "preserve the recovery window",
        "saved progress",
        "keep the durable source",
        vec!["src/recovered.rs".into()],
        "continue from the saved source",
        "finish the recovery",
    )
}

fn filler_payload() -> CheckpointPayload {
    CheckpointPayload::compact("monitoring", "", "history filler", "", Vec::new(), "", "")
}

async fn seed_history(
    store: &Coordinator,
    source: CheckpointPayload,
    legacy_complete: bool,
) -> CheckpointEvent {
    store
        .transition(CHANNEL, |runtime| {
            assert!(
                runtime
                    .observe(ObserveInput {
                        channel_id: CHANNEL.into(),
                        primary_turn_id: "restore-window".into(),
                        signal: DetectorSignal::StreamIdleTimeout,
                    })
                    .spawn
                    .is_some()
            );
            let source = runtime
                .note_fallback_progress(CHANNEL, CheckpointEventKind::FallbackProgress, source)
                .unwrap()
                .unwrap();
            for _ in 0..9 {
                runtime
                    .note_fallback_progress(CHANNEL, CheckpointEventKind::Stall, filler_payload())
                    .unwrap()
                    .unwrap();
            }
            if legacy_complete {
                runtime
                    .note_fallback_progress(
                        CHANNEL,
                        CheckpointEventKind::Complete,
                        CheckpointPayload::compact(
                            "old-fallback",
                            "",
                            "fallback complete",
                            "",
                            Vec::new(),
                            "",
                            "",
                        ),
                    )
                    .unwrap()
                    .unwrap();
            }
            Ok(Some(source))
        })
        .await
        .unwrap()
        .unwrap()
}

fn assert_source(packet: &str, source: &CheckpointEvent) {
    assert!(packet.contains(&format!("checkpoint_id={}", source.id)));
    assert!(packet.contains(&format!("Goal: {}", source.payload.goal)));
    assert!(packet.contains(&format!("Files: {}", source.payload.files.join(", "))));
    assert!(packet.contains(&format!("Next: {}", source.payload.next)));
}

async fn assert_repeated_restore(store: &Coordinator, source: &CheckpointEvent) {
    for _ in 0..2 {
        let plan = store
            .transition(CHANNEL, |runtime| {
                Ok(runtime.restore_owner(CHANNEL, true, "owner available"))
            })
            .await
            .unwrap()
            .unwrap();
        assert_source(&plan.packet, source);
        let pending = pending_operation(CHANNEL).await.unwrap().unwrap();
        let OperationPlan::Restore(plan) = pending.plan else {
            panic!("expected a pending owner restore");
        };
        assert_source(&plan.packet, source);
    }
}

#[tokio::test]
async fn postgres_restore_window_preserves_source_past_legacy_complete_and_pending_restart() {
    let db = DispatchPostgresTestDb::create("restore_window", "restore window").await;
    let pool = db.connect_and_migrate_with_max_connections(5).await;
    test_store::with_store(pool.clone(), enabled_runtime().catalog().clone(), async {
        let store = coordinator();
        let source = seed_history(store, source_payload(), true).await;
        assert_repeated_restore(store, &source).await;
        let events = load_checkpoint_events(&pool, CHANNEL, 100).await.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == CheckpointEventKind::Complete)
                .count(),
            1,
            "restoring must not append more synthetic completions"
        );
    })
    .await;
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn postgres_restore_window_stops_at_the_existing_byte_cap() {
    for exceeds_cap in [false, true] {
        let db = DispatchPostgresTestDb::create("restore_byte_cap", "restore byte cap").await;
        let pool = db.connect_and_migrate_with_max_connections(5).await;
        test_store::with_store(pool.clone(), enabled_runtime().catalog().clone(), async {
            let mut payload = source_payload();
            payload.progress.clear();
            let fixed_bytes = serde_json::to_vec(&payload).unwrap().len();
            let filler_bytes = serde_json::to_vec(&filler_payload()).unwrap().len();
            let padding = READ_BYTE_CAP - fixed_bytes - 9 * filler_bytes;
            payload.progress = "p".repeat(padding + usize::from(exceeds_cap));
            let store = coordinator();
            let source = seed_history(store, payload, false).await;
            assert_eq!(
                source.payload_bytes + 9 * filler_bytes,
                READ_BYTE_CAP + usize::from(exceeds_cap)
            );
            let snapshot = store.snapshot(&pool, CHANNEL).await.unwrap();
            let events = snapshot.events(CHANNEL);
            assert!(
                events
                    .iter()
                    .map(|event| event.payload_bytes)
                    .sum::<usize>()
                    <= READ_BYTE_CAP
            );
            assert_eq!(
                super::super::restore::latest_progress_or_complete(events).map(|event| &event.id),
                (!exceeds_cap).then_some(&source.id),
                "the saved source fits only at or below the existing byte cap"
            );
            if !exceeds_cap {
                assert_repeated_restore(store, &source).await;
            }
        })
        .await;
        pool.close().await;
        db.drop().await;
    }
}
